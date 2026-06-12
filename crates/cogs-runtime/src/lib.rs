//! Runtime orchestration: writer election, file watching, and keeping the
//! in-memory VaultIndex + on-disk graph DB in sync with the vault.
//!
//! One process per vault wins the writer lock and becomes the *primary*: it
//! owns the read-write GraphDb, watches the filesystem, and runs incremental
//! syncs. Other processes run as *readers* — their in-memory index still
//! works fully (it's process-local), they just never write the DB.

use std::collections::HashSet;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{unbounded, Sender};
use tracing::{error, info, warn};

use cogs_core::config::Vault;
use cogs_core::parse::parse_note;
use cogs_core::scan::VaultScanner;
use cogs_core::VaultIndex;
use cogs_graph::{GraphDb, SyncEngine};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Primary,
    Reader,
}

enum Msg {
    /// Filesystem reported changes (paths are absolute).
    FsChanged(Vec<PathBuf>),
    /// An editor overlay changed: (rel_path, Some(text)) on open/change,
    /// (rel_path, None) when the doc closes and disk truth resumes.
    Overlay(String, Option<String>),
    /// Force an incremental DB sync now (e.g. on didSave).
    SyncNow,
    Shutdown,
}

/// Handle to the runtime. Cloneable view of shared state plus the channel
/// into the indexer thread.
pub struct Runtime {
    pub vault: Arc<Vault>,
    pub index: Arc<RwLock<VaultIndex>>,
    pub role: Role,
    tx: Sender<Msg>,
    /// For gating editor overlays: excluded files (index.md, log.md, …) must
    /// never enter the in-memory index even when opened in the editor.
    scanner: VaultScanner,
    /// Kept alive for the runtime's lifetime; Mutex only for Sync.
    _watcher: Option<std::sync::Mutex<Debouncer>>,
}

type Debouncer = notify_debouncer_full::Debouncer<
    notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

