//! Per-root daemon file layout under `<root>/.semnav/` and the liveness probe
//! used by both `serve`'s auto-spawn path (Step 3) and `daemon stop`
//! (`docs/design/daemon-lifecycle.md`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;

/// `daemon.sock` — the Unix domain socket the daemon's `server.rs` binds.
pub fn sock_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("daemon.sock")
}

/// `daemon.lock` — the [`super::lock::DaemonLock`] guard file.
pub fn lock_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("daemon.lock")
}

/// `daemon.pid` — informational only (pid + start time), for `daemon stop`
/// diagnostics. Never used to determine liveness (pids get reused).
pub fn pid_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("daemon.pid")
}

/// `daemon.log` — the detached daemon's redirected stdout/stderr (Step 3).
pub fn log_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("daemon.log")
}

/// Result of [`probe_liveness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// Something is listening and accepted a connection.
    Live,
    /// No socket file, or a stale one nothing is listening on (removed).
    NotRunning,
}

/// Bounded wait for a single connect attempt — a hung daemon (accepting but
/// never completing the connection) must not block the caller forever.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe whether a daemon is live at `cache_dir`. Does not perform any
/// protocol handshake — only checks that *something* accepts a connection at
/// `daemon.sock`; the caller sends its actual request afterward.
pub async fn probe_liveness(cache_dir: &Path) -> Liveness {
    let sock = sock_path(cache_dir);
    if !sock.exists() {
        return Liveness::NotRunning;
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&sock)).await {
        Ok(Ok(_stream)) => Liveness::Live,
        // Connection refused/not found ⇒ a stale socket left behind by a
        // daemon that died without cleaning up; remove it so a later spawn
        // doesn't collide on `bind()`. A `NotFound` race (the daemon
        // shutting down between our `exists()` check and `connect()`) hits
        // the same branch and is handled identically.
        Ok(Err(_)) => {
            let _ = std::fs::remove_file(&sock);
            Liveness::NotRunning
        }
        // Timed out mid-connect: something is bound but not accepting
        // promptly. Treat as not-usably-running rather than live, but don't
        // remove the socket — a wedged-but-alive daemon is a different
        // problem than a crashed one, and deleting its live socket out from
        // under it would make things worse.
        Err(_) => Liveness::NotRunning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn no_socket_file_is_not_running() {
        let dir = tempdir().unwrap();
        assert_eq!(probe_liveness(dir.path()).await, Liveness::NotRunning);
    }

    #[tokio::test]
    async fn stale_socket_file_is_removed_and_reported_not_running() {
        let dir = tempdir().unwrap();
        let sock = sock_path(dir.path());
        // A socket file with nothing listening: bind-then-drop leaves the
        // path on disk (Unix sockets aren't auto-removed on listener drop),
        // which is exactly the "crashed daemon" shape we need to simulate.
        {
            let _listener = UnixListener::bind(&sock).unwrap();
        }
        assert!(sock.exists());

        assert_eq!(probe_liveness(dir.path()).await, Liveness::NotRunning);
        assert!(!sock.exists(), "stale socket must be removed");
    }

    #[tokio::test]
    async fn live_listener_is_reported_live() {
        let dir = tempdir().unwrap();
        let sock = sock_path(dir.path());
        let listener = UnixListener::bind(&sock).unwrap();
        let _accept_task = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        assert_eq!(probe_liveness(dir.path()).await, Liveness::Live);
    }

    #[test]
    fn path_helpers_are_named_consistently_under_cache_dir() {
        let cache_dir = Path::new("/repo/.semnav");
        assert_eq!(sock_path(cache_dir), cache_dir.join("daemon.sock"));
        assert_eq!(lock_path(cache_dir), cache_dir.join("daemon.lock"));
        assert_eq!(pid_path(cache_dir), cache_dir.join("daemon.pid"));
        assert_eq!(log_path(cache_dir), cache_dir.join("daemon.log"));
    }
}
