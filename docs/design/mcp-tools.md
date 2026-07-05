# MCP Tools (0.0.1)

The tool interface that the Semantic Graph MCP exposes to agents. 0.0.1 provides **7 query tools** (`find_symbol` / `find_definition` / `find_references` / `find_callers` / `find_callees` / `find_call_path` / `read_range`) plus **1 maintenance tool** (`restart_lsp`, see below).

> MCP tool interface (C). See [graph-model.md](./graph-model.md) for the internal schema, [lsp-integration.md](./lsp-integration.md) for the LSP origin of each edge, and [indexing-and-cache.md](./indexing-and-cache.md) for on-demand Edge construction.

> **Runtime topology (2026-07, daemon step):** from the MCP client's perspective nothing below changed — same 8 tools, same wire format. Internally, `semnav serve` (what the client actually spawns) no longer executes these tools itself; it proxies each call to a persistent background `semnav daemon` process. See [daemon-lifecycle.md](./daemon-lifecycle.md).

> **Discoverability:** `ProxyServer`/`SemnavServer` set `InitializeResult.instructions` (via `#[tool_handler(instructions = "...")]`) to a short "prefer these tools over grep/Read" directive. Some MCP clients surface a connected server's `instructions` unconditionally at session start, even for tools the client itself defers behind an explicit tool-search step — this is the one channel guaranteed to reach the agent before it falls back to grep.

## Design principles

1. **The Graph does not hold body text.** The Graph keeps only range metadata (`range_*`/`sel_*`); source code body is read from the FS on demand by `read_range`. This keeps it always up to date, avoids the need for didChange sync, and keeps the Graph size down.
2. **On-demand Edges.** An edge that hasn't been built yet is constructed via UPSERT by querying the LSP at request time, and the result is returned ([indexing-and-cache.md](./indexing-and-cache.md)). The agent doesn't need to be aware of whether it's "already built."
3. **External nodes are excluded by default.** Nodes originating from typeshed / node_modules / site-packages / .venv (`is_external=1`) appear only when `include_external=true`.
4. **Returned DTOs never include body text.** A Node contains only metadata such as `fqn`/`uri`/`range`/`kind`/`signature`. If body text is needed, use `read_range`.

## Common types

### Range / Position

LSP-compliant. **Both line and column are 0-based.**

```
Position = { line: uint, character: uint }
Range    = { start: Position, end: Position }
```

> Note for agents: Claude Code's Read tool etc. use 1-based lines, but semnav's interface aligns with LSP and is uniformly **0-based**. This is called out explicitly in documentation and error messages wherever relevant.

### SymbolRef

Input that identifies a target symbol. Specify **either `fqn` or `at`** (not both).

```
SymbolRef =
  | { fqn: string }                       // fully qualified name (primary search key, human-readable)
  | { at: { uri: string, line: uint, character: uint } }  // any occurrence position (LSP-native, robust to staleness)
```

