//! Reads the complete lines that were added to a JSONL file since the last read.
//!
//! The first `head_len` bytes of each line decide whether the caller needs it. Unwanted lines are
//! skipped without being kept in memory, so a 47 MB line costs one pass over its bytes and no
//! allocation. Wanted lines are buffered up to `MAX_LINE`.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

const CHUNK: usize = 1 << 20;
pub const MAX_LINE: usize = 8 << 20;

/// Where the next read of one file starts. `offset` is just after the last complete line,
/// so a line that the writer has not finished is read again next time.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub ino: u64,
    pub offset: u64,
    pub size: u64,
    pub mtime_ns: i64,
}

impl Cursor {
    /// True when the file has not changed since this cursor was stored.
    pub fn unchanged(&self, meta: &fs::Metadata) -> bool {
        self.ino == meta.ino() && self.size == meta.len() && self.mtime_ns == mtime_ns(meta)
    }
}

pub enum Line<'a> {
    Wanted(&'a [u8]),
    /// A complete line that the caller does not need.
    Other,
    /// A wanted line longer than MAX_LINE. It is skipped.
    Oversize,
}

#[derive(Debug, Default, PartialEq)]
pub struct ReadResult {
    /// The file was replaced or got shorter, so the read started again from the beginning.
    pub reset: bool,
}

pub fn read_new(
    path: &Path,
    cursor: &mut Cursor,
    head_len: usize,
    wants: impl Fn(&[u8]) -> bool,
    mut on_line: impl FnMut(Line),
) -> io::Result<ReadResult> {
    // Read-only open. The tracker never writes, locks or renames source files.
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    let mut result = ReadResult::default();
    if meta.ino() != cursor.ino || meta.len() < cursor.offset {
        result.reset = cursor.ino != 0;
        cursor.ino = meta.ino();
        cursor.offset = 0;
    }
    cursor.size = meta.len();
    cursor.mtime_ns = mtime_ns(&meta);
    if meta.len() == cursor.offset {
        return Ok(result);
    }
    file.seek(SeekFrom::Start(cursor.offset))?;

    #[derive(PartialEq)]
    enum Mode {
        Undecided,
        Keep,
        Skip,
        Oversize,
    }
    let mut buf = vec![0u8; CHUNK];
    let mut line: Vec<u8> = Vec::new();
    let mut mode = Mode::Undecided;
    let mut pos = cursor.offset; // file position of buf[0]
    let mut line_start = cursor.offset;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut s = 0;
        while s < n {
            let newline = memchr::memchr(b'\n', &buf[s..n]);
            let end = newline.map_or(n, |i| s + i);
            let piece = &buf[s..end];
            match mode {
                Mode::Undecided => {
                    line.extend_from_slice(piece);
                    if line.len() >= head_len {
                        if wants(&line[..head_len]) {
                            mode = Mode::Keep;
                        } else {
                            mode = Mode::Skip;
                            line.clear();
                        }
                    }
                }
                Mode::Keep if line.len() + piece.len() > MAX_LINE => {
                    mode = Mode::Oversize;
                    line = Vec::new();
                }
                Mode::Keep => line.extend_from_slice(piece),
                Mode::Skip | Mode::Oversize => {}
            }
            if newline.is_none() {
                break;
            }
            match mode {
                Mode::Undecided if line.is_empty() => {}
                Mode::Undecided if wants(&line) => on_line(Line::Wanted(&line)),
                Mode::Undecided | Mode::Skip => on_line(Line::Other),
                Mode::Keep => on_line(Line::Wanted(&line)),
                Mode::Oversize => on_line(Line::Oversize),
            }
            line.clear();
            mode = Mode::Undecided;
            s = end + 1;
            line_start = pos + s as u64;
        }
        pos += n as u64;
    }
    cursor.offset = line_start;
    Ok(result)
}

pub fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.mtime() * 1_000_000_000 + meta.mtime_nsec()
}

