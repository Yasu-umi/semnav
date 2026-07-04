//! Query orchestration: the five `find_*` tools + `read_range`, composing graph
//! reads with on-demand LSP edge construction (`docs/design/mcp-tools.md`).
//!
//! The [`QueryEngine`] holds the db handle and workspace root URI; an LSP client
//! is supplied per call (`None` ⇒ cache-only degradation). This module is
//! rmcp-independent and unit-testable: the real client (`ClientLspQueryClient`)
//! and the in-memory mock share one code path through the `LspQueryClient`
//! trait. The rmcp server (Step 6) is a thin serializer over these results.
//!
//! Submodules:
//! - [`dto`] — wire DTOs and `kind_label` normalization;
//! - [`filter`] — `SymbolRef` / `Filter` / `Page` and the opaque cursor codec;
//! - [`lsp_query`] — the query-time LSP trait + real/mock impls;
//! - [`resolver`] — on-demand edge construction and neighbor grouping;
//! - [`ops`] — the six public operations on [`QueryEngine`].

mod dto;
mod filter;
mod lsp_query;
mod ops;
mod pool;
mod resolver;

pub use dto::{
    CallGraphNode, CallGraphResult, FindDefinitionResult, FindReferencesResult, FindSymbolResult,
    NodeDto, Position, RangeDto, ReadRangeResult, ReferenceGroup, kind_label,
};
pub use filter::{
    Cursor, Filter, FindSymbolOptions, MAX_PAGE_LIMIT, MatchMode, Page, SortKey, SymbolRef,
};
pub use lsp_query::{
    CallHierarchyItem, Hover, IncomingCall, Location, LspQueryClient, OutgoingCall,
};
pub use pool::{Degradation, DegradeReason, LspStatus, QueryRuntime};

use std::path::PathBuf;

use crate::graph::DbActor;
use crate::indexer::uri_to_path;

/// The query engine: graph reads + on-demand LSP edge construction.
///
/// Holds the db handle and the workspace root URI. Each operation takes the LSP
/// client by reference (`None` reads only already-materialized edges). Engine
/// methods never spawn servers themselves — the caller (Step 6 rmcp layer, via
/// the supervisor pool) acquires a per-language client and passes it in.
///
/// `Clone` is cheap (an `mpsc::Sender` clone plus a `String` clone) — `Clone`
/// so `QueryRuntime` can hand a detached background refresh
/// (`docs/design/lsp-integration.md` "cache-first + background refresh") its
/// own owned handle rather than a borrow tied to the foreground call's stack.
#[derive(Clone)]
pub struct QueryEngine {
    db: DbActor,
    root_uri: String,
}

impl QueryEngine {
    pub fn new(db: DbActor, root_uri: String) -> Self {
        Self { db, root_uri }
    }

    /// Workspace root URI (file discovery / `module_path_from_uri`).
    pub fn root_uri(&self) -> &str {
        &self.root_uri
    }

    /// The backing db actor.
    pub fn db(&self) -> &DbActor {
        &self.db
    }

    /// Resolve `uri` to a filesystem path confined to the workspace root,
    /// rejecting any path (via `..`, symlinks, etc.) that escapes it. This is
    /// the sole gate between client-supplied URIs and filesystem reads
    /// (`read_range`, `ensure_open`) — `uri_to_path` itself does no validation.
    pub(super) fn confine(&self, uri: &str) -> anyhow::Result<PathBuf> {
        let root = uri_to_path(&self.root_uri);
        let candidate = uri_to_path(uri);
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        // `..` must be resolved lexically first: if `candidate` doesn't exist
        // (e.g. it's a nonexistent-file probe), `canonicalize` fails and falls
        // back to the raw path below — and `PathBuf::starts_with` compares
        // components literally, without resolving `..`. Without this step, a
        // path like `<root>/../etc/passwd` would literally start with `<root>`
        // and slip past the check.
        let normalized = Self::lexically_normalize(&candidate);
        let resolved = std::fs::canonicalize(&normalized).unwrap_or(normalized);
        if !resolved.starts_with(&root) {
            anyhow::bail!("path escapes workspace root: {uri}");
        }
        Ok(resolved)
    }

    /// Collapse `.` and `..` components without touching the filesystem (for
    /// paths that may not exist yet, where `canonicalize` can't be used).
    fn lexically_normalize(path: &std::path::Path) -> PathBuf {
        let mut result = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    result.pop();
                }
                std::path::Component::CurDir => {}
                other => result.push(other),
            }
        }
        result
    }
}

/// Cursor-paginate a slice already sorted (ascending) by `key_of`. Drops every
/// item at or before the cursor, keeps `limit`, and returns a cursor pinned to
/// the last kept item when more remain. Callers serialize it via
/// [`Cursor::encode`].
pub(super) fn paginate<T, F>(items: Vec<T>, page: &Page, key_of: F) -> (Vec<T>, Option<Cursor>)
where
    F: Fn(&T) -> SortKey,
{
    let mut items = items;
    if let Some(cur) = &page.cursor {
        let key = cur.key.clone();
        items.retain(|t| key_of(t) > key);
    }
    let limit = page.limit.clamp(1, MAX_PAGE_LIMIT);
    let next = if items.len() > limit {
        Some(Cursor {
            key: key_of(&items[limit - 1]),
        })
    } else {
        None
    };
    items.truncate(limit);
    (items, next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn confine_allows_in_root_path() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("mod.py"), "x").unwrap();
        let root_uri = format!("file://{}", dir.path().display());
        let db = DbActor::spawn(&dir.path().join("g.db")).unwrap();
        let engine = QueryEngine::new(db, root_uri);

        let uri = format!("file://{}/mod.py", dir.path().display());
        let resolved = engine.confine(&uri).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap().join("mod.py"));
    }

    #[test]
    fn paginate_clamps_limit_to_max_page_limit() {
        let items: Vec<i64> = (0..(MAX_PAGE_LIMIT as i64 + 10)).collect();
        let page = Page {
            limit: MAX_PAGE_LIMIT + 1000,
            cursor: None,
        };
        let (page_items, next) = paginate(items, &page, |n| SortKey {
            fqn: format!("{n:06}"),
            uri: String::new(),
            start_line: 0,
            start_col: 0,
        });
        assert_eq!(page_items.len(), MAX_PAGE_LIMIT);
        assert!(next.is_some());
    }

    #[tokio::test]
    async fn confine_rejects_path_escaping_root() {
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let root_uri = format!("file://{}", workspace.display());
        let db = DbActor::spawn(&workspace.join("g.db")).unwrap();
        let engine = QueryEngine::new(db, root_uri);

        // Escapes the workspace root via `..`.
        let escaping_uri = format!("file://{}/../etc/passwd", workspace.display());
        assert!(engine.confine(&escaping_uri).is_err());
    }
}
