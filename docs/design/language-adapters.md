# Language Adapters & Provisioning

## Language Adapter

0.0.1 targets **Python**, **TypeScript (tsserver)**, and **Rust (rust-analyzer)**.
**Go (gopls)** was added afterward.

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

The LSP-standard `SymbolKind` is a protocol-common enum, and tsserver, pyright, and gopls all return the same values for it, so **no normalization is needed**. `NodeKind` holds the standard values as an enum, and falls back to a string for values outside the LSP spec so no information is lost.

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

Go needs no such refinement: gopls's `documentSymbol` reports struct/interface/func/method/field with unambiguous standard `SymbolKind` values, with no TS-`type`-alias-style collision to disambiguate via hover.

## LSP Server Provisioning

LSP server binaries are not bundled with the Rust binary; they are procured via **automatic detection and automatic installation**.

1. Detect the target language's LSP server from `PATH`
2. If not found, install it into an isolated directory
   * TypeScript: `typescript-language-server` (via npm)
   * Python: `pyright` (via npm) or basedpyright
   * Rust: not auto-installed — `rust-analyzer` isn't npm-distributed. `LanguageAdapter::server_package` returns `None` for it, which short-circuits step 2 into a clear error pointing at `rustup component add rust-analyzer` rather than attempting an npm install.
   * Go: not auto-installed either — `gopls` isn't npm-distributed. `server_package` also returns `None`, pointing users at `go install golang.org/x/tools/gopls@latest`.
3. If the runtime requirements (Node.js / Python) are not met, guide the user through the installation steps with a clear error message

The binary is kept lightweight, and language runtimes ride on whatever is present in the environment.

### Environment Overrides

Two environment variables, keyed by `language_name()` upper-cased (`PYTHON`/`TYPESCRIPT`/`RUST`/`GO`), let the process launching `semnav serve`/`index` (an MCP client's `.mcp.json` `env` block, typically) override provisioning without any semnav-specific configuration protocol — plain launch-time environment variables are the entire "config surface" a stdio MCP server has:

* `SEMNAV_LSP_<LANG>_COMMAND` — replaces the resolved program outright (skips `PATH`/isolated-install resolution). Points at a custom build, a wrapper script, or a server binary at a nonstandard location.
* `SEMNAV_LSP_<LANG>_ARGS` — extra args appended after the adapter's built-in `CommandSpec::args` (space-separated; no shell-quoting support). Useful for server-specific startup flags semnav doesn't hardcode (e.g. `SEMNAV_LSP_RUST_ARGS="--log-file /tmp/ra.log"`).

The three LSP timeouts (`docs/design/lsp-lifecycle.md`) are similarly overridable process-wide (not per-language): `SEMNAV_INITIALIZE_TIMEOUT_SECS`, `SEMNAV_DOCUMENT_SYMBOL_TIMEOUT_SECS`, `SEMNAV_QUERY_TIMEOUT_SECS`.

## Custom/Generic Adapter

`SEMNAV_CUSTOM_LANGUAGES` lets a user point semnav at an LSP server for a
language with no built-in adapter (Java, C/C++, Ruby, PHP, ...) — a "just
make it run" escape hatch, not a plugin system. No new Rust code, no release
needed; parsed once per process by `src/adapters/custom.rs::custom_adapters`
and appended to `builtin_adapters()`.

* `SEMNAV_CUSTOM_LANGUAGES` — comma-separated tags to register, e.g.
  `SEMNAV_CUSTOM_LANGUAGES=java,cpp`.
* Per tag `T` (upper-cased, e.g. `JAVA`):
  * `SEMNAV_LSP_T_EXTENSIONS` — comma-separated file extensions with the
    leading dot (e.g. `.java`). **Required**: a tag with no (or empty)
    extensions is dropped during parsing (logged to stderr, not a hard
    error) — an adapter matching zero files would only produce a spurious
    failed `index_language` attempt, since every language in
    `builtin_adapters()` is attempted unconditionally regardless of match
    count.
  * `SEMNAV_LSP_T_COMMAND` — **required** in practice: a custom language has
    no built-in default program, so this is the same
    `command_override_from_env` var described above, and an unset one falls
    into the `server_package() == None` bail path (below) naming this exact
    var.
  * `SEMNAV_LSP_T_ARGS` — same generic override as above (extra args
    appended after the adapter's, empty for custom).
  * `SEMNAV_LSP_T_EXTERNAL_MARKERS` — optional comma-separated dependency-path
    fragments (default empty — only the trait's default "not under
    `root_uri`" `is_external` check applies).

A custom adapter never auto-installs (`server_package()` is always `None`,
same as Rust/Go) and gets the generic hover-based `construct` refinement for
free (that logic is value-driven, not language-gated — see
[lsp-integration.md](./lsp-integration.md)), but has no per-language
special-casing like TypeScript/Go's interface-to-implementation dispatch
correction.

## Distribution

Distributed as a single Rust binary. LSP servers and language runtimes are not bundled; they are procured automatically.

### Graph Storage

The storage location is determined by the following priority order:

1. If the `SEMNAV_CACHE_DIR` environment variable is set, use that path
2. If unset, use `<repo_root>/.semnav/` (default)

Directory structure (the contents below are fixed):

* `graph.db` — SQLite (the Graph itself)
* `servers/` — isolated environments for automatically installed LSP servers
* `daemon.sock` / `daemon.lock` / `daemon.pid` / `daemon.log` — daemon runtime files (see [daemon-lifecycle.md](./daemon-lifecycle.md) "File layout")

#### Git Pollution Mitigation

Since it is placed inside the repository, `.semnav/` appears as untracked in `git status`. On first launch, adding it to `.gitignore` is only **recommended via a guidance message**; repository files are not rewritten automatically. For CI, containers, and similar environments, the intended practice is to relocate it outside the repository using `SEMNAV_CACHE_DIR`.
