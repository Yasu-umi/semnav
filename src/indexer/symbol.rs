//! `textDocument/documentSymbol` types and the tree→flat conversion that
//! builds FQNs and parent links for the indexer.
//!
//! We advertise `hierarchicalDocumentSymbolSupport`, so the server returns
//! `DocumentSymbol[]` (nested). This module deserializes that shape, flattens
//! it depth-first (parents before children), and stamps each symbol with its
//! FQN (`<module path>.<container chain>.<name>`) and parent index — everything
//! the indexer needs to UPSERT nodes + `contains` edges in a single pass.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use serde::Deserialize;

use crate::adapters::SymbolKind;
use crate::graph::Range;

/// LSP `Position` (0-based line, UTF-16 `character`).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct LspPosition {
    pub line: u32,
    pub character: u32,
}

/// LSP `Range`.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct LspRange {
    pub start: LspPosition,
    pub end: LspPosition,
}

/// LSP hierarchical `DocumentSymbol`. `detail`/`tags` arrive as `null` from
/// pyright and absent-or-null from tsserver, so both are `Option` with defaults.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DocumentSymbol {
    pub name: String,
    #[serde(default)]
    pub detail: Option<String>,
    pub kind: u32,
    #[serde(default)]
    pub tags: Option<Vec<u32>>,
    pub range: LspRange,
    #[serde(rename = "selectionRange")]
    pub selection_range: LspRange,
    #[serde(default)]
    pub children: Option<Vec<DocumentSymbol>>,
}

/// A flattened symbol awaiting persistence. `parent` is the index of the
/// containing symbol within the same flat list (`None` = top-level), letting
/// the indexer UPSERT parents first and thread the DB id into children.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatSymbol {
    pub fqn: String,
    pub name: String,
    /// Raw LSP `SymbolKind`; the adapter maps it to `NodeKind` at node assembly.
    pub kind: u32,
    pub range: Range,
    pub sel: Range,
    pub detail: Option<String>,
    pub parent: Option<usize>,
}

impl LspRange {
    /// Convert to the graph's `Range` (widening u32 → i64).
    fn to_graph(self) -> Range {
        Range {
            start_line: self.start.line as i64,
            start_col: self.start.character as i64,
            end_line: self.end.line as i64,
            end_col: self.end.character as i64,
        }
    }
}

impl FlatSymbol {
    /// Synthetic root symbol for `module_path`, spanning the whole file.
    ///
    /// `textDocument/documentSymbol` never yields an entry for the module
    /// itself, so a bare module-top-level position (a call or reference
    /// outside every `def`/`class`) has no indexed node covering it —
    /// `find_node_by_position` (`src/graph/db.rs`) returns `None` and the
    /// caller silently drops that occurrence (`docs/design/lsp-integration.md`
    /// "callHierarchy" pyright note: "a module node is generated to serve as
    /// the entry-point caller"). Appending this to the flat list gives every
    /// position in the file a covering container.
    pub fn module_root(module_path: &str) -> Self {
        let name = module_path
            .rsplit('.')
            .next()
            .unwrap_or(module_path)
            .to_string();
        FlatSymbol {
            fqn: module_path.to_string(),
            name,
            kind: SymbolKind::Module as u32,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: i64::MAX,
                end_col: i64::MAX,
            },
            sel: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
            },
            detail: None,
            parent: None,
        }
    }
}

/// Flatten a hierarchical `DocumentSymbol[]` into a parent-before-child list,
/// building each FQN as `<module_path>.<container chain>.<name>`.
///
/// Note: 0.0.1 builds FQNs by name only. TypeScript overload normalization
/// (arity-suffixed FQN like `app.repo.load#1`) needs the hover signature and is
/// deferred to the enrichment step; with documentSymbol-only indexing, same-name
/// overloads collapse onto one row via `ON CONFLICT(fqn)`.
pub fn flatten_document_symbols(symbols: &[DocumentSymbol], module_path: &str) -> Vec<FlatSymbol> {
    let mut out = Vec::new();
    for symbol in symbols {
        flatten_one(symbol, module_path, None, &mut out);
    }
    out
}

fn flatten_one(
    symbol: &DocumentSymbol,
    prefix: &str,
    parent: Option<usize>,
    out: &mut Vec<FlatSymbol>,
) {
    let fqn = format!("{prefix}.{}", symbol.name);
    let idx = out.len();
    out.push(FlatSymbol {
        fqn: fqn.clone(),
        name: symbol.name.clone(),
        kind: symbol.kind,
        range: symbol.range.to_graph(),
        sel: symbol.selection_range.to_graph(),
        detail: symbol.detail.clone(),
        parent,
    });
    if let Some(children) = &symbol.children {
        for child in children {
            // Recurse with this symbol's FQN as the child prefix and its index
            // as the parent link (depth-first pre-order keeps parents first).
            flatten_one(child, &fqn, Some(idx), out);
        }
    }
}

