//! rmcp-boundary DTOs: request shapes deserialized straight from tool-call
//! arguments, and response wrappers that fold in the degraded-response fields
//! (`docs/design/mcp-tools.md`, `docs/design/resilience.md`). No domain logic
//! lives here — every conversion is a mechanical reshape into/out of the
//! `query` module's own types.

use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::query::{
    CallGraphNode, CallGraphResult, DEFAULT_CALL_PATH_LSP_BUDGET, DEFAULT_CALL_PATH_MAX_DEPTH,
    Degradation, DegradeReason, Filter, FindCallPathResult, FindDefinitionResult,
    FindReferencesResult, FindSymbolOptions, LspStatus, MAX_CALL_PATH_LSP_BUDGET,
    MAX_CALL_PATH_MAX_DEPTH, MAX_PAGE_LIMIT, MatchMode, Page, SymbolRef,
};

/// An occurrence position (`docs/design/mcp-tools.md` SymbolRef `at`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AtRef {
    pub uri: String,
    pub line: u32,
    pub character: u32,
}

impl From<AtRef> for SymbolRef {
    fn from(at: AtRef) -> Self {
        SymbolRef::At {
            uri: at.uri,
            line: at.line,
            character: at.character,
        }
    }
}

/// A symbol reference: `{fqn}` xor `{at}`. A plain struct (not an untagged
/// enum) because sibling flattened fields (e.g. `FilterInput::language`) share
/// the same JSON object as this one in `SymbolQueryInput`, and an untagged
/// enum's per-variant `deny_unknown_fields` would reject those sibling keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SymbolRefInput {
    pub fqn: Option<String>,
    pub at: Option<AtRef>,
}

impl TryFrom<SymbolRefInput> for SymbolRef {
    type Error = ErrorData;

    fn try_from(input: SymbolRefInput) -> Result<Self, Self::Error> {
        match (input.fqn, input.at) {
            (Some(fqn), None) => Ok(SymbolRef::Fqn(fqn)),
            (None, Some(at)) => Ok(at.into()),
            (Some(_), Some(_)) => Err(ErrorData::invalid_params(
                "symbol ref must not specify both fqn and at",
                None,
            )),
            (None, None) => Err(ErrorData::invalid_params(
                "symbol ref must specify fqn or at",
                None,
            )),
        }
    }
}

/// `find_definition` only accepts an occurrence position — an fqn has no
/// single "definition" to resolve to (`docs/design/mcp-tools.md`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindDefinitionInput {
    pub at: AtRef,
}

impl From<FindDefinitionInput> for SymbolRef {
    fn from(input: FindDefinitionInput) -> Self {
        input.at.into()
    }
}

/// Cross-tool narrowing (`docs/design/mcp-tools.md` Filter).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct FilterInput {
    pub language: Option<String>,
    pub kind: Option<Vec<String>>,
    #[serde(default)]
    pub include_external: bool,
}

impl From<FilterInput> for Filter {
    fn from(input: FilterInput) -> Self {
        Filter {
            language: input.language,
            kind: input.kind,
            include_external: input.include_external,
        }
    }
}

/// Cursor pagination request (`docs/design/mcp-tools.md` Page). `limit`
/// defaults to 100 and is clamped to at least 1; `cursor` is the opaque token
/// handed back as a prior response's `next_cursor`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PageInput {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

impl PageInput {
    pub fn into_page(self) -> Result<Page, ErrorData> {
        let cursor = self
            .cursor
            .map(|token| {
                crate::query::Cursor::decode(&token)
                    .map_err(|e| ErrorData::invalid_params(format!("invalid cursor: {e}"), None))
            })
            .transpose()?;
        Ok(Page {
            limit: self.limit.unwrap_or(100).clamp(1, MAX_PAGE_LIMIT as u32) as usize,
            cursor,
        })
    }
}

/// `find_symbol` request (`docs/design/mcp-tools.md`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindSymbolInput {
    pub pattern: String,
    #[serde(rename = "match", default)]
    pub match_mode: MatchMode,
    #[serde(default)]
    pub ignore_case: bool,
    /// Return only `fqns: string[]` instead of full `nodes: Node[]` — for
    /// gauging match count / narrowing a pattern before paying for full
    /// metadata on a wide result set.
    #[serde(default)]
    pub brief: bool,
    /// Best-effort hover-derived `signature` backfill for each returned node
    /// that doesn't already have one — costs one extra LSP `hover` round trip
    /// per such node, so it's opt-in rather than the default
    /// (`docs/design/mcp-tools.md`). No-op when `brief` is set.
    #[serde(default)]
    pub with_signature: bool,
    #[serde(flatten)]
    pub filter: FilterInput,
    #[serde(flatten)]
    pub page: PageInput,
}

