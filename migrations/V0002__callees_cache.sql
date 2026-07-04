-- Query-time caching for on-demand edge construction.
-- Source of truth: docs/design/lsp-integration.md "on-demand edge construction".

-- ============================================================================
-- callees_cache — precise freshness key for `find_callees` (outgoing `calls`).
--   The outgoing callee list of a node depends only on its own file's text, so
--   a content-hash match means the cached graph is exact (no LSP call needed).
--   Cleared unconditionally whenever the anchor's file is reconciled with
--   changed content (`reconcile_file_symbols_tx`), since a same-line-count
--   body edit can change callees without changing `signature_hash`.
-- ============================================================================
CREATE TABLE callees_cache (
    anchor_id    INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL
);

-- ============================================================================
-- materialized — "warm" marker for `find_callers` / `find_references`
-- (incoming edges). Presence of a row means the anchor has been materialized
-- at least once for that edge_type; a warm anchor is served cache-first with
-- a background refresh, a cold one blocks so a first query is never a false
-- empty. Never cleared: staleness is handled by the background refresh, not
-- by invalidating the marker.
-- ============================================================================
CREATE TABLE materialized (
    anchor_id INTEGER NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    edge_type TEXT    NOT NULL,   -- 'calls' (incoming) | 'references'
    PRIMARY KEY (anchor_id, edge_type)
);
