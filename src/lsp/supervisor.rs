//! Per-language-server health state machine + exponential-backoff restart.
//!
//! Drives one LSP server process through the lifecycle defined in
//! `docs/design/lsp-lifecycle.md` ("Health State Machine" / "Restart Policy" /
//! "Initialization Failure"): `not_started → starting → healthy → restarting → down`, with
//! 1→2→4→8→16s backoff, a `down` state after 5 consecutive failures, and a 30s
//! background retry out of `down`. Health is persisted to the `index_meta` KV
//! (`<lang>.lsp_status` / `<lang>.lsp_last_success_at` /
//! `<lang>.lsp_consecutive_failures`) — **log/debug-only in 0.0.1**, since no
//! `graph status` tool reads it yet.
//!
//! Architecture (house style, `docs/design/crate-structure.md` Decision Point 4): a plain
//! `tokio::spawn` actor owns all state as locals (no module-level state, no
//! shared `Mutex`), driven by an `mpsc` command channel with `oneshot` replies.
//! The [`ServerFactory`] seam is a **native async trait used as a generic
//! parameter** (not `dyn`) — consistent with `SymbolFetcher`, no `async-trait`
//! crate, no boxed futures — and the type parameter is hidden behind a
//! non-generic [`SupervisorHandle`] so callers never see it.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Sleep};

use crate::adapters::{ProvisionContext, adapter_for_language, provision};
use crate::graph::DbActor;
use crate::lsp::{LspClient, SHUTDOWN_GRACE, ServerExit, ServerProcess, initialize};

/// Best-effort sink for the supervisor's health record (the `index_meta` KV in
/// production). A seam, not a full db abstraction: the supervisor only *writes*
/// (`<lang>.lsp_status` / `lsp_consecutive_failures` / `lsp_last_success_at`);
/// a future `graph status` tool (Step 5+) reads them back.
///
/// The real [`DbActor`] drives SQLite on `spawn_blocking`, which stalls tokio's
/// paused-time auto-advance — so tests substitute an in-memory `MetaStore` and
/// keep their `start_paused` state-machine tests deterministic. The trait method
/// returns an explicit `impl Future + Send` (native async-in-trait, no
/// `async-trait` crate) so the spawned supervisor task stays `Send`.
pub trait MetaStore: Clone + Send + Sync + 'static {
    /// Upsert `key`/`value`. The caller ignores the outcome (fire-and-forget).
    fn record_meta(&self, key: String, value: String) -> impl Future<Output = ()> + Send;
}

impl MetaStore for DbActor {
    fn record_meta(&self, key: String, value: String) -> impl Future<Output = ()> + Send {
        let db = self.clone();
        async move {
            let _ = db.set_meta(&key, &value).await;
        }
    }
}

/// Lifecycle state of one language server (`lsp-lifecycle.md` Health State Machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    NotStarted,
    Starting,
    Healthy,
    Restarting,
    Down,
}

impl ServerState {
    /// The string written to `<lang>.lsp_status` in `index_meta`.
    fn as_str(self) -> &'static str {
        match self {
            ServerState::NotStarted => "not_started",
            ServerState::Starting => "starting",
            ServerState::Healthy => "healthy",
            ServerState::Restarting => "restarting",
            ServerState::Down => "down",
        }
    }
}

/// Why a server failed to start. The split drives the restart policy:
/// [`StartError::Provision`] → `down` directly with **no** background retry
/// (the runtime itself is missing; retrying is pointless until it is fixed),
/// [`StartError::SpawnOrInit`] → exponential backoff restart (a transient
/// crash/timeout).
#[derive(Debug)]
pub enum StartError {
    /// Provisioning failed (node/npm missing, install failed). No retry.
    Provision(String),
    /// Spawn or `initialize` handshake failed (process crash, timeout). Retry.
    SpawnOrInit(String),
}

/// Why [`SupervisorHandle::acquire`] could not hand out a client.
#[derive(Debug)]
pub enum AcquireError {
    /// The server is `down` (and will not be retried automatically on a
    /// provision failure). Carries a human-readable reason.
    Down(String),
    /// The server is mid-restart; the caller may retry shortly.
    Restarting,
    /// The (first) start attempt failed but a restart is scheduled. The caller
    /// may retry after the backoff.
    StartFailed(String),
}