impl FindSymbolInput {
    pub fn into_parts(
        self,
    ) -> Result<(String, MatchMode, bool, FindSymbolOptions, Filter, Page), ErrorData> {
        let page = self.page.into_page()?;
        let options = FindSymbolOptions {
            brief: self.brief,
            with_signature: self.with_signature,
        };
        Ok((
            self.pattern,
            self.match_mode,
            self.ignore_case,
            options,
            self.filter.into(),
            page,
        ))
    }
}

/// Shared request shape for `find_references`/`find_callers`/`find_callees`
/// (`docs/design/mcp-tools.md`): a symbol ref, a filter, and a page.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SymbolQueryInput {
    #[serde(flatten)]
    pub symbol: SymbolRefInput,
    #[serde(flatten)]
    pub filter: FilterInput,
    #[serde(flatten)]
    pub page: PageInput,
}

impl SymbolQueryInput {
    pub fn into_parts(self) -> Result<(SymbolRef, Filter, Page), ErrorData> {
        let page = self.page.into_page()?;
        Ok((self.symbol.try_into()?, self.filter.into(), page))
    }
}

/// `find_callers`/`find_callees` request: [`SymbolQueryInput`] plus the
/// `with_signature` opt-in (`docs/design/mcp-tools.md`). A separate type
/// rather than a field on `SymbolQueryInput` itself, so `find_references`
/// (which shares that struct) doesn't advertise a flag it doesn't implement.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CallGraphQueryInput {
    #[serde(flatten)]
    pub query: SymbolQueryInput,
    /// Best-effort hover-derived `signature` backfill for each returned
    /// node that doesn't already have one — costs one extra LSP `hover`
    /// round trip per such node, so it's opt-in rather than the default.
    #[serde(default)]
    pub with_signature: bool,
}

impl CallGraphQueryInput {
    pub fn into_parts(self) -> Result<(SymbolRef, Filter, Page, bool), ErrorData> {
        let (symref, filter, page) = self.query.into_parts()?;
        Ok((symref, filter, page, self.with_signature))
    }
}

/// `find_call_path` request (`docs/design/mcp-tools.md`): does `from` reach
/// `to` through zero or more `calls` hops? `max_depth`/`max_lsp_calls` bound
/// the BFS (clamped server-side); both default when omitted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindCallPathInput {
    pub from: SymbolRefInput,
    pub to: SymbolRefInput,
    /// Max `calls` hops from `from` before giving up. Defaults to
    /// `DEFAULT_CALL_PATH_MAX_DEPTH`, clamped to `[1, MAX_CALL_PATH_MAX_DEPTH]`.
    pub max_depth: Option<u32>,
    /// Max live call-hierarchy round trips spent expanding cold nodes before
    /// giving up. Defaults to `DEFAULT_CALL_PATH_LSP_BUDGET`, clamped to
    /// `[0, MAX_CALL_PATH_LSP_BUDGET]`.
    pub max_lsp_calls: Option<u32>,
}

impl FindCallPathInput {
    pub fn into_parts(self) -> Result<(SymbolRef, SymbolRef, u32, u32), ErrorData> {
        let from = self.from.try_into()?;
        let to = self.to.try_into()?;
        let max_depth = self
            .max_depth
            .unwrap_or(DEFAULT_CALL_PATH_MAX_DEPTH)
            .clamp(1, MAX_CALL_PATH_MAX_DEPTH);
        let max_lsp_calls = self
            .max_lsp_calls
            .unwrap_or(DEFAULT_CALL_PATH_LSP_BUDGET)
            .clamp(0, MAX_CALL_PATH_LSP_BUDGET);
        Ok((from, to, max_depth, max_lsp_calls))
    }
}

