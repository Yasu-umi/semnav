# MCP Tools (0.0.1)

The tool interface that the Semantic Graph MCP exposes to agents. 0.0.1 provides **6 query tools** (`find_symbol` / `find_definition` / `find_references` / `find_callers` / `find_callees` / `read_range`) plus **1 maintenance tool** (`restart_lsp`, see below).

> MCP tool interface (C). See [graph-model.md](./graph-model.md) for the internal schema, [lsp-integration.md](./lsp-integration.md) for the LSP origin of each edge, and [indexing-and-cache.md](./indexing-and-cache.md) for on-demand Edge construction.

> **Runtime topology (2026-07, daemon step):** from the MCP client's perspective nothing below changed — same 7 tools, same wire format. Internally, `semnav serve` (what the client actually spawns) no longer executes these tools itself; it proxies each call to a persistent background `semnav daemon` process. See [daemon-lifecycle.md](./daemon-lifecycle.md).

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

* `fqn`: the normalized FQN matching the `fqn` column in [graph-model.md](./graph-model.md)
* `at`: **any occurrence position** of the symbol (declaration, reference, or call site). The Graph resolves it to the node containing that position

Reason both modes are supported: agents want to ask both "what's the definition at this reference position?" (position) and "who calls the function named `save`?" (name). Position stays accurate even mid-rename; FQN is human-readable.

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
  pattern:      string,
  match?:       "segment" | "contains" | "exact" = "segment",
  ignore_case?: bool = false,
  brief?:       bool = false,
} & Filter & Page
output = { nodes: Node[], fqns: string[], next_cursor?: string }
```

* **`match="segment"` (default)**: matches against **each segment** of the FQN split by its delimiter. `save` hits `app.repo.save` (trailing `save`), but not `preserve`. A default that's neither too broad nor too narrow
* `contains`: substring match (`%save%`). Nothing is missed, but it also hits `preserve`
* `exact`: exact FQN match
* **`brief`**: when `true`, the response fills `fqns` (just the matched FQN strings) and leaves `nodes` empty, instead of the reverse. A wide `match="contains"` pattern can page through hundreds of full `Node`s (`range`/`signature`/`documentation`/...) and blow past a response token budget; `brief` lets a caller gauge match count or narrow the pattern first, before paying for full metadata.

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
}
```

* Origin: `textDocument/references` (`references` edge). Cross-file workspace scan
* `context.includeDeclaration` is **true** (the declaration itself is included among references)
* **Can grow large** (for popular symbols) → paginated via Page

### find_callers — call sites of the caller

```
input  = SymbolRef & Filter & Page
output = {
  callers: [{ node: Node, call_sites: Range[] }][],   // node=caller, call_sites=positions where the call occurs
  next_cursor?: string,
}
```

* Origin: `callHierarchy/incomingCalls` (`calls` edge). `fromRanges` becomes `call_sites`
* **The `calls` edge cannot be built from documentSymbol alone** → built on first use on demand by querying callHierarchy ([lsp-integration.md](./lsp-integration.md))
* The caller of a module-scope call (`main()`) becomes a `(module)` node (`kind=2`)

### find_callees — call targets

```
input  = SymbolRef & Filter & Page
output = {
  callees: [{ node: Node, call_sites: Range[] }][],   // node=call target, call_sites=positions where the call occurs within that function
  next_cursor?: string,
}
```

* Origin: `callHierarchy/outgoingCalls` (`calls` edge)
* tsserver: `to` sometimes points to the **type-resolved target (an interface method)** → the query side corrects this to the implementing class's method via the `implements` edge ([lsp-integration.md](./lsp-integration.md))

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
| 9 | Interface |
| 11 | Function |
| 12 | Variable |
| 13 | Variable\* |
| 22 | Struct |

\* `kind_num=13` collides between tsserver's TypeAlias and Variable. When `construct=type` (from hover), it is promoted to `kind_label="TypeAlias"` ([hover-based refinement in language-adapters.md](./language-adapters.md#refinement-via-hover)).

Custom values outside the LSP spec get `kind_label="Unknown(<num>)"`. Filter's `kind` is specified using this `kind_label` string.

## Errors and degradation

* **Null is a normal outcome** (definition/references/hover returning null) → returns an empty array. The Graph simply ends up with no edge ([lsp-integration.md](./lsp-integration.md) caveat 3)
* For degraded operation **when the LSP server misbehaves** (crash, unresponsive) → see [resilience.md](./resilience.md)