/// The three anomaly kinds the supervisor reacts to (`lsp-lifecycle.md` Failure Detection).
///
/// `ChildExit` is observed directly from the exit watcher; `Transport` /
/// `Timeout` are reported by the holder of a plain [`LspClient`] via
/// [`SupervisorHandle::report_failure`], classified from the error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    ChildExit,
    Transport,
    Timeout,
}

impl From<&anyhow::Error> for FailureKind {
    /// Classify an LSP-round-trip error: a "timed out" message is a
    /// [`FailureKind::Timeout`]; stream/write failures (and anything else) are
    /// [`FailureKind::Transport`]. [`FailureKind::ChildExit`] never comes from
    /// an error — it is observed from the exit watcher.
    fn from(e: &anyhow::Error) -> Self {
        let msg = format!("{e}");
        if msg.contains("timed out") {
            FailureKind::Timeout
        } else {
            FailureKind::Transport
        }
    }
}

/// Backoff schedule, failure threshold, and `down`-retry interval. The default
/// real policy matches the design (1→2→4→8→16s, 5 failures, 30s retry); tests
/// construct tighter policies directly.
#[derive(Clone, Copy)]
pub struct RestartPolicy {
    backoff: [Duration; 5],
    max_failures: u32,
    down_retry: Duration,
}

impl RestartPolicy {
    /// 1→2→4→8→16s backoff, 5 consecutive failures → `down`, 30s background retry.
    pub fn default_real() -> Self {
        Self {
            backoff: [
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
            ],
            max_failures: 5,
            down_retry: Duration::from_secs(30),
        }
    }

    /// The wait before the `n`-th (0-indexed) restart attempt, clamped to the
    /// last slot so a long failure storm does not exceed 16s between tries.
    fn nth_backoff(self, n: u32) -> Duration {
        let idx = std::cmp::min(n, 4) as usize;
        self.backoff[idx]
    }
}

/// A started, healthy server: a client handle, its exit watcher, and the
/// keep-alive that holds the underlying [`ServerProcess`] (and its child) alive.
///
/// `keep_alive` is `None` for in-process mocks (no real child to hold).
pub struct StartedServer {
    pub client: LspClient,
    pub exit: watch::Receiver<Option<ServerExit>>,
    pub keep_alive: Option<ServerProcess>,
}

/// How to obtain a started server. Native async-in-trait, used as a **generic
/// parameter** (not `dyn`) so there is no boxed future and no `async-trait`
/// crate. The return is an explicit `impl Future + Send` (not `async fn`) so
/// the spawned supervisor task — which awaits `start()` across an await point —
/// knows the future is `Send`; this mirrors `SymbolFetcher`.
pub trait ServerFactory: Send + Sync + 'static {
    /// Provision + spawn + handshake, returning the live server or a
    /// [`StartError`] whose variant drives the restart policy.
    fn start(&self) -> impl Future<Output = std::result::Result<StartedServer, StartError>> + Send;
}

/// Production factory: provision the real server for `language`, spawn it,
/// and run the `initialize` handshake.
pub struct RealServerFactory {
    pub language: String,
    pub servers_dir: PathBuf,
    pub root_uri: String,
    pub workspace_name: String,
}

impl RealServerFactory {
    /// Derive the handshake `workspaceName` from a root URI — the root's last
    /// non-empty, non-scheme path segment (`file:///repo/myapp/` ⇒ `myapp`).
    /// Falls back to `"workspace"` for bare roots (`file:///`, `""`) so a
    /// degenerate URI never yields a scheme fragment like `"file:"`.
    pub fn workspace_name_for(root_uri: &str) -> String {
        root_uri
            .trim_end_matches('/')
            .rsplit('/')
            .find(|seg| !seg.is_empty() && !seg.contains(':'))
            .unwrap_or("workspace")
            .to_string()
    }
}

impl ServerFactory for RealServerFactory {
    fn start(&self) -> impl Future<Output = std::result::Result<StartedServer, StartError>> + Send {
        let language = self.language.clone();
        let servers_dir = self.servers_dir.clone();
        let root_uri = self.root_uri.clone();
        let workspace_name = self.workspace_name.clone();
        async move {
            let adapter = adapter_for_language(&language)
                .ok_or_else(|| StartError::Provision(format!("no adapter for {language}")))?;
            let mut cmd = provision(
                adapter,
                &ProvisionContext {
                    servers_dir: servers_dir.clone(),
                },
            )
            .await
            .map_err(|e| StartError::Provision(format!("provision: {e}")))?;

            let server = ServerProcess::spawn(&mut cmd)
                .map_err(|e| StartError::SpawnOrInit(format!("spawn: {e}")))?;
            let client = server.client();
            initialize(&client, &root_uri, &workspace_name)
                .await
                .map_err(|e| StartError::SpawnOrInit(format!("initialize: {e}")))?;
            Ok(StartedServer {
                client,
                exit: server.exit_watcher(),
                keep_alive: Some(server),
            })
        }
    }
}

