//! Per-uri reconciliation glue for the FS watcher (`src/indexer/watcher.rs`):
//! LSP didOpen/didChange + documentSymbol, flatten, diff against the graph.
//!
//! Kept thin — the hard diff-and-apply algorithm lives in
//! [`DbActor::reconcile_file_symbols`] (`src/graph/db.rs`); this module only
//! does the LSP round-trip and the `FlatSymbol` → `ReconcileSymbol` shape
//! conversion (mirroring `pipeline.rs`'s node-building loop).

use anyhow::Result;

use crate::adapters::select_for_uri;
use crate::graph::{DbActor, ReconcileSymbol};
use crate::indexer::{
    flatten_document_symbols, module_path_from_uri, request_document_symbols,
    signature_fingerprint, uri_to_path,
};
use crate::lsp::DOCUMENT_SYMBOL_TIMEOUT;
use crate::query::QueryRuntime;

/// Reconcile one uri's nodes against its current on-disk content. A missing
/// file (deleted/moved away) reads as empty text, which the LSP server
/// reports as zero symbols — uniformly driving the orphan path in
/// `reconcile_file_symbols` with no special-casing here. No-ops if the uri's
/// language server is currently unavailable; a later FS event catches up
/// once it recovers (`docs/design/indexing-and-cache.md`).
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

    let text = tokio::fs::read_to_string(uri_to_path(uri))
        .await
        .unwrap_or_default();

    client.ensure_document(uri, language, &text).await?;

    let symbols = request_document_symbols(&client, uri, DOCUMENT_SYMBOL_TIMEOUT).await?;
    let module_path = module_path_from_uri(uri, root_uri);
    let flat = flatten_document_symbols(&symbols, &module_path);
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
