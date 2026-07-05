//! Per-uri reconciliation glue for the FS watcher (`src/indexer/watcher.rs`):
//! LSP didOpen/didChange + documentSymbol, flatten, diff against the graph.
//!
//! Kept thin — the hard diff-and-apply algorithm lives in
//! [`DbActor::reconcile_file_symbols`] (`src/graph/db.rs`); this module only
//! does the LSP round-trip and the `FlatSymbol` → `ReconcileSymbol` shape
//! conversion (mirroring `pipeline.rs`'s node-building loop).
//!
//! Also home to [`reconcile_startup_drift`], the daemon-startup catch-up pass
//! that reuses this same per-uri glue for a different trigger: not an fs
//! event, but "a daemon just started and doesn't know what changed while no
//! watcher was running" (`docs/design/daemon-lifecycle.md` "Startup drift
//! reconciliation"). Its per-uri step, `reconcile_uri_for_startup_drift`,
//! additionally guards against a still-warming-up LSP server reporting a
//! false "zero symbols" for a file that previously had real ones
//! (github.com/Yasu-umi/semnav#6), and against a still-unavailable LSP server
//! being silently treated as a successful "no symbols" commit rather than
//! retried (github.com/Yasu-umi/semnav#7).

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::adapters::select_for_uri;
use crate::graph::{DbActor, ReconcileSymbol};
use crate::indexer::{
    FlatSymbol, discover_files, flatten_document_symbols, module_path_from_uri,
    request_document_symbols, signature_fingerprint, uri_to_path,
};
use crate::lsp::document_symbol_timeout_from_env;
use crate::query::QueryRuntime;

/// A uri's symbols freshly fetched from the LSP server, not yet committed.
struct FetchedSymbols {
    language: &'static str,
    is_external: bool,
    module_fqn: String,
    /// `false` once a missing file (deleted/moved away) is read as empty
    /// text — the module-root node is omitted in that case, see
    /// [`fetch_uri_symbols`].
    file_exists: bool,
    /// Count of real (non-module-root) symbols the LSP server returned.
    real_symbol_count: usize,
    reconcile_symbols: Vec<ReconcileSymbol>,
}

/// Outcome of trying to fetch a uri's current symbols from its LSP server.
enum FetchOutcome {
    /// No adapter recognizes this uri (e.g. `Cargo.toml`) — not a language
    /// semnav indexes, permanently and not worth retrying.
    Unsupported,
    /// The uri's language server is currently unavailable. Distinct from
    /// [`FetchOutcome::Fetched`] with zero symbols: callers must not treat
    /// "couldn't ask" the same as "asked and got nothing"
    /// (github.com/Yasu-umi/semnav#7).
    LspUnavailable,
    Fetched(FetchedSymbols),
}

/// Fetch `uri`'s current symbols from its LSP server.
async fn fetch_uri_symbols(
    query_runtime: &QueryRuntime,
    root_uri: &str,
    uri: &str,
) -> Result<FetchOutcome> {
    let Some(adapter) = select_for_uri(uri) else {
        return Ok(FetchOutcome::Unsupported);
    };
    let language = adapter.language_name();
    let Some(client) = query_runtime.acquire_for_watcher(language).await else {
        return Ok(FetchOutcome::LspUnavailable);
    };

    let read = tokio::fs::read_to_string(uri_to_path(uri)).await;
    let file_exists = read.is_ok();
    let text = read.unwrap_or_default();

    client.ensure_document(uri, language, &text).await?;

    let symbols =
        request_document_symbols(&client, uri, document_symbol_timeout_from_env()).await?;
    let module_path = module_path_from_uri(uri, root_uri);
    let mut flat = flatten_document_symbols(&symbols, &module_path);
    let real_symbol_count = flat.len();
    if file_exists {
        flat.push(FlatSymbol::module_root(&module_path));
    }
    let is_external = adapter.is_external(uri, root_uri);
    let reconcile_symbols: Vec<ReconcileSymbol> = flat
        .iter()
        .map(|sym| ReconcileSymbol {
            fqn: sym.fqn.clone(),
            name: sym.name.clone(),
            kind: sym.kind as i64,
            node_kind: adapter.map_symbol_kind(sym.kind).to_label(),
            range: sym.range,
            sel: sym.sel,
            detail: sym.detail.clone(),
            signature_hash: signature_fingerprint(sym),
            parent: sym.parent,
        })
        .collect();

    Ok(FetchOutcome::Fetched(FetchedSymbols {
        language,
        is_external,
        module_fqn: module_path,
        file_exists,
        real_symbol_count,
        reconcile_symbols,
    }))
}

