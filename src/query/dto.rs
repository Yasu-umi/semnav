//! Wire DTOs for the query layer: the LSP-shaped `Position`/`Range` and the
//! metadata-only `Node` returned by every find_* tool, plus `kind_label`
//! normalization (`docs/design/mcp-tools.md` "Common types" / "kind_label normalization").
//!
//! `kind_label` reuses the adapter classification already stored on
//! `nodes.node_kind` (the same string `map_symbol_kind` produces) rather than
//! re-deriving from `kind_num`, so adapter `Custom` values survive. The one
//! query-time refinement is the TS `type`-alias trap: a `Variable` whose hover
//! `construct` is `"type"` is promoted to `"TypeAlias"`. 0.0.1 indexing does
//! not run hover, so `construct` is `NULL` until an on-demand query fills it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::graph::{Node, Range};

/// LSP `Position` (0-based line, UTF-16 `character`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP `Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RangeDto {
    pub start: Position,
    pub end: Position,
}

impl From<Range> for RangeDto {
    fn from(r: Range) -> Self {
        Self {
            start: Position {
                line: to_u32(r.start_line),
                character: to_u32(r.start_col),
            },
            end: Position {
                line: to_u32(r.end_line),
                character: to_u32(r.end_col),
            },
        }
    }
}

/// Widen a graph column (`i64`) to an LSP position field (`u32`). Graph spans
/// are always non-negative; saturate as a guard against a corrupt row.
fn to_u32(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}

/// A metadata-only symbol node. Code body is deliberately absent — clients that
/// need source call `read_range` (design principles 1/4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NodeDto {
    pub fqn: String,
    pub uri: String,
    pub name: String,
    pub language: String,
    /// Human-readable kind, normalized from `node_kind` (+ `construct`).
    pub kind_label: String,
    /// Raw LSP `SymbolKind` value.
    pub kind_num: u32,
    /// Hover-derived auxiliary classification (`"type"` / `"interface"` / ...).
    pub construct: Option<String>,
    /// Declaration span.
    pub range: RangeDto,
    /// Identifier (`selectionRange`) span.
    pub selection_range: RangeDto,
    /// Hover-derived signature, when known.
    pub signature: Option<String>,
    /// Hover-derived docstring/JSDoc, when known.
    pub documentation: Option<String>,
    pub is_external: bool,
}

impl NodeDto {
    /// Build the DTO from a persisted [`Node`], applying `kind_label`
    /// normalization.
    pub fn from_node(node: &Node) -> Self {
        Self {
            fqn: node.fqn.clone(),
            uri: node.uri.clone(),
            name: node.name.clone(),
            language: node.language.clone(),
            kind_label: kind_label(&node.node_kind, node.construct.as_deref()),
            kind_num: to_u32(node.kind),
            construct: node.construct.clone(),
            range: node.range.into(),
            selection_range: node.sel.into(),
            signature: node.signature.clone(),
            documentation: node.documentation.clone(),
            is_external: node.is_external,
        }
    }
}

/// Normalize a stored `node_kind` label into the DTO `kind_label`, applying the
/// TS `type`-alias promotion when a hover `construct` is present.
///
/// `node_kind` already carries the adapter classification (including `Custom`
/// labels like `"TypeAlias"` or `"Unknown(99)"`), so this is a near-identity —
/// the only rewrite is `Variable` + `construct="type"` → `"TypeAlias"`.
pub fn kind_label(node_kind: &str, construct: Option<&str>) -> String {
    if node_kind == "Variable" && construct == Some("type") {
        "TypeAlias".to_string()
    } else {
        node_kind.to_string()
    }
}

// ---- operation result shapes ------------------------------------------------
//
// One canonical type per operation (`docs/design/mcp-tools.md`). The rmcp layer
// (Step 6) is a thin serializer over these; tests target the engine, not wire
// framing. `next_cursor` is the opaque encoded [`crate::query::filter::Cursor`].

/// `find_symbol` → a page of metadata nodes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FindSymbolResult {
    pub nodes: Vec<NodeDto>,
    pub next_cursor: Option<String>,
}

/// `read_range` → the slice of source for `[start, end)` (0-based, exclusive
/// end line). `total_lines` is the file's full line count (best-effort).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadRangeResult {
    pub uri: String,
    pub content: String,
    pub range: RangeDto,
    pub total_lines: u32,
}

