# LSP Integration — Live-Server Observation Notes

Return structures, per-language differences, and gotchas for each LSP method, derived from live probes against pyright (Python) and typescript-language-server (TS). Reference material for implementation.

> Test environment: pyright `1.1.409` / typescript-language-server `5.1.3` + typescript `6.0.3`. JSON-RPC over stdio driven by a custom harness to capture raw responses.

## Prerequisite: client capabilities

The following are advertised in `initialize` (to maximize the information returned):

* `hierarchicalDocumentSymbolSupport: true` → documentSymbol comes back in nested form
* `linkSupport: true` (definition/typeDefinition/implementation) → LocationLink may be returned (tsserver only; pyright ignores this)
* `callHierarchy` → enables the callHierarchy family

## Server-side textDocumentSync (observed in practice)

Both servers negotiate `InitializeResult.capabilities.textDocumentSync = 2` (Incremental), but the Graph side does not compute range-based diffs — it sends the whole document as a single `contentChanges` entry with no range (see "Detecting re-analysis completion after didChange" in [indexing-and-cache.md](./indexing-and-cache.md) for details). The **next request after didChange responds only once re-analysis has completed** (synchronous behavior), so the Graph side does not need to wait for completion separately ([lsp-lifecycle.md](./lsp-lifecycle.md) A2).

## Return structure and language differences per method

### textDocument/documentSymbol → `contains` edge + Node

* **Format**: `DocumentSymbol[]` (nested). Both servers return the nested form.
* **Fields**: `name` / `kind` / `range` (the entire declaration) / `selectionRange` (the identifier — a required sub-range contained within `range`) / `children` (array) / `detail` / `tags`
* **pyright**: `detail`/`tags` are always null. `children` also includes parameters and instance attributes (e.g. `self.base`).
* **tsserver**: `detail` is almost always an empty string. **Overloads appear as separate entries with the same name** (parallel children, all with `kind=6`).
* **Note**: Type information (signature/docstring) is not carried in documentSymbol → **must be obtained separately via hover**.

### textDocument/definition → `defines` edge

