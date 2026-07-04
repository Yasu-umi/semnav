# Language Adapters & Provisioning

## Language Adapter

0.0.1 targets **Python**, **TypeScript (tsserver)**, and **Rust (rust-analyzer)**.

> For the actual response structures per LSP method and the language differences (pyright vs tsserver), see [lsp-integration.md](./lsp-integration.md).

The LSP protocol itself is language-agnostic, so per-language differences are factored out into small adapters.

```rust
trait LanguageAdapter {
    fn file_extensions(&self) -> &[&str];
    fn server_command(&self) -> CommandSpec;   // pyright-langserver --stdio, etc.
    fn provision(&self) -> ProvisionedServer;  // detection or automatic installation
    fn map_symbol_kind(&self, lsp: u32) -> NodeKind;
}
```

## NodeKind

The LSP-standard `SymbolKind` is a protocol-common enum, and tsserver and pyright return the same values for it, so **no normalization is needed**. `NodeKind` holds the standard values as an enum, and falls back to a string for values outside the LSP spec so no information is lost.

```rust
enum NodeKind {
    Standard(SymbolKind),   // LSP standard values (Class / Function / ...)
    Custom(String),         // Label for non-standard values (e.g. TypeAlias). "Unknown(N)" if unrecognized
}
```

`map_symbol_kind` converts standard values directly into the enum, and non-standard values into a string. Each adapter maintains a table of its own server's known non-standard values mapped to labels.

### Refinement via hover

There are cases that can't be classified by standard `SymbolKind` alone. A representative example is TypeScript's **`type` alias**, which tsserver returns as `SymbolKind=13` (the same value as Variable) (confirmed on a live server).

For this reason, in addition to the first-pass approximation (pass-through) obtained from `documentSymbol`, `NodeKind` **extracts an auxiliary classification (`construct`: `type` / `interface` / `class` / `function` / `const`, etc.) from the leading keyword of the `hover` signature, and is promoted to `Custom` if a more accurate classification can be determined**. Even for `SymbolKind=13`, if `construct=type` is determined, it becomes `Custom("TypeAlias")`.

## LSP Server Provisioning

LSP server binaries are not bundled with the Rust binary; they are procured via **automatic detection and automatic installation**.

1. Detect the target language's LSP server from `PATH`
2. If not found, install it into an isolated directory
   * TypeScript: `typescript-language-server` (via npm)
   * Python: `pyright` (via npm) or basedpyright
   * Rust: not auto-installed — `rust-analyzer` isn't npm-distributed. `LanguageAdapter::server_package` returns `None` for it, which short-circuits step 2 into a clear error pointing at `rustup component add rust-analyzer` rather than attempting an npm install.
3. If the runtime requirements (Node.js / Python) are not met, guide the user through the installation steps with a clear error message

The binary is kept lightweight, and language runtimes ride on whatever is present in the environment.

## Distribution

Distributed as a single Rust binary. LSP servers and language runtimes are not bundled; they are procured automatically.

### Graph Storage

The storage location is determined by the following priority order:

1. If the `SEMNAV_CACHE_DIR` environment variable is set, use that path
2. If unset, use `<repo_root>/.semnav/` (default)

Directory structure (the contents below are fixed):

* `graph.db` — SQLite (the Graph itself)
* `servers/` — isolated environments for automatically installed LSP servers
* `log/` — debug logs

#### Git Pollution Mitigation

Since it is placed inside the repository, `.semnav/` appears as untracked in `git status`. On first launch, adding it to `.gitignore` is only **recommended via a guidance message**; repository files are not rewritten automatically. For CI, containers, and similar environments, the intended practice is to relocate it outside the repository using `SEMNAV_CACHE_DIR`.
