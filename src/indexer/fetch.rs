//! Abstracts `textDocument/documentSymbol` so the pipeline runs against a real
//! [`LspClient`] or an in-memory mock in tests.

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;
use tokio::time::timeout;

use crate::indexer::{DocumentSymbol, uri_to_path};
use crate::lsp::LspClient;

/// Fetch the document symbols for a single file URI.
pub trait SymbolFetcher {
    fn document_symbols(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<Vec<DocumentSymbol>>> + Send;
}

/// Real fetcher over an [`LspClient`], enforcing a per-file timeout.
///
/// Each request is preceded by `textDocument/didOpen`: pyright (and tsserver)
/// return an empty symbol list for a closed document fired at immediately after
/// `initialized`, because their background workspace scan has not yet picked
/// the file up. Opening the document forces on-demand analysis, so the symbol
/// result is deterministic regardless of scan timing. The text is read from
/// disk (the indexer only ever walks real files).
pub struct LspSymbolFetcher<'a> {
    client: &'a LspClient,
    timeout: Duration,
    language_id: &'a str,
}

impl<'a> LspSymbolFetcher<'a> {
    /// `language_id` is the LSP `textDocument/didOpen` language id for every
    /// file this fetcher opens (e.g. `"python"`, `"typescript"`). One fetcher
    /// serves one language's server.
    pub fn new(client: &'a LspClient, timeout: Duration, language_id: &'a str) -> Self {
        Self {
            client,
            timeout,
            language_id,
        }
    }
}

impl SymbolFetcher for LspSymbolFetcher<'_> {
    fn document_symbols(
        &self,
        uri: &str,
    ) -> impl Future<Output = Result<Vec<DocumentSymbol>>> + Send {
        let client = self.client.clone();
        let deadline = self.timeout;
        let language_id = self.language_id.to_string();
        let uri = uri.to_string();
        async move {
            let path = uri_to_path(&uri);
            let text = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| anyhow!("didOpen: cannot read {}: {e}", path.display()))?;
            client
                .notify(
                    "textDocument/didOpen",
                    Some(json!({
                        "textDocument": {
                            "uri": uri,
                            "languageId": language_id,
                            "version": 1,
                            "text": text,
                        }
                    })),
                )
                .await
                .map_err(|e| anyhow!("didOpen notify failed: {e}"))?;

            request_document_symbols(&client, &uri, deadline).await
        }
    }
}

/// Request `textDocument/documentSymbol` for an already-open `uri`, enforcing
/// `deadline` and normalizing the server's `null`-for-no-symbols response.
/// Shared by [`LspSymbolFetcher`] (which sends its own `didOpen` first) and the
/// FS-watcher reconcile path (`src/indexer/reconcile.rs`), which manages
/// `didOpen`/`didChange` itself and must not send a redundant second `didOpen`.
pub(crate) async fn request_document_symbols(
    client: &LspClient,
    uri: &str,
    deadline: Duration,
) -> Result<Vec<DocumentSymbol>> {
    let params = json!({ "textDocument": { "uri": uri } });
    let raw = timeout(
        deadline,
        client.request("textDocument/documentSymbol", Some(params)),
    )
    .await
    .map_err(|_| anyhow!("documentSymbol timed out after {deadline:?}"))??;
    // Servers return `null` for files with no symbols.
    if raw.is_null() {
        return Ok(Vec::new());
    }
    let symbols: Vec<DocumentSymbol> = serde_json::from_value(raw)?;
    Ok(symbols)
}
