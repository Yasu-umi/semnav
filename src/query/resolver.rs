//! On-demand edge construction + shared query helpers.
//!
//! The `find_*` tools read existing edges from the graph first; only when a
//! [`LspQueryClient`] is supplied do they ask the language server and
//! *materialize* the discovered edges (UPSERT) so the next call is a pure graph
//! read (`docs/design/lsp-integration.md` "on-demand edge construction"). A
//! server-side error degrades gracefully (empty result ⇒ serve the cache);
//! database errors still propagate.
//!
//! 0.0.1 policy:
//! - `find_definition` resolves each `textDocument/definition` target to an
//!   indexed node; external/unindexed targets are dropped (synthetic external
//!   nodes are deferred to 0.1+).
//! - `find_references` / `find_callers` / `find_callees` resolve each
//!   occurrence's container via `find_node_by_position`; only indexed containers
//!   receive an edge, so every result is backed by a real declaration node.

use std::cmp::Ordering;

use anyhow::Result;

use crate::graph::{Direction, Edge, Neighbor, Node, Range, Site};
use crate::indexer::LspRange;

use super::QueryEngine;
use super::lsp_query::{LspQueryClient, is_timeout};

impl QueryEngine {
    /// Best-effort `didOpen` so a fresh query-time server has `uri` open before a
    /// positional request. External / unreadable files are silently skipped.
    pub(super) async fn ensure_open<C: LspQueryClient>(&self, uri: &str, client: &C) {
        if let Some(text) = self.read_file_text(uri).await {
            let _ = client.open_document(uri, &text).await;
        }
    }

    /// Read `uri`'s current text, confined to the workspace root. `None` for
    /// an external/unreadable file (mirrors `ensure_open`'s silent skip).
    pub(super) async fn read_file_text(&self, uri: &str) -> Option<String> {
        let path = self.confine(uri).ok()?;
        tokio::fs::read_to_string(&path).await.ok()
    }

    /// Resolve a `SymbolRef::At` to the anchor declaration node. With a client we
    /// go definition-first (so a cursor on a *usage* still pins the declaration);
    /// falling back to the indexed node at the position if the server yields no
    /// resolvable target. The returned `bool` is `true` when the `definition`
    /// call itself timed out (as opposed to genuinely returning nothing).
    pub(super) async fn definition_anchor<C: LspQueryClient>(
        &self,
        uri: &str,
        line: u32,
        character: u32,
        client: &C,
    ) -> Result<(Option<Node>, bool)> {
        let (locs, timed_out) = match client.definition(uri, line, character).await {
            Ok(locs) => (locs, false),
            Err(e) => (Vec::new(), is_timeout(&e)),
        };
        for loc in locs {
            if let Some(node) = self
                .db
                .find_node_by_position(
                    &loc.uri,
                    loc.range.start.line as i64,
                    loc.range.start.character as i64,
                )
                .await?
            {
                return Ok((Some(node), timed_out));
            }
        }
        let node = self
            .db
            .find_node_by_position(uri, line as i64, character as i64)
            .await?;
        Ok((node, timed_out))
    }

    /// Materialize `references` edges into the anchor (incoming). Each LSP
    /// reference site is attached to its indexed container node. Returns `true`
    /// if the `references` call itself timed out.
    pub(super) async fn materialize_references<C: LspQueryClient>(
        &self,
        anchor_id: i64,
        anchor: &Node,
        client: &C,
    ) -> Result<bool> {
        let (locs, timed_out) = match client
            .references(
                &anchor.uri,
                anchor.sel.start_line as u32,
                anchor.sel.start_col as u32,
                true,
            )
            .await
        {
            Ok(locs) => (locs, false),
            Err(e) => (Vec::new(), is_timeout(&e)),
        };
        for loc in locs {
            let Some(container) = self
                .db
                .find_node_by_position(
                    &loc.uri,
                    loc.range.start.line as i64,
                    loc.range.start.character as i64,
                )
                .await?
            else {
                continue;
            };
            let Some(container_id) = container.id else {
                continue;
            };
            let site = Site {
                uri: loc.uri.clone(),
                range: lsp_range_to_range(&loc.range),
            };
            let _ = self
                .db
                .upsert_edge(located_edge(
                    container_id,
                    anchor_id,
                    "references",
                    Some(site),
                ))
                .await?;
        }
        Ok(timed_out)
    }

