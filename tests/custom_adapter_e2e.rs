//! Real pyright, driven through a `CustomAdapter` instead of the built-in
//! `PythonAdapter` — proves the config-driven custom-language pipeline
//! (extension matching → `SEMNAV_LSP_<TAG>_COMMAND` override →
//! `server_package() == None` fallback path → indexing) works end to end
//! without needing a genuinely new LSP binary in the dev/CI environment.
//!
//! Lives in its own `tests/*.rs` file (a separate process/binary from the
//! `src/lib.rs` unit test binary) deliberately: `custom_adapters()`
//! (`src/adapters/custom.rs`) reads `SEMNAV_CUSTOM_LANGUAGES` into a
//! process-wide `OnceLock` on first access, so if any *other* `--ignored`
//! test in the same process calls `builtin_adapters()` before this one sets
//! the env var, the lock latches without the custom language and this test
//! fails with "no built-in adapter for language fakelang" — a real
//! test-ordering failure observed when running `cargo test -- --ignored`
//! without `--test-threads=1`. A dedicated integration test file gets its
//! own process, so this test's `OnceLock` can never be contaminated by
//! another test's `builtin_adapters()` call.
//!
//! Ignored by default — it needs node/npm and provisions pyright from npm
//! on first run.

use std::fs;

use semnav::graph::DbActor;
use semnav::indexer::{index_language, path_to_uri};

fn root_uri_for(dir: &std::path::Path) -> String {
    format!("{}/", path_to_uri(dir).trim_end_matches('/'))
}

#[ignore = "requires node/npm; provisions pyright from npm on first run"]
#[tokio::test]
async fn index_language_real_custom_adapter_via_pyright() {
    // SAFETY: this test is the only thing running in this process (its own
    // `tests/*.rs` binary), so there's no cross-test race on these vars or
    // on the `custom_adapters()` `OnceLock` they seed.
    unsafe {
        std::env::set_var("SEMNAV_CUSTOM_LANGUAGES", "fakelang");
        std::env::set_var("SEMNAV_LSP_FAKELANG_EXTENSIONS", ".fakelang");
        std::env::set_var("SEMNAV_LSP_FAKELANG_COMMAND", "pyright-langserver");
        std::env::set_var("SEMNAV_LSP_FAKELANG_ARGS", "--stdio");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let app = dir.path().join("app");
    fs::create_dir_all(&app).unwrap();
    fs::write(
        app.join("repo.fakelang"),
        "class Repo:\n    def load(self) -> None: ...\n",
    )
    .unwrap();

    let root_uri = root_uri_for(dir.path());
    let cache_dir = dir.path().join(".semnav");
    let servers_dir = cache_dir.join("servers");
    let db_path = cache_dir.join("graph.db");
    fs::create_dir_all(db_path.parent().unwrap()).unwrap();
    let db = DbActor::spawn(&db_path).expect("spawn db");

    let stats = index_language(&db, "fakelang", &root_uri, &servers_dir)
        .await
        .expect("index fakelang");
    assert!(
        stats.files_indexed >= 1,
        "expected at least one file indexed, got {stats:?}"
    );

    let status = db
        .get_meta("fakelang.lsp_status")
        .await
        .expect("get_meta")
        .expect("fakelang.lsp_status recorded");
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
    assert_eq!(parent.language, "fakelang");
    assert!(
        matches!(child.kind, 6 | 12),
        "expected Method/Function, got kind {} ({})",
        child.kind,
        child.node_kind
    );
    assert_eq!(child.name, "load");
    assert_eq!(child.container_id, parent.id, "load is contained by Repo");

    unsafe {
        std::env::remove_var("SEMNAV_CUSTOM_LANGUAGES");
        std::env::remove_var("SEMNAV_LSP_FAKELANG_EXTENSIONS");
        std::env::remove_var("SEMNAV_LSP_FAKELANG_COMMAND");
        std::env::remove_var("SEMNAV_LSP_FAKELANG_ARGS");
    }
}
