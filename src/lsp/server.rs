//! LSP child-process supervision: spawn the language server, feed its piped
//! stdio into an [`LspClient`], and observe process exit independently of the
//! stream closing.
//!
//! This is the transport-level process layer: spawn the server, feed its piped
//! stdio into an [`LspClient`], observe process exit independently of the
//! stream closing, and run the [`shutdown`](ServerProcess::shutdown)
//! escalation (`shutdown`→`exit`→SIGTERM→SIGKILL). The health state machine
//! and exponential-backoff restart layer on top in
//! [`supervisor`](super::supervisor). The spawn target (`pyright-langserver
//! --stdio`, `tsserver`, ...) is constructed by the `adapters` crate as a
//! `tokio::process::Command` and passed in here, so this module stays
//! language-agnostic.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time;

use super::client::LspClient;

/// Grace period for each stage of the [`ServerProcess::shutdown`] escalation
/// (`lsp-lifecycle.md` Shutdown): the server gets this long to exit after
/// the `shutdown`/`exit` handshake, then again after SIGTERM, before SIGKILL.
/// Tests pass a shorter `grace` directly.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// The observed outcome of the supervised server exiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerExit {
    /// `true` if the process reported a zero status (clean exit).
    pub success: bool,
    /// The raw exit code, if one was reported (signals/aborts give `None`).
    pub code: Option<i32>,
}

/// A spawned language server process plus the [`LspClient`] driving its stdio
/// and an exit watcher.
///
/// The child is moved into an exit-watch task; the [`LspClient`] is driven by
/// the server's piped stdout/stdin. When the process exits the stdio stream
/// ends and the [`LspClient`]'s in-flight requests fail — the exit watcher
/// surfaces the *why* (code/success) for the caller's restart decision. The
/// child's pid is retained here (not in the exit-watch task) so
/// [`shutdown`](Self::shutdown) can escalate to SIGTERM/SIGKILL.
pub struct ServerProcess {
    client: LspClient,
    exit: watch::Receiver<Option<ServerExit>>,
    pid: Option<u32>,
}

impl ServerProcess {
    /// Spawn `command` as the language server with piped stdin/stdout (stderr
    /// inherited for diagnostics), wire its streams into a fresh [`LspClient`],
    /// and begin observing exit.
    pub fn spawn(command: &mut Command) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            // Backstop against orphaning this child if its owning `Child`
            // handle is ever dropped without being reaped (e.g. the
            // exit-watch task unwinding on a panic, or the runtime shutting
            // down mid-task) — the graceful paths (`shutdown`, `wait_exit`)
            // already consume the `Child` themselves, so this only fires on
            // the abnormal path.
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .with_context(|| "failed to spawn language server")?;
        let stdin = child.stdin.take().context("server stdin not piped")?;
        let stdout = child.stdout.take().context("server stdout not piped")?;
        // Capture the pid before the `Child` moves into the exit-watch task:
        // `shutdown` needs it to send SIGTERM/SIGKILL later. `None` only if the
        // pid is somehow already unavailable right after spawn.
        let pid = child.id();

        let (exit_tx, exit_rx) = watch::channel(None);
        tokio::spawn(wait_exit(child, exit_tx));

