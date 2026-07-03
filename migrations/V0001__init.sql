-- semnav 0.0.1 schema.
-- Source of truth: docs/design/graph-model.md (Schema 0.0.1).
-- Migration runner: refinery (`embed_migrations!`).
--
-- Notes:
--   * PRAGMA (journal_mode=WAL, foreign_keys=ON, ...) are connection-level and
--     are applied by the db actor in Rust at open time, NOT here.
--   * This file holds DDL only; refinery wraps each migration in a transaction.

-- ============================================================================
-- nodes — declared symbols.
--   Logical primary key: fqn (fully-qualified name, human-readable search key).
--   Physical uniqueness: (uri, range_start_line, range_start_col, name).
--   See graph-model.md "Stable Symbol ID".
-- ============================================================================
CREATE TABLE nodes (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,

    -- identity
    fqn               TEXT    NOT NULL,
    uri               TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    language          TEXT    NOT NULL,            -- "python" | "typescript" | ...

    -- LSP SymbolKind (numeric, pass-through; 1..44+). Not range-checked.
    kind              INTEGER NOT NULL,
    -- Serialized NodeKind (adapter-classified enum string).
    node_kind         TEXT    NOT NULL,
    -- Hover-derived auxiliary classification; NULL until hover is obtained.
    construct         TEXT,

    -- Parent/child (DocumentSymbol children nesting). Self-reference.
    container_id      INTEGER REFERENCES nodes(id) ON DELETE SET NULL,

    -- Declaration range (LSP Range, 0-based, UTF-16 char units).
    range_start_line  INTEGER NOT NULL,
    range_start_col   INTEGER NOT NULL,
    range_end_line    INTEGER NOT NULL,
    range_end_col     INTEGER NOT NULL,
    -- selectionRange (identifier span).
    sel_start_line    INTEGER NOT NULL,
    sel_start_col     INTEGER NOT NULL,
    sel_end_line      INTEGER NOT NULL,
    sel_end_col       INTEGER NOT NULL,

    -- Extracted from hover / documentSymbol.
    signature         TEXT,
    documentation     TEXT,
    detail            TEXT,

    -- Cache control (graph-model.md "dirty lifecycle").
    signature_hash    TEXT,                         -- NULL until signature known
    valid             INTEGER NOT NULL DEFAULT 1 CHECK (valid       IN (0,1)),
    orphan            INTEGER NOT NULL DEFAULT 0 CHECK (orphan      IN (0,1)),
    generation        INTEGER NOT NULL DEFAULT 0,   -- reserved/unused in 0.0.1; two-strike
                                                     -- orphan reclamation uses `orphan` instead
    is_external       INTEGER NOT NULL DEFAULT 0 CHECK (is_external IN (0,1))
);

-- Logical primary key (search by FQN).
CREATE UNIQUE INDEX idx_nodes_fqn        ON nodes(fqn);
-- Physical uniqueness: (uri, range, name) — range represented by start position.
CREATE UNIQUE INDEX idx_nodes_phys       ON nodes(uri, range_start_line, range_start_col, name);
-- Children traversal (DocumentSymbol parent → children).
CREATE INDEX        idx_nodes_container  ON nodes(container_id);
-- File-scoped invalidation (FS watcher dirties a file's nodes).
CREATE INDEX        idx_nodes_uri        ON nodes(uri);

-- ============================================================================
-- edges — one row per (relation, fromRange).
--   site_* holds the call/ref occurrence span; NULL for site-less edges
--   (e.g. 'contains'). "1 edge = 1 fromRange" enables row-level invalidation
--   (graph-model.md Edges).
-- ============================================================================
CREATE TABLE edges (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,

    src_id            INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    dst_id            INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    edge_type         TEXT    NOT NULL CHECK (edge_type IN
                        ('contains','calls','references','type_of',
                         'implements','defines','inherits','imports')),

    -- Call/ref occurrence location (NULL for site-less edges like 'contains').
    site_uri          TEXT,
    site_start_line   INTEGER,
    site_start_col    INTEGER,
    site_end_line     INTEGER,
    site_end_col      INTEGER,

    valid             INTEGER NOT NULL DEFAULT 1 CHECK (valid IN (0,1)),

    -- Dedupe: same relation + same site = one edge.
    UNIQUE (src_id, dst_id, edge_type, site_uri, site_start_line, site_start_col)
);

CREATE INDEX idx_edges_src      ON edges(src_id, edge_type);
CREATE INDEX idx_edges_dst      ON edges(dst_id, edge_type);
CREATE INDEX idx_edges_site_uri ON edges(site_uri);

-- ============================================================================
-- events — runtime call events (FUTURE, dynamic-graph.md).
--   0.0.1 reserves the table only; no writes. Columns land in a later
--   migration when the tracer plugin contract is finalized.
-- ============================================================================
CREATE TABLE events (
    id                INTEGER PRIMARY KEY
);

-- ============================================================================
-- index_meta — key/value for schema version, LSP version, index progress,
--   didOpen'd files, etc. Values are JSON-encoded strings.
-- ============================================================================
CREATE TABLE index_meta (
    key               TEXT PRIMARY KEY,
    value             TEXT NOT NULL
);
