# Dynamic Graph (Future)

To complement what static analysis cannot capture — `getattr()`, reflection, dynamic dispatch, DI containers, etc. — runtime information is added to the Graph.

> Not implemented in 0.0.1. However, the runtime event schema and a slot for the `events` table are already reserved ([graph-model.md](./graph-model.md#events)).

## Generic Tracer Plugin Architecture

Dynamic analysis is realized with a **language- and runner-agnostic** plugin architecture. Specializing for a particular runner (pytest, etc.) would drag the protocol along when adding another language later, and break down.

It is split into 3 layers:

```
┌─────────────────────────────────────────────┐
│  semnav Graph (Rust, language-agnostic)      │
│  Ingested via a common event schema          │
└──────────────────┬──────────────────────────┘
                   │ Common transport protocol (JSONL)
┌──────────────────┴──────────────────────────┐
│  Tracer SDK (per-language, thin)             │
│  Schema serialization + cache location resolution │
├──────────┬──────────┬───────────────────────┤
│ Python   │ JS/TS    │ Go  ...               │
│ tracer   │ tracer   │ tracer                │
│(pytest11)│(vitest)  │(go test)              │
└──────────┴──────────┴───────────────────────┘
```

| Layer            | Shared                                                       | Language-specific                                                             |
| ---------------- | ----------------------------------------------------------- | ----------------------------------------------------------------------------- |
| Event schema     | caller/callee are expressed as a **symbol reference (`uri`+`range`+`fqn`)** | —                                                                             |
| Transport protocol | Appended as JSONL to `${SEMNAV_CACHE_DIR}/events-<run_id>.jsonl` | —                                                                             |
| Tracing method    | —                                                             | Python: `sys.monitoring` / Node: AST instrumentation or V8 / Go: `runtime/trace` |
| Runner distribution | —                                                           | pip+pytest11 / npm+vitest plugin / go build tag                               |

The core idea is representing caller/callee as a "symbol reference." As long as tracers honor the contract of normalizing runtime events into `(uri, range, fqn)`, any tracer can feed the same Graph.

## Tracing Methods and Confidence by Language

| Language | Method                                                                       | Confidence | Notes                                                        |
| ------ | -------------------------------------------------------------------------- | ---- | ----------------------------------------------------------- |
| Python | `sys.monitoring` (3.12+)                                                    | High | Low overhead, no bytecode rewriting needed                                        |
| JS/TS  | esbuild/swc AST instrumentation (vitest transform hook), or V8 profiler | Medium   | Capture rate for dynamic dispatch is lower than Python's. Transform injection is the practical approach |
| Go     | `runtime/trace` + build tag instrumentation                                | Medium   | Test-function-level tracing is easy; call granularity is limited by trace           |
| Rust   | Assumes eBPF / `tracing` crate                                             | Low   | Native, so handled on a case-by-case basis                                   |

Differences in coverage between languages are accepted as-is.

## Tracer Plugin Contract

Minimum requirements each language's tracer must satisfy:

1. Receive `run_id` from the `SEMNAV_RUN_ID` environment variable (shared)
2. Append JSONL in the common schema to `${SEMNAV_CACHE_DIR}/events-<run_id>.jsonl`
3. Normalize caller/callee to `(uri, range, fqn)` (sharing the uri convention with semnav)
4. Do not record C extension / Native internals (Non Goal, shared)

## Integration Levels

| Lv       | Method                                                                                         | Runner modification |
| -------- | -------------------------------------------------------------------------------------------- | ------------ |
| L1 (recommended) | Load the tracer via each ecosystem's standard plugin path (pytest11 / vitest plugin / go build tag) | None         |
| L2       | A `semnav trace -- <runner>` wrapper enables the tracer via environment variables                | None         |
| L3       | The runner links the semnav library and writes to the Graph directly, in-process                       | Required     |

For now, L1: a native tracing shim built only on each ecosystem's stdlib/standard plugin path, rather than reimplementing the runner's own plugin machinery.

## Limitations

- **Calls into C extension / Native internals** — Cannot be recorded because tracing stops at the Python/JS level. Non Goal
- **Exact async execution paths** — Stack semantics break down across the event loop. CALLS edges (who calls whom) can be captured, but "exact reconstruction of the path" is only approximate
- **Dynamically generated functions** — For those with no Node corresponding to source, such as via `exec`/`eval`/decorators, events are either discarded or treated as an Anonymous Node
- **Parallel workers** — Since they are forked/distributed, they are merged by `run_id` + `worker_id`. Monitoring is re-registered when a worker starts

The Graph stores `CALLS` (static) and `CALLS_DYNAMIC` (runtime-observed) as distinct edge types.

## Runtime Events

Not just edges, but the call events themselves are also stored:

```json
{
  "run_id":    "...",
  "worker_id": "...",
  "test_name": "...",
  "caller":    {"uri":"file://...", "range":[L,C,L,C], "fqn":"..."},
  "callee":    {"uri":"file://...", "range":[L,C,L,C], "fqn":"..."},
  "ts":        1234567890,
  "thread":    "...",
  "kind":      "call"
}
```

This enables queries such as "the path taken by this test," "only actually observed calls," and "call frequency."