    /// Materialize `calls` edges for the anchor in `direction` via call hierarchy.
    /// `from_ranges` are the call-site spans; their `uri` is the caller's document
    /// (`from.uri` for incoming, the anchor for outgoing). Returns `true` if
    /// `prepareCallHierarchy` or the direction's call-edges request timed out.
    pub(super) async fn materialize_call_edges<C: LspQueryClient>(
        &self,
        anchor_id: i64,
        anchor: &Node,
        direction: Direction,
        client: &C,
    ) -> Result<bool> {
        let (items, mut timed_out) = match client
            .prepare_call_hierarchy(
                &anchor.uri,
                anchor.sel.start_line as u32,
                anchor.sel.start_col as u32,
            )
            .await
        {
            Ok(items) => (items, false),
            Err(e) => (Vec::new(), is_timeout(&e)),
        };
        let Some(item) = items.into_iter().next() else {
            return Ok(timed_out);
        };
        match direction {
            Direction::Incoming => {
                let calls = match client.incoming_calls(&item).await {
                    Ok(calls) => calls,
                    Err(e) => {
                        timed_out |= is_timeout(&e);
                        Vec::new()
                    }
                };
                for call in calls {
                    let Some(caller) = self
                        .db
                        .find_node_by_position(
                            &call.from.uri,
                            call.from.selection_range.start.line as i64,
                            call.from.selection_range.start.character as i64,
                        )
                        .await?
                    else {
                        continue;
                    };
                    let Some(caller_id) = caller.id else {
                        continue;
                    };
                    for range in call.from_ranges {
                        let site = Site {
                            uri: call.from.uri.clone(),
                            range: lsp_range_to_range(&range),
                        };
                        let _ = self
                            .db
                            .upsert_edge(located_edge(caller_id, anchor_id, "calls", Some(site)))
                            .await?;
                    }
                }
            }
            Direction::Outgoing => {
                let calls = match client.outgoing_calls(&item).await {
                    Ok(calls) => calls,
                    Err(e) => {
                        timed_out |= is_timeout(&e);
                        Vec::new()
                    }
                };
                for call in calls {
                    let Some(callee) = self
                        .db
                        .find_node_by_position(
                            &call.to.uri,
                            call.to.selection_range.start.line as i64,
                            call.to.selection_range.start.character as i64,
                        )
                        .await?
                    else {
                        continue;
                    };
                    let Some(callee_id) = callee.id else {
                        continue;
                    };
                    for range in call.from_ranges {
                        let site = Site {
                            uri: anchor.uri.clone(),
                            range: lsp_range_to_range(&range),
                        };
                        let _ = self
                            .db
                            .upsert_edge(located_edge(anchor_id, callee_id, "calls", Some(site)))
                            .await?;
                    }
                }
            }
        }
        Ok(timed_out)
    }
}

/// Build a `valid` edge of `edge_type` with an optional occurrence site.
fn located_edge(src: i64, dst: i64, edge_type: &str, site: Option<Site>) -> Edge {
    Edge {
        id: None,
        src_id: src,
        dst_id: dst,
        edge_type: edge_type.to_string(),
        site,
        valid: true,
    }
}

/// `LspRange` (u32) → graph `Range` (i64).
fn lsp_range_to_range(r: &LspRange) -> Range {
    Range {
        start_line: r.start.line as i64,
        start_col: r.start.character as i64,
        end_line: r.end.line as i64,
        end_col: r.end.character as i64,
    }
}

/// Total order over nodes matching the SQL `ORDER BY` sort key.
fn node_cmp(a: &Node, b: &Node) -> Ordering {
    (
        &a.fqn as &str,
        &a.uri as &str,
        a.range.start_line,
        a.range.start_col,
    )
        .cmp(&(
            &b.fqn as &str,
            &b.uri as &str,
            b.range.start_line,
            b.range.start_col,
        ))
}

/// Total order over occurrence sites by `(uri, line, col)`; `None` sorts first.
fn site_cmp(a: &Option<Site>, b: &Option<Site>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => (&x.uri as &str, x.range.start_line, x.range.start_col).cmp(&(
            &y.uri as &str,
            y.range.start_line,
            y.range.start_col,
        )),
    }
}

