//! The index pipeline: discover files → fetch documentSymbols → UPSERT nodes
//! and `contains` edges. Sequential (one stdio connection; the server scans the
//! workspace itself), documentSymbol-only for 0.0.1
//! (`docs/design/indexing-and-cache.md`).

use anyhow::Result;

use crate::adapters::{LanguageAdapter, select_for_uri};
use crate::graph::{DbActor, Edge, Node};
use crate::indexer::{
    FlatSymbol, SymbolFetcher, discover_files, flatten_document_symbols, module_path_from_uri,
    signature_fingerprint, uri_to_path,
};

/// Aggregate counts from a single indexing pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files_seen: u64,
    pub files_indexed: u64,
    pub symbols_upserted: u64,
    pub fetch_errors: u64,
}

/// Index the `language` files under `root_uri`: discover, fetch symbols per file
/// via `fetcher` (bound to that language's LSP server), and persist nodes +
/// `contains` edges. Files of other languages are skipped — a real fetcher is
/// language-specific (pyright cannot answer a `.ts`). A file whose fetch fails
/// is counted and skipped so the rest of the workspace still indexes.
pub async fn index_repository(
    db: &DbActor,
    fetcher: &impl SymbolFetcher,
    root_uri: &str,
    language: &str,
) -> Result<IndexStats> {
    let root = uri_to_path(root_uri);
    let files = discover_files(&root)?;
    let mut stats = IndexStats::default();
    for uri in files {
        let Some(adapter) = select_for_uri(&uri) else {
            continue;
        };
        if adapter.language_name() != language {
            continue;
        }
        stats.files_seen += 1;
        let symbols = match fetcher.document_symbols(&uri).await {
            Ok(symbols) => symbols,
            Err(_) => {
                stats.fetch_errors += 1;
                continue;
            }
        };
        let module_path = module_path_from_uri(&uri, root_uri);
        let flat = flatten_document_symbols(&symbols, &module_path);
        upsert_symbol_tree(db, &flat, adapter, &uri, root_uri).await?;
        stats.files_indexed += 1;
        stats.symbols_upserted += flat.len() as u64;
    }
    Ok(stats)
}

