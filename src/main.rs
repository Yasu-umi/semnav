//! semnav — LSP-backed Semantic Graph MCP server (CLI entry point).
//!
//! 0.0.1: a thin CLI over the [`semnav`] library. `discover` lists source
//! files; `index` provisions a real LSP server per language and writes the
//! documentSymbol → graph index into `<root>/.semnav/graph.db`. See
//! `docs/design/crate-structure.md`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use rmcp::ServiceExt;
use semnav::adapters::{adapter_for_language, builtin_adapters};
use semnav::daemon;
use semnav::graph::DbActor;
use semnav::indexer::{
    FsWatcher, discover_files, index_language, path_to_uri, reconcile_startup_drift,
};
use semnav::mcp::{ProxyServer, SemnavServer};
use semnav::query::{QueryEngine, QueryRuntime};

fn main() -> ExitCode {
    init_tracing();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("discover") => discover(&args[2..]),
        Some("index") => index(&args[2..]),
        Some("serve") => serve(&args[2..]),
        Some("daemon") => daemon_cmd(&args[2..]),
        Some("--help") | Some("-h") | Some("help") => {
            print_help();
            ExitCode::SUCCESS
        }
        _ => {
            print_help();
            ExitCode::SUCCESS
        }
    }
}

/// Wire up `tracing` for the `tool{}`/`lsp_request{}` spans
/// (`docs/design/observability.md`). Silent by default (`warn`-and-above, and
/// nothing currently logs at that level) — writes to stderr only, so it can
/// never land on the stdout stream `serve`'s MCP JSON-RPC protocol owns
/// (`rmcp::transport::io::stdio()`, below). Controlled by `SEMNAV_LOG`
/// (`RUST_LOG`-style syntax, e.g. `SEMNAV_LOG=semnav=debug`), not `RUST_LOG`
/// itself, to stay under the same `SEMNAV_*` namespace as this binary's other
/// env vars (see `print_help`).
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(tracing::level_filters::LevelFilter::WARN.into())
        .with_env_var("SEMNAV_LOG")
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_writer(std::io::stderr)
        .init();
}

fn print_help() {
    eprintln!("semnav 0.0.1 — LSP-backed Semantic Graph MCP server");
    eprintln!();
    eprintln!("usage:");
    eprintln!("  semnav discover <root>   list source files (Python/TS/Rust/Go) under <root>");
    eprintln!("  semnav index <root>      index <root> into <root>/.semnav/graph.db");
    eprintln!("                           (provisions pyright/tsserver via npm, needs node + npm;");
    eprintln!(
        "                            rust-analyzer and gopls must already be on PATH, e.g. via"
    );
    eprintln!("                            rustup / `go install golang.org/x/tools/gopls@latest`)");
    eprintln!("  semnav serve <root>      serve the 8 MCP tools over stdio, proxied to a");
    eprintln!("                           background daemon (auto-started; run `index` first)");
    eprintln!(
        "  semnav daemon <root>     run the persistent daemon directly (usually auto-started by `serve`)"
    );
    eprintln!("  semnav daemon stop <root> stop a running daemon for <root>");
    eprintln!();
    eprintln!("environment:");
    eprintln!("  SEMNAV_CACHE_DIR         override the index/cache dir (default <root>/.semnav)");
    eprintln!(
        "  SEMNAV_DAEMON_IDLE_TIMEOUT_SECS  daemon self-shutdown after this many idle seconds (default 1800)"
    );
    eprintln!(
        "  SEMNAV_INITIALIZE_TIMEOUT_SECS         LSP `initialize` handshake timeout (default 60)"
    );
    eprintln!("  SEMNAV_DOCUMENT_SYMBOL_TIMEOUT_SECS    LSP `documentSymbol` timeout (default 30)");
    eprintln!(
        "  SEMNAV_QUERY_TIMEOUT_SECS              query-time LSP round-trip timeout (default 150)"
    );
    eprintln!(
        "  SEMNAV_LSP_<LANG>_COMMAND  override the LSP server binary for <LANG> (e.g. RUST, PYTHON, TYPESCRIPT, GO)"
    );
    eprintln!(
        "  SEMNAV_LSP_<LANG>_ARGS     extra args appended to that language's LSP server startup command"
    );
    eprintln!(
        "  SEMNAV_LOG                 tracing filter (RUST_LOG syntax, e.g. `semnav=debug`);\n                             default is silent (docs/design/observability.md)"
    );
}

