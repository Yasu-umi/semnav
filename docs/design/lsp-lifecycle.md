# LSP Process & Lifecycle

LSP server process management within the daemon process. Process model, failure detection, restarts, timeouts, shutdown.

> Process & LSP lifecycle (A1 = process management / A2 = didOpen strategy, both below). For Graph behavior under degradation see [resilience.md](./resilience.md); for provisioning see [language-adapters.md](./language-adapters.md).

## Process Model

* **One process per language** (per LanguageAdapter). Spawned as a child process of the **daemon** process, driven via JSON-RPC over stdio
* **Lazy startup**: started on the first query / first index for that language. Servers for unused languages are never started (saves resources)
* The **daemon** is the **sole client of the LSP server**. didChange is also unified through the Graph

> **Revised (2026-07, daemon step):** prior to this, "the MCP process" meant `semnav serve` itself, which owned the LSP servers for the lifetime of one client connection — every new connection meant a fresh, cold LSP process. `serve` is now a stateless stdio↔socket proxy (`docs/design/daemon-lifecycle.md`); a persistent `semnav daemon <root>` process owns the LSP supervisors instead, so they survive across many `serve` connections. Nothing about the health state machine, restart policy, or shutdown escalation below changed — only *which* process hosts them.

## Failure Detection

Three kinds are monitored:

1. **Child process exit monitoring** — unexpected termination (SIGSEGV / OOM / panic, etc.)
2. **JSON-RPC communication errors** — stdio read/write failures (broken pipe, etc.)
3. **Response timeouts** — described below. An `id`-bearing request that fails to respond within the deadline

## Timeouts (Finalized, Implemented)

| Operation | Value | Rationale |
|---|---|---|
| `initialize` (startup + provision) | 60s | Longer to accommodate first-time provisioning (isolated install) |
| `documentSymbol` (1 file) | 30s | Includes initial indexing and dependency resolution |
| Query operations (definition/refs/callHierarchy/hover) | 150s | Prioritizes correctness over raw responsiveness: on large repos, pyright's cross-file requests queue behind a single serialized background-analysis pass and real-world traces have shown ~135s round-trips. A short timeout turned a slow-but-live query into a silent, empty result (see `degrade_reason: "lsp_timeout"` in [resilience.md](./resilience.md)) |

> The initial whole-workspace index is treated as progress (per-file timeouts are as above). `initialize`/`documentSymbol` were finalized based on the observed response distribution of real servers (pyright / tsserver); the query-operation timeout was revised upward after observing pyright's serialized background-analysis pass dominate latency on a ~17k-file repo.

## Health State Machine

```
[not_started] --first query--> [starting] --initialize succeeds--> [healthy]
                                   [starting] --failure (provision, etc.)--> [down]
                 [healthy] --crash/timeout--> [restarting]
                 [restarting] --restart succeeds within backoff--> [healthy]
                 [restarting] --5 consecutive failures--> [down]
                 [down] --background retry every 30s succeeds--> [healthy]
```

`lsp_status` values (recorded per language server in `index_meta`):

* `not_started` / `starting` / `healthy` / `restarting` / `down`
* While `down` or `restarting`, find_* returns degraded responses (the `degraded` flag in [resilience.md](./resilience.md))

## Restart Policy (Backoff → Degradation)

On crash/timeout detection:

1. **Exponential backoff restart**: attempts restart at intervals of `1s → 2s → 4s → 8s → 16s`
2. Increment the consecutive-failure counter (reset on success)
3. On **5 consecutive failures**, transition to `lsp_status=down` (degraded mode, [resilience.md](./resilience.md))
4. While `down`, **background restart attempts continue every 30s**; on success, revert to `healthy` and reset the counter

This tolerates transient crashes while automatically detecting recovery. Under persistent failure, it stabilizes at `down` rather than wasting resources.

## Initialization Failure

If `provision` fails (Node.js / Python not installed, server binary fetch failure), the state transitions from `starting` to `down`. A clear error message guides the user through installation steps (see provisioning in [language-adapters.md](./language-adapters.md)). In this case background retries have little value, so a manual restart (e.g. via the `graph` CLI) is expected once the runtime issue is resolved.

## Shutdown

When the **daemon** process exits (idle timeout, explicit `semnav daemon stop <root>`, or a signal — `docs/design/daemon-lifecycle.md`), all running LSP child processes are terminated in sequence. **`serve` exiting terminates nothing** — that's the entire point of separating the two; a `serve` process (even `kill -9`'d) has no effect on the daemon or its LSP children.

The escalation itself:

1. `shutdown` request → `exit` request (LSP standard)
2. 5s grace period → if no response, **SIGTERM**
3. A further 5s → if no response, **SIGKILL**

> **Implemented (2026-07, Step 3b-iii):** `ServerProcess::shutdown(grace)` runs the
> escalation above (`src/lsp/server.rs`, `SHUTDOWN_GRACE = 5s`). The child's `Child`
> is owned by the exit-watch task, so the pid is retained on the `ServerProcess` and
> SIGTERM/SIGKILL are sent via `libc::kill(pid, sig)` under `#[cfg(unix)]` — only
> while the exit watch is still pending (pid not yet reaped), to avoid pid-reuse
> TOCTOU. The supervisor runs this on both the explicit `shutdown().await` path and
> the detached last-handle-drop path (`src/lsp/supervisor.rs::shutdown_graceful`); the
> `index` CLI calls `shutdown().await` so the child is reaped before runtime teardown.
> **Known 0.0.1 limitation:** the *restart-recycle* path (`drop_current`) still uses a
> fast drop → stdin EOF rather than escalating the *old* server; a server that ignores
> EOF can orphan on restart. Teardown (process exit) is fully escalated.

---

## A2. didOpen & Dependency File Strategy — ✅ Resolved (verified on real servers, U1)

**Adopted: declare `workspaceFolders` in `initialize` and rely on the background scan**. Real-server verification (U1) confirmed that with **zero didOpen calls and only a `workspaceFolders` declaration**, documentSymbol (16 symbols), cross-file definition, and references (5 hits) all resolve correctly. The earlier assumption that "dependency files must be didOpen'd in advance" turned out to be caused by the probe not passing `workspaceFolders` (correction to note 1 in [lsp-integration.md](./lsp-integration.md)).

* **didOpen / didChange**: only for **live-changed files detected by the FS watcher**. Pre-emptive didOpen of typeshed / node_modules / site-packages is entirely unnecessary
* **`rootUri` alone is not sufficient**: deprecated by the LSP spec and does not trigger a scan. `workspaceFolders` must always be passed
* **Do not wait for the scan to complete**: while the background scan is in progress, it's acceptable for references to be filled in progressively (the Graph philosophy: it grows richer the more it's used). `window/logMessage`'s "Found N source files" is shown only as a rough progress indicator; there is no strict completion block

> **Revision (2026-07, provisioning step):** the "didOpen zero" claim above does
> **not** hold when `documentSymbol` is fired *immediately* after `initialized`.
> pyright 1.1.409 returns `[]` for a closed workspace file until its background
> scan reaches it, so an index that does not wait for the scan gets zero symbols.
> The indexer therefore opens each source file with `textDocument/didOpen`
> (reading the text from disk) before requesting `documentSymbol`, forcing
> on-demand analysis and a deterministic result regardless of scan timing
> (`src/indexer/fetch.rs`). This didOpen is for the *initial index* of source
> files only; the A2 point that typeshed / node_modules / site-packages need no
> prior didOpen still stands.
