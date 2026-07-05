//! `serve`'s stdio-facing rmcp server: the same 8 tools as [`SemnavServer`],
//! but each one forwards to a background `daemon` over [`DaemonClient`]
//! instead of touching a [`QueryRuntime`] directly. Holds no domain state —
//! not `DbActor`, not `QueryRuntime`, not an LSP supervisor — matching this
//! module's own "no domain logic" rule (`docs/design/crate-structure.md`
//! Decision Point 5), now applied to the daemon link too
//! (`docs/design/daemon-lifecycle.md`).

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};
use serde::de::DeserializeOwned;

use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonRequest;
use crate::query::{FindSymbolResult, ReadRangeResult};

use super::dto::{
    CallGraphQueryInput, FindCallPathInput, FindCallPathOutput, FindCalleesOutput,
    FindCallersOutput, FindDefinitionInput, FindDefinitionOutput, FindReferencesOutput,
    FindSymbolInput, ReadRangeInput, RestartLspInput, RestartLspResult, SymbolQueryInput,
};

/// The proxy MCP server, exposing the same 8 tools as [`SemnavServer`] by
/// forwarding every call through [`DaemonClient`].
#[derive(Clone)]
pub struct ProxyServer {
    daemon: DaemonClient,
    tool_router: ToolRouter<Self>,
}

impl ProxyServer {
    pub fn new(daemon: DaemonClient) -> Self {
        Self {
            daemon,
            tool_router: Self::tool_router(),
        }
    }

    /// Forward `request` to the daemon and deserialize its JSON result into
    /// `T`, reducing any protocol/tool-level failure to an `ErrorData`
    /// (mirrors `SemnavServer`'s own `internal_error` mapping).
    async fn call<T: DeserializeOwned>(&self, request: DaemonRequest) -> Result<T, ErrorData> {
        let value = self
            .daemon
            .call(request)
            .await
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
        serde_json::from_value(value)
            .map_err(|e| ErrorData::internal_error(format!("malformed daemon response: {e}"), None))
    }
}

#[tool_handler(
    router = self.tool_router,
    instructions = "Before grepping or reading whole files to navigate this codebase's structure, load this server's tools (find_symbol, find_definition, find_references, find_callers, find_callees, find_call_path, read_range) — they may need an explicit tool-search step to become callable. They resolve real semantic relationships via LSP (definitions, references, call hierarchy), not text matches, and are almost always cheaper and more precise than grep/Read for 'where is X defined', 'who calls/uses X', or 'does A reach B'."
)]
impl ServerHandler for ProxyServer {}