        // stdin/stdout are swapped deliberately: the client *reads* the server's
        // stdout and *writes* the server's stdin.
        let client = LspClient::spawn(stdout, stdin);
        Ok(Self {
            client,
            exit: exit_rx,
            pid,
        })
    }

    /// A cheap-to-clone handle to the [`LspClient`] driving this server.
    pub fn client(&self) -> LspClient {
        self.client.clone()
    }

    /// Clone of the exit watcher. `borrow_and_update()` / `changed().await`
    /// yield `Some(ServerExit)` once the process has terminated.
    pub fn exit_watcher(&self) -> watch::Receiver<Option<ServerExit>> {
        self.exit.clone()
    }

    /// Resolve once the server exits, returning its [`ServerExit`].
    pub async fn wait_for_exit(&mut self) -> ServerExit {
        if let Some(exit) = *self.exit.borrow() {
            return exit;
        }
        while self.exit.changed().await.is_ok() {
            if let Some(exit) = *self.exit.borrow() {
                return exit;
            }
        }
        // Sender dropped without publishing (task panicked): treat as a crash.
        ServerExit {
            success: false,
            code: None,
        }
    }

    /// Gracefully stop the server per `lsp-lifecycle.md` Shutdown:
    ///
    /// 1. `shutdown` request, then `exit` notification (LSP standard).
    /// 2. Wait up to `grace`; if it has not exited, send **SIGTERM**.
    /// 3. Wait up to `grace` more; if it still lives, send **SIGKILL**.
    ///
    /// Each stage is bounded by `grace`, so the worst case is ~3×`grace`. The
    /// server exiting at any stage short-circuits the rest. Signals are sent
    /// only while the exit watch is still pending: the exit-watch task publishes
    /// the outcome right after reaping the child, so a pending watch means the
    /// pid still belongs to our child and has not been reused.
    pub async fn shutdown(&mut self, grace: Duration) {
        if self.is_exited() {
            return;
        }

        // 1. LSP handshake. Best-effort: a server that never speaks LSP (or
        //    ignores stdio) will not reply, so bound the `shutdown` request by
        //    `grace`; the `exit` notification is fire-and-forget.
        let _ = time::timeout(grace, self.client.request("shutdown", None)).await;
        let _ = self.client.notify("exit", None).await;
        if self.wait_exit_bounded(grace).await {
            return;
        }

        // 2 & 3. Forced escalation. Unix-only: semnav targets darwin/linux, and
        //        pid-based signaling is the only way to send SIGTERM without
        //        retaining the `Child` (which lives in the exit-watch task). On
        //        a non-Unix build these stages vanish and the final bounded wait
        //        below is all that remains.
        #[cfg(unix)]
        {
            self.signal(libc::SIGTERM);
            if self.wait_exit_bounded(grace).await {
                return;
            }
            self.signal(libc::SIGKILL);
        }

        // Give the (now SIGKILLed, on Unix) process a final grace to be reaped
        // and published by the exit-watch task.
        self.wait_exit_bounded(grace).await;
    }

    /// True once the exit-watch task has observed the process terminating.
    fn is_exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// Resolve once the server exits, or after `grace` elapses. Returns `true`
    /// if it exited within the bound (re-checked after the timeout in case the
    /// watch published in the same instant the timer fired).
    async fn wait_exit_bounded(&mut self, grace: Duration) -> bool {
        if self.is_exited() {
            return true;
        }
        let timed_out = time::timeout(grace, async {
            loop {
                if self.exit.changed().await.is_err() {
                    return;
                }
                if self.exit.borrow().is_some() {
                    return;
                }
            }
        })
        .await
        .is_err();
        !timed_out || self.is_exited()
    }

    /// Send `sig` to the child, but only if we have a pid and have not already
    /// observed its exit. Returns `true` when a signal was actually delivered.
    #[cfg(unix)]
    fn signal(&self, sig: libc::c_int) -> bool {
        // Re-check the watch immediately before signaling: the exit-watch task
        // may have published since the caller's last check.
        if self.is_exited() {
            return false;
        }
        match self.pid {
            Some(pid) => {
                // SAFETY: `kill` is the POSIX signal-sending syscall. `pid` is
                // our own child's pid, captured at spawn; we only signal while
                // the watch is pending, i.e. before the exit-watch task reaps
                // the child, so the pid still belongs to this child.
                unsafe { libc::kill(pid as libc::pid_t, sig) == 0 }
            }
            None => false,
        }
    }
}

