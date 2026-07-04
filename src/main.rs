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
use semnav::daemon;
use semnav::graph::DbActor;
use semnav::indexer::{FsWatcher, discover_files, index_language, path_to_uri};
use semnav::mcp::SemnavServer;
use semnav::query::{QueryEngine, QueryRuntime};

fn main() -> ExitCode {
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

fn print_help() {
    eprintln!("semnav 0.0.1 — LSP-backed Semantic Graph MCP server");
    eprintln!();
    eprintln!("usage:");
    eprintln!("  semnav discover <root>   list source files (Python/TS) under <root>");
    eprintln!("  semnav index <root>      index <root> into <root>/.semnav/graph.db");
    eprintln!("                           (provisions pyright/tsserver via npm; needs node + npm)");
    eprintln!("  semnav serve <root>      serve the 6 MCP tools over stdio against");
    eprintln!("                           <root>/.semnav/graph.db (run `index` first)");
    eprintln!("  semnav daemon <root>     run a persistent daemon holding LSP servers warm");
    eprintln!("                           across connections (standalone; not yet wired into `serve`)");
    eprintln!("  semnav daemon stop <root> stop a running daemon for <root>");
    eprintln!();
    eprintln!("environment:");
    eprintln!("  SEMNAV_CACHE_DIR         override the index/cache dir (default <root>/.semnav)");
    eprintln!(
        "  SEMNAV_DAEMON_IDLE_TIMEOUT_SECS  daemon self-shutdown after this many idle seconds (default 1800)"
    );
}

fn discover(args: &[String]) -> ExitCode {
    let Some(root) = args.first() else {
        eprintln!("usage: semnav discover <root>");
        return ExitCode::from(2);
    };
    match discover_files(&PathBuf::from(root)) {
        Ok(uris) => {
            for uri in &uris {
                println!("{uri}");
            }
            eprintln!("{} source file(s) under {root}", uris.len());
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
    let root = PathBuf::from(root_arg);
    if !root.is_dir() {
        eprintln!("index: {root_arg} is not a directory");
        return ExitCode::FAILURE;
    }
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
    let root = PathBuf::from(root_arg);
    if !root.is_dir() {
        eprintln!("serve: {root_arg} is not a directory");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("serve: failed to start runtime: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_serve(&root))
}

/// Serve the 6 MCP tools over stdio against an already-indexed graph
/// (`docs/design/mcp-tools.md`). Runs until the client closes the stdio
/// transport, then shuts down every provisioned LSP server before exiting.
async fn run_serve(root: &Path) -> ExitCode {
    let cache_dir = resolve_cache_dir(root);
    let servers_dir = cache_dir.join("servers");
    let db_path = cache_dir.join("graph.db");
    if !db_path.exists() {
        eprintln!(
            "serve: {} not found — run `semnav index {}` first",
            db_path.display(),
            root.display()
        );
        return ExitCode::FAILURE;
    }

    let db = match DbActor::spawn(&db_path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("serve: cannot open {}: {err:#}", db_path.display());
            return ExitCode::FAILURE;
        }
    };

    let root_uri = root_uri_for(root);
    let engine = QueryEngine::new(db.clone(), root_uri.clone());
    let query_runtime = Arc::new(QueryRuntime::open(engine, servers_dir));
    let server = SemnavServer::new(query_runtime.clone());

    let watcher = FsWatcher::spawn(db, query_runtime.clone(), root.to_path_buf(), root_uri)
        .inspect_err(|err| {
            eprintln!(
                "serve: fs watcher failed to start (continuing without live invalidation): {err:#}"
            );
        })
        .ok();

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
            if let Some(watcher) = &watcher {
                watcher.shutdown().await;
            }
            query_runtime.shutdown_all().await;
            return ExitCode::FAILURE;
        }
    };

    if let Some(watcher) = &watcher {
        watcher.shutdown().await;
    }
    query_runtime.shutdown_all().await;
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
        let root = PathBuf::from(root_arg);
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
    let root = PathBuf::from(root_arg);
    if !root.is_dir() {
        eprintln!("daemon: {root_arg} is not a directory");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("daemon: failed to start runtime: {err:#}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_daemon(&root))
}

/// Run a persistent daemon for `root`: the same `DbActor`/`QueryEngine`/
/// `QueryRuntime`/`SemnavServer`/`FsWatcher` construction `run_serve` does,
/// but bound to a Unix socket (`daemon::discovery::sock_path`) instead of
/// stdio, and kept alive until signaled, told to stop, or idle
/// (`docs/design/daemon-lifecycle.md`). Standalone in this step — nothing
/// spawns it automatically yet; a future `run_serve` will.
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

    let watcher = FsWatcher::spawn(db, query_runtime.clone(), root.to_path_buf(), root_uri)
        .inspect_err(|err| {
            eprintln!(
                "daemon: fs watcher failed to start (continuing without live invalidation): {err:#}"
            );
        })
        .ok();

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
    if daemon::discovery::probe_liveness(&cache_dir).await == daemon::discovery::Liveness::NotRunning
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
        daemon::protocol::read_line(&mut reader).await.unwrap_or(None);

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
    let languages = ["python", "typescript"];
    let mut failures = 0u32;
    for language in languages {
        match index_language(&db, language, &root_uri, &servers_dir).await {
            Ok(stats) => {
                eprintln!("[{language}] {stats:?}");
            }
            Err(err) => {
                failures += 1;
                eprintln!("[{language}] failed: {err:#}");
                eprintln!(
                    "  hint: ensure node + npm are on PATH; semnav provisions the \
                     {language} LSP server via npm into {}",
                    servers_dir.display()
                );
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
