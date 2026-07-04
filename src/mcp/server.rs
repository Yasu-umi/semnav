//! The rmcp server boundary: 8 tools over one [`QueryRuntime`]. Holds no
//! domain logic (`docs/design/crate-structure.md` Decision Point 5) — every tool
//! destructures its input DTO, calls straight into `runtime`, and reshapes
//! the result via `super::dto`.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{ErrorData, Json, ServerHandler, tool, tool_handler, tool_router};

use crate::query::{FindSymbolResult, QueryRuntime, ReadRangeResult, SymbolRef};

use super::dto::{
    CallGraphQueryInput, FindCallPathInput, FindCallPathOutput, FindCalleesOutput,
    FindCallersOutput, FindDefinitionInput, FindDefinitionOutput, FindReferencesOutput,
    FindSymbolInput, ReadRangeInput, RestartLspInput, RestartLspResult, SymbolQueryInput,
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
        description = "Find symbols by fqn/name pattern across the whole repo, indexed via LSP — not a text search. Prefer this over grep when locating a function/class/symbol by name: it won't false-positive on comments/strings/unrelated identically-named text."
    )]
    pub async fn find_symbol(
        &self,
        Parameters(input): Parameters<FindSymbolInput>,
    ) -> Result<Json<FindSymbolResult>, ErrorData> {
        let (pattern, mode, ignore_case, options, filter, page) = input.into_parts()?;
        let result = self
            .runtime
            .find_symbol(&pattern, mode, ignore_case, options, &filter, &page)
            .await
            .map_err(internal_error)?;
        Ok(Json(result))
    }

    #[tool(
        name = "find_definition",
        description = "Resolve the declaration for a symbol or an occurrence position, via LSP go-to-definition. Prefer this over grep/Read when you have a usage site and need its actual declaration — it follows real jumps (renames, re-exports, overloads) that a name-based text search can't."
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
        description = "List every reference to a symbol across the repo, semantically resolved via LSP find-references. Prefer this over grep for 'who uses this' — it matches actual usages of this exact symbol, not every text occurrence of a name that might be reused elsewhere."
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
        description = "List every caller of a function/method via LSP call hierarchy. Prefer this over grep for 'who calls this' — it resolves real call sites, not every text match of the function's name (including unrelated same-named functions)."
    )]
    pub async fn find_callers(
        &self,
        Parameters(input): Parameters<CallGraphQueryInput>,
    ) -> Result<Json<FindCallersOutput>, ErrorData> {
        let (symref, filter, page, with_signature) = input.into_parts()?;
        let result = self
            .runtime
            .find_callers(&symref, &filter, &page, with_signature)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "find_callees",
        description = "List every function/method called from within a function/method, via LSP call hierarchy. Prefer this over manually reading the function body when tracing what it calls, especially across files."
    )]
    pub async fn find_callees(
        &self,
        Parameters(input): Parameters<CallGraphQueryInput>,
    ) -> Result<Json<FindCalleesOutput>, ErrorData> {
        let (symref, filter, page, with_signature) = input.into_parts()?;
        let result = self
            .runtime
            .find_callees(&symref, &filter, &page, with_signature)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "find_call_path",
        description = "Check whether `from` reaches `to` through zero or more `calls` hops (BFS), returning one example path when found. Prefer this over manually chaining find_callees hop-by-hop to answer 'does A call B, even transitively?' across layers. Bounded by max_depth hops and max_lsp_calls live call-hierarchy round trips; when the search is cut short before proving `to` unreachable, the response's limit_reached is true and a reachable:false result must be read as inconclusive, not as a proof that `from` never calls `to`."
    )]
    pub async fn find_call_path(
        &self,
        Parameters(input): Parameters<FindCallPathInput>,
    ) -> Result<Json<FindCallPathOutput>, ErrorData> {
        let (from, to, max_depth, max_lsp_calls) = input.into_parts()?;
        let result = self
            .runtime
            .find_call_path(&from, &to, max_depth, max_lsp_calls)
            .await
            .map_err(internal_error)?;
        Ok(Json(result.into()))
    }

    #[tool(
        name = "read_range",
        description = "Read a source slice directly from disk by line range. Pair with the ranges returned by find_* (which never include body text) instead of re-reading whole files."
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

    #[tool(
        name = "restart_lsp",
        description = "Force a language's LSP server to restart (or all servers if language is omitted); a maintenance operation, not a graph query."
    )]
    pub async fn restart_lsp(
        &self,
        Parameters(input): Parameters<RestartLspInput>,
    ) -> Result<Json<RestartLspResult>, ErrorData> {
        let restarted = self
            .runtime
            .restart_language(input.language.as_deref())
            .await;
        Ok(Json(RestartLspResult { restarted }))
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
            brief: false,
            with_signature: false,
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
    async fn find_call_path_for_unresolvable_endpoints_is_not_degraded() {
        let server = test_server();
        let input = FindCallPathInput {
            from: super::super::dto::SymbolRefInput {
                fqn: Some("repo.nope_a".into()),
                at: None,
            },
            to: super::super::dto::SymbolRefInput {
                fqn: Some("repo.nope_b".into()),
                at: None,
            },
            max_depth: None,
            max_lsp_calls: None,
        };
        let Json(output) = server
            .find_call_path(Parameters(input))
            .await
            .expect("degrades to a proven negative, not an error");
        assert!(!output.result.reachable);
        assert!(!output.result.limit_reached);
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
