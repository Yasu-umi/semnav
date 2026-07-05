# Observability

Debug logging for answering "is this call slow because of the LSP server or because of semnav itself?" and "how often is a given tool/LSP method actually called?" — not a general-purpose logging framework, just enough structure to answer those two questions from `daemon.log`.

## `SEMNAV_LOG`

`tracing` + `tracing-subscriber` (`init_tracing`, `src/main.rs`), filtered via the `SEMNAV_LOG` env var — `RUST_LOG`-style directive syntax (e.g. `SEMNAV_LOG=semnav=debug`, `SEMNAV_LOG=semnav::lsp=trace`). Named `SEMNAV_LOG` rather than the conventional `RUST_LOG` to stay under this binary's existing `SEMNAV_*` env var namespace (`print_help`, `src/main.rs`) rather than reacting to a variable other CLI tools in the user's shell may already set for unrelated purposes.

Default is `warn`-and-above with no directive set, and nothing currently logs at `warn` or above, so **the default behavior is silent** — this only speaks when a caller opts in. Written to stderr only (`.with_writer(std::io::stderr)`), same discipline the rest of the codebase already follows for diagnostics (`eprintln!` throughout, `docs/design/daemon-lifecycle.md` File layout). This matters specifically because `semnav serve`'s MCP protocol owns stdout exclusively (`rmcp::transport::io::stdio()`) — logging that ever touched stdout would corrupt the JSON-RPC stream. Since stdout/stderr are physically separate streams, no extra branching on "am I being invoked as a tool right now" is needed; stderr is safe unconditionally, whether the calling context is `serve`'s stdio proxy, a directly-run `daemon`, or a one-off `index`/`discover`.

## Spans

Two span kinds cover the two places time actually goes:

* **`tool{name="find_symbol"}`** etc. — wraps each of the 8 tool method bodies in `src/mcp/server.rs`. One span per MCP tool call; its close event is both "how long did this whole call take" and (by counting close-log lines for a given `name`) "how many times was this tool called."
* **`lsp_request{method="textDocument/documentSymbol"}`** etc. — wraps `LspClient::request` (`src/lsp/client.rs`), the single choke point every outbound LSP request goes through regardless of which tool triggered it.

A `tool` span's LSP-bound work runs `lsp_request` spans nested inside it (same async call stack), so a `SEMNAV_LOG=semnav=debug` trace naturally answers "LSP or semnav": read off the nested `lsp_request` span(s)' contribution and compare against the enclosing `tool` span's total.

```
DEBUG lsp_request{method="textDocument/references"}: close time.busy=18.2µs time.idle=94ms
INFO  tool{name="find_references"}: close time.busy=1.1ms time.idle=97ms
```

Here the `tool` span's total (`busy+idle` ≈ 98ms) is almost entirely accounted for by the single nested `lsp_request` (`busy+idle` ≈ 94ms) — this call was LSP-bound, not semnav-bound. A call where the `tool` total is much larger than the sum of its nested `lsp_request`s spent that gap in semnav's own code (the db actor, reconcile diffing, resolver/pagination logic) instead.

`time.busy` vs `time.idle` (`tracing-subscriber`'s built-in span-close fields, enabled via `FmtSpan::CLOSE`) is deliberately not collapsed into a single "elapsed" number: `busy` is CPU-active time, `idle` is time the span existed but wasn't being polled (e.g. genuinely blocked awaiting the LSP process's response over stdio). A span dominated by `idle` is waiting on something external (the LSP server, another task); one dominated by `busy` is doing real CPU work on this thread. For an I/O-bound `lsp_request`, expect `idle` to carry almost the entire wall-clock cost — that's the round-trip itself, not a red flag.

## Non-goals (0.0.1)

* No spans inside `DbActor`'s SQLite round-trips or `reconcile_uri`'s diff-and-apply — if a `tool` span's time isn't accounted for by its nested `lsp_request` spans, that's already enough to say "not LSP," without needing a third category to attribute it more finely.
* No metrics export (Prometheus, OpenTelemetry, etc.) — `daemon.log` plus `grep`/`jq` is the whole analysis story for 0.0.1.