/// Reconcile one uri's nodes against its current on-disk content. A missing
/// file (deleted/moved away) reads as empty text, which the LSP server
/// reports as zero symbols — uniformly driving the orphan path in
/// `reconcile_file_symbols` with no special-casing here. Used by the live FS
/// watcher, where an empty result always corresponds to a real edit that just
/// happened, so it's trusted unconditionally (unlike
/// [`reconcile_uri_for_startup_drift`]).
pub(crate) async fn reconcile_uri(
    db: &DbActor,
    query_runtime: &QueryRuntime,
    root_uri: &str,
    uri: &str,
) -> Result<()> {
    let fetched = match fetch_uri_symbols(query_runtime, root_uri, uri).await? {
        FetchOutcome::Unsupported | FetchOutcome::LspUnavailable => return Ok(()),
        FetchOutcome::Fetched(fetched) => fetched,
    };
    db.reconcile_file_symbols(
        uri,
        fetched.language,
        fetched.is_external,
        fetched.reconcile_symbols,
    )
    .await?;
    Ok(())
}

/// Result of one startup-drift reconcile attempt for a single uri.
#[derive(Debug)]
enum DriftAttempt {
    /// Fetched and committed (possibly a no-op past the diff step).
    Committed,
    /// The file exists and previously had real symbols indexed, but this
    /// fetch came back with zero — most likely the LSP server is still
    /// warming up rather than the file genuinely having been emptied
    /// (github.com/Yasu-umi/semnav#6). Left uncommitted so the existing index
    /// isn't overwritten with a false "no symbols" result; the caller retries
    /// later.
    SuspiciousEmpty,
    /// The uri's language server wasn't available for this attempt at all —
    /// there's no fetch result to judge, suspicious or otherwise. Must not be
    /// treated as [`DriftAttempt::Committed`]: unlike the live watcher, this
    /// pass has no guaranteed future FS event to catch up on, so a silently
    /// skipped uri here means it never gets reconciled this pass
    /// (github.com/Yasu-umi/semnav#7).
    LspUnavailable,
}

/// Like [`reconcile_uri`], but for the startup-drift pass specifically: an
/// empty `documentSymbol` result isn't trusted as-is when the uri previously
/// had real symbols, since (unlike the live watcher) this fetch isn't
/// triggered by an actual edit — it's a cold probe that can race the LSP
/// server's own warm-up scan.
async fn reconcile_uri_for_startup_drift(
    db: &DbActor,
    query_runtime: &QueryRuntime,
    root_uri: &str,
    uri: &str,
) -> Result<DriftAttempt> {
    let fetched = match fetch_uri_symbols(query_runtime, root_uri, uri).await? {
        FetchOutcome::Unsupported => return Ok(DriftAttempt::Committed),
        FetchOutcome::LspUnavailable => return Ok(DriftAttempt::LspUnavailable),
        FetchOutcome::Fetched(fetched) => fetched,
    };
    if fetched.file_exists && fetched.real_symbol_count == 0 {
        let previously_had_symbols = db.count_real_nodes(uri, &fetched.module_fqn).await? > 0;
        if previously_had_symbols {
            return Ok(DriftAttempt::SuspiciousEmpty);
        }
    }
    db.reconcile_file_symbols(
        uri,
        fetched.language,
        fetched.is_external,
        fetched.reconcile_symbols,
    )
    .await?;
    Ok(DriftAttempt::Committed)
}

/// Union of a fresh disk walk with the graph's already-known uris, deduped.
/// The walk alone would miss files deleted while nothing was watching (they
/// no longer appear in `discovered`, so the only way to reconcile them —
/// and let them take their orphan strike — is to also revisit every uri the
/// graph still remembers).
fn drift_candidates(discovered: Vec<String>, known: Vec<String>) -> Vec<String> {
    let mut set: HashSet<String> = discovered.into_iter().collect();
    set.extend(known);
    set.into_iter().collect()
}