impl Runtime {
    /// Build the in-memory index synchronously, elect a role, start the
    /// indexer thread (and watcher when `watch`).
    pub fn start(vault: Vault, watch: bool) -> Result<Runtime> {
        let vault = Arc::new(vault);
        std::fs::create_dir_all(vault.runtime_dir())?;

        // ---- in-memory index ------------------------------------------------
        let scanner = VaultScanner::new(&vault)?;
        let (note_paths, _) = scanner.walk(&vault.root)?;
        let mut index = VaultIndex::default();
        for p in &note_paths {
            match std::fs::read_to_string(vault.root.join(p)) {
                Ok(text) => index.upsert(parse_note(p, &text, &vault.config)),
                Err(e) => warn!("skipping unreadable note {p}: {e}"),
            }
        }
        index.rebuild_derived();
        info!(notes = index.len(), "vault index built");
        let index = Arc::new(RwLock::new(index));

        // ---- writer election ---------------------------------------------
        let lock_path = vault.runtime_dir().join("writer.lock");
        let lock_file = File::create(&lock_path)
            .with_context(|| format!("creating {}", lock_path.display()))?;
        // Leak the lock intentionally: the flock must live for the process
        // lifetime and is auto-released by the OS on exit/crash.
        let lock: &'static mut fd_lock::RwLock<File> =
            Box::leak(Box::new(fd_lock::RwLock::new(lock_file)));
        let role = match lock.try_write() {
            Ok(guard) => {
                std::mem::forget(guard);
                Role::Primary
            }
            Err(_) => Role::Reader,
        };
        info!(?role, "writer election complete");

        // ---- indexer thread -----------------------------------------------
        let (tx, rx) = unbounded::<Msg>();
        {
            let vault = Arc::clone(&vault);
            let index = Arc::clone(&index);
            let is_primary = role == Role::Primary;
            std::thread::Builder::new()
                .name("cogs-indexer".into())
                .spawn(move || {
                    let db = if is_primary {
                        match GraphDb::open_rw(&vault, false) {
                            Ok(db) => Some(db),
                            Err(e) => {
                                error!("could not open graph db read-write: {e:#}");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let engine = match SyncEngine::new(&vault) {
                        Ok(e) => Some(e),
                        Err(e) => {
                            error!("could not build sync engine: {e:#}");
                            None
                        }
                    };
                    let provider = if is_primary && vault.config.embeddings.enabled {
                        match cogs_graph::make_provider(&vault.config.embeddings) {
                            Ok(p) => Some(p),
                            Err(e) => {
                                warn!("embeddings unavailable: {e:#}");
                                None
                            }
                        }
                    } else {
                        None
                    };
                    // Initial DB sync so the graph reflects the working tree.
                    if let (Some(db), Some(engine)) = (&db, &engine) {
                        match engine.sync_with(db, false, provider.as_deref()) {
                            Ok(out) => info!(?out, "initial graph sync complete"),
                            Err(e) => error!("initial graph sync failed: {e:#}"),
                        }
                    }
                    indexer_loop(rx, vault, index, db, engine, provider);
                })
                .expect("spawning indexer thread");
        }

        // ---- watcher --------------------------------------------------------
        let watcher = if watch {
            match start_watcher(&vault, tx.clone()) {
                Ok(w) => Some(std::sync::Mutex::new(w)),
                Err(e) => {
                    warn!("file watcher unavailable: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        let scanner = VaultScanner::new(&vault)?;
        Ok(Runtime { vault, index, role, tx, scanner, _watcher: watcher })
    }

    /// Whether a vault-relative path is a note per the config globs (opened
    /// docs that aren't — index.md, log.md, files outside the notes globs —
    /// must not enter the index or get wiki diagnostics).
    pub fn is_note(&self, rel_path: &str) -> bool {
        self.scanner.is_note(rel_path)
    }

    /// Editor overlay update: re-parse `text` for `rel_path` and refresh the
    /// in-memory index immediately (synchronous, cheap). DB sync happens on
    /// save/watcher events, not on every keystroke.
    pub fn update_overlay(&self, rel_path: &str, text: Option<String>) {
        if !self.scanner.is_note(rel_path) {
            return;
        }
        if let Some(text) = &text {
            let note = parse_note(rel_path, text, &self.vault.config);
            let mut idx = self.index.write().unwrap();
            idx.upsert(note);
            idx.rebuild_derived();
        }
        let _ = self.tx.send(Msg::Overlay(rel_path.to_string(), text));
    }

    /// Request an incremental DB sync (no-op for readers).
    pub fn sync_now(&self) {
        let _ = self.tx.send(Msg::SyncNow);
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
    }

    /// Vault-relative path (forward slashes) for an absolute path, if inside
    /// the vault. Canonicalizes on miss so symlinked roots (e.g. macOS /tmp →
    /// /private/tmp) still match.
    pub fn rel_path(&self, abs: &std::path::Path) -> Option<String> {
        let rel = abs
            .strip_prefix(&self.vault.root)
            .ok()
            .map(|p| p.to_path_buf())
            .or_else(|| {
                let canon = abs.canonicalize().ok()?;
                canon.strip_prefix(&self.vault.root).ok().map(|p| p.to_path_buf())
            })?;
        Some(rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
    }
}

fn start_watcher(vault: &Arc<Vault>, tx: Sender<Msg>) -> Result<Debouncer> {
    use notify::RecursiveMode;
    use notify_debouncer_full::new_debouncer;

    let state_dir = vault.state_dir();
    let mut debouncer = new_debouncer(
        Duration::from_millis(400),
        None,
        move |result: notify_debouncer_full::DebounceEventResult| match result {
            Ok(events) => {
                let paths: Vec<PathBuf> = events
                    .into_iter()
                    .flat_map(|e| e.event.paths.clone())
                    .filter(|p| !p.starts_with(&state_dir))
                    .collect();
                if !paths.is_empty() {
                    let _ = tx.send(Msg::FsChanged(paths));
                }
            }
            Err(errors) => {
                for e in errors {
                    warn!("watch error: {e}");
                }
            }
        },
    )?;
    debouncer.watch(&vault.root, RecursiveMode::Recursive)?;
    info!("watching {}", vault.root.display());
    Ok(debouncer)
}

fn indexer_loop(
    rx: crossbeam_channel::Receiver<Msg>,
    vault: Arc<Vault>,
    index: Arc<RwLock<VaultIndex>>,
    db: Option<GraphDb>,
    engine: Option<SyncEngine>,
    provider: Option<Box<dyn cogs_graph::EmbeddingProvider>>,
) {
    let scanner = match VaultScanner::new(&vault) {
        Ok(s) => s,
        Err(e) => {
            error!("indexer: scanner construction failed: {e:#}");
            return;
        }
    };
    // Paths whose truth currently lives in the editor, not on disk.
    let mut overlays: HashSet<String> = HashSet::new();

    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Shutdown => break,
            Msg::Overlay(rel, text) => {
                match text {
                    Some(_) => {
                        overlays.insert(rel);
                    }
                    None => {
                        // Doc closed: disk truth resumes; re-parse from disk.
                        overlays.remove(&rel);
                        refresh_from_disk(&vault, &index, &scanner, &[rel]);
                    }
                }
            }
            Msg::FsChanged(paths) => {
                let rels: Vec<String> = paths
                    .iter()
                    .filter_map(|p| {
                        p.strip_prefix(&vault.root).ok().map(|r| {
                            r.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
                        })
                    })
                    .filter(|rel| !overlays.contains(rel))
                    .collect();
                refresh_from_disk(&vault, &index, &scanner, &rels);
                run_sync(&db, &engine, provider.as_deref());
            }
            Msg::SyncNow => run_sync(&db, &engine, provider.as_deref()),
        }
    }
}

/// Re-parse the given vault-relative paths from disk into the index
/// (upserting present files, removing vanished ones).
fn refresh_from_disk(
    vault: &Vault,
    index: &Arc<RwLock<VaultIndex>>,
    scanner: &VaultScanner,
    rels: &[String],
) {
    let note_rels: Vec<&String> = rels.iter().filter(|r| scanner.is_note(r)).collect();
    if note_rels.is_empty() {
        return;
    }
    let mut idx = index.write().unwrap();
    for rel in note_rels {
        let abs = vault.root.join(rel);
        match std::fs::read_to_string(&abs) {
            Ok(text) => {
                idx.upsert(parse_note(rel, &text, &vault.config));
            }
            Err(_) => {
                idx.remove_by_path(rel);
            }
        }
    }
    idx.rebuild_derived();
}

fn run_sync(
    db: &Option<GraphDb>,
    engine: &Option<SyncEngine>,
    provider: Option<&dyn cogs_graph::EmbeddingProvider>,
) {
    if let (Some(db), Some(engine)) = (db, engine) {
        match engine.sync_with(db, false, provider) {
            Ok(out) => {
                if out.notes_synced + out.notes_relinked + out.deleted + out.resources_synced > 0 {
                    info!(
                        notes = out.notes_synced,
                        relinked = out.notes_relinked,
                        resources = out.resources_synced,
                        deleted = out.deleted,
                        "graph sync"
                    );
                }
            }
            Err(e) => error!("graph sync failed: {e:#}"),
        }
    }
}