/// Command sent to the supervisor actor.
enum SupervisorMsg {
    Acquire {
        reply: oneshot::Sender<std::result::Result<LspClient, AcquireError>>,
    },
    ReportFailure {
        kind: FailureKind,
        reply: oneshot::Sender<()>,
    },
    /// Explicit teardown: run graceful shutdown on the live server and stop the
    /// actor. Acknowledged once the escalation completes.
    Shutdown { reply: oneshot::Sender<()> },
}

/// Cheap-to-clone handle to one language server's supervisor.
///
/// **Teardown** runs the graceful escalation of `lsp-lifecycle.md` Shutdown
/// (`shutdown`→`exit`→SIGTERM→SIGKILL) on the live server:
/// - [`shutdown`](SupervisorHandle::shutdown) awaits the full escalation —
///   callers that must guarantee the child is reaped before the runtime tears
///   down (the `index` CLI, the MCP server) use it.
/// - Dropping the last clone also triggers the escalation, but detached: it
///   races runtime teardown, so it is best-effort.
#[derive(Clone)]
pub struct SupervisorHandle {
    tx: mpsc::Sender<SupervisorMsg>,
}

impl SupervisorHandle {
    /// Acquire a client to a healthy server, starting it (lazily) on first
    /// call. Returns the error variant if the server is down or mid-restart.
    pub async fn acquire(&self) -> std::result::Result<LspClient, AcquireError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(SupervisorMsg::Acquire { reply: tx })
            .await
            .map_err(|_| AcquireError::Down("supervisor actor closed".into()))?;
        rx.await
            .map_err(|_| AcquireError::Down("supervisor dropped reply".into()))?
    }

    /// Report a transport/timeout anomaly observed by the holder of a client.
    /// Triggers a restart if the server was healthy; ignored otherwise.
    pub async fn report_failure(&self, kind: FailureKind) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(SupervisorMsg::ReportFailure { kind, reply: tx })
            .await
            .map_err(|_| anyhow::anyhow!("supervisor actor closed"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("supervisor dropped reply"))?;
        Ok(())
    }

    /// Gracefully shut down the supervised server and stop the actor. Awaits the
    /// full `shutdown`→`exit`→SIGTERM→SIGKILL escalation (worst case
    /// ~3×`SHUTDOWN_GRACE`). After this returns the actor is gone; calls on any
    /// clone surface [`AcquireError::Down`]. Callers that must guarantee the
    /// child is reaped before the runtime tears down (the `index` CLI, the MCP
    /// server) use this instead of relying on drop.
    pub async fn shutdown(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(SupervisorMsg::Shutdown { reply: tx })
            .await
            .map_err(|_| anyhow::anyhow!("supervisor actor closed"))?;
        let _ = rx.await;
        Ok(())
    }
}

/// Namespace for the generic `spawn`; the handle carries no type parameter.
pub struct ServerSupervisor;

impl ServerSupervisor {
    /// Spawn the supervisor actor for `language`. `db` records health to
    /// `index_meta`; `factory` provisions/starts the server; `policy` sets the
    /// backoff/down-retry schedule.
    pub fn spawn<F: ServerFactory, M: MetaStore>(
        db: M,
        factory: F,
        language: &str,
        policy: RestartPolicy,
    ) -> SupervisorHandle {
        let (tx, rx) = mpsc::channel::<SupervisorMsg>(64);
        let actor = Supervisor {
            factory,
            db,
            language: language.to_string(),
            policy,
            state: ServerState::NotStarted,
            current_client: None,
            current_exit: None,
            current_keepalive: None,
            failures: 0,
            last_success_at: 0,
            restart_timer: None,
            rx,
        };
        tokio::spawn(actor.run());
        SupervisorHandle { tx }
    }
}

/// All supervisor state, owned by the actor task as locals.
struct Supervisor<F: ServerFactory, M: MetaStore> {
    factory: F,
    db: M,
    language: String,
    policy: RestartPolicy,
    state: ServerState,
    current_client: Option<LspClient>,
    current_exit: Option<watch::Receiver<Option<ServerExit>>>,
    current_keepalive: Option<ServerProcess>,
    failures: u32,
    last_success_at: i64,
    restart_timer: Option<Pin<Box<Sleep>>>,
    rx: mpsc::Receiver<SupervisorMsg>,
}