/// Bounded number of extra passes over uris flagged
/// [`DriftAttempt::SuspiciousEmpty`] or [`DriftAttempt::LspUnavailable`],
/// giving a still-warming-up (github.com/Yasu-umi/semnav#6) or
/// still-unavailable (github.com/Yasu-umi/semnav#7) LSP server more time
/// before its result — or lack of one — is accepted as ground truth.
const SUSPICIOUS_RETRY_ATTEMPTS: usize = 5;
/// Delay between retry passes over suspicious uris.
const SUSPICIOUS_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Catch up on drift that accumulated while no daemon was watching `root`
/// (`docs/design/daemon-lifecycle.md` "Startup drift reconciliation"): every
/// file a fresh walk finds, plus every uri the graph already knows about (so
/// a file deleted during the gap gets reconciled — and orphaned — too), goes
/// through [`reconcile_uri_for_startup_drift`]. Reconciling an unchanged file
/// is a no-op past the diff step in [`DbActor::reconcile_file_symbols`], but
/// each one still pays a full LSP round-trip, so the caller should run this
/// in the background rather than block on it before serving queries.
/// Individual reconcile failures are logged and skipped, not propagated — one
/// broken file (or a momentarily unavailable LSP server) shouldn't stop the
/// rest of the catch-up. Files whose fetch looked suspiciously empty
/// (previously had real symbols, now has none, github.com/Yasu-umi/semnav#6)
/// or whose LSP server was unavailable for the attempt
/// (github.com/Yasu-umi/semnav#7) are held back from the first pass and
/// retried up to `SUSPICIOUS_RETRY_ATTEMPTS` times, spaced
/// `SUSPICIOUS_RETRY_DELAY` apart, before their existing index entries are
/// left untouched and logged as skipped.
pub async fn reconcile_startup_drift(
    db: &DbActor,
    query_runtime: &QueryRuntime,
    root: &Path,
    root_uri: &str,
) -> Result<()> {
    let walk_root = root.to_path_buf();
    let discovered = tokio::task::spawn_blocking(move || discover_files(&walk_root)).await??;
    let known = db.known_uris().await?;
    let uris = drift_candidates(discovered, known);

    eprintln!(
        "semnav: startup drift reconcile: checking {} file(s) for {}",
        uris.len(),
        root.display()
    );
    let mut failures = 0usize;
    let mut suspicious = Vec::new();
    for uri in &uris {
        query_runtime.wait_until_query_idle().await;
        match reconcile_uri_for_startup_drift(db, query_runtime, root_uri, uri).await {
            Ok(DriftAttempt::Committed) => {}
            Ok(DriftAttempt::SuspiciousEmpty | DriftAttempt::LspUnavailable) => {
                suspicious.push(uri.clone())
            }
            Err(err) => {
                failures += 1;
                eprintln!("semnav: startup drift reconcile failed for {uri}: {err:#}");
            }
        }
    }

    let mut skipped = Vec::new();
    for uri in suspicious {
        let mut resolved = false;
        for _ in 0..SUSPICIOUS_RETRY_ATTEMPTS {
            tokio::time::sleep(SUSPICIOUS_RETRY_DELAY).await;
            query_runtime.wait_until_query_idle().await;
            match reconcile_uri_for_startup_drift(db, query_runtime, root_uri, &uri).await {
                Ok(DriftAttempt::Committed) => {
                    resolved = true;
                    break;
                }
                Ok(DriftAttempt::SuspiciousEmpty | DriftAttempt::LspUnavailable) => continue,
                Err(err) => {
                    failures += 1;
                    eprintln!("semnav: startup drift reconcile failed for {uri}: {err:#}");
                    resolved = true;
                    break;
                }
            }
        }
        if !resolved {
            eprintln!(
                "semnav: startup drift reconcile: {uri} could not be reconciled after \
                 {SUSPICIOUS_RETRY_ATTEMPTS} retries — either it kept returning zero symbols \
                 despite previously having real ones, or its LSP server never became available \
                 — leaving its existing index entries untouched \
                 (github.com/Yasu-umi/semnav#6, github.com/Yasu-umi/semnav#7)"
            );
            skipped.push(uri);
        }
    }

    eprintln!(
        "semnav: startup drift reconcile done: {} file(s) checked, {failures} failure(s), \
         {} skipped as unresolved (suspiciously empty or LSP unavailable)",
        uris.len(),
        skipped.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::QueryEngine;
    use tempfile::tempdir;

    #[test]
    fn drift_candidates_dedupes_and_unions() {
        let discovered = vec!["file:///a.py".to_string(), "file:///b.py".to_string()];
        let known = vec!["file:///b.py".to_string(), "file:///c.py".to_string()];

        let mut got = drift_candidates(discovered, known);
        got.sort();
        assert_eq!(
            got,
            vec![
                "file:///a.py".to_string(),
                "file:///b.py".to_string(),
                "file:///c.py".to_string(),
            ],
            "b.py (present in both) appears once; a.py (disk-only) and \
             c.py (graph-only, i.e. deleted while unwatched) both survive"
        );
    }

    /// Forces `acquire_for_watcher` to fail on its very first (synchronous
    /// spawn) attempt by pointing `SEMNAV_LSP_RUST_COMMAND` at a nonexistent
    /// binary (`src/adapters/provision.rs::command_override_from_env`) —
    /// no real rust-analyzer process, no network, no backoff wait. Proves
    /// `reconcile_uri_for_startup_drift` reports `LspUnavailable` for a
    /// dead-on-arrival LSP server instead of silently treating it as
    /// `Committed` (github.com/Yasu-umi/semnav#7 defect 2).
    #[tokio::test]
    async fn reconcile_uri_for_startup_drift_flags_lsp_unavailable_instead_of_committing() {
        let dir = tempdir().expect("tempdir");
        let db = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn db");
        let engine = QueryEngine::new(db.clone(), "file:///root".to_string());
        let query_runtime = QueryRuntime::open(engine, dir.path().join("servers"));

        // One test per var, set→check→cleanup within the same test function —
        // env vars are global process state, racy across parallel tests if
        // split across functions touching the same var.
        unsafe {
            std::env::set_var(
                "SEMNAV_LSP_RUST_COMMAND",
                "/nonexistent/semnav-test-rust-analyzer",
            );
        }
        let result = reconcile_uri_for_startup_drift(
            &db,
            &query_runtime,
            "file:///root",
            "file:///root/lib.rs",
        )
        .await;
        unsafe {
            std::env::remove_var("SEMNAV_LSP_RUST_COMMAND");
        }

        assert!(
            matches!(result, Ok(DriftAttempt::LspUnavailable)),
            "expected LspUnavailable for a dead-on-arrival LSP server, got {result:?}"
        );
    }
}
