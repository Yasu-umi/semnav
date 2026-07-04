//! MCP server boundary — rmcp server, the 6 tools
//! (`find_symbol`/`definition`/`references`/`callers`/`callees`/`read_range`),
//! DTO shaping, cursor pagination, and degraded responses.
//!
//! `mcp` calls `query`; it holds no domain logic. See `docs/design/mcp-tools.md`
//! and `docs/design/resilience.md`.

// `pub(crate)` (not private): the daemon module dispatches to `SemnavServer`'s
// inherent tool methods directly, so it needs these same input/output DTOs.
pub(crate) mod dto;
mod server;

pub use server::SemnavServer;
