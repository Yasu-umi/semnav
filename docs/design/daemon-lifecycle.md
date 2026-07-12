# Daemon Lifecycle (2026-07)

`serve`'s stdio↔daemon split: why it exists, the file layout, discovery/locking, detachment, the wire protocol, and idle shutdown.

> For LSP process management *within* the daemon (health state machine, restart policy, timeouts), see [lsp-lifecycle.md](./lsp-lifecycle.md) — unchanged by this doc, just relocated to a longer-lived host process. For degradation semantics, see [resilience.md](./resilience.md).

## Motivation

Pyright has **no persistent cross-process cache**. Confirmed empirically against a real ~17,419-file Python monorepo: every fresh `pyright-langserver` process re-scans the entire workspace from scratch, and `find_references`/`find_callers` return fast-but-silently-incomplete snapshots for roughly the first minute (no error, no degrade signal — legitimate "normal-case null," not a bug) before settling on complete results.

Before this change, `semnav serve <root>` was a single process spawned by the MCP client (Claude Code) over stdio, owning the LSP supervisors, `DbActor`, and `QueryRuntime` for exactly the lifetime of that one connection. Since the MCP client tears down and respawns `serve` on every new session/reconnect, users repeatedly paid this cold-start tax.

## Process split

* **`semnav daemon <root>`**: owns the real state — `DbActor`, `QueryEngine`, `QueryRuntime` (LSP supervisors), `FsWatcher`, `SemnavServer`. Lives until idle-timeout, an explicit `daemon stop`, or a signal. Exactly one daemon runs per `<root>` at a time.
* **`semnav serve <root>`**: the process the MCP client actually spawns over stdio — unchanged from the client's point of view. Holds **no domain state** — no `DbActor`, no `QueryRuntime`, no LSP supervisor. On startup it ensures a daemon is running (auto-spawning one if needed), then proxies all 8 tools to it through a [`ReconnectingDaemonClient`](#reconnect) that re-attaches if that daemon later disappears. `serve` exiting (gracefully or via `kill -9`) has **zero effect** on the daemon or its LSP children — that inversion of the old "MCP process owns everything" model is the entire point.

## File layout

Under `<root>/.semnav/` (sibling to the existing `graph.db`, `servers/`):

| File | Purpose |
|---|---|
| `daemon.sock` | Unix domain socket — the live protocol endpoint |
| `daemon.lock` | `flock` guard file, used only to prevent two `serve` processes from racing to spawn a daemon |
| `daemon.pid` | Informational only (pid + write time) — for `daemon stop` diagnostics, never used to determine liveness (pids get reused) |
| `daemon.log` | The detached daemon's redirected stdout/stderr (append mode) |

## Discovery & liveness

A daemon is "live" if a `UnixStream::connect` to `daemon.sock` succeeds within a short bound (2s). Three outcomes:

* **No socket file** → not running.
* **Connect fails** (`ECONNREFUSED`/`ENOENT`) → a stale socket left behind by a daemon that died without cleaning up; removed immediately so a later spawn doesn't collide on `bind()`.
* **Connect succeeds** → live.

This is deliberately **not** based on `daemon.pid` or `flock` state — a crashed daemon (SIGKILL, OOM-kill) leaves its socket *file* on disk but nothing listening, which the connect-failure branch above already handles correctly with no separate pid-liveness check needed.

## Preventing a double spawn

Two `serve` processes starting concurrently against the same root both race on `DaemonLock::try_acquire(daemon.lock)` (`libc::flock(fd, LOCK_EX | LOCK_NB)` — `libc` is already a dependency, used for `kill` elsewhere in `src/lsp/server.rs`, so this added no new crate). Exactly one wins:

1. **Winner**: releases the lock *immediately* (before spawning!), then spawns the daemon. The daemon acquires the same lock itself as the first thing it does in `run_daemon` — if `serve` were still holding it, the daemon would see `EWOULDBLOCK` and refuse to start, mistaking its own parent's coordination lock for a real competing daemon.
2. **Loser(s)**: skip spawning and just poll the liveness probe until *some* daemon answers.

A narrow window exists between the winner releasing the lock and the spawned daemon acquiring its own copy, where a second `serve` could slip in and also decide to spawn. This is accepted and self-healing: at most one daemon actually binds `daemon.sock` (a second one's own `run_daemon` self-check — `probe_liveness` at startup, then its own `try_acquire` — fails cleanly and it exits), and every `serve` that spawned a "loser" just keeps polling until the real daemon's socket appears.

## Detachment

`serve` spawns `semnav daemon <root>` via `tokio::process::Command`:

* `.process_group(0)` (Unix only) — puts the daemon in its own process group so it isn't killed by a signal sent to `serve`'s group.
* `.kill_on_drop(false)` — the daemon must outlive `serve`; dropping the `Child` handle must not touch the running process.
* stdio redirected to `daemon.log` (not inherited, not `/dev/null`) — the daemon must not hold `serve`'s stdio pipes open (which would interfere with the MCP client's own EOF-based disconnect detection), but its own diagnostics still need to land somewhere.
* Not `.wait()`ed. `serve` polls the liveness probe afterward instead of tracking its specific spawned child's outcome — see the self-healing note above for why that's the right thing to watch.

