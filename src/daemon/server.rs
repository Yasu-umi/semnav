//! The daemon's accept loop: reads framed [`DaemonRequest`]s off each
//! connection and dispatches them straight to [`SemnavServer`]'s existing
//! inherent tool methods (bypassing rmcp's own dispatcher entirely — this
//! link isn't MCP, see `protocol.rs`), tracks idle time across connections,
//! and returns once told to stop (signal, explicit `daemon stop`, or idle
//! timeout) so the caller (`main.rs`) can run the same teardown sequence
//! `run_serve` already uses.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json};
use serde::Serialize;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

use crate::mcp::SemnavServer;

use super::protocol::{
    DaemonEnvelope, DaemonRequest, DaemonResponseEnvelope, read_line, write_line,
};

/// Default idle window before a daemon with zero active connections
/// self-terminates. Overridden by `SEMNAV_DAEMON_IDLE_TIMEOUT_SECS`.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How often the accept loop re-checks the idle condition. Independent of
/// `DEFAULT_IDLE_TIMEOUT` — just a poll granularity.
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// `SEMNAV_DAEMON_IDLE_TIMEOUT_SECS` if set (mirrors the `SEMNAV_CACHE_DIR`
/// convention in `main.rs`), else [`DEFAULT_IDLE_TIMEOUT`].
pub fn idle_timeout_from_env() -> Duration {
    std::env::var("SEMNAV_DAEMON_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_IDLE_TIMEOUT)
}

/// Pure connection-count/idle-clock bookkeeping, independent of any socket or
/// async runtime so it's deterministically unit-testable with real `Instant`
/// arithmetic instead of a mocked clock.
#[derive(Debug)]
struct IdleTracker {
    active: usize,
    /// `Some(t)` ⇒ has been at `active == 0` continuously since `t`.
    /// `None` while `active > 0`.
    idle_since: Option<Instant>,
}

impl IdleTracker {
    fn new(now: Instant) -> Self {
        Self {
            active: 0,
            idle_since: Some(now),
        }
    }

    fn on_connect(&mut self) {
        self.active += 1;
        self.idle_since = None;
    }

    fn on_disconnect(&mut self, now: Instant) {
        self.active = self.active.saturating_sub(1);
        if self.active == 0 {
            self.idle_since = Some(now);
        }
    }

    fn should_shut_down(&self, now: Instant, timeout: Duration) -> bool {
        match self.idle_since {
            Some(since) => self.active == 0 && now.saturating_duration_since(since) >= timeout,
            None => false,
        }
    }
}

/// Why the accept loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    /// The process-level shutdown signal fired (ctrl-c/SIGTERM).
    Signal,
    /// A connection sent [`DaemonRequest::Shutdown`] (`daemon stop`).
    ExplicitStop,
    /// No active connections for at least the idle timeout.
    Idle,
}

/// Run the accept loop until told to stop. `shutdown_rx` is the same
/// ctrl-c/SIGTERM watch `run_serve` uses (see `main.rs::install_shutdown_signal`).
pub async fn run(
    semnav_server: SemnavServer,
    listener: UnixListener,
    idle_timeout: Duration,
    mut shutdown_rx: watch::Receiver<bool>,
) -> ShutdownReason {
    let tracker = Arc::new(Mutex::new(IdleTracker::new(Instant::now())));
    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    let next_conn_id = Arc::new(AtomicU64::new(0));

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.changed() => return ShutdownReason::Signal,
            _ = stop_rx.recv() => return ShutdownReason::ExplicitStop,
            _ = tokio::time::sleep(IDLE_CHECK_INTERVAL) => {
                let idle = tracker.lock().expect("idle tracker poisoned").should_shut_down(Instant::now(), idle_timeout);
                if idle {
                    return ShutdownReason::Idle;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _addr)) = accepted else { continue };
                tracker.lock().expect("idle tracker poisoned").on_connect();
                let conn_id = next_conn_id.fetch_add(1, Ordering::Relaxed);
                let semnav_server = semnav_server.clone();
                let stop_tx = stop_tx.clone();
                let tracker = tracker.clone();
                tokio::spawn(async move {
                    handle_connection(conn_id, semnav_server, stream, stop_tx).await;
                    tracker.lock().expect("idle tracker poisoned").on_disconnect(Instant::now());
                });
            }
        }
    }
}

