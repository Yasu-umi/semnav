# LSP Integration — Live-Server Observation Notes

Return structures, per-language differences, and gotchas for each LSP method, derived from live probes against pyright (Python), typescript-language-server (TS), and gopls (Go). Reference material for implementation.

> Test environment: pyright `1.1.409` / typescript-language-server `5.1.3` + typescript `6.0.3` / gopls `0.22.0` (go `1.26.4`). JSON-RPC over stdio driven by a custom harness to capture raw responses.

## Prerequisite: client capabilities

The following are advertised in `initialize` (to maximize the information returned):

* `hierarchicalDocumentSymbolSupport: true` → documentSymbol comes back in nested form
* `linkSupport: true` (definition/typeDefinition/implementation) → LocationLink may be returned (tsserver only; pyright ignores this)
* `callHierarchy` → enables the callHierarchy family

## Server-side textDocumentSync (observed in practice)

All three servers negotiate `InitializeResult.capabilities.textDocumentSync = 2` (Incremental), but the Graph side does not compute range-based diffs — it sends the whole document as a single `contentChanges` entry with no range (see "Detecting re-analysis completion after didChange" in [indexing-and-cache.md](./indexing-and-cache.md) for details). The **next request after didChange responds only once re-analysis has completed** (synchronous behavior), so the Graph side does not need to wait for completion separately ([lsp-lifecycle.md](./lsp-lifecycle.md) A2).

## Return structure and language differences per method

### textDocument/documentSymbol → `contains` edge + Node

* **Format**: `DocumentSymbol[]` (nested). All three servers return the nested form.
* **Fields**: `name` / `kind` / `range` (the entire declaration) / `selectionRange` (the identifier — a required sub-range contained within `range`) / `children` (array) / `detail` / `tags`
* **pyright**: `detail`/`tags` are always null. `children` also includes parameters and instance attributes (e.g. `self.base`).
* **tsserver**: `detail` is almost always an empty string. **Overloads appear as separate entries with the same name** (parallel children, all with `kind=6`).
* **gopls**: `detail` is populated (e.g. `"struct{...}"`, `"func() string"`). Struct fields and interface method signatures nest normally as `children` (kind `Struct`=23 / `Interface`=11 containers). **A method with a receiver does NOT nest under its receiver struct** — `func (p *Person) Greet() string` comes back as a **top-level sibling** entry named `"(*Person).Greet"` (kind=6, `Method`), not a child of `Person`. This means Go structs get no `contains` edge to their receiver methods (unlike Python/TS classes to their methods); the method's own name string carries the receiver association instead. Pinned in `tests/fixtures/lsp-probe/captures/go_document_symbol_receiver_method.json` and `src/indexer/symbol.rs`'s `flatten_keeps_gopls_receiver_method_as_a_top_level_sibling`.
* **Note**: Type information (signature/docstring) is not carried in documentSymbol → **must be obtained separately via hover**.

### textDocument/definition → `defines` edge

