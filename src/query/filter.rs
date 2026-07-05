//! Query request shape: `SymbolRef`, `Filter`, `MatchMode`, `Page`, plus the
//! stable sort key and its opaque base64 cursor codec
//! (`docs/design/mcp-tools.md` "Common types" / "Page").

use anyhow::{Result, anyhow};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::graph::Node;
use crate::query::dto::kind_label;

/// How a `find_symbol` pattern is matched against a symbol's fqn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Match whole dot-delimited segments: `repo` matches `app.repo.Repo` (the
    /// `repo` segment) but not `app.repos.X`.
    #[default]
    Segment,
    /// Substring match anywhere in the fqn.
    Contains,
    /// Exact full-fqn match.
    Exact,
    /// Typo-tolerant match against each dot-delimited segment, using
    /// normalized Levenshtein similarity (`FUZZY_SIMILARITY_THRESHOLD`).
    /// Same per-segment shape as `Segment`, so a misspelled short name (e.g.
    /// `helpr`) still finds `app.repo.helper`.
    Fuzzy,
}

/// Minimum `strsim::normalized_levenshtein` similarity (in `[0.0, 1.0]`) for
/// `MatchMode::Fuzzy` to consider a segment a match. Chosen empirically: high
/// enough that unrelated short names (e.g. `main` vs `helper`) don't
/// collide, low enough to absorb a single-character typo/transposition in a
/// typical identifier length.
const FUZZY_SIMILARITY_THRESHOLD: f64 = 0.7;

impl MatchMode {
    /// Apply this mode to an fqn. `ignore_case` folds ASCII case for both sides.
    pub fn matches(self, pattern: &str, ignore_case: bool, fqn: &str) -> bool {
        match self {
            Self::Segment => fqn.split('.').any(|seg| eq(pattern, seg, ignore_case)),
            Self::Exact => eq(pattern, fqn, ignore_case),
            Self::Contains => {
                if ignore_case {
                    fqn.to_ascii_lowercase()
                        .contains(&pattern.to_ascii_lowercase())
                } else {
                    fqn.contains(pattern)
                }
            }
            Self::Fuzzy => fqn
                .split('.')
                .any(|seg| fuzzy_eq(pattern, seg, ignore_case)),
        }
    }
}

/// Whether `a`/`b` are similar enough for `MatchMode::Fuzzy`, per
/// `FUZZY_SIMILARITY_THRESHOLD`. `ignore_case` ASCII-folds both sides first,
/// matching `eq`'s convention.
fn fuzzy_eq(a: &str, b: &str, ignore_case: bool) -> bool {
    if ignore_case {
        let a = a.to_ascii_lowercase();
        let b = b.to_ascii_lowercase();
        strsim::normalized_levenshtein(&a, &b) >= FUZZY_SIMILARITY_THRESHOLD
    } else {
        strsim::normalized_levenshtein(a, b) >= FUZZY_SIMILARITY_THRESHOLD
    }
}

/// `find_symbol`'s two response-shaping toggles, grouped into one parameter
/// to keep `QueryRuntime::find_symbol`'s signature under clippy's
/// argument-count lint. `brief` swaps `nodes` for `fqns`; `with_signature`
/// opts into a hover backfill pass on each returned node still missing a
/// `signature` (`docs/design/mcp-tools.md` "Populating `signature`") — a
/// no-op when `brief` is set, since there are no `Node`s to enrich.
#[derive(Debug, Clone, Copy, Default)]
pub struct FindSymbolOptions {
    pub brief: bool,
    pub with_signature: bool,
}

fn eq(a: &str, b: &str, ignore_case: bool) -> bool {
    if ignore_case {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// A symbol reference: by fqn, or by an occurrence position (`docs/design/
/// mcp-tools.md` SymbolRef). LSP positions are 0-based line/character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SymbolRef {
    Fqn(String),
    At {
        uri: String,
        line: u32,
        character: u32,
    },
}

/// Cross-tool narrowing: language, kind-label allow-list, external inclusion
/// (`docs/design/mcp-tools.md` Filter). Defaults pass everything non-external.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub language: Option<String>,
    /// Normalized `kind_label` values to keep (None = all kinds).
    pub kind: Option<Vec<String>>,
    pub include_external: bool,
}

