//! LSP `initialize` handshake — request params/result, the client capabilities
//! semnav relies on, the `workspaceFolders` declaration, and the lifecycle
//! timeouts.
//!
//! `initialize` advertises hierarchical document symbols, hover, and call
//! hierarchy support, and **declares the workspace folder** (rootUri alone does
//! not trigger the server's workspace scan). See `docs/design/lsp-integration.md`
//! and `docs/design/lsp-lifecycle.md` for the timeouts.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::time::timeout;

use super::client::LspClient;

/// Maximum wait for the server's `initialize` response before giving up.
pub const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum wait for a `textDocument/documentSymbol` response.
pub const DOCUMENT_SYMBOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum wait for a query-time LSP round-trip (hover, definition, ...).
///
/// On large repos, pyright's cross-file requests (`references`,
/// `callHierarchy`) queue behind a single serialized background-analysis pass
/// and can take well over a minute — real-world traces have shown ~135s. This
/// is wide enough to cover that, so a slow-but-live query succeeds instead of
/// timing out and prompting a client retry that only adds to the backlog.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(150);

/// Build the `initialize` request params.
///
/// `workspaceFolders` MUST be declared so the server performs its workspace
/// scan; `rootUri` is sent alongside for older servers but is not sufficient on
/// its own. Capabilities advertise exactly what the indexer/query path needs:
/// hierarchical symbols, hover (for `construct` refinement), and call hierarchy.
pub fn build_initialize_params(root_uri: &str, workspace_name: &str) -> Value {
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "workspaceFolders": [
            { "uri": root_uri, "name": workspace_name }
        ],
        "capabilities": {
            "workspace": {
                "workspaceFolders": true
            },
            "textDocument": {
                "documentSymbol": {
                    "hierarchicalDocumentSymbolSupport": true
                },
                "hover": {},
                "callHierarchy": {}
            }
        }
    })
}

/// Run the `initialize` / `initialized` handshake with the default
/// [`INITIALIZE_TIMEOUT`]. Returns the server's `InitializeResult`.
pub async fn initialize(client: &LspClient, root_uri: &str, workspace_name: &str) -> Result<Value> {
    initialize_with_timeout(client, root_uri, workspace_name, INITIALIZE_TIMEOUT).await
}

/// Run the handshake with an explicit timeout. `pub(crate)` so tests can drive
/// the timeout path quickly without waiting 60s.
pub(crate) async fn initialize_with_timeout(
    client: &LspClient,
    root_uri: &str,
    workspace_name: &str,
    deadline: Duration,
) -> Result<Value> {
    let params = build_initialize_params(root_uri, workspace_name);
    let result = timeout(deadline, client.request("initialize", Some(params)))
        .await
        .map_err(|_| anyhow!("initialize timed out after {deadline:?}"))??;
    client
        .notify("initialized", Some(json!({})))
        .await
        .map_err(|e| anyhow!("initialized notification failed: {e}"))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::client::LspClient;
    use tokio::io::duplex;

    #[test]
    fn initialize_params_declare_workspace_and_capabilities() {
        let params = build_initialize_params("file:///repo", "repo");
        assert_eq!(params["rootUri"], "file:///repo");
        assert_eq!(params["workspaceFolders"][0]["uri"], "file:///repo");
        assert_eq!(params["workspaceFolders"][0]["name"], "repo");
        assert_eq!(
            params["capabilities"]["workspace"]["workspaceFolders"],
            true
        );
        assert_eq!(
            params["capabilities"]["textDocument"]["documentSymbol"]["hierarchicalDocumentSymbolSupport"],
            true
        );
        assert!(params["capabilities"]["textDocument"]["hover"].is_object());
        assert!(params["capabilities"]["textDocument"]["callHierarchy"].is_object());
        assert!(
            params["processId"].as_u64().is_some(),
            "processId is a number"
        );
    }

    #[test]
    fn timeouts_match_design_doc() {
        assert_eq!(INITIALIZE_TIMEOUT, Duration::from_secs(60));
        assert_eq!(DOCUMENT_SYMBOL_TIMEOUT, Duration::from_secs(30));
        assert_eq!(QUERY_TIMEOUT, Duration::from_secs(150));
    }

    /// A duplex-backed server that reads nothing and never replies: holding the
    /// ends open without responding forces the initialize round-trip to time out
    /// rather than fail on a closed stream.
    #[tokio::test]
    async fn initialize_times_out_when_server_is_silent() {
        let (client_writer, server_reader) = duplex(8192);
        let (server_writer, client_reader) = duplex(8192);
        tokio::spawn(async move {
            let _ = server_reader;
            let _ = server_writer;
            std::future::pending::<()>().await;
        });
        let client = LspClient::spawn(client_reader, client_writer);

        let res =
            initialize_with_timeout(&client, "file:///r", "r", Duration::from_millis(100)).await;
        let err = res.expect_err("should time out");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "unexpected error: {msg}");
    }
}
