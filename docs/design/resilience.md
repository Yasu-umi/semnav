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

\* `find_symbol` needs LSP to re-validate dirty Nodes (see the dirty lifecycle in [graph-model.md](./graph-model.md)), but while degraded it returns the stale cache as-is and does not attach `degraded` (a stale FQN is still practically useful for search purposes). The `valid` field of any individual Node awaiting dirty re-validation stays `false`.

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

### Agent experience in degraded mode

By seeing `degraded: true`, the agent knows the result is limited by the cache. It can distinguish "callers/callees are few because of degradation" from "genuinely few," and, when needed, has the fallback of reading code directly via `read_range` (which is LSP-independent and always works).

## Health recording in index_meta

The following is recorded in the KV (`index_meta`) described in [graph-model.md](./graph-model.md):

* `lsp_status`: `healthy` / `degraded` / `down` (per language server)
* `lsp_last_success_at`: timestamp of last success
* `lsp_consecutive_failures`: consecutive failure count

In 0.0.1, a tool like `graph status` is out of scope (only the 6 Query API tools exist), so this is **for logging/debugging only**. It reaches the agent via the `degraded` flag.

## Timeouts (finalized, implemented)

Timeouts that trigger degradation. The finalized values, based on observed response distributions from real servers (pyright / tsserver), are in the table in [lsp-lifecycle.md](./lsp-lifecycle.md) (`initialize`=60s / `documentSymbol`=30s / query-type=10s).
