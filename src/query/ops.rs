//! The six query operations on [`QueryEngine`] (`docs/design/mcp-tools.md`):
//! `find_symbol`, `read_range`, `find_definition`, `find_references`,
//! `find_callers`, `find_callees`. Each is generic over an optional
//! [`LspQueryClient`]; with `None` they serve the materialized cache, with
//! `Some` they construct edges on demand first.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use anyhow::{Result, anyhow};

use crate::adapters::NodeKind;
use crate::graph::Direction;

use super::dto::{
    CallGraphNode, CallGraphResult, FindDefinitionResult, FindReferencesResult, FindSymbolResult,
    NodeDto, Position, RangeDto, ReadRangeResult, ReferenceGroup,
};
use super::filter::{Filter, MatchMode, Page, SortKey, SymbolRef};
use super::lsp_query::{LspQueryClient, is_timeout};
use super::resolver::group_by_node;
use super::{QueryEngine, paginate};

impl QueryEngine {
    /// List symbols whose fqn matches `pattern` under `mode`, narrowed by
    /// `filter`, paginated by the stable sort key. `brief` returns just the
    /// matched `fqn`s instead of full nodes, for a much smaller payload when
    /// a wide pattern (e.g. `match="contains"`) would otherwise return
    /// hundreds of full `NodeDto`s.
    pub async fn find_symbol(
        &self,
        pattern: &str,
        mode: MatchMode,
        ignore_case: bool,
        brief: bool,
        filter: &Filter,
        page: &Page,
    ) -> Result<FindSymbolResult> {
        let mut nodes = self.db.list_nodes(filter.language.as_deref()).await?;
        nodes.retain(|n| mode.matches(pattern, ignore_case, &n.fqn) && filter.matches(n));
        let (page_items, next) = paginate(nodes, page, SortKey::from_node);
        let next_cursor = next.map(|c| c.encode());
        if brief {
            Ok(FindSymbolResult {
                nodes: Vec::new(),
                fqns: page_items.into_iter().map(|n| n.fqn).collect(),
                next_cursor,
            })
        } else {
            Ok(FindSymbolResult {
                nodes: page_items.iter().map(NodeDto::from_node).collect(),
                fqns: Vec::new(),
                next_cursor,
            })
        }
    }

