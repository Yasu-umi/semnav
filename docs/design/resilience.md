# Resilience & Degradation

**Normal-case null** from LSP servers vs. **degraded operation on failure** (crash, unresponsive, timeout).

> Covers handling of null/crash/partial failure (E). For the normal-case tool interface, see [mcp-tools.md](./mcp-tools.md); for per-language differences in LSP return values, see [lsp-integration.md](./lsp-integration.md).

## Normal-case null (decided)

It is **normal** for definition / references / hover to return null (type resolution failure, unresolved import, single-file state). The Graph can simply have **no edge** (see [lsp-integration.md](./lsp-integration.md), caveat 3). Index completion rate is measured via `index_meta`, but null is not treated as an error.

This is not degradation, just "no information." No `degraded` flag is attached to the response.

## Degraded operation (on failure)

### Basic policy: cache-only degradation

The Graph is a **persistent cache of LSP query results**. Even when the LSP server is failing, it **keeps responding using only cached information**. It never fails outright — that is the whole point of the cache.

### Boundary with A

* **E (this doc)**: **what the Graph returns to the agent** (the degraded response schema, per-tool behavior)
* **A ([lsp-lifecycle.md](./lsp-lifecycle.md))**: **how the LSP process is recovered** (timeout values, restart count, interval, provisioning coordination)

The **transition mechanism and thresholds** for "N consecutive failures → enter degraded mode → background restart attempts → revival returns to normal mode" are **defined in A**. E covers only the Graph's behavior while in degraded mode.

### Per-tool behavior when degraded

Per the decisions in [mcp-tools.md](./mcp-tools.md) (read_range reads the FS directly, find_* build edges on demand), LSP dependence varies by tool:

| Tool | LSP dependence | Behavior when degraded | `degraded` attached |
|---|---|---|---|
| `read_range` | **None** (direct FS read) | **Always works**. Functions even with LSP fully down — the lifeline during degradation | No |
| `find_symbol` | Low (Nodes from documentSymbol are indexed on first pass) | **Works from cache**. Only re-validation of `valid=false` (dirty) Nodes is unavailable | No\* |
| `find_definition` | Medium (`defines` edge) | Returns cached edges; unbuilt ones come back empty | Yes |
| `find_references` | High (on-demand) | Cache only; unbuilt ones cannot be returned | Yes |
| `find_callers` | High (on-demand callHierarchy) | Same as above | Yes |
| `find_callees` | High (on-demand callHierarchy) | Same as above | Yes |
| `find_call_path` | High (on-demand callHierarchy, budgeted BFS) | Search stops early on `max_depth`/`max_lsp_calls`/no client; `limit_reached: true` marks the answer as inconclusive rather than proven | No\*\* |

\* `find_symbol` needs LSP to re-validate dirty Nodes (see the dirty lifecycle in [graph-model.md](./graph-model.md)), but while degraded it returns the stale cache as-is and does not attach `degraded` (a stale FQN is still practically useful for search purposes). The `valid` field of any individual Node awaiting dirty re-validation stays `false`.

\*\* `find_call_path` doesn't use the `degraded`/`degrade_reason`/`lsp_status` schema at all — an unavailable LSP client is just one more reason the BFS budget runs out, folded into the same `limit_reached` honesty signal it already needs for `max_depth`/`max_lsp_calls` ([mcp-tools.md](./mcp-tools.md) "find_call_path").

### degraded response schema

LSP-dependent, on-demand edge tools (find_definition / find_references / find_callers / find_callees) attach the following to cached results when degraded:

```
{
  ...(normal fields: nodes / references / callers / callees / next_cursor),
  degraded:       true,
  degrade_reason: "lsp_unavailable" | "lsp_timeout" | "lsp_partial",
  lsp_status:     "down" | "degraded",
}
```

