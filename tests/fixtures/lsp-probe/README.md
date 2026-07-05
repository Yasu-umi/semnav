# LSP conformance fixtures

Captured raw LSP JSON-RPC responses backing the conformance tests that pin
the empirically-observed behaviors documented in
[`docs/design/lsp-integration.md`](../../../docs/design/lsp-integration.md).

These replace the throwaway `/tmp` probe scripts that originally produced
that document: instead of a one-off script whose output is prose and then
discarded, a real server's response is captured once into `captures/*.json`
so the parsing logic can be tested against real data without starting a
language server on every `cargo test` run.

The `#[ignore]`d real-server e2e tests (`src/indexer/runner.rs`,
`src/query/pool.rs`) don't read from this directory — they follow this
codebase's existing convention of writing their tiny source fixtures inline
via `fs::write` into a tempdir at test time. This directory exists only for
data that must be frozen (a real server's actual response), not for input
source that a live test can just as easily write inline.

## Layout

* `captures/<language>_<method>_<case>.json` — a raw JSON-RPC response body
  recorded from a real server, loaded via `include_str!` from unit tests in
  `src/query/lsp_query.rs`, `src/indexer/symbol.rs`, and `src/adapters/kind.rs`.
  Every URI inside a capture is sanitized to a stable placeholder
  (`file:///repo/mod.py`, never a real tempdir path) before it's committed.
  There's no separate source-fixture file per capture — the tiny snippet the
  request was made against is documented in the consuming test's doc comment
  right above the `include_str!` call.

## Server versions used to record the current captures

pyright `1.1.409` / typescript-language-server `5.1.3` + typescript `6.0.3`
(same environment as `docs/design/lsp-integration.md`'s original probes).

## Refreshing a capture

1. Recreate the fixture snippet documented in the consuming test's doc
   comment, and drive a real server against it (e.g. spin up a `QueryRuntime`
   in a throwaway `#[ignore]`d test, acquire a client, and call
   `LspClient::request` directly with `eprintln!`-dumped output — this is how
   every capture in this directory was produced).
2. Sanitize any real filesystem path in the dumped JSON down to the
   `file:///repo/...` placeholder convention.
3. Compare against the existing file under `captures/`. If the server's
   behavior genuinely changed, update the capture and the assertions in the
   consuming test together, and update the "Server versions used" section
   above.