/// `read_range` request (`docs/design/mcp-tools.md`). `range` omitted reads
/// the whole file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadRangeInput {
    pub uri: String,
    pub range: Option<RangeInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RangeInput {
    pub start: PositionInput,
    pub end: PositionInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PositionInput {
    pub line: u32,
    pub character: u32,
}

impl ReadRangeInput {
    pub fn into_parts(self) -> (String, u32, u32) {
        let (start_line, end_line) = self
            .range
            .map(|r| (r.start.line, r.end.line))
            .unwrap_or((0, u32::MAX));
        (self.uri, start_line, end_line)
    }
}

/// `restart_lsp` request: force a specific language's server to restart, or
/// every provisioned language when `language` is omitted. A maintenance
/// operation, not one of the 6 graph-query tools (`docs/design/mcp-tools.md`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestartLspInput {
    pub language: Option<String>,
}

/// `restart_lsp` response: the languages whose server was actually reset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RestartLspResult {
    pub restarted: Vec<String>,
}

/// The degraded-response annotation, folded into a tool's output only when a
/// degradation actually occurred (`docs/design/resilience.md`) — never
/// serialized as `degraded: false`. Owned `String` fields (not `&'static
/// str`): the daemon's proxy leg round-trips this through JSON on both
/// directions, and `&'static str` can't implement `Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DegradeInfo {
    pub degraded: bool,
    pub degrade_reason: String,
    pub lsp_status: String,
}

impl From<Degradation> for DegradeInfo {
    fn from(d: Degradation) -> Self {
        DegradeInfo {
            degraded: true,
            degrade_reason: degrade_reason_str(d.reason).to_string(),
            lsp_status: lsp_status_str(d.status).to_string(),
        }
    }
}

fn degrade_reason_str(reason: DegradeReason) -> &'static str {
    match reason {
        DegradeReason::LspUnavailable => "lsp_unavailable",
        DegradeReason::LspTimeout => "lsp_timeout",
    }
}

fn lsp_status_str(status: LspStatus) -> &'static str {
    match status {
        LspStatus::Down => "down",
        LspStatus::Degraded => "degraded",
    }
}

/// A stale cached result was served immediately while a fresh
/// materialization runs in the background (`docs/design/lsp-integration.md`
/// "cache-first + background refresh"; `docs/design/resilience.md`) — a
/// distinct signal from `degraded` (timeout/unavailable): the server is fine,
/// the cache is just possibly missing a very recent change. Folded in only
/// when a refresh was actually kicked off, so this is never serialized as
/// `refreshing: false`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FreshnessInfo {
    pub refreshing: bool,
}

/// `find_definition` response: the domain result plus an optional degradation
/// annotation, flattened together (field names already match the wire
/// contract).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindDefinitionOutput {
    #[serde(flatten)]
    pub result: FindDefinitionResult,
    #[serde(flatten)]
    pub degrade: Option<DegradeInfo>,
}

impl From<(FindDefinitionResult, Option<Degradation>)> for FindDefinitionOutput {
    fn from((result, degradation): (FindDefinitionResult, Option<Degradation>)) -> Self {
        FindDefinitionOutput {
            result,
            degrade: degradation.map(DegradeInfo::from),
        }
    }
}

/// `find_references` response: see [`FindDefinitionOutput`]. Also carries an
/// optional `refreshing` flag (see [`FreshnessInfo`]) when a warm-cache
/// answer was served while a background refresh runs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesOutput {
    #[serde(flatten)]
    pub result: FindReferencesResult,
    #[serde(flatten)]
    pub degrade: Option<DegradeInfo>,
    #[serde(flatten)]
    pub freshness: Option<FreshnessInfo>,
}

impl From<(FindReferencesResult, Option<Degradation>, bool)> for FindReferencesOutput {
    fn from(
        (result, degradation, refreshing): (FindReferencesResult, Option<Degradation>, bool),
    ) -> Self {
        FindReferencesOutput {
            result,
            degrade: degradation.map(DegradeInfo::from),
            freshness: refreshing.then_some(FreshnessInfo { refreshing: true }),
        }
    }
}

/// `find_callers` response. Domain's `CallGraphResult.items` is renamed to
/// `callers` on the wire, so this reconstructs fields explicitly rather than
/// flattening. Also carries an optional `refreshing` flag (see
/// [`FreshnessInfo`]) when a warm-cache answer was served while a background
/// refresh runs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindCallersOutput {
    pub callers: Vec<CallGraphNode>,
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub hint_fqns: Vec<String>,
    #[serde(flatten)]
    pub degrade: Option<DegradeInfo>,
    #[serde(flatten)]
    pub freshness: Option<FreshnessInfo>,
}