/// Serve one connection until it sends `Shutdown`, closes cleanly, or a
/// protocol/IO error occurs.
async fn handle_connection(
    conn_id: u64,
    semnav_server: SemnavServer,
    stream: UnixStream,
    stop_tx: mpsc::Sender<()>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    loop {
        let envelope: DaemonEnvelope = match read_line(&mut reader).await {
            Ok(Some(envelope)) => envelope,
            Ok(None) => return,
            Err(err) => {
                eprintln!("daemon: connection {conn_id}: malformed request: {err:#}");
                return;
            }
        };

        if matches!(envelope.request, DaemonRequest::Shutdown) {
            let _ = stop_tx.send(()).await;
            let response = DaemonResponseEnvelope {
                id: envelope.id,
                result: Ok(serde_json::Value::Null),
            };
            let _ = write_line(&mut write_half, &response).await;
            return;
        }

        let result = dispatch(&semnav_server, envelope.request).await;
        let response = DaemonResponseEnvelope {
            id: envelope.id,
            result,
        };
        if write_line(&mut write_half, &response).await.is_err() {
            return;
        }
    }
}

/// Route one request to the matching `SemnavServer` inherent method, reducing
/// its `Result<Json<T>, ErrorData>` to the protocol's `Result<Value, String>`.
async fn dispatch(
    semnav_server: &SemnavServer,
    request: DaemonRequest,
) -> Result<serde_json::Value, String> {
    match request {
        DaemonRequest::FindSymbol(input) => {
            to_result(semnav_server.find_symbol(Parameters(input)).await)
        }
        DaemonRequest::FindDefinition(input) => {
            to_result(semnav_server.find_definition(Parameters(input)).await)
        }
        DaemonRequest::FindReferences(input) => {
            to_result(semnav_server.find_references(Parameters(input)).await)
        }
        DaemonRequest::FindCallers(input) => {
            to_result(semnav_server.find_callers(Parameters(input)).await)
        }
        DaemonRequest::FindCallees(input) => {
            to_result(semnav_server.find_callees(Parameters(input)).await)
        }
        DaemonRequest::FindCallPath(input) => {
            to_result(semnav_server.find_call_path(Parameters(input)).await)
        }
        DaemonRequest::ReadRange(input) => {
            to_result(semnav_server.read_range(Parameters(input)).await)
        }
        DaemonRequest::RestartLsp(input) => {
            to_result(semnav_server.restart_lsp(Parameters(input)).await)
        }
        // Handled by the caller before reaching `dispatch`.
        DaemonRequest::Shutdown => Ok(serde_json::Value::Null),
    }
}