/// Wait for the child to exit and publish the outcome on `exit_tx`.
async fn wait_exit(mut child: Child, exit_tx: watch::Sender<Option<ServerExit>>) {
    let exit = match child.wait().await {
        Ok(status) => ServerExit {
            success: status.success(),
            code: status.code(),
        },
        Err(_) => ServerExit {
            success: false,
            code: None,
        },
    };
    let _ = exit_tx.send(Some(exit));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::process::Command;

    /// `true` exists on both darwin and linux and exits 0 immediately — a
    /// portable, dependency-free stand-in for a real language server.
    fn true_command() -> Command {
        Command::new("true")
    }

    #[tokio::test]
    async fn spawn_immediate_exit_observed_as_success() {
        let mut server = ServerProcess::spawn(&mut true_command()).expect("spawn");
        let exit = tokio::time::timeout(Duration::from_secs(2), server.wait_for_exit())
            .await
            .expect("exit observed within timeout");
        assert!(exit.success);
        assert_eq!(exit.code, Some(0));
    }

    #[tokio::test]
    async fn exit_watcher_publishes_after_spawn() {
        let server = ServerProcess::spawn(&mut true_command()).expect("spawn");
        let mut watcher = server.exit_watcher();
        // Either already published or will be shortly.
        let exit = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(e) = *watcher.borrow_and_update() {
                    return e;
                }
                watcher.changed().await.expect("watcher alive");
            }
        })
        .await
        .expect("exit published within timeout");
        assert!(exit.success);
    }

    #[tokio::test]
    async fn request_fails_once_process_has_exited() {
        // The process exits immediately; once the reader observes EOF every
        // pending request must surface an error rather than hang forever.
        let server = ServerProcess::spawn(&mut true_command()).expect("spawn");
        // Allow the process to exit and the reader task to notice the closed stream.
        tokio::time::sleep(Duration::from_millis(80)).await;
        let err = server
            .client()
            .request("initialize", None)
            .await
            .expect_err("request should fail on a dead stream");
        let msg = format!("{err}");
        assert!(
            msg.contains("closed") || msg.contains("stream") || msg.contains("code"),
            "unexpected error: {msg}"
        );
    }

    // --- graceful shutdown escalation (lsp-lifecycle.md Shutdown) ---

    /// A shell that stays alive and ignores the LSP frames written to its stdin.
    /// Portable to darwin/linux `sh`. Scripts use a short-sleep loop rather than
    /// one long `sleep`: a non-interactive shell defers a trapped signal's action
    /// until the current foreground command returns, so a long `sleep` would
    /// delay the trap by the whole sleep — the loop bounds that deferral to
    /// ~`step` and also bounds any orphaned child a trap leaves behind.
    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    /// `shutdown` must not signal (or wait a full grace) when the server has
    /// already exited on its own — `true` exits 0 immediately, so the watch
    /// publishes before any escalation stage.
    #[tokio::test]
    async fn shutdown_skips_escalation_when_already_exited() {
        let mut server = ServerProcess::spawn(&mut true_command()).expect("spawn");
        let grace = Duration::from_secs(2);
        let started = std::time::Instant::now();
        tokio::time::timeout(grace * 3, server.shutdown(grace))
            .await
            .expect("shutdown completes");
        assert!(
            started.elapsed() < grace,
            "shutdown waited longer than one grace (no fast-path): {:?}",
            started.elapsed()
        );
    }

    /// A server that ignores LSP but exits 42 on SIGTERM: only SIGTERM delivery
    /// can produce `code == Some(42)`, proving the escalation reached stage 2
    /// and SIGTERM alone sufficed (stage 3 was not needed). The short-sleep loop
    /// keeps the trap responsive (see [`shell`]) so it fires well within `grace`.
    #[tokio::test]
    async fn shutdown_sigterm_terminates_unresponsive() {
        let mut server = ServerProcess::spawn(&mut shell(
            "trap 'exit 42' TERM; while :; do sleep 0.05; done",
        ))
        .expect("spawn");
        let exit = tokio::time::timeout(Duration::from_secs(10), async {
            server.shutdown(Duration::from_millis(500)).await;
            server.wait_for_exit().await
        })
        .await
        .expect("shutdown terminates within budget");
        assert_eq!(
            exit.code,
            Some(42),
            "SIGTERM trap should make the shell exit 42, got {exit:?}"
        );
    }

    /// A server that ignores SIGTERM entirely: only SIGKILL can stop it, and a
    /// SIGKILL death reports no exit code (signal) — proving the stage-3 backstop.
    #[tokio::test]
    async fn shutdown_sigkill_when_sigterm_ignored() {
        let mut server = ServerProcess::spawn(&mut shell("trap '' TERM; sleep 10")).expect("spawn");
        let exit = tokio::time::timeout(Duration::from_secs(10), async {
            server.shutdown(Duration::from_millis(500)).await;
            server.wait_for_exit().await
        })
        .await
        .expect("shutdown terminates within budget");
        assert!(
            exit.code.is_none(),
            "SIGKILL death carries no exit code, got {exit:?}"
        );
        assert!(!exit.success);
    }
}
