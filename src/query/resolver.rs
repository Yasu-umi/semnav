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
use crate::indexer::{LspRange, module_path_from_uri};

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
                    // A module-scope call site (outside every def/class) has
                    // no real identifier to anchor a `selectionRange` to, so
                    // both pyright and typescript-language-server report it
                    // as a synthetic pseudo-item whose `selectionRange` is
                    // the zero-width sentinel `{0,0}-{0,0}` (observed live;
                    // `docs/design/lsp-integration.md` callHierarchy note) —
                    // not a real occurrence of code at that exact point. A
                    // plain position lookup for `(0,0)` would tie-break
                    // toward whichever *real* symbol happens to start there
                    // (e.g. the file's first top-level `def`/`class`) via
                    // `find_node_by_position`'s innermost-span rule, instead
                    // of the synthesized module node — so the sentinel is
                    // resolved directly against the module's own root
                    // instead of by position.
                    let caller = if call.from.selection_range.start.line == 0
                        && call.from.selection_range.start.character == 0
                        && call.from.selection_range.end.line == 0
                        && call.from.selection_range.end.character == 0
                    {
                        let module_fqn = module_path_from_uri(&call.from.uri, self.root_uri());
                        self.db.get_node_by_fqn(&module_fqn).await?
                    } else {
                        self.db
                            .find_node_by_position(
                                &call.from.uri,
                                call.from.selection_range.start.line as i64,
                                call.from.selection_range.start.character as i64,
                            )
                            .await?
                    };
                    let Some(caller) = caller else {
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
                    let (target_ids, correction_timed_out) = self
                        .resolve_outgoing_callee(anchor, &callee, callee_id, client)
                        .await?;
                    timed_out |= correction_timed_out;
                    for target_id in target_ids {
                        for range in &call.from_ranges {
                            let site = Site {
                                uri: anchor.uri.clone(),
                                range: lsp_range_to_range(range),
                            };
                            let _ = self
                                .db
                                .upsert_edge(located_edge(
                                    anchor_id,
                                    target_id,
                                    "calls",
                                    Some(site),
                                ))
                                .await?;
                        }
                    }
                }
            }
        }
        Ok(timed_out)
    }

    /// Resolve an `outgoingCalls` `to` item's *actual* target callee id(s).
    ///
    /// tsserver's and gopls's `outgoingCalls.to` both sometimes point to an
    /// interface's method declaration rather than the concrete type actually
    /// invoked through it (`docs/design/lsp-integration.md` callHierarchy
    /// note, `docs/design/mcp-tools.md` find_callees) — gopls does this for
    /// any call through an interface-typed parameter, same as tsserver's
    /// interface-typed dispatch. pyright can't even answer `implementation`
    /// (`-32601 Unhandled method`, pinned in `src/lsp/client.rs`), so the
    /// correction is gated to TypeScript and Go — language, not capability,
    /// since a Python container never has `node_kind == "Interface"` anyway.
    ///
    /// Falls back to `callee_id` unchanged whenever the correction doesn't
    /// apply, times out, or resolves to nothing — never worse than the
    /// uncorrected edge this replaces.
    async fn resolve_outgoing_callee<C: LspQueryClient>(
        &self,
        anchor: &Node,
        callee: &Node,
        callee_id: i64,
        client: &C,
    ) -> Result<(Vec<i64>, bool)> {
        if !matches!(anchor.language.as_str(), "typescript" | "go") {
            return Ok((vec![callee_id], false));
        }
        let is_interface = match callee.container_id {
            Some(container_id) => self
                .db
                .get_node(container_id)
                .await?
                .is_some_and(|c| c.node_kind == "Interface"),
            None => false,
        };
        if !is_interface {
            return Ok((vec![callee_id], false));
        }

        // `callee` lives in whatever file declared the interface method —
        // not necessarily `anchor.uri` — and a query-time client only has
        // the anchor's file open (`ensure_open`, in the caller above this
        // one). tsserver answers `implementation` with an empty result for
        // an unopened document, so it must be opened here too.
        self.ensure_open(&callee.uri, client).await;
        let (locs, timed_out) = match client
            .implementation(
                &callee.uri,
                callee.sel.start_line as u32,
                callee.sel.start_col as u32,
            )
            .await
        {
            Ok(locs) => (locs, false),
            Err(e) => (Vec::new(), is_timeout(&e)),
        };

        let mut concrete_ids = Vec::new();
        for loc in &locs {
            let Some(concrete) = self
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
            let Some(concrete_id) = concrete.id else {
                continue;
            };
            let _ = self
                .db
                .upsert_edge(located_edge(callee_id, concrete_id, "implements", None))
                .await?;
            concrete_ids.push(concrete_id);
        }

        if concrete_ids.is_empty() {
            return Ok((vec![callee_id], timed_out));
        }
        Ok((concrete_ids, timed_out))
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
    use crate::query::lsp_query::{CallHierarchyItem, Location, MockLspQueryClient, OutgoingCall};
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
        engine
            .db()
            .upsert_node(node("repo.helper", 0))
            .await
            .unwrap();

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

    // --- outgoing-call interface-to-implementation correction ---

    fn rng(sl: u32, sc: u32, el: u32, ec: u32) -> LspRange {
        LspRange {
            start: crate::indexer::LspPosition {
                line: sl,
                character: sc,
            },
            end: crate::indexer::LspPosition {
                line: el,
                character: ec,
            },
        }
    }

    fn ts_node(
        fqn: &str,
        name: &str,
        node_kind: &str,
        line: i64,
        container_id: Option<i64>,
    ) -> Node {
        let mut n = node(fqn, line);
        n.name = name.to_string();
        n.node_kind = node_kind.to_string();
        n.language = "typescript".to_string();
        n.container_id = container_id;
        n.uri = "file:///m.ts".to_string();
        n
    }

    /// A TS `Greeter` interface (with a `greet` method) and an
    /// `EnglishGreeter` class implementing it (with its own `greet`), plus a
    /// `caller` function whose `outgoingCalls` mock response points `to` the
    /// interface's method — the exact shape observed live
    /// (`tests/fixtures/lsp-probe/captures/typescript_outgoing_calls_to_interface_method.json`).
    /// Returns `(engine, mock, anchor, anchor_id, interface_method_id, concrete_method_id)`.
    async fn interface_dispatch_scenario() -> (QueryEngine, MockLspQueryClient, Node, i64, i64, i64)
    {
        let engine = engine();
        let caller = ts_node("repo.caller", "caller", "Function", 10, None);
        let anchor_id = engine.db().upsert_node(caller.clone()).await.unwrap();

        let mut interface = ts_node("repo.Greeter", "Greeter", "Interface", 0, None);
        // Span wider than the default 1-line `node()` range so it strictly
        // *contains* (not ties with) `interface_method`'s range below —
        // `find_node_by_position`'s innermost-span tie-break needs a real
        // size difference to pick the nested method over its container.
        interface.range.end_line = 3;
        let interface_id = engine.db().upsert_node(interface).await.unwrap();
        let interface_method = ts_node(
            "repo.Greeter.greet",
            "greet",
            "Method",
            1,
            Some(interface_id),
        );
        let interface_method_id = engine.db().upsert_node(interface_method).await.unwrap();

        let mut concrete_class = ts_node("repo.EnglishGreeter", "EnglishGreeter", "Class", 4, None);
        // Same reasoning as `interface` above — must strictly contain
        // `concrete_method`'s range, not tie with it.
        concrete_class.range.end_line = 7;
        let concrete_class_id = engine.db().upsert_node(concrete_class).await.unwrap();
        let concrete_method = ts_node(
            "repo.EnglishGreeter.greet",
            "greet",
            "Method",
            5,
            Some(concrete_class_id),
        );
        let concrete_method_id = engine.db().upsert_node(concrete_method).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        let caller_item = CallHierarchyItem {
            name: "caller".into(),
            kind: 12,
            uri: "file:///m.ts".into(),
            range: rng(10, 0, 12, 0),
            selection_range: rng(10, 0, 10, 4),
            raw: serde_json::json!({ "uri": "file:///m.ts", "name": "caller" }),
        };
        mock.prepare.insert(
            ("file:///m.ts".to_string(), 10, 0),
            vec![caller_item.clone()],
        );
        let interface_method_item = CallHierarchyItem {
            name: "greet".into(),
            kind: 6,
            uri: "file:///m.ts".into(),
            range: rng(1, 0, 1, 5),
            selection_range: rng(1, 0, 1, 4),
            raw: serde_json::json!({ "uri": "file:///m.ts", "name": "greet" }),
        };
        mock.outgoing.insert(
            caller_item.key(),
            vec![OutgoingCall {
                to: interface_method_item,
                from_ranges: vec![rng(11, 4, 11, 9)],
            }],
        );
        mock.implementations.insert(
            ("file:///m.ts".to_string(), 1, 0),
            vec![Location {
                uri: "file:///m.ts".to_string(),
                range: rng(5, 0, 5, 4),
            }],
        );

        (
            engine,
            mock,
            caller,
            anchor_id,
            interface_method_id,
            concrete_method_id,
        )
    }

    #[tokio::test]
    async fn outgoing_call_to_interface_method_is_corrected_to_the_implementing_class() {
        let (engine, mock, anchor, anchor_id, interface_method_id, concrete_method_id) =
            interface_dispatch_scenario().await;

        let timed_out = engine
            .materialize_call_edges(anchor_id, &anchor, Direction::Outgoing, &mock)
            .await
            .unwrap();
        assert!(!timed_out);

        let callees = engine
            .db()
            .get_neighbors(anchor_id, "calls", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(callees.len(), 1);
        assert_eq!(
            callees[0].0.id,
            Some(concrete_method_id),
            "the calls edge must point at the concrete method, not the interface's"
        );

        let implements = engine
            .db()
            .get_neighbors(interface_method_id, "implements", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(implements.len(), 1);
        assert_eq!(implements[0].0.id, Some(concrete_method_id));
    }

    fn go_node(
        fqn: &str,
        name: &str,
        node_kind: &str,
        line: i64,
        container_id: Option<i64>,
    ) -> Node {
        let mut n = node(fqn, line);
        n.name = name.to_string();
        n.node_kind = node_kind.to_string();
        n.language = "go".to_string();
        n.container_id = container_id;
        n.uri = "file:///m.go".to_string();
        n
    }

    /// Go analogue of [`interface_dispatch_scenario`]: a `Greeter` interface
    /// (with a `Greet` method) and a `Person` struct implementing it (with
    /// its own `Greet`), plus a `SayHello` function whose `outgoingCalls`
    /// mock response points `to` the interface's method — the exact shape
    /// observed live probing gopls (`docs/design/lsp-integration.md`
    /// callHierarchy note). The concrete container is `"Struct"` rather than
    /// `"Class"`, since the correction only checks the *callee's* container
    /// is `"Interface"`, not the shape of the implementer.
    /// Returns `(engine, mock, anchor, anchor_id, interface_method_id, concrete_method_id)`.
    async fn interface_dispatch_scenario_go()
    -> (QueryEngine, MockLspQueryClient, Node, i64, i64, i64) {
        let engine = engine();
        let caller = go_node("repo.SayHello", "SayHello", "Function", 10, None);
        let anchor_id = engine.db().upsert_node(caller.clone()).await.unwrap();

        let mut interface = go_node("repo.Greeter", "Greeter", "Interface", 0, None);
        interface.range.end_line = 3;
        let interface_id = engine.db().upsert_node(interface).await.unwrap();
        let interface_method = go_node(
            "repo.Greeter.Greet",
            "Greet",
            "Method",
            1,
            Some(interface_id),
        );
        let interface_method_id = engine.db().upsert_node(interface_method).await.unwrap();

        let mut concrete_struct = go_node("repo.Person", "Person", "Struct", 4, None);
        concrete_struct.range.end_line = 7;
        let concrete_struct_id = engine.db().upsert_node(concrete_struct).await.unwrap();
        let concrete_method = go_node(
            "repo.(*Person).Greet",
            "(*Person).Greet",
            "Method",
            5,
            Some(concrete_struct_id),
        );
        let concrete_method_id = engine.db().upsert_node(concrete_method).await.unwrap();

        let mut mock = MockLspQueryClient::new();
        let caller_item = CallHierarchyItem {
            name: "SayHello".into(),
            kind: 12,
            uri: "file:///m.go".into(),
            range: rng(10, 0, 12, 0),
            selection_range: rng(10, 0, 10, 8),
            raw: serde_json::json!({ "uri": "file:///m.go", "name": "SayHello" }),
        };
        mock.prepare.insert(
            ("file:///m.go".to_string(), 10, 0),
            vec![caller_item.clone()],
        );
        let interface_method_item = CallHierarchyItem {
            name: "Greet".into(),
            kind: 6,
            uri: "file:///m.go".into(),
            range: rng(1, 0, 1, 5),
            selection_range: rng(1, 0, 1, 5),
            raw: serde_json::json!({ "uri": "file:///m.go", "name": "Greet" }),
        };
        mock.outgoing.insert(
            caller_item.key(),
            vec![OutgoingCall {
                to: interface_method_item,
                from_ranges: vec![rng(11, 4, 11, 9)],
            }],
        );
        mock.implementations.insert(
            ("file:///m.go".to_string(), 1, 0),
            vec![Location {
                uri: "file:///m.go".to_string(),
                range: rng(5, 0, 5, 4),
            }],
        );

        (
            engine,
            mock,
            caller,
            anchor_id,
            interface_method_id,
            concrete_method_id,
        )
    }

    #[tokio::test]
    async fn outgoing_call_to_interface_method_is_corrected_for_go() {
        let (engine, mock, anchor, anchor_id, interface_method_id, concrete_method_id) =
            interface_dispatch_scenario_go().await;

        let timed_out = engine
            .materialize_call_edges(anchor_id, &anchor, Direction::Outgoing, &mock)
            .await
            .unwrap();
        assert!(!timed_out);

        let callees = engine
            .db()
            .get_neighbors(anchor_id, "calls", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(callees.len(), 1);
        assert_eq!(
            callees[0].0.id,
            Some(concrete_method_id),
            "the calls edge must point at the concrete method, not the interface's"
        );

        let implements = engine
            .db()
            .get_neighbors(interface_method_id, "implements", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(implements.len(), 1);
        assert_eq!(implements[0].0.id, Some(concrete_method_id));
    }

    #[tokio::test]
    async fn outgoing_call_to_interface_method_is_not_corrected_for_python() {
        let (engine, mock, mut anchor, anchor_id, interface_method_id, _concrete_method_id) =
            interface_dispatch_scenario().await;
        anchor.language = "python".to_string();
        // Re-persist the anchor with the python language so `find_node_by_position`
        // (used elsewhere) stays consistent; the correction reads `anchor.language`
        // from the argument passed to `materialize_call_edges` directly, so this
        // update isn't strictly required for the gate itself, but keeps the
        // fixture honest.
        engine.db().upsert_node(anchor.clone()).await.unwrap();

        let timed_out = engine
            .materialize_call_edges(anchor_id, &anchor, Direction::Outgoing, &mock)
            .await
            .unwrap();
        assert!(!timed_out);

        let callees = engine
            .db()
            .get_neighbors(anchor_id, "calls", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(callees.len(), 1);
        assert_eq!(
            callees[0].0.id,
            Some(interface_method_id),
            "a non-TypeScript anchor must never call `implementation`, so the edge stays uncorrected"
        );

        let implements = engine
            .db()
            .get_neighbors(interface_method_id, "implements", Direction::Outgoing)
            .await
            .unwrap();
        assert!(
            implements.is_empty(),
            "no implements edge without ever calling implementation"
        );
    }

    #[tokio::test]
    async fn outgoing_call_to_interface_method_falls_back_when_implementation_is_empty() {
        let (engine, mut mock, anchor, anchor_id, interface_method_id, _concrete_method_id) =
            interface_dispatch_scenario().await;
        mock.implementations.clear();

        let timed_out = engine
            .materialize_call_edges(anchor_id, &anchor, Direction::Outgoing, &mock)
            .await
            .unwrap();
        assert!(!timed_out);

        let callees = engine
            .db()
            .get_neighbors(anchor_id, "calls", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(callees.len(), 1);
        assert_eq!(
            callees[0].0.id,
            Some(interface_method_id),
            "an empty implementation() result must fall back to the interface method, never worse than pre-fix behavior"
        );
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
