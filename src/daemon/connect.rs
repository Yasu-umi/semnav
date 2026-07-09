//! Auto-spawn-and-connect for the `serve`↔`daemon` link
//! (`docs/design/daemon-lifecycle.md`). [`ensure_and_connect`] is the one
//! entry point both `run_serve`'s initial connect and
//! [`super::reconnect::ReconnectingDaemonClient`]'s recovery path use, so
//! "make sure a daemon exists for `root`, then attach" has a single
//! implementation instead of two that can drift.

use std::path::Path;
use std::time::Duration;

use super::client::DaemonClient;
use super::discovery::{self, Liveness};
use super::lock::DaemonLock;

/// Maximum wait for a freshly-spawned (or concurrently-being-spawned) daemon
/// to bind its socket. The daemon itself doesn't wait on LSP readiness before
/// accepting connections (supervisors are lazy, exactly as `serve` used to
/// be), so this only has to cover process startup + db open, not any LSP
/// round-trip.
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Ensure a daemon is running for `root`, spawning one (detached) if none is
/// live yet, then connect to it. Two callers racing here is safe: whichever
/// loses the `DaemonLock` just waits for the winner's socket; if a narrow
/// race lets a second daemon process start anyway, its own `run_daemon`
/// self-check refuses to bind and exits — this only cares that *some* daemon
/// eventually answers, not which one.
pub async fn ensure_and_connect(root: &Path, cache_dir: &Path) -> Result<DaemonClient, String> {
    ensure_daemon_running(root, cache_dir).await?;
    let sock_path = discovery::sock_path(cache_dir);
    DaemonClient::connect(&sock_path)
        .await
        .map_err(|e| format!("{e:#}"))
}

async fn ensure_daemon_running(root: &Path, cache_dir: &Path) -> Result<(), String> {
    if discovery::probe_liveness(cache_dir).await == Liveness::Live {
        return Ok(());
    }
    if let Err(err) = tokio::fs::create_dir_all(cache_dir).await {
        return Err(format!("cannot create {}: {err:#}", cache_dir.display()));
    }

    let lock_path = discovery::lock_path(cache_dir);
    match DaemonLock::try_acquire(&lock_path) {
        Ok(Some(lock)) => {
            // Release immediately: the spawned daemon acquires this same
            // lock itself as the first thing it does, so we must not still
            // be holding it when that happens.
            drop(lock);
            spawn_detached_daemon(root, cache_dir)?;
            wait_for_daemon_ready(cache_dir).await
        }
        Ok(None) => wait_for_daemon_ready(cache_dir).await,
        Err(err) => Err(format!("cannot acquire {}: {err:#}", lock_path.display())),
    }
}

/// Spawn `semnav daemon <root>` detached from this process: a new process
/// group (so it isn't killed by a signal sent to the caller's group) and
/// redirected stdio (so it doesn't hold the caller's pipes open, and its own
/// diagnostics land in `daemon.log` instead of nowhere). Deliberately not
/// `.wait()`ed — the daemon must outlive its spawner.
fn spawn_detached_daemon(root: &Path, cache_dir: &Path) -> Result<(), String> {
    let current_exe =
        std::env::current_exe().map_err(|e| format!("cannot resolve current exe: {e:#}"))?;
    let log_path = discovery::log_path(cache_dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("cannot open {}: {e:#}", log_path.display()))?;
    let log_file2 = log_file
        .try_clone()
        .map_err(|e| format!("cannot clone log file handle: {e:#}"))?;

    let mut cmd = tokio::process::Command::new(current_exe);
    cmd.arg("daemon")
        .arg(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log_file))
        .stderr(std::process::Stdio::from(log_file2))
        .kill_on_drop(false);
    #[cfg(unix)]
    cmd.process_group(0);

    cmd.spawn()
        .map(|_child| ())
        .map_err(|e| format!("failed to spawn daemon: {e:#}"))
}

/// Poll the liveness probe until a daemon answers or `DAEMON_STARTUP_TIMEOUT`
/// elapses.
async fn wait_for_daemon_ready(cache_dir: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + DAEMON_STARTUP_TIMEOUT;
    loop {
        if discovery::probe_liveness(cache_dir).await == Liveness::Live {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "daemon did not become ready within {DAEMON_STARTUP_TIMEOUT:?} — see {}",
                discovery::log_path(cache_dir).display()
            ));
        }
        tokio::time::sleep(DAEMON_POLL_INTERVAL).await;
    }
}