impl From<(CallGraphResult, Option<Degradation>, bool)> for FindCallersOutput {
    fn from(
        (result, degradation, refreshing): (CallGraphResult, Option<Degradation>, bool),
    ) -> Self {
        FindCallersOutput {
            callers: result.items,
            next_cursor: result.next_cursor,
            hint_fqns: result.hint_fqns,
            degrade: degradation.map(DegradeInfo::from),
            freshness: refreshing.then_some(FreshnessInfo { refreshing: true }),
        }
    }
}

/// `find_callees` response: see [`FindCallersOutput`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindCalleesOutput {
    pub callees: Vec<CallGraphNode>,
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub hint_fqns: Vec<String>,
    #[serde(flatten)]
    pub degrade: Option<DegradeInfo>,
}

impl From<(CallGraphResult, Option<Degradation>)> for FindCalleesOutput {
    fn from((result, degradation): (CallGraphResult, Option<Degradation>)) -> Self {
        FindCalleesOutput {
            callees: result.items,
            next_cursor: result.next_cursor,
            hint_fqns: result.hint_fqns,
            degrade: degradation.map(DegradeInfo::from),
        }
    }
}

/// `find_call_path` response: see [`FindDefinitionOutput`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FindCallPathOutput {
    #[serde(flatten)]
    pub result: FindCallPathResult,
    #[serde(flatten)]
    pub degrade: Option<DegradeInfo>,
}

