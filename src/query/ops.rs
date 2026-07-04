//! The six query operations on [`QueryEngine`] (`docs/design/mcp-tools.md`):
//! `find_symbol`, `read_range`, `find_definition`, `find_references`,
//! `find_callers`, `find_callees`. Each is generic over an optional
//! [`LspQueryClient`]; with `None` they serve the materialized cache, with
//! `Some` they construct edges on demand first.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};

use anyhow::{Result, anyhow};

use crate::adapters::NodeKind;
use crate::graph::{Direction, Node};

use super::dto::{
    CallGraphNode, CallGraphResult, FindCallPathResult, FindDefinitionResult, FindReferencesResult,
    FindSymbolResult, NodeDto, Position, RangeDto, ReadRangeResult, ReferenceGroup,
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
                        if (node.construct.is_none() || node.signature.is_none())
                            && let Some(id) = node.id
                        {
                            // `node.uri` is the resolved *target* file, which
                            // may differ from `uri` (the usage site already
                            // opened above) for a cross-file definition —
                            // pyright/tsserver answer `hover` from live
                            // in-memory state, not a background scan, so an
                            // unopened target silently yields a null hover.
                            self.ensure_open(&node.uri, client).await;
                            if let Ok(Some(hover)) = client
                                .hover(
                                    &node.uri,
                                    node.sel.start_line as u32,
                                    node.sel.start_col as u32,
                                )
                                .await
                            {
                                if node.construct.is_none()
                                    && let Some(construct) =
                                        NodeKind::construct_from_hover(&hover.value)
                                {
                                    let _ = self.db.update_node_construct(id, &construct).await;
                                    node.construct = Some(construct);
                                }
                                let signature = hover.value.trim();
                                if node.signature.is_none() && !signature.is_empty() {
                                    let _ = self.db.update_node_signature(id, signature).await;
                                    node.signature = Some(signature.to_string());
                                }
                            }
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

    /// Multi-hop reachability: does `from` reach `to` through zero or more
    /// outgoing `calls` hops (`docs/design/mcp-tools.md` "find_call_path")?
    /// Breadth-first, so the first path found is shortest; each unvisited node
    /// is expanded via the same precise content-hash cache `find_callees` uses
    /// ([`Self::expand_outgoing_calls`]), spending one unit of `max_lsp_calls`
    /// only when a live materialization round trip is actually needed (a fresh
    /// cache hit is free). `max_depth` caps hops from `from`.
    ///
    /// The returned `bool` is `true` if any underlying LSP call timed out.
    /// `FindCallPathResult::limit_reached` is `true` whenever the search
    /// stopped — depth cap, budget, or a missing/unresolvable client — before
    /// it could prove `to` unreachable, so callers don't mistake "not found
    /// within these limits" for a proven negative.
    pub async fn find_call_path<C: LspQueryClient>(
        &self,
        from: &SymbolRef,
        to: &SymbolRef,
        max_depth: u32,
        max_lsp_calls: u32,
        client: Option<&C>,
    ) -> Result<(FindCallPathResult, bool)> {
        let not_found = |limit_reached: bool| FindCallPathResult {
            reachable: false,
            path: Vec::new(),
            limit_reached,
        };
        let (from_anchor, from_timed_out) = self.resolve_anchor_id(from, client).await?;
        let (to_anchor, to_timed_out) = self.resolve_anchor_id(to, client).await?;
        let mut timed_out = from_timed_out || to_timed_out;
        let (Some((from_id, from_node)), Some((to_id, _))) = (from_anchor, to_anchor) else {
            // An unresolvable endpoint isn't a search limit — there's nothing
            // to search from/for.
            return Ok((not_found(false), timed_out));
        };
        if from_id == to_id {
            return Ok((
                FindCallPathResult {
                    reachable: true,
                    path: vec![NodeDto::from_node(&from_node)],
                    limit_reached: false,
                },
                timed_out,
            ));
        }

        let mut visited: HashSet<i64> = HashSet::from([from_id]);
        let mut queue: VecDeque<(i64, Node, Vec<Node>)> = VecDeque::new();
        queue.push_back((from_id, from_node.clone(), vec![from_node]));
        let mut remaining_lsp_calls = max_lsp_calls;
        let mut limit_reached = false;

        while let Some((node_id, node, path)) = queue.pop_front() {
            // `path.len()` already counts `node` itself, so this is the hop
            // count so far; expanding past `max_depth` would produce a path
            // longer than the caller asked for.
            if path.len() as u32 > max_depth {
                limit_reached = true;
                continue;
            }
            let (usable, expand_timed_out) = self
                .expand_outgoing_calls(node_id, &node, client, &mut remaining_lsp_calls)
                .await?;
            timed_out |= expand_timed_out;
            if !usable {
                limit_reached = true;
            }
            let neighbors = self
                .db
                .get_neighbors(node_id, "calls", Direction::Outgoing)
                .await?;
            for (callee, _site) in group_by_node(neighbors) {
                let Some(callee_id) = callee.id else {
                    continue;
                };
                if callee_id == to_id {
                    let mut found_path = path;
                    found_path.push(callee);
                    return Ok((
                        FindCallPathResult {
                            reachable: true,
                            path: found_path.iter().map(NodeDto::from_node).collect(),
                            limit_reached: false,
                        },
                        timed_out,
                    ));
                }
                if visited.insert(callee_id) {
                    let mut next_path = path.clone();
                    next_path.push(callee.clone());
                    queue.push_back((callee_id, callee, next_path));
                }
            }
        }

        Ok((not_found(limit_reached), timed_out))
    }

    /// Ensure `anchor`'s outgoing `calls` edges are fresh before
    /// [`Self::find_call_path`]'s BFS reads them, reusing `find_callees`'s
    /// precise content-hash cache: an unchanged file's cached edges are
    /// trusted for free, so only an actual cache miss spends one unit of
    /// `remaining_lsp_calls`. Returns `(usable, timed_out)` — `usable` is
    /// `false` when no client was supplied, or the file changed but the
    /// budget was already spent, in which case the node's cached outgoing
    /// edges may be stale or incomplete and the caller must not treat an
    /// empty read as proof of "no callees."
    async fn expand_outgoing_calls<C: LspQueryClient>(
        &self,
        anchor_id: i64,
        anchor: &Node,
        client: Option<&C>,
        remaining_lsp_calls: &mut u32,
    ) -> Result<(bool, bool)> {
        let Some(client) = client else {
            return Ok((false, false));
        };
        let text = self.read_file_text(&anchor.uri).await;
        if let Some(text) = &text {
            let _ = client.open_document(&anchor.uri, text).await;
        }
        let hash = text.as_deref().map(content_hash);
        if let Some(hash) = &hash {
            let cached = self.db.get_callees_cache_hash(anchor_id).await?;
            if cached.as_deref() == Some(hash.as_str()) {
                return Ok((true, false));
            }
        }
        if *remaining_lsp_calls == 0 {
            return Ok((false, false));
        }
        *remaining_lsp_calls -= 1;
        self.db.invalidate_outgoing_calls(anchor_id).await?;
        let timed_out = self
            .materialize_call_edges(anchor_id, anchor, Direction::Outgoing, client)
            .await?;
        if let Some(hash) = &hash {
            self.db.set_callees_cache_hash(anchor_id, hash).await?;
        }
        Ok((true, timed_out))
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
        assert_eq!(
            res.nodes[0].signature.as_deref(),
            Some("```typescript\ntype Alias = string\n```"),
            "the same hover round trip that refines construct also backfills signature"
        );
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_definition_at_persists_signature_to_the_db() {
        let (engine, mut mock) = scenario().await;
        mock.hovers.insert(
            (URI.to_string(), 0, 4),
            Some(Hover {
                value: "def helper() -> int".to_string(),
            }),
        );

        let (res, _) = engine
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
        assert_eq!(
            res.nodes[0].signature.as_deref(),
            Some("def helper() -> int")
        );

        // Persisted, not just attached to this response: a later cache-only
        // lookup (no client, no hover) sees it too.
        let stored = engine
            .db()
            .get_node_by_fqn("repo.helper")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.signature.as_deref(), Some("def helper() -> int"));
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

    // --- find_call_path ---
    //
    // A 4-node call chain `a -> b -> c -> d`, plus an isolated `z` with no
    // calls at all, keyed by each node's `sel` position (mirroring
    // `scenario()`'s single caller/helper pair). Unlike `callees_cache_scenario`,
    // no file is ever written to disk, so `expand_outgoing_calls` always
    // takes its "unreadable file, no freshness key" branch — every expansion
    // with budget remaining unconditionally spends one unit, which is exactly
    // what makes the budget-exhaustion tests below deterministic.

    fn chain_item(name: &str, line: u32) -> CallHierarchyItem {
        CallHierarchyItem {
            name: name.into(),
            kind: 12,
            uri: URI.into(),
            range: rng(line, 0, line + 2, 0),
            selection_range: rng(line, 4, line, 4 + name.len() as u32),
            raw: json!({ "uri": URI, "name": name, "data": name }),
        }
    }

    async fn chain_scenario() -> (QueryEngine, MockLspQueryClient) {
        let dir = tempdir().unwrap();
        let db = crate::graph::DbActor::spawn(&dir.path().join("g.db")).unwrap();
        std::mem::forget(dir);
        let engine = QueryEngine::new(db, "file:///repo".to_string());

        for (fqn, name, line) in [
            ("repo.a", "a", 0u32),
            ("repo.b", "b", 10),
            ("repo.c", "c", 20),
            ("repo.d", "d", 30),
            ("repo.z", "z", 40),
        ] {
            engine
                .db()
                .upsert_node(func(fqn, name, line as i64))
                .await
                .unwrap();
        }

        let item_a = chain_item("a", 0);
        let item_b = chain_item("b", 10);
        let item_c = chain_item("c", 20);
        let item_d = chain_item("d", 30);

        let mut mock = MockLspQueryClient::new();
        mock.prepare
            .insert((URI.to_string(), 0, 4), vec![item_a.clone()]);
        mock.prepare
            .insert((URI.to_string(), 10, 4), vec![item_b.clone()]);
        mock.prepare
            .insert((URI.to_string(), 20, 4), vec![item_c.clone()]);
        mock.prepare
            .insert((URI.to_string(), 30, 4), vec![item_d.clone()]);
        // `z` has no `prepare` entry, so `prepareCallHierarchy` for it yields
        // no items and `d` has no `outgoing` entry — both dead ends.

        mock.outgoing.insert(
            item_a.key(),
            vec![OutgoingCall {
                to: item_b.clone(),
                from_ranges: vec![rng(1, 4, 1, 5)],
            }],
        );
        mock.outgoing.insert(
            item_b.key(),
            vec![OutgoingCall {
                to: item_c.clone(),
                from_ranges: vec![rng(11, 4, 11, 5)],
            }],
        );
        mock.outgoing.insert(
            item_c.key(),
            vec![OutgoingCall {
                to: item_d.clone(),
                from_ranges: vec![rng(21, 4, 21, 5)],
            }],
        );

        (engine, mock)
    }

    #[tokio::test]
    async fn find_call_path_same_node_is_trivially_reachable_at_zero_cost() {
        let (engine, mock) = chain_scenario().await;
        let (res, timed_out) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.a".into()),
                10,
                10,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(res.reachable);
        assert_eq!(res.path.len(), 1);
        assert_eq!(res.path[0].fqn, "repo.a");
        assert!(!res.limit_reached);
        assert!(!timed_out);
        assert_eq!(
            mock.call_count("prepareCallHierarchy"),
            0,
            "from == to must short-circuit before touching the LSP"
        );
    }

    #[tokio::test]
    async fn find_call_path_finds_direct_one_hop_path() {
        let (engine, mock) = chain_scenario().await;
        let (res, timed_out) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.b".into()),
                10,
                10,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(res.reachable);
        assert_eq!(
            res.path.iter().map(|n| n.fqn.as_str()).collect::<Vec<_>>(),
            vec!["repo.a", "repo.b"]
        );
        assert!(!res.limit_reached);
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_call_path_finds_multi_hop_shortest_path() {
        let (engine, mock) = chain_scenario().await;
        let (res, _) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.d".into()),
                10,
                10,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(res.reachable);
        assert_eq!(
            res.path.iter().map(|n| n.fqn.as_str()).collect::<Vec<_>>(),
            vec!["repo.a", "repo.b", "repo.c", "repo.d"]
        );
        assert!(!res.limit_reached);
    }

    #[tokio::test]
    async fn find_call_path_unreachable_within_budget_is_an_exhaustive_negative() {
        let (engine, mock) = chain_scenario().await;
        // `z` is isolated — no edge in the chain ever reaches it — and the
        // depth/budget here are generous enough to fully expand every
        // reachable node, so this is a *proven* negative.
        let (res, _) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.z".into()),
                10,
                10,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(!res.reachable);
        assert!(
            !res.limit_reached,
            "the whole reachable set was exhausted within the given limits"
        );
    }

    #[tokio::test]
    async fn find_call_path_depth_cap_marks_search_inconclusive() {
        let (engine, mock) = chain_scenario().await;
        // `d` is 3 hops away; a 1-hop cap can't reach it, and must say so
        // rather than claiming a proven "not reachable".
        let (res, _) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.d".into()),
                1,
                10,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(!res.reachable);
        assert!(res.limit_reached);
    }

    #[tokio::test]
    async fn find_call_path_lsp_budget_exhausted_marks_search_inconclusive() {
        let (engine, mock) = chain_scenario().await;
        // Only enough budget to expand `a` itself; `b` is discovered but
        // never expanded, so the search cannot rule out `d`.
        let (res, _) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.d".into()),
                10,
                1,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(!res.reachable);
        assert!(res.limit_reached);
        assert_eq!(
            mock.call_count("prepareCallHierarchy"),
            1,
            "the budget must gate the LSP call itself, not just get ignored"
        );
    }

    #[tokio::test]
    async fn find_call_path_without_a_client_is_inconclusive_when_nothing_is_cached() {
        let (engine, _mock) = chain_scenario().await;
        let (res, timed_out) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.b".into()),
                10,
                10,
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert!(!res.reachable);
        assert!(
            res.limit_reached,
            "no client and nothing cached ⇒ the negative is not proven"
        );
        assert!(!timed_out);
    }

    #[tokio::test]
    async fn find_call_path_without_a_client_still_uses_precached_edges() {
        let (engine, _mock) = chain_scenario().await;
        let a = engine
            .db()
            .get_node_by_fqn("repo.a")
            .await
            .unwrap()
            .unwrap();
        let b = engine
            .db()
            .get_node_by_fqn("repo.b")
            .await
            .unwrap()
            .unwrap();
        engine
            .db()
            .upsert_edge(Edge {
                id: None,
                src_id: a.id.unwrap(),
                dst_id: b.id.unwrap(),
                edge_type: "calls".into(),
                site: Some(Site {
                    uri: URI.into(),
                    range: GRange {
                        start_line: 1,
                        start_col: 4,
                        end_line: 1,
                        end_col: 5,
                    },
                }),
                valid: true,
            })
            .await
            .unwrap();

        let (res, _) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.b".into()),
                10,
                10,
                None::<&MockLspQueryClient>,
            )
            .await
            .unwrap();
        assert!(res.reachable);
        assert_eq!(
            res.path.iter().map(|n| n.fqn.as_str()).collect::<Vec<_>>(),
            vec!["repo.a", "repo.b"]
        );
        assert!(
            !res.limit_reached,
            "finding the target makes the limit question moot"
        );
    }

    #[tokio::test]
    async fn find_call_path_unresolvable_endpoint_is_not_a_search_limit() {
        let (engine, mock) = chain_scenario().await;
        let (res, _) = engine
            .find_call_path(
                &SymbolRef::Fqn("repo.a".into()),
                &SymbolRef::Fqn("repo.nope".into()),
                10,
                10,
                Some(&mock),
            )
            .await
            .unwrap();
        assert!(!res.reachable);
        assert!(res.path.is_empty());
        assert!(
            !res.limit_reached,
            "an unresolvable endpoint means there's nothing to search, not that a search was cut short"
        );
    }
}
