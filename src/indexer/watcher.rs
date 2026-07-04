//! Filesystem watcher: keeps the `nodes` table in sync with on-disk edits
//! while `semnav serve` is running (`docs/design/indexing-and-cache.md` "Cache
//! Invalidation"). Mirrors [`crate::lsp::ServerSupervisor`]'s actor pattern —
//! an `mpsc`-driven loop with no stored `JoinHandle`, torn down via an
//! explicit `Shutdown` + `oneshot` ack rather than relying on drop.
//!
//! The `notify` watcher itself runs its callback on its own OS thread (not on
//! a tokio worker), so the callback can only hand events off with
//! `blocking_send`. Events are pre-filtered by [`select_for_uri`] (adapter
//! knows the extension) and a single root-level `.gitignore` — a documented
//! 0.0.1 simplification versus `discover_files`'s full nested-`.gitignore`
//! `WalkBuilder`; under-exclusion here only costs a wasted reconcile, not
//! correctness. A 300ms debounce coalesces bursts (editors often emit several
//! events per save) before uris are reconciled sequentially, one LSP
//! connection at a time, matching the codebase's serial-stdio posture.
//!
//! macOS's FSEvents backend (what `notify` uses there) reports symlink-
//! resolved paths — e.g. `/private/tmp/...` for a watched `/tmp/...` root,
//! since `/tmp` is itself a symlink. Left uncorrected, this produces uris
//! that don't match the ones `discover_files`/`index_repository` wrote at
//! index time (which walks the literal root, unresolved), so every edit
//! would look like a brand-new file instead of reconciling the existing one.
//! [`Actor::filter`] rewrites each event path back onto the literal root
//! before converting it to a uri.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Sleep, sleep};

use crate::adapters::select_for_uri;
use crate::graph::DbActor;
use crate::indexer::path_to_uri;
use crate::query::QueryRuntime;

use super::reconcile::reconcile_uri;

const DEBOUNCE: Duration = Duration::from_millis(300);

enum WatcherMsg {
    Shutdown { reply: oneshot::Sender<()> },
}

/// Handle to a running [`FsWatcher`]. Cheap to clone; dropping every clone
/// without calling [`shutdown`](Self::shutdown) leaves the actor to tear
/// itself down when its channel closes (detached, best-effort — the same
/// fallback `ServerSupervisor` uses).
#[derive(Clone)]
pub struct FsWatcherHandle {
    tx: mpsc::Sender<WatcherMsg>,
}

impl FsWatcherHandle {
    /// Stop the watcher and wait for it to finish tearing down.
    pub async fn shutdown(&self) {
        let (reply_tx, reply_rx) = oneshot::channel();
        if self
            .tx
            .send(WatcherMsg::Shutdown { reply: reply_tx })
            .await
            .is_ok()
        {
            let _ = reply_rx.await;
        }
    }
}

/// Namespace for [`FsWatcher::spawn`]; the actor itself is a private struct.
pub struct FsWatcher;

impl FsWatcher {
    /// Start watching `root` recursively and spawn the reconcile actor.
    /// Non-fatal to the caller on error — `main.rs` logs and continues
    /// without live invalidation.
    pub fn spawn(
        db: DbActor,
        query_runtime: Arc<QueryRuntime>,
        root: PathBuf,
        root_uri: String,
    ) -> Result<FsWatcherHandle> {
        let gitignore = Self::load_gitignore(&root);
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        let (event_tx, event_rx) = mpsc::channel::<PathBuf>(256);
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else {
                return;
            };
            if !matches!(
                event.kind,
                EventKind::Any | EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                return;
            }
            for path in event.paths {
                // Runs on notify's own OS thread, not a tokio worker.
                let _ = event_tx.blocking_send(path);
            }
        })
        .context("fs watcher: failed to create notify watcher")?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .with_context(|| format!("fs watcher: failed to watch {}", root.display()))?;

        let (msg_tx, msg_rx) = mpsc::channel::<WatcherMsg>(8);
        let actor = Actor {
            _watcher: watcher,
            db,
            query_runtime,
            root,
            canonical_root,
            root_uri,
            gitignore,
            event_rx,
            msg_rx,
        };
        tokio::spawn(actor.run());
        Ok(FsWatcherHandle { tx: msg_tx })
    }

    fn load_gitignore(root: &Path) -> Gitignore {
        let (gitignore, _not_found_or_parse_error) = Gitignore::new(root.join(".gitignore"));
        gitignore
    }
}

struct Actor {
    // Held only to keep the OS watch alive; dropped (and thus stopped) when
    // the actor loop exits.
    _watcher: RecommendedWatcher,
    db: DbActor,
    query_runtime: Arc<QueryRuntime>,
    // Literal root as given on the command line; event paths are rewritten
    // onto this before becoming uris, so they match `discover_files`'s uris.
    root: PathBuf,
    // Symlink-resolved root; what `notify`'s FSEvents backend actually
    // reports paths relative to on macOS.
    canonical_root: PathBuf,
    root_uri: String,
    gitignore: Gitignore,
    event_rx: mpsc::Receiver<PathBuf>,
    msg_rx: mpsc::Receiver<WatcherMsg>,
}

impl Actor {
    async fn run(mut self) {
        let mut pending: HashSet<String> = HashSet::new();
        let mut debounce: Option<Pin<Box<Sleep>>> = None;
        loop {
            tokio::select! {
                biased;

                msg = self.msg_rx.recv() => match msg {
                    Some(WatcherMsg::Shutdown { reply }) => {
                        let _ = reply.send(());
                        break;
                    }
                    None => break,
                },

                path = self.event_rx.recv() => match path {
                    Some(path) => {
                        if let Some(uri) = self.filter(&path) {
                            pending.insert(uri);
                            debounce = Some(Box::pin(sleep(DEBOUNCE)));
                        }
                    }
                    None => break,
                },

                _ = async {
                    match debounce.as_mut() {
                        Some(timer) => timer.await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    debounce = None;
                    let uris: Vec<String> = pending.drain().collect();
                    for uri in uris {
                        // Watcher yields to live queries (`docs/design/indexing-and-cache.md`):
                        // defer starting this file's reconcile while a
                        // foreground query holds the LSP server, so the
                        // watcher's own `documentSymbol` traffic doesn't
                        // compound with it. Doesn't preempt a reconcile
                        // already in flight.
                        self.query_runtime.wait_until_query_idle().await;
                        if let Err(err) = reconcile_uri(
                            &self.db,
                            &self.query_runtime,
                            &self.root_uri,
                            &uri,
                        )
                        .await
                        {
                            eprintln!("semnav: watcher reconcile failed for {uri}: {err:#}");
                        }
                    }
                }
            }
        }
    }

    /// `None` for events outside a known adapter's extensions or matched by
    /// the root `.gitignore`; `Some(uri)` otherwise.
    fn filter(&self, path: &Path) -> Option<String> {
        let path = self.rebase_onto_literal_root(path);
        if self.gitignore.matched(&path, path.is_dir()).is_ignore() {
            return None;
        }
        let uri = path_to_uri(&path);
        select_for_uri(&uri)?;
        Some(uri)
    }

    /// Rewrite a `notify`-reported path back onto [`Self::root`] (the literal,
    /// unresolved root passed at startup), undoing macOS FSEvents' symlink
    /// resolution so the resulting uri matches what `discover_files` indexed.
    fn rebase_onto_literal_root(&self, path: &Path) -> PathBuf {
        match path.strip_prefix(&self.canonical_root) {
            Ok(rel) => self.root.join(rel),
            Err(_) => path.to_path_buf(),
        }
    }
}