/// Content fingerprint for cache invalidation (`docs/design/graph-model.md`
/// "dirty lifecycle"). Excludes absolute position (only span *sizes*, via
/// [`span`]) so a pure line-shift elsewhere in the file doesn't spuriously
/// register as "content changed."
pub fn signature_fingerprint(sym: &FlatSymbol) -> String {
    let mut h = DefaultHasher::new();
    sym.name.hash(&mut h);
    sym.kind.hash(&mut h);
    sym.detail.hash(&mut h);
    span(&sym.range).hash(&mut h);
    span(&sym.sel).hash(&mut h);
    format!("{:016x}", h.finish())
}

fn span(r: &Range) -> (i64, i64, i64, i64) {
    (r.start_line, r.start_col, r.end_line, r.end_col)
}

/// Derive a 0.0.1 module path from a file URI: root-relative path, extension
/// stripped, separators → `.`. Precise package resolution (`__init__.py` /
/// `package.json` "main") is deferred to 0.1+.
pub fn module_path_from_uri(uri: &str, root_uri: &str) -> String {
    let rel = uri.strip_prefix(root_uri).unwrap_or(uri);
    let stem = rel.rsplit_once('.').map(|(base, _)| base).unwrap_or(rel);
    let stem = stem.strip_prefix('/').unwrap_or(stem);
    stem.replace(['/', '\\'], ".")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS_WITH_METHOD: &str = r#"[
        {
            "name": "Repo",
            "detail": null,
            "kind": 5,
            "tags": null,
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 0}},
            "selectionRange": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 10}},
            "children": [
                {
                    "name": "load",
                    "kind": 12,
                    "range": {"start": {"line": 1, "character": 4}, "end": {"line": 3, "character": 4}},
                    "selectionRange": {"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 11}},
                    "children": []
                }
            ]
        },
        {
            "name": "helper",
            "kind": 12,
            "range": {"start": {"line": 6, "character": 0}, "end": {"line": 7, "character": 0}},
            "selectionRange": {"start": {"line": 6, "character": 4}, "end": {"line": 6, "character": 10}}
        }
    ]"#;

    #[test]
    fn flatten_builds_fqns_and_parent_links() {
        let symbols: Vec<DocumentSymbol> = serde_json::from_str(CLASS_WITH_METHOD).expect("parse");
        let flat = flatten_document_symbols(&symbols, "app.repo");

        assert_eq!(flat.len(), 3);

        assert_eq!(flat[0].fqn, "app.repo.Repo");
        assert_eq!(flat[0].name, "Repo");
        assert_eq!(flat[0].kind, 5);
        assert_eq!(flat[0].parent, None);
        assert_eq!(flat[0].sel.start_col, 6);

        assert_eq!(flat[1].fqn, "app.repo.Repo.load");
        assert_eq!(flat[1].name, "load");
        assert_eq!(flat[1].kind, 12);
        assert_eq!(flat[1].parent, Some(0));

        assert_eq!(flat[2].fqn, "app.repo.helper");
        assert_eq!(flat[2].parent, None);

        // Parents always precede their children in the flat list.
        for (i, sym) in flat.iter().enumerate() {
            if let Some(p) = sym.parent {
                assert!(p < i, "parent {p} must precede child {i}");
            }
        }
    }

    #[test]
    fn flatten_maps_ranges() {
        let symbols: Vec<DocumentSymbol> = serde_json::from_str(CLASS_WITH_METHOD).expect("parse");
        let flat = flatten_document_symbols(&symbols, "app.repo");
        assert_eq!(
            flat[1].range,
            Range {
                start_line: 1,
                start_col: 4,
                end_line: 3,
                end_col: 4
            }
        );
    }

    #[test]
    fn flatten_handles_deeply_nested_symbols() {
        let json = r#"[
            {
                "name": "A",
                "kind": 5,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 9, "character": 0}},
                "selectionRange": {"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 7}},
                "children": [
                    {
                        "name": "B",
                        "kind": 5,
                        "range": {"start": {"line": 1, "character": 4}, "end": {"line": 8, "character": 4}},
                        "selectionRange": {"start": {"line": 1, "character": 10}, "end": {"line": 1, "character": 11}},
                        "children": [
                            {
                                "name": "c",
                                "kind": 12,
                                "range": {"start": {"line": 2, "character": 8}, "end": {"line": 3, "character": 8}},
                                "selectionRange": {"start": {"line": 2, "character": 15}, "end": {"line": 2, "character": 16}}
                            }
                        ]
                    }
                ]
            }
        ]"#;
        let symbols: Vec<DocumentSymbol> = serde_json::from_str(json).expect("parse");
        let flat = flatten_document_symbols(&symbols, "m");
        assert_eq!(flat[0].fqn, "m.A");
        assert_eq!(flat[1].fqn, "m.A.B");
        assert_eq!(flat[2].fqn, "m.A.B.c");
        assert_eq!(flat[2].parent, Some(1));
        assert_eq!(flat[1].parent, Some(0));
    }

    #[test]
    fn module_root_spans_whole_file_and_has_no_parent() {
        let root = FlatSymbol::module_root("app.repo");
        assert_eq!(root.fqn, "app.repo");
        assert_eq!(root.name, "repo");
        assert_eq!(root.kind, 2);
        assert_eq!(root.parent, None);
        assert_eq!(root.range.start_line, 0);
        assert_eq!(root.range.start_col, 0);
        assert_eq!(root.range.end_line, i64::MAX);
        assert_eq!(root.range.end_col, i64::MAX);
    }

    #[test]
    fn module_path_handles_trailing_and_no_slash() {
        assert_eq!(
            module_path_from_uri("file:///repo/app/repo.py", "file:///repo/"),
            "app.repo"
        );
        assert_eq!(
            module_path_from_uri("file:///repo/app/repo.py", "file:///repo"),
            "app.repo"
        );
        assert_eq!(
            module_path_from_uri("file:///repo/pkg/mod.tsx", "file:///repo/"),
            "pkg.mod"
        );
    }

    #[test]
    fn deserializes_pyright_null_detail_and_tags() {
        let symbols: Vec<DocumentSymbol> = serde_json::from_str(CLASS_WITH_METHOD).expect("parse");
        assert_eq!(symbols[0].detail, None);
        assert_eq!(symbols[0].tags, None);
        assert_eq!(symbols[1].detail, None);
    }

    /// Real pyright `textDocument/documentSymbol` on `class Repo: def
    /// __init__(self): self.base = 1` (`tests/fixtures/lsp-probe/`,
    /// `docs/design/lsp-integration.md`: "`children` also includes
    /// parameters and instance attributes (e.g. `self.base`)"). `base`
    /// (`kind=13` Variable) must flatten as a child of `Repo`, sitting
    /// alongside `__init__`, not get dropped or promoted to a sibling.
    #[test]
    fn flatten_includes_pyright_self_attribute_as_a_child() {
        let raw = include_str!(
            "../../tests/fixtures/lsp-probe/captures/python_document_symbol_self_attribute.json"
        );
        let symbols: Vec<DocumentSymbol> = serde_json::from_str(raw).expect("parse capture");
        let flat = flatten_document_symbols(&symbols, "app.mod");

        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].fqn, "app.mod.Repo");
        assert_eq!(flat[1].fqn, "app.mod.Repo.__init__");
        assert_eq!(flat[1].parent, Some(0));
        assert_eq!(flat[2].fqn, "app.mod.Repo.base");
        assert_eq!(flat[2].kind, 13);
        assert_eq!(
            flat[2].parent,
            Some(0),
            "self.base is a child of Repo, a sibling of __init__ — not nested under it"
        );
    }

    /// Real typescript-language-server `textDocument/documentSymbol` on a
    /// class with 3 overload signatures for the same method name
    /// (`tests/fixtures/lsp-probe/`, `docs/design/lsp-integration.md`:
    /// "Overloads appear as separate entries with the same name (parallel
    /// children, all with `kind=6`)"). 0.0.1's `ON CONFLICT(fqn)` upsert
    /// collapses same-name overloads into one graph row — this pins that as
    /// the current, intentional (if lossy) flattening behavior: all 3 raw
    /// entries survive `flatten_document_symbols` with the identical fqn,
    /// so it's the later UPSERT, not this step, that does the collapsing.
    #[test]
    fn flatten_keeps_tsserver_overload_duplicates_as_separate_entries() {
        let raw = include_str!(
            "../../tests/fixtures/lsp-probe/captures/typescript_document_symbol_overloads.json"
        );
        let symbols: Vec<DocumentSymbol> = serde_json::from_str(raw).expect("parse capture");
        let flat = flatten_document_symbols(&symbols, "app.mod");

        assert_eq!(flat.len(), 4, "Greeter + 3 greet overloads");
        let overloads: Vec<_> = flat.iter().filter(|s| s.name == "greet").collect();
        assert_eq!(overloads.len(), 3);
        assert!(
            overloads.iter().all(|s| s.kind == 6),
            "all 3 overloads must keep kind=6 (Method)"
        );
        assert!(
            overloads.iter().all(|s| s.fqn == "app.mod.Greeter.greet"),
            "same fqn for all 3 — flatten_document_symbols does not deduplicate; \
             the ON CONFLICT(fqn) upsert at write time is what collapses them"
        );
    }
}
