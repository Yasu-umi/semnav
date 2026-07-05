# Changelog

Notable changes per release. The release workflow uses the matching section
as the GitHub release notes (auto-generated notes are the fallback).

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
