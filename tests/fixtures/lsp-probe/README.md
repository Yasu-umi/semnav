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
* `captures/<language>_<method>_<case>.source.<ext>` — the exact source file
  the request above was made against, kept side-by-side purely as
  human-readable provenance for refreshing the capture later. Not read by any
  test.

## Server versions used to record the current captures

pyright `1.1.409` / typescript-language-server `5.1.3` + typescript `6.0.3`
(same environment as `docs/design/lsp-integration.md`'s original probes).

## Refreshing a capture

1. Drive the server directly over stdio (initialize → didOpen → the request
   in question) against the paired `.source.<ext>` file, e.g. with a
   throwaway script, and dump the raw response.
2. Compare the new raw response against the existing file under `captures/`.
3. If the server's behavior genuinely changed, update the capture and the
   assertions in the consuming test together, and update the "Server versions
   used" section above.