* **pyright**: `Location[]` (`{uri, range}`). Ignores `linkSupport`. Multiple resolutions are possible (overloads). **null is a normal outcome** (type resolution failure, unresolved import).
* **tsserver**: `LocationLink[]` (`{originSelectionRange, targetUri, targetRange, targetSelectionRange}`), when `linkSupport: true`.
* **Note**: originSelectionRange (tsserver) gives a more precise reference position for the edge. The parser handles both `Location` and `LocationLink` (if `targetUri` is present it's a LocationLink; otherwise determined via `uri`/`range`).

### textDocument/references → `references` edge

* **Both servers**: `Location[]` (flat). Supports cross-file (whole-workspace scan). `context.includeDeclaration` controls whether the declaration is included.
* **pyright note (correction)**: declaring `workspaceFolders` in `initialize` triggers a background scan, and cross-file references resolve correctly. **Pre-emptive `didOpen` on dependency files is not required** (live verification U1: references can be obtained with zero didOpen calls). The earlier claim that "pre-emptive didOpen is required" was actually caused by the probe not passing `workspaceFolders` ([lsp-lifecycle.md](./lsp-lifecycle.md) A2).

### textDocument/typeDefinition → `type_of` edge

* **Both servers**: `Location[]`. Variable/field → type node. May jump into typeshed/node_modules (separated via `is_external`).
* **Watch for null/empty as normal outcomes (observed live)**: pyright returns **null** at an annotation type position (the `int` in `n: int`) (resolves if the variable type is inferred). tsserver returns an **empty array `[]`** when unresolvable (not null). The Graph treats `null || empty` as "no edge" ([resilience.md](./resilience.md), normal-case null handling).
* **stdlib split (observed live)**: for standard library imports, pyright returns **two Locations simultaneously** — `.pyi` (typeshed) and `.py` (the actual implementation). Both are stored with `is_external=1`, which splits the same symbol into two nodes, but this causes no practical harm since the default query (`include_external=false`) excludes them. De-duplication is a 0.1+ concern.

### textDocument/implementation → `implements` edge

* **pyright**: **unsupported** (`-32601 Unhandled method`). Python's duck typing makes the notion of "implementation" weak.
* **tsserver**: supported. `Location[]`. interface → implementing class.
* **Note**: the `implements` edge is **TS-only**. For Python, `typeDefinition` + `references` serve as a substitute.

### textDocument/hover → `signature` / `documentation` / `construct`

* **Both servers**: `{contents: MarkupContent, range}`. signature + docstring combined into a single string.
* **pyright**: `kind` observed as `plaintext` for both functions and methods (`tests/fixtures/lsp-probe/captures/python_hover_method_self_notation.json`) — not the markdown code block an earlier version of this note claimed. Method hovers use `Self@ClassName` notation.
* **tsserver**: markdown (a ```` ```typescript ```` code block + JSDoc).
* **Usage**: source for extracting the `signature`/`documentation` columns. **Required for TS TypeAlias detection** (a leading `type` keyword in the signature → `construct=type` → promoted to `NodeKind::Custom("TypeAlias")`).
* **`signature` is populated lazily, not at index time**: 0.0.1 indexing is documentSymbol-only, so `signature` starts `null`. It's backfilled the first time a node is hovered — automatically (no extra cost) on `find_definition`'s `at` branch, which already hovers to refine `construct`; or on `find_symbol`/`find_callers`/`find_callees` only when the caller passes `with_signature=true`, since those tools otherwise never touch the LSP to build their `Node`s ([mcp-tools.md](./mcp-tools.md) "Populating `signature`"). Once backfilled it's persisted to the `nodes.signature` column, so later queries never re-hover for it.

### callHierarchy (prepare / incoming / outgoing) → `calls` edge

* **Both servers**: fully supported.
* **prepareCallHierarchy** → `CallHierarchyItem[]` (`{name, kind, uri, range, selectionRange}`)
* **incomingCalls** → `[{from: CallHierarchyItem, fromRanges: Range[]}]`
* **outgoingCalls** → `[{to: CallHierarchyItem, fromRanges: Range[]}]`
* **`fromRanges`**: an array of Ranges for the call-site locations (aggregates multiple call sites within the same function) → mapped to `site_*` on the `calls` edge, with **1 edge per fromRange**.
* **pyright note**: for a module-scope call (e.g. `main()`), the caller becomes `(module) sample.py` (`kind=2`, `range={0,0}`) → a module node (`kind=2`) is generated to serve as the entry-point caller. `outgoingCalls` sometimes returns null instead of an empty array.
* **tsserver note**: outgoing's `to` sometimes points to a **type-resolved target (an interface method)** → query-side logic is needed to correct this to the implementing class's method via the `implements` edge.
* **Note**: the `calls` edge cannot be built from documentSymbol (call relationships aren't carried there) → it can only be constructed **on demand by invoking callHierarchy**.

### textDocument/workspaceSymbol

* **pyright**: returns an empty array in this environment (not to be trusted).
* **Workaround**: build the project-wide symbol list by aggregating each file's `documentSymbol`.

## Summary: method → return format → edge

| LSP method | pyright | tsserver | semnav edge / usage |
|---|---|---|---|
| documentSymbol | `DocumentSymbol[]` (nested) | `DocumentSymbol[]` (nested) | `contains` + Node creation |
| definition | `Location[]` | `LocationLink[]` | `defines` |
| references | `Location[]` | `Location[]` | `references` |
| typeDefinition | `Location[]` | `Location[]` | `type_of` |
| implementation | **unsupported** (-32601) | `Location[]` | `implements` (**TS only**) |
| hover | `MarkupContent` (md/plain mixed) | `MarkupContent` (md) | `signature`/`documentation`/`construct` |
| callHierarchy (in/out) | fully supported | fully supported | `calls` (`fromRanges` = `site_*`) |

## General notes

1. **Declaring `workspaceFolders` is required** (to avoid pyright's cross-file null). Passing `workspaceFolders: [{uri: rootUri, name}]` in `initialize` triggers a background scan, so **pre-emptive `didOpen` is not required** for dependency files, typeshed, or node_modules. didOpen/didChange are only issued for live files that the FS watcher detects as changed. (`rootUri` alone is deprecated and does not trigger the scan.)
2. **Separation of external resolutions**: nodes resolved into typeshed (bundled with pyright) / node_modules / site-packages / .venv are stored with `is_external=1` and excluded from the default query (determined via `uri` path prefix).
3. **null as a normal outcome**: definition/references/hover can return null (type resolution failure, unresolved import, single-file state). The Graph simply treats this as "no edge." Index completion rate is measured via `index_meta`.
4. **Multiple resolutions**: when definition returns multiple `Location`s (e.g. overloads) → this becomes edge multiplicity. Node uniqueness is guaranteed by `uri + selectionRange`.
5. **The SymbolKind trap (TS)**: a `type` alias has `kind=13` (the same as Variable). `construct=type` is extracted via hover and promoted to `NodeKind::Custom("TypeAlias")` ([hover-based refinement in language-adapters.md](./language-adapters.md#refinement-via-hover)).

## Query-time caching and freshness

On-demand edge construction (above) means every `find_references`/`find_callers`/`find_callees` call can potentially pay a live LSP round trip. Two distinct caching strategies close that gap, split by whether the relation's freshness key is cheap to check:

### `find_callees` — precise content-hash cache

The outgoing callee list of a node depends only on that node's own file text, so a single-file content hash is a sufficient, *exact* freshness key: `callees_cache(anchor_id, content_hash)` records the anchor file's text hash at the time its callees were last materialized. A query hashes the anchor's current file text and compares; on a match, the graph is served straight from `edges` with no LSP call — correct even when the callee list is empty, since the write path (`reconcile_file_symbols_tx`, [indexing-and-cache.md](./indexing-and-cache.md)) drops the cache row on every reconcile of that file, regardless of whether `signature_hash` changed.

Because `materialize_call_edges` only upserts the edges callHierarchy reports (it doesn't know what to remove), a cache miss first invalidates the anchor's existing outgoing `calls` edges before re-materializing — otherwise a callee removed since the last materialization would leave a stale, still-`valid` edge behind.

### `find_callers` / `find_references` — cache-first + background refresh

Unlike callees, an incoming relation (who calls/references this anchor) can change from *any* file in the workspace without touching the anchor's own file — there is no cheap per-node freshness key. Instead:

* A `materialized(anchor_id, edge_type)` marker records whether the anchor has ever been materialized for `"calls"` (incoming) or `"references"`.
* **Cold** (no marker row): blocks on one materialization, same as before, then records the marker.
* **Warm** (marker present): the cached graph is served immediately (a DB read, not an LSP round trip), and a detached background task re-materializes so the *next* query is fresh. The response carries `refreshing: true` ([resilience.md](./resilience.md)) so the caller knows to re-query for the converged answer.

This is deliberately asymmetric with the "never lie with an empty result" invariant: a cold anchor's empty cache is indistinguishable from "nobody calls this," so cold always blocks; a warm anchor can only be stale in the "missing a newly-added caller" direction, converging on the next query, which is far less dangerous than a false negative on a navigation tool.

### Watcher yields to live queries

The FS watcher's reconcile loop and a live query can both want the same language server at once. Rather than a priority queue (the reconcile loop already holds at most one in-flight `documentSymbol` request — see [indexing-and-cache.md](./indexing-and-cache.md)), `QueryRuntime` tracks an in-flight count of foreground LSP-touching queries (`find_references`/`find_callers`/`find_callees`, and `find_definition`'s `at`-position path). The watcher awaits the count reaching zero before *starting* its next per-file reconcile, so a live query isn't taxed by concurrent `documentSymbol` traffic saturating the server. It cannot preempt a reconcile already in flight, and background refreshes (above) deliberately don't hold this gate — they're best-effort load, not live queries, and holding it would let a stream of warm queries starve the watcher.

## Provenance of the live verification

These findings came from an ad hoc harness driving pyright and typescript-language-server directly over stdio against small scratch fixtures. The fixtures were never checked into the repo and no longer exist, so the findings above are a point-in-time empirical record, not a reproducible test suite. Turning them into a proper conformance suite (e.g. under `tests/fixtures/lsp-probe/`) is tracked as future work.