* **pyright**: `Location[]` (`{uri, range}`). Ignores `linkSupport`. Multiple resolutions are possible (overloads). **null is a normal outcome** (type resolution failure, unresolved import).
* **tsserver**: `LocationLink[]` (`{originSelectionRange, targetUri, targetRange, targetSelectionRange}`), when `linkSupport: true`.
* **gopls**: `Location[]`, same flat shape as pyright — even though gopls also declares `implementationProvider`/`typeDefinitionProvider` and the client advertises `linkSupport: true`, it just doesn't use `LocationLink` here. Handled by the existing parser with zero code changes (`parse_locations_matches_captured_gopls_definition`, `src/query/lsp_query.rs`).
* **Note**: originSelectionRange (tsserver) gives a more precise reference position for the edge. The parser handles both `Location` and `LocationLink` (if `targetUri` is present it's a LocationLink; otherwise determined via `uri`/`range`).

### textDocument/references → `references` edge

* **All three servers**: `Location[]` (flat). Supports cross-file (whole-workspace scan). `context.includeDeclaration` controls whether the declaration is included.
* **pyright note (correction)**: declaring `workspaceFolders` in `initialize` triggers a background scan, and cross-file references resolve correctly. **Pre-emptive `didOpen` on dependency files is not required** (live verification U1: references can be obtained with zero didOpen calls). The earlier claim that "pre-emptive didOpen is required" was actually caused by the probe not passing `workspaceFolders` ([lsp-lifecycle.md](./lsp-lifecycle.md) A2).
* **gopls note**: an interface method's `references` includes the interface declaration itself and textual usages of the interface type (e.g. a parameter typed `g Greeter`), but **not** a struct's method that merely satisfies the interface structurally — Go's implicit interface satisfaction leaves no explicit textual reference to follow, unlike TS's `implements` keyword.

### textDocument/typeDefinition → `type_of` edge

* **All three servers**: `Location[]`. Variable/field → type node. May jump into typeshed/node_modules (separated via `is_external`).
* **Watch for null/empty as normal outcomes (observed live)**: pyright returns **null** at an annotation type position (the `int` in `n: int`) (resolves if the variable type is inferred). tsserver returns an **empty array `[]`** when unresolvable (not null). The Graph treats `null || empty` as "no edge" ([resilience.md](./resilience.md), normal-case null handling).
* **stdlib split (observed live)**: for standard library imports, pyright returns **two Locations simultaneously** — `.pyi` (typeshed) and `.py` (the actual implementation). Both are stored with `is_external=1`, which splits the same symbol into two nodes, but this causes no practical harm since the default query (`include_external=false`) excludes them. De-duplication is a 0.1+ concern.

### textDocument/implementation → `implements` edge

* **pyright**: **unsupported** (`-32601 Unhandled method`). Python's duck typing makes the notion of "implementation" weak.
* **tsserver**: supported. `Location[]`. interface → implementing class.
* **gopls**: supported. `Location[]`. interface → satisfying concrete struct — resolves Go's implicit interface satisfaction even though there's no `implements` keyword to key off (`tests/fixtures/lsp-probe/captures/go_implementation_interface_to_struct.json`).
* **Note**: the `implements` edge is materialized for **TS and Go**. For Python, `typeDefinition` + `references` serve as a substitute.
* **Consumer**: `find_callees`'s interface-to-implementation correction (`src/query/resolver.rs` `resolve_outgoing_callee`, [mcp-tools.md](./mcp-tools.md)) — the only place `implementation` is called from. Gated on `anchor.language` being `"typescript"` or `"go"` (language, not capability — a Python container never has `node_kind == "Interface"` anyway). The callee's file must be `didOpen`ed on the query-time client before this call, same as the anchor's own file; tsserver (and gopls) answer an unopened document's `implementation` request with an empty array rather than an error, which silently looked like "no implementation" the first time this was wired up.

### textDocument/hover → `signature` / `documentation` / `construct`

* **All three servers**: `{contents: MarkupContent, range}`. signature + docstring combined into a single string.
* **pyright**: `kind` observed as `plaintext` for both functions and methods (`tests/fixtures/lsp-probe/captures/python_hover_method_self_notation.json`) — not the markdown code block an earlier version of this note claimed. Method hovers use `Self@ClassName` notation.
* **tsserver**: markdown (a ```` ```typescript ```` code block + JSDoc).
* **gopls**: markdown (a ```` ```go ```` code block, e.g. `func (p *Person) Greet() string`, plus a trailing pkg.go.dev link). No hover-based `construct` refinement is needed for Go (see `docs/design/language-adapters.md` "Refinement via hover"), so the `"func"` keyword was never added to `NodeKind::construct_from_hover`'s `KEYWORDS` table — there's nothing for it to disambiguate.
* **Usage**: source for extracting the `signature`/`documentation` columns. **Required for TS TypeAlias detection** (a leading `type` keyword in the signature → `construct=type` → promoted to `NodeKind::Custom("TypeAlias")`).
* **`signature` is populated lazily, not at index time**: 0.0.1 indexing is documentSymbol-only, so `signature` starts `null`. It's backfilled the first time a node is hovered — automatically (no extra cost) on `find_definition`'s `at` branch, which already hovers to refine `construct`; or on `find_symbol`/`find_callers`/`find_callees` only when the caller passes `with_signature=true`, since those tools otherwise never touch the LSP to build their `Node`s ([mcp-tools.md](./mcp-tools.md) "Populating `signature`"). Once backfilled it's persisted to the `nodes.signature` column, so later queries never re-hover for it.

### callHierarchy (prepare / incoming / outgoing) → `calls` edge

* **All three servers**: fully supported.
* **prepareCallHierarchy** → `CallHierarchyItem[]` (`{name, kind, uri, range, selectionRange}`)
* **incomingCalls** → `[{from: CallHierarchyItem, fromRanges: Range[]}]`
* **outgoingCalls** → `[{to: CallHierarchyItem, fromRanges: Range[]}]`
* **`fromRanges`**: an array of Ranges for the call-site locations (aggregates multiple call sites within the same function) → mapped to `site_*` on the `calls` edge, with **1 edge per fromRange**.
* **pyright note**: for a module-scope call (e.g. `main()`), the caller becomes `(module) sample.py` (`kind=2`, `range={0,0}`) → a module node (`kind=2`) is generated to serve as the entry-point caller. `outgoingCalls` sometimes returns null instead of an empty array.
* **tsserver note**: outgoing's `to` sometimes points to a **type-resolved target (an interface method)** → query-side logic is needed to correct this to the implementing class's method via the `implements` edge.
* **gopls note**: same interface-dispatch quirk as tsserver — a call through an interface-typed parameter reports `to` as the interface's abstract method, corrected the same way (`resolve_outgoing_callee`, now gated on TS **or** Go). Unlike pyright, package-scope calls attribute cleanly to the enclosing function with no synthetic `(module)` caller needed — gopls never reports the `{0,0}` sentinel. gopls's `CallHierarchyItem.kind` is also observed as `12` (Function) uniformly, even for a method (`tests/fixtures/lsp-probe/captures/go_outgoing_calls_to_interface_method.json`) — harmless here since the correction resolves the callee via `find_node_by_position` against the already-indexed node, not via this `kind` field.
* **Note**: the `calls` edge cannot be built from documentSymbol (call relationships aren't carried there) → it can only be constructed **on demand by invoking callHierarchy**.

### textDocument/workspaceSymbol

* **pyright**: returns an empty array in this environment (not to be trusted).
* **gopls**: functional, like tsserver — returns real results, including workspace-root-external stdlib matches (harmless noise; excluded from indexed queries by `is_external`, and semnav never calls `workspace/symbol` from production code anyway).
* **Workaround**: build the project-wide symbol list by aggregating each file's `documentSymbol`.

## Summary: method → return format → edge

| LSP method | pyright | tsserver | gopls | semnav edge / usage |
|---|---|---|---|---|
| documentSymbol | `DocumentSymbol[]` (nested) | `DocumentSymbol[]` (nested) | `DocumentSymbol[]` (nested; receiver methods flat, not nested) | `contains` + Node creation |
| definition | `Location[]` | `LocationLink[]` | `Location[]` | `defines` |
| references | `Location[]` | `Location[]` | `Location[]` | `references` |
| typeDefinition | `Location[]` | `Location[]` | `Location[]` | `type_of` |
| implementation | **unsupported** (-32601) | `Location[]` | `Location[]` | `implements` (**TS and Go**) |
| hover | `MarkupContent` (md/plain mixed) | `MarkupContent` (md) | `MarkupContent` (md) | `signature`/`documentation`/`construct` |
| callHierarchy (in/out) | fully supported | fully supported | fully supported | `calls` (`fromRanges` = `site_*`) |
| workspaceSymbol | untrustworthy (empty) | — | functional | not used from production code |

## General notes

1. **Declaring `workspaceFolders` is required** (to avoid pyright's cross-file null). Passing `workspaceFolders: [{uri: rootUri, name}]` in `initialize` triggers a background scan, so **pre-emptive `didOpen` is not required** for dependency files, typeshed, or node_modules. didOpen/didChange are only issued for live files that the FS watcher detects as changed. (`rootUri` alone is deprecated and does not trigger the scan.)
2. **Separation of external resolutions**: nodes resolved into typeshed (bundled with pyright) / node_modules / site-packages / .venv / vendor / the Go module cache are stored with `is_external=1` and excluded from the default query (determined via `uri` path prefix). For Go specifically, the stdlib (`GOROOT`) resolves entirely outside the workspace root, so it's already excluded by the "not under `root_uri`" branch without needing an explicit path marker; only vendored deps (`/vendor/`) and the module cache (`/pkg/mod/`) need one, for the case where they happen to sit under root.
3. **null as a normal outcome**: definition/references/hover can return null (type resolution failure, unresolved import, single-file state). The Graph simply treats this as "no edge." Index completion rate is measured via `index_meta`.
4. **Multiple resolutions**: when definition returns multiple `Location`s (e.g. overloads) → this becomes edge multiplicity. Node uniqueness is guaranteed by `uri + selectionRange`.
5. **The SymbolKind trap (TS)**: a `type` alias has `kind=13` (the same as Variable). `construct=type` is extracted via hover and promoted to `NodeKind::Custom("TypeAlias")` ([hover-based refinement in language-adapters.md](./language-adapters.md#refinement-via-hover)). Go has no equivalent trap — its `SymbolKind` values are unambiguous.

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

These findings were originally captured with an ad hoc harness driving pyright and typescript-language-server directly over stdio against scratch fixtures that were never checked into the repo (the gopls findings were captured the same way, in a throwaway Python JSON-RPC probe script). Most of them are now pinned as regression tests against real captured server responses under [`tests/fixtures/lsp-probe/`](../../tests/fixtures/lsp-probe/):

* Real-server, end-to-end (need node/npm, or `gopls` on `PATH` for the Go tests; `#[ignore]`d by default, run locally with `cargo test -- --ignored`): `callhierarchy_incoming_from_module_scope_synthesizes_module_caller`, `callees_handles_pyright_null_outgoing_calls_as_empty`, `references_resolve_cross_file_without_preemptive_didopen`, `workspace_symbol_returns_empty_array_for_pyright`, `find_callees_corrects_interface_dispatch_to_the_implementing_class`, `find_callees_corrects_interface_dispatch_to_the_implementing_class_for_go` (`src/query/pool.rs`); `index_language_real_pyright`, `index_language_real_tsserver`, `index_language_real_gopls` (`src/indexer/runner.rs`).
* Fast, offline, against a captured response (ordinary `cargo test`, no LSP server needed): `parse_locations_matches_captured_pyright_definition`, `parse_locations_matches_captured_tsserver_locationlink`, `parse_locations_matches_captured_tsserver_implementation`, `parse_locations_matches_captured_gopls_definition`, `parse_locations_matches_captured_gopls_implementation` (`src/query/lsp_query.rs`); `construct_from_hover_ignores_self_at_classname_notation`, `construct_from_hover_extracts_type_keyword_from_fenced_code_block` (`src/adapters/kind.rs`); `flatten_includes_pyright_self_attribute_as_a_child`, `flatten_keeps_tsserver_overload_duplicates_as_separate_entries`, `flatten_keeps_gopls_receiver_method_as_a_top_level_sibling` (`src/indexer/symbol.rs`); `unhandled_method_error_surfaces_pyrights_exact_envelope` (`src/lsp/client.rs`); `outgoing_call_to_interface_method_is_corrected_to_the_implementing_class`, `outgoing_call_to_interface_method_is_corrected_for_go`, and their Python-gate/fallback siblings (`src/query/resolver.rs`).

Not yet pinned: the `typeDefinition` null-vs-`[]` semantics and the stdlib `.pyi`/`.py` double-`Location` split — `type_of` remains unimplemented (no consumer needs it yet, per `docs/design/graph-model.md`'s edge_type table; `implements` was implemented once `find_callees` needed it, see above), so there's no parser to pin these against. Capturing them is deferred to whichever future theme implements `type_of`.

This suite already caught two real bugs the original ad hoc probes missed: a module-scope call-hierarchy caller could be misattributed to a real symbol that happened to start at the same `(0,0)` position as pyright/tsserver's synthetic sentinel (fixed in `src/query/resolver.rs`), and `initialize` never actually declared `linkSupport`, so tsserver silently fell back to a plain `Location` instead of the `LocationLink` this doc always claimed (fixed in `src/lsp/handshake.rs`).
