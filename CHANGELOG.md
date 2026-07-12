# Changelog

Notable changes per release. The release workflow uses the matching section
as the GitHub release notes (auto-generated notes are the fallback).

## Unreleased

### Added

- **Go language support (gopls)**: a fourth built-in `LanguageAdapter`
  alongside Python/TypeScript/Rust, with full feature parity (`find_symbol`,
  `find_definition`, `find_references`, `find_callers`, `find_callees`,
  `find_call_path`). `find_callees`'s interface-to-implementation correction
  (previously TS-only) now also covers Go's implicit interface satisfaction.
  `gopls` isn't npm-distributed — install it with
  `go install golang.org/x/tools/gopls@latest` and ensure it's on `PATH`,
  same as rust-analyzer.

## v0.0.2 (2026-07-10)

### Added

- **Tool discoverability**: `serve`/`daemon` advertise a "prefer these tools
  over grep/Read" directive via the MCP `InitializeResult.instructions` field,
  which reaches the agent even when a client defers a connected server's
  tools behind an explicit tool-search step.
- **fqn resolution hints**: `find_references`/`find_callers`/`find_callees`/
  `find_call_path` return `hint_fqns` (segment match, falling back to
  typo-tolerant fuzzy match) when a `fqn` resolves to no anchor at all,
  instead of an empty result indistinguishable from "genuinely zero
  callers/references".
- **`implements` edge for TypeScript**: `find_callees` now resolves a call
  through a TS interface method to the implementing class instead of the
  interface declaration itself.
- **Daemon reconnect**: `serve` transparently reconnects to a fresh daemon if
  the one it was using disappears mid-session (idle timeout, an explicit
  `daemon stop`, or a crash), instead of failing every subsequent tool call
  for the rest of that `serve` process's life.
- **Faster daemon restart**: the startup-drift reconcile pass skips a file's
  LSP round-trip entirely when its on-disk content hash matches the last
  committed reconcile, instead of re-fetching every file on every restart.

### Fixed

- Daemon restart now reconciles filesystem changes made while no daemon was
  running, which previously stayed invisible to the graph (#4).
- Reconciling a file no longer hits a `UNIQUE` constraint failure when
  same-named symbols swap position (#5).
- Daemon restart no longer silently drops a file's symbols when the LSP
  returns a false-empty `documentSymbol` response during warm-up, and no
  longer silently commits a still-unavailable LSP server's result as if it
  had succeeded (#6, #7).

## v0.0.1 (2026-07-05)

### Added

- **Semantic Graph MCP server**: persists LSP query results (`documentSymbol`,
  references, call hierarchy) into a SQLite-backed graph so agents query the
  graph instead of repeatedly round-tripping to the LSP or reading whole files.
- **8 MCP tools** over stdio: `find_symbol`, `find_definition`,
  `find_references`, `find_callers`, `find_callees`, `find_call_path`,
  `read_range`, plus the `restart_lsp` maintenance tool.
- **On-demand edge construction**: reference/call-hierarchy edges are built by
  querying the LSP the first time they're needed, then served from the cache
  on subsequent queries.
- **Filesystem watcher** keeps the graph in sync with on-disk edits, including
  rename tracking, while a daemon is running for a given root.
- **daemon/serve split**: `serve` is a stateless proxy over a Unix socket to a
  persistent `daemon` that owns the LSP servers and the graph, so both stay
  warm across repeated MCP client connections.
- **Language support**: Python (pyright), TypeScript (typescript-language-server),
  Rust (rust-analyzer), with LSP servers auto-provisioned via npm where needed.
- **Resilience**: an LSP health state machine (backoff, automatic restart-on-failure)
  and a graceful shutdown escalation (`shutdown`/`exit` → `SIGTERM` → `SIGKILL`).
- **Observability**: tracing spans behind `SEMNAV_LOG` to separate LSP-bound
  time from semnav's own work.