/// The outcome of an attempted start, for the caller to translate into an
/// [`AcquireError`] reply or ignore (timer-driven attempts have no caller).
enum StartOutcome {
    /// Now healthy; carries a fresh client clone for an `Acquire` reply.
    Started(LspClient),
    /// Provision failure → went straight to `down` with no retry armed.
    ProvisionFailed,
    /// Spawn/init failure → went to `restarting` (or `down` at the threshold).
    SpawnOrInitFailed,
}

impl<F: ServerFactory, M: MetaStore> Supervisor<F, M> {
    async fn run(mut self) {
        loop {
            tokio::select! {
                biased;
                // Anomaly #1: child exit (only an anomaly while healthy; an
                // exit observed mid-restart is the old server finishing).
                Some(_) = async {
                    match self.current_exit.as_mut() {
                        Some(rx) => rx.changed().await.ok(),
                        None => std::future::pending::<Option<()>>().await,
                    }
                } => {
                    if self.state == ServerState::Healthy {
                        self.transition_after_anomaly().await;
                    }
                }
                // Backoff / down-retry timer.
                _ = async {
                    match self.restart_timer.as_mut() {
                        Some(t) => { t.await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.restart_timer = None;
                    self.on_restart_timer().await;
                }
                msg = self.rx.recv() => match msg {
                    Some(SupervisorMsg::Acquire { reply }) => {
                        let res = self.handle_acquire().await;
                        let _ = reply.send(res);
                    }
                    Some(SupervisorMsg::ReportFailure { kind, reply }) => {
                        // 0.0.1 reacts to every anomaly kind (child-exit /
                        // transport / timeout) identically — backoff restart.
                        // `kind` is read here so the field stays live for the
                        // future per-kind routing/logging; no branching yet.
                        let _ = kind;
                        if self.state == ServerState::Healthy {
                            self.transition_after_anomaly().await;
                        }
                        let _ = reply.send(());
                    }
                    Some(SupervisorMsg::Shutdown { reply }) => {
                        // Explicit teardown: escalate the live server, then stop.
                        self.shutdown_graceful().await;
                        let _ = reply.send(());
                        break;
                    }
                    None => {
                        // All handles dropped: best-effort graceful shutdown of
                        // the live server before the actor exits. Callers that
                        // need to *guarantee* completion call `shutdown().await`
                        // explicitly — this detached path races runtime teardown.
                        self.shutdown_graceful().await;
                        break;
                    }
                }
            }
        }
    }

    /// A fresh attempt to start the server. Updates state, writes `index_meta`,
    /// and returns the outcome. The `Ok` path also stores the live server.
    async fn attempt_start(&mut self) -> StartOutcome {
        match self.factory.start().await {
            Ok(server) => {
                let client = self.set_healthy(server);
                self.record_state().await;
                StartOutcome::Started(client)
            }
            Err(StartError::Provision(msg)) => {
                // Provision failure: straight to `down`, no background retry.
                self.drop_current();
                self.state = ServerState::Down;
                self.restart_timer = None;
                self.record_state().await;
                // The runtime is missing (node/npm/pyright); detail surfaces via
                // `index_meta` and the `AcquireError::Down` message.
                let _ = msg;
                StartOutcome::ProvisionFailed
            }
            Err(StartError::SpawnOrInit(_msg)) => {
                self.transition_after_anomaly().await;
                StartOutcome::SpawnOrInitFailed
            }
        }
    }

    /// Take a freshly started `server` into the healthy state: store its
    /// client/exit/keep-alive, reset the failure counter, stamp
    /// `last_success_at`. Returns a client clone for the caller.
    fn set_healthy(&mut self, server: StartedServer) -> LspClient {
        let StartedServer {
            client,
            exit,
            keep_alive,
        } = server;
        let clone = client.clone();
        self.current_client = Some(client);
        self.current_exit = Some(exit);
        self.current_keepalive = keep_alive;
        self.state = ServerState::Healthy;
        self.failures = 0;
        self.last_success_at = now_secs();
        clone
    }

    /// A restart/down-retry timer fired: attempt a start and settle into the
    /// resulting state (healthy, still-down-with-retry, or down-no-retry).
    async fn on_restart_timer(&mut self) {
        match self.attempt_start().await {
            StartOutcome::Started(_) => {}
            StartOutcome::ProvisionFailed => {}
            StartOutcome::SpawnOrInitFailed => {
                // `transition_after_anomaly` re-armed the timer (backoff or 30s).
            }
        }
    }

    /// React to a child-exit / report-failure anomaly from a healthy state:
    /// increment the counter, then go `restarting` (backoff) or `down` (at the
    /// threshold, arming the 30s background retry).
    async fn transition_after_anomaly(&mut self) {
        self.drop_current_gracefully();
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.policy.max_failures {
            self.state = ServerState::Down;
            self.restart_timer = Some(Box::pin(time::sleep(self.policy.down_retry)));
        } else {
            self.state = ServerState::Restarting;
            // failures is now 1..=4 → backoff index failures-1 (0..=3).
            self.restart_timer = Some(Box::pin(time::sleep(
                self.policy.nth_backoff(self.failures - 1),
            )));
        }
        self.record_state().await;
    }

    /// Handle an `Acquire`: serve a live client if healthy, lazily start from
    /// `not_started`, or report the degraded state.
    async fn handle_acquire(&mut self) -> std::result::Result<LspClient, AcquireError> {
        match self.state {
            ServerState::Healthy => self
                .current_client
                .clone()
                .ok_or_else(|| AcquireError::StartFailed("healthy state had no client".into())),
            ServerState::Restarting => Err(AcquireError::Restarting),
            ServerState::Down => Err(AcquireError::Down(format!(
                "{} language server is down after {} consecutive failures",
                self.language, self.failures
            ))),
            ServerState::NotStarted => {
                self.state = ServerState::Starting;
                self.record_state().await;
                self.start_and_settle().await
            }
            ServerState::Starting => Err(AcquireError::Restarting),
        }
    }

    /// The first start, triggered by `Acquire` from `not_started`. Translates
    /// the start outcome into an `Acquire` reply.
    async fn start_and_settle(&mut self) -> std::result::Result<LspClient, AcquireError> {
        match self.attempt_start().await {
            StartOutcome::Started(client) => Ok(client),
            StartOutcome::ProvisionFailed => Err(AcquireError::Down(format!(
                "{} language server provision failed; runtime must be installed before retry",
                self.language
            ))),
            StartOutcome::SpawnOrInitFailed => {
                if self.state == ServerState::Down {
                    Err(AcquireError::Down(format!(
                        "{} language server start failed repeatedly; now down",
                        self.language
                    )))
                } else {
                    Err(AcquireError::StartFailed(format!(
                        "{} language server start failed; restarting",
                        self.language
                    )))
                }
            }
        }
    }

    /// Drop the held server (client + exit watcher + keep-alive). Used on the
    /// restart path: dropping the keep-alive lets the child see stdin EOF and
    /// exit. (Graceful kill of the *old* server on restart is deferred — see the
    /// `lsp-lifecycle.md` note; teardown uses [`Self::shutdown_graceful`].)
    fn drop_current(&mut self) {
        self.current_client = None;
        self.current_exit = None;
        self.current_keepalive = None;
    }

    /// Restart-path teardown of the *old* server: clears held state immediately
    /// (so the restart isn't blocked on shutdown latency) while still giving the
    /// old process a chance to exit cleanly, by handing its keep-alive to a
    /// detached task that runs the same graceful escalation as
    /// [`Self::shutdown_graceful`].
    fn drop_current_gracefully(&mut self) {
        self.current_client = None;
        self.current_exit = None;
        if let Some(mut server) = self.current_keepalive.take() {
            tokio::spawn(async move {
                server.shutdown(SHUTDOWN_GRACE).await;
            });
        }
    }

    /// Teardown path: run the graceful escalation (`shutdown`→`exit`→SIGTERM→
    /// SIGKILL, `lsp-lifecycle.md` Shutdown) on the live server, then clear
    /// all held state and disarm the restart timer. A no-op when there is no
    /// live server to hold (mock factories set `keep_alive = None`; Restarting /
    /// Down hold none). Awaited inline within a `select!` match arm, so neither
    /// the restart timer nor the exit watcher can fire concurrently and spawn a
    /// fresh server mid-shutdown.
    async fn shutdown_graceful(&mut self) {
        if let Some(mut server) = self.current_keepalive.take() {
            server.shutdown(SHUTDOWN_GRACE).await;
        }
        self.current_client = None;
        self.current_exit = None;
        self.restart_timer = None;
    }

    /// Persist the current state, failure count, and last-success timestamp to
    /// the meta store. Fire-and-forget (the actor never recovers from a
    /// meta-write failure).
    async fn record_state(&self) {
        self.db
            .record_meta(
                format!("{}.lsp_status", self.language),
                self.state.as_str().to_string(),
            )
            .await;
        self.db
            .record_meta(
                format!("{}.lsp_consecutive_failures", self.language),
                self.failures.to_string(),
            )
            .await;
        self.db
            .record_meta(
                format!("{}.lsp_last_success_at", self.language),
                self.last_success_at.to_string(),
            )
            .await;
    }
}

/// Unix epoch seconds (dep-free). The design's ISO timestamp is deferred —
/// `lsp_last_success_at` is log/debug-only in 0.0.1.
fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use tokio::io::duplex;

    /// Programmed outcome for one `MockFactory::start` call.
    #[derive(Clone, Copy)]
    enum MockOutcome {
        Ok,
        ProvisionFail,
        SpawnFail,
    }

    /// Test factory: returns the next programmed outcome per `start`, and — on
    /// success — hands back a `StartedServer` over a duplex whose exit watch the
    /// test can poke to simulate a child crash.
    struct MockFactory {
        outcomes: Arc<Mutex<VecDeque<MockOutcome>>>,
        exit_senders: Arc<Mutex<Vec<watch::Sender<Option<ServerExit>>>>>,
    }

    impl MockFactory {
        fn new(outcomes: Vec<MockOutcome>) -> Self {
            Self {
                outcomes: Arc::new(Mutex::new(outcomes.into())),
                exit_senders: Arc::new(Mutex::new(Vec::new())),
            }
        }

        /// Clone the shared list of exit-watch senders, so the test can fire a
        /// synthetic child exit on the most-recently-started server.
        fn exit_senders(&self) -> Arc<Mutex<Vec<watch::Sender<Option<ServerExit>>>>> {
            self.exit_senders.clone()
        }
    }

    impl ServerFactory for MockFactory {
        // Mirrors `RealServerFactory::start`: explicit `impl Future + Send`
        // (clone the shared state out, then `async move`) so the spawned
        // supervisor task — which awaits this — stays `Send`.
        fn start(
            &self,
        ) -> impl Future<Output = std::result::Result<StartedServer, StartError>> + Send {
            let outcomes = self.outcomes.clone();
            let exit_senders = self.exit_senders.clone();
            async move {
                let next = outcomes.lock().unwrap().pop_front();
                match next {
                    Some(MockOutcome::ProvisionFail) => {
                        Err(StartError::Provision("mock provision fail".into()))
                    }
                    Some(MockOutcome::SpawnFail) => {
                        Err(StartError::SpawnOrInit("mock spawn fail".into()))
                    }
                    // `None` (outcomes exhausted) is treated as success, so a test
                    // that programs "N failures then recover" needs no trailing Ok.
                    None | Some(MockOutcome::Ok) => {
                        let (client_read, server_write) = duplex(1024);
                        let (client_write, server_read) = duplex(1024);
                        // Leak the server-side halves so the client's stream stays
                        // open for the test's lifetime (no real process to hold).
                        std::mem::forget((server_read, server_write));
                        let client = LspClient::spawn(client_read, client_write);
                        let (tx, rx) = watch::channel(None);
                        exit_senders.lock().unwrap().push(tx);
                        Ok(StartedServer {
                            client,
                            exit: rx,
                            keep_alive: None,
                        })
                    }
                }
            }
        }
    }

    /// A tiny policy so paused-time tests run the whole state machine in
    /// milliseconds rather than seconds.
    fn fast_policy() -> RestartPolicy {
        RestartPolicy {
            backoff: [
                Duration::from_millis(10),
                Duration::from_millis(20),
                Duration::from_millis(40),
                Duration::from_millis(80),
                Duration::from_millis(160),
            ],
            max_failures: 5,
            down_retry: Duration::from_millis(300),
        }
    }

    /// In-memory `MetaStore` for deterministic `start_paused` tests. Unlike the
    /// real [`DbActor`] it does no `spawn_blocking`, so it does not stall tokio's
    /// paused-time auto-advance; reads are synchronous via the shared map.
    #[derive(Clone, Default)]
    struct RecordingMeta(Arc<Mutex<HashMap<String, String>>>);

    impl RecordingMeta {
        fn get(&self, key: &str) -> Option<String> {
            self.0.lock().unwrap().get(key).cloned()
        }
    }

    impl MetaStore for RecordingMeta {
        fn record_meta(&self, key: String, value: String) -> impl Future<Output = ()> + Send {
            let store = self.clone();
            async move {
                store.0.lock().unwrap().insert(key, value);
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn happy_path_starts_healthy_and_writes_meta() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::Ok]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "python", fast_policy());

        let client = sup.acquire().await.expect("acquire healthy client");
        drop(client);

        assert_eq!(db.get("python.lsp_status"), Some("healthy".to_string()));
        assert_eq!(
            db.get("python.lsp_consecutive_failures"),
            Some("0".to_string())
        );
        // last_success_at is a positive epoch second.
        let ts: i64 = db
            .get("python.lsp_last_success_at")
            .unwrap()
            .parse()
            .unwrap();
        assert!(ts > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_after_success_returns_live_client() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::Ok]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let c1 = sup.acquire().await.expect("first acquire");
        // Second acquire must serve the same healthy server, not restart.
        let c2 = sup.acquire().await.expect("second acquire");
        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));
        drop(c1);
        drop(c2);
    }

    #[tokio::test(start_paused = true)]
    async fn spawn_fail_3_then_ok_recovers_with_reset_counter() {
        let db = RecordingMeta::default();
        // First start (the Acquire) fails, then 3 more failures on the backoff
        // timers, then success — total 4 failures before recovery.
        let factory = MockFactory::new(vec![
            MockOutcome::SpawnFail,
            MockOutcome::SpawnFail,
            MockOutcome::SpawnFail,
            MockOutcome::SpawnFail,
            MockOutcome::Ok,
        ]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let first = sup.acquire().await;
        assert!(
            matches!(first, Err(AcquireError::StartFailed(_))),
            "first acquire should fail with StartFailed, got {first:?}"
        );

        // Drive the backoff timers (10+20+40+80ms) to recovery.
        time::sleep(Duration::from_millis(500)).await;

        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));
        assert_eq!(db.get("py.lsp_consecutive_failures"), Some("0".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn five_spawn_fails_reach_down() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::SpawnFail; 6]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let _ = sup.acquire().await;
        // 4 backoff retries after the first failure (10+20+40+80ms) → 5th
        // failure flips to `down` and arms the 300ms retry. The 6th programmed
        // failure (consumed by the 450ms down-retry) keeps it down, so within
        // the 500ms window failures must read exactly 5 at the down flip...
        // but the 450ms retry bumps it to 6. Assert the stable end state.
        time::sleep(Duration::from_millis(500)).await;

        assert_eq!(db.get("py.lsp_status"), Some("down".to_string()));
        let failures = db
            .get("py.lsp_consecutive_failures")
            .map(|s| s.parse::<u32>().unwrap())
            .unwrap_or(0);
        assert!(
            failures >= 5,
            "down after ≥5 consecutive failures, got {failures}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn down_retries_in_background_and_recovers() {
        let db = RecordingMeta::default();
        // 5 SpawnFail → down; the 300ms down-retry then succeeds (outcomes
        // exhausted → Ok).
        let factory = MockFactory::new(vec![MockOutcome::SpawnFail; 5]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let _ = sup.acquire().await;
        // 5 failures land at t=150ms (10+20+40+80) and arm the 300ms retry at
        // t=450ms. Check `down` before the retry fires, then drive past it.
        time::sleep(Duration::from_millis(200)).await;
        assert_eq!(db.get("py.lsp_status"), Some("down".to_string()));

        // The 300ms down-retry (t=450ms) recovers to healthy.
        time::sleep(Duration::from_millis(400)).await;
        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));
        assert_eq!(db.get("py.lsp_consecutive_failures"), Some("0".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn provision_fail_goes_down_with_no_background_retry() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::ProvisionFail]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let first = sup.acquire().await;
        assert!(
            matches!(first, Err(AcquireError::Down(_))),
            "provision failure should surface as Down, got {first:?}"
        );
        assert_eq!(db.get("py.lsp_status"), Some("down".to_string()));
        // Provision failures do not increment the restart counter.
        assert_eq!(db.get("py.lsp_consecutive_failures"), Some("0".to_string()));

        // No 30s/300ms timer was armed: advancing well past the down-retry
        // interval must NOT flip the state out of down.
        time::sleep(Duration::from_secs(2)).await;
        assert_eq!(db.get("py.lsp_status"), Some("down".to_string()));
        // A subsequent acquire stays down.
        let again = sup.acquire().await;
        assert!(matches!(again, Err(AcquireError::Down(_))));
    }

    #[tokio::test(start_paused = true)]
    async fn child_exit_while_healthy_triggers_restart() {
        let db = RecordingMeta::default();
        // The factory makes one Ok slot; on restart it returns Ok (outcomes
        // exhausted → Ok), so recovery is automatic after the crash.
        let factory = MockFactory::new(vec![MockOutcome::Ok]);
        // Clone the shared exit-sender list before the factory moves into the
        // actor, so the test can poke the watch the factory registered on start.
        let senders = factory.exit_senders();
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let _client = sup.acquire().await.expect("acquire");

        // Let the actor settle into healthy, then simulate a child crash.
        time::sleep(Duration::from_millis(5)).await;
        let tx = {
            let mut guard = senders.lock().unwrap();
            guard.pop().expect("an exit sender was registered on start")
        };
        tx.send(Some(ServerExit {
            success: false,
            code: None,
        }))
        .expect("poke exit");

        // The crash flips healthy→restarting (1 failure); the 10ms backoff then
        // recovers to healthy (outcomes exhausted → Ok).
        time::sleep(Duration::from_millis(100)).await;
        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));
        assert_eq!(db.get("py.lsp_consecutive_failures"), Some("0".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn report_failure_transport_triggers_restart() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::Ok]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let _client = sup.acquire().await.expect("acquire");
        time::sleep(Duration::from_millis(5)).await;

        // A transport error reported by the client holder → restart. Outcomes
        // exhausted → the 10ms backoff recovers to healthy.
        sup.report_failure(FailureKind::Transport)
            .await
            .expect("report");
        time::sleep(Duration::from_millis(100)).await;
        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn failure_kind_classifies_timeout_and_transport() {
        let timeout_err = anyhow::anyhow!("documentSymbol timed out after 30s");
        let closed_err = anyhow::anyhow!("client stream closed");
        let write_err = anyhow::anyhow!("write failed: pipe broken");
        assert_eq!(FailureKind::from(&timeout_err), FailureKind::Timeout);
        assert_eq!(FailureKind::from(&closed_err), FailureKind::Transport);
        assert_eq!(FailureKind::from(&write_err), FailureKind::Transport);
    }

    #[tokio::test(start_paused = true)]
    async fn restart_policy_clamps_last_backoff_slot() {
        let p = RestartPolicy::default_real();
        assert_eq!(p.nth_backoff(0), Duration::from_secs(1));
        assert_eq!(p.nth_backoff(4), Duration::from_secs(16));
        assert_eq!(p.nth_backoff(99), Duration::from_secs(16));
    }

    // --- graceful shutdown teardown (lsp-lifecycle.md Shutdown) ---

    /// An explicit `shutdown().await` runs the escalation, breaks the actor loop,
    /// and drops the receiver — so any later `acquire` surfaces `Down` (the send
    /// fails because the actor is gone). The mock holds `keep_alive = None`, so
    /// `shutdown_graceful` is a no-op; the assertion is the actor's termination.
    #[tokio::test(start_paused = true)]
    async fn explicit_shutdown_closes_actor() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::Ok]);
        let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());

        let client = sup.acquire().await.expect("acquire healthy client");
        drop(client);
        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));

        sup.shutdown().await.expect("shutdown ack");

        let after = sup.acquire().await;
        assert!(
            matches!(after, Err(AcquireError::Down(_))),
            "actor should be closed after shutdown, got {after:?}"
        );
    }

    /// Dropping the last handle makes `rx.recv()` return `None`, which runs the
    /// *detached* graceful-shutdown path (`shutdown_graceful`) before the actor
    /// exits. With the mock (`keep_alive = None`) the escalation is a no-op; the
    /// test pins that this path completes without hanging or panicking, and that
    /// the last-recorded state survives the teardown.
    #[tokio::test(start_paused = true)]
    async fn drop_last_handle_runs_graceful_path() {
        let db = RecordingMeta::default();
        let factory = MockFactory::new(vec![MockOutcome::Ok]);
        {
            let sup = ServerSupervisor::spawn(db.clone(), factory, "py", fast_policy());
            let _client = sup.acquire().await.expect("acquire");
            // Dropping `sup` here drops the last sender → actor observes None.
        }
        // Let the actor observe the dropped sender and run the None path.
        time::sleep(Duration::from_millis(50)).await;
        // Reaching here means the detached teardown completed cleanly; the state
        // recorded before the drop is still the last write.
        assert_eq!(db.get("py.lsp_status"), Some("healthy".to_string()));
    }
}
