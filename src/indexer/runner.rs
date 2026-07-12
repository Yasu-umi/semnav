//! One language's index pass end-to-end: drive the LSP server through its
//! health-state supervisor (provision → spawn → `initialize` → index), which
//! owns the process lifecycle, backoff restart, and `index_meta` health record
//! (`docs/design/lsp-lifecycle.md`). This is the seam that runs Step 4's indexer
//! against a live pyright/tsserver instead of a mock.

use std::path::Path;

use anyhow::Result;

use crate::adapters::adapter_for_language;
use crate::graph::DbActor;
use crate::indexer::{IndexStats, LspSymbolFetcher, index_repository};
use crate::lsp::{
    FailureKind, RealServerFactory, RestartPolicy, ServerSupervisor,
    document_symbol_timeout_from_env,
};

/// Drive the LSP server for `language` through its supervisor, indexing its
/// files under `root_uri` into `db`. `servers_dir` is the isolated npm-install
/// location (`<cache_dir>/servers`); it is created on first install.
///
/// The supervisor provisions + spawns + handshakes the server, records
/// `<lang>.lsp_status=healthy` on success, and — on an index-round-trip error —
/// is told about the anomaly (`report_failure`) so any *future* acquire reacts.
/// On return the handle is shut down explicitly (`shutdown`→`exit`→SIGTERM→
/// SIGKILL) so the child is reaped before the CLI's runtime tears down, rather
/// than relying on a detached drop racing runtime exit.
pub async fn index_language(
    db: &DbActor,
    language: &str,
    root_uri: &str,
    servers_dir: &Path,
) -> Result<IndexStats> {
    // Fail fast on an unsupported language before spawning the supervisor.
    if adapter_for_language(language).is_none() {
        anyhow::bail!("no built-in adapter for language {language:?}");
    }

    // A workspace name is required for the handshake; the root's last path
    // segment is a stable choice (shared with the query-time pool).
    let workspace_name = RealServerFactory::workspace_name_for(root_uri);

    let factory = RealServerFactory {
        language: language.to_string(),
        servers_dir: servers_dir.to_path_buf(),
        root_uri: root_uri.to_string(),
        workspace_name: workspace_name.to_string(),
    };
    let sup = ServerSupervisor::spawn(db.clone(), factory, language, RestartPolicy::default_real());
    let client = sup.acquire().await.map_err(|e| {
        anyhow::anyhow!(
            "acquire LSP client for {language}: {e:?} (see <lang>.lsp_status in index_meta)"
        )
    })?;

    let fetcher = LspSymbolFetcher::new(&client, document_symbol_timeout_from_env(), language);
    match index_repository(db, &fetcher, root_uri, language).await {
        Ok(stats) => {
            let _ = sup.shutdown().await;
            Ok(stats)
        }
        Err(e) => {
            // Record the transport/timeout anomaly for any future acquire; this
            // one-shot still returns the error to the caller (CLI tolerates it).
            let _ = sup.report_failure(FailureKind::from(&e)).await;
            let _ = sup.shutdown().await;
            Err(e.context("index pass"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::path_to_uri;
    use std::fs;
    use tempfile::tempdir;

    fn root_uri_for(dir: &std::path::Path) -> String {
        format!("{}/", path_to_uri(dir).trim_end_matches('/'))
    }

    /// Real pyright, end-to-end: provisions pyright from npm (first run),
    /// indexes `class Repo: def load`, and asserts the symbols land in the
    /// graph with the right FQN and container linkage. Ignored by default — it
    /// needs node/npm and network on the first run.
    #[ignore = "requires node/npm; provisions pyright from npm on first run"]
    #[tokio::test]
    async fn index_language_real_pyright() {
        let dir = tempdir().expect("tempdir");
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("repo.py"),
            "class Repo:\n    def load(self) -> None: ...\n",
        )
        .unwrap();

        let root_uri = root_uri_for(dir.path());
        let cache_dir = dir.path().join(".semnav");
        let servers_dir = cache_dir.join("servers");
        let db_path = cache_dir.join("graph.db");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");

        let stats = index_language(&db, "python", &root_uri, &servers_dir)
            .await
            .expect("index python");
        assert!(
            stats.files_indexed >= 1,
            "expected at least one file indexed, got {stats:?}"
        );

        // The supervisor must have recorded a healthy python server in
        // `index_meta` (proves the RealServerFactory + health-write path).
        let status = db
            .get_meta("python.lsp_status")
            .await
            .expect("get_meta")
            .expect("python.lsp_status recorded");
        assert_eq!(status, "healthy");

        let parent = db
            .get_node_by_fqn("app.repo.Repo")
            .await
            .unwrap()
            .expect("Repo node");
        let child = db
            .get_node_by_fqn("app.repo.Repo.load")
            .await
            .unwrap()
            .expect("load node");
        assert_eq!(parent.node_kind, "Class");
        assert_eq!(parent.language, "python");
        // pyright reports a class method as Method (6); allow Function (12) too.
        assert!(
            matches!(child.kind, 6 | 12),
            "expected Method/Function, got kind {} ({})",
            child.kind,
            child.node_kind
        );
        assert_eq!(child.name, "load");
        assert_eq!(child.container_id, parent.id, "load is contained by Repo");
    }

    /// Real typescript-language-server, end-to-end — the first real e2e
    /// coverage for this language (every other real-server test in this
    /// codebase only exercises pyright, `docs/design/lsp-integration.md`
    /// "Provenance"). Provisions tsserver from npm (first run), indexes
    /// `class Repo { load() {} }`, and asserts the symbols land in the graph
    /// with the right FQN and container linkage. Ignored by default — it
    /// needs node/npm and network on the first run.
    #[ignore = "requires node/npm; provisions tsserver from npm on first run"]
    #[tokio::test]
    async fn index_language_real_tsserver() {
        let dir = tempdir().expect("tempdir");
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(
            app.join("repo.ts"),
            "export class Repo {\n    load(): void {}\n}\n",
        )
        .unwrap();

        let root_uri = root_uri_for(dir.path());
        let cache_dir = dir.path().join(".semnav");
        let servers_dir = cache_dir.join("servers");
        let db_path = cache_dir.join("graph.db");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");

        let stats = index_language(&db, "typescript", &root_uri, &servers_dir)
            .await
            .expect("index typescript");
        assert!(
            stats.files_indexed >= 1,
            "expected at least one file indexed, got {stats:?}"
        );

        let status = db
            .get_meta("typescript.lsp_status")
            .await
            .expect("get_meta")
            .expect("typescript.lsp_status recorded");
        assert_eq!(status, "healthy");

        let parent = db
            .get_node_by_fqn("app.repo.Repo")
            .await
            .unwrap()
            .expect("Repo node");
        let child = db
            .get_node_by_fqn("app.repo.Repo.load")
            .await
            .unwrap()
            .expect("load node");
        assert_eq!(parent.node_kind, "Class");
        assert_eq!(parent.language, "typescript");
        assert_eq!(child.kind, 6, "tsserver reports a class method as Method");
        assert_eq!(child.name, "load");
        assert_eq!(child.container_id, parent.id, "load is contained by Repo");
    }

    /// Real gopls, end-to-end. Indexes a `Repo` interface with a `Load`
    /// method and asserts the symbols land in the graph with the right FQN
    /// and container linkage. Uses an interface (not a struct+receiver
    /// method) because gopls's `documentSymbol` nests an interface's methods
    /// under it, but does **not** nest a struct's receiver methods under the
    /// struct (`docs/design/lsp-integration.md` documentSymbol note,
    /// `src/indexer/symbol.rs`'s `flatten_keeps_gopls_receiver_method_as_a_top_level_sibling`)
    /// — this test exercises the shape that mirrors pyright/tsserver's
    /// class+method containment most directly. Ignored by default — it
    /// needs `gopls` on `PATH` (`go install golang.org/x/tools/gopls@latest`).
    #[ignore = "requires gopls on PATH (go install golang.org/x/tools/gopls@latest)"]
    #[tokio::test]
    async fn index_language_real_gopls() {
        let dir = tempdir().expect("tempdir");
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("go.mod"), "module app\n\ngo 1.22\n").unwrap();
        fs::write(
            app.join("repo.go"),
            "package app\n\ntype Repo interface {\n\tLoad()\n}\n",
        )
        .unwrap();

        let root_uri = root_uri_for(dir.path());
        let cache_dir = dir.path().join(".semnav");
        let servers_dir = cache_dir.join("servers");
        let db_path = cache_dir.join("graph.db");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");

        let stats = index_language(&db, "go", &root_uri, &servers_dir)
            .await
            .expect("index go");
        assert!(
            stats.files_indexed >= 1,
            "expected at least one file indexed, got {stats:?}"
        );

        let status = db
            .get_meta("go.lsp_status")
            .await
            .expect("get_meta")
            .expect("go.lsp_status recorded");
        assert_eq!(status, "healthy");

        let parent = db
            .get_node_by_fqn("app.repo.Repo")
            .await
            .unwrap()
            .expect("Repo node");
        let child = db
            .get_node_by_fqn("app.repo.Repo.Load")
            .await
            .unwrap()
            .expect("Load node");
        assert_eq!(parent.node_kind, "Interface");
        assert_eq!(parent.language, "go");
        assert_eq!(child.kind, 6, "gopls reports an interface method as Method");
        assert_eq!(child.name, "Load");
        assert_eq!(child.container_id, parent.id, "Load is contained by Repo");
    }

    // `index_language_real_custom_adapter_via_pyright` lives in
    // `tests/custom_adapter_e2e.rs` instead of here: `custom_adapters()`
    // (`src/adapters/custom.rs`) latches `SEMNAV_CUSTOM_LANGUAGES` into a
    // process-wide `OnceLock` on first access, so sharing this unit test
    // binary's process with other `--ignored` tests risked another test's
    // `builtin_adapters()` call locking it in before this one set the env
    // var. A dedicated `tests/*.rs` file gets its own process.
}