/// Group neighbor rows by their node, collecting each node's occurrence sites.
/// Rows are sorted by `(node key, site key)` first, so a node's rows are
/// contiguous and its sites are deterministically ordered. Returns one entry per
/// distinct node, in sort-key order.
pub(super) fn group_by_node(mut neighbors: Vec<Neighbor>) -> Vec<(Node, Vec<Site>)> {
    neighbors.sort_by(|a, b| node_cmp(&a.0, &b.0).then(site_cmp(&a.1, &b.1)));
    let mut out: Vec<(Node, Vec<Site>)> = Vec::new();
    for (node, site) in neighbors {
        if let Some(last) = out.last_mut()
            && node_cmp(&last.0, &node) == Ordering::Equal
        {
            if let Some(s) = site {
                last.1.push(s);
            }
            continue;
        }
        out.push((node, site.into_iter().collect()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Range;
    use crate::query::lsp_query::MockLspQueryClient;
    use tempfile::tempdir;

    fn node(fqn: &str, line: i64) -> Node {
        Node {
            id: Some(fqn.len() as i64),
            fqn: fqn.to_string(),
            uri: "file:///m.py".to_string(),
            name: fqn.to_string(),
            language: "python".to_string(),
            kind: 12,
            node_kind: "Function".to_string(),
            construct: None,
            container_id: None,
            range: Range {
                start_line: line,
                start_col: 0,
                end_line: line + 1,
                end_col: 0,
            },
            sel: Range {
                start_line: line,
                start_col: 0,
                end_line: line,
                end_col: 4,
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

    fn site(line: i64) -> Site {
        Site {
            uri: "file:///m.py".to_string(),
            range: Range {
                start_line: line,
                start_col: 0,
                end_line: line,
                end_col: 2,
            },
        }
    }

    /// A helper for a QueryEngine backed by an isolated on-disk graph db, for
    /// tests that need to exercise the LSP-materialization paths rather than
    /// just `group_by_node`'s pure sort.
    fn engine() -> QueryEngine {
        let dir = tempdir().unwrap();
        let db = crate::graph::DbActor::spawn(&dir.path().join("g.db")).unwrap();
        std::mem::forget(dir);
        QueryEngine::new(db, "file:///repo".to_string())
    }

    #[tokio::test]
    async fn definition_anchor_falls_back_to_position_and_reports_timeout() {
        let engine = engine();
        engine.db().upsert_node(node("repo.helper", 0)).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        mock.timeout_ops.insert("definition");

        let (anchor, timed_out) = engine
            .definition_anchor("file:///m.py", 0, 0, &mock)
            .await
            .unwrap();
        assert!(timed_out, "definition timeout must be reported");
        // Falls back to the indexed node at the cursor position itself.
        assert_eq!(anchor.unwrap().fqn, "repo.helper");
    }

    #[tokio::test]
    async fn materialize_references_reports_timeout_without_edges() {
        let engine = engine();
        let helper = node("repo.helper", 0);
        let anchor_id = engine.db().upsert_node(helper.clone()).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        mock.timeout_ops.insert("references");

        let timed_out = engine
            .materialize_references(anchor_id, &helper, &mock)
            .await
            .unwrap();
        assert!(timed_out, "references timeout must be reported");
        let neighbors = engine
            .db()
            .get_neighbors(anchor_id, "references", Direction::Incoming)
            .await
            .unwrap();
        assert!(neighbors.is_empty(), "no edge materialized on timeout");
    }

    #[tokio::test]
    async fn materialize_call_edges_reports_timeout_when_prepare_times_out() {
        let engine = engine();
        let helper = node("repo.helper", 0);
        let anchor_id = engine.db().upsert_node(helper.clone()).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        mock.timeout_ops.insert("prepareCallHierarchy");

        let timed_out = engine
            .materialize_call_edges(anchor_id, &helper, Direction::Incoming, &mock)
            .await
            .unwrap();
        assert!(timed_out, "prepareCallHierarchy timeout must be reported");
    }

    #[test]
    fn group_by_node_collapses_repeats_and_sorts_sites() {
        // Unsorted input with two sites on the same node interleaved with another.
        let neighbors = vec![
            (node("a", 1), Some(site(10))),
            (node("b", 5), Some(site(3))),
            (node("a", 1), Some(site(2))),
            (node("a", 1), None),
        ];
        let grouped = group_by_node(neighbors);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0.fqn, "a");
        // Sites sorted ascending; the None occurrence contributes nothing.
        assert_eq!(grouped[0].1.len(), 2);
        assert_eq!(grouped[0].1[0].range.start_line, 2);
        assert_eq!(grouped[0].1[1].range.start_line, 10);
        assert_eq!(grouped[1].0.fqn, "b");
    }
}
