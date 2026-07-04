//! Exclusive advisory lock guarding a root's daemon slot
//! (`docs/design/daemon-lifecycle.md`). Prevents two `semnav daemon <root>`
//! processes (or, later, two racing `semnav serve` auto-spawns) from running
//! concurrently against the same `<root>/.semnav/`. Hand-rolled via
//! `libc::flock` — `libc` is already a crate dependency (used for `kill` in
//! `src/lsp/server.rs`), so this needs no new crate. `flock` is released by
//! the kernel on process exit for any reason, including SIGKILL, so a crashed
//! daemon can never leave a stale, un-acquirable lock behind.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

/// An acquired exclusive lock on the file at `path`. Dropping it closes the
/// underlying fd, which releases the `flock` immediately.
pub struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    /// Try to acquire the exclusive lock at `path` (created if absent).
    /// `Ok(None)` means another process already holds it — not an error.
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        // SAFETY: `file`'s fd is valid for the duration of this call, and
        // `flock` only inspects/mutates kernel-side lock state for it.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            Ok(Some(Self { _file: file }))
        } else {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EWOULDBLOCK) => Ok(None),
                _ => Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn second_acquire_on_the_same_path_fails_while_the_first_is_held() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        let first = DaemonLock::try_acquire(&path).unwrap();
        assert!(first.is_some(), "first acquire must succeed");

        let second = DaemonLock::try_acquire(&path).unwrap();
        assert!(second.is_none(), "second acquire must observe it's held");
    }

    #[test]
    fn dropping_the_guard_releases_the_lock_for_the_next_acquirer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("daemon.lock");

        let first = DaemonLock::try_acquire(&path).unwrap();
        assert!(first.is_some());
        drop(first);

        let second = DaemonLock::try_acquire(&path).unwrap();
        assert!(
            second.is_some(),
            "lock must be acquirable again after release"
        );
    }

    #[test]
    fn acquire_creates_the_lock_file_if_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("daemon.lock");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        assert!(!path.exists());
        let guard = DaemonLock::try_acquire(&path).unwrap();
        assert!(guard.is_some());
        assert!(path.exists());
    }
}
