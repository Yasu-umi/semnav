# Crate Structure (0.0.1)

> Finalized crate/module structure for the 0.0.1 implementation. Each decision point was worked backward from the architecture decisions and each design doc (2026-07-02). Originally a "pre-implementation review memo," but promoted to a finalized structure once all 6 decision points were settled.

## Premises (finalized)

* Implementation language: **Rust**, distributed as a **single binary**
* Dependency crates (finalized):
  * `rmcp` — MCP server SDK (exposes 6 tools)
  * `rusqlite` (+ bundled SQLite, WAL) — persistent cache
  * `refinery` — migration runner (`embed_migrations!`, manages `migrations/*.sql`). Decision Point 3
  * `notify` — FS watcher (invalidation)
  * `ignore` — file discovery respecting `.gitignore` (file discovery in [indexing-and-cache.md](./indexing-and-cache.md))
  * `serde` / `serde_json` — DTOs, JSON-RPC
  * `tokio` (multi-thread) — async runtime (integrates rmcp/notify/watcher/db actor)
* Rejected: `async-lsp` (Decision Point 2)

## Finalized structure — single crate with module split (Option A)

```
semnav/                 # bin crate
  Cargo.toml
  migrations/V0001__init.sql
  src/
    main.rs             # bin, cli, cache_dir resolution
    mcp/                # rmcp server, tools, DTOs
    graph/              # SQLite, CRUD, migration, is_external, db actor
    lsp/                # process, state machine, JSON-RPC, health
    adapters/           # LanguageAdapter trait + pyright/tsserver + provisioning
    indexer/            # file discovery, documentSymbol collection, watcher, invalidation
    query/              # on-demand edges, SymbolRef, read_range
```

* Fits the 0.0.1 scale, compiles fast, and is irrelevant to distribution (single binary either way)
* Even if a workspace promotion is needed when adding a language, keeping trait boundaries inside modules keeps migration cost low

## Concerns map (design docs → module responsibilities)

Breakdown of each design doc's expected responsibility into the implementation module that owns it (authoritative).

| Module | Responsibility | Basis doc |
|---|---|---|
| `mcp` | rmcp server, 6 tools (`find_symbol`/`definition`/`references`/`callers`/`callees`/`read_range`), DTOs, pagination cursor, degraded responses | [mcp-tools.md](./mcp-tools.md) / [resilience.md](./resilience.md) |
| `graph` | SQLite (nodes/edges/events/index_meta) CRUD, `valid`/`orphan`/`generation`, FQN construction, `is_external` determination, refinery migration, **db actor** (sole ownership of the Connection) | [graph-model.md](./graph-model.md) |
| `lsp` | Child process management (spawn/exit monitoring), health state machine, backoff, timeouts, **thin homegrown JSON-RPC** (Content-Length framing + id pairing), `workspaceFolders`/didOpen/didChange, health (`index_meta` KV) | [lsp-lifecycle.md](./lsp-lifecycle.md) |
| `adapters` | `LanguageAdapter` trait, pyright/tsserver implementations, provisioning (isolated npm install), `map_symbol_kind`/`NodeKind`, hover refinement (`construct` extraction) | [language-adapters.md](./language-adapters.md) |
| `indexer` | File discovery (`ignore`), serial documentSymbol collection, FS watcher (`notify`), invalidation flow, orphan reclamation | [indexing-and-cache.md](./indexing-and-cache.md) |
| `query` | On-demand edge construction (definition/references/callHierarchy/typeDefinition/implementation), `SymbolRef` resolution (fqn\|at), Filter, `read_range` (direct FS read) | [mcp-tools.md](./mcp-tools.md) / [indexing-and-cache.md](./indexing-and-cache.md) |
| `bin` / `cli` | Entry point, `discover`/`index`/`serve` CLI, `SEMNAV_CACHE_DIR`/`.semnav` resolution, provisioning guidance messages, `shutdown` (SIGTERM/SIGKILL) | [language-adapters.md](./language-adapters.md) Distribution / [lsp-lifecycle.md](./lsp-lifecycle.md) Shutdown |

> `mcp` (rmcp boundary, DTO shaping) calls `query` (Graph↔LSP orchestration). The boundary between them was finalized as separate in Decision Point 5.

## Decisions and rationale

