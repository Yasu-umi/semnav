# Graph Model & Schema

The logical model of the Semantic Graph ([Graph Model](#graph-model)) and its physical schema ([Schema (0.0.1)](#schema-001)).

## Graph Model

### Nodes

* Repository / Package / Module / File / Class / Interface / Function / Method / Variable (optional)

Each Node has:

* Stable Symbol ID
* URI / Range / Signature / Kind / Documentation / Optional Summary

Range is used by agents to read only the minimal necessary code.

### Stable Symbol ID

Node identity is **hybrid**: name identity and content identity are kept separate.

* **Primary key (name identity)**: FQN (fully qualified name). Human-readable, used as the search key
* **dirty detection (content identity)**: range (start position) combined with the signature fingerprint (`signature_hash`)

Each Node holds:

* `fqn`: normalized fully qualified name (primary key)
* `range`: `{ uri, start_line, start_col }` (change anchor, used for rename tracking)
* `signature_hash`: fingerprint derived from the signature (used to detect content changes)

#### Normalization rules

* **Overloads (TypeScript)**: multiple candidates collide under the same FQN, so the FQN is disambiguated by appending arity or a type signature. E.g. `app.repo.load#1`, `load[number]`
* **Python**: no overloads, so the FQN is used as-is. `@typing.overload` is merged into the implementation

#### dirty lifecycle

1. The FS watcher detects a file change → Node/Edge entries under the affected File are marked `valid=false`
2. On the next query, `documentSymbol` is re-fetched
3. Each symbol is matched against the old Node by FQN + range (proximity):
   * **FQN match, fingerprint match** → content unchanged, `valid=true` is restored
   * **FQN match, fingerprint mismatch** → content changed, Edges are rebuilt via an LSP re-query
   * **FQN mismatch, range close** → treated as a **rename**. Edges from the old ID are re-pointed to the new FQN (see below)
   * **FQN mismatch, range far** → new symbol

#### Rename tracking

With the FQN alone, a `save` → `persist` rename would make the ID discontinuous, leaving Edges to the old ID as stale orphans. To prevent this:

* If LSP `textDocument/rename`-family events are available, they take priority: a mapping from old FQN → new FQN is built from them and Edges are inherited
* If not available, the old Node is identified via range-proximity matching and Edges are re-pointed the same way
* Old Nodes/Edges that could not be re-pointed are marked `orphan=true` and discarded after a grace period (see [Orphan Reclamation](./indexing-and-cache.md#orphan-reclamation))

### Edges

Derived from static analysis: `CONTAINS` / `CALLS` / `REFERENCES` / `TYPE_OF` / `IMPLEMENTS` / `INHERITS` / `IMPORTS` / `DEFINES`

Planned, derived from dynamic analysis: `CALLS_DYNAMIC`

## Schema (0.0.1)

The physical schema is stored in SQLite.

### nodes

Declared symbols. The logical primary key is FQN; the physical unique key is `(uri, range, name)`.

Key columns:

* `fqn` — fully qualified name (for search). Computed from the parent chain of `container_id`
* `uri` / `name` / `language` / `kind` (LSP SymbolKind numeric value) / `node_kind` (serialized NodeKind) / `construct` (auxiliary classification derived from hover)
* `container_id` — parent node (DocumentSymbol children nesting)
* `range_*` (whole declaration) / `sel_*` (selectionRange, the identifier)
* `signature` / `documentation` / `detail` — extracted from hover and documentSymbol
* `signature_hash` / `valid` / `orphan` — cache control. `generation` is unused in 0.0.1 (always 0) and reserved for future multi-stage grace period tuning — orphan reclamation implements two-strike using `orphan` alone
* `is_external` — separates external nodes (typeshed / node_modules / site-packages / .venv)

### edges

One row per relationship. Call-site locations (`fromRanges` / `originSelectionRange` / `Location.range`) are stored in `site_*`, and **1 edge = 1 fromRange** (so line-level invalidation works).

Key columns:

* `src_id` / `dst_id` — node references
* `edge_type` — see table below
* `site_*` — call/reference occurrence location (NULL for edges without a location, such as `contains`)
* `valid` — cache control

`edge_type` values:

| edge_type | source LSP method | meaning |
|---|---|---|
| `contains` | `documentSymbol` (children) | parent → child containment |
| `calls` | `callHierarchy/outgoingCalls` | caller → callee |
| `references` | `references` | referrer → referenced |
| `type_of` | `typeDefinition` | variable/field → type |
| `implements` | `implementation` | interface → implementation (**TS only**, Python not supported) |
| `defines` | `definition` | usage site → declaration |
| `inherits` | (planned) | class inheritance |
| `imports` | (planned) | import relationship |

> For the real-world return structure and per-language differences of the LSP method behind each edge, see [lsp-integration.md](./lsp-integration.md).

### events

Runtime Events (planned, see [Tracer Plugin Contract](./dynamic-graph.md#tracer-plugin-contract)). In 0.0.1 only the table slot is reserved; nothing is written.

### index_meta

Key-value store. Records schema version, LSP version, indexing progress, files that have completed `didOpen`, etc.

### Handling of external nodes

Externally resolved nodes such as typeshed / node_modules / site-packages / .venv are stored with `is_external=1` and **excluded from default queries** so they don't pollute the user's code graph. Boundary determination (based on real-world observation):

`is_external=1` ⟺ any of the following (OR, biased toward the safe side):

1. Matches a **positive external-prefix list** (against the path portion of `uri`):
   * `/typeshed-fallback/` (pyright-bundled typeshed)
   * `/node_modules/`
   * `/site-packages/`
   * `/.venv/lib/` (standard venv layout) + regex `<venv-root>/lib/python<ver>/` (pyenv/pip standard library implementation)
2. **Not under rootUri** (this condition is disabled when rootUri is unset)

Why both are implemented: (1) alone risks misclassifying an in-project `node_modules` directory; (2) alone risks treating everything as external if rootUri is misconfigured.

**stdlib split (observed in practice)**: for a standard-library import, pyright returns **two Locations simultaneously**: a `.pyi` (typeshed) and a `.py` (implementation). Both are stored with `is_external=1`, so the same symbol splits into two nodes, but since the default query (`include_external=false`) excludes them, this causes no practical harm. De-duplication is a 0.1+ concern.

The complete DDL lives in `migrations/V0001__init.sql`.
