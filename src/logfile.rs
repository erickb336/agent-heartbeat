//! A small operational log: state changes only, never a payload, a secret, a path or content.
//! It rotates at 256 KB and keeps one older file, so it never uses more than about 512 KB.

use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::Path};

const MAX_BYTES: u64 = 256 * 1024;

pub fn log(path: &Path, message: &str) {
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
    let line = format!("{} {message}\n", crate::time::iso_ms(crate::time::now_ms()));
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_stays_bounded() {
        let dir = std::env::temp_dir().join(format!("ah-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("agent-heartbeat.log");
        for _ in 0..12_000 {
            log(&p, "heartbeat accepted; active agents 3");
        }
        let total: u64 = fs::read_dir(&dir).unwrap().flatten().map(|e| e.metadata().unwrap().len()).sum();
        assert!(total < 2 * MAX_BYTES + 1024, "{total}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
    }
}