/// Persist a flattened symbol tree: nodes first (threading each parent's DB id
/// into its children's `container_id`), then one `contains` edge per parent→child.
async fn upsert_symbol_tree(
    db: &DbActor,
    flat: &[FlatSymbol],
    adapter: &dyn LanguageAdapter,
    uri: &str,
    root_uri: &str,
) -> Result<()> {
    let language = adapter.language_name();
    let is_external = adapter.is_external(uri, root_uri);
    // Parents always precede children (depth-first pre-order), so each parent id
    // is known by the time we build its child's `container_id`.
    let mut ids = Vec::with_capacity(flat.len());
    for sym in flat {
        let container_id = sym.parent.and_then(|p| ids.get(p).copied());
        let node = Node {
            id: None,
            fqn: sym.fqn.clone(),
            uri: uri.to_string(),
            name: sym.name.clone(),
            language: language.to_string(),
            kind: sym.kind as i64,
            node_kind: adapter.map_symbol_kind(sym.kind).to_label(),
            construct: None,
            container_id,
            range: sym.range,
            sel: sym.sel,
            signature: None,
            documentation: None,
            detail: sym.detail.clone(),
            signature_hash: Some(signature_fingerprint(sym)),
            valid: true,
            orphan: false,
            generation: 0,
            is_external,
        };
        ids.push(db.upsert_node(node).await?);
    }
    for (i, sym) in flat.iter().enumerate() {
        if let Some(parent) = sym.parent {
            db.upsert_edge(Edge {
                id: None,
                src_id: ids[parent],
                dst_id: ids[i],
                edge_type: "contains".to_string(),
                site: None,
                valid: true,
            })
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{DocumentSymbol, LspPosition, LspRange, path_to_uri};
    use rusqlite::params;
    use std::collections::HashMap;
    use std::fs;
    use std::future::Future;
    use tempfile::tempdir;

    /// In-memory fetcher keyed by URI; returns a canned symbol list.
    struct MockFetcher(HashMap<String, Vec<DocumentSymbol>>);

    impl SymbolFetcher for MockFetcher {
        fn document_symbols(
            &self,
            uri: &str,
        ) -> impl Future<Output = Result<Vec<DocumentSymbol>>> + Send {
            let res = self
                .0
                .get(uri)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no mock for {uri}"));
            async move { res }
        }
    }

    fn pos(line: u32, ch: u32) -> LspPosition {
        LspPosition {
            line,
            character: ch,
        }
    }

    fn rng(sl: u32, sc: u32, el: u32, ec: u32) -> LspRange {
        LspRange {
            start: pos(sl, sc),
            end: pos(el, ec),
        }
    }

    fn root_uri_for(dir: &std::path::Path) -> String {
        format!("{}/", path_to_uri(dir).trim_end_matches('/'))
    }

    #[tokio::test]
    async fn index_repository_persists_nodes_and_contains_edges() {
        let dir = tempdir().expect("tempdir");
        let app = dir.path().join("app");
        fs::create_dir_all(&app).unwrap();
        let py = app.join("repo.py");
        fs::write(&py, "class Repo:\n    def load(self): ...\n").unwrap();

        let root_uri = root_uri_for(dir.path());
        let py_uri = path_to_uri(&py);

        let symbols = vec![DocumentSymbol {
            name: "Repo".into(),
            detail: None,
            kind: 5,
            tags: None,
            range: rng(0, 0, 3, 0),
            selection_range: rng(0, 6, 0, 10),
            children: Some(vec![DocumentSymbol {
                name: "load".into(),
                detail: None,
                kind: 12,
                tags: None,
                range: rng(1, 4, 2, 4),
                selection_range: rng(1, 8, 1, 12),
                children: Some(vec![]),
            }]),
        }];
        let mut map = HashMap::new();
        map.insert(py_uri, symbols);
        let fetcher = MockFetcher(map);

        let db_path = dir.path().join(".semnav").join("graph.db");
        fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let db = DbActor::spawn(&db_path).expect("spawn db");

        let stats = index_repository(&db, &fetcher, &root_uri, "python")
            .await
            .expect("index");
        assert_eq!(stats.files_seen, 1);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.symbols_upserted, 2);
        assert_eq!(stats.fetch_errors, 0);

        let parent = db
            .get_node_by_fqn("app.repo.Repo")
            .await
            .unwrap()
            .expect("Repo node");
        let child = db
            .get_node_by_fqn("app.repo.Repo.load")
            .await
            .unwrap()
            .expect("load node");
        assert_eq!(parent.name, "Repo");
        assert_eq!(parent.node_kind, "Class");
        assert_eq!(child.name, "load");
        assert_eq!(child.node_kind, "Function");
        assert_eq!(child.language, "python");
        assert_eq!(child.container_id, parent.id);
        assert!(!child.is_external);

        // The `contains` edge: a second WAL reader can inspect it concurrently.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let contains: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src_id=?1 AND dst_id=?2 AND edge_type='contains'",
                params![parent.id.unwrap(), child.id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(contains, 1, "exactly one contains edge parent→child");
    }

    #[tokio::test]
    async fn index_repository_counts_fetch_errors_and_continues() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("a.py"), "").unwrap();
        fs::write(dir.path().join("b.py"), "").unwrap();

        let root_uri = root_uri_for(dir.path());
        let b_uri = path_to_uri(&dir.path().join("b.py"));
        // Only b.py is mocked; a.py will fail to fetch.
        let mut map = HashMap::new();
        map.insert(
            b_uri,
            vec![DocumentSymbol {
                name: "f".into(),
                detail: None,
                kind: 12,
                tags: None,
                range: rng(0, 0, 1, 0),
                selection_range: rng(0, 4, 0, 5),
                children: Some(vec![]),
            }],
        );
        let fetcher = MockFetcher(map);

        let db_path = dir.path().join("graph.db");
        let db = DbActor::spawn(&db_path).expect("spawn db");

        let stats = index_repository(&db, &fetcher, &root_uri, "python")
            .await
            .expect("index");
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.fetch_errors, 1);
        assert_eq!(stats.symbols_upserted, 1);
    }
}
