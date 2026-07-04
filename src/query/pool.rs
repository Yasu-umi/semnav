//! Long-lived, lazily-created per-language supervisor pool that hands the
//! query engine a *real* [`LspQueryClient`] (`docs/design/lsp-integration.md`
//! "on-demand edge construction").
//!
//! Each query-time language server is owned by its [`ServerSupervisor`], which
//! keeps the process alive across calls (`acquire` is idempotent once healthy)
//! — the opposite of the indexer, which shuts each server down at the end of
//! one pass. [`QueryRuntime`] owns one [`SupervisorHandle`] per language in a
//! lazily-filled map; the first query for a language provisions + handshakes
//! its server, and every later query reuses it.
//!
//! Resilience: a missing language or an `acquire` failure (server `down` /
//! `restarting`) degrades to cache-only rather than erroring the query — the
//! engine already treats `None` client as "serve the materialized cache"
//! (`docs/design/resilience.md`). The supervisor records health to
//! `index_meta` in the background regardless.
//!
//! House style: the supervisor map lives behind a `Mutex<HashMap<..>>` **field
//! on the struct** (not module-level state), and the lock is released before
//! any `await` — `supervisor_for` clones the cheap [`SupervisorHandle`] out,
//! then `acquire` runs lock-free.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;

use crate::adapters::select_for_uri;
use crate::lsp::{
    AcquireError, LspClient, RealServerFactory, RestartPolicy, ServerSupervisor, SupervisorHandle,
};

use super::lsp_query::ClientLspQueryClient;
use super::{
    CallGraphResult, Filter, FindDefinitionResult, FindReferencesResult, FindSymbolResult,
    MatchMode, Page, QueryEngine, ReadRangeResult, SymbolRef,
};

/// Why an LSP-dependent operation fell back to cache-only (or partially
/// cache-only) data (`docs/design/resilience.md`). `LspUnavailable` covers
/// acquire-time failures (server `down`/`restarting`); `LspTimeout` covers a
/// request-level round-trip that exceeded `QUERY_TIMEOUT` on an
/// already-acquired client (`resolver.rs`/`ops.rs` surface this via each
/// operation's returned timeout flag). `lsp_partial` (spec-unsupported
/// methods) remains a reserved, unproduced wire value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradeReason {
    LspUnavailable,
    LspTimeout,
}

impl DegradeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LspUnavailable => "lsp_unavailable",
            Self::LspTimeout => "lsp_timeout",
        }
    }
}

/// The language server's observed health at the moment of a degraded query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspStatus {
    Down,
    Degraded,
}

impl LspStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Degraded => "degraded",
        }
    }
}

/// A cache-only fallback, with enough detail for the mcp layer to populate
/// `degraded`/`degrade_reason`/`lsp_status` (`docs/design/resilience.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Degradation {
    pub reason: DegradeReason,
    pub status: LspStatus,
}

impl From<AcquireError> for Degradation {
    /// `Down` ⇒ the server has exhausted its restart budget; `Restarting` /
    /// `StartFailed` ⇒ mid-recovery, likely to heal on a later call.
    fn from(err: AcquireError) -> Self {
        let status = match err {
            AcquireError::Down(_) => LspStatus::Down,
            AcquireError::Restarting | AcquireError::StartFailed(_) => LspStatus::Degraded,
        };
        Self {
            reason: DegradeReason::LspUnavailable,
            status,
        }
    }
}

/// The query-time runtime: a [`QueryEngine`] plus a lazily-created pool of
/// per-language LSP supervisors. Each public operation acquires a real client
/// for the request's language (when one can be pinned and the server is up),
/// wraps it as a [`ClientLspQueryClient`], and delegates to the engine —
/// falling back to cache-only on any acquisition miss.
pub struct QueryRuntime {
    engine: QueryEngine,
    servers_dir: PathBuf,
    workspace_name: String,
    supervisors: Mutex<HashMap<String, SupervisorHandle>>,
}