#[tool_router]
impl ProxyServer {
    #[tool(
        name = "find_symbol",
        description = "Find symbols by fqn pattern (docs/design/mcp-tools.md)."
    )]
    pub async fn find_symbol(
        &self,
        Parameters(input): Parameters<FindSymbolInput>,
    ) -> Result<Json<FindSymbolResult>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::FindSymbol(input)).await?))
    }

    #[tool(
        name = "find_definition",
        description = "Resolve the declaration at an occurrence position (docs/design/mcp-tools.md)."
    )]
    pub async fn find_definition(
        &self,
        Parameters(input): Parameters<FindDefinitionInput>,
    ) -> Result<Json<FindDefinitionOutput>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::FindDefinition(input)).await?))
    }

    #[tool(
        name = "find_references",
        description = "List references to a symbol (docs/design/mcp-tools.md)."
    )]
    pub async fn find_references(
        &self,
        Parameters(input): Parameters<SymbolQueryInput>,
    ) -> Result<Json<FindReferencesOutput>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::FindReferences(input)).await?))
    }

    #[tool(
        name = "find_callers",
        description = "List callers of a symbol (docs/design/mcp-tools.md)."
    )]
    pub async fn find_callers(
        &self,
        Parameters(input): Parameters<CallGraphQueryInput>,
    ) -> Result<Json<FindCallersOutput>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::FindCallers(input)).await?))
    }

    #[tool(
        name = "find_callees",
        description = "List callees of a symbol (docs/design/mcp-tools.md)."
    )]
    pub async fn find_callees(
        &self,
        Parameters(input): Parameters<CallGraphQueryInput>,
    ) -> Result<Json<FindCalleesOutput>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::FindCallees(input)).await?))
    }

    #[tool(
        name = "find_call_path",
        description = "Check reachability from one symbol to another over calls hops (docs/design/mcp-tools.md)."
    )]
    pub async fn find_call_path(
        &self,
        Parameters(input): Parameters<FindCallPathInput>,
    ) -> Result<Json<FindCallPathOutput>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::FindCallPath(input)).await?))
    }

    #[tool(
        name = "read_range",
        description = "Read a source slice directly from disk (docs/design/mcp-tools.md)."
    )]
    pub async fn read_range(
        &self,
        Parameters(input): Parameters<ReadRangeInput>,
    ) -> Result<Json<ReadRangeResult>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::ReadRange(input)).await?))
    }

    #[tool(
        name = "restart_lsp",
        description = "Force a language's LSP server to restart (or all servers if language is omitted); a maintenance operation, not a graph query (docs/design/mcp-tools.md)."
    )]
    pub async fn restart_lsp(
        &self,
        Parameters(input): Parameters<RestartLspInput>,
    ) -> Result<Json<RestartLspResult>, ErrorData> {
        Ok(Json(self.call(DaemonRequest::RestartLsp(input)).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{DaemonEnvelope, DaemonResponseEnvelope, read_line, write_line};
    use tokio::io::{AsyncRead, AsyncWrite, BufReader, duplex};

    /// A minimal fake daemon that answers every request by echoing a canned
    /// value keyed by the request's `op` tag — enough to prove `ProxyServer`
    /// forwards correctly and deserializes into the right output type,
    /// without a real `QueryRuntime`/pyright underneath.
    async fn fake_daemon<R, W>(reader: R, mut writer: W)
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut reader = BufReader::new(reader);
        loop {
            let envelope: DaemonEnvelope = match read_line(&mut reader).await {
                Ok(Some(e)) => e,
                _ => return,
            };
            let result = match &envelope.request {
                DaemonRequest::FindSymbol(_) => {
                    Ok(serde_json::json!({"nodes": [], "next_cursor": null}))
                }
                DaemonRequest::RestartLsp(_) => Ok(serde_json::json!({"restarted": ["python"]})),
                _ => Err("unsupported in this fake".to_string()),
            };
            let response = DaemonResponseEnvelope {
                id: envelope.id,
                result,
            };
            if write_line(&mut writer, &response).await.is_err() {
                return;
            }
        }
    }

    fn test_proxy() -> ProxyServer {
        let (client_reader, server_writer) = duplex(4096);
        let (server_reader, client_writer) = duplex(4096);
        tokio::spawn(fake_daemon(server_reader, server_writer));
        ProxyServer::new(DaemonClient::spawn(client_reader, client_writer))
    }

    #[tokio::test]
    async fn find_symbol_forwards_and_deserializes_the_typed_result() {
        let proxy = test_proxy();
        let input = FindSymbolInput {
            pattern: "repo".into(),
            match_mode: Default::default(),
            ignore_case: false,
            brief: false,
            with_signature: false,
            filter: Default::default(),
            page: Default::default(),
        };
        let Json(result) = proxy.find_symbol(Parameters(input)).await.unwrap();
        assert!(result.nodes.is_empty());
    }

    #[tokio::test]
    async fn restart_lsp_forwards_and_deserializes_the_typed_result() {
        let proxy = test_proxy();
        let input = RestartLspInput { language: None };
        let Json(result) = proxy.restart_lsp(Parameters(input)).await.unwrap();
        assert_eq!(result.restarted, vec!["python".to_string()]);
    }

    #[tokio::test]
    async fn get_info_advertises_instructions_for_deferred_tool_discovery() {
        let proxy = test_proxy();
        let instructions = proxy.get_info().instructions.expect("instructions set");
        assert!(instructions.contains("find_symbol"));
    }

    #[tokio::test]
    async fn daemon_error_surfaces_as_internal_error() {
        let proxy = test_proxy();
        let input = ReadRangeInput {
            uri: "file:///whatever.py".into(),
            range: None,
        };
        let result = proxy.read_range(Parameters(input)).await;
        assert!(result.is_err());
    }
}