    /// Read the source slice for lines `[start_line, end_line)` (0-based,
    /// half-open) directly from the filesystem. `range` is the covered span
    /// (end pinned to the start of the line after the last included one).
    pub async fn read_range(
        &self,
        uri: &str,
        start_line: u32,
        end_line: u32,
    ) -> Result<ReadRangeResult> {
        let path = self.confine(uri).map_err(|e| anyhow!("read_range: {e}"))?;
        let text = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| anyhow!("read_range: cannot read {uri}: {e}"))?;
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let start = (start_line as usize).min(total);
        let end = (end_line as usize).max(start).min(total);
        let content = lines[start..end].join("\n");
        Ok(ReadRangeResult {
            uri: uri.to_string(),
            content,
            range: RangeDto {
                start: Position {
                    line: start as u32,
                    character: 0,
                },
                end: Position {
                    line: end as u32,
                    character: 0,
                },
            },
            total_lines: total as u32,
        })
    }

    /// Resolve a declaration. `Fqn` ⇒ the node itself; `At` ⇒ on-demand
    /// `textDocument/definition`, each target resolved to an indexed node
    /// (external targets dropped). Cache-only `At` (no client) returns empty.
    /// The returned `bool` is `true` when the `definition` call itself timed
    /// out (the `hover` call used for construct-refinement is not flagged: its
    /// silent failure doesn't corrupt the returned definition).
    pub async fn find_definition<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        client: Option<&C>,
    ) -> Result<(FindDefinitionResult, bool)> {
        match symref {
            SymbolRef::Fqn(fqn) => {
                let node = self.db.get_node_by_fqn(fqn).await?;
                let nodes = node.into_iter().map(|n| NodeDto::from_node(&n)).collect();
                Ok((FindDefinitionResult { nodes }, false))
            }
            SymbolRef::At {
                uri,
                line,
                character,
            } => {
                let Some(client) = client else {
                    return Ok((FindDefinitionResult { nodes: Vec::new() }, false));
                };
                self.ensure_open(uri, client).await;
                let (locs, timed_out) = match client.definition(uri, *line, *character).await {
                    Ok(locs) => (locs, false),
                    Err(e) => (Vec::new(), is_timeout(&e)),
                };
                let mut nodes = Vec::new();
                for loc in locs {
                    if let Some(mut node) = self
                        .db
                        .find_node_by_position(
                            &loc.uri,
                            loc.range.start.line as i64,
                            loc.range.start.character as i64,
                        )
                        .await?
                    {
                        if node.construct.is_none()
                            && let Some(id) = node.id
                            && let Ok(Some(hover)) = client
                                .hover(
                                    &node.uri,
                                    node.sel.start_line as u32,
                                    node.sel.start_col as u32,
                                )
                                .await
                            && let Some(construct) = NodeKind::construct_from_hover(&hover.value)
                        {
                            let _ = self.db.update_node_construct(id, &construct).await;
                            node.construct = Some(construct);
                        }
                        nodes.push(NodeDto::from_node(&node));
                    }
                }
                Ok((FindDefinitionResult { nodes }, timed_out))
            }
        }
    }

    /// Find nodes referencing `symref`, grouped by referencing node with each
    /// occurrence range. On-demand `textDocument/references` materializes edges
    /// when a client is supplied. The returned `bool` is `true` if any
    /// underlying LSP call (anchor resolution or `references`) timed out.
    pub async fn find_references<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        filter: &Filter,
        page: &Page,
        client: Option<&C>,
    ) -> Result<(FindReferencesResult, bool)> {
        let empty = || FindReferencesResult {
            references: Vec::new(),
            next_cursor: None,
        };
        let (anchor_id, mut timed_out) = self.resolve_anchor_id(symref, client).await?;
        let Some((anchor_id, anchor)) = anchor_id else {
            return Ok((empty(), timed_out));
        };
        if let Some(client) = client {
            self.ensure_open(&anchor.uri, client).await;
            timed_out |= self
                .materialize_references(anchor_id, &anchor, client)
                .await?;
        }
        let neighbors = self
            .db
            .get_neighbors(anchor_id, "references", Direction::Incoming)
            .await?;
        let grouped = group_by_node(neighbors);
        let filtered: Vec<_> = grouped
            .into_iter()
            .filter(|(n, _)| filter.matches(n))
            .collect();
        let (page_items, next) = paginate(filtered, page, |(n, _)| SortKey::from_node(n));
        let references = page_items
            .into_iter()
            .map(|(node, sites)| ReferenceGroup {
                node: NodeDto::from_node(&node),
                sites: sites.into_iter().map(|s| RangeDto::from(s.range)).collect(),
            })
            .collect();
        Ok((
            FindReferencesResult {
                references,
                next_cursor: next.map(|c| c.encode()),
            },
            timed_out,
        ))
    }

    /// Find `symref`'s callers (incoming `calls`). On-demand call hierarchy when
    /// a client is supplied.
    pub async fn find_callers<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        filter: &Filter,
        page: &Page,
        client: Option<&C>,
    ) -> Result<(CallGraphResult, bool)> {
        self.call_graph(symref, Direction::Incoming, filter, page, client)
            .await
    }

    /// Find `symref`'s callees (outgoing `calls`) via a precise content-hash
    /// cache (`docs/design/lsp-integration.md` "callees precise cache"): the
    /// outgoing callee list of a node depends only on its own file's text, so
    /// a byte-identical anchor file since the last materialization means the
    /// cached graph is exact — including the zero-callees case, since the
    /// write path (`reconcile_file_symbols_tx`) unconditionally drops the
    /// cache row whenever the anchor's file is reconciled with changed
    /// content (a same-line-count body edit can change callees without
    /// changing `signature_hash`, which the ordinary edge invalidation keys
    /// off). A cache miss falls back to on-demand call hierarchy, as before.
    pub async fn find_callees<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        filter: &Filter,
        page: &Page,
        client: Option<&C>,
    ) -> Result<(CallGraphResult, bool)> {
        let empty = || CallGraphResult {
            items: Vec::new(),
            next_cursor: None,
        };
        let (anchor_id, mut timed_out) = self.resolve_anchor_id(symref, client).await?;
        let Some((anchor_id, anchor)) = anchor_id else {
            return Ok((empty(), timed_out));
        };
        if let Some(client) = client {
            // Read once and reuse for both `didOpen` and the freshness hash,
            // rather than re-reading via `ensure_open`.
            let text = self.read_file_text(&anchor.uri).await;
            if let Some(text) = &text {
                let _ = client.open_document(&anchor.uri, text).await;
            }
            match &text {
                Some(text) => {
                    let hash = content_hash(text);
                    let cached = self.db.get_callees_cache_hash(anchor_id).await?;
                    if cached.as_deref() != Some(hash.as_str()) {
                        // `materialize_call_edges` only upserts edges it
                        // discovers; a callee removed since the last
                        // materialization needs its stale edge invalidated
                        // first, or a re-materialization that finds nothing
                        // would leave it looking valid forever.
                        self.db.invalidate_outgoing_calls(anchor_id).await?;
                        timed_out |= self
                            .materialize_call_edges(anchor_id, &anchor, Direction::Outgoing, client)
                            .await?;
                        self.db.set_callees_cache_hash(anchor_id, &hash).await?;
                    }
                }
                // No freshness key for an unreadable/external anchor file —
                // always materialize (mirrors `ensure_open`'s silent skip).
                None => {
                    self.db.invalidate_outgoing_calls(anchor_id).await?;
                    timed_out |= self
                        .materialize_call_edges(anchor_id, &anchor, Direction::Outgoing, client)
                        .await?;
                }
            }
        }
        let result = self
            .read_calls_page(anchor_id, Direction::Outgoing, filter, page)
            .await?;
        Ok((result, timed_out))
    }

    /// Shared callers/callees body: resolve the anchor, optionally materialize
    /// `calls` edges, read them back grouped by adjacent callable. The returned
    /// `bool` is `true` if any underlying LSP call (anchor resolution or call
    /// hierarchy) timed out.
    async fn call_graph<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        direction: Direction,
        filter: &Filter,
        page: &Page,
        client: Option<&C>,
    ) -> Result<(CallGraphResult, bool)> {
        let empty = || CallGraphResult {
            items: Vec::new(),
            next_cursor: None,
        };
        let (anchor_id, mut timed_out) = self.resolve_anchor_id(symref, client).await?;
        let Some((anchor_id, anchor)) = anchor_id else {
            return Ok((empty(), timed_out));
        };
        if let Some(client) = client {
            self.ensure_open(&anchor.uri, client).await;
            timed_out |= self
                .materialize_call_edges(anchor_id, &anchor, direction, client)
                .await?;
        }
        let result = self
            .read_calls_page(anchor_id, direction, filter, page)
            .await?;
        Ok((result, timed_out))
    }

    /// The `get_neighbors("calls", direction) → group → filter → paginate`
    /// tail shared by [`Self::call_graph`], [`Self::find_callees`]'s cache hit
    /// and miss paths, and `QueryRuntime`'s cache-first `find_callers`
    /// (`callers_from_cache`).
    async fn read_calls_page(
        &self,
        anchor_id: i64,
        direction: Direction,
        filter: &Filter,
        page: &Page,
    ) -> Result<CallGraphResult> {
        let neighbors = self.db.get_neighbors(anchor_id, "calls", direction).await?;
        let grouped = group_by_node(neighbors);
        let filtered: Vec<_> = grouped
            .into_iter()
            .filter(|(n, _)| filter.matches(n))
            .collect();
        let (page_items, next) = paginate(filtered, page, |(n, _)| SortKey::from_node(n));
        let items = page_items
            .into_iter()
            .map(|(node, sites)| CallGraphNode {
                node: NodeDto::from_node(&node),
                call_sites: sites.into_iter().map(|s| RangeDto::from(s.range)).collect(),
            })
            .collect();
        Ok(CallGraphResult {
            items,
            next_cursor: next.map(|c| c.encode()),
        })
    }

    /// Cache-only read of `symref`'s callers (incoming `calls`), for
    /// `QueryRuntime`'s warm path (`docs/design/lsp-integration.md`
    /// "cache-first + background refresh") — no LSP call, however stale.
    pub async fn callers_from_cache(
        &self,
        anchor_id: i64,
        filter: &Filter,
        page: &Page,
    ) -> Result<CallGraphResult> {
        self.read_calls_page(anchor_id, Direction::Incoming, filter, page)
            .await
    }

    /// Cache-only read of `symref`'s references, mirroring
    /// [`Self::callers_from_cache`].
    pub async fn references_from_cache(
        &self,
        anchor_id: i64,
        filter: &Filter,
        page: &Page,
    ) -> Result<FindReferencesResult> {
        let neighbors = self
            .db
            .get_neighbors(anchor_id, "references", Direction::Incoming)
            .await?;
        let grouped = group_by_node(neighbors);
        let filtered: Vec<_> = grouped
            .into_iter()
            .filter(|(n, _)| filter.matches(n))
            .collect();
        let (page_items, next) = paginate(filtered, page, |(n, _)| SortKey::from_node(n));
        let references = page_items
            .into_iter()
            .map(|(node, sites)| ReferenceGroup {
                node: NodeDto::from_node(&node),
                sites: sites.into_iter().map(|s| RangeDto::from(s.range)).collect(),
            })
            .collect();
        Ok(FindReferencesResult {
            references,
            next_cursor: next.map(|c| c.encode()),
        })
    }

    /// Thin wrapper over [`Self::resolve_anchor_id`], exposed so
    /// `QueryRuntime` can resolve the anchor once and then independently
    /// decide "read cache now" vs. "materialize in background"
    /// (`docs/design/lsp-integration.md` "cache-first + background refresh").
    pub async fn resolve_anchor<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        client: Option<&C>,
    ) -> Result<(Option<(i64, crate::graph::Node)>, bool)> {
        self.resolve_anchor_id(symref, client).await
    }

    /// Resolve `symref` to `(id, node)`. `Fqn` ⇒ direct lookup; `At` ⇒
    /// definition-first when a client is present, else the indexed node at the
    /// position. `None` when no anchor is resolvable; the `bool` is `true` when
    /// the `definition` call itself timed out.
    async fn resolve_anchor_id<C: LspQueryClient>(
        &self,
        symref: &SymbolRef,
        client: Option<&C>,
    ) -> Result<(Option<(i64, crate::graph::Node)>, bool)> {
        let (anchor, timed_out) = match symref {
            SymbolRef::Fqn(fqn) => (self.db.get_node_by_fqn(fqn).await?, false),
            SymbolRef::At {
                uri,
                line,
                character,
            } => match client {
                Some(client) => {
                    self.definition_anchor(uri, *line, *character, client)
                        .await?
                }
                None => (
                    self.db
                        .find_node_by_position(uri, *line as i64, *character as i64)
                        .await?,
                    false,
                ),
            },
        };
        Ok((
            anchor.and_then(|node| node.id.map(|id| (id, node))),
            timed_out,
        ))
    }
}

