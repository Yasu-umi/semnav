//! The rmcp server boundary: 6 tools over one [`QueryRuntime`]. Holds no
//! domain logic (`docs/design/crate-structure.md` Decision Point 5) — every tool
//! destructures its input DTO, calls straight into `runtime`, and reshapes
//! the result via `super::dto`.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};

use crate::query::{FindSymbolResult, QueryRuntime, ReadRangeResult, SymbolRef};

use super::dto::{
    FindCalleesOutput, FindCallersOutput, FindDefinitionInput, FindDefinitionOutput,
    FindReferencesOutput, FindSymbolInput, ReadRangeInput, SymbolQueryInput,
};

/// The MCP server, exposing `QueryRuntime`'s 6 operations as tools.
#[derive(Clone)]
pub struct SemnavServer {
    runtime: Arc<QueryRuntime>,
    tool_router: ToolRouter<Self>,
}

impl SemnavServer {
    pub fn new(runtime: Arc<QueryRuntime>) -> Self {
        Self {
            runtime,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SemnavServer {}

#[tool_router]
impl SemnavServer {
    #[tool(
        name = "find_symbol",
        description = "Find symbols by fqn pattern (docs/design/mcp-tools.md)."
    )]
    pub async fn find_symbol(
        &self,
        Parameters(input): Parameters<FindSymbolInput>,
    ) -> Result<Json<FindSymbolResult>, ErrorData> {
        let (pattern, mode, ignore_case, filter, page) = input.into_parts()?;
        let result = self
            .runtime
            .find_symbol(&pattern, mode, ignore_case, &filter, &page)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "find_definition",
        description = "Resolve the declaration at an occurrence position (docs/design/mcp-tools.md)."
    )]
    pub async fn find_definition(
        &self,
        Parameters(input): Parameters<FindDefinitionInput>,
    ) -> Result<Json<FindDefinitionOutput>, ErrorData> {
        let symref = SymbolRef::from(input);
        let result = self
            .runtime
            .find_definition(&symref)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "find_references",
        description = "List references to a symbol (docs/design/mcp-tools.md)."
    )]
    pub async fn find_references(
        &self,
        Parameters(input): Parameters<SymbolQueryInput>,
    ) -> Result<Json<FindReferencesOutput>, ErrorData> {
        let (symref, filter, page) = input.into_parts()?;
        let result = self
            .runtime
            .find_references(&symref, &filter, &page)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "find_callers",
        description = "List callers of a symbol (docs/design/mcp-tools.md)."
    )]
    pub async fn find_callers(
        &self,
        Parameters(input): Parameters<SymbolQueryInput>,
    ) -> Result<Json<FindCallersOutput>, ErrorData> {
        let (symref, filter, page) = input.into_parts()?;
        let result = self
            .runtime
            .find_callers(&symref, &filter, &page)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "find_callees",
        description = "List callees of a symbol (docs/design/mcp-tools.md)."
    )]
    pub async fn find_callees(
        &self,
        Parameters(input): Parameters<SymbolQueryInput>,
    ) -> Result<Json<FindCalleesOutput>, ErrorData> {
        let (symref, filter, page) = input.into_parts()?;
        let result = self
            .runtime
            .find_callees(&symref, &filter, &page)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "read_range",
        description = "Read a source slice directly from disk (docs/design/mcp-tools.md)."
    )]
    pub async fn read_range(
        &self,
        Parameters(input): Parameters<ReadRangeInput>,
    ) -> Result<Json<ReadRangeResult>, ErrorData> {
        let (uri, start_line, end_line) = input.into_parts();
        let result = self
            .runtime
            .read_range(&uri, start_line, end_line)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }
}

fn internal_error(err: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(err.to_string(), None)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use crate::graph::DbActor;
    use crate::query::{MatchMode, QueryEngine};

    use super::*;

    fn test_server() -> SemnavServer {
        let dir = tempdir().unwrap();
        let db = DbActor::spawn(&dir.path().join("g.db")).unwrap();
        let engine = QueryEngine::new(db, "file:///repo".into());
        let runtime = QueryRuntime::open(engine, dir.path().join("servers"));
        // Leak the tempdir so its files outlive the runtime for the test's duration.
        std::mem::forget(dir);
        SemnavServer::new(Arc::new(runtime))
    }

    #[test]
    fn find_symbol_schema_declares_pattern_as_string() {
        let attr = SemnavServer::find_symbol_tool_attr();
        let props = attr.input_schema.get("properties").unwrap();
        assert_eq!(props.get("pattern").unwrap().get("type").unwrap(), "string");
    }

    #[tokio::test]
    async fn find_symbol_on_empty_graph_returns_empty_page() {
        let server = test_server();
        let input = FindSymbolInput {
            pattern: "repo".into(),
            match_mode: MatchMode::Segment,
            ignore_case: false,
            filter: Default::default(),
            page: Default::default(),
        };
        let Json(result) = server
            .find_symbol(Parameters(input))
            .await
            .expect("empty graph query succeeds");
        assert!(result.nodes.is_empty());
        assert!(result.next_cursor.is_none());
    }

    #[tokio::test]
    async fn find_references_for_unknown_fqn_is_not_degraded() {
        let server = test_server();
        let input = SymbolQueryInput {
            symbol: super::super::dto::SymbolRefInput {
                fqn: Some("repo.nope".into()),
                at: None,
            },
            filter: Default::default(),
            page: Default::default(),
        };
        let Json(output) = server
            .find_references(Parameters(input))
            .await
            .expect("degrades to empty, not an error");
        assert!(output.result.references.is_empty());
        assert!(output.degrade.is_none());
    }

    #[tokio::test]
    async fn read_range_on_missing_file_is_an_internal_error() {
        let server = test_server();
        let input = ReadRangeInput {
            uri: "file:///does/not/exist.py".into(),
            range: None,
        };
        let result = server.read_range(Parameters(input)).await;
        let Err(err) = result else {
            panic!("missing file surfaces as an error");
        };
        assert!(err.message.contains("read_range"));
    }

    #[tokio::test]
    async fn find_references_with_bad_cursor_is_invalid_params() {
        let server = test_server();
        let input = SymbolQueryInput {
            symbol: super::super::dto::SymbolRefInput {
                fqn: Some("repo.nope".into()),
                at: None,
            },
            filter: Default::default(),
            page: super::super::dto::PageInput {
                limit: None,
                cursor: Some("!!!not-base64".into()),
            },
        };
        let result = server.find_references(Parameters(input)).await;
        let Err(err) = result else {
            panic!("malformed cursor rejected before hitting the runtime");
        };
        assert_eq!(err.code, ErrorData::invalid_params("", None).code);
    }
}
