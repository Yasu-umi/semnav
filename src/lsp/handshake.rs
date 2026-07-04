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
/// Overridden by `SEMNAV_INITIALIZE_TIMEOUT_SECS`.
const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum wait for a `textDocument/documentSymbol` response. Overridden by
/// `SEMNAV_DOCUMENT_SYMBOL_TIMEOUT_SECS`.
const DEFAULT_DOCUMENT_SYMBOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum wait for a query-time LSP round-trip (hover, definition, ...).
/// Overridden by `SEMNAV_QUERY_TIMEOUT_SECS`.
///
/// On large repos, pyright's cross-file requests (`references`,
/// `callHierarchy`) queue behind a single serialized background-analysis pass
/// and can take well over a minute — real-world traces have shown ~135s. This
/// is wide enough to cover that, so a slow-but-live query succeeds instead of
/// timing out and prompting a client retry that only adds to the backlog.
const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(150);

/// `var` as a whole-second override if set and parseable, else `default`
/// (mirrors the `SEMNAV_CACHE_DIR` convention in `main.rs`).
fn timeout_from_env(var: &str, default: Duration) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// [`DEFAULT_INITIALIZE_TIMEOUT`], overridden by `SEMNAV_INITIALIZE_TIMEOUT_SECS`.
pub fn initialize_timeout_from_env() -> Duration {
    timeout_from_env("SEMNAV_INITIALIZE_TIMEOUT_SECS", DEFAULT_INITIALIZE_TIMEOUT)
}

/// [`DEFAULT_DOCUMENT_SYMBOL_TIMEOUT`], overridden by
/// `SEMNAV_DOCUMENT_SYMBOL_TIMEOUT_SECS`.
pub fn document_symbol_timeout_from_env() -> Duration {
    timeout_from_env(
        "SEMNAV_DOCUMENT_SYMBOL_TIMEOUT_SECS",
        DEFAULT_DOCUMENT_SYMBOL_TIMEOUT,
    )
}

/// [`DEFAULT_QUERY_TIMEOUT`], overridden by `SEMNAV_QUERY_TIMEOUT_SECS`.
pub fn query_timeout_from_env() -> Duration {
    timeout_from_env("SEMNAV_QUERY_TIMEOUT_SECS", DEFAULT_QUERY_TIMEOUT)
}

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

/// Run the `initialize` / `initialized` handshake with
/// [`initialize_timeout_from_env`]. Returns the server's `InitializeResult`.
pub async fn initialize(client: &LspClient, root_uri: &str, workspace_name: &str) -> Result<Value> {
    initialize_with_timeout(
        client,
        root_uri,
        workspace_name,
        initialize_timeout_from_env(),
    )
    .await
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
        assert_eq!(DEFAULT_INITIALIZE_TIMEOUT, Duration::from_secs(60));
        assert_eq!(DEFAULT_DOCUMENT_SYMBOL_TIMEOUT, Duration::from_secs(30));
        assert_eq!(DEFAULT_QUERY_TIMEOUT, Duration::from_secs(150));
    }

    // One test, not three: `std::env::set_var`/`remove_var` mutate global
    // process state, so tests toggling different vars in parallel are fine,
    // but each var's own set→check→cleanup must stay within one test function
    // (mirrors `daemon/server.rs::idle_timeout_from_env_defaults_when_unset_and_parses_override`).
    #[test]
    fn timeout_from_env_defaults_when_unset_and_parses_override() {
        unsafe { std::env::remove_var("SEMNAV_QUERY_TIMEOUT_SECS") };
        assert_eq!(query_timeout_from_env(), DEFAULT_QUERY_TIMEOUT);

        unsafe { std::env::set_var("SEMNAV_QUERY_TIMEOUT_SECS", "300") };
        assert_eq!(query_timeout_from_env(), Duration::from_secs(300));

        unsafe { std::env::set_var("SEMNAV_QUERY_TIMEOUT_SECS", "not-a-number") };
        assert_eq!(
            query_timeout_from_env(),
            DEFAULT_QUERY_TIMEOUT,
            "unparseable override falls back to the default"
        );

        unsafe { std::env::remove_var("SEMNAV_QUERY_TIMEOUT_SECS") };
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