/// Resolve a CLI `<root>` argument to an absolute, canonical, existing
/// directory. Every subcommand must go through this: [`path_to_uri`] assumes
/// an absolute path and has no way to detect a relative one, so an
/// un-canonicalized root (e.g. `.`) silently produces a malformed
/// `file:///.`-style URI. That URI round-trips through `uri_to_path` as
/// filesystem-root-relative, so the indexer ends up walking `/` instead of
/// the intended directory — canonicalizing here, once, before the path is
/// ever turned into a URI, is what closes that off.
fn resolve_root(root_arg: &str) -> Result<PathBuf, String> {
    let root = PathBuf::from(root_arg)
        .canonicalize()
        .map_err(|e| format!("{root_arg}: {e}"))?;
    if !root.is_dir() {
        return Err(format!("{root_arg} is not a directory"));
    }
    Ok(root)
}

fn discover(args: &[String]) -> ExitCode {
    let Some(root_arg) = args.first() else {
        eprintln!("usage: semnav discover <root>");
        return ExitCode::from(2);
    };
    let root = match resolve_root(root_arg) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("discover: {err}");
            return ExitCode::FAILURE;
        }
    };
    match discover_files(&root) {
        Ok(uris) => {
            for uri in &uris {
                println!("{uri}");
            }
            eprintln!("{} source file(s) under {}", uris.len(), root.display());
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("discover failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn index(args: &[String]) -> ExitCode {
    let Some(root_arg) = args.first() else {
        eprintln!("usage: semnav index <root>");
        return ExitCode::from(2);
    };
    let root = match resolve_root(root_arg) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("index: {err}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("index: failed to start runtime: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_index(&root))
}

fn serve(args: &[String]) -> ExitCode {
    let Some(root_arg) = args.first() else {
        eprintln!("usage: semnav serve <root>");
        return ExitCode::from(2);
    };
    let root = match resolve_root(root_arg) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("serve: {err}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("serve: failed to start runtime: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_serve(&root))
}

/// Serve the 8 MCP tools over stdio, proxied to a persistent background
/// `daemon` (`docs/design/daemon-lifecycle.md`). `serve` holds none of the
/// domain state itself — no `DbActor`, no `QueryRuntime`, no LSP supervisor —
/// it auto-starts the daemon if one isn't already running for `root`, then
/// forwards every tool call over `DaemonClient`. Runs until the client closes
/// the stdio transport; the daemon keeps running independently afterward.
async fn run_serve(root: &Path) -> ExitCode {
    let cache_dir = resolve_cache_dir(root);
    let db_path = cache_dir.join("graph.db");
    if !db_path.exists() {
        eprintln!(
            "serve: {} not found — run `semnav index {}` first",
            db_path.display(),
            root.display()
        );
        return ExitCode::FAILURE;
    }

    let daemon_client = match daemon::connect::ensure_and_connect(root, &cache_dir).await {
        Ok(client) => client,
        Err(err) => {
            eprintln!("serve: {err}");
            return ExitCode::FAILURE;
        }
    };
    let reconnecting = daemon::reconnect::ReconnectingDaemonClient::new(
        root.to_path_buf(),
        cache_dir.clone(),
        daemon_client,
    );
    let server = ProxyServer::new(reconnecting);

    let mut shutdown_rx = install_shutdown_signal();
    let result = match server.serve(rmcp::transport::io::stdio()).await {
        Ok(running) => {
            let cancel_token = running.cancellation_token();
            tokio::spawn(async move {
                let _ = shutdown_rx.changed().await;
                cancel_token.cancel();
            });
            running.waiting().await.map(|_| ())
        }
        Err(err) => {
            eprintln!("serve: failed to start: {err:#}");
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("serve: transport error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Install a ctrl-c/SIGTERM handler that flips the returned watch to `true`
/// once either fires. Shared by `run_serve` (triggers the rmcp cancellation
/// token) and `run_daemon` (triggers the accept loop's shutdown, `daemon.rs`
/// `ShutdownReason::Signal`) so the two don't carry independently-drifting
/// copies of the same signal-handling logic.
fn install_shutdown_signal() -> tokio::sync::watch::Receiver<bool> {
    let (tx, rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        #[cfg(unix)]
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        };
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        let _ = tx.send(true);
    });
    rx
}

fn daemon_cmd(args: &[String]) -> ExitCode {
    if args.first().map(String::as_str) == Some("stop") {
        let Some(root_arg) = args.get(1) else {
            eprintln!("usage: semnav daemon stop <root>");
            return ExitCode::from(2);
        };
        let root = match resolve_root(root_arg) {
            Ok(root) => root,
            Err(err) => {
                eprintln!("daemon stop: {err}");
                return ExitCode::FAILURE;
            }
        };
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(err) => {
                eprintln!("daemon stop: failed to start runtime: {err:#}");
                return ExitCode::FAILURE;
            }
        };
        return runtime.block_on(run_daemon_stop(&root));
    }

    let Some(root_arg) = args.first() else {
        eprintln!("usage: semnav daemon <root>");
        return ExitCode::from(2);
    };
    let root = match resolve_root(root_arg) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("daemon: {err}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("daemon: failed to start runtime: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_daemon(&root))
}

/// Run a persistent daemon for `root`: the `DbActor`/`QueryEngine`/
/// `QueryRuntime`/`SemnavServer`/`FsWatcher` construction `run_serve` used to
/// do itself, now bound to a Unix socket (`daemon::discovery::sock_path`)
/// instead of stdio, and kept alive until signaled, told to stop, or idle
/// (`docs/design/daemon-lifecycle.md`). Usually auto-spawned by `run_serve`
/// via `ensure_daemon_running`, but also runnable directly for debugging.
async fn run_daemon(root: &Path) -> ExitCode {
    let cache_dir = resolve_cache_dir(root);
    let servers_dir = cache_dir.join("servers");
    let db_path = cache_dir.join("graph.db");
    if !db_path.exists() {
        eprintln!(
            "daemon: {} not found — run `semnav index {}` first",
            db_path.display(),
            root.display()
        );
        return ExitCode::FAILURE;
    }
    if let Err(err) = tokio::fs::create_dir_all(&cache_dir).await {
        eprintln!("daemon: cannot create {}: {err:#}", cache_dir.display());
        return ExitCode::FAILURE;
    }

    if daemon::discovery::probe_liveness(&cache_dir).await == daemon::discovery::Liveness::Live {
        eprintln!("daemon: a daemon is already running for {}", root.display());
        return ExitCode::FAILURE;
    }

    let lock_path = daemon::discovery::lock_path(&cache_dir);
    let _lock = match daemon::lock::DaemonLock::try_acquire(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            eprintln!(
                "daemon: another daemon is already starting for {}",
                root.display()
            );
            return ExitCode::FAILURE;
        }
        Err(err) => {
            eprintln!("daemon: cannot acquire {}: {err:#}", lock_path.display());
            return ExitCode::FAILURE;
        }
    };

    let sock_path = daemon::discovery::sock_path(&cache_dir);
    // A crashed daemon can leave the socket inode behind; probe_liveness
    // already removes a genuinely stale one, but be defensive since we now
    // hold the lock exclusively and are about to bind.
    let _ = std::fs::remove_file(&sock_path);
    let listener = match tokio::net::UnixListener::bind(&sock_path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("daemon: cannot bind {}: {err:#}", sock_path.display());
            return ExitCode::FAILURE;
        }
    };

    let pid_path = daemon::discovery::pid_path(&cache_dir);
    let _ = tokio::fs::write(&pid_path, format!("{}\n", std::process::id())).await;

    let db = match DbActor::spawn(&db_path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("daemon: cannot open {}: {err:#}", db_path.display());
            let _ = std::fs::remove_file(&sock_path);
            let _ = std::fs::remove_file(&pid_path);
            return ExitCode::FAILURE;
        }
    };

    let root_uri = root_uri_for(root);
    let engine = QueryEngine::new(db.clone(), root_uri.clone());
    let query_runtime = Arc::new(QueryRuntime::open(engine, servers_dir));
    let semnav_server = SemnavServer::new(query_runtime.clone());

    let watcher = FsWatcher::spawn(
        db.clone(),
        query_runtime.clone(),
        root.to_path_buf(),
        root_uri.clone(),
    )
    .inspect_err(|err| {
        eprintln!(
            "daemon: fs watcher failed to start (continuing without live invalidation): {err:#}"
        );
    })
    .ok();

    // Catch up on drift the watcher couldn't have seen — changes made while
    // this root had no daemon running at all (`docs/design/daemon-lifecycle.md`
    // "Startup drift reconciliation"). Backgrounded so a large repo doesn't
    // delay this daemon accepting connections; queries in the meantime just
    // see the same pre-existing snapshot they always would have.
    let drift_root = root.to_path_buf();
    tokio::spawn({
        let db = db.clone();
        let query_runtime = query_runtime.clone();
        let root_uri = root_uri.clone();
        async move {
            if let Err(err) =
                reconcile_startup_drift(&db, &query_runtime, &drift_root, &root_uri).await
            {
                eprintln!("daemon: startup drift reconcile failed: {err:#}");
            }
        }
    });

    let shutdown_rx = install_shutdown_signal();
    let idle_timeout = daemon::server::idle_timeout_from_env();
    let reason = daemon::server::run(semnav_server, listener, idle_timeout, shutdown_rx).await;
    eprintln!("daemon: shutting down ({reason:?})");

    if let Some(watcher) = &watcher {
        watcher.shutdown().await;
    }
    query_runtime.shutdown_all().await;
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);
    // `_lock` drops here, releasing the flock for a future daemon.

    ExitCode::SUCCESS
}