### Decision Point 1: single crate (module split) — adopted / workspace rejected

* Single-binary distribution → crate splitting is irrelevant to distribution (either way it's a single bin)
* At 0.0.1 scale, the incremental-build advantage of a workspace is negligible, and the glue cost outweighs it
* Test boundaries and future extensibility are secured by **creating trait boundaries within modules**. If a need arises to support language/tracer plugins via dlopen, promote to a workspace (`semnav-core` / `semnav-adapters` / `semnav-mcp` / `semnav`)

### Decision Point 2: thin homegrown JSON-RPC — adopted / `async-lsp` rejected

What `async-lsp` can provide is limited to JSON-RPC framing, request/response pairing, notification reception, and `$` methods. The **child process spawn/exit monitoring, health state machine, backoff-based restart, timeout monitoring, provisioning, and `index_meta` health KV** required by [lsp-lifecycle.md](./lsp-lifecycle.md) all must be built in-house regardless.

* **Owning Child + stdio directly** integrates cleanly with this process monitoring and health transition logic. This avoids the concern that `async-lsp`'s transport/MainLoop and our own process monitoring responsibilities would overlap, requiring an absorbing wrapper
* **Content-Length framing + id pairing is small**, and the probe in [lsp-integration.md](./lsp-integration.md) has **already driven all LSP methods with a homegrown harness** (proven)
* Removes one dependency (frees us from following `async-lsp`'s transport breaking changes)

Implementation notes:
* Content-Length header + body framing, response pairing via an `id` → pending map, notification reception, `$/cancelRequest` (0.0.1 abandons via timeout and discards responses for unrecognized ids)
* Timeouts use the values from [lsp-lifecycle.md](./lsp-lifecycle.md) (initialize=60s / documentSymbol=30s / query=150s)

### Decision Point 3: `refinery` (`embed_migrations!`) — adopted

The design treats the SQL files `migrations/*.sql` as the source of truth ([graph-model.md](./graph-model.md): "the complete DDL is migrations/V0001__init.sql").

* `embed_migrations!` embeds and manages the SQL files as-is → **fully consistent with SQL-file-based operations**, proven in practice, embeddable in a single binary
* Rejected: `rusqlite_migration` (defines SQL in Rust code, which clashes with SQL-file-based operations); a homegrown sequential runner (`PRAGMA user_version` + `include_str!` has zero dependencies, but would require self-managing order/checksum bookkeeping as it grows in the future)

### Decision Point 4: dedicated thread (actor) + mpsc — adopted / `spawn_blocking`+`Mutex` rejected

`rusqlite` is synchronous (blocking). Even with WAL, writes must be serialized.

* **Sole ownership of the Connection** (db actor) → serialized writes (a WAL requirement), no `Mutex` needed, batching/priority assignment possible
* In 0.0.1, both reads and writes are unified into the actor. If read latency becomes an issue, a dedicated read connection (parallel WAL reads) can be added later

Implementation pattern (idiomatic):
* Receive commands via `tokio::sync::mpsc::channel`, reply to the caller via `tokio::sync::oneshot`
* The db actor is driven by `tokio::task::spawn_blocking(move || loop { rx.blocking_recv()... })` (`blocking_recv` is for use only on threads outside the runtime, which `spawn_blocking` guarantees)
* `oneshot::Sender::send` is synchronous, so it can be returned directly from the blocking thread
* FS watcher (`notify`) events fan in to the index/invalidation task

### Decision Point 5: `mcp` and `query` are separated — adopted

* `query` = Graph↔LSP orchestration (domain). **Unit-testable independent of rmcp**
* `mcp` = rmcp boundary, DTO shaping, cursor, attaching degraded status. `mcp` calls `query`
* Reason not to merge them: `query`'s on-demand edge construction (cache lookup → LSP fallback → UPSERT → response) is domain logic and should not be mixed with the transport layer

### tracer crate (dynamic-graph) — not reserved in 0.0.1

[dynamic-graph.md](./dynamic-graph.md) is Future work. 0.0.1 only reserves the `events` table slot ([graph-model.md](./graph-model.md)). A crate boundary will be carved out when the tracer is added in the future.

## Implementation Status

The structure above and all 6 decision points have been fully implemented (including `migrations/V0001__init.sql`, the db actor, and the homegrown JSON-RPC client).
