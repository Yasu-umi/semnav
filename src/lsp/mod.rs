//! LSP client — child-process management (spawn/exit watch), health state
//! machine, backoff restart, timeouts, a thin hand-rolled JSON-RPC layer
//! (Content-Length framing + id pairing), `workspaceFolders`/`didOpen`/`didChange`,
//! and health persisted to the `index_meta` KV.
//!
//! See `docs/design/lsp-lifecycle.md`.

mod client;
mod handshake;
mod proto;
mod server;
mod supervisor;
mod transport;

pub use client::LspClient;
pub use handshake::{
    build_initialize_params, document_symbol_timeout_from_env, initialize,
    initialize_timeout_from_env, query_timeout_from_env,
};
pub use server::{SHUTDOWN_GRACE, ServerExit, ServerProcess};
pub use supervisor::{
    AcquireError, FailureKind, MetaStore, RealServerFactory, RestartPolicy, ServerFactory,
    ServerState, ServerSupervisor, StartError, StartedServer, SupervisorHandle,
};
