//! semnav — LSP-backed Semantic Graph library.
//!
//! The library surface: language adapters, the SQLite graph store + db actor,
//! the LSP client, and the indexer pipeline. The binary (`src/main.rs`) is a
//! thin CLI over this library. See `docs/design/crate-structure.md`.

pub mod adapters;
pub mod graph;
pub mod indexer;
pub mod lsp;
pub mod mcp;
pub mod query;
