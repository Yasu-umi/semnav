//! MCP server boundary — rmcp server, the 8 tools
//! (`find_symbol`/`definition`/`references`/`callers`/`callees`/`call_path`/`read_range`/`restart_lsp`),
//! DTO shaping, cursor pagination, and degraded responses.
//!
//! `mcp` calls `query`; it holds no domain logic. See `docs/design/mcp-tools.md`
//! and `docs/design/resilience.md`.

// `pub(crate)` (not private): the daemon module dispatches to `SemnavServer`'s
// inherent tool methods directly, so it needs these same input/output DTOs.
pub(crate) mod dto;
mod proxy;
mod server;

pub use proxy::ProxyServer;
pub use server::SemnavServer;