## Wire protocol (`serve` ↔ `daemon`)

Newline-delimited JSON over a raw `UnixStream` — deliberately **not** MCP/rmcp on this link. Evaluated and rejected reusing rmcp's `transport-streamable-http-server` bound to a `UnixListener`: architecturally possible, but rmcp ships no server-side Unix-socket precedent (only a client exists), and standing up the HTTP/1.1 framing by hand pulls in `hyper`/`tower` just to shuttle 8 fixed operations between two processes of the same binary. See `docs/design/crate-structure.md` Decision Point 6 for the full comparison.

```
{"id": 1, "request": {"op": "FindSymbol", "params": {...}}}          // serve -> daemon
{"id": 1, "result": {"Ok": {...}}}                                    // daemon -> serve
```

* One JSON object per line (`\n`-terminated); `id` multiplexes concurrent in-flight calls over a single physical connection (rmcp can dispatch several tool calls concurrently, and `serve` funnels all of them through one `DaemonClient`).
* Request payloads reuse the existing tool DTOs (`mcp::dto`, `query::dto`) verbatim — no parallel schema. This required adding `Deserialize` to types that previously only needed `Serialize` (they were tool-call *outputs*, now also round-tripped as protocol payloads), and changing `DegradeInfo`'s two fields from `&'static str` to `String` (`&'static str` can't implement `Deserialize`).
* `DaemonRequest::Shutdown` is the one non-tool control message — triggers the daemon's graceful-shutdown path immediately regardless of connection count (`daemon stop`).
* On the daemon side, requests dispatch straight to `SemnavServer`'s inherent tool methods (`Parameters(input)` → the method → `Json(output)`), bypassing rmcp's own dispatcher entirely — this link was never MCP to begin with.

## Idle shutdown & explicit stop

The daemon tracks active connection count and, whenever it drops to zero, the time it did so. A periodic check (every 5s) shuts the daemon down once it's been at zero connections for the idle timeout — default 30 minutes, overridable via `SEMNAV_DAEMON_IDLE_TIMEOUT_SECS`.

`semnav daemon stop <root>` connects, sends `Shutdown`, and then **blocks until the liveness probe confirms the daemon actually exited** (bounded, ~5s) — not just until the request is sent — so `semnav daemon stop <root> && semnav index <root>` gets a real exclusivity guarantee on `graph.db`.

Either shutdown path (idle, explicit stop, or SIGTERM) runs the same teardown sequence `run_serve` used to run itself: `watcher.shutdown().await` then `query_runtime.shutdown_all().await`, then remove `daemon.sock`/`daemon.pid` and release the lock.

## Startup drift reconciliation

`FsWatcher` only reacts to fs events it's actually subscribed for, so any change made while *no* daemon was running for a root — explicit stop, crash, or simply editing files between `semnav index` and the first `semnav daemon` start — is invisible to it: the graph stays frozen at whatever it looked like the last time a daemon was watching, with no error and no `Degradation` signal (github.com/Yasu-umi/semnav/issues/4).

`run_daemon` closes this gap on every startup, right after `FsWatcher::spawn`: it kicks off `reconcile_startup_drift` (`src/indexer/reconcile.rs`) as a detached background task, *before* the daemon starts accepting connections. It:

1. Walks `root` with the same `discover_files` the indexer uses.
2. Reads every uri the graph already has non-orphan nodes for (`DbActor::known_uris`).
3. Unions the two sets — the walk alone would miss a file deleted during the gap, since it no longer exists to be discovered; only a uri the graph still remembers can drive that file's nodes through the orphan path.
4. Runs each uri through `reconcile_uri_for_startup_drift`, yielding to foreground queries via `wait_until_query_idle` between files, matching the watcher's own live-query-priority gate.

This is deliberately **not** git-based — semnav has no requirement that `root` be a git repo, and uncommitted/untracked edits (exactly what a live coding session produces) wouldn't show up in a commit-level diff anyway. Drift is a per-file, git-agnostic concept here, same as the rest of the invalidation flow (`indexing-and-cache.md` "Cache Invalidation").

