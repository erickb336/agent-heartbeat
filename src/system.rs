//! Read-only checks of the operating system through libproc system calls.
//! They replace `ps` and `lsof`: no child process starts, and each check takes about a millisecond.

use std::{collections::HashSet, ffi::CStr, path::Path};

pub trait Probe {
    /// Start time (epoch seconds) of a live process. None when the process does not exist.
    fn start_time(&self, pid: i32) -> Option<i64>;
    /// Base names of the files directly in `dir` that a live process holds open.
    /// With `name_prefix`, only processes whose name starts with it are checked.
    /// None when the check failed.
    fn held_files(&self, dir: &Path, name_prefix: Option<&str>) -> Option<HashSet<String>>;
}

pub struct LibProc;

// Constants and a structure from <sys/proc_info.h> that the libc crate does not export.
const PROC_PIDFDVNODEPATHINFO: i32 = 2;
const VNODE_FDINFOWITHPATH_SIZE: usize = 1200;
const VNODE_PATH_OFFSET: usize = 176;
const PROC_UID_ONLY: u32 = 4;

impl Probe for LibProc {
    fn start_time(&self, pid: i32) -> Option<i64> {
        if pid <= 0 {
            return None;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let n = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        (n == size).then_some(info.pbi_start_tvsec as i64)
    }

    fn held_files(&self, dir: &Path, name_prefix: Option<&str>) -> Option<HashSet<String>> {
        // The kernel reports real paths, for example /private/var for /var, so resolve links first.
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let prefix = format!("{}/", dir.to_str()?);
        let mut held = HashSet::new();
        for pid in user_pids()? {
            if let Some(want) = name_prefix {
                if !process_name(pid).is_some_and(|n| n.starts_with(want)) {
                    continue;
                }
            }
            for path in open_vnode_paths(pid) {
                if let Some(rest) = path.strip_prefix(&prefix) {
                    if !rest.contains('/') {
                        held.insert(rest.to_string());
                    }
                }
            }
        }
        Some(held)
    }
}

/// The process IDs of the current user.
fn user_pids() -> Option<Vec<i32>> {
    let uid = unsafe { libc::getuid() };
    let bytes = unsafe { libc::proc_listpids(PROC_UID_ONLY, uid, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return None;
    }
    // Room for processes that start between the two calls.
    let mut pids = vec![0i32; bytes as usize / 4 + 64];
    let got = unsafe {
        libc::proc_listpids(PROC_UID_ONLY, uid, pids.as_mut_ptr().cast(), (pids.len() * 4) as i32)
    };
    if got <= 0 {
        return None;
    }
    pids.truncate(got as usize / 4);
    pids.retain(|&p| p > 0);
    Some(pids)
}

fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 64];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

/// Paths of the files (vnodes) that a process holds open. Empty when the process is gone.
fn open_vnode_paths(pid: i32) -> Vec<String> {
    let fd_size = std::mem::size_of::<libc::proc_fdinfo>();
    let bytes = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    let mut fds = vec![libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }; bytes as usize / fd_size + 16];
    let got = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), (fds.len() * fd_size) as i32)
    };
    if got <= 0 {
        return Vec::new();
    }
    fds.truncate(got as usize / fd_size);
    let mut paths = Vec::new();
    let mut info = [0u8; VNODE_FDINFOWITHPATH_SIZE];
    for fd in fds.iter().filter(|f| f.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32) {
        let n = unsafe {
            libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDVNODEPATHINFO, info.as_mut_ptr().cast(), info.len() as i32)
        };
        if n as usize != VNODE_FDINFOWITHPATH_SIZE {
            continue;
        }
        if let Ok(path) = CStr::from_bytes_until_nul(&info[VNODE_PATH_OFFSET..]) {
            paths.push(path.to_string_lossy().into_owned());
        }
    }
    paths
}

#[cfg(test)]
pub mod fake {
    //! A probe with fixed answers, for tests.
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    pub struct FakeProbe {
        pub starts: HashMap<i32, i64>,
        /// None makes held_files fail.
        pub held: Option<HashSet<String>>,
    }

    impl Probe for FakeProbe {
        fn start_time(&self, pid: i32) -> Option<i64> {
            self.starts.get(&pid).copied()
        }
        fn held_files(&self, _dir: &Path, _prefix: Option<&str>) -> Option<HashSet<String>> {
            self.held.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn own_process_has_a_start_time_and_a_dead_pid_has_none() {
        let start = LibProc.start_time(std::process::id() as i32).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        assert!(start <= now && now - start < 3600);
        assert_eq!(LibProc.start_time(0), None);
        assert_eq!(LibProc.start_time(i32::MAX), None);
    }

    #[test]
    fn finds_a_file_this_process_holds_open() {
        let dir = std::env::temp_dir().join(format!("ah-sys-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Not canonicalised: the temporary folder is under /var, a link to /private/var.
        let held_path = dir.join("thread-a.lock");
        fs::write(&held_path, b"").unwrap();
        fs::write(dir.join("thread-b.lock"), b"").unwrap();
        let _open = fs::File::open(&held_path).unwrap();
        let held = LibProc.held_files(&dir, None).unwrap();
        assert!(held.contains("thread-a.lock"));
        assert!(!held.contains("thread-b.lock"));
        let none = LibProc.held_files(&dir, Some("no-such-process-name")).unwrap();
        assert!(none.is_empty());
    }
}
