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
* A filesystem watcher keeps the graph in sync with on-disk edits while `serve` is running, including rename tracking (see [docs/design/graph-model.md](docs/design/graph-model.md)).
* Semantic analysis is fully delegated to the underlying LSP server, so any language with an LSP implementation can in principle be supported. Currently supported: **Python** (pyright) and **TypeScript** (typescript-language-server).

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
semnav discover <root>   list source files (Python/TS) under <root>
semnav index <root>      index <root> into <root>/.semnav/graph.db
                         (provisions pyright/tsserver via npm; needs node + npm)
semnav serve <root>      serve the 6 MCP tools over stdio against
                         <root>/.semnav/graph.db (run `index` first)
```

Environment:

* `SEMNAV_CACHE_DIR` — override the index/cache directory (default: `<root>/.semnav`)

### Typical flow

```sh
semnav index /path/to/your/project
semnav serve /path/to/your/project
```

`serve` speaks MCP over stdio, so it's meant to be launched by an MCP client (e.g. registered as an MCP server in your agent's config) rather than run interactively.

## MCP tools

`serve` exposes 6 tools: `find_symbol`, `find_definition`, `find_references`, `find_callers`, `find_callees`, `read_range`. Full input/output schemas are documented in [docs/design/mcp-tools.md](docs/design/mcp-tools.md).

## Documentation

* [docs/vision.md](docs/vision.md) — motivation and goals
* [docs/design/](docs/design/) — graph schema, LSP integration, indexing/cache/invalidation, resilience, crate structure, and more

## License

[MIT](LICENSE)