* `fqn`: the normalized FQN matching the `fqn` column in [graph-model.md](./graph-model.md) — **exact match only** (unlike `find_symbol`'s `pattern`, which defaults to matching any dot-delimited segment). A bare or wrong-prefixed name doesn't resolve; see `hint_fqns` below
* `at`: **any occurrence position** of the symbol (declaration, reference, or call site). The Graph resolves it to the node containing that position

Reason both modes are supported: agents want to ask both "what's the definition at this reference position?" (position) and "who calls `app.repo.save` (the full FQN)?" (name). Position stays accurate even mid-rename; FQN is human-readable but must be exact.

### `hint_fqns` — recovering from a wrong `fqn`

`find_references`/`find_callers`/`find_callees`/`find_call_path` all resolve a `SymbolRef::Fqn` via the same exact-match lookup. Since a caller often knows a symbol's short name but not its full FQN, a bare or wrong-prefixed `fqn` would otherwise return an empty result indistinguishable from "this symbol genuinely has zero callers/references" (issue #3).

Whenever that lookup finds no anchor at all, the response's `hint_fqns` (`find_call_path`: `from_hint_fqns`/`to_hint_fqns`, evaluated independently per endpoint) is filled with FQNs sharing the requested name's last dot-segment — the same signal `find_symbol`'s default `match="segment"` uses — capped at 10 entries. An empty `hint_fqns` on a no-anchor result means no similarly-named symbol exists either; a non-empty one is a pointer to retry with `find_symbol` or the suggested FQN directly. `hint_fqns` never applies to `at`: an unresolvable position is a normal LSP-null outcome, not a naming problem.

### Filter (optional parameter for all find_* tools)

```
Filter = {
  language?:         "python" | "typescript" | "rust",
  kind?:             string[],          // inclusion array of kind_label. e.g. ["Function","Method"]
  include_external?: bool = false,      // false=exclude external nodes (default)
}
```

### Page (references / callers / callees)

```
Page(in)  = { limit?: uint = 100, cursor?: string }   // cursor is an opaque token
Page(out) = { next_cursor?: string }                  // if present, there is a continuation page
```

* **Stable sort key**: `(fqn, uri, start_line, start_char)`. The cursor encodes the resume position for this key
* `total` is not exposed. Producing it accurately would require exhaustively scanning LSP's `references()`/`callHierarchy`, which is incompatible with pagination. Whether to continue is determined by the presence/absence of `next_cursor`

### Node (returned DTO)

Does not include body text. Columns correspond to `nodes` in [graph-model.md](./graph-model.md).

```
Node = {
  fqn:              string,
  uri:              string,
  name:             string,
  language:         "python" | "typescript" | "rust",
  kind_label:       string,          // "Function" / "Class" / "TypeAlias" etc. (normalized below)
  kind_num:         uint,            // raw LSP SymbolKind value
  construct?:       string,          // auxiliary classification from hover ("type"/"interface"/...) [language-adapters.md]
  range:            Range,           // entire declaration
  selection_range:  Range,           // identifier (selectionRange)
  signature?:       string,          // signature from hover
  documentation?:   string,          // docstring/JSDoc from hover
  is_external:      bool,
}
```

## Tool definitions

### find_symbol — search for a symbol by name

```
input  = {
  pattern:         string,
  match?:          "segment" | "contains" | "exact" = "segment",
  ignore_case?:    bool = false,
  brief?:          bool = false,
  with_signature?: bool = false,
} & Filter & Page
output = { nodes: Node[], fqns: string[], next_cursor?: string }
```

* **`match="segment"` (default)**: matches against **each segment** of the FQN split by its delimiter. `save` hits `app.repo.save` (trailing `save`), but not `preserve`. A default that's neither too broad nor too narrow
* `contains`: substring match (`%save%`). Nothing is missed, but it also hits `preserve`
* `exact`: exact FQN match
* **`brief`**: when `true`, the response fills `fqns` (just the matched FQN strings) and leaves `nodes` empty, instead of the reverse. A wide `match="contains"` pattern can page through hundreds of full `Node`s (`range`/`signature`/`documentation`/...) and blow past a response token budget; `brief` lets a caller gauge match count or narrow the pattern first, before paying for full metadata.
* **`with_signature`**: best-effort hover backfill of `signature` for each returned node that doesn't already have one — see "Populating `signature`" below. No-op when `brief` is set (there are no `Node`s to enrich).

### find_definition — usage position → declaration

```
input  = { at: { uri, line, character } }    // position required
output = { nodes: Node[] }                    // may contain multiple entries for overloads
```

* **`at` is required** (`fqn` is not allowed). Finding a definition starts from a "usage position." To search by name, use `find_symbol`
* Origin: `textDocument/definition` (`defines` edge). pyright returns `Location[]` / tsserver returns `LocationLink[]` ([lsp-integration.md](./lsp-integration.md))
* **Null is a normal outcome** (type resolution failure, unresolved import) → returns `nodes: []`

### find_references — list of referencing sites

```
input  = SymbolRef & Filter & Page
output = {
  references: [{ node: Node, sites: Range[] }][],   // node=referencing site, sites=reference positions within that same file
  next_cursor?: string,
  hint_fqns: string[],  // non-empty only when `fqn` resolved to no anchor at all — see "hint_fqns" above
  refreshing?: bool,    // true only when a background refresh was kicked off; see resilience.md
}
```

* Origin: `textDocument/references` (`references` edge). Cross-file workspace scan
* `context.includeDeclaration` is **true** (the declaration itself is included among references)
* **Can grow large** (for popular symbols) → paginated via Page
* **Cache-first + background refresh**: a previously-seen anchor is served from the cache immediately, with a fresh materialization running in the background (`refreshing: true`); a first-ever query for an anchor blocks on one materialization instead, so it's never a false empty ([lsp-integration.md](./lsp-integration.md), [resilience.md](./resilience.md))

### find_callers — call sites of the caller

```
input  = SymbolRef & Filter & Page & { with_signature?: bool = false }
output = {
  callers: [{ node: Node, call_sites: Range[] }][],   // node=caller, call_sites=positions where the call occurs
  next_cursor?: string,
  hint_fqns: string[],  // non-empty only when `fqn` resolved to no anchor at all — see "hint_fqns" above
  refreshing?: bool,    // true only when a background refresh was kicked off; see resilience.md
}
```

* Origin: `callHierarchy/incomingCalls` (`calls` edge). `fromRanges` becomes `call_sites`
* **The `calls` edge cannot be built from documentSymbol alone** → built on first use on demand by querying callHierarchy ([lsp-integration.md](./lsp-integration.md))
* The caller of a module-scope call (`main()`) becomes a `(module)` node (`kind=2`)
* **Cache-first + background refresh**: same contract as `find_references` above — a warm anchor is served from the cache with `refreshing: true` while a fresh materialization runs in the background; a cold anchor blocks once ([lsp-integration.md](./lsp-integration.md), [resilience.md](./resilience.md))
* **`with_signature`**: best-effort hover backfill of `signature` on each returned `node` — see "Populating `signature`" below

### find_callees — call targets

```
input  = SymbolRef & Filter & Page & { with_signature?: bool = false }
output = {
  callees: [{ node: Node, call_sites: Range[] }][],   // node=call target, call_sites=positions where the call occurs within that function
  next_cursor?: string,
  hint_fqns: string[],  // non-empty only when `fqn` resolved to no anchor at all — see "hint_fqns" above
}
```

* Origin: `callHierarchy/outgoingCalls` (`calls` edge)
* tsserver: `to` sometimes points to the **type-resolved target (an interface method)** rather than the concrete implementation actually invoked through it. `src/query/resolver.rs`'s `resolve_outgoing_callee` corrects this: when the resolved callee's container is a TS `Interface` node, it calls `textDocument/implementation` on that method and redirects the `calls` edge to the concrete class's method (persisting an `implements` edge, `interface method → concrete method`, alongside it). Gated on the anchor's language being `"typescript"` — pyright answers `-32601 Unhandled method` for `implementation` ([lsp-integration.md](./lsp-integration.md)), so Python never reaches this path. An empty/failed `implementation` call falls back to the uncorrected interface-method edge, never worse than not having this correction at all. `implements` has no MCP-visible surface of its own — it's internal plumbing that only `find_callees`/`find_call_path` benefit from
* **Precise content-hash cache, not cache-first + background refresh**: unlike `find_callers`/`find_references`, the callee list is fully determined by the anchor's own file, so a byte-identical anchor file since the last materialization serves an exact cached answer with no LSP call and no `refreshing` field at all ([lsp-integration.md](./lsp-integration.md))
* **`with_signature`**: best-effort hover backfill of `signature` on each returned `node` — see "Populating `signature`" below

### find_call_path — multi-hop reachability between two symbols

```
input  = {
  from:          SymbolRef,
  to:            SymbolRef,
  max_depth?:    uint = 8,    // clamped to [1, 20]
  max_lsp_calls?: uint = 30,  // clamped to [0, 200]
}
output = {
  reachable:      bool,
  path:           Node[],      // from -> ... -> to inclusive; empty when reachable=false
  limit_reached:  bool,
  from_hint_fqns: string[],    // non-empty only when `from` resolved to no anchor at all — see "hint_fqns" above
  to_hint_fqns:   string[],    // same, for `to`; evaluated independently — either or both can be non-empty
}
```

* Answers "does `from` reach `to` through zero or more `calls` hops?" — the multi-hop counterpart to `find_callers`/`find_callees`'s single hop, for layered-architecture questions like "does this entry point ever reach that low-level dependency?" without the caller manually chaining `find_callees` one layer at a time
* **Breadth-first**, so `path` is a *shortest* path when `reachable` is `true` (not necessarily the only one)
* **Always expands, bounded by a budget** — deliberately not cache-only + opt-in expansion. The `calls` graph is materialized lazily per query ([lsp-integration.md](./lsp-integration.md)), so a cache-only reachability answer can't distinguish "provably doesn't call" from "hasn't been queried yet." Every unvisited node's outgoing edges are refreshed via the same precise content-hash cache `find_callees` uses — a byte-identical file since the last materialization is free, a changed/never-seen file costs one unit of `max_lsp_calls`
* **`limit_reached`** is the field that keeps a `reachable: false` answer honest: it's `true` whenever the search stopped — `max_depth` hit, `max_lsp_calls` exhausted, or no LSP client available at all — *before* every reachable node could be proven exhausted. `{reachable: false, limit_reached: true}` means "not found within these limits," not a proof of non-reachability; only `{reachable: false, limit_reached: false}` is an exhaustive negative
* `from == to` (same resolved anchor) is trivially `reachable: true` with a one-node `path`, at zero LSP cost
* Origin: `callHierarchy/outgoingCalls`, the same edge type as `find_callees` (`calls`)

### Populating `signature`

`signature` is hover-derived, not carried in documentSymbol, so it's populated lazily rather than at index time ([lsp-integration.md](./lsp-integration.md)):

* **Always, for free**: `find_definition`'s `at` branch already calls `hover` once per resolved target to refine `construct` — that same round trip backfills `signature` too, at no extra LSP cost.
* **Opt-in elsewhere**: `find_symbol`/`find_callers`/`find_callees` don't otherwise touch the LSP to build their `Node`s, so backfilling `signature` broadly means one extra `hover` call per node still missing one — up to `MAX_PAGE_LIMIT` (500) per page. `with_signature=true` opts into that cost; the default (`false`) keeps these three tools a pure graph/cache read.
* **Persisted, not just attached to one response**: a successful backfill is written to the `nodes.signature` column immediately, so a later query for the same node (with or without `with_signature`) returns it from the cache without hovering again.
* **Best-effort**: a hover failure (timeout, null, no client available) is swallowed and simply leaves `signature` unset — it never turns an otherwise-successful page into an error, matching `construct`-refinement's own silent-failure contract.

### read_range — source body text for a given range

```
input  = {
  uri:   string,
  range?: Range,    // if omitted, the entire file
}
output = {
  uri:         string,
  content:     string,       // the text of the requested range
  range:       Range,        // the range actually read (if range was omitted, the whole file up to EOF)
  total_lines: uint,         // total line count of the file (for progress/position tracking)
}
```

* **Reads directly from the FS.** Body text is not cached in the Graph (SQLite) (design principle 1)
* Always returns the current file state. Even if the `range` of a dirty Node is passed in, the current file content is returned (callers should be aware of possible line drift)
* If `range` extends past the end of the file, the result is clipped

## restart_lsp — force a language server to restart

```
input  = {
  language?: string,   // if omitted, restart every provisioned language
}
output = {
  restarted: string[], // languages whose server was actually reset
}
```

* A **maintenance operation**, not a graph query — it returns no `Node`s and carries no `degraded`/`degrade_reason`/`lsp_status` fields (`docs/design/resilience.md`)
* Gracefully shuts down the current server for `language` (or all servers, if omitted) and drops it from the pool; the *next* on-demand query for that language transparently respawns it via the same lazy-start path a first-ever query takes — no separate "warm it back up" step is needed
* Exists because the automatic restart-on-failure policy ([lsp-lifecycle.md](./lsp-lifecycle.md)) only fires on detected crashes/timeouts — it does nothing for a server that's alive but wedged (e.g. serving stale/incomplete results without erroring). This is the only way to force recovery from that state short of restarting the whole process
* No-op (returns `restarted: []`) if the named language (or, for the omit-language form, any language) was never acquired in the first place

## kind_label normalization

Maps `kind_num` (LSP SymbolKind) to a human-readable label. Stringifies the same classification as `map_symbol_kind` ([language-adapters.md](./language-adapters.md)).

Primary mapping (LSP standard values):

| kind_num | kind_label |
|---|---|
| 1 | File |
| 2 | Module |
| 5 | Class |
| 6 | Method |
| 11 | Interface |
| 12 | Function |
| 13 | Variable\* |
| 23 | Struct |

\* `kind_num=13` collides between tsserver's TypeAlias and Variable. When `construct=type` (from hover), it is promoted to `kind_label="TypeAlias"` ([hover-based refinement in language-adapters.md](./language-adapters.md#refinement-via-hover)).

Custom values outside the LSP spec get `kind_label="Unknown(<num>)"`. Filter's `kind` is specified using this `kind_label` string.

## Errors and degradation

* **Null is a normal outcome** (definition/references/hover returning null) → returns an empty array. The Graph simply ends up with no edge ([lsp-integration.md](./lsp-integration.md) caveat 3)
* For degraded operation **when the LSP server misbehaves** (crash, unresponsive) → see [resilience.md](./resilience.md)