/// `find_definition` → the declaration node(s) for an occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FindDefinitionResult {
    pub nodes: Vec<NodeDto>,
}

/// One entry in a `find_references` page: the referencing node and every range
/// inside it where the target is mentioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReferenceGroup {
    pub node: NodeDto,
    pub sites: Vec<RangeDto>,
}

/// `find_references` → referencing nodes grouped by their declaration, plus the
/// continuation cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesResult {
    pub references: Vec<ReferenceGroup>,
    pub next_cursor: Option<String>,
}

/// One entry in a `find_callers`/`find_callees` page: the adjacent callable and
/// the call-site ranges tying it to the anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CallGraphNode {
    pub node: NodeDto,
    pub call_sites: Vec<RangeDto>,
}

/// `find_callers`/`find_callees` → adjacent callables grouped by their
/// declaration, plus the continuation cursor. The rmcp layer renames `items` to
/// `callers`/`callees` per the tool contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CallGraphResult {
    pub items: Vec<CallGraphNode>,
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_dto_widens_graph_columns() {
        let r = Range {
            start_line: 0,
            start_col: 4,
            end_line: 3,
            end_col: 12,
        };
        let dto = RangeDto::from(r);
        assert_eq!(
            dto.start,
            Position {
                line: 0,
                character: 4
            }
        );
        assert_eq!(
            dto.end,
            Position {
                line: 3,
                character: 12
            }
        );
    }

    #[test]
    fn range_dto_clamps_negative_to_zero() {
        let r = Range {
            start_line: -1,
            start_col: -5,
            end_line: 0,
            end_col: 0,
        };
        let dto = RangeDto::from(r);
        assert_eq!(
            dto.start,
            Position {
                line: 0,
                character: 0
            }
        );
    }

    #[test]
    fn range_dto_clamps_above_u32_max() {
        let r = Range {
            start_line: u32::MAX as i64 + 1,
            start_col: 0,
            end_line: 0,
            end_col: 0,
        };
        let dto = RangeDto::from(r);
        assert_eq!(dto.start.line, u32::MAX);
    }

    #[test]
    fn kind_label_passes_standard_through() {
        assert_eq!(kind_label("Function", None), "Function");
        assert_eq!(kind_label("Class", None), "Class");
    }

    #[test]
    fn kind_label_preserves_custom_labels() {
        assert_eq!(kind_label("Unknown(99)", None), "Unknown(99)");
        assert_eq!(kind_label("TypeAlias", None), "TypeAlias");
    }

    #[test]
    fn kind_label_promotes_variable_with_type_construct() {
        // TS `type` alias arrives as kind=13 (Variable) until hover refine.
        assert_eq!(kind_label("Variable", Some("type")), "TypeAlias");
    }

    #[test]
    fn kind_label_leaves_variable_without_type_construct() {
        assert_eq!(kind_label("Variable", None), "Variable");
        assert_eq!(kind_label("Variable", Some("other")), "Variable");
    }

    fn sample_node(node_kind: &str, construct: Option<&str>) -> Node {
        Node {
            id: Some(1),
            fqn: "app.repo.Repo".to_string(),
            uri: "file:///app/repo.py".to_string(),
            name: "Repo".to_string(),
            language: "python".to_string(),
            kind: 5,
            node_kind: node_kind.to_string(),
            construct: construct.map(str::to_string),
            container_id: None,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 5,
                end_col: 0,
            },
            sel: Range {
                start_line: 0,
                start_col: 6,
                end_line: 0,
                end_col: 10,
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

    #[test]
    fn from_node_maps_all_fields_and_normalizes_kind() {
        let n = sample_node("Class", None);
        let dto = NodeDto::from_node(&n);
        assert_eq!(dto.fqn, "app.repo.Repo");
        assert_eq!(dto.name, "Repo");
        assert_eq!(dto.language, "python");
        assert_eq!(dto.kind_label, "Class");
        assert_eq!(dto.kind_num, 5);
        assert_eq!(
            dto.selection_range.start,
            Position {
                line: 0,
                character: 6
            }
        );
        assert!(!dto.is_external);
    }

    #[test]
    fn from_node_promotes_type_alias() {
        let n = sample_node("Variable", Some("type"));
        let dto = NodeDto::from_node(&n);
        assert_eq!(dto.kind_label, "TypeAlias");
        assert_eq!(dto.construct.as_deref(), Some("type"));
    }
}
