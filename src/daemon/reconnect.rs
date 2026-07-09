//! Transparent reconnect for `serve`'s daemon link.
//!
//! `run_serve` used to hand `ProxyServer` a single [`DaemonClient`] connected
//! once at startup and never revisited (`docs/design/daemon-lifecycle.md`).
//! `serve` is long-lived — it lives for the whole MCP client session, often
//! far longer than the daemon's 30-minute idle timeout — while a daemon can
//! disappear out from under it at any time: idle shutdown, an explicit
//! `daemon stop` (e.g. `rebuild-daemon` picking up a freshly built binary),
//! or a crash. Once that happened, every tool call on that `serve` process
//! failed forever with "daemon client closed", because nothing ever told it
//! to look for a new daemon. [`ReconnectingDaemonClient`] closes that gap: on
//! a call that fails because the connection died, it re-runs
//! [`ensure_and_connect`] and retries the call once against the fresh
//! connection.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use tokio::sync::RwLock;

use super::client::DaemonClient;
use super::connect::ensure_and_connect;
use super::protocol::DaemonRequest;

/// Cheap-to-clone handle around a [`DaemonClient`] that re-establishes the
/// connection when it dies, instead of failing every call from then on.
#[derive(Clone)]
pub struct ReconnectingDaemonClient {
    root: PathBuf,
    cache_dir: PathBuf,
    current: Arc<RwLock<DaemonClient>>,
}

impl ReconnectingDaemonClient {
    pub fn new(root: PathBuf, cache_dir: PathBuf, initial: DaemonClient) -> Self {
        Self {
            root,
            cache_dir,
            current: Arc::new(RwLock::new(initial)),
        }
    }

    /// Forward `request` to the daemon, transparently reconnecting once if
    /// the current connection turns out to be dead — either already, or as
    /// the cause of this call's own failure. A genuine tool/protocol error
    /// from a live daemon is returned as-is, with no retry (retrying it
    /// against a fresh connection would just reproduce the same failure).
    pub async fn call(&self, request: DaemonRequest) -> Result<serde_json::Value> {
        let client = self.current.read().await.clone();
        let client = if client.is_closed() {
            self.reconnect().await?
        } else {
            client
        };

        match client.call(request.clone()).await {
            Ok(value) => Ok(value),
            Err(_) if client.is_closed() => self.reconnect().await?.call(request).await,
            Err(err) => Err(err),
        }
    }

    /// Re-run `ensure_and_connect` and swap in the fresh client. Checks
    /// whether another caller already reconnected while this one waited for
    /// the write lock, so concurrent callers hitting the same dead
    /// connection don't each spawn/connect redundantly.
    async fn reconnect(&self) -> Result<DaemonClient> {
        let mut guard = self.current.write().await;
        if !guard.is_closed() {
            return Ok(guard.clone());
        }
        let fresh = ensure_and_connect(&self.root, &self.cache_dir)
            .await
            .map_err(|e| anyhow!("daemon reconnect failed: {e}"))?;
        *guard = fresh.clone();
        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::discovery;
    use crate::daemon::protocol::{DaemonEnvelope, DaemonResponseEnvelope, read_line, write_line};
    use crate::mcp::dto::RestartLspInput;
    use tokio::io::{AsyncRead, AsyncWrite, BufReader};
    use tokio::net::UnixListener;
    use tokio::sync::watch;

    /// Answers every request with `{"restarted": []}` on one accepted
    /// connection until `stop` fires.
    async fn handle_connection<R, W>(reader: R, mut writer: W, stop: &mut watch::Receiver<bool>)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        loop {
            tokio::select! {
                _ = stop.changed() => return,
                line = read_line::<_, DaemonEnvelope>(&mut reader) => match line {
                    Ok(Some(envelope)) => {
                        let response = DaemonResponseEnvelope {
                            id: envelope.id,
                            result: Ok(serde_json::json!({"restarted": []})),
                        };
                        if write_line(&mut writer, &response).await.is_err() {
                            return;
                        }
                    }
                    _ => return,
                },
            }
        }
    }

    /// A real `UnixListener` under a temp root, so `ensure_and_connect`'s
    /// liveness probe + connect can succeed against it exactly like a real
    /// spawned daemon — proving the reconnect path end-to-end rather than
    /// just the in-memory duplex plumbing. Accepts in a loop (one handler
    /// task per connection) rather than a single one-shot `accept()`,
    /// because `ensure_and_connect` itself opens a short-lived probe
    /// connection before the real one — a one-shot listener would hand that
    /// probe its only accept and die the instant the probe disconnects.
    async fn spawn_fake_daemon_at(cache_dir: &std::path::Path) -> watch::Sender<bool> {
        tokio::fs::create_dir_all(cache_dir).await.unwrap();
        let sock = discovery::sock_path(cache_dir);
        let listener = UnixListener::bind(&sock).unwrap();
        let (stop_tx, stop_rx) = watch::channel(false);
        tokio::spawn(async move {
            loop {
                let mut stop_rx = stop_rx.clone();
                tokio::select! {
                    _ = stop_rx.changed() => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        tokio::spawn(async move {
                            let (r, w) = stream.into_split();
                            handle_connection(r, w, &mut stop_rx).await;
                        });
                    }
                }
            }
        });
        stop_tx
    }

    #[tokio::test]
    async fn reconnects_after_the_daemon_dies_once_a_replacement_is_listening() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let cache_dir = dir.path().join("root/.semnav");
        tokio::fs::create_dir_all(&root).await.unwrap();

        let stop_tx = spawn_fake_daemon_at(&cache_dir).await;
        let sock = discovery::sock_path(&cache_dir);
        let initial = DaemonClient::connect(&sock).await.unwrap();
        let reconnecting = ReconnectingDaemonClient::new(root.clone(), cache_dir.clone(), initial);

        let ok = reconnecting
            .call(DaemonRequest::RestartLsp(RestartLspInput {
                language: None,
            }))
            .await;
        assert!(ok.is_ok(), "call against the live daemon must succeed");

        // Kill the daemon and remove its socket, exactly like probe_liveness
        // would find a crashed one, then stand up a replacement at the same
        // path — `ensure_and_connect` finds it live immediately and never
        // needs to actually spawn a process.
        let _ = stop_tx.send(true);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let _ = std::fs::remove_file(&sock);
        let _stop_tx2 = spawn_fake_daemon_at(&cache_dir).await;

        let recovered = reconnecting
            .call(DaemonRequest::RestartLsp(RestartLspInput {
                language: None,
            }))
            .await;
        assert!(
            recovered.is_ok(),
            "call after the daemon died must recover via reconnect, got {recovered:?}"
        );
    }
}
