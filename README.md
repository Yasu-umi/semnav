# semnav

A Semantic Graph MCP server that persistently caches LSP results, so AI coding agents can explore a codebase's structure without repeatedly re-running LSP queries or reading whole files.

## Why

AI coding agents typically explore a codebase with grep, file reads, LSP, and embedding search. In practice, the cost of **deciding what to read** dominates the cost of reading the code itself, and passing whole files to the LLM burns tokens. LSP already holds rich semantic information (definitions, references, type definitions, implementations, call hierarchies, document symbols), but its API is query-based — there's no interface that returns "the semantic graph of the whole repository."

semnav closes that gap: it persists LSP query results into a SQLite-backed graph and exposes it over MCP, so agents query the **graph**, not the LSP, and read source text only for the specific range they need.

See [docs/vision.md](docs/vision.md) for the full rationale and [docs/design/](docs/design/) for the detailed design (graph schema, LSP integration notes, indexing/cache/invalidation, resilience, crate structure).

## How it works

* The graph is a **cache of LSP results**, not a source of truth — when it's stale, it's re-evaluated via LSP on the next query ([docs/design/indexing-and-cache.md](docs/design/indexing-and-cache.md)).
* Nodes carry only metadata (`fqn` / `uri` / `range` / `kind` / `signature`, ...); source text is never cached in the graph and is read from disk on demand via `read_range`.
* Edges that haven't been built yet (references / call hierarchy) are constructed on demand by querying the LSP, then cached for next time.
* A filesystem watcher keeps the graph in sync with on-disk edits while a `daemon` is running for `<root>`, including rename tracking (see [docs/design/graph-model.md](docs/design/graph-model.md)).
* Semantic analysis is fully delegated to the underlying LSP server, so any language with an LSP implementation can in principle be supported. Currently supported: **Python** (pyright), **TypeScript** (typescript-language-server), and **Rust** (rust-analyzer).

## Requirements

* Rust (2024 edition)
* Node.js + npm (semnav provisions `pyright` and `typescript-language-server` via npm on first `index`)

## Installation

```sh
git clone https://github.com/Yasu-umi/semnav.git
cd semnav
cargo build --release
```

The binary is produced at `target/release/semnav`.

## Usage

```
semnav discover <root>   list source files (Python/TS/Rust) under <root>
semnav index <root>      index <root> into <root>/.semnav/graph.db
                         (provisions pyright/tsserver via npm, needs node + npm;
                          rust-analyzer must already be on PATH, e.g. via rustup)
semnav serve <root>      serve the 8 MCP tools over stdio, proxied to a
                         background daemon (auto-started; run `index` first)
semnav daemon <root>     run the persistent daemon directly (usually auto-started by `serve`)
semnav daemon stop <root> stop a running daemon for <root>
```

`serve` holds no state itself: it auto-starts (or reuses) a background `semnav daemon <root>` process that owns the LSP servers and the graph, and forwards every tool call to it over a Unix socket. This lets the daemon stay warm — and keep LSP servers indexed — across repeated `serve` connections from the MCP client (see [docs/design/daemon-lifecycle.md](docs/design/daemon-lifecycle.md)).

Environment:

* `SEMNAV_CACHE_DIR` — override the index/cache directory (default: `<root>/.semnav`)
* `SEMNAV_DAEMON_IDLE_TIMEOUT_SECS` — daemon self-shutdown after this many idle seconds (default: 1800)
* `SEMNAV_INITIALIZE_TIMEOUT_SECS` — LSP `initialize` handshake timeout (default: 60)
* `SEMNAV_DOCUMENT_SYMBOL_TIMEOUT_SECS` — LSP `textDocument/documentSymbol` timeout (default: 30)
* `SEMNAV_QUERY_TIMEOUT_SECS` — query-time LSP round-trip timeout for `find_references`/`find_callers`/`find_callees`/etc. (default: 150)
* `SEMNAV_LSP_<LANG>_COMMAND` — override the LSP server binary for `<LANG>` (`PYTHON`/`TYPESCRIPT`/`RUST`, upper-cased `language_name()`), bypassing `PATH`/npm-install resolution entirely — point it at a custom build or wrapper script
* `SEMNAV_LSP_<LANG>_ARGS` — extra args appended to that language's LSP server startup command (space-separated), e.g. `SEMNAV_LSP_RUST_ARGS="--log-file /tmp/ra.log"`
* `SEMNAV_LOG` — tracing filter (`RUST_LOG`-style syntax, e.g. `SEMNAV_LOG=semnav=debug`); silent by default (see [docs/design/observability.md](docs/design/observability.md))

All of these are ordinary process environment variables, so an MCP client that launches `semnav serve` (e.g. via `.mcp.json`'s `env` field) can set them per-project without any semnav-specific protocol support.

### Typical flow

```sh
semnav index /path/to/your/project
semnav serve /path/to/your/project
```

`serve` speaks MCP over stdio, so it's meant to be launched by an MCP client (e.g. registered as an MCP server in your agent's config) rather than run interactively. For example, in a project's `.mcp.json` (Claude Code) or an equivalent MCP client config:

```json
{
  "mcpServers": {
    "semnav": {
      "command": "/path/to/semnav",
      "args": ["serve", "/path/to/your/project"]
    }
  }
}
```

## MCP tools

`serve` exposes 8 tools: 7 query tools (`find_symbol`, `find_definition`, `find_references`, `find_callers`, `find_callees`, `find_call_path`, `read_range`) plus a `restart_lsp` maintenance tool for forcing a wedged language server to restart. Full input/output schemas are documented in [docs/design/mcp-tools.md](docs/design/mcp-tools.md).

## Documentation

* [docs/vision.md](docs/vision.md) — motivation and goals
* [docs/design/](docs/design/) — graph schema, LSP integration, indexing/cache/invalidation, resilience, crate structure, and more

## License

[MIT](LICENSE)