impl From<(FindCallPathResult, Option<Degradation>)> for FindCallPathOutput {
    fn from((result, degradation): (FindCallPathResult, Option<Degradation>)) -> Self {
        FindCallPathOutput {
            result,
            degrade: degradation.map(DegradeInfo::from),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_ref_input_accepts_fqn() {
        let v = serde_json::json!({"fqn": "app.repo.Repo"});
        let input: SymbolRefInput = serde_json::from_value(v).unwrap();
        assert_eq!(
            SymbolRef::try_from(input).unwrap(),
            SymbolRef::Fqn("app.repo.Repo".into())
        );
    }

    #[test]
    fn symbol_ref_input_accepts_at() {
        let v = serde_json::json!({"at": {"uri": "file:///x.py", "line": 1, "character": 2}});
        let input: SymbolRefInput = serde_json::from_value(v).unwrap();
        assert_eq!(
            SymbolRef::try_from(input).unwrap(),
            SymbolRef::At {
                uri: "file:///x.py".into(),
                line: 1,
                character: 2
            }
        );
    }

    #[test]
    fn symbol_ref_input_rejects_both_fqn_and_at() {
        let v = serde_json::json!({
            "fqn": "app.repo.Repo",
            "at": {"uri": "file:///x.py", "line": 1, "character": 2}
        });
        let input: SymbolRefInput = serde_json::from_value(v).unwrap();
        assert!(SymbolRef::try_from(input).is_err());
    }

    #[test]
    fn symbol_ref_input_rejects_neither() {
        let v = serde_json::json!({});
        let input: SymbolRefInput = serde_json::from_value(v).unwrap();
        assert!(SymbolRef::try_from(input).is_err());
    }

    #[test]
    fn page_input_defaults_limit_to_100() {
        let page = PageInput::default().into_page().unwrap();
        assert_eq!(page.limit, 100);
        assert!(page.cursor.is_none());
    }

    #[test]
    fn page_input_clamps_zero_limit_to_one() {
        let page = PageInput {
            limit: Some(0),
            cursor: None,
        }
        .into_page()
        .unwrap();
        assert_eq!(page.limit, 1);
    }

    #[test]
    fn page_input_clamps_limit_above_max_page_limit() {
        let page = PageInput {
            limit: Some(u32::MAX),
            cursor: None,
        }
        .into_page()
        .unwrap();
        assert_eq!(page.limit, MAX_PAGE_LIMIT);
    }

    #[test]
    fn page_input_rejects_garbage_cursor() {
        let err = PageInput {
            limit: None,
            cursor: Some("!!!not-base64".into()),
        }
        .into_page()
        .unwrap_err();
        assert!(err.message.contains("invalid cursor"));
    }

    #[test]
    fn read_range_input_defaults_to_whole_file() {
        let input = ReadRangeInput {
            uri: "file:///x.py".into(),
            range: None,
        };
        assert_eq!(input.into_parts(), ("file:///x.py".into(), 0, u32::MAX));
    }

    #[test]
    fn read_range_input_passes_through_explicit_range() {
        let input = ReadRangeInput {
            uri: "file:///x.py".into(),
            range: Some(RangeInput {
                start: PositionInput {
                    line: 2,
                    character: 0,
                },
                end: PositionInput {
                    line: 5,
                    character: 0,
                },
            }),
        };
        assert_eq!(input.into_parts(), ("file:///x.py".into(), 2, 5));
    }

    #[test]
    fn degrade_info_omitted_when_none_serializes_without_fields() {
        let output = FindDefinitionOutput {
            result: FindDefinitionResult { nodes: vec![] },
            degrade: None,
        };
        let v = serde_json::to_value(&output).unwrap();
        assert!(v.get("degraded").is_none());
    }

    #[test]
    fn degrade_info_present_serializes_true_never_false() {
        let d = Degradation {
            reason: DegradeReason::LspUnavailable,
            status: LspStatus::Down,
        };
        let output = FindDefinitionOutput {
            result: FindDefinitionResult { nodes: vec![] },
            degrade: Some(d.into()),
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v.get("degraded").unwrap(), &serde_json::json!(true));
        assert_eq!(v.get("degrade_reason").unwrap(), "lsp_unavailable");
        assert_eq!(v.get("lsp_status").unwrap(), "down");
    }

    #[test]
    fn degrade_info_serializes_lsp_timeout_reason() {
        let d = Degradation {
            reason: DegradeReason::LspTimeout,
            status: LspStatus::Degraded,
        };
        let output = FindDefinitionOutput {
            result: FindDefinitionResult { nodes: vec![] },
            degrade: Some(d.into()),
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v.get("degrade_reason").unwrap(), "lsp_timeout");
        assert_eq!(v.get("lsp_status").unwrap(), "degraded");
    }

    #[test]
    fn find_call_path_input_defaults_depth_and_budget_when_omitted() {
        let input = FindCallPathInput {
            from: SymbolRefInput {
                fqn: Some("repo.a".into()),
                at: None,
            },
            to: SymbolRefInput {
                fqn: Some("repo.b".into()),
                at: None,
            },
            max_depth: None,
            max_lsp_calls: None,
        };
        let (from, to, max_depth, max_lsp_calls) = input.into_parts().unwrap();
        assert_eq!(from, SymbolRef::Fqn("repo.a".into()));
        assert_eq!(to, SymbolRef::Fqn("repo.b".into()));
        assert_eq!(max_depth, DEFAULT_CALL_PATH_MAX_DEPTH);
        assert_eq!(max_lsp_calls, DEFAULT_CALL_PATH_LSP_BUDGET);
    }

    #[test]
    fn find_call_path_input_clamps_out_of_range_depth_and_budget() {
        let input = FindCallPathInput {
            from: SymbolRefInput {
                fqn: Some("repo.a".into()),
                at: None,
            },
            to: SymbolRefInput {
                fqn: Some("repo.b".into()),
                at: None,
            },
            max_depth: Some(0),
            max_lsp_calls: Some(u32::MAX),
        };
        let (_, _, max_depth, max_lsp_calls) = input.into_parts().unwrap();
        assert_eq!(max_depth, 1, "0 hops would forbid even a trivial search");
        assert_eq!(max_lsp_calls, MAX_CALL_PATH_LSP_BUDGET);
    }

    #[test]
    fn find_call_path_input_rejects_missing_endpoint() {
        let input = FindCallPathInput {
            from: SymbolRefInput::default(),
            to: SymbolRefInput {
                fqn: Some("repo.b".into()),
                at: None,
            },
            max_depth: None,
            max_lsp_calls: None,
        };
        assert!(input.into_parts().is_err());
    }

    #[test]
    fn find_callers_output_renames_items_to_callers() {
        let result = CallGraphResult {
            items: vec![],
            next_cursor: None,
            hint_fqns: Vec::new(),
        };
        let output: FindCallersOutput = (result, None, false).into();
        let v = serde_json::to_value(&output).unwrap();
        assert!(v.get("callers").is_some());
        assert!(v.get("items").is_none());
    }
}
