//! Indexer — file discovery (the `ignore` crate), documentSymbol collection,
//! FS watcher (`notify`), the invalidation flow, and orphan reclamation.
//!
//! See `docs/design/indexing-and-cache.md`.

mod discovery;
mod fetch;
mod pipeline;
mod reconcile;
mod runner;
mod symbol;
mod watcher;

pub use discovery::{discover_files, path_to_uri, uri_to_path};
pub(crate) use fetch::request_document_symbols;
pub use fetch::{LspSymbolFetcher, SymbolFetcher};
pub use pipeline::{IndexStats, index_repository};
pub use reconcile::reconcile_startup_drift;
pub use runner::index_language;
pub use symbol::{
    DocumentSymbol, FlatSymbol, LspPosition, LspRange, flatten_document_symbols,
    module_path_from_uri, signature_fingerprint,
};
pub use watcher::{FsWatcher, FsWatcherHandle};
