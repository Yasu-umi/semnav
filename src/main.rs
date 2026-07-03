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
    eprintln!();
    eprintln!("environment:");
    eprintln!("  SEMNAV_CACHE_DIR         override the index/cache dir (default <root>/.semnav)");
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

    let result = match server.serve(rmcp::transport::io::stdio()).await {
        Ok(running) => {
            let cancel_token = running.cancellation_token();
            tokio::spawn(async move {
                let ctrl_c = tokio::signal::ctrl_c();
                #[cfg(unix)]
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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