impl QueryRuntime {
    /// Build a runtime over `engine`, provisioning servers into `servers_dir`
    /// (the same isolated npm-install location the indexer uses).
    pub fn new(engine: QueryEngine, servers_dir: PathBuf, workspace_name: String) -> Self {
        Self {
            engine,
            servers_dir,
            workspace_name,
            supervisors: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience: build a runtime and derive `workspace_name` from the
    /// engine's root URI (the root's last path segment), mirroring the indexer.
    pub fn open(engine: QueryEngine, servers_dir: PathBuf) -> Self {
        let workspace_name = RealServerFactory::workspace_name_for(engine.root_uri());
        Self::new(engine, servers_dir, workspace_name)
    }

    /// The underlying engine (for direct graph reads / `read_range` callers).
    pub fn engine(&self) -> &QueryEngine {
        &self.engine
    }

    // --- the six operations --------------------------------------------------

    /// `find_symbol` — a pure graph read (no LSP); delegates straight through.
    pub async fn find_symbol(
        &self,
        pattern: &str,
        mode: MatchMode,
        ignore_case: bool,
        brief: bool,
        filter: &Filter,
        page: &Page,
    ) -> Result<FindSymbolResult> {
        self.engine
            .find_symbol(pattern, mode, ignore_case, brief, filter, page)
            .await
    }

    /// `read_range` — a pure filesystem read (no LSP); delegates straight through.
    pub async fn read_range(
        &self,
        uri: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<ReadRangeResult> {
        self.engine.read_range(uri, start_line, end_line).await
    }

    /// `find_definition` — `At` acquires the uri's language server for
    /// `textDocument/definition`; `Fqn` is a pure graph read (no server).
    pub async fn find_definition(
        &self,
        symref: &SymbolRef,
    ) -> Result<(FindDefinitionResult, Option<Degradation>)> {
        let language = match symref {
            // Fqn definitions are a pure graph lookup — never spawn a server.
            SymbolRef::Fqn(_) => None,
            SymbolRef::At { uri, .. } => select_for_uri(uri).map(|a| a.language_name().to_string()),
        };
        let (client, degradation) = match self.acquire_opt(language.as_deref()).await {
            Ok(client) => (client, None),
            Err(d) => (None, Some(d)),
        };
        let wrapper = client.as_ref().map(|c| {
            ClientLspQueryClient::with_default_timeout(c, language.as_deref().unwrap_or(""))
        });
        let (result, timed_out) = self
            .engine
            .find_definition(symref, wrapper.as_ref())
            .await?;
        Ok((result, degradation.or(timeout_degradation(timed_out))))
    }

    /// `find_references` — on-demand `textDocument/references` materialization
    /// when the anchor's language server is up, else the cached edges.
    pub async fn find_references(
        &self,
        symref: &SymbolRef,
        filter: &Filter,
        page: &Page,
    ) -> Result<(FindReferencesResult, Option<Degradation>)> {
        let (client, degradation) = self.client_for(symref).await?;
        let wrapper = client
            .as_ref()
            .map(|c| ClientLspQueryClient::with_default_timeout(&c.client, c.language_id()));
        let (result, timed_out) = self
            .engine
            .find_references(symref, filter, page, wrapper.as_ref())
            .await?;
        Ok((result, degradation.or(timeout_degradation(timed_out))))
    }

    /// `find_callers` — on-demand incoming call hierarchy when the anchor's
    /// language server is up, else the cached `calls` edges.
    pub async fn find_callers(
        &self,
        symref: &SymbolRef,
        filter: &Filter,
        page: &Page,
    ) -> Result<(CallGraphResult, Option<Degradation>)> {
        let (client, degradation) = self.client_for(symref).await?;
        let wrapper = client
            .as_ref()
            .map(|c| ClientLspQueryClient::with_default_timeout(&c.client, c.language_id()));
        let (result, timed_out) = self
            .engine
            .find_callers(symref, filter, page, wrapper.as_ref())
            .await?;
        Ok((result, degradation.or(timeout_degradation(timed_out))))
    }

    /// `find_callees` — on-demand outgoing call hierarchy when the anchor's
    /// language server is up, else the cached `calls` edges.
    pub async fn find_callees(
        &self,
        symref: &SymbolRef,
        filter: &Filter,
        page: &Page,
    ) -> Result<(CallGraphResult, Option<Degradation>)> {
        let (client, degradation) = self.client_for(symref).await?;
        let wrapper = client
            .as_ref()
            .map(|c| ClientLspQueryClient::with_default_timeout(&c.client, c.language_id()));
        let (result, timed_out) = self
            .engine
            .find_callees(symref, filter, page, wrapper.as_ref())
            .await?;
        Ok((result, degradation.or(timeout_degradation(timed_out))))
    }

    /// Explicitly shut down every provisioned server (the graceful
    /// `shutdown`→`exit`→SIGTERM→SIGKILL escalation). Callers that must
    /// guarantee the children are reaped before runtime teardown (the MCP
    /// server) use this instead of relying on drop.
    pub async fn shutdown_all(&self) {
        let handles: Vec<SupervisorHandle> = self
            .supervisors
            .lock()
            .expect("supervisor map poisoned")
            .values()
            .cloned()
            .collect();
        for sup in handles {
            let _ = sup.shutdown().await;
        }
    }

    /// Force a specific language's server to restart (or every provisioned
    /// language, if `None`): gracefully shuts down its current supervisor and
    /// drops it from the pool, so the next `acquire` lazily respawns it from
    /// `NotStarted` — the exact path a first-ever query for that language
    /// already takes. No new supervisor message type is needed. For a stuck
    /// server that isn't erroring (so the automatic restart-on-failure policy
    /// never kicks in), this is the only way to force recovery short of
    /// restarting the whole process. Returns the languages that were reset.
    pub async fn restart_language(&self, language: Option<&str>) -> Vec<String> {
        let handles: Vec<(String, SupervisorHandle)> = {
            let mut map = self.supervisors.lock().expect("supervisor map poisoned");
            match language {
                Some(lang) => map
                    .remove(lang)
                    .into_iter()
                    .map(|h| (lang.to_string(), h))
                    .collect(),
                None => map.drain().collect(),
            }
        };
        let restarted: Vec<String> = handles.iter().map(|(l, _)| l.clone()).collect();
        for (_, handle) in handles {
            let _ = handle.shutdown().await;
        }
        restarted
    }

    /// Acquire the same live per-language client `find_references`/
    /// `find_callers`/`find_callees` use, for the FS watcher's `didChange`
    /// plumbing (`src/indexer/reconcile.rs`) — sharing the connection (rather
    /// than opening a private one) is what makes a watcher-sent `didChange`
    /// visible to later on-demand edge materialization on that same server.
    /// `None` means the server is down/restarting; the watcher skips
    /// reconciliation for this event and catches up on a later one.
    pub async fn acquire_for_watcher(&self, language: &str) -> Option<LspClient> {
        self.acquire_opt(Some(language)).await.ok().flatten()
    }

    // --- internals -----------------------------------------------------------

    /// Resolve the anchor language for a symref and acquire a client for it.
    /// `At` ⇒ the uri's adapter; `Fqn` ⇒ the anchor node's stored language (a
    /// DB peek). The client is `None` (⇒ cache-only) either because no
    /// language could be pinned (not degraded) or because the server is
    /// unavailable (degraded — see the returned [`Degradation`]).
    async fn client_for(
        &self,
        symref: &SymbolRef,
    ) -> Result<(Option<AcquiredClient>, Option<Degradation>)> {
        let language = match symref {
            SymbolRef::At { uri, .. } => select_for_uri(uri).map(|a| a.language_name().to_string()),
            SymbolRef::Fqn(fqn) => self
                .engine
                .db()
                .get_node_by_fqn(fqn)
                .await?
                .map(|n| n.language),
        };
        let (client, degradation) = match self.acquire_opt(language.as_deref()).await {
            Ok(client) => (client, None),
            Err(d) => (None, Some(d)),
        };
        let acquired = client.map(|c| AcquiredClient {
            client: c,
            language,
        });
        Ok((acquired, degradation))
    }

    /// Acquire a real client for `language`. `None` in ⇒ `Ok(None)` out (no
    /// language to pin a server, not degraded); an acquire failure (server
    /// `down` / `restarting` / first-start hiccup) ⇒ `Err(Degradation)` so the
    /// caller can serve cache-only while still surfacing the reason.
    async fn acquire_opt(&self, language: Option<&str>) -> Result<Option<LspClient>, Degradation> {
        let Some(language) = language else {
            return Ok(None);
        };
        let sup = self.supervisor_for(language);
        sup.acquire().await.map(Some).map_err(Degradation::from)
    }

    /// Get (lazily creating) the supervisor handle for `language`. The lock is
    /// held only for the map insert — the returned [`SupervisorHandle`] is a
    /// cheap clone, so `acquire` can run lock-free by the caller.
    fn supervisor_for(&self, language: &str) -> SupervisorHandle {
        let mut map = self.supervisors.lock().expect("supervisor map poisoned");
        map.entry(language.to_string())
            .or_insert_with(|| self.spawn_supervisor(language))
            .clone()
    }

    /// Provision + spawn + handshake a fresh supervisor for `language`, wiring
    /// the db as the health [`MetaStore`] (`crate::lsp::supervisor::MetaStore`
    /// is implemented for `DbActor`).
    fn spawn_supervisor(&self, language: &str) -> SupervisorHandle {
        let factory = RealServerFactory {
            language: language.to_string(),
            servers_dir: self.servers_dir.clone(),
            root_uri: self.engine.root_uri().to_string(),
            workspace_name: self.workspace_name.clone(),
        };
        ServerSupervisor::spawn(
            self.engine.db().clone(),
            factory,
            language,
            RestartPolicy::default_real(),
        )
    }
}

/// `Some(Degradation)` when `timed_out` — the acquired client was healthy, but
/// a request-level LSP round-trip on it hit `QUERY_TIMEOUT`. Callers only
/// apply this when acquire itself didn't already degrade (an acquire failure
/// takes precedence over a same-request timeout that never got the chance to
/// happen).
fn timeout_degradation(timed_out: bool) -> Option<Degradation> {
    timed_out.then_some(Degradation {
        reason: DegradeReason::LspTimeout,
        status: LspStatus::Degraded,
    })
}

/// A live client paired with the language it serves, so the caller can build a
/// correctly-tagged [`ClientLspQueryClient`] (`language_id` for `didOpen`).
struct AcquiredClient {
    client: LspClient,
    language: Option<String>,
}

impl AcquiredClient {
    /// The didOpen language id. Always `Some` here — `acquire_opt` only yields
    /// a client when a language was pinned — but fall back defensively.
    fn language_id(&self) -> &str {
        self.language.as_deref().unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::DbActor;
    use crate::indexer::{index_language, path_to_uri};
    use crate::query::QueryEngine;
    use std::fs;
    use tempfile::tempdir;

    fn root_uri_for(dir: &std::path::Path) -> String {
        format!("{}/", path_to_uri(dir).trim_end_matches('/'))
    }

    /// `workspace_name_for` mirrors the indexer's last-segment rule, and falls
    /// back to `"workspace"` for a bare root (never the `"file:"` scheme fragment).
    #[test]
    fn workspace_name_uses_last_root_segment() {
        assert_eq!(
            RealServerFactory::workspace_name_for("file:///repo/myapp/"),
            "myapp"
        );
        assert_eq!(
            RealServerFactory::workspace_name_for("file:///repo/"),
            "repo"
        );
        // A bare root still yields a name, never a scheme fragment like "file:".
        assert_eq!(
            RealServerFactory::workspace_name_for("file:///"),
            "workspace"
        );
    }

    /// `Down` means the restart budget is exhausted — surfaced as `LspStatus::Down`
    /// so the mcp layer can distinguish "give up" from "retry later".
    #[test]
    fn degradation_from_down_is_down_status() {
        let d = Degradation::from(AcquireError::Down("gave up".into()));
        assert_eq!(d.status, LspStatus::Down);
        assert_eq!(d.reason, DegradeReason::LspUnavailable);
    }

    /// `Restarting` and `StartFailed` are both mid-recovery — surfaced as
    /// `LspStatus::Degraded` since a later call is likely to succeed.
    #[test]
    fn degradation_from_restarting_or_start_failed_is_degraded_status() {
        assert_eq!(
            Degradation::from(AcquireError::Restarting).status,
            LspStatus::Degraded
        );
        assert_eq!(
            Degradation::from(AcquireError::StartFailed("boom".into())).status,
            LspStatus::Degraded
        );
    }

    /// A timed-out request-level round-trip on an already-acquired client
    /// degrades as `LspTimeout`/`Degraded` — distinct from an acquire-time
    /// `LspUnavailable`, since the server itself is healthy.
    #[test]
    fn timeout_degradation_true_yields_lsp_timeout_degraded() {
        let d = timeout_degradation(true).expect("timed out ⇒ degraded");
        assert_eq!(d.reason, DegradeReason::LspTimeout);
        assert_eq!(d.status, LspStatus::Degraded);
    }

    #[test]
    fn timeout_degradation_false_is_none() {
        assert!(timeout_degradation(false).is_none());
    }

    /// A cache-only query against an empty graph degrades cleanly: no language
    /// can be pinned (the fqn is unknown), so no server is spawned and the
    /// result is the empty cache. Proves the pool never blocks on a missing
    /// server.
    #[tokio::test]
    async fn references_for_unknown_fqn_degrades_to_empty_without_spawning() {
        let dir = tempdir().unwrap();
        let db = DbActor::spawn(&dir.path().join("g.db")).unwrap();
        let engine = QueryEngine::new(db, "file:///repo".into());
        let runtime = QueryRuntime::open(engine, dir.path().join("servers"));

        let (res, degradation) = runtime
            .find_references(
                &SymbolRef::Fqn("repo.nope".into()),
                &Filter::default(),
                &Page::default(),
            )
            .await
            .expect("degrades, not errors");
        assert!(res.references.is_empty());
        assert!(degradation.is_none(), "unresolvable fqn is not degraded");
        // No supervisor was created for an unresolvable language.
        assert!(
            runtime.supervisors.lock().unwrap().is_empty(),
            "no server spawned for an unknown fqn"
        );
    }

    /// `restart_language` on a runtime that never acquired anything is a safe
    /// no-op — it must not panic or try to shut down a supervisor that was
    /// never spawned.
    #[tokio::test]
    async fn restart_language_on_empty_runtime_is_noop() {
        let dir = tempdir().unwrap();
        let db = DbActor::spawn(&dir.path().join("g.db")).unwrap();
        let engine = QueryEngine::new(db, "file:///repo".into());
        let runtime = QueryRuntime::open(engine, dir.path().join("servers"));

        assert_eq!(
            runtime.restart_language(Some("python")).await,
            Vec::<String>::new()
        );
        assert_eq!(runtime.restart_language(None).await, Vec::<String>::new());
    }

    /// Real pyright, end-to-end: acquire python's supervisor via a live query,
    /// force-restart it, then confirm the pool cleanly respawns on the next
    /// query rather than reusing a stale handle. Ignored by default — it needs
    /// node/npm and provisions pyright from npm on first run.
    #[ignore = "requires node/npm; provisions pyright from npm on first run"]
    #[tokio::test]
    async fn restart_language_removes_and_respawns_supervisor() {
        let dir = tempdir().expect("tempdir");
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("mod.py"), "def helper():\n    return 1\n").unwrap();

        let root_uri = root_uri_for(dir.path());
        let cache_dir = dir.path().join(".semnav");
        let servers_dir = cache_dir.join("servers");
        let db_path = cache_dir.join("graph.db");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");
        index_language(&db, "python", &root_uri, &servers_dir)
            .await
            .expect("index python");

        let engine = QueryEngine::new(db, root_uri.clone());
        let runtime = QueryRuntime::open(engine, servers_dir);

        let uri = format!("{}app/mod.py", root_uri);
        runtime
            .find_definition(&SymbolRef::At {
                uri: uri.clone(),
                line: 0,
                character: 4,
            })
            .await
            .expect("first query spawns python's supervisor");
        assert!(runtime.supervisors.lock().unwrap().contains_key("python"));

        let restarted = runtime.restart_language(Some("python")).await;
        assert_eq!(restarted, vec!["python".to_string()]);
        assert!(!runtime.supervisors.lock().unwrap().contains_key("python"));

        // The next query must respawn cleanly, not error on a stale handle.
        let (res, degradation) = runtime
            .find_definition(&SymbolRef::At {
                uri,
                line: 0,
                character: 4,
            })
            .await
            .expect("query respawns python's supervisor after restart");
        assert_eq!(res.nodes.len(), 1);
        assert!(degradation.is_none());

        runtime.shutdown_all().await;
    }

    /// Real pyright, end-to-end: index a module, then query the runtime for the
    /// definition at a *usage* — the lazily-created python supervisor must
    /// acquire a live client, the engine must `definition` through it, and the
    /// result must resolve to the indexed `helper` declaration. Ignored by
    /// default — it needs node/npm and provisions pyright from npm on first run.
    #[ignore = "requires node/npm; provisions pyright from npm on first run"]
    #[tokio::test]
    async fn query_runtime_real_pyright_resolves_definition_at_usage() {
        let dir = tempdir().expect("tempdir");
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        // helper at line 0; called inside caller at line 4 (`    return helper()`),
        // where the `helper` identifier starts at column 11.
        fs::write(
            app.join("mod.py"),
            "def helper():\n    return 1\n\ndef caller():\n    return helper()\n",
        )
        .unwrap();

        let root_uri = root_uri_for(dir.path());
        let cache_dir = dir.path().join(".semnav");
        let servers_dir = cache_dir.join("servers");
        let db_path = cache_dir.join("graph.db");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");

        // Index so the declaration node exists for the engine to resolve into.
        index_language(&db, "python", &root_uri, &servers_dir)
            .await
            .expect("index python");

        let engine = QueryEngine::new(db, root_uri.clone());
        let runtime = QueryRuntime::open(engine, servers_dir);

        let uri = format!("{}app/mod.py", root_uri);
        let (res, degradation) = runtime
            .find_definition(&SymbolRef::At {
                uri,
                line: 4,
                character: 11,
            })
            .await
            .expect("find_definition through real pyright");
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(res.nodes[0].name, "helper");
        assert!(degradation.is_none(), "live pyright is not degraded");

        runtime.shutdown_all().await;
    }
}