impl Filter {
    /// Whether `node` (with normalized `kind_label`) survives this filter.
    pub fn matches(&self, node: &Node) -> bool {
        if !self.include_external && node.is_external {
            return false;
        }
        if let Some(lang) = &self.language
            && &node.language != lang
        {
            return false;
        }
        if let Some(kinds) = &self.kind {
            let label = kind_label(&node.node_kind, node.construct.as_deref());
            if !kinds.iter().any(|k| k == &label) {
                return false;
            }
        }
        true
    }
}

/// Upper bound on `Page::limit`, applied at every pagination boundary so a
/// client-supplied limit can't force an unbounded scan/response.
pub const MAX_PAGE_LIMIT: usize = 500;

/// `find_call_path`'s default/max hop cap and LSP-call budget
/// (`docs/design/mcp-tools.md` "find_call_path"). The BFS always expands a
/// cold node via a live call-hierarchy round trip rather than silently
/// stopping at whatever's already cached (a cache-only "not reachable" would
/// be indistinguishable from "never queried") — these bounds exist only to
/// keep a wide fan-out from turning one request into hundreds of round trips.
pub const DEFAULT_CALL_PATH_MAX_DEPTH: u32 = 8;
pub const MAX_CALL_PATH_MAX_DEPTH: u32 = 20;
pub const DEFAULT_CALL_PATH_LSP_BUDGET: u32 = 30;
pub const MAX_CALL_PATH_LSP_BUDGET: u32 = 200;

/// Cursor pagination: a page size and an opaque resumption token
/// (`docs/design/mcp-tools.md` Page).
#[derive(Debug, Clone)]
pub struct Page {
    /// Max items in this page; clamped to ≥1 by the caller.
    pub limit: usize,
    pub cursor: Option<Cursor>,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: 100,
            cursor: None,
        }
    }
}

/// The cursor handed back to a client, encoding the last-returned sort key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub key: SortKey,
}

impl Cursor {
    /// Opaque URL-safe base64 of a small JSON payload. Clients treat this as a
    /// black box; decoding only happens server-side via [`Cursor::decode`].
    pub fn encode(&self) -> String {
        let payload = CursorPayload {
            f: self.key.fqn.clone(),
            u: self.key.uri.clone(),
            l: self.key.start_line,
            c: self.key.start_col,
        };
        let bytes = serde_json::to_vec(&payload).unwrap_or_default();
        URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Decode an opaque token back into a cursor. Fails on malformed base64/JSON
    /// so a corrupt/guessed token surfaces as an error rather than a wrong page.
    pub fn decode(token: &str) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(token.as_bytes())
            .map_err(|e| anyhow!("invalid cursor (base64): {e}"))?;
        let payload: CursorPayload =
            serde_json::from_slice(&bytes).map_err(|e| anyhow!("invalid cursor (json): {e}"))?;
        Ok(Self {
            key: SortKey {
                fqn: payload.f,
                uri: payload.u,
                start_line: payload.l,
                start_col: payload.c,
            },
        })
    }
}

/// Stable, total ordering used for deterministic pagination
/// (`docs/design/mcp-tools.md`): `(fqn, uri, start_line, start_col)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SortKey {
    pub fqn: String,
    pub uri: String,
    pub start_line: i64,
    pub start_col: i64,
}

