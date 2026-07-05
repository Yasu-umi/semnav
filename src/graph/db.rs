//! The db actor — the single owner of the SQLite `Connection`. Commands arrive
//! over `mpsc`, replies return over `oneshot`. All reads/writes are serialized
//! on one blocking thread (`docs/design/crate-structure.md` Decision Point 4).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use anyhow::{Result, anyhow};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

use super::model::{Edge, Node, Range, ReconcileOutcome, ReconcileSymbol, Site};
use super::schema;

/// Rename-candidate matching tolerates at most this many lines of drift between
/// an old node's and a new symbol's start line (`docs/design/graph-model.md`
/// "Rename tracking").
const RENAME_MAX_LINE_DRIFT: i64 = 3;

/// Edge traversal direction for neighbor lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Edges pointing *into* `node_id` (`dst_id = node_id`): who calls/references it.
    Incoming,
    /// Edges pointing *out of* `node_id` (`src_id = node_id`): what it calls/references.
    Outgoing,
}

/// A neighbor result: the node on the other end of an edge, plus the located
/// occurrence (`None` for site-less edges like `contains`). One row per edge, so
/// the same node may recur with distinct sites — callers group as needed.
pub type Neighbor = (Node, Option<Site>);

/// A command the db actor runs on its owned `Connection`.
pub enum DbCommand {
    UpsertNode {
        node: Box<Node>,
        reply: oneshot::Sender<Result<i64>>,
    },
    GetNodeByFqn {
        fqn: String,
        reply: oneshot::Sender<Result<Option<Node>>>,
    },
    /// List all valid nodes (optionally narrowed to one language). 0.0.1 graphs
    /// are small and documentSymbol-only, so query tools fetch + filter in Rust
    /// rather than push the predicate into SQL.
    ListNodes {
        language: Option<String>,
        reply: oneshot::Sender<Result<Vec<Node>>>,
    },
    /// Resolve the most specific (smallest-range) valid node whose declaration
    /// range contains `(line, character)` in `uri`. Used by `SymbolRef::At`.
    FindNodeByPosition {
        uri: String,
        line: i64,
        col: i64,
        reply: oneshot::Sender<Result<Option<Node>>>,
    },
    /// Read the nodes connected to `node_id` via `edge_type`, in `direction`,
    /// each with its occurrence site. Ordered by the stable sort key so cursor
    /// pagination over the result is deterministic.
    GetNeighbors {
        node_id: i64,
        edge_type: String,
        direction: Direction,
        reply: oneshot::Sender<Result<Vec<Neighbor>>>,
    },
    UpsertEdge {
        edge: Edge,
        reply: oneshot::Sender<Result<i64>>,
    },
    /// Set an `index_meta` key/value (upsert). Carries the LSP health record
    /// (`<lang>.lsp_status` / `<lang>.lsp_last_success_at` /
    /// `<lang>.lsp_consecutive_failures`) and other small KV.
    SetMeta {
        key: String,
        value: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Read an `index_meta` value by key (`None` if absent).
    GetMeta {
        key: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    /// Diff freshly-fetched `symbols` for `uri` against its existing `nodes`
    /// rows and apply the result — update, rename (same row id so edges
    /// follow), insert, orphan, or delete — in one transaction
    /// (`docs/design/indexing-and-cache.md` "Cache Invalidation").
    ReconcileFileSymbols {
        uri: String,
        language: String,
        is_external: bool,
        symbols: Vec<ReconcileSymbol>,
        reply: oneshot::Sender<Result<ReconcileOutcome>>,
    },
    /// Persist a hover-derived `construct` refinement for an existing node
    /// (`docs/design/language-adapters.md` "Refinement via hover").
    UpdateNodeConstruct {
        id: i64,
        construct: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Persist a hover-derived `signature` for an existing node, by id
    /// (`find_definition`'s `at` branch, which already has the node loaded).
    UpdateNodeSignature {
        id: i64,
        signature: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Persist a hover-derived `signature` for an existing node, by `fqn` —
    /// used by the `with_signature` opt-in on `find_symbol`/`find_callers`/
    /// `find_callees` (`QueryRuntime`), which only has the result `NodeDto`
    /// (no numeric id) in hand.
    UpdateNodeSignatureByFqn {
        fqn: String,
        signature: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Read `anchor_id`'s cached outgoing-callees content hash, if any
    /// (`docs/design/lsp-integration.md` "callees precise cache").
    GetCalleesCacheHash {
        anchor_id: i64,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    /// Upsert `anchor_id`'s cached outgoing-callees content hash.
    SetCalleesCacheHash {
        anchor_id: i64,
        content_hash: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// `true` if `anchor_id` has been materialized at least once for
    /// `edge_type` (`docs/design/lsp-integration.md` "cache-first + background
    /// refresh").
    IsMaterialized {
        anchor_id: i64,
        edge_type: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    /// Mark `anchor_id` as materialized for `edge_type`. Idempotent.
    MarkMaterialized {
        anchor_id: i64,
        edge_type: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Invalidate `anchor_id`'s existing outgoing `calls` edges before a
    /// callees re-materialization (`docs/design/lsp-integration.md` "callees
    /// precise cache"). `materialize_call_edges` only upserts edges it
    /// discovers; without this, a callee removed since the last
    /// materialization would leave a stale, still-`valid` edge behind.
    InvalidateOutgoingCalls {
        anchor_id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Distinct `uri`s of every non-orphan node, i.e. every file the graph
    /// currently believes exists (`docs/design/daemon-lifecycle.md` "Startup
    /// drift reconciliation"). Used to catch deletions a startup drift pass
    /// wouldn't otherwise see, since a fresh `discover_files` walk only lists
    /// files that still exist on disk.
    KnownUris {
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
}

/// Handle to the db actor. Cheap to clone (just an `mpsc::Sender`).
#[derive(Clone)]
pub struct DbActor {
    tx: mpsc::Sender<DbCommand>,
}

impl DbActor {
    /// Open (or create) the database at `path`, apply pragmas + migrations,
    /// then spawn the single-`Connection` owner on a blocking thread.
    pub fn spawn(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        schema::migrations::runner()
            .run(&mut conn)
            .map_err(|e| anyhow!(e))?;

        let (tx, rx) = mpsc::channel::<DbCommand>(64);
        tokio::task::spawn_blocking(move || Self::run(conn, rx));
        Ok(Self { tx })
    }

    fn run(mut conn: Connection, mut rx: mpsc::Receiver<DbCommand>) {
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                DbCommand::UpsertNode { node, reply } => {
                    let res = upsert_node(&conn, &node);
                    let _ = reply.send(res);
                }
                DbCommand::GetNodeByFqn { fqn, reply } => {
                    let res = get_node_by_fqn(&conn, &fqn);
                    let _ = reply.send(res);
                }
                DbCommand::ListNodes { language, reply } => {
                    let res = list_nodes(&conn, language.as_deref());
                    let _ = reply.send(res);
                }
                DbCommand::FindNodeByPosition {
                    uri,
                    line,
                    col,
                    reply,
                } => {
                    let res = find_node_by_position(&conn, &uri, line, col);
                    let _ = reply.send(res);
                }
                DbCommand::GetNeighbors {
                    node_id,
                    edge_type,
                    direction,
                    reply,
                } => {
                    let res = get_neighbors(&conn, node_id, &edge_type, direction);
                    let _ = reply.send(res);
                }
                DbCommand::UpsertEdge { edge, reply } => {
                    let res = upsert_edge(&conn, &edge);
                    let _ = reply.send(res);
                }
                DbCommand::SetMeta { key, value, reply } => {
                    let res = set_meta(&conn, &key, &value);
                    let _ = reply.send(res);
                }
                DbCommand::GetMeta { key, reply } => {
                    let res = get_meta(&conn, &key);
                    let _ = reply.send(res);
                }
                DbCommand::ReconcileFileSymbols {
                    uri,
                    language,
                    is_external,
                    symbols,
                    reply,
                } => {
                    let res =
                        reconcile_file_symbols(&mut conn, &uri, &language, is_external, &symbols);
                    let _ = reply.send(res);
                }
                DbCommand::UpdateNodeConstruct {
                    id,
                    construct,
                    reply,
                } => {
                    let res = update_node_construct(&conn, id, &construct);
                    let _ = reply.send(res);
                }
                DbCommand::UpdateNodeSignature {
                    id,
                    signature,
                    reply,
                } => {
                    let res = update_node_signature(&conn, id, &signature);
                    let _ = reply.send(res);
                }
                DbCommand::UpdateNodeSignatureByFqn {
                    fqn,
                    signature,
                    reply,
                } => {
                    let res = update_node_signature_by_fqn(&conn, &fqn, &signature);
                    let _ = reply.send(res);
                }
                DbCommand::GetCalleesCacheHash { anchor_id, reply } => {
                    let res = get_callees_cache_hash(&conn, anchor_id);
                    let _ = reply.send(res);
                }
                DbCommand::SetCalleesCacheHash {
                    anchor_id,
                    content_hash,
                    reply,
                } => {
                    let res = set_callees_cache_hash(&conn, anchor_id, &content_hash);
                    let _ = reply.send(res);
                }
                DbCommand::IsMaterialized {
                    anchor_id,
                    edge_type,
                    reply,
                } => {
                    let res = is_materialized(&conn, anchor_id, &edge_type);
                    let _ = reply.send(res);
                }
                DbCommand::MarkMaterialized {
                    anchor_id,
                    edge_type,
                    reply,
                } => {
                    let res = mark_materialized(&conn, anchor_id, &edge_type);
                    let _ = reply.send(res);
                }
                DbCommand::InvalidateOutgoingCalls { anchor_id, reply } => {
                    let res = invalidate_outgoing_calls(&conn, anchor_id);
                    let _ = reply.send(res);
                }
                DbCommand::KnownUris { reply } => {
                    let res = known_uris(&conn);
                    let _ = reply.send(res);
                }
            }
        }
    }

    pub async fn upsert_node(&self, node: Node) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::UpsertNode {
                node: Box::new(node),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    pub async fn get_node_by_fqn(&self, fqn: &str) -> Result<Option<Node>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetNodeByFqn {
                fqn: fqn.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// List all valid nodes, optionally narrowed to one language.
    pub async fn list_nodes(&self, language: Option<&str>) -> Result<Vec<Node>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::ListNodes {
                language: language.map(str::to_string),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Resolve the most specific valid node whose declaration range contains the
    /// given (0-based) position in `uri`.
    pub async fn find_node_by_position(
        &self,
        uri: &str,
        line: i64,
        col: i64,
    ) -> Result<Option<Node>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::FindNodeByPosition {
                uri: uri.to_string(),
                line,
                col,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Read the neighbors of `node_id` along `edge_type` in `direction`.
    pub async fn get_neighbors(
        &self,
        node_id: i64,
        edge_type: &str,
        direction: Direction,
    ) -> Result<Vec<Neighbor>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetNeighbors {
                node_id,
                edge_type: edge_type.to_string(),
                direction,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Insert (or reuse) an edge, returning its row id. Idempotent on
    /// `(src, dst, type)` plus a NULL-safe site match (`docs/design/graph-model.md`).
    pub async fn upsert_edge(&self, edge: Edge) -> Result<i64> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::UpsertEdge { edge, reply: tx })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Upsert an `index_meta` key/value pair.
    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::SetMeta {
                key: key.to_string(),
                value: value.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Read an `index_meta` value, or `None` if the key is absent.
    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetMeta {
                key: key.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Reconcile `uri`'s existing `nodes` rows against a freshly-fetched
    /// `symbols` list — update, rename, insert, orphan, or delete as needed
    /// (`docs/design/indexing-and-cache.md` "Cache Invalidation"). Pass an
    /// empty `symbols` list to treat the file as deleted (all its nodes go
    /// through the orphan path).
    pub async fn reconcile_file_symbols(
        &self,
        uri: &str,
        language: &str,
        is_external: bool,
        symbols: Vec<ReconcileSymbol>,
    ) -> Result<ReconcileOutcome> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::ReconcileFileSymbols {
                uri: uri.to_string(),
                language: language.to_string(),
                is_external,
                symbols,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Persist a hover-derived `construct` refinement for an existing node.
    pub async fn update_node_construct(&self, id: i64, construct: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::UpdateNodeConstruct {
                id,
                construct: construct.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Persist a hover-derived `signature` for an existing node, by id.
    pub async fn update_node_signature(&self, id: i64, signature: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::UpdateNodeSignature {
                id,
                signature: signature.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Persist a hover-derived `signature` for an existing node, by `fqn`.
    pub async fn update_node_signature_by_fqn(&self, fqn: &str, signature: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::UpdateNodeSignatureByFqn {
                fqn: fqn.to_string(),
                signature: signature.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Read `anchor_id`'s cached outgoing-callees content hash, if any.
    pub async fn get_callees_cache_hash(&self, anchor_id: i64) -> Result<Option<String>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::GetCalleesCacheHash {
                anchor_id,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Upsert `anchor_id`'s cached outgoing-callees content hash.
    pub async fn set_callees_cache_hash(&self, anchor_id: i64, content_hash: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::SetCalleesCacheHash {
                anchor_id,
                content_hash: content_hash.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// `true` if `anchor_id` has been materialized at least once for `edge_type`.
    pub async fn is_materialized(&self, anchor_id: i64, edge_type: &str) -> Result<bool> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::IsMaterialized {
                anchor_id,
                edge_type: edge_type.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Mark `anchor_id` as materialized for `edge_type`. Idempotent.
    pub async fn mark_materialized(&self, anchor_id: i64, edge_type: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::MarkMaterialized {
                anchor_id,
                edge_type: edge_type.to_string(),
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Invalidate `anchor_id`'s existing outgoing `calls` edges ahead of a
    /// callees re-materialization.
    pub async fn invalidate_outgoing_calls(&self, anchor_id: i64) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::InvalidateOutgoingCalls {
                anchor_id,
                reply: tx,
            })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Distinct `uri`s of every non-orphan node — every file the graph
    /// currently believes exists.
    pub async fn known_uris(&self) -> Result<Vec<String>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(DbCommand::KnownUris { reply: tx })
            .await
            .map_err(|_| anyhow!("db actor closed"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // A second process touching this file (e.g. a `semnav index` re-run
    // while a `serve`/`daemon` process still holds it) retries on lock
    // contention for 5s instead of failing immediately with SQLITE_BUSY.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

const UPSERT_SQL: &str = r#"
    INSERT INTO nodes (
        fqn, uri, name, language, kind, node_kind, construct, container_id,
        range_start_line, range_start_col, range_end_line, range_end_col,
        sel_start_line, sel_start_col, sel_end_line, sel_end_col,
        signature, documentation, detail, signature_hash,
        valid, orphan, generation, is_external
    ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
    ON CONFLICT(fqn) DO UPDATE SET
        uri=excluded.uri, name=excluded.name, language=excluded.language,
        kind=excluded.kind, node_kind=excluded.node_kind, construct=excluded.construct,
        container_id=excluded.container_id,
        range_start_line=excluded.range_start_line, range_start_col=excluded.range_start_col,
        range_end_line=excluded.range_end_line, range_end_col=excluded.range_end_col,
        sel_start_line=excluded.sel_start_line, sel_start_col=excluded.sel_start_col,
        sel_end_line=excluded.sel_end_line, sel_end_col=excluded.sel_end_col,
        signature=excluded.signature, documentation=excluded.documentation, detail=excluded.detail,
        signature_hash=excluded.signature_hash,
        valid=excluded.valid, orphan=excluded.orphan, generation=excluded.generation,
        is_external=excluded.is_external
    ON CONFLICT(uri, range_start_line, range_start_col, name) DO UPDATE SET
        fqn=excluded.fqn, language=excluded.language,
        kind=excluded.kind, node_kind=excluded.node_kind, construct=excluded.construct,
        container_id=excluded.container_id,
        range_end_line=excluded.range_end_line, range_end_col=excluded.range_end_col,
        sel_start_line=excluded.sel_start_line, sel_start_col=excluded.sel_start_col,
        sel_end_line=excluded.sel_end_line, sel_end_col=excluded.sel_end_col,
        signature=excluded.signature, documentation=excluded.documentation, detail=excluded.detail,
        signature_hash=excluded.signature_hash,
        valid=excluded.valid, orphan=excluded.orphan, generation=excluded.generation,
        is_external=excluded.is_external
    RETURNING id
"#;

fn upsert_node(conn: &Connection, n: &Node) -> Result<i64> {
    let id = conn.query_row(
        UPSERT_SQL,
        params![
            n.fqn,
            n.uri,
            n.name,
            n.language,
            n.kind,
            n.node_kind,
            n.construct,
            n.container_id,
            n.range.start_line,
            n.range.start_col,
            n.range.end_line,
            n.range.end_col,
            n.sel.start_line,
            n.sel.start_col,
            n.sel.end_line,
            n.sel.end_col,
            n.signature,
            n.documentation,
            n.detail,
            n.signature_hash,
            n.valid,
            n.orphan,
            n.generation,
            n.is_external,
        ],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(id)
}

const SELECT_SQL: &str = r#"
    SELECT id, fqn, uri, name, language, kind, node_kind, construct, container_id,
           range_start_line, range_start_col, range_end_line, range_end_col,
           sel_start_line, sel_start_col, sel_end_line, sel_end_col,
           signature, documentation, detail, signature_hash,
           valid, orphan, generation, is_external
    FROM nodes WHERE fqn = ?1
"#;

fn get_node_by_fqn(conn: &Connection, fqn: &str) -> Result<Option<Node>> {
    let node = conn
        .query_row(SELECT_SQL, [fqn], node_from_row)
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(node)
}

fn node_from_row(row: &rusqlite::Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: row.get("id")?,
        fqn: row.get("fqn")?,
        uri: row.get("uri")?,
        name: row.get("name")?,
        language: row.get("language")?,
        kind: row.get("kind")?,
        node_kind: row.get("node_kind")?,
        construct: row.get("construct")?,
        container_id: row.get("container_id")?,
        range: Range {
            start_line: row.get("range_start_line")?,
            start_col: row.get("range_start_col")?,
            end_line: row.get("range_end_line")?,
            end_col: row.get("range_end_col")?,
        },
        sel: Range {
            start_line: row.get("sel_start_line")?,
            start_col: row.get("sel_start_col")?,
            end_line: row.get("sel_end_line")?,
            end_col: row.get("sel_end_col")?,
        },
        signature: row.get("signature")?,
        documentation: row.get("documentation")?,
        detail: row.get("detail")?,
        signature_hash: row.get("signature_hash")?,
        valid: row.get::<_, i64>("valid")? != 0,
        orphan: row.get::<_, i64>("orphan")? != 0,
        generation: row.get("generation")?,
        is_external: row.get::<_, i64>("is_external")? != 0,
    })
}

const LIST_NODES_SQL: &str = r#"
    SELECT id, fqn, uri, name, language, kind, node_kind, construct, container_id,
           range_start_line, range_start_col, range_end_line, range_end_col,
           sel_start_line, sel_start_col, sel_end_line, sel_end_col,
           signature, documentation, detail, signature_hash,
           valid, orphan, generation, is_external
    FROM nodes
    WHERE valid = 1
    ORDER BY fqn, uri, range_start_line, range_start_col
"#;

const LIST_NODES_BY_LANG_SQL: &str = r#"
    SELECT id, fqn, uri, name, language, kind, node_kind, construct, container_id,
           range_start_line, range_start_col, range_end_line, range_end_col,
           sel_start_line, sel_start_col, sel_end_line, sel_end_col,
           signature, documentation, detail, signature_hash,
           valid, orphan, generation, is_external
    FROM nodes
    WHERE valid = 1 AND language = ?1
    ORDER BY fqn, uri, range_start_line, range_start_col
"#;

/// List valid nodes, optionally narrowed to one language. Ordered by the stable
/// sort key so the query layer's cursor pagination is deterministic.
fn list_nodes(conn: &Connection, language: Option<&str>) -> Result<Vec<Node>> {
    let nodes = match language {
        Some(lang) => {
            let mut stmt = conn.prepare(LIST_NODES_BY_LANG_SQL)?;
            stmt.query_map(params![lang], node_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
        None => {
            let mut stmt = conn.prepare(LIST_NODES_SQL)?;
            stmt.query_map([], node_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        }
    };
    Ok(nodes)
}

const KNOWN_URIS_SQL: &str = "SELECT DISTINCT uri FROM nodes WHERE orphan = 0";

/// Distinct `uri`s of every non-orphan node.
fn known_uris(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(KNOWN_URIS_SQL)?;
    let uris = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(uris)
}

/// Resolve the smallest valid node whose declaration range contains `(line, col)`
/// in `uri`. Two-sided containment on `(line, col)`; ordered by span size so the
/// most specific (innermost) symbol wins.
const FIND_BY_POSITION_SQL: &str = r#"
    SELECT id, fqn, uri, name, language, kind, node_kind, construct, container_id,
           range_start_line, range_start_col, range_end_line, range_end_col,
           sel_start_line, sel_start_col, sel_end_line, sel_end_col,
           signature, documentation, detail, signature_hash,
           valid, orphan, generation, is_external
    FROM nodes
    WHERE uri = ?1 AND valid = 1
      AND (range_start_line < ?2 OR (range_start_line = ?2 AND range_start_col <= ?3))
      AND (range_end_line > ?2 OR (range_end_line = ?2 AND range_end_col >= ?3))
    ORDER BY (range_end_line - range_start_line) ASC,
             (range_end_col - range_start_col) ASC
    LIMIT 1
"#;

fn find_node_by_position(
    conn: &Connection,
    uri: &str,
    line: i64,
    col: i64,
) -> Result<Option<Node>> {
    let node = conn
        .query_row(FIND_BY_POSITION_SQL, params![uri, line, col], node_from_row)
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(node)
}

/// `Incoming` joins the neighbor as the edge *source* (`dst_id = node_id`);
/// `Outgoing` joins it as the edge *destination* (`src_id = node_id`). Reads the
/// node columns via `node_from_row` and the located `site_*` columns alongside.
fn get_neighbors(
    conn: &Connection,
    node_id: i64,
    edge_type: &str,
    direction: Direction,
) -> Result<Vec<Neighbor>> {
    let sql = match direction {
        Direction::Incoming => {
            r#"
            SELECT n.*, e.site_uri, e.site_start_line, e.site_start_col,
                   e.site_end_line, e.site_end_col
            FROM edges e JOIN nodes n ON n.id = e.src_id
            WHERE e.dst_id = ?1 AND e.edge_type = ?2 AND e.valid = 1 AND n.valid = 1
            ORDER BY n.fqn, n.uri, n.range_start_line, n.range_start_col
        "#
        }
        Direction::Outgoing => {
            r#"
            SELECT n.*, e.site_uri, e.site_start_line, e.site_start_col,
                   e.site_end_line, e.site_end_col
            FROM edges e JOIN nodes n ON n.id = e.dst_id
            WHERE e.src_id = ?1 AND e.edge_type = ?2 AND e.valid = 1 AND n.valid = 1
            ORDER BY n.fqn, n.uri, n.range_start_line, n.range_start_col
        "#
        }
    };
    let mut stmt = conn.prepare(sql)?;
    let neighbors = stmt
        .query_map(params![node_id, edge_type], neighbor_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(neighbors)
}

/// Read a neighbor row: the node columns (via [`node_from_row`]) plus the
/// optional `site_*` occurrence span from the joined edge.
fn neighbor_from_row(row: &rusqlite::Row) -> rusqlite::Result<Neighbor> {
    let node = node_from_row(row)?;
    let site_uri: Option<String> = row.get("site_uri")?;
    let site = match site_uri {
        Some(uri) => Some(Site {
            uri,
            range: Range {
                start_line: row.get("site_start_line")?,
                start_col: row.get("site_start_col")?,
                end_line: row.get("site_end_line")?,
                end_col: row.get("site_end_col")?,
            },
        }),
        None => None,
    };
    Ok((node, site))
}

const SELECT_EDGE_SQL: &str = r#"
    SELECT id FROM edges
    WHERE src_id = ?1 AND dst_id = ?2 AND edge_type = ?3
      AND site_uri IS ?4 AND site_start_line IS ?5 AND site_start_col IS ?6
"#;

const REACTIVATE_EDGE_SQL: &str = r#"
    UPDATE edges
    SET valid = ?2, site_end_line = ?3, site_end_col = ?4
    WHERE id = ?1
"#;

const INSERT_EDGE_SQL: &str = r#"
    INSERT INTO edges (
        src_id, dst_id, edge_type,
        site_uri, site_start_line, site_start_col, site_end_line, site_end_col,
        valid
    ) VALUES (?,?,?,?,?,?,?,?,?)
"#;

/// Idempotent edge upsert. Looks up by `(src, dst, type)` plus a NULL-safe site
/// comparison and reuses the row if present; otherwise inserts. We look-then-insert
/// instead of `ON CONFLICT` because SQLite treats multiple NULLs in a UNIQUE
/// constraint as distinct, so a re-indexed site-less `contains` edge would
/// otherwise duplicate. A matched row is refreshed to `e.valid`/the new site end —
/// otherwise an edge invalidated by `reconcile_file_symbols` (e.g. by a rename)
/// would stay permanently invalid, since a later on-demand re-materialization for
/// the same `(src, dst, type, site)` would just return the old, still-invalid id.
fn upsert_edge(conn: &Connection, e: &Edge) -> Result<i64> {
    let (site_uri, sl, sc, el, ec) = match &e.site {
        Some(s) => (
            Some(s.uri.as_str()),
            Some(s.range.start_line),
            Some(s.range.start_col),
            Some(s.range.end_line),
            Some(s.range.end_col),
        ),
        None => (None, None, None, None, None),
    };
    if let Some(id) = conn
        .query_row(
            SELECT_EDGE_SQL,
            params![e.src_id, e.dst_id, e.edge_type, site_uri, sl, sc],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        conn.execute(REACTIVATE_EDGE_SQL, params![id, e.valid, el, ec])?;
        return Ok(id);
    }
    conn.execute(
        INSERT_EDGE_SQL,
        params![
            e.src_id,
            e.dst_id,
            e.edge_type,
            site_uri,
            sl,
            sc,
            el,
            ec,
            e.valid,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

const UPSERT_META_SQL: &str = r#"
    INSERT INTO index_meta (key, value) VALUES (?, ?)
    ON CONFLICT(key) DO UPDATE SET value = excluded.value
"#;

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(UPSERT_META_SQL, params![key, value])?;
    Ok(())
}

fn update_node_construct(conn: &Connection, id: i64, construct: &str) -> Result<()> {
    conn.execute(
        "UPDATE nodes SET construct = ?1 WHERE id = ?2",
        params![construct, id],
    )?;
    Ok(())
}

fn update_node_signature(conn: &Connection, id: i64, signature: &str) -> Result<()> {
    conn.execute(
        "UPDATE nodes SET signature = ?1 WHERE id = ?2",
        params![signature, id],
    )?;
    Ok(())
}

fn update_node_signature_by_fqn(conn: &Connection, fqn: &str, signature: &str) -> Result<()> {
    conn.execute(
        "UPDATE nodes SET signature = ?1 WHERE fqn = ?2",
        params![signature, fqn],
    )?;
    Ok(())
}

const GET_CALLEES_CACHE_SQL: &str = "SELECT content_hash FROM callees_cache WHERE anchor_id = ?1";

fn get_callees_cache_hash(conn: &Connection, anchor_id: i64) -> Result<Option<String>> {
    let hash = conn
        .query_row(GET_CALLEES_CACHE_SQL, [anchor_id], |row| {
            row.get::<_, String>(0)
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(hash)
}

const SET_CALLEES_CACHE_SQL: &str = r#"
    INSERT INTO callees_cache (anchor_id, content_hash) VALUES (?1, ?2)
    ON CONFLICT(anchor_id) DO UPDATE SET content_hash = excluded.content_hash
"#;

fn set_callees_cache_hash(conn: &Connection, anchor_id: i64, content_hash: &str) -> Result<()> {
    conn.execute(SET_CALLEES_CACHE_SQL, params![anchor_id, content_hash])?;
    Ok(())
}

const IS_MATERIALIZED_SQL: &str =
    "SELECT 1 FROM materialized WHERE anchor_id = ?1 AND edge_type = ?2";

fn is_materialized(conn: &Connection, anchor_id: i64, edge_type: &str) -> Result<bool> {
    let warm = conn
        .query_row(IS_MATERIALIZED_SQL, params![anchor_id, edge_type], |row| {
            row.get::<_, i64>(0)
        })
        .optional()?
        .is_some();
    Ok(warm)
}

const MARK_MATERIALIZED_SQL: &str =
    "INSERT OR IGNORE INTO materialized (anchor_id, edge_type) VALUES (?1, ?2)";

fn mark_materialized(conn: &Connection, anchor_id: i64, edge_type: &str) -> Result<()> {
    conn.execute(MARK_MATERIALIZED_SQL, params![anchor_id, edge_type])?;
    Ok(())
}

const INVALIDATE_OUTGOING_CALLS_SQL: &str =
    "UPDATE edges SET valid = 0 WHERE src_id = ?1 AND edge_type = 'calls'";

fn invalidate_outgoing_calls(conn: &Connection, anchor_id: i64) -> Result<()> {
    conn.execute(INVALIDATE_OUTGOING_CALLS_SQL, params![anchor_id])?;
    Ok(())
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value FROM index_meta WHERE key = ?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(value)
}

const SELECT_BY_URI_SQL: &str = r#"
    SELECT id, fqn, uri, name, language, kind, node_kind, construct, container_id,
           range_start_line, range_start_col, range_end_line, range_end_col,
           sel_start_line, sel_start_col, sel_end_line, sel_end_col,
           signature, documentation, detail, signature_hash,
           valid, orphan, generation, is_external
    FROM nodes WHERE uri = ?1
"#;

/// All nodes for `uri`, valid or not — reconciliation must see orphaned rows
/// too, so a symbol that reappears can be revived instead of re-inserted.
fn nodes_by_uri(conn: &Connection, uri: &str) -> Result<Vec<Node>> {
    let mut stmt = conn.prepare(SELECT_BY_URI_SQL)?;
    let nodes = stmt
        .query_map([uri], node_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(nodes)
}

const UPDATE_NODE_SQL: &str = r#"
    UPDATE nodes SET
        fqn=?1, name=?2, kind=?3, node_kind=?4, container_id=?5,
        range_start_line=?6, range_start_col=?7, range_end_line=?8, range_end_col=?9,
        sel_start_line=?10, sel_start_col=?11, sel_end_line=?12, sel_end_col=?13,
        detail=?14, signature_hash=?15, valid=1, orphan=0
    WHERE id=?16
"#;

const INVALIDATE_EDGES_SQL: &str = "UPDATE edges SET valid = 0 WHERE src_id = ?1 OR dst_id = ?1";
// Restores `range_start_line` (see the Pass-3 sentinel parking above) to its
// true pre-reconcile value — an orphaned-but-not-deleted row stays eligible
// for Pass 2's line-drift rename/revival heuristic on a future reconcile,
// which needs its real position, not the transient sentinel.
const ORPHAN_NODE_SQL: &str =
    "UPDATE nodes SET orphan = 1, valid = 0, range_start_line = ?2 WHERE id = ?1";
const DELETE_NODE_SQL: &str = "DELETE FROM nodes WHERE id = ?1";

/// The trailing-name-stripped fqn (`a.b.C.method` -> `a.b.C`), used to require
/// a rename candidate's container to be unchanged — a symbol that moved to a
/// different class/module is treated as new+orphan, not a rename.
fn container_prefix(fqn: &str) -> &str {
    fqn.rsplit_once('.').map(|(prefix, _)| prefix).unwrap_or("")
}

/// Ephemeral (unpersisted) fingerprint used only to score rename candidates —
/// same shape as `signature_fingerprint` (`src/indexer/symbol.rs`) minus
/// `name`, since a rename by definition changes the name.
fn body_fingerprint(kind: i64, detail: &Option<String>, range: &Range, sel: &Range) -> u64 {
    let mut h = DefaultHasher::new();
    kind.hash(&mut h);
    detail.hash(&mut h);
    span(range).hash(&mut h);
    span(sel).hash(&mut h);
    h.finish()
}

fn span(r: &Range) -> (i64, i64, i64, i64) {
    (r.start_line, r.start_col, r.end_line, r.end_col)
}

/// A scored rename candidate pairing an unclaimed old node with an unclaimed
/// incoming symbol.
struct RenameCandidate {
    /// `1` when the body fingerprint matches (confident), `0` otherwise (probable).
    tier: u8,
    line_drift: i64,
    old_idx: usize,
    sym_idx: usize,
}

fn reconcile_file_symbols(
    conn: &mut Connection,
    uri: &str,
    language: &str,
    is_external: bool,
    symbols: &[ReconcileSymbol],
) -> Result<ReconcileOutcome> {
    let tx = conn.transaction()?;
    let outcome = reconcile_file_symbols_tx(&tx, uri, language, is_external, symbols)?;
    tx.commit()?;
    Ok(outcome)
}

fn reconcile_file_symbols_tx(
    tx: &Connection,
    uri: &str,
    language: &str,
    is_external: bool,
    symbols: &[ReconcileSymbol],
) -> Result<ReconcileOutcome> {
    // The callees cache is keyed on file content, not `signature_hash` (which
    // under-fires on a same-line-count body edit — see
    // `docs/design/lsp-integration.md` "callees precise cache"), so every
    // node in this uri's cache is dropped unconditionally on each reconcile.
    tx.execute(
        "DELETE FROM callees_cache WHERE anchor_id IN (SELECT id FROM nodes WHERE uri = ?1)",
        params![uri],
    )?;

    let old_nodes = nodes_by_uri(tx, uri)?;

    // Park every existing row's physical key (`idx_nodes_phys`) on a sentinel
    // derived from its own (globally unique) id before Pass 3 starts writing
    // real target positions. `idx_nodes_phys` only keys on (uri, line, col,
    // name) — not fqn or kind — so two unrelated same-named symbols (e.g.
    // `Foo::new`/`Bar::new`) that swap positions across a single reconcile
    // (one moving onto the line the other used to occupy) would otherwise hit
    // a real `UNIQUE constraint failed` mid-loop: `UPDATE_NODE_SQL` below has
    // no `ON CONFLICT` fallback (unlike `UPSERT_SQL`'s insert path), because a
    // plain `UPDATE` can't declare one for a non-`fqn` unique index. Parking
    // first makes every row's key trivially unique (ids never collide) before
    // any row is moved to a real, potentially-still-occupied target, so
    // ordering within Pass 3 can no longer matter. Negative lines are outside
    // any real symbol's range and only ever observed transiently within this
    // transaction — Pass 3/4 below give every parked row a real final value
    // (or restore its original one; see `ORPHAN_NODE_SQL`) before commit.
    if !old_nodes.is_empty() {
        tx.execute(
            "UPDATE nodes SET range_start_line = -id WHERE uri = ?1",
            params![uri],
        )?;
    }

    let mut old_used = vec![false; old_nodes.len()];
    let mut assigned: Vec<Option<usize>> = vec![None; symbols.len()];

    // Pass 1: exact fqn match.
    for (i, sym) in symbols.iter().enumerate() {
        for (j, old) in old_nodes.iter().enumerate() {
            if !old_used[j] && old.fqn == sym.fqn {
                assigned[i] = Some(j);
                old_used[j] = true;
                break;
            }
        }
    }

    // Pass 2: rename candidates among what's left, scored and greedily
    // assigned best-first (`docs/design/graph-model.md` "Rename tracking").
    let mut candidates = Vec::new();
    for (i, sym) in symbols.iter().enumerate() {
        if assigned[i].is_some() {
            continue;
        }
        for (j, old) in old_nodes.iter().enumerate() {
            if old_used[j] {
                continue;
            }
            if old.kind != sym.kind {
                continue;
            }
            if container_prefix(&old.fqn) != container_prefix(&sym.fqn) {
                continue;
            }
            let line_drift = (old.range.start_line - sym.range.start_line).abs();
            if line_drift > RENAME_MAX_LINE_DRIFT {
                continue;
            }
            let tier = if body_fingerprint(old.kind, &old.detail, &old.range, &old.sel)
                == body_fingerprint(sym.kind, &sym.detail, &sym.range, &sym.sel)
            {
                1
            } else {
                0
            };
            candidates.push(RenameCandidate {
                tier,
                line_drift,
                old_idx: j,
                sym_idx: i,
            });
        }
    }
    candidates.sort_by(|a, b| {
        b.tier
            .cmp(&a.tier)
            .then(a.line_drift.cmp(&b.line_drift))
            .then(a.old_idx.cmp(&b.old_idx))
    });
    for c in candidates {
        if old_used[c.old_idx] || assigned[c.sym_idx].is_some() {
            continue;
        }
        assigned[c.sym_idx] = Some(c.old_idx);
        old_used[c.old_idx] = true;
    }

    // Pass 3: apply per-symbol in parent-before-child order, threading each
    // winning id into its children's `container_id` (mirrors
    // `pipeline.rs::upsert_symbol_tree`).
    let mut ids: Vec<Option<i64>> = vec![None; symbols.len()];
    let mut outcome = ReconcileOutcome::default();
    for (i, sym) in symbols.iter().enumerate() {
        let container_id = match sym.parent {
            Some(p) => Some(ids[p].ok_or_else(|| anyhow!("parent processed before child"))?),
            None => None,
        };
        match assigned[i] {
            Some(j) => {
                let old = &old_nodes[j];
                let old_id = old
                    .id
                    .ok_or_else(|| anyhow!("node read from db has an id"))?;
                let is_rename = old.fqn != sym.fqn;
                let hash_changed =
                    old.signature_hash.as_deref() != Some(sym.signature_hash.as_str());
                tx.execute(
                    UPDATE_NODE_SQL,
                    params![
                        sym.fqn,
                        sym.name,
                        sym.kind,
                        sym.node_kind,
                        container_id,
                        sym.range.start_line,
                        sym.range.start_col,
                        sym.range.end_line,
                        sym.range.end_col,
                        sym.sel.start_line,
                        sym.sel.start_col,
                        sym.sel.end_line,
                        sym.sel.end_col,
                        sym.detail,
                        sym.signature_hash,
                        old_id,
                    ],
                )?;
                if is_rename {
                    outcome.renamed += 1;
                    tx.execute(INVALIDATE_EDGES_SQL, params![old_id])?;
                } else if hash_changed {
                    outcome.updated += 1;
                    tx.execute(INVALIDATE_EDGES_SQL, params![old_id])?;
                } else {
                    outcome.unchanged += 1;
                }
                ids[i] = Some(old_id);
            }
            None => {
                let node = Node {
                    id: None,
                    fqn: sym.fqn.clone(),
                    uri: uri.to_string(),
                    name: sym.name.clone(),
                    language: language.to_string(),
                    kind: sym.kind,
                    node_kind: sym.node_kind.clone(),
                    construct: None,
                    container_id,
                    range: sym.range,
                    sel: sym.sel,
                    signature: None,
                    documentation: None,
                    detail: sym.detail.clone(),
                    signature_hash: Some(sym.signature_hash.clone()),
                    valid: true,
                    orphan: false,
                    generation: 0,
                    is_external,
                };
                ids[i] = Some(upsert_node(tx, &node)?);
                outcome.inserted += 1;
            }
        }
    }

    // Pass 4: unclaimed old nodes — two-strike orphan reclamation.
    for (j, old) in old_nodes.iter().enumerate() {
        if old_used[j] {
            continue;
        }
        let old_id = old
            .id
            .ok_or_else(|| anyhow!("node read from db has an id"))?;
        if old.orphan {
            tx.execute(DELETE_NODE_SQL, params![old_id])?;
            outcome.deleted += 1;
        } else {
            tx.execute(ORPHAN_NODE_SQL, params![old_id, old.range.start_line])?;
            outcome.orphaned += 1;
        }
    }

    // Pass 5: `contains` edges (idempotent, so re-reconciling never duplicates).
    for (i, sym) in symbols.iter().enumerate() {
        if let Some(p) = sym.parent {
            upsert_edge(
                tx,
                &Edge {
                    id: None,
                    src_id: ids[p].ok_or_else(|| anyhow!("parent processed before child"))?,
                    dst_id: ids[i].ok_or_else(|| anyhow!("this symbol was just assigned an id"))?,
                    edge_type: "contains".to_string(),
                    site: None,
                    valid: true,
                },
            )?;
        }
    }

    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Site;
    use tempfile::tempdir;

    #[test]
    fn apply_pragmas_sets_busy_timeout() {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        let busy_timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5000);
    }

    fn sample_node(fqn: &str) -> Node {
        Node {
            id: None,
            fqn: fqn.to_string(),
            uri: "file:///app/mod.py".to_string(),
            name: "sample".to_string(),
            language: "python".to_string(),
            kind: 12, // Function
            node_kind: "function".to_string(),
            construct: None,
            container_id: None,
            range: Range {
                start_line: 1,
                start_col: 0,
                end_line: 3,
                end_col: 0,
            },
            sel: Range {
                start_line: 1,
                start_col: 4,
                end_line: 1,
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

    #[tokio::test]
    async fn upsert_then_read_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        let node = sample_node("app.sample");
        let id = actor.upsert_node(node.clone()).await.expect("upsert");
        assert!(id > 0);

        let got = actor
            .get_node_by_fqn(&node.fqn)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(got.id, Some(id));
        assert_eq!(got.fqn, node.fqn);
        assert_eq!(got.range, node.range);
        assert_eq!(got.sel, node.sel);
        assert_eq!(got.kind, node.kind);
        assert_eq!(got.node_kind, node.node_kind);
        assert!(got.valid);
        assert!(!got.orphan);
        assert!(!got.is_external);
    }

    #[tokio::test]
    async fn update_node_signature_persists_by_id() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        let id = actor
            .upsert_node(sample_node("app.sample"))
            .await
            .expect("upsert");
        actor
            .update_node_signature(id, "def sample() -> None")
            .await
            .expect("update");

        let got = actor
            .get_node_by_fqn("app.sample")
            .await
            .expect("get")
            .expect("row");
        assert_eq!(got.signature.as_deref(), Some("def sample() -> None"));
    }

    #[tokio::test]
    async fn update_node_signature_by_fqn_persists() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        actor
            .upsert_node(sample_node("app.sample"))
            .await
            .expect("upsert");
        actor
            .update_node_signature_by_fqn("app.sample", "def sample() -> None")
            .await
            .expect("update");

        let got = actor
            .get_node_by_fqn("app.sample")
            .await
            .expect("get")
            .expect("row");
        assert_eq!(got.signature.as_deref(), Some("def sample() -> None"));
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_fqn() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        let mut node = sample_node("app.dup");
        let id1 = actor.upsert_node(node.clone()).await.expect("upsert1");

        // Same fqn, different kind → ON CONFLICT(fqn) must reuse the row id.
        node.kind = 5; // Class
        let id2 = actor.upsert_node(node.clone()).await.expect("upsert2");
        assert_eq!(id1, id2);

        let got = actor
            .get_node_by_fqn(&node.fqn)
            .await
            .expect("get")
            .expect("row");
        assert_eq!(got.kind, 5, "re-upsert should update the row");
    }

    #[tokio::test]
    async fn upsert_is_idempotent_on_physical_position() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        let first = node_with("app.old_name", "thing", 10);
        let id1 = actor.upsert_node(first).await.expect("upsert1");

        // Same (uri, range_start_line, range_start_col, name) but a different
        // fqn — a rename discovered at the same physical position must upsert
        // via `idx_nodes_phys`, not raise a UNIQUE constraint violation.
        let mut renamed = node_with("app.new_name", "thing", 10);
        renamed.kind = 5; // Class
        let id2 = actor.upsert_node(renamed).await.expect("upsert2");
        assert_eq!(id1, id2);

        let got = actor
            .get_node_by_fqn("app.new_name")
            .await
            .expect("get")
            .expect("row");
        assert_eq!(got.id, Some(id1));
        assert_eq!(got.kind, 5, "re-upsert should update the row");

        let old = actor.get_node_by_fqn("app.old_name").await.expect("get");
        assert!(old.is_none(), "old fqn must no longer resolve");
    }

    #[tokio::test]
    async fn get_missing_fqn_returns_none() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");
        let got = actor.get_node_by_fqn("does.not.exist").await.expect("get");
        assert!(got.is_none());
    }

    /// `sample_node` but with a distinct physical identity (`name` + span), so
    /// two siblings do not collide on the `idx_nodes_phys` unique index.
    fn node_with(fqn: &str, name: &str, line: i64) -> Node {
        let mut n = sample_node(fqn);
        n.name = name.to_string();
        n.range = Range {
            start_line: line,
            start_col: 0,
            end_line: line + 2,
            end_col: 0,
        };
        n.sel = Range {
            start_line: line,
            start_col: 4,
            end_line: line,
            end_col: 4 + name.len() as i64,
        };
        n
    }

    /// A `ReconcileSymbol` matching `node_with`'s physical-identity conventions
    /// (span derived from `line` and `name`), so hand-built rename/match
    /// scenarios line up with nodes seeded via `node_with`.
    fn sym(
        fqn: &str,
        name: &str,
        kind: i64,
        node_kind: &str,
        line: i64,
        signature_hash: &str,
        parent: Option<usize>,
    ) -> ReconcileSymbol {
        ReconcileSymbol {
            fqn: fqn.to_string(),
            name: name.to_string(),
            kind,
            node_kind: node_kind.to_string(),
            range: Range {
                start_line: line,
                start_col: 0,
                end_line: line + 2,
                end_col: 0,
            },
            sel: Range {
                start_line: line,
                start_col: 4,
                end_line: line,
                end_col: 4 + name.len() as i64,
            },
            detail: None,
            signature_hash: signature_hash.to_string(),
            parent,
        }
    }

    #[tokio::test]
    async fn reconcile_unchanged_content_is_noop() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut node = node_with("app.mod.helper", "helper", 1);
        node.signature_hash = Some("hash1".to_string());
        let id = actor.upsert_node(node).await.unwrap();

        let incoming = vec![sym(
            "app.mod.helper",
            "helper",
            12,
            "function",
            1,
            "hash1",
            None,
        )];
        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, incoming)
            .await
            .expect("reconcile");
        assert_eq!(
            outcome,
            ReconcileOutcome {
                unchanged: 1,
                ..Default::default()
            }
        );

        let got = actor
            .get_node_by_fqn("app.mod.helper")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.id, Some(id));
        assert!(got.valid);
        assert!(!got.orphan);
    }

    /// Two same-named methods on different types (`Foo::new`/`Bar::new` — a
    /// common pattern, `idx_nodes_phys` only keys on `name`, not `fqn`) swap
    /// which one comes first in the file. `Foo::new` moves down onto the line
    /// `Bar::new` currently occupies; `Bar::new` moves further down. Applying
    /// updates one row at a time in symbol order means `Foo::new`'s row is
    /// written to `Bar::new`'s still-unmoved physical key before `Bar::new`'s
    /// own row gets out of the way — a real edit pattern this session
    /// actually hit (`semnav`'s own `src/graph/db.rs`, causing every
    /// subsequent watcher reconcile of that file to fail with `UNIQUE
    /// constraint failed` until this was fixed).
    #[tokio::test]
    async fn reconcile_survives_a_same_name_position_swap() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut foo_new = node_with("app.Foo.new", "new", 0);
        foo_new.signature_hash = Some("hashFoo".to_string());
        actor.upsert_node(foo_new).await.unwrap();
        let mut bar_new = node_with("app.Bar.new", "new", 5);
        bar_new.signature_hash = Some("hashBar".to_string());
        actor.upsert_node(bar_new).await.unwrap();

        // Same fqns (exact-match in Pass 1), but `Foo::new` now sits where
        // `Bar::new` used to be, and `Bar::new` moved further down — in
        // symbol/document order, `Foo::new` (now first) is processed before
        // `Bar::new` vacates line 5.
        let incoming = vec![
            sym("app.Foo.new", "new", 12, "function", 5, "hashFoo", None),
            sym("app.Bar.new", "new", 12, "function", 10, "hashBar", None),
        ];
        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, incoming)
            .await
            .expect("reconcile survives the transient physical-key collision");
        assert_eq!(
            outcome,
            ReconcileOutcome {
                unchanged: 2,
                ..Default::default()
            },
            "same fqn/hash, only position moved — both read as unchanged"
        );

        let foo = actor.get_node_by_fqn("app.Foo.new").await.unwrap().unwrap();
        let bar = actor.get_node_by_fqn("app.Bar.new").await.unwrap().unwrap();
        assert_eq!(foo.range.start_line, 5);
        assert_eq!(bar.range.start_line, 10);
    }

    #[tokio::test]
    async fn reconcile_content_changed_invalidates_edges() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db_path).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut caller = node_with("app.mod.caller", "caller", 10);
        caller.signature_hash = Some("hashCaller".to_string());
        let caller_id = actor.upsert_node(caller).await.unwrap();

        let mut target = node_with("app.mod.helper", "helper", 1);
        target.signature_hash = Some("hash1".to_string());
        let target_id = actor.upsert_node(target).await.unwrap();

        actor
            .upsert_edge(Edge {
                id: None,
                src_id: caller_id,
                dst_id: target_id,
                edge_type: "calls".to_string(),
                site: None,
                valid: true,
            })
            .await
            .unwrap();

        let incoming = vec![
            sym(
                "app.mod.caller",
                "caller",
                12,
                "function",
                10,
                "hashCaller",
                None,
            ),
            sym("app.mod.helper", "helper", 12, "function", 1, "hash2", None),
        ];
        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, incoming)
            .await
            .expect("reconcile");
        assert_eq!(outcome.unchanged, 1, "caller's hash matches");
        assert_eq!(outcome.updated, 1, "helper's hash changed");

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let valid: i64 = conn
            .query_row(
                "SELECT valid FROM edges WHERE src_id=?1 AND dst_id=?2",
                params![caller_id, target_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(valid, 0, "edge into the changed node must be invalidated");
    }

    #[tokio::test]
    async fn reconcile_rename_preserves_id_and_edge_row() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db_path).expect("spawn");
        let uri = "file:///app/mod.py";

        let old = node_with("app.mod.old_name", "old_name", 1);
        let old_id = actor.upsert_node(old).await.unwrap();
        let caller_id = actor
            .upsert_node(node_with("app.mod.caller", "caller", 10))
            .await
            .unwrap();
        actor
            .upsert_edge(Edge {
                id: None,
                src_id: caller_id,
                dst_id: old_id,
                edge_type: "calls".to_string(),
                site: None,
                valid: true,
            })
            .await
            .unwrap();

        // Same kind, same container, same span (only the name changed) — a
        // confident (tier-1) rename candidate.
        let incoming = vec![
            sym(
                "app.mod.caller",
                "caller",
                12,
                "function",
                10,
                "hashCaller",
                None,
            ),
            sym(
                "app.mod.new_name",
                "new_name",
                12,
                "function",
                1,
                "hash1",
                None,
            ),
        ];
        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, incoming)
            .await
            .expect("reconcile");
        assert_eq!(outcome.renamed, 1);

        assert!(
            actor
                .get_node_by_fqn("app.mod.old_name")
                .await
                .unwrap()
                .is_none()
        );
        let renamed = actor
            .get_node_by_fqn("app.mod.new_name")
            .await
            .unwrap()
            .expect("renamed node");
        assert_eq!(renamed.id, Some(old_id), "rename preserves the row id");

        // The edge row survives the rename (it's an UPDATE, not delete+insert),
        // so it "follows" the renamed node with zero edge rewrites — even
        // though it's marked invalid pending self-heal.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src_id=?1 AND dst_id=?2",
                params![caller_id, old_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 1, "the edge row must not be deleted by a rename");
    }

    #[tokio::test]
    async fn reconcile_new_symbol_inserts_with_contains_edge() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db_path).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut parent = node_with("app.mod.Repo", "Repo", 0);
        parent.kind = 5;
        parent.node_kind = "class".to_string();
        parent.signature_hash = Some("classhash".to_string());
        let parent_id = actor.upsert_node(parent).await.unwrap();

        let incoming = vec![
            sym("app.mod.Repo", "Repo", 5, "class", 0, "classhash", None),
            sym(
                "app.mod.Repo.load",
                "load",
                12,
                "function",
                1,
                "newhash",
                Some(0),
            ),
        ];
        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, incoming)
            .await
            .expect("reconcile");
        assert_eq!(outcome.unchanged, 1, "Repo is unchanged");
        assert_eq!(outcome.inserted, 1, "load is new");

        let child = actor
            .get_node_by_fqn("app.mod.Repo.load")
            .await
            .unwrap()
            .expect("load node");
        assert_eq!(child.container_id, Some(parent_id));

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let contains: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE src_id=?1 AND dst_id=?2 AND edge_type='contains'",
                params![parent_id, child.id.unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(contains, 1);
    }

    #[tokio::test]
    async fn reconcile_missing_symbol_first_strike_is_grace_period() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let uri = "file:///app/mod.py";
        let id = actor
            .upsert_node(node_with("app.mod.gone", "gone", 1))
            .await
            .unwrap();

        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, vec![])
            .await
            .expect("reconcile");
        assert_eq!(outcome.orphaned, 1);
        assert_eq!(outcome.deleted, 0);

        let got = actor
            .get_node_by_fqn("app.mod.gone")
            .await
            .unwrap()
            .expect("row still present during grace period");
        assert_eq!(got.id, Some(id));
        assert!(got.orphan);
        assert!(!got.valid);
    }

    #[tokio::test]
    async fn reconcile_missing_symbol_second_strike_deletes_and_cascades_edges() {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db_path).expect("spawn");
        let uri = "file:///app/mod.py";

        let gone_id = actor
            .upsert_node(node_with("app.mod.gone", "gone", 1))
            .await
            .unwrap();
        let other_id = actor
            .upsert_node(node_with("app.mod.other", "other", 10))
            .await
            .unwrap();
        actor
            .upsert_edge(Edge {
                id: None,
                src_id: other_id,
                dst_id: gone_id,
                edge_type: "calls".to_string(),
                site: None,
                valid: true,
            })
            .await
            .unwrap();

        let still_here = vec![sym(
            "app.mod.other",
            "other",
            12,
            "function",
            10,
            "hash-other",
            None,
        )];
        actor
            .reconcile_file_symbols(uri, "python", false, still_here.clone())
            .await
            .expect("strike 1");

        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, still_here)
            .await
            .expect("strike 2");
        assert_eq!(outcome.deleted, 1);

        assert!(
            actor
                .get_node_by_fqn("app.mod.gone")
                .await
                .unwrap()
                .is_none()
        );

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE dst_id=?1",
                params![gone_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 0, "cascade delete removes the edge too");
    }

    #[tokio::test]
    async fn reconcile_orphan_revives_when_symbol_reappears() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut node = node_with("app.mod.helper", "helper", 1);
        node.signature_hash = Some("hash1".to_string());
        let id = actor.upsert_node(node).await.unwrap();

        actor
            .reconcile_file_symbols(uri, "python", false, vec![])
            .await
            .expect("strike 1");
        let orphaned = actor
            .get_node_by_fqn("app.mod.helper")
            .await
            .unwrap()
            .unwrap();
        assert!(orphaned.orphan);
        assert!(!orphaned.valid);

        let outcome = actor
            .reconcile_file_symbols(
                uri,
                "python",
                false,
                vec![sym(
                    "app.mod.helper",
                    "helper",
                    12,
                    "function",
                    1,
                    "hash1",
                    None,
                )],
            )
            .await
            .expect("revival");
        assert_eq!(outcome.unchanged, 1);

        let revived = actor
            .get_node_by_fqn("app.mod.helper")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(revived.id, Some(id));
        assert!(revived.valid);
        assert!(!revived.orphan);
    }

    #[tokio::test]
    async fn upsert_contains_edge_is_idempotent() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        let parent = actor
            .upsert_node(node_with("app.Parent", "Parent", 1))
            .await
            .expect("upsert parent");
        let child = actor
            .upsert_node(node_with("app.Parent.method", "method", 3))
            .await
            .expect("upsert child");

        let edge = Edge {
            id: None,
            src_id: parent,
            dst_id: child,
            edge_type: "contains".to_string(),
            site: None,
            valid: true,
        };
        let id1 = actor
            .upsert_edge(edge.clone())
            .await
            .expect("upsert edge 1");
        let id2 = actor
            .upsert_edge(edge.clone())
            .await
            .expect("upsert edge 2");
        assert_eq!(id1, id2, "re-inserting a contains edge reuses the row");
    }

    #[tokio::test]
    async fn upsert_edge_distinguishes_by_site() {
        let dir = tempdir().expect("tempdir");
        let db = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db).expect("spawn");

        let a = actor
            .upsert_node(node_with("app.A", "A", 1))
            .await
            .expect("upsert a");
        let b = actor
            .upsert_node(node_with("app.B", "B", 5))
            .await
            .expect("upsert b");

        let site = |line: i64| Site {
            uri: "file:///app/m.py".to_string(),
            range: Range {
                start_line: line,
                start_col: 2,
                end_line: line,
                end_col: 6,
            },
        };
        let make = |site: Option<Site>| Edge {
            id: None,
            src_id: a,
            dst_id: b,
            edge_type: "calls".to_string(),
            site,
            valid: true,
        };

        let id1 = actor
            .upsert_edge(make(Some(site(10))))
            .await
            .expect("edge1");
        let id2 = actor
            .upsert_edge(make(Some(site(20))))
            .await
            .expect("edge2");
        assert_ne!(id1, id2, "different sites are distinct edges");

        // Same (src, dst, type, site) reuses the row.
        let id1b = actor
            .upsert_edge(make(Some(site(10))))
            .await
            .expect("edge1 again");
        assert_eq!(id1, id1b, "same site reuses the row");
    }

    #[tokio::test]
    async fn upsert_edge_after_rename_reactivates_the_invalidated_row() {
        // Reproduces the FS-watcher rename flow end to end: a rename invalidates
        // the renamed node's edges as a cache-only-mode safety net
        // (`reconcile_rename_preserves_id_and_edge_row`); a live LSP client is
        // then expected to self-heal by re-upserting the same (src, dst, type,
        // site) edge on the next `find_references`/`find_callers` call, and
        // `get_neighbors` must see it as valid again. Before the `upsert_edge`
        // fix (which only looked up a matching row and returned its id without
        // refreshing `valid`), the edge stayed permanently invisible.
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("graph.db");
        let actor = DbActor::spawn(&db_path).expect("spawn");
        let uri = "file:///app/mod.py";

        let old_id = actor
            .upsert_node(node_with("app.mod.old_name", "old_name", 1))
            .await
            .unwrap();
        let caller_id = actor
            .upsert_node(node_with("app.mod.caller", "caller", 10))
            .await
            .unwrap();
        let site = Site {
            uri: uri.to_string(),
            range: Range {
                start_line: 11,
                start_col: 4,
                end_line: 11,
                end_col: 12,
            },
        };
        let edge = Edge {
            id: None,
            src_id: caller_id,
            dst_id: old_id,
            edge_type: "references".to_string(),
            site: Some(site),
            valid: true,
        };
        let edge_id = actor.upsert_edge(edge.clone()).await.unwrap();

        let incoming = vec![
            sym(
                "app.mod.caller",
                "caller",
                12,
                "function",
                10,
                "hashCaller",
                None,
            ),
            sym(
                "app.mod.new_name",
                "new_name",
                12,
                "function",
                1,
                "hash1",
                None,
            ),
        ];
        let outcome = actor
            .reconcile_file_symbols(uri, "python", false, incoming)
            .await
            .expect("reconcile");
        assert_eq!(outcome.renamed, 1);

        let during = actor
            .get_neighbors(old_id, "references", Direction::Incoming)
            .await
            .unwrap();
        assert!(
            during.is_empty(),
            "rename invalidates the edge pending self-heal"
        );

        let mut healed = edge;
        healed.valid = true;
        let reused_id = actor.upsert_edge(healed).await.expect("self-heal upsert");
        assert_eq!(
            reused_id, edge_id,
            "same (src, dst, type, site) reuses the row"
        );

        let after = actor
            .get_neighbors(old_id, "references", Direction::Incoming)
            .await
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "edge visible again after the self-heal upsert"
        );
    }

    #[tokio::test]
    async fn meta_set_get_roundtrip() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        actor
            .set_meta("python.lsp_status", "healthy")
            .await
            .expect("set");
        let got = actor
            .get_meta("python.lsp_status")
            .await
            .expect("get")
            .expect("row present");
        assert_eq!(got, "healthy");
    }

    #[tokio::test]
    async fn meta_set_overwrites() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        actor.set_meta("k", "v1").await.expect("set1");
        actor.set_meta("k", "v2").await.expect("set2");
        assert_eq!(actor.get_meta("k").await.unwrap(), Some("v2".to_string()));
    }

    #[tokio::test]
    async fn meta_get_missing_is_none() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        assert_eq!(actor.get_meta("nope").await.unwrap(), None);
    }

    // --- list / position / neighbor reads (Step 5 query support) ---

    #[tokio::test]
    async fn list_nodes_returns_valid_nodes_ordered_by_sort_key() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        actor.upsert_node(node_with("b.B", "B", 1)).await.unwrap();
        actor.upsert_node(node_with("a.A", "A", 5)).await.unwrap();

        let fqns: Vec<String> = actor
            .list_nodes(None)
            .await
            .unwrap()
            .iter()
            .map(|n| n.fqn.clone())
            .collect();
        assert_eq!(fqns, vec!["a.A", "b.B"]);
    }

    #[tokio::test]
    async fn list_nodes_narrows_by_language() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let mut py = node_with("a.A", "A", 1);
        py.language = "python".to_string();
        let mut ts = node_with("b.B", "B", 5);
        ts.language = "typescript".to_string();
        actor.upsert_node(py).await.unwrap();
        actor.upsert_node(ts).await.unwrap();

        let py_only = actor.list_nodes(Some("python")).await.unwrap();
        assert_eq!(py_only.len(), 1);
        assert_eq!(py_only[0].fqn, "a.A");
    }

    #[tokio::test]
    async fn known_uris_excludes_orphans_and_dedupes_per_file() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");

        let mut a1 = node_with("a.A", "A", 0);
        a1.uri = "file:///app/a.py".to_string();
        let mut a2 = node_with("a.B", "B", 5);
        a2.uri = "file:///app/a.py".to_string();
        let mut b = node_with("b.C", "C", 0);
        b.uri = "file:///app/b.py".to_string();
        b.orphan = true;
        actor.upsert_node(a1).await.unwrap();
        actor.upsert_node(a2).await.unwrap();
        actor.upsert_node(b).await.unwrap();

        let mut uris = actor.known_uris().await.unwrap();
        uris.sort();
        assert_eq!(
            uris,
            vec!["file:///app/a.py".to_string()],
            "a.py's two nodes collapse to one uri; b.py is orphaned and excluded"
        );
    }

    #[tokio::test]
    async fn find_node_by_position_picks_innermost_span() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let mut parent = node_with("app.C", "C", 0);
        parent.range = Range {
            start_line: 0,
            start_col: 0,
            end_line: 10,
            end_col: 0,
        };
        let mut method = node_with("app.C.m", "m", 2);
        method.range = Range {
            start_line: 2,
            start_col: 4,
            end_line: 4,
            end_col: 0,
        };
        actor.upsert_node(parent).await.unwrap();
        actor.upsert_node(method).await.unwrap();

        // Position (3, 0) lies inside the method but also inside the class; the
        // smallest (innermost) span wins.
        let got = actor
            .find_node_by_position("file:///app/mod.py", 3, 0)
            .await
            .unwrap()
            .expect("a node");
        assert_eq!(got.fqn, "app.C.m");
    }

    #[tokio::test]
    async fn find_node_by_position_none_when_outside() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        actor.upsert_node(node_with("app.C", "C", 0)).await.unwrap();

        let got = actor
            .find_node_by_position("file:///app/mod.py", 50, 0)
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn get_neighbors_traverses_incoming_and_outgoing() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let a = actor.upsert_node(node_with("app.A", "A", 1)).await.unwrap();
        let b = actor.upsert_node(node_with("app.B", "B", 5)).await.unwrap();
        let site = Site {
            uri: "file:///app/mod.py".to_string(),
            range: Range {
                start_line: 2,
                start_col: 0,
                end_line: 2,
                end_col: 3,
            },
        };
        actor
            .upsert_edge(Edge {
                id: None,
                src_id: a,
                dst_id: b,
                edge_type: "calls".to_string(),
                site: Some(site.clone()),
                valid: true,
            })
            .await
            .unwrap();

        // B is called by A: incoming neighbor is A, carrying the call site.
        let incoming = actor
            .get_neighbors(b, "calls", Direction::Incoming)
            .await
            .unwrap();
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].0.fqn, "app.A");
        assert_eq!(incoming[0].1.as_ref().unwrap().range.start_line, 2);

        // A calls B: outgoing neighbor is B, same site.
        let outgoing = actor
            .get_neighbors(a, "calls", Direction::Outgoing)
            .await
            .unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].0.fqn, "app.B");
    }

    #[tokio::test]
    async fn get_neighbors_empty_for_unrelated_edge_type() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let a = actor.upsert_node(node_with("app.A", "A", 1)).await.unwrap();
        let b = actor.upsert_node(node_with("app.B", "B", 5)).await.unwrap();
        actor
            .upsert_edge(Edge {
                id: None,
                src_id: a,
                dst_id: b,
                edge_type: "calls".to_string(),
                site: None,
                valid: true,
            })
            .await
            .unwrap();

        let refs = actor
            .get_neighbors(a, "references", Direction::Outgoing)
            .await
            .unwrap();
        assert!(refs.is_empty(), "no references edge exists");
    }

    // --- callees_cache / materialized (query caching & freshness) ---

    #[tokio::test]
    async fn callees_cache_hash_roundtrip_and_overwrite() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let id = actor.upsert_node(node_with("app.A", "A", 1)).await.unwrap();

        assert_eq!(actor.get_callees_cache_hash(id).await.unwrap(), None);

        actor.set_callees_cache_hash(id, "hash1").await.unwrap();
        assert_eq!(
            actor.get_callees_cache_hash(id).await.unwrap(),
            Some("hash1".to_string())
        );

        actor.set_callees_cache_hash(id, "hash2").await.unwrap();
        assert_eq!(
            actor.get_callees_cache_hash(id).await.unwrap(),
            Some("hash2".to_string())
        );
    }

    #[tokio::test]
    async fn reconcile_content_change_drops_callees_cache() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut node = node_with("app.mod.helper", "helper", 1);
        node.signature_hash = Some("hash1".to_string());
        let id = actor.upsert_node(node).await.unwrap();
        actor.set_callees_cache_hash(id, "content1").await.unwrap();

        // A same-line-count body edit: signature_hash changes, so the write
        // path must drop the callees cache row regardless.
        actor
            .reconcile_file_symbols(
                uri,
                "python",
                false,
                vec![sym(
                    "app.mod.helper",
                    "helper",
                    12,
                    "function",
                    1,
                    "hash2",
                    None,
                )],
            )
            .await
            .expect("reconcile");

        assert_eq!(actor.get_callees_cache_hash(id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn reconcile_unchanged_content_still_drops_callees_cache() {
        // The DELETE fires unconditionally on every reconcile of a uri, even
        // when the symbol's `signature_hash` is unchanged — this is what
        // compensates for `signature_hash` under-firing on a callee-only edit
        // elsewhere in the same file.
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let uri = "file:///app/mod.py";

        let mut node = node_with("app.mod.helper", "helper", 1);
        node.signature_hash = Some("hash1".to_string());
        let id = actor.upsert_node(node).await.unwrap();
        actor.set_callees_cache_hash(id, "content1").await.unwrap();

        actor
            .reconcile_file_symbols(
                uri,
                "python",
                false,
                vec![sym(
                    "app.mod.helper",
                    "helper",
                    12,
                    "function",
                    1,
                    "hash1",
                    None,
                )],
            )
            .await
            .expect("reconcile");

        assert_eq!(actor.get_callees_cache_hash(id).await.unwrap(), None);
    }

    #[tokio::test]
    async fn materialized_marker_roundtrip_is_per_edge_type() {
        let dir = tempdir().expect("tempdir");
        let actor = DbActor::spawn(&dir.path().join("graph.db")).expect("spawn");
        let id = actor.upsert_node(node_with("app.A", "A", 1)).await.unwrap();

        assert!(!actor.is_materialized(id, "calls").await.unwrap());
        assert!(!actor.is_materialized(id, "references").await.unwrap());

        actor.mark_materialized(id, "calls").await.unwrap();
        assert!(actor.is_materialized(id, "calls").await.unwrap());
        assert!(
            !actor.is_materialized(id, "references").await.unwrap(),
            "marking one edge_type must not warm the other"
        );

        // Idempotent.
        actor.mark_materialized(id, "calls").await.unwrap();
        assert!(actor.is_materialized(id, "calls").await.unwrap());
    }
}