/// Content fingerprint for the callees precise cache — a plain hash of the
/// anchor file's full text. Deliberately distinct from `signature_hash`
/// (`src/indexer/symbol.rs`), which hashes symbol structure, not file bytes,
/// and under-fires on a same-line-count body edit
/// (`docs/design/lsp-integration.md` "callees precise cache").
fn content_hash(text: &str) -> String {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    format!("{:016x}", h.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Range as GRange, Site};
    use crate::indexer::{LspPosition, LspRange};
    use crate::query::lsp_query::{
        CallHierarchyItem, Hover, IncomingCall, Location, MockLspQueryClient, OutgoingCall,
    };
    use crate::query::{Cursor, Filter};
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    const URI: &str = "file:///repo/mod.py";

    /// A function node spanning `[line, line+2)`, name identifier at col 4.
    fn func(fqn: &str, name: &str, line: i64) -> crate::graph::Node {
        crate::graph::Node {
            id: None,
            fqn: fqn.to_string(),
            uri: URI.to_string(),
            name: name.to_string(),
            language: "python".to_string(),
            kind: 12,
            node_kind: "Function".to_string(),
            construct: None,
            container_id: None,
            range: GRange {
                start_line: line,
                start_col: 0,
                end_line: line + 2,
                end_col: 0,
            },
            sel: GRange {
                start_line: line,
                start_col: 4,
                end_line: line,
                end_col: 4 + name.len() as i64,
            },
            signature: None,
            documentation: None,
            detail: None,
            signature_hash: None,
            valid: true,
            orphan: false,
            generation: 0,
            is_external: false,
        }
    }

    fn rng(sl: u32, sc: u32, el: u32, ec: u32) -> LspRange {
        LspRange {
            start: LspPosition {
                line: sl,
                character: sc,
            },
            end: LspPosition {
                line: el,
                character: ec,
            },
        }
    }

    /// Seed the caller→helper scenario: `helper` at line 0, `caller` at line 6
    /// which references/calls `helper` at line 7. Returns the engine + mock.
    async fn scenario() -> (QueryEngine, MockLspQueryClient) {
        let dir = tempdir().unwrap();
        let db = crate::graph::DbActor::spawn(&dir.path().join("g.db")).unwrap();
        // Persist the dir for the engine's lifetime via the leak of tempdir's
        // guard (the engine outlives the test, which is fine).
        std::mem::forget(dir);
        let engine = QueryEngine::new(db, "file:///repo".to_string());

        // helper: range [0,2), sel (0,4)-(0,10).
        let helper = func("repo.helper", "helper", 0);
        // caller: range [6,8), sel (6,4)-(6,10).
        let caller = func("repo.caller", "caller", 6);
        engine.db().upsert_node(helper.clone()).await.unwrap();
        engine.db().upsert_node(caller.clone()).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        // find_definition(usage at (7,4)) → helper's identifier span.
        mock.definitions.insert(
            (URI.to_string(), 7, 4),
            vec![Location {
                uri: URI.to_string(),
                range: rng(0, 4, 0, 10),
            }],
        );
        // find_references(helper @ (0,4)) → one usage inside caller at (7,4).
        mock.references.insert(
            (URI.to_string(), 0, 4),
            vec![Location {
                uri: URI.to_string(),
                range: rng(7, 4, 7, 10),
            }],
        );
        // call hierarchy for helper/caller, with caller↔helper at (7,4).
        let helper_item = CallHierarchyItem {
            name: "helper".into(),
            kind: 12,
            uri: URI.into(),
            range: rng(0, 0, 2, 0),
            selection_range: rng(0, 4, 0, 10),
            raw: json!({ "uri": URI, "name": "helper", "data": "h" }),
        };
        let caller_item = CallHierarchyItem {
            name: "caller".into(),
            kind: 12,
            uri: URI.into(),
            range: rng(6, 0, 8, 0),
            selection_range: rng(6, 4, 6, 10),
            raw: json!({ "uri": URI, "name": "caller", "data": "c" }),
        };
        mock.prepare
            .insert((URI.to_string(), 0, 4), vec![helper_item.clone()]);
        mock.prepare
            .insert((URI.to_string(), 6, 4), vec![caller_item.clone()]);
        mock.incoming.insert(
            helper_item.key(),
            vec![IncomingCall {
                from: caller_item.clone(),
                from_ranges: vec![rng(7, 4, 7, 10)],
            }],
        );
        mock.outgoing.insert(
            caller_item.key(),
            vec![OutgoingCall {
                to: helper_item.clone(),
                from_ranges: vec![rng(7, 4, 7, 10)],
            }],
        );
        (engine, mock)
    }

    #[tokio::test]
    async fn find_symbol_segments_and_filters() {
        let (engine, _mock) = scenario().await;
        // Two nodes seeded: repo.caller, repo.helper.
        let res = engine
            .find_symbol(
                "helper",
                MatchMode::Segment,
                false,
                false,
                &Filter::default(),
                &Page::default(),
            )
            .await
            .unwrap();
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(res.nodes[0].fqn, "repo.helper");
        assert!(res.next_cursor.is_none());
    }

    #[tokio::test]
    async fn find_symbol_brief_returns_fqns_not_nodes() {
        let (engine, _mock) = scenario().await;
        let res = engine
            .find_symbol(
                "",
                MatchMode::Contains,
                false,
                true,
                &Filter::default(),
                &Page::default(),
            )
            .await
            .unwrap();
        assert!(res.nodes.is_empty());
        assert_eq!(
            res.fqns,
            vec!["repo.caller".to_string(), "repo.helper".to_string()]
        );
    }

    #[tokio::test]
    async fn find_symbol_paginates_with_cursor() {
        let (engine, _mock) = scenario().await;
        let page = Page {
            limit: 1,
            cursor: None,
        };
        let first = engine
            .find_symbol(
                "",
                MatchMode::Contains,
                false,
                false,
                &Filter::default(),
                &page,
            )
            .await
            .unwrap();
        assert_eq!(first.nodes.len(), 1);
        assert_eq!(first.nodes[0].fqn, "repo.caller"); // sort order: caller < helper
        let cursor = first.next_cursor.expect("more remain");

        let second = engine
            .find_symbol(
                "",
                MatchMode::Contains,
                false,
                false,
                &Filter::default(),
                &Page {
                    limit: 1,
                    cursor: Some(Cursor::decode(&cursor).unwrap()),
                },
            )
            .await
            .unwrap();
        assert_eq!(second.nodes.len(), 1);
        assert_eq!(second.nodes[0].fqn, "repo.helper");
        assert!(second.next_cursor.is_none());
    }

    #[tokio::test]
    async fn read_range_slices_file_and_clamps() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mod.py");
        fs::write(&path, "a\nb\nc\nd\ne\n").unwrap();
        let uri = format!("file://{}", path.display());
        let root_uri = format!("file://{}", dir.path().display());
        let engine = QueryEngine::new(
            crate::graph::DbActor::spawn(&dir.path().join("g.db")).unwrap(),
            root_uri,
        );

        let res = engine.read_range(&uri, 1, 3).await.unwrap();
        assert_eq!(res.content, "b\nc");
        assert_eq!(res.range.start.line, 1);
        assert_eq!(res.range.end.line, 3);
        assert_eq!(res.total_lines, 5);

        // start beyond EOF → empty, clamped.
        let over = engine.read_range(&uri, 100, 110).await.unwrap();
        assert_eq!(over.content, "");
    }

    #[tokio::test]
    async fn find_definition_fqn_returns_node() {
        let (engine, _mock) = scenario().await;
        let (res, timed_out) = engine
            .find_definition(
                &SymbolRef::Fqn("repo.helper".into()),
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(res.nodes[0].fqn, "repo.helper");
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_definition_at_uses_lsp_then_resolves_target() {
        let (engine, mock) = scenario().await;
        // Cursor on the usage inside caller at (7,4).
        let (res, timed_out) = engine
            .find_definition(
                &SymbolRef::At {
                    uri: URI.into(),
                    line: 7,
                    character: 4,
                },
                Some(&mock),
            )
            .await
            .unwrap();
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(res.nodes[0].fqn, "repo.helper");
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_definition_at_promotes_construct_via_hover() {
        let (engine, mut mock) = scenario().await;
        // A tsserver-style type alias: SymbolKind=13 (Variable), no construct yet.
        let mut alias = func("repo.alias", "Alias", 20);
        alias.kind = 13;
        alias.node_kind = "Variable".to_string();
        engine.db().upsert_node(alias).await.unwrap();

        // Usage at (21,4) resolves via LSP to the alias's identifier span (20,4)-(20,9).
        mock.definitions.insert(
            (URI.to_string(), 21, 4),
            vec![Location {
                uri: URI.to_string(),
                range: rng(20, 4, 20, 9),
            }],
        );
        mock.hovers.insert(
            (URI.to_string(), 20, 4),
            Some(Hover {
                value: "```typescript\ntype Alias = string\n```".to_string(),
            }),
        );

        let (res, timed_out) = engine
            .find_definition(
                &SymbolRef::At {
                    uri: URI.into(),
                    line: 21,
                    character: 4,
                },
                Some(&mock),
            )
            .await
            .unwrap();
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(res.nodes[0].fqn, "repo.alias");
        assert_eq!(res.nodes[0].kind_label, "TypeAlias");
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_definition_at_without_client_is_empty() {
        let (engine, _mock) = scenario().await;
        let (res, timed_out) = engine
            .find_definition(
                &SymbolRef::At {
                    uri: URI.into(),
                    line: 7,
                    character: 4,
                },
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert!(res.nodes.is_empty());
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_references_materializes_and_groups() {
        let (engine, mock) = scenario().await;
        let (res, timed_out) = engine
            .find_references(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter::default(),
                &Page::default(),
                Some(&mock),
            )
            .await
            .unwrap();
        assert_eq!(res.references.len(), 1);
        let group = &res.references[0];
        assert_eq!(group.node.fqn, "repo.caller");
        assert_eq!(group.sites.len(), 1);
        assert_eq!(group.sites[0].start.line, 7);
        assert!(!timed_out);

        // Re-run cache-only: the materialized edge is served without the client.
        let (cached, cached_timed_out) = engine
            .find_references(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter::default(),
                &Page::default(),
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert_eq!(cached.references.len(), 1);
        assert_eq!(cached.references[0].node.fqn, "repo.caller");
        assert!(!cached_timed_out);
    }

    #[tokio::test]
    async fn find_references_cache_only_reads_preseeded_edge() {
        let (engine, _mock) = scenario().await;
        // Hand-place a references edge: caller → helper at (9,0).
        let caller = engine
            .db()
            .get_node_by_fqn("repo.caller")
            .await
            .unwrap()
            .unwrap();
        let helper = engine
            .db()
            .get_node_by_fqn("repo.helper")
            .await
            .unwrap()
            .unwrap();
        engine
            .db()
            .upsert_edge(Edge {
                id: None,
                src_id: caller.id.unwrap(),
                dst_id: helper.id.unwrap(),
                edge_type: "references".into(),
                site: Some(Site {
                    uri: URI.into(),
                    range: GRange {
                        start_line: 9,
                        start_col: 0,
                        end_line: 9,
                        end_col: 6,
                    },
                }),
                valid: true,
            })
            .await
            .unwrap();

        let (res, timed_out) = engine
            .find_references(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter::default(),
                &Page::default(),
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert_eq!(res.references.len(), 1);
        assert_eq!(res.references[0].sites[0].start.line, 9);
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_callers_materializes_incoming_calls() {
        let (engine, mock) = scenario().await;
        let (res, timed_out) = engine
            .find_callers(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter::default(),
                &Page::default(),
                Some(&mock),
            )
            .await
            .unwrap();
        assert_eq!(res.items.len(), 1);
        assert_eq!(res.items[0].node.fqn, "repo.caller");
        assert_eq!(res.items[0].call_sites.len(), 1);
        assert_eq!(res.items[0].call_sites[0].start.line, 7);
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_callers_falls_back_to_module_node_for_top_level_call_site() {
        // No `scenario()` here: it seeds only function nodes, but this test
        // needs a synthetic module node (`FlatSymbol::module_root`) to prove
        // the fix — a call site outside every def/class now resolves instead
        // of being silently dropped by `find_node_by_position` returning None.
        let dir = tempdir().unwrap();
        let db = crate::graph::DbActor::spawn(&dir.path().join("g.db")).unwrap();
        std::mem::forget(dir);
        let engine = QueryEngine::new(db, "file:///repo".to_string());

        let helper = func("repo.mod.helper", "helper", 0);
        engine.db().upsert_node(helper).await.unwrap();

        let module = crate::graph::Node {
            id: None,
            fqn: "repo.mod".to_string(),
            uri: URI.to_string(),
            name: "mod".to_string(),
            language: "python".to_string(),
            kind: 2,
            node_kind: "Module".to_string(),
            construct: None,
            container_id: None,
            range: GRange {
                start_line: 0,
                start_col: 0,
                end_line: i64::MAX,
                end_col: i64::MAX,
            },
            sel: GRange {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
            },
            signature: None,
            documentation: None,
            detail: None,
            signature_hash: None,
            valid: true,
            orphan: false,
            generation: 0,
            is_external: false,
        };
        engine.db().upsert_node(module).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        let helper_item = CallHierarchyItem {
            name: "helper".into(),
            kind: 12,
            uri: URI.into(),
            range: rng(0, 0, 2, 0),
            selection_range: rng(0, 4, 0, 10),
            raw: json!({ "uri": URI, "name": "helper", "data": "h" }),
        };
        mock.prepare
            .insert((URI.to_string(), 0, 4), vec![helper_item.clone()]);

        // Call site at bare module top level (line 20) — outside every
        // def/class, so only the synthetic module node covers it.
        let module_item = CallHierarchyItem {
            name: "mod".into(),
            kind: 2,
            uri: URI.into(),
            range: rng(0, 0, 100, 0),
            selection_range: rng(20, 0, 20, 6),
            raw: json!({ "uri": URI, "name": "mod", "data": "m" }),
        };
        mock.incoming.insert(
            helper_item.key(),
            vec![IncomingCall {
                from: module_item,
                from_ranges: vec![rng(20, 0, 20, 6)],
            }],
        );

        let (res, timed_out) = engine
            .find_callers(
                &SymbolRef::Fqn("repo.mod.helper".into()),
                &Filter::default(),
                &Page::default(),
                Some(&mock),
            )
            .await
            .unwrap();
        assert_eq!(res.items.len(), 1);
        assert_eq!(res.items[0].node.fqn, "repo.mod");
        assert_eq!(res.items[0].call_sites[0].start.line, 20);
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_callees_materializes_outgoing_calls() {
        let (engine, mock) = scenario().await;
        let (res, timed_out) = engine
            .find_callees(
                &SymbolRef::Fqn("repo.caller".into()),
                &Filter::default(),
                &Page::default(),
                Some(&mock),
            )
            .await
            .unwrap();
        assert_eq!(res.items.len(), 1);
        assert_eq!(res.items[0].node.fqn, "repo.helper");
        assert_eq!(res.items[0].call_sites[0].start.line, 7);
        assert!(!timed_out);
    }

    // --- §5.A: find_callees precise content-hash cache ---
    //
    // Unlike `scenario()` (whose files are never written to disk),
    // `find_callees`'s cache hashes the anchor's real file text, so these
    // tests need an actual on-disk file to edit between calls.

    fn node_at(uri: &str, fqn: &str, name: &str, line: i64) -> crate::graph::Node {
        let mut n = func(fqn, name, line);
        n.uri = uri.to_string();
        n
    }

    /// A real on-disk `helper`/`caller` file (`caller` calls `helper` on line
    /// 4), a matching engine, and a mock wired for one `prepareCallHierarchy`
    /// / `outgoingCalls` round trip. The backing tempdir is leaked so the file
    /// outlives the test.
    async fn callees_cache_scenario() -> (
        QueryEngine,
        MockLspQueryClient,
        SymbolRef,
        std::path::PathBuf,
    ) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mod.py");
        fs::write(
            &path,
            "def helper():\n    return 1\n\ndef caller():\n    return helper()\n",
        )
        .unwrap();
        let uri = format!("file://{}", path.display());
        let root_uri = format!("file://{}", dir.path().display());
        let db = crate::graph::DbActor::spawn(&dir.path().join("g.db")).unwrap();
        std::mem::forget(dir);
        let engine = QueryEngine::new(db, root_uri);

        engine
            .db()
            .upsert_node(node_at(&uri, "repo.helper", "helper", 0))
            .await
            .unwrap();
        engine
            .db()
            .upsert_node(node_at(&uri, "repo.caller", "caller", 3))
            .await
            .unwrap();

        let helper_item = CallHierarchyItem {
            name: "helper".into(),
            kind: 12,
            uri: uri.clone(),
            range: rng(0, 0, 2, 0),
            selection_range: rng(0, 4, 0, 10),
            raw: json!({ "uri": uri, "name": "helper", "data": "h" }),
        };
        let caller_item = CallHierarchyItem {
            name: "caller".into(),
            kind: 12,
            uri: uri.clone(),
            range: rng(3, 0, 5, 0),
            selection_range: rng(3, 4, 3, 10),
            raw: json!({ "uri": uri, "name": "caller", "data": "c" }),
        };
        let mut mock = MockLspQueryClient::new();
        mock.prepare
            .insert((uri.clone(), 3, 4), vec![caller_item.clone()]);
        mock.outgoing.insert(
            caller_item.key(),
            vec![OutgoingCall {
                to: helper_item,
                from_ranges: vec![rng(4, 11, 4, 17)],
            }],
        );

        (engine, mock, SymbolRef::Fqn("repo.caller".into()), path)
    }

    #[tokio::test]
    async fn find_callees_precise_cache_skips_lsp_when_file_unchanged() {
        let (engine, mock, symref, _path) = callees_cache_scenario().await;

        let (first, timed_out) = engine
            .find_callees(&symref, &Filter::default(), &Page::default(), Some(&mock))
            .await
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.items[0].node.fqn, "repo.helper");
        assert!(!timed_out);
        assert_eq!(mock.call_count("prepareCallHierarchy"), 1);

        let (second, timed_out) = engine
            .find_callees(&symref, &Filter::default(), &Page::default(), Some(&mock))
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(!timed_out);
        assert_eq!(
            mock.call_count("prepareCallHierarchy"),
            1,
            "unchanged file must be served from the cache, not re-hit the LSP"
        );
    }

    #[tokio::test]
    async fn find_callees_precise_cache_remateralizes_on_same_line_count_body_edit() {
        let (engine, mut mock, symref, path) = callees_cache_scenario().await;

        let (first, _) = engine
            .find_callees(&symref, &Filter::default(), &Page::default(), Some(&mock))
            .await
            .unwrap();
        assert_eq!(first.items.len(), 1);
        assert_eq!(mock.call_count("prepareCallHierarchy"), 1);

        // Same line count as the original (`return helper()` -> `return 0`):
        // `caller`'s own declaration span/kind/detail are untouched, so a
        // `signature_hash`-gated invalidation would miss this — the trap the
        // content-hash cache exists to close.
        fs::write(
            &path,
            "def helper():\n    return 1\n\ndef caller():\n    return 0\n",
        )
        .unwrap();
        mock.outgoing.clear();

        let (second, _) = engine
            .find_callees(&symref, &Filter::default(), &Page::default(), Some(&mock))
            .await
            .unwrap();
        assert!(
            second.items.is_empty(),
            "the callee was removed by the edit"
        );
        assert_eq!(
            mock.call_count("prepareCallHierarchy"),
            2,
            "changed file content must re-hit the LSP"
        );
    }

    #[tokio::test]
    async fn find_callees_zero_callees_is_served_from_cache_without_lsp() {
        let (engine, mut mock, symref, _path) = callees_cache_scenario().await;
        // No outgoing calls programmed for `caller` at all.
        mock.outgoing.clear();

        let (first, _) = engine
            .find_callees(&symref, &Filter::default(), &Page::default(), Some(&mock))
            .await
            .unwrap();
        assert!(first.items.is_empty());
        assert_eq!(mock.call_count("prepareCallHierarchy"), 1);

        let (second, _) = engine
            .find_callees(&symref, &Filter::default(), &Page::default(), Some(&mock))
            .await
            .unwrap();
        assert!(second.items.is_empty());
        assert_eq!(
            mock.call_count("prepareCallHierarchy"),
            1,
            "an authoritative zero-callees result must be cached, not re-fetched"
        );
    }

    #[tokio::test]
    async fn references_excludes_external_by_default_filter() {
        let (engine, _mock) = scenario().await;
        let caller = engine
            .db()
            .get_node_by_fqn("repo.caller")
            .await
            .unwrap()
            .unwrap();
        let helper = engine
            .db()
            .get_node_by_fqn("repo.helper")
            .await
            .unwrap()
            .unwrap();
        // Mark the caller external; default filter must drop it.
        let mut ext = caller.clone();
        ext.is_external = true;
        engine.db().upsert_node(ext).await.unwrap();
        engine
            .db()
            .upsert_edge(Edge {
                id: None,
                src_id: caller.id.unwrap(),
                dst_id: helper.id.unwrap(),
                edge_type: "references".into(),
                site: Some(Site {
                    uri: URI.into(),
                    range: GRange {
                        start_line: 1,
                        start_col: 0,
                        end_line: 1,
                        end_col: 2,
                    },
                }),
                valid: true,
            })
            .await
            .unwrap();

        let (hidden, _timed_out) = engine
            .find_references(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter::default(),
                &Page::default(),
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert!(hidden.references.is_empty());

        let (shown, _timed_out) = engine
            .find_references(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter {
                    include_external: true,
                    ..Default::default()
                },
                &Page::default(),
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert_eq!(shown.references.len(), 1);
    }

    #[tokio::test]
    async fn find_references_reports_timeout_from_materialize() {
        let (engine, mut mock) = scenario().await;
        mock.timeout_ops.insert("references");
        let (res, timed_out) = engine
            .find_references(
                &SymbolRef::Fqn("repo.helper".into()),
                &Filter::default(),
                &Page::default(),
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(timed_out);
        assert!(res.references.is_empty());
    }

    #[tokio::test]
    async fn find_callers_reports_timeout_from_anchor_resolution() {
        let (engine, mut mock) = scenario().await;
        // `find_callers(Fqn(...))` never calls `definition`, so drive the
        // timeout through the `At` anchor-resolution path instead.
        mock.timeout_ops.insert("definition");
        let (res, timed_out) = engine
            .find_callers(
                &SymbolRef::At {
                    uri: URI.into(),
                    line: 7,
                    character: 4,
                },
                &Filter::default(),
                &Page::default(),
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(timed_out);
        // Anchor resolution falls back to the indexed node at the cursor
        // position, which is inside `caller` — not `helper` — so no incoming
        // calls are materialized for it.
        assert!(res.items.is_empty());
    }
}
