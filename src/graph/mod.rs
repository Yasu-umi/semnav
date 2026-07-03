//! SQLite persistence — `nodes`/`edges`/`events`/`index_meta` CRUD,
//! `valid`/`orphan`/`generation` cache flags, FQN construction, `is_external`
//! detection, refinery migration, and the **db actor** (single `Connection`
//! owner driven by `tokio::sync::mpsc` + `oneshot`).
//!
//! See `docs/design/graph-model.md`.

mod db;
mod model;
mod schema;

pub use db::{DbActor, DbCommand, Direction, Neighbor};
pub use model::{Edge, Node, Range, ReconcileOutcome, ReconcileSymbol, Site};
