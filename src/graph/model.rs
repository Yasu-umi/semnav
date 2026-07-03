//! Domain types for the graph store.

use serde::{Deserialize, Serialize};

/// An LSP line/character span (0-based, UTF-16 character units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
}

/// A declared symbol node (`docs/design/graph-model.md` "Nodes").
///
/// `id` is `None` for nodes not yet persisted; the db actor fills it on read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: Option<i64>,
    pub fqn: String,
    pub uri: String,
    pub name: String,
    pub language: String,
    /// LSP SymbolKind numeric value (pass-through).
    pub kind: i64,
    /// Adapter-classified NodeKind, serialized to a string.
    pub node_kind: String,
    pub construct: Option<String>,
    pub container_id: Option<i64>,
    pub range: Range,
    pub sel: Range,
    pub signature: Option<String>,
    pub documentation: Option<String>,
    pub detail: Option<String>,
    pub signature_hash: Option<String>,
    pub valid: bool,
    pub orphan: bool,
    pub generation: i64,
    pub is_external: bool,
}

/// A relation between two nodes (`docs/design/graph-model.md` "Edges").
///
/// `site` is the call/ref occurrence span; `None` for site-less edges such as
/// `contains`. "1 edge = 1 fromRange" enables row-level invalidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edge {
    pub id: Option<i64>,
    pub src_id: i64,
    pub dst_id: i64,
    /// `"contains"` / `"calls"` / `"references"` / ... (see `migrations/V0001__init.sql`).
    pub edge_type: String,
    /// Located occurrence; `None` for site-less edges like `contains`.
    pub site: Option<Site>,
    pub valid: bool,
}

/// A located occurrence (`fromRanges` / `originSelectionRange` / `Location.range`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub uri: String,
    pub range: Range,
}

/// A freshly-fetched symbol awaiting reconciliation against a uri's existing
/// `nodes` rows (`docs/design/graph-model.md` "dirty lifecycle" /
/// "Rename tracking"). Mirrors [`crate::indexer::FlatSymbol`] plus the
/// adapter-classified `node_kind` and precomputed `signature_hash`, so
/// `reconcile_file_symbols` never needs adapter access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileSymbol {
    pub fqn: String,
    pub name: String,
    pub kind: i64,
    pub node_kind: String,
    pub range: Range,
    pub sel: Range,
    pub detail: Option<String>,
    pub signature_hash: String,
    /// Index of the containing symbol within the same reconcile batch
    /// (`None` = top-level), mirroring `FlatSymbol::parent`.
    pub parent: Option<usize>,
}

/// Per-uri reconciliation result (`docs/design/indexing-and-cache.md` "Cache
/// Invalidation") — counts only, for logging; callers needing node ids should
/// re-read via `get_node_by_fqn`/`list_nodes`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Exact fqn match, identical `signature_hash`.
    pub unchanged: u64,
    /// Exact fqn match, `signature_hash` differs (edges invalidated).
    pub updated: u64,
    /// Matched as a rename candidate (same row id, new fqn/name).
    pub renamed: u64,
    /// No match in the old set — a fresh node.
    pub inserted: u64,
    /// No match in the new set, first miss — grace period.
    pub orphaned: u64,
    /// No match in the new set, second consecutive miss — physically deleted.
    pub deleted: u64,
}