/// Ask a running daemon to stop and wait (bounded) for it to actually exit,
/// so a caller doing `semnav daemon stop <root> && semnav index <root>` gets
/// a real exclusivity guarantee on `graph.db`. A no-op success if no daemon
/// is running — safe to call defensively.
async fn run_daemon_stop(root: &Path) -> ExitCode {
    let cache_dir = resolve_cache_dir(root);
    if daemon::discovery::probe_liveness(&cache_dir).await
        == daemon::discovery::Liveness::NotRunning
    {
        eprintln!("daemon stop: no daemon running for {}", root.display());
        return ExitCode::SUCCESS;
    }

    let sock_path = daemon::discovery::sock_path(&cache_dir);
    let stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(stream) => stream,
        Err(err) => {
            eprintln!(
                "daemon stop: cannot connect to {}: {err:#}",
                sock_path.display()
            );
            return ExitCode::FAILURE;
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);
    let envelope = daemon::protocol::DaemonEnvelope {
        id: 0,
        request: daemon::protocol::DaemonRequest::Shutdown,
    };
    if let Err(err) = daemon::protocol::write_line(&mut write_half, &envelope).await {
        eprintln!("daemon stop: failed to send stop request: {err:#}");
        return ExitCode::FAILURE;
    }
    let _: Option<daemon::protocol::DaemonResponseEnvelope> =
        daemon::protocol::read_line(&mut reader)
            .await
            .unwrap_or(None);

    for _ in 0..50 {
        if daemon::discovery::probe_liveness(&cache_dir).await
            == daemon::discovery::Liveness::NotRunning
        {
            eprintln!("daemon stop: stopped");
            return ExitCode::SUCCESS;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    eprintln!("daemon stop: sent stop request but the daemon did not exit within 5s");
    ExitCode::FAILURE
}

/// Resolve the cache dir: `SEMNAV_CACHE_DIR` if set, else `<root>/.semnav`
/// (`docs/design/crate-structure.md` — cache resolution is the bin layer's job).
fn resolve_cache_dir(root: &Path) -> PathBuf {
    match std::env::var("SEMNAV_CACHE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => root.join(".semnav"),
    }
}

/// Normalize a root path to a `file://` URI with exactly one trailing slash.
fn root_uri_for(root: &Path) -> String {
    format!("{}/", path_to_uri(root).trim_end_matches('/'))
}

/// Drive the index pass: open the graph, provision + index each language, and
/// tolerate partial failure (a single language's server failing still leaves a
/// useful index for the others). Exits `FAILURE` only if every language failed.
async fn run_index(root: &Path) -> ExitCode {
    let cache_dir = resolve_cache_dir(root);
    let first_creation = !cache_dir.exists();
    let servers_dir = cache_dir.join("servers");
    let db_path = cache_dir.join("graph.db");

    if let Err(err) = tokio::fs::create_dir_all(&servers_dir).await {
        eprintln!("index: cannot create {}: {err:#}", servers_dir.display());
        return ExitCode::FAILURE;
    }

    let db = match DbActor::spawn(&db_path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("index: cannot open {}: {err:#}", db_path.display());
            return ExitCode::FAILURE;
        }
    };

    let root_uri = root_uri_for(root);
    let languages: Vec<&'static str> = builtin_adapters()
        .into_iter()
        .map(|a| a.language_name())
        .collect();
    let mut failures = 0u32;
    for language in languages.iter().copied() {
        match index_language(&db, language, &root_uri, &servers_dir).await {
            Ok(stats) => {
                eprintln!("[{language}] {stats:?}");
            }
            Err(err) => {
                failures += 1;
                eprintln!("[{language}] failed: {err:#}");
                let npm_provisioned = adapter_for_language(language)
                    .and_then(|a| a.server_package())
                    .is_some();
                if npm_provisioned {
                    eprintln!(
                        "  hint: ensure node + npm are on PATH; semnav provisions the \
                         {language} LSP server via npm into {}",
                        servers_dir.display()
                    );
                } else {
                    eprintln!(
                        "  hint: this language's server isn't auto-installable; ensure its \
                         LSP server is on PATH (e.g. `rustup component add rust-analyzer` for Rust)"
                    );
                }
            }
        }
    }

    if first_creation {
        eprintln!(
            "note: created {} — consider adding `.semnav/` to {}/.gitignore",
            cache_dir.display(),
            root.display()
        );
    }

    if failures == languages.len() as u32 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A root ending in a relative `.` component (as `discover_files`'s walker
    /// would hand back verbatim, or a CLI arg like `some/dir/.`) must
    /// canonicalize away the `.` — this is the exact shape that, unresolved,
    /// turns into a malformed `file:///.`-style uri and makes the indexer walk
    /// `/` instead of the intended directory.
    #[test]
    fn resolve_root_canonicalizes_relative_dot_component() {
        let dir = tempdir().expect("tempdir");
        let messy = dir.path().join(".");
        let resolved = resolve_root(messy.to_str().unwrap()).expect("resolves");
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
        assert!(!resolved.to_string_lossy().ends_with('.'));
    }

    #[test]
    fn resolve_root_rejects_nonexistent_path() {
        let err = resolve_root("/no/such/semnav-test-path-xyz").expect_err("must fail");
        assert!(err.contains("/no/such/semnav-test-path-xyz"));
    }

    #[test]
    fn resolve_root_rejects_a_file() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("not_a_dir.txt");
        std::fs::write(&file, "x").unwrap();
        let err = resolve_root(file.to_str().unwrap()).expect_err("must fail");
        assert!(err.contains("is not a directory"));
    }
}