impl SortKey {
    pub fn from_node(n: &Node) -> Self {
        Self {
            fqn: n.fqn.clone(),
            uri: n.uri.clone(),
            start_line: n.range.start_line,
            start_col: n.range.start_col,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CursorPayload {
    f: String,
    u: String,
    l: i64,
    c: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Range;

    #[test]
    fn match_mode_segment_matches_any_dot_segment() {
        let mode = MatchMode::Segment;
        assert!(mode.matches("repo", false, "app.repo.Repo"));
        assert!(mode.matches("Repo", false, "app.repo.Repo"));
        assert!(!mode.matches("rep", false, "app.repo.Repo"));
        assert!(!mode.matches("repo", false, "app.repos.X"));
    }

    #[test]
    fn match_mode_contains_is_substring() {
        assert!(MatchMode::Contains.matches("epo", false, "app.repo.Repo"));
        assert!(!MatchMode::Contains.matches("zzz", false, "app.repo.Repo"));
    }

    #[test]
    fn match_mode_exact_is_whole_fqn() {
        assert!(MatchMode::Exact.matches("app.repo.Repo", false, "app.repo.Repo"));
        assert!(!MatchMode::Exact.matches("Repo", false, "app.repo.Repo"));
    }

    #[test]
    fn match_mode_fuzzy_tolerates_a_single_typo() {
        assert!(MatchMode::Fuzzy.matches("helpr", false, "app.repo.helper"));
        assert!(MatchMode::Fuzzy.matches("Repo", false, "app.repo.Repo"));
    }

    #[test]
    fn match_mode_fuzzy_rejects_unrelated_names() {
        assert!(!MatchMode::Fuzzy.matches("main", false, "app.repo.Repo"));
        assert!(!MatchMode::Fuzzy.matches("totally_unrelated", false, "app.repo.helper"));
    }

    #[test]
    fn match_mode_fuzzy_ignore_case_folds_ascii() {
        assert!(MatchMode::Fuzzy.matches("HELPR", true, "app.repo.helper"));
        assert!(!MatchMode::Fuzzy.matches("HELPR", false, "app.repo.helper"));
    }

    #[test]
    fn match_mode_ignore_case_folds_ascii() {
        assert!(MatchMode::Segment.matches("REPO", true, "app.repo.Repo"));
        assert!(!MatchMode::Segment.matches("REPO", false, "app.repo.Repo"));
    }

    fn node(language: &str, kind: &str, external: bool) -> Node {
        Node {
            id: Some(1),
            fqn: "x".to_string(),
            uri: "file:///x".to_string(),
            name: "x".to_string(),
            language: language.to_string(),
            kind: 11,
            node_kind: kind.to_string(),
            construct: None,
            container_id: None,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 0,
            },
            sel: Range {
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
            is_external: external,
        }
    }

    #[test]
    fn filter_excludes_external_by_default() {
        let f = Filter::default();
        assert!(f.matches(&node("python", "Function", false)));
        assert!(!f.matches(&node("python", "Function", true)));
    }

    #[test]
    fn filter_include_external_keeps_them() {
        let f = Filter {
            include_external: true,
            ..Default::default()
        };
        assert!(f.matches(&node("python", "Function", true)));
    }

    #[test]
    fn filter_narrows_by_language_and_kind_label() {
        let f = Filter {
            language: Some("python".to_string()),
            kind: Some(vec!["Function".to_string()]),
            ..Default::default()
        };
        assert!(f.matches(&node("python", "Function", false)));
        assert!(!f.matches(&node("typescript", "Function", false)));
        assert!(!f.matches(&node("python", "Class", false)));
    }

    #[test]
    fn filter_kind_uses_normalized_label() {
        // Variable + construct=type normalizes to TypeAlias.
        let f = Filter {
            kind: Some(vec!["TypeAlias".to_string()]),
            ..Default::default()
        };
        let mut n = node("typescript", "Variable", false);
        n.construct = Some("type".to_string());
        assert!(f.matches(&n));
    }

    #[test]
    fn cursor_roundtrips_sort_key() {
        let cur = Cursor {
            key: SortKey {
                fqn: "app.repo.Repo".to_string(),
                uri: "file:///app/repo.py".to_string(),
                start_line: 12,
                start_col: 4,
            },
        };
        let token = cur.encode();
        assert_eq!(Cursor::decode(&token).unwrap(), cur);
    }

    #[test]
    fn cursor_decode_rejects_garbage() {
        assert!(Cursor::decode("!!!not-base64").is_err());
        assert!(Cursor::decode("").is_err());
    }

    #[test]
    fn sort_key_orders_by_fqn_first() {
        let a = SortKey {
            fqn: "a".into(),
            uri: "u".into(),
            start_line: 0,
            start_col: 0,
        };
        let b = SortKey {
            fqn: "b".into(),
            uri: "u".into(),
            start_line: 0,
            start_col: 0,
        };
        assert!(a < b);
    }
}