**Suspicious-empty guard** (github.com/Yasu-umi/semnav#6): a `textDocument/documentSymbol` request fired immediately after `initialize` can race a language server's own project/crate-graph discovery (observed with rust-analyzer), coming back empty not because the file genuinely has no symbols but because the server hasn't finished its warm-up scan yet. Unlike the live watcher's `reconcile_uri` — where an empty result always corresponds to a real edit that just happened, so it's trusted outright — `reconcile_uri_for_startup_drift` treats "file exists, fetch returned zero real symbols, but `DbActor::count_real_nodes` shows the uri previously had some" as suspicious rather than committing it as ground truth. `count_real_nodes` counts a uri's real (non-module-root) node rows whether or not they're currently orphaned, not just `orphan = 0` ones — a uri that's already taken one orphan strike still has to answer "did this ever have real symbols", and a strike doesn't erase the row, only marks it (github.com/Yasu-umi/semnav#7; without this the guard only ever protected a file the first time it was affected).

The same retry queue also catches a still-unavailable LSP server (github.com/Yasu-umi/semnav#7): `fetch_uri_symbols`'s inability to acquire a client is a distinct `FetchOutcome::LspUnavailable`, not folded into a successful zero-symbols fetch, so `reconcile_uri_for_startup_drift` reports `DriftAttempt::LspUnavailable` for it instead of silently treating the uri as committed with no future FS event guaranteed to catch it up.

Suspicious-empty and LSP-unavailable uris are both held back from the first pass and retried up to `SUSPICIOUS_RETRY_ATTEMPTS` (5) times, `SUSPICIOUS_RETRY_DELAY` (2s) apart; a uri still unresolved after every retry is left with its existing index entries untouched and logged explicitly (`... skipped as unresolved (suspiciously empty or LSP unavailable)`) so the drop is visible instead of silently folding into the `0 failure(s)` count.

It runs in the background rather than blocking `daemon::server::run`, so a large repo doesn't delay this daemon accepting connections — a query landing mid-reconcile just sees the same pre-existing snapshot it would have seen anyway, not a new failure mode.

**Content-hash skip**: before paying for a uri's LSP round-trip (`ensure_document` + `documentSymbol`) at all, `reconcile_uri_for_startup_drift` reads its current on-disk bytes and compares a non-cryptographic fingerprint (`src/indexer/reconcile.rs::current_content_hash`, the same `DefaultHasher` pattern as `signature_fingerprint`) against the one recorded under `startup_drift_content_hash::<uri>` in `index_meta` the last time this uri was committed here. A match means the file provably hasn't changed since then, so it's reported as a no-op `Committed` without ever calling the LSP — the common case when the daemon was down only briefly. A missing file (deleted while unwatched) never matches — its stored fingerprint is cleared to an empty string, which a real fingerprint (always 16 hex digits) can never equal — so deletion, and a later file recreated with byte-for-byte identical content, both still drive the real fetch and the orphan path it feeds.

## Reconnect

`serve` is typically far longer-lived than a daemon: an MCP client session can run for days, while a daemon self-terminates after 30 minutes idle by default and can also disappear via an explicit `daemon stop` (e.g. after rebuilding the binary and restarting the daemon to pick it up) or a crash. `ProxyServer` (`src/mcp/proxy.rs`) doesn't hold a raw `DaemonClient` for this reason; it holds a `ReconnectingDaemonClient` (`src/daemon/reconnect.rs`), which wraps one behind an `Arc<RwLock<..>>` and re-runs `ensure_and_connect` (`src/daemon/connect.rs` — the same auto-spawn-or-attach logic `run_serve` uses for its first connection) whenever a call finds the current connection dead, retrying that one call once against the fresh connection before giving up.

"Dead" is `DaemonClient::is_closed`, not a string match on the error: the client actor's `mpsc::Receiver` drops the instant the actor loop exits — on a read EOF/protocol error *or* a write failure (e.g. `EPIPE` from writing into an already-closed socket, which can be observed before the reader side notices the same close) — so `is_closed()` reflects connection death immediately and unambiguously, without confusing it for a genuine tool/protocol-level error a live daemon returned (retrying those against a fresh connection would just reproduce the same failure).

## Known risks (accepted, not solved here)

* **A daemon panic before its teardown sequence runs can still orphan LSP children.** `kill_on_drop(true)` on the LSP child `Command` (`src/lsp/server.rs`) is a backstop for the case where the owning `Child` handle is dropped during an unwind, but a `kill -9` on the daemon itself can't run any cleanup code, Rust or otherwise — same residual risk as any long-lived supervisor process in any language. Mitigation is operational: prefer `daemon stop` over `kill -9`, and if a daemon is ever killed that way, `pkill -f pyright-langserver`/`pkill -f typescript-language-server` cleans up stragglers manually.
* **Idle-timeout (30 min) and startup-timeout (~60s) defaults are reasonable guesses, not measurements.** Both are environment-variable-tunable (`SEMNAV_DAEMON_IDLE_TIMEOUT_SECS`; the startup timeout is not yet exposed as an env var, since 60s has been sufficient in practice — revisit if that changes) rather than hardcoded assumptions baked into the design.
* **Two daemons for the same root can theoretically both attempt to bind `daemon.sock` in the narrow spawn-race window** described above. Accepted as self-healing (one wins the `bind()`, the other's own startup self-check fails it cleanly) rather than closed with a more complex fd-handoff between `serve` and the child it spawns.