fn to_result<T: Serialize>(
    outcome: Result<Json<T>, ErrorData>,
) -> Result<serde_json::Value, String> {
    match outcome {
        Ok(Json(value)) => serde_json::to_value(value).map_err(|e| e.to_string()),
        Err(err) => Err(err.message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_with_zero_connections_is_idle_from_construction() {
        let t0 = Instant::now();
        let tracker = IdleTracker::new(t0);
        assert!(!tracker.should_shut_down(t0, Duration::from_secs(60)));
        assert!(tracker.should_shut_down(t0 + Duration::from_secs(61), Duration::from_secs(60)));
    }

    #[test]
    fn connecting_disarms_the_idle_timer() {
        let t0 = Instant::now();
        let mut tracker = IdleTracker::new(t0);
        tracker.on_connect();
        assert!(!tracker.should_shut_down(t0 + Duration::from_secs(3600), Duration::from_secs(60)));
    }

    #[test]
    fn disconnecting_to_zero_restarts_the_idle_clock_from_now() {
        let t0 = Instant::now();
        let mut tracker = IdleTracker::new(t0);
        tracker.on_connect();
        let disconnect_at = t0 + Duration::from_secs(100);
        tracker.on_disconnect(disconnect_at);
        // Not yet timed out relative to the disconnect time, not t0.
        assert!(!tracker.should_shut_down(
            disconnect_at + Duration::from_secs(10),
            Duration::from_secs(60)
        ));
        assert!(tracker.should_shut_down(
            disconnect_at + Duration::from_secs(61),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn multiple_connections_only_go_idle_once_all_disconnect() {
        let t0 = Instant::now();
        let mut tracker = IdleTracker::new(t0);
        tracker.on_connect();
        tracker.on_connect();
        tracker.on_disconnect(t0 + Duration::from_secs(10));
        // One connection is still active.
        assert!(!tracker.should_shut_down(t0 + Duration::from_secs(9999), Duration::from_secs(60)));
        tracker.on_disconnect(t0 + Duration::from_secs(20));
        assert!(tracker.should_shut_down(t0 + Duration::from_secs(200), Duration::from_secs(60)));
    }

    // One test, not two: `std::env::set_var`/`remove_var` mutate global
    // process state, so two tests toggling the same var race under the
    // default parallel test harness. Both assertions share ownership of the
    // var's lifecycle within a single test function to avoid that.
    #[test]
    fn idle_timeout_from_env_defaults_when_unset_and_parses_override() {
        unsafe { std::env::remove_var("SEMNAV_DAEMON_IDLE_TIMEOUT_SECS") };
        assert_eq!(idle_timeout_from_env(), DEFAULT_IDLE_TIMEOUT);

        unsafe { std::env::set_var("SEMNAV_DAEMON_IDLE_TIMEOUT_SECS", "120") };
        assert_eq!(idle_timeout_from_env(), Duration::from_secs(120));

        unsafe { std::env::remove_var("SEMNAV_DAEMON_IDLE_TIMEOUT_SECS") };
    }

    /// Real pyright, end-to-end: index a module, run the accept loop over a
    /// real Unix socket, and drive it through two *sequential, independent*
    /// connections (simulating two separate `serve` proxy sessions) —
    /// proving the LSP server's warm background-analysis state survives a
    /// connection closing and a new one opening, which is the entire point
    /// of the daemon. Ignored by default — it needs node/npm and provisions
    /// pyright from npm on first run.
    #[ignore = "requires node/npm; provisions pyright from npm on first run"]
    #[tokio::test]
    async fn daemon_stays_warm_across_two_sequential_connections() {
        use crate::daemon::protocol::{
            DaemonEnvelope, DaemonRequest, DaemonResponseEnvelope, read_line, write_line,
        };
        use crate::graph::DbActor;
        use crate::indexer::index_language;
        use crate::mcp::dto::{AtRef, FindDefinitionInput};
        use crate::query::{QueryEngine, QueryRuntime};
        use tokio::net::{UnixListener, UnixStream};
        use tokio::sync::watch;

        let dir = tempfile::tempdir().expect("tempdir");
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        fs_write_helper_module(&app);

        let root_uri = format!("file://{}/", dir.path().display());
        let cache_dir = dir.path().join(".semnav");
        let servers_dir = cache_dir.join("servers");
        let db_path = cache_dir.join("graph.db");
        std::fs::create_dir_all(&servers_dir).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");
        index_language(&db, "python", &root_uri, &servers_dir)
            .await
            .expect("index python");

        let engine = QueryEngine::new(db, root_uri.clone());
        let query_runtime = Arc::new(QueryRuntime::open(engine, servers_dir));
        let semnav_server = SemnavServer::new(query_runtime.clone());

        let sock_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let server_task = tokio::spawn(run(
            semnav_server,
            listener,
            Duration::from_secs(300),
            shutdown_rx,
        ));

        let uri = format!("{}app/mod.py", root_uri);
        let find_definition_at_usage = |id: u64| DaemonEnvelope {
            id,
            request: DaemonRequest::FindDefinition(FindDefinitionInput {
                at: AtRef {
                    uri: uri.clone(),
                    line: 4,
                    character: 11,
                },
            }),
        };

        // Connection 1: pays whatever cold-scan cost pyright has.
        let stream1 = UnixStream::connect(&sock_path).await.unwrap();
        let (read1, mut write1) = stream1.into_split();
        let mut reader1 = BufReader::new(read1);
        write_line(&mut write1, &find_definition_at_usage(1))
            .await
            .unwrap();
        let response1: DaemonResponseEnvelope = read_line(&mut reader1).await.unwrap().unwrap();
        let value1 = response1.result.expect("connection 1 resolves definition");
        assert_eq!(value1["nodes"][0]["name"], "helper");
        assert!(
            value1.get("degraded").is_none(),
            "connection 1 must not be degraded"
        );
        drop(write1);
        drop(reader1);

        // Connection 2: independent connection to the *same* daemon — the
        // LSP server underneath must already be warm.
        let stream2 = UnixStream::connect(&sock_path).await.unwrap();
        let (read2, mut write2) = stream2.into_split();
        let mut reader2 = BufReader::new(read2);
        write_line(&mut write2, &find_definition_at_usage(2))
            .await
            .unwrap();
        let response2: DaemonResponseEnvelope = read_line(&mut reader2).await.unwrap().unwrap();
        let value2 = response2.result.expect("connection 2 resolves definition");
        assert_eq!(value2["nodes"][0]["name"], "helper");
        assert!(
            value2.get("degraded").is_none(),
            "connection 2 must not be degraded"
        );

        // Explicit stop, mirroring `daemon stop`.
        write_line(
            &mut write2,
            &DaemonEnvelope {
                id: 3,
                request: DaemonRequest::Shutdown,
            },
        )
        .await
        .unwrap();
        let reason = server_task.await.expect("accept loop task");
        assert_eq!(reason, ShutdownReason::ExplicitStop);

        query_runtime.shutdown_all().await;
    }

    fn fs_write_helper_module(app: &std::path::Path) {
        std::fs::write(
            app.join("mod.py"),
            "def helper():\n    return 1\n\ndef caller():\n    return helper()\n",
        )
        .unwrap();
    }
}
