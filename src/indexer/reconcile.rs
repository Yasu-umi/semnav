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
//! reconciliation").

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;

use crate::adapters::select_for_uri;
use crate::graph::{DbActor, ReconcileSymbol};
use crate::indexer::{
    FlatSymbol, discover_files, flatten_document_symbols, module_path_from_uri,
    request_document_symbols, signature_fingerprint, uri_to_path,
};
use crate::lsp::document_symbol_timeout_from_env;
use crate::query::QueryRuntime;

/// Reconcile one uri's nodes against its current on-disk content. A missing
/// file (deleted/moved away) reads as empty text, which the LSP server
/// reports as zero symbols — uniformly driving the orphan path in
/// `reconcile_file_symbols` with no special-casing here. The synthetic module
/// node (see [`FlatSymbol::module_root`]) is only added when the read
/// succeeds — otherwise it would keep being reclaimed by fqn on every pass
/// and never join the orphan path like the rest of a deleted file's nodes.
/// No-ops if the uri's language server is currently unavailable; a later FS
/// event catches up once it recovers (`docs/design/indexing-and-cache.md`).
pub(crate) async fn reconcile_uri(
    db: &DbActor,
    query_runtime: &QueryRuntime,
    root_uri: &str,
    uri: &str,
) -> Result<()> {
    let Some(adapter) = select_for_uri(uri) else {
        return Ok(());
    };
    let language = adapter.language_name();
    let Some(client) = query_runtime.acquire_for_watcher(language).await else {
        return Ok(());
    };

    let read = tokio::fs::read_to_string(uri_to_path(uri)).await;
    let file_exists = read.is_ok();
    let text = read.unwrap_or_default();

    client.ensure_document(uri, language, &text).await?;

    let symbols =
        request_document_symbols(&client, uri, document_symbol_timeout_from_env()).await?;
    let module_path = module_path_from_uri(uri, root_uri);
    let mut flat = flatten_document_symbols(&symbols, &module_path);
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

    db.reconcile_file_symbols(uri, language, is_external, reconcile_symbols)
        .await?;
    Ok(())
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

/// Catch up on drift that accumulated while no daemon was watching `root`
/// (`docs/design/daemon-lifecycle.md` "Startup drift reconciliation"): every
/// file a fresh walk finds, plus every uri the graph already knows about (so
/// a file deleted during the gap gets reconciled — and orphaned — too), goes
/// through the same [`reconcile_uri`] the live watcher uses. Reconciling an
/// unchanged file is a no-op past the diff step in
/// [`DbActor::reconcile_file_symbols`], but each one still pays a full LSP
/// round-trip, so the caller should run this in the background rather than
/// block on it before serving queries. Individual reconcile failures are
/// logged and skipped, not propagated — one broken file (or a momentarily
/// unavailable LSP server) shouldn't stop the rest of the catch-up.
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
    for uri in &uris {
        query_runtime.wait_until_query_idle().await;
        if let Err(err) = reconcile_uri(db, query_runtime, root_uri, uri).await {
            failures += 1;
            eprintln!("semnav: startup drift reconcile failed for {uri}: {err:#}");
        }
    }
    eprintln!(
        "semnav: startup drift reconcile done: {} file(s) checked, {failures} failure(s)",
        uris.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