* `degrade_reason` / `lsp_status` are attached only when `degraded: true` (omitted when false/normal)
* **Cached edges are still returned** — an empty result can mean either "genuinely none exist" or "could not be built due to degradation"; `degraded` distinguishes the two
* `degrade_reason`:
  * `lsp_unavailable` — process down / crashed
  * `lsp_timeout` — response timed out
  * `lsp_partial` — only some methods failed (e.g. pyright's `implementation` is unsupported, but that's a spec limitation, not a failure, so it's a normal response)

## `refreshing` — distinct from `degraded`

`find_callers`/`find_references` additionally carry an optional `refreshing: true` field, folded in only when a background refresh was actually kicked off (never serialized as `refreshing: false`):

```
{
  ...(normal fields: callers / references / next_cursor),
  refreshing: true,
}
```

This is **not** a degradation signal in the sense of "the server is unhealthy" — a warm anchor's server is up and being used right now. It means the anchor was *warm* (materialized before): the result was served immediately from the cache while a fresh materialization runs in the background ([lsp-integration.md](./lsp-integration.md) "cache-first + background refresh"), so it may be missing a caller/reference added since the last materialization. Re-querying picks up the converged answer once the background refresh completes. `find_definition`/`find_symbol`/`find_callees` never set this field — `find_callees` uses a precise content-hash cache instead ([lsp-integration.md](./lsp-integration.md)), so its cached answer needs no freshness signal at all.

`degraded` and `refreshing` are independent signals and can, in a narrow edge case, both be set: for a `{at: ...}` symbol ref, anchor resolution itself is a `textDocument/definition` call that can time out (`degrade_reason: "lsp_timeout"`) while still falling back to the indexed node at that position; if *that* anchor happens to be warm, a background refresh is still spawned. `degraded` here describes the anchor-resolution step, not the cached answer's freshness.

### Agent experience in degraded mode

By seeing `degraded: true`, the agent knows the result is limited by the cache. It can distinguish "callers/callees are few because of degradation" from "genuinely few," and, when needed, has the fallback of reading code directly via `read_range` (which is LSP-independent and always works).

If a language's server stays wedged (alive but not recovering) despite the automatic restart policy, `restart_lsp` (`docs/design/mcp-tools.md`) lets the agent force a reset without restarting the whole process.

> **Note (2026-07, daemon step):** LSP health state (and therefore degradation) is now tracked by the persistent `daemon` process (`docs/design/daemon-lifecycle.md`), not by `serve`. A `serve` session that connects to an already-warm daemon inherits whatever health state it already has — a second or third session typically sees *fewer* degradations than the first, since the first session's connection (or an earlier one) already paid any acquire-time cost.

### Boundary with the daemon

`serve` no longer acquires LSP clients or observes `AcquireError` itself — that only happens inside the `daemon` process, behind `DaemonClient`. If `serve` can't reach the daemon at all (it failed to start, or the socket is unreachable), that's a **startup failure** of `serve` itself (a hard error, matching the existing "`graph.db` not found" pattern), not a `degraded: true` response — there is no partial/degraded mode for "no daemon," since without it there's no tool to even attempt a cache-only read.

## Health recording in index_meta

The following is recorded in the KV (`index_meta`) described in [graph-model.md](./graph-model.md):

* `lsp_status`: `healthy` / `degraded` / `down` (per language server)
* `lsp_last_success_at`: timestamp of last success
* `lsp_consecutive_failures`: consecutive failure count

In 0.0.1, a tool like `graph status` is out of scope (only the 7 Query API tools plus `restart_lsp` exist), so this is **for logging/debugging only**. It reaches the agent via the `degraded` flag.

## Timeouts (finalized, implemented)

Timeouts that trigger degradation. The finalized values, based on observed response distributions from real servers (pyright / tsserver), are in the table in [lsp-lifecycle.md](./lsp-lifecycle.md) (`initialize`=60s / `documentSymbol`=30s / query-type=150s). A query-type timeout now surfaces as `degrade_reason: "lsp_timeout"` (`lsp_status: "degraded"`) rather than being silently swallowed.
