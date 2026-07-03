# Indexing & Cache

## Initial Indexing

The initial run does not require fully parsing the entire Repository. At minimum, only `documentSymbol` is fetched, creating Node entries only.

### File Discovery & Collection (0.0.1)

* **Target**: Walk the tree under `workspaceFolders` (rootUri) and collect files with the target language extensions (`.py` / `.ts` / `.tsx`)
* **Exclusions**: Respect `.gitignore` via the `ignore` crate (plus cache directories such as `.semnav/`). `is_external` paths (the external prefix list in [graph-model.md](./graph-model.md)) are also excluded from the Graph's documentSymbol collection (the LSP generates them as external nodes on reference)
* **Collection order**: Request documentSymbol from the LSP one file at a time. 0.0.1 is **serial** (a single stdio connection processed sequentially is safe). Progress is measured via `window/logMessage` plus the index progress in `index_meta`
* **Interruption**: Aborted by the Graph process's cancellation signal (a partial index is acceptable)
* **The LSP side is separate**: Independently of what the Graph collects, the LSP server itself performs a background full scan over `workspaceFolders` ([lsp-lifecycle.md](./lsp-lifecycle.md) A2). Graph Node construction and the LSP's internal index are independent

Edges can be generated on demand:

```
find_callers(save)
  ↓
CALLS not yet built
  ↓
references() / callHierarchy
  ↓
Graph update
  ↓
Return result
```

The Graph grows the more it is used.

### Warmup (0.1+)

An option to pre-collect References / Definition / Call Hierarchy, etc. via `graph warmup`. In 0.0.1, only documentSymbol is collected.

## Cache Invalidation

The Graph is a cache. On source code changes, rather than rebuilding the entire Graph, only the changed Node and Edge entries are marked Dirty (`valid=false`).

### 0.0.1 Invalidation Flow

File changes are detected via the FS watcher (`notify`):

```
File change detected (notify)
  ↓
Mark the corresponding File Node dirty, set related Edges to valid=false
  ↓
Graph sends textDocument/didChange to the LSP (updates the LSP's internal index)
  ↓
On the next query, UPSERT valid=false Edges by re-querying the LSP
```

When an AI agent itself edits a file, it sends an Invalidate notification to the Graph upon completing the edit (the watcher also picks this up via the file save, but an explicit notification is also accepted for immediacy).

#### Detecting Re-analysis Completion After didChange (Observed Behavior)

Both servers negotiate `textDocumentSync=2` (Incremental), but the Graph side does not compute a ranged diff — it sends the **entire document, without a range**, as a single `contentChanges` entry (this is valid per spec even under Incremental negotiation). This is a simplification that prioritizes consistency with the design policy of not holding a diff algorithm or a cache of prior content (`notify_did_change` in `src/lsp/client.rs`). **The Graph side does not need to wait for re-analysis to complete** — on real hardware, we confirmed that the next request (references/documentSymbol) issued immediately after didChange **synchronously returns fresh results** (pyright 0.237s / tsserver 5ms). Because the server completes re-analysis before responding to a query, no explicit synchronization is required.

Using `publishDiagnostics` as a completion signal is **not viable** (it fans out beyond the changed file / pyright emits a two-stage clear→final / tsserver emits zero notifications for a clean→clean transition). Given this, the "re-query the LSP on the next query" step in the flow above may be performed immediately after didChange.

### Orphan Reclamation

Old symbols that could not be re-linked by rename tracking ([Stable Symbol ID Rename Tracking](./graph-model.md#rename-tracking)) become `orphan=true`. Deletion happens via a **two-strike grace period based solely on the `orphan` flag** (the `generation` column is reserved for future multi-stage tuning but is unused in 0.0.1 — always 0). Time-based TTL is not used (to prevent a mass deletion sweep from firing immediately after startup, since only the clock advances while the Graph process is stopped).

Lifecycle:

```
Disappears on the first documentSymbol re-fetch
  → orphan=false transitions to orphan=true (1st strike, grace period)

On the next documentSymbol re-fetch:
  ├─ Still missing (remains orphan=true) → 2nd strike → physical deletion (Edges are automatically deleted via ON DELETE CASCADE)
  └─ Reappears (revert / conflict resolution) → restored to orphan=false (revival)
```

* **The grace period is granted once.** If the symbol fails to reappear for two consecutive checks, it is judged a genuine deletion
* **Queries return only `orphan=false` by default.** Garbage in the grace period does not leak into results
* **On revival**: reset to `orphan=false`. Since Edges may already have been discarded, they are set to `valid=false` to force re-evaluation
* **In 0.0.1, reclamation is automatic only** — the second-strike deletion above is the sole path to physical deletion; there is no manual CLI escape hatch
