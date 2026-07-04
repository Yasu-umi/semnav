//! Persistent per-root daemon behind `semnav serve`'s stdio contract
//! (`docs/design/daemon-lifecycle.md`). `serve` proxies all 7 tools to a
//! long-lived `semnav daemon <root>` process so LSP servers' background
//! analysis stays warm across many short-lived client connections — pyright
//! itself has no cross-process cache, so every fresh process re-scans the
//! whole workspace from scratch (confirmed empirically; see the design doc).
//!
//! `serve` holds none of this module's server-side state: it never opens
//! `graph.db` or an LSP supervisor directly once proxying is wired in (Step 3).

pub mod discovery;
pub mod lock;
pub mod protocol;
pub mod server;