pub fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.mtime() * 1000 + meta.mtime_nsec() / 1_000_000
}

/// Lists `*.jsonl` files under `root` down to `depth` directory levels, with their metadata.
/// A missing or unreadable directory yields nothing.
pub fn list_jsonl(root: &Path, depth: usize, out: &mut Vec<(PathBuf, fs::Metadata)>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            if depth > 0 {
                list_jsonl(&path, depth - 1, out);
            }
        } else if meta.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
            out.push((path, meta));
        }
    }
}

/// True when `needle` occurs in `haystack`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn collect(path: &Path, cursor: &mut Cursor) -> (Vec<String>, usize, ReadResult) {
        let mut wanted = Vec::new();
        let mut other = 0;
        let r = read_new(path, cursor, 4, |h| h.starts_with(b"WANT"), |l| match l {
            Line::Wanted(b) => wanted.push(String::from_utf8_lossy(b).into_owned()),
            Line::Other | Line::Oversize => other += 1,
        })
        .unwrap();
        (wanted, other, r)
    }

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ah-tail-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("f.jsonl")
    }

    #[test]
    fn partial_line_waits_until_complete() {
        let p = temp_file("partial");
        fs::write(&p, "WANT a\nskip\nWANT b-part").unwrap();
        let mut c = Cursor::default();
        let (w, o, _) = collect(&p, &mut c);
        assert_eq!(w, vec!["WANT a"]);
        assert_eq!(o, 1);
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"ial\n").unwrap();
        let (w, _, _) = collect(&p, &mut c);
        assert_eq!(w, vec!["WANT b-partial"]);
        let (w, _, _) = collect(&p, &mut c);
        assert!(w.is_empty(), "nothing new");
    }

    #[test]
    fn replaced_or_truncated_file_is_read_again() {
        let p = temp_file("rotate");
        fs::write(&p, "WANT 1\nWANT 2\n").unwrap();
        let mut c = Cursor::default();
        assert_eq!(collect(&p, &mut c).0.len(), 2);
        // Truncated in place.
        fs::write(&p, "WANT 3\n").unwrap();
        let (w, _, r) = collect(&p, &mut c);
        assert_eq!((w, r.reset), (vec!["WANT 3".to_string()], true));
        // Replaced by a new file (new inode), longer than before.
        let tmp = p.with_extension("new");
        fs::write(&tmp, "WANT 4\nWANT 5\nWANT 6\n").unwrap();
        fs::rename(&tmp, &p).unwrap();
        let (w, _, r) = collect(&p, &mut c);
        assert_eq!((w.len(), r.reset), (3, true));
    }

    #[test]
    fn long_unwanted_lines_are_skipped_without_buffering() {
        let p = temp_file("long");
        let mut big = b"skip".to_vec();
        big.resize(3 * CHUNK + 17, b'x');
        big.push(b'\n');
        big.extend_from_slice(b"WANT after\n");
        fs::write(&p, &big).unwrap();
        let mut c = Cursor::default();
        let (w, o, _) = collect(&p, &mut c);
        assert_eq!((w, o), (vec!["WANT after".to_string()], 1));
        assert_eq!(c.offset, big.len() as u64);
    }

    #[test]
    fn oversize_wanted_lines_are_reported_not_kept() {
        let p = temp_file("oversize");
        let mut big = b"WANT".to_vec();
        big.resize(MAX_LINE + 10, b'y');
        big.extend_from_slice(b"\nWANT ok\n");
        fs::write(&p, &big).unwrap();
        let mut c = Cursor::default();
        let mut events = Vec::new();
        read_new(&p, &mut c, 4, |h| h.starts_with(b"WANT"), |l| {
            events.push(match l {
                Line::Wanted(b) => String::from_utf8_lossy(b).into_owned(),
                Line::Other => "other".into(),
                Line::Oversize => "oversize".into(),
            })
        })
        .unwrap();
        assert_eq!(events, vec!["oversize", "WANT ok"]);
    }
}
