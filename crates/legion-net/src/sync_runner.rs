//! Long-lived sync folder runner (`legion sync run`).
//!
//! One process per Legion home (enforced by an exclusive kernel lock on
//! `<state_dir>/sync/runner.lock`). Opens every folder listed for this machine in `sync.toml`,
//! watches it with native OS notifications (or periodic scans alone when `--no-watch` is set),
//! pulls incrementally from reachable members with exponential backoff for offline machines,
//! writes atomic per-folder status JSON files under `<state_dir>/sync/status/`, serves lock-free
//! MVCC index queries to `legion stream` over an owner-only local control endpoint, exchanges
//! `sync.toml` updates with reachable peers, and exits cleanly on SIGINT/SIGTERM.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use legion_core::config::{ConfigStore, NodeFile, parse};
use legion_core::proto::{CallParams, ErrorBody, ErrorCode};
use legion_core::{CancellationToken, EventSink, Paths};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use legion_sync::{
    BinEntry, ConflictChoice, ConflictInfo, DEFAULT_PAGE_LIMIT, DeletionBrakeConfig,
    DeletionBrakeDecision, FetchOptions, FolderConfig, FolderStatus, HandshakeHello,
    INDEX_DAMAGED_PROBLEM_SENTENCE, IgnoreMatcher, InboundDeletionBrakeDecision, MemberState,
    MemberStatus, ProblemKind, RealFs, RedbIndexStore, RestoreStrategy, ScanOptions, SyncEngine,
    SyncError, SyncProblem, check_fetch_file_from_store, check_folders_not_nested,
    serve_handshake_from_store, serve_records_since_from_store,
};
use legion_tools::sync::config::{
    SyncFile, SyncFolderEntry, apply_sync_merge_blocking, format_stamp_time, load_sync_file,
};
use legion_tools::sync::control::{
    ControlEnvelope, ControlRequest, ControlResponse, MAX_CONTROL_LINE_BYTES,
    PersistedFolderStatus, RunnerPidInfo, control_sock_path, control_sock_ref_path,
    control_token_path, ensure_owner_dir, folder_index_path, remove_folder_status,
    runner_lock_path, runner_pid_path, sync_state_dir, write_folder_status, write_owner_file,
};
use legion_tools::sync::{
    collect_held_back_summaries, expand_member_path, now_xfer_timestamp, sync_error_to_body,
};
use legion_xfer::Hash;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use crate::config::FleetFile;
use crate::pool::Fleet;
use crate::sync_peer::FleetSyncPeer;
use crate::sync_watch::{CoalescedScan, DirtyAccumulator, FolderWatcher, WatchSignal};

/// Runtime options for [`run`].
#[derive(Clone, Debug)]
pub struct RunnerConfig {
    /// Enable OS file watching (`inotify`/`FSEvents`/`ReadDirectoryChangesW`). When `false`,
    /// periodic full scans drive all change detection.
    pub watch: bool,
    /// Interval between periodic full scans and bin expiry passes.
    pub scan_interval: Duration,
    /// Interval between periodic reconciliation pulls from peers.
    pub pull_interval: Duration,
    /// Maximum index pages pulled from one peer in a single pull session before yielding to the
    /// next peer.
    pub pull_max_pages: usize,
    /// Maximum wall-clock duration spent pulling from one peer in a single pull session before
    /// yielding to the next peer.
    pub pull_max_duration: Duration,
    /// Optional override for the outbound deletion brake's move-awareness settle period in seconds
    /// (`None` uses the engine default of 30 seconds).
    pub brake_settle_secs: Option<u64>,
    /// Interval between periodic `sync.toml` exchanges with reachable fleet peers.
    pub config_sync_interval: Duration,
    /// Debounce window coalescing watcher events before scanning dirty subtrees.
    pub debounce: Duration,
    /// Wait between first and second look for newly modified files that may still be writing.
    pub stability_recheck: Duration,
    /// Initial retry backoff after a peer is unreachable (doubles up to `backoff_max`).
    pub backoff_base: Duration,
    /// Maximum retry backoff for an unreachable peer.
    pub backoff_max: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        let env_no_watch = std::env::var_os("LEGION_SYNC_NO_WATCH")
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
        Self {
            watch: !env_no_watch,
            scan_interval: Duration::from_secs(60),
            pull_interval: Duration::from_secs(15),
            pull_max_pages: 16,
            pull_max_duration: Duration::from_secs(30),
            brake_settle_secs: None,
            config_sync_interval: Duration::from_secs(60),
            debounce: Duration::from_millis(80),
            stability_recheck: Duration::from_millis(80),
            backoff_base: Duration::from_secs(2),
            backoff_max: Duration::from_secs(60),
        }
    }
}

/// Concurrent MVCC read handle for one active sync folder.
///
/// Serves `legion stream` (`ServeHandshake`, `ServeRecordsSince`, `ServeFetchCheck`) and read-only
/// conflict/bin listings without ever locking the writer engine thread — making AB-BA distributed
/// deadlocks impossible even while two nodes pull from each other simultaneously.
struct FolderReadHandle {
    folder_id: String,
    fs: RealFs,
    store: RedbIndexStore,
    ignore: RwLock<IgnoreMatcher>,
    healthy: AtomicBool,
    damaged: AtomicBool,
}

impl FolderReadHandle {
    fn check_healthy(&self) -> Result<(), ErrorBody> {
        if self.damaged.load(Ordering::Acquire) {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Sync folder {:?}: {}",
                    self.folder_id, INDEX_DAMAGED_PROBLEM_SENTENCE
                ),
            )
            .with_hint("Rebuild this folder's sync record explicitly with `legion sync.rebuild`."));
        }
        if !self.healthy.load(Ordering::Acquire) {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Sync folder {:?} is unavailable: {}",
                    self.folder_id,
                    legion_sync::FOLDER_MARKER_PROBLEM_SENTENCE
                ),
            ));
        }
        Ok(())
    }

    fn handshake(
        &self,
        caller: &str,
        hello: &HandshakeHello,
    ) -> Result<legion_sync::PeerHandshake, ErrorBody> {
        self.check_healthy()?;
        serve_handshake_from_store(&self.fs, &self.store, caller, hello).map_err(sync_error_to_body)
    }

    fn records_since(
        &self,
        caller: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<legion_sync::RecordsPage, ErrorBody> {
        self.check_healthy()?;
        serve_records_since_from_store(
            &self.fs,
            &self.store,
            caller,
            since_incarnation,
            since_seq,
            limit,
        )
        .map_err(sync_error_to_body)
    }

    fn check_fetch(&self, caller: &str, path: &str, expected_hash: Hash) -> Result<u64, ErrorBody> {
        self.check_healthy()?;
        let ignore = self.ignore.read().unwrap_or_else(|p| p.into_inner());
        check_fetch_file_from_store(&self.fs, &self.store, &ignore, caller, path, expected_hash)
            .map_err(sync_error_to_body)
    }

    fn list_conflicts(&self) -> Result<Vec<ConflictInfo>, ErrorBody> {
        use legion_sync::IndexStore;
        let mut out = Vec::new();
        for rec in self.store.list_conflicts().map_err(sync_error_to_body)? {
            if rec.deleted || rec.conflict_dismissed {
                continue;
            }
            if let Some(orig) = rec.conflict_of {
                out.push(ConflictInfo {
                    original_path: orig,
                    conflict_path: rec.path,
                    conflict_member: rec.modified_by,
                    conflict_mtime: rec.mtime,
                    conflict_size: rec.size,
                    conflict_hash: rec.hash,
                });
            }
        }
        out.sort_by(|a, b| {
            a.original_path
                .cmp(&b.original_path)
                .then_with(|| a.conflict_path.cmp(&b.conflict_path))
        });
        Ok(out)
    }

    fn list_bin(&self) -> Result<Vec<BinEntry>, ErrorBody> {
        use legion_sync::IndexStore;
        let mut entries = self.store.list_bin_entries().map_err(sync_error_to_body)?;
        entries.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| a.original_path.cmp(&b.original_path))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(entries)
    }
}

/// Command sent to one folder's dedicated worker thread.
enum FolderCmd {
    Watch(WatchSignal),
    UpdateConfig(Box<SyncFolderEntry>),
    SyncNow {
        reply: Option<oneshot::Sender<Result<PersistedFolderStatus, ErrorBody>>>,
    },
    Pause {
        reply: oneshot::Sender<Result<PersistedFolderStatus, ErrorBody>>,
    },
    Resume {
        reply: oneshot::Sender<Result<PersistedFolderStatus, ErrorBody>>,
    },
    ResolveBrake {
        decision: DeletionBrakeDecision,
        reply: oneshot::Sender<Result<PersistedFolderStatus, ErrorBody>>,
    },
    ResolveInboundBrake {
        peer: String,
        decision: InboundDeletionBrakeDecision,
        reply: oneshot::Sender<Result<PersistedFolderStatus, ErrorBody>>,
    },
    DismissProblem {
        kind: ProblemKind,
        peer: Option<String>,
        reply: oneshot::Sender<Result<(usize, PersistedFolderStatus), ErrorBody>>,
    },
    ResolveConflict {
        conflict_path: String,
        choice: ConflictChoice,
        reply: oneshot::Sender<Result<PersistedFolderStatus, ErrorBody>>,
    },
    RestoreBin {
        entry_id: String,
        dest_path: Option<String>,
        strategy: RestoreStrategy,
        reply: oneshot::Sender<Result<String, ErrorBody>>,
    },
    Shutdown,
}

struct RunningFolder {
    local_path: PathBuf,
    read_handle: Arc<FolderReadHandle>,
    tx: std_mpsc::Sender<FolderCmd>,
    cancel_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

struct DamagedFolder {
    local_path: PathBuf,
    entry: SyncFolderEntry,
}

enum FolderSlot {
    Running(RunningFolder),
    Damaged(Box<DamagedFolder>),
}

impl FolderSlot {
    fn local_path(&self) -> &PathBuf {
        match self {
            Self::Running(rf) => &rf.local_path,
            Self::Damaged(df) => &df.local_path,
        }
    }
}

enum SpawnFolderError {
    IndexDamaged,
    Other(String),
}

/// Shared coordinator state accessed by the local control server and reload/config-sync loops.
struct RunnerState {
    paths: Paths,
    node_name: String,
    fleet: Fleet,
    config_store: Arc<ConfigStore>,
    runner_cfg: RunnerConfig,
    runtime: Handle,
    shutdown: CancellationToken,
    poke_tx: tokio::sync::mpsc::UnboundedSender<PokeEvent>,
    reload_tx: tokio::sync::mpsc::UnboundedSender<()>,
    folders: Mutex<BTreeMap<String, FolderSlot>>,
}

#[derive(Clone, Debug)]
struct PokeEvent {
    folder_id: String,
    peers: Vec<String>,
}

impl RunnerState {
    /// Reloads `sync.toml`, opening newly added folders, updating config on changed folders, and
    /// stopping removed folders (leaving files untouched on disk).
    fn reload_folders(&self) -> Result<(), ErrorBody> {
        let sync_file = load_sync_file(&self.paths)?;
        let desired_list = sync_file.folders_for_machine(&self.node_name);
        let desired_ids: BTreeSet<String> =
            desired_list.iter().map(|(id, _, _)| (*id).to_owned()).collect();

        // Check for overlapping paths among this node's desired folders before opening.
        let mut accepted_roots: Vec<(String, PathBuf)> = Vec::new();
        let mut overlap_errors: BTreeMap<String, String> = BTreeMap::new();
        for (id, _entry, member) in &desired_list {
            let root = expand_member_path(&self.paths, &member.path);
            let existing_refs: Vec<&std::path::Path> =
                accepted_roots.iter().map(|(_, p)| p.as_path()).collect();
            if let Err(err) = check_folders_not_nested(&root, &existing_refs) {
                overlap_errors.insert((*id).to_owned(), err.to_string());
            } else {
                accepted_roots.push(((*id).to_owned(), root));
            }
        }

        // 1. Stop folders that are no longer active for this machine (or whose root path changed).
        let mut to_join: Vec<RunningFolder> = Vec::new();
        {
            let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
            let current_ids: Vec<String> = guard.keys().cloned().collect();
            for id in current_ids {
                let desired_path = sync_file
                    .folders
                    .get(&id)
                    .and_then(|f| f.members.get(&self.node_name))
                    .map(|m| expand_member_path(&self.paths, &m.path));
                let remove = !desired_ids.contains(&id)
                    || overlap_errors.contains_key(&id)
                    || guard
                        .get(&id)
                        .is_some_and(|slot| Some(slot.local_path()) != desired_path.as_ref());
                if remove && let Some(slot) = guard.remove(&id) {
                    if let FolderSlot::Running(rf) = slot {
                        rf.cancel_flag.store(true, Ordering::Relaxed);
                        let _ = rf.tx.send(FolderCmd::Shutdown);
                        to_join.push(rf);
                    }
                    if !desired_ids.contains(&id) {
                        remove_folder_status(&self.paths, &id);
                    }
                }
            }
        }
        for mut rf in to_join {
            if let Some(handle) = rf.thread.take() {
                let _ = handle.join();
            }
        }

        // 2. Open new folders or send updated config to existing folders.
        for (id, entry, member) in desired_list {
            let root = expand_member_path(&self.paths, &member.path);
            if let Some(reason) = overlap_errors.get(id) {
                write_open_problem_status(&self.paths, id, entry, &self.node_name, &root, reason);
                continue;
            }
            let existing = {
                let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                match guard.get_mut(id) {
                    Some(FolderSlot::Running(rf)) => {
                        Some(Ok((rf.tx.clone(), Arc::clone(&rf.read_handle))))
                    }
                    Some(FolderSlot::Damaged(df)) => {
                        df.entry = entry.clone();
                        Some(Err(()))
                    }
                    None => None,
                }
            };
            match existing {
                Some(Ok((tx, read_handle))) => {
                    if let Ok(matcher) = IgnoreMatcher::compile(&entry.ignore) {
                        let mut ig = read_handle.ignore.write().unwrap_or_else(|p| p.into_inner());
                        *ig = matcher;
                    }
                    let _ = tx.send(FolderCmd::UpdateConfig(Box::new(entry.clone())));
                    continue;
                }
                Some(Err(())) => {
                    // Folder index is damaged: keep it stopped and never auto-rebuild.
                    let _ = write_damaged_index_status(&self.paths, id, entry, &self.node_name, &root);
                    continue;
                }
                None => {}
            }

            match self.spawn_folder_worker(id, entry, root.clone()) {
                Ok(rf) => {
                    let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                    guard.insert(id.to_owned(), FolderSlot::Running(rf));
                }
                Err(SpawnFolderError::IndexDamaged) => {
                    let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                    guard.insert(
                        id.to_owned(),
                        FolderSlot::Damaged(Box::new(DamagedFolder {
                            local_path: root.clone(),
                            entry: entry.clone(),
                        })),
                    );
                    let _ = write_damaged_index_status(&self.paths, id, entry, &self.node_name, &root);
                }
                Err(SpawnFolderError::Other(reason)) => {
                    write_open_problem_status(&self.paths, id, entry, &self.node_name, &root, &reason);
                }
            }
        }
        Ok(())
    }

    fn build_folder_config(&self, folder_id: &str, entry: &SyncFolderEntry) -> FolderConfig {
        let mut cfg = FolderConfig::new(folder_id, &self.node_name, entry.members.keys().cloned());
        cfg.former_members = entry.removed_members.keys().cloned().collect();
        cfg.ignore = entry.ignore.clone();
        cfg.bin_retention_secs = u64::from(entry.bin_days).saturating_mul(86_400);
        if let Some(settle_secs) = self.runner_cfg.brake_settle_secs {
            cfg.deletion_brake = DeletionBrakeConfig {
                settle_secs,
                ..DeletionBrakeConfig::default()
            };
        }
        cfg
    }

    fn spawn_folder_worker(
        &self,
        folder_id: &str,
        entry: &SyncFolderEntry,
        local_root: PathBuf,
    ) -> Result<RunningFolder, SpawnFolderError> {
        let cfg = self.build_folder_config(folder_id, entry);

        let real_fs =
            RealFs::open(&local_root).map_err(|e| SpawnFolderError::Other(e.to_string()))?;
        let store = match RedbIndexStore::open(folder_index_path(&self.paths, folder_id)) {
            Ok(s) => s,
            Err(SyncError::IndexDamaged { .. } | SyncError::CorruptData { .. }) => {
                return Err(SpawnFolderError::IndexDamaged);
            }
            Err(err) => return Err(SpawnFolderError::Other(err.to_string())),
        };
        let read_store = store.clone();
        let read_fs = real_fs.clone();
        let ignore_matcher = IgnoreMatcher::compile(&entry.ignore)
            .map_err(|e| SpawnFolderError::Other(e.to_string()))?;

        let mut engine = match SyncEngine::open(cfg, real_fs, store) {
            Ok(e) => e,
            Err(SyncError::IndexDamaged { .. } | SyncError::CorruptData { .. }) => {
                return Err(SpawnFolderError::IndexDamaged);
            }
            Err(err) => return Err(SpawnFolderError::Other(err.to_string())),
        };
        if entry.paused && !engine.is_index_damaged() {
            let _ = engine.set_paused(true);
        }

        let damaged = engine.is_index_damaged();
        let healthy = !engine.has_folder_problem();
        let read_handle = Arc::new(FolderReadHandle {
            folder_id: folder_id.to_owned(),
            fs: read_fs,
            store: read_store,
            ignore: RwLock::new(ignore_matcher),
            healthy: AtomicBool::new(healthy),
            damaged: AtomicBool::new(damaged),
        });

        let (tx, rx) = std_mpsc::channel::<FolderCmd>();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        let ctx = FolderWorkerCtx {
            folder_id: folder_id.to_owned(),
            node_name: self.node_name.clone(),
            paths: self.paths.clone(),
            local_root: local_root.clone(),
            fleet: self.fleet.clone(),
            config_store: self.config_store.clone(),
            runner_cfg: self.runner_cfg.clone(),
            runtime: self.runtime.clone(),
            shutdown: self.shutdown.clone(),
            cancel_flag: Arc::clone(&cancel_flag),
            read_handle: Arc::clone(&read_handle),
            poke_tx: self.poke_tx.clone(),
            reload_tx: self.reload_tx.clone(),
            entry: entry.clone(),
            tx_self: tx.clone(),
        };

        let thread_name = format!("legion-sync-{folder_id}");
        let handle = thread::Builder::new()
            .name(thread_name)
            .spawn(move || folder_worker_loop(ctx, engine, rx))
            .map_err(|e| {
                SpawnFolderError::Other(format!("could not spawn folder worker thread: {e}"))
            })?;

        Ok(RunningFolder {
            local_path: local_root,
            read_handle,
            tx,
            cancel_flag,
            thread: Some(handle),
        })
    }

    fn shutdown_all(&self) {
        let drained: Vec<RunningFolder> = {
            let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
            let keys: Vec<String> = guard.keys().cloned().collect();
            keys.into_iter()
                .filter_map(|k| match guard.remove(&k) {
                    Some(FolderSlot::Running(rf)) => Some(rf),
                    _ => None,
                })
                .collect()
        };
        for rf in &drained {
            rf.cancel_flag.store(true, Ordering::Relaxed);
            let _ = rf.tx.send(FolderCmd::Shutdown);
        }
        for mut rf in drained {
            if let Some(handle) = rf.thread.take() {
                let _ = handle.join();
            }
        }
    }

    async fn handle_control_request(&self, req: ControlRequest) -> ControlResponse {
        match req {
            ControlRequest::Ping => ControlResponse::Ok,
            ControlRequest::Reload => match self.reload_folders() {
                Ok(()) => ControlResponse::Ok,
                Err(error) => ControlResponse::Error { error },
            },
            ControlRequest::SyncNow { folder_id, wait } => {
                if let Err(error) = self.reload_folders() {
                    return ControlResponse::Error { error };
                }
                let mut running_targets: Vec<(String, std_mpsc::Sender<FolderCmd>)> = Vec::new();
                let mut damaged_statuses: Vec<PersistedFolderStatus> = Vec::new();
                {
                    let guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                    match folder_id {
                        Some(ref id) => match guard.get(id) {
                            Some(FolderSlot::Running(rf)) => {
                                running_targets.push((id.clone(), rf.tx.clone()));
                            }
                            Some(FolderSlot::Damaged(df)) => {
                                damaged_statuses.push(write_damaged_index_status(
                                    &self.paths,
                                    id,
                                    &df.entry,
                                    &self.node_name,
                                    &df.local_path,
                                ));
                            }
                            None => {
                                return ControlResponse::Error {
                                    error: ErrorBody::new(
                                        ErrorCode::NotFound,
                                        format!("Sync folder {id:?} is not active on this machine."),
                                    ),
                                };
                            }
                        },
                        None => {
                            for (k, slot) in guard.iter() {
                                match slot {
                                    FolderSlot::Running(rf) => {
                                        running_targets.push((k.clone(), rf.tx.clone()));
                                    }
                                    FolderSlot::Damaged(df) => {
                                        damaged_statuses.push(write_damaged_index_status(
                                            &self.paths,
                                            k,
                                            &df.entry,
                                            &self.node_name,
                                            &df.local_path,
                                        ));
                                    }
                                }
                            }
                        }
                    }
                };
                if !wait {
                    for (_, tx) in running_targets {
                        let _ = tx.send(FolderCmd::SyncNow { reply: None });
                    }
                    return ControlResponse::Ok;
                }
                let mut statuses = damaged_statuses;
                for (_, tx) in running_targets {
                    let (rtx, rrx) = oneshot::channel();
                    if tx.send(FolderCmd::SyncNow { reply: Some(rtx) }).is_err() {
                        continue;
                    }
                    match rrx.await {
                        Ok(Ok(st)) => statuses.push(st),
                        Ok(Err(error)) => return ControlResponse::Error { error },
                        Err(_) => {}
                    }
                }
                ControlResponse::Statuses { folders: statuses }
            }
            ControlRequest::Pause { folder_id } => {
                let _ = self.reload_folders();
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::Pause { reply: rtx });
                match rrx.await {
                    Ok(Ok(st)) => ControlResponse::Statuses { folders: vec![st] },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::Resume { folder_id } => {
                let _ = self.reload_folders();
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::Resume { reply: rtx });
                match rrx.await {
                    Ok(Ok(st)) => ControlResponse::Statuses { folders: vec![st] },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::ResolveBrake {
                folder_id,
                decision,
            } => {
                let _ = self.reload_folders();
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::ResolveBrake {
                    decision,
                    reply: rtx,
                });
                match rrx.await {
                    Ok(Ok(st)) => ControlResponse::Statuses { folders: vec![st] },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::ResolveInboundBrake {
                folder_id,
                peer,
                decision,
            } => {
                let _ = self.reload_folders();
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::ResolveInboundBrake {
                    peer,
                    decision,
                    reply: rtx,
                });
                match rrx.await {
                    Ok(Ok(st)) => ControlResponse::Statuses { folders: vec![st] },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::RebuildIndex { folder_id, force } => {
                let _ = self.reload_folders();
                let sync_file = match load_sync_file(&self.paths) {
                    Ok(sf) => sf,
                    Err(error) => return ControlResponse::Error { error },
                };
                let Some(entry) = sync_file.folders.get(&folder_id).cloned() else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let Some(member) = entry.members.get(&self.node_name) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let local_root = expand_member_path(&self.paths, &member.path);

                let is_damaged = {
                    let guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                    match guard.get(&folder_id) {
                        Some(FolderSlot::Damaged(_)) => true,
                        Some(FolderSlot::Running(rf)) => rf.read_handle.damaged.load(Ordering::Acquire),
                        None => false,
                    }
                };
                if !is_damaged && !force {
                    return ControlResponse::Error {
                        error: ErrorBody::new(
                            ErrorCode::Conflict,
                            format!("Sync folder {folder_id:?} has a healthy sync record; refusing to rebuild without `force=true`."),
                        )
                        .with_hint("Pass `force=true` only if you intentionally want to rebuild the index from disk."),
                    };
                }

                // Stop and join any active worker so every `redb` handle is released before replacing the file.
                let removed_slot = {
                    let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                    guard.remove(&folder_id)
                };
                if let Some(FolderSlot::Running(mut rf)) = removed_slot {
                    rf.cancel_flag.store(true, Ordering::Relaxed);
                    let _ = rf.tx.send(FolderCmd::Shutdown);
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Some(handle) = rf.thread.take() {
                            let _ = handle.join();
                        }
                        drop(rf);
                    })
                    .await;
                }

                let cfg = self.build_folder_config(&folder_id, &entry);
                let paths_clone = self.paths.clone();
                let root_clone = local_root.clone();
                let fid_clone = folder_id.clone();
                let entry_clone = entry.clone();
                let rebuild_res = tokio::task::spawn_blocking(move || {
                    let real_fs = RealFs::open(&root_clone).map_err(sync_error_to_body)?;
                    let index_path = folder_index_path(&paths_clone, &fid_clone);
                    let (mut engine, report) = SyncEngine::rebuild_redb_from_disk(
                        cfg,
                        real_fs,
                        &index_path,
                        now_xfer_timestamp(),
                    )
                    .map_err(sync_error_to_body)?;
                    if entry_clone.paused {
                        let _ = engine.set_paused(true);
                    }
                    let files_indexed = report.created + report.modified + report.unchanged;
                    let status = engine.status().map_err(sync_error_to_body)?;
                    let held_back = collect_held_back_summaries(&engine, &status);
                    let bin_entries = status.bin.len();
                    let persisted = PersistedFolderStatus {
                        folder_id: fid_clone,
                        folder_name: entry_clone.name,
                        local_path: root_clone.display().to_string(),
                        paused_by_config: entry_clone.paused,
                        index_damaged: engine.is_index_damaged(),
                        held_back,
                        updated_at: format_stamp_time(Timestamp::now()),
                        status,
                    };
                    write_folder_status(&paths_clone, &persisted)?;
                    drop(engine);
                    Ok::<_, ErrorBody>((files_indexed, bin_entries, persisted))
                })
                .await;

                let (files_indexed, bin_entries, persisted) = match rebuild_res {
                    Ok(Ok(tuple)) => tuple,
                    Ok(Err(error)) => {
                        let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                        guard.insert(
                            folder_id.clone(),
                            FolderSlot::Damaged(Box::new(DamagedFolder {
                                local_path: local_root,
                                entry,
                            })),
                        );
                        return ControlResponse::Error { error };
                    }
                    Err(e) => {
                        return ControlResponse::Error {
                            error: ErrorBody::new(
                                ErrorCode::Internal,
                                format!("rebuild task failed: {e}"),
                            ),
                        };
                    }
                };

                match self.spawn_folder_worker(&folder_id, &entry, local_root.clone()) {
                    Ok(rf) => {
                        let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                        guard.insert(folder_id.clone(), FolderSlot::Running(rf));
                    }
                    Err(SpawnFolderError::IndexDamaged) => {
                        let mut guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
                        guard.insert(
                            folder_id.clone(),
                            FolderSlot::Damaged(Box::new(DamagedFolder {
                                local_path: local_root,
                                entry,
                            })),
                        );
                        return ControlResponse::Error {
                            error: ErrorBody::new(
                                ErrorCode::Failed,
                                format!("Rebuilt index for {folder_id:?} could not be reopened."),
                            ),
                        };
                    }
                    Err(SpawnFolderError::Other(reason)) => {
                        return ControlResponse::Error {
                            error: ErrorBody::new(ErrorCode::Failed, reason),
                        };
                    }
                }

                ControlResponse::Rebuilt {
                    files_indexed,
                    bin_entries,
                    status: Box::new(persisted),
                }
            }
            ControlRequest::DismissProblem {
                folder_id,
                kind,
                peer,
            } => {
                let _ = self.reload_folders();
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::DismissProblem {
                    kind,
                    peer,
                    reply: rtx,
                });
                match rrx.await {
                    Ok(Ok((count, st))) => ControlResponse::Dismissed {
                        count,
                        status: Box::new(st),
                    },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::ListConflicts { folder_id } => {
                let Some(handle) = self.read_handle(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                match tokio::task::spawn_blocking(move || handle.list_conflicts()).await {
                    Ok(Ok(conflicts)) => ControlResponse::Conflicts { conflicts },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(e) => ControlResponse::Error {
                        error: ErrorBody::new(ErrorCode::Internal, format!("task failed: {e}")),
                    },
                }
            }
            ControlRequest::ResolveConflict {
                folder_id,
                conflict_path,
                choice,
            } => {
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::ResolveConflict {
                    conflict_path,
                    choice,
                    reply: rtx,
                });
                match rrx.await {
                    Ok(Ok(st)) => ControlResponse::Statuses { folders: vec![st] },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::ListBin { folder_id } => {
                let Some(handle) = self.read_handle(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                match tokio::task::spawn_blocking(move || handle.list_bin()).await {
                    Ok(Ok(entries)) => ControlResponse::Bin { entries },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(e) => ControlResponse::Error {
                        error: ErrorBody::new(ErrorCode::Internal, format!("task failed: {e}")),
                    },
                }
            }
            ControlRequest::RestoreBin {
                folder_id,
                entry_id,
                dest_path,
                strategy,
            } => {
                let Some(tx) = self.folder_tx(&folder_id) else {
                    return ControlResponse::Error {
                        error: not_active_error(&folder_id),
                    };
                };
                let (rtx, rrx) = oneshot::channel();
                let _ = tx.send(FolderCmd::RestoreBin {
                    entry_id,
                    dest_path,
                    strategy: strategy.into(),
                    reply: rtx,
                });
                match rrx.await {
                    Ok(Ok(path)) => ControlResponse::Restored { path },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(_) => ControlResponse::Error {
                        error: worker_stopped_error(&folder_id),
                    },
                }
            }
            ControlRequest::ServeHandshake {
                folder_id,
                caller,
                hello,
            } => {
                let handle = match self.read_handle_or_damaged(&folder_id) {
                    Ok(h) => h,
                    Err(error) => return ControlResponse::Error { error },
                };
                match tokio::task::spawn_blocking(move || handle.handshake(&caller, &hello)).await {
                    Ok(Ok(handshake)) => ControlResponse::Handshake { handshake },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(e) => ControlResponse::Error {
                        error: ErrorBody::new(ErrorCode::Internal, format!("task failed: {e}")),
                    },
                }
            }
            ControlRequest::ServeRecordsSince {
                folder_id,
                caller,
                since_incarnation,
                since_seq,
                limit,
            } => {
                let handle = match self.read_handle_or_damaged(&folder_id) {
                    Ok(h) => h,
                    Err(error) => return ControlResponse::Error { error },
                };
                match tokio::task::spawn_blocking(move || {
                    handle.records_since(&caller, since_incarnation, since_seq, limit)
                })
                .await
                {
                    Ok(Ok(page)) => ControlResponse::Records { page },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(e) => ControlResponse::Error {
                        error: ErrorBody::new(ErrorCode::Internal, format!("task failed: {e}")),
                    },
                }
            }
            ControlRequest::ServeFetchCheck {
                folder_id,
                caller,
                path,
                expected_hash,
            } => {
                let handle = match self.read_handle_or_damaged(&folder_id) {
                    Ok(h) => h,
                    Err(error) => return ControlResponse::Error { error },
                };
                match tokio::task::spawn_blocking(move || {
                    handle.check_fetch(&caller, &path, expected_hash)
                })
                .await
                {
                    Ok(Ok(size)) => ControlResponse::FetchAllowed { size },
                    Ok(Err(error)) => ControlResponse::Error { error },
                    Err(e) => ControlResponse::Error {
                        error: ErrorBody::new(ErrorCode::Internal, format!("task failed: {e}")),
                    },
                }
            }
        }
    }

    fn folder_tx(&self, folder_id: &str) -> Option<std_mpsc::Sender<FolderCmd>> {
        self.folders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(folder_id)
            .and_then(|slot| match slot {
                FolderSlot::Running(rf) => Some(rf.tx.clone()),
                FolderSlot::Damaged(_) => None,
            })
    }

    fn read_handle(&self, folder_id: &str) -> Option<Arc<FolderReadHandle>> {
        self.folders
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(folder_id)
            .and_then(|slot| match slot {
                FolderSlot::Running(rf) => Some(Arc::clone(&rf.read_handle)),
                FolderSlot::Damaged(_) => None,
            })
    }

    fn read_handle_or_damaged(&self, folder_id: &str) -> Result<Arc<FolderReadHandle>, ErrorBody> {
        let guard = self.folders.lock().unwrap_or_else(|p| p.into_inner());
        match guard.get(folder_id) {
            Some(FolderSlot::Running(rf)) => Ok(Arc::clone(&rf.read_handle)),
            Some(FolderSlot::Damaged(_)) => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Sync folder {folder_id:?}: {INDEX_DAMAGED_PROBLEM_SENTENCE}"),
            )
            .with_hint("Rebuild this folder's sync record explicitly with `legion sync.rebuild`.")),
            None => Err(not_active_error(folder_id)),
        }
    }
}

struct FolderWorkerCtx {
    folder_id: String,
    node_name: String,
    paths: Paths,
    local_root: PathBuf,
    fleet: Fleet,
    config_store: Arc<ConfigStore>,
    runner_cfg: RunnerConfig,
    runtime: Handle,
    shutdown: CancellationToken,
    cancel_flag: Arc<AtomicBool>,
    read_handle: Arc<FolderReadHandle>,
    poke_tx: tokio::sync::mpsc::UnboundedSender<PokeEvent>,
    reload_tx: tokio::sync::mpsc::UnboundedSender<()>,
    entry: SyncFolderEntry,
    tx_self: std_mpsc::Sender<FolderCmd>,
}

struct PeerBackoff {
    failures: u32,
    next_try: Instant,
}

fn compute_settle_deadline(engine: &SyncEngine<RealFs, RedbIndexStore>) -> Option<Instant> {
    let ready_at = engine.next_pending_deletion_ready_at().ok().flatten()?;
    let now = now_xfer_timestamp();
    if now.secs > ready_at.secs || (now.secs == ready_at.secs && now.nanos >= ready_at.nanos) {
        return Some(Instant::now());
    }
    let delta_secs = (ready_at.secs - now.secs).max(0) as u64;
    let extra_nanos = ready_at.nanos.saturating_sub(now.nanos);
    Some(Instant::now() + Duration::new(delta_secs, extra_nanos) + Duration::from_millis(25))
}

fn folder_worker_loop(
    mut ctx: FolderWorkerCtx,
    mut engine: SyncEngine<RealFs, RedbIndexStore>,
    rx: std_mpsc::Receiver<FolderCmd>,
) {
    // Start OS watcher unless disabled (skip watcher if index is already damaged).
    let _watcher = if ctx.runner_cfg.watch && !engine.is_index_damaged() {
        let tx = ctx.tx_self.clone();
        let (wtx, wrx) = std_mpsc::channel::<WatchSignal>();
        // Bridge WatchSignal -> FolderCmd without blocking notify's thread.
        let bridge_tx = tx.clone();
        let _ = thread::Builder::new()
            .name(format!("legion-watch-{}", ctx.folder_id))
            .spawn(move || {
                while let Ok(sig) = wrx.recv() {
                    if bridge_tx.send(FolderCmd::Watch(sig)).is_err() {
                        break;
                    }
                }
            });
        match FolderWatcher::start(ctx.local_root.clone(), wtx) {
            Ok(w) => Some(w),
            Err(err) => {
                let _ = io::Write::write_all(
                    &mut io::stderr(),
                    format!("legion sync: watcher fallback for {}: {err}\n", ctx.folder_id)
                        .as_bytes(),
                );
                None
            }
        }
    } else {
        None
    };

    let mut dirty = DirtyAccumulator::default();
    let mut debounce_deadline: Option<Instant> = None;
    let mut unstable_deadline: Option<Instant> = None;
    let mut unstable_paths: Vec<String> = Vec::new();
    let mut peer_backoffs: HashMap<String, PeerBackoff> = HashMap::new();
    let mut peers_cache: HashMap<String, FleetSyncPeer> = HashMap::new();
    let mut peer_rr_offset: usize = 0;

    // Initial reconciliation immediately on start: full scan + bin expiry + pull from every peer.
    // Never scan or pull if the index is damaged (nothing changes on disk until `sync.rebuild`).
    let mut initial_yielded = false;
    if !engine.is_index_damaged() {
        let seq_before_init = engine.meta().map(|m| m.local_seq).unwrap_or(0);
        if !ctx.entry.paused && !engine.is_paused().unwrap_or(false) {
            let now = now_xfer_timestamp();
            if let Ok(report) = engine.scan_with_options(
                now,
                ScanOptions {
                    require_second_look: false,
                    min_stable_age: Duration::ZERO,
                    ..ScanOptions::default()
                },
            ) && !report.held_back_unstable.is_empty()
            {
                unstable_paths = report.held_back_unstable;
                unstable_deadline = Some(Instant::now() + ctx.runner_cfg.stability_recheck);
            }
            let _ = engine.expire_bin(now);
            let pass = pull_all_peers(
                &mut ctx,
                &mut engine,
                &mut peers_cache,
                &mut peer_backoffs,
                &mut peer_rr_offset,
                true,
            );
            initial_yielded = pass.any_yielded;
        }
        let seq_after_init = engine.meta().map(|m| m.local_seq).unwrap_or(0);
        if seq_after_init > seq_before_init {
            poke_peers(&ctx, None);
        }
    }
    let _ = persist_status(&ctx, &engine);

    let mut next_full_scan = Instant::now() + ctx.runner_cfg.scan_interval;
    let mut next_pull = if initial_yielded {
        Instant::now() + Duration::from_millis(5)
    } else {
        Instant::now() + ctx.runner_cfg.pull_interval
    };

    loop {
        if ctx.cancel_flag.load(Ordering::Relaxed) {
            break;
        }
        let damaged = engine.is_index_damaged();
        let settle_deadline = if damaged {
            None
        } else {
            compute_settle_deadline(&engine)
        };
        let now_inst = Instant::now();
        let mut wake_at = next_full_scan.min(next_pull);
        if let Some(d) = debounce_deadline {
            wake_at = wake_at.min(d);
        }
        if let Some(u) = unstable_deadline {
            wake_at = wake_at.min(u);
        }
        if let Some(s) = settle_deadline {
            wake_at = wake_at.min(s);
        }
        for bo in peer_backoffs.values() {
            wake_at = wake_at.min(bo.next_try);
        }

        let wait = wake_at.saturating_duration_since(now_inst).min(Duration::from_secs(1));
        match rx.recv_timeout(wait) {
            Ok(cmd) => {
                if handle_folder_cmd(
                    &mut ctx,
                    &mut engine,
                    cmd,
                    &mut dirty,
                    &mut debounce_deadline,
                    &mut peers_cache,
                    &mut peer_backoffs,
                    &mut peer_rr_offset,
                    &mut next_pull,
                ) {
                    break;
                }
                // Drain any burst of queued commands immediately before scanning.
                while let Ok(extra) = rx.try_recv() {
                    if handle_folder_cmd(
                        &mut ctx,
                        &mut engine,
                        extra,
                        &mut dirty,
                        &mut debounce_deadline,
                        &mut peers_cache,
                        &mut peer_backoffs,
                        &mut peer_rr_offset,
                        &mut next_pull,
                    ) {
                        close_all_peers(&mut peers_cache);
                        return;
                    }
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if ctx.cancel_flag.load(Ordering::Relaxed) {
            break;
        }

        if engine.is_index_damaged() {
            let _ = persist_status(&ctx, &engine);
            continue;
        }

        let now_inst = Instant::now();
        let paused = ctx.entry.paused || engine.is_paused().unwrap_or(false);

        // 1. Debounced watcher batch -> ONE coalesced engine scan (subtree or full), feeding
        //    strictly through the engine's settle period and windowed deletion brake.
        if let Some(deadline) = debounce_deadline
            && now_inst >= deadline
        {
            debounce_deadline = None;
            if !paused && dirty.is_dirty() {
                let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                let scan_opts = ScanOptions {
                    require_second_look: true,
                    min_stable_age: Duration::ZERO,
                    ..ScanOptions::default()
                };
                let now_ts = now_xfer_timestamp();
                let mut newly_unstable = Vec::new();
                match dirty.take_coalesced() {
                    Some(CoalescedScan::Full) => {
                        if let Ok(rep) = engine.scan_with_options(now_ts, scan_opts) {
                            newly_unstable.extend(rep.held_back_unstable);
                        }
                    }
                    Some(CoalescedScan::Subtree(rel)) => {
                        if let Ok(rep) = engine.scan_subtree(&rel, now_ts, scan_opts) {
                            newly_unstable.extend(rep.held_back_unstable);
                        }
                    }
                    None => {}
                }
                if !newly_unstable.is_empty() {
                    unstable_paths.extend(newly_unstable);
                    unstable_paths.sort();
                    unstable_paths.dedup();
                    unstable_deadline = Some(Instant::now() + ctx.runner_cfg.stability_recheck);
                }
                let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                if seq_after > seq_before {
                    poke_peers(&ctx, None);
                }
                let _ = persist_status(&ctx, &engine);
            }
        }

        // 2. Stability second-look recheck for files that were still changing (coalesced into one scan).
        if let Some(deadline) = unstable_deadline
            && now_inst >= deadline
        {
            unstable_deadline = None;
            if !paused && !unstable_paths.is_empty() {
                let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                let pending = std::mem::take(&mut unstable_paths);
                let mut unstable_acc = DirtyAccumulator::default();
                unstable_acc.push_paths(pending);
                let scan_opts = ScanOptions {
                    require_second_look: true,
                    min_stable_age: Duration::ZERO,
                    ..ScanOptions::default()
                };
                let now_ts = now_xfer_timestamp();
                let mut still_unstable = Vec::new();
                match unstable_acc.take_coalesced() {
                    Some(CoalescedScan::Full) => {
                        if let Ok(rep) = engine.scan_with_options(now_ts, scan_opts) {
                            still_unstable.extend(rep.held_back_unstable);
                        }
                    }
                    Some(CoalescedScan::Subtree(rel)) => {
                        if let Ok(rep) = engine.scan_subtree(&rel, now_ts, scan_opts) {
                            still_unstable.extend(rep.held_back_unstable);
                        }
                    }
                    None => {}
                }
                if !still_unstable.is_empty() {
                    unstable_paths = still_unstable;
                    unstable_deadline = Some(Instant::now() + ctx.runner_cfg.stability_recheck);
                }
                let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                if seq_after > seq_before {
                    poke_peers(&ctx, None);
                }
                let _ = persist_status(&ctx, &engine);
            }
        }

        // 2b. Promote local deletions whose settle period has elapsed (never earlier than the engine).
        if !paused
            && let Some(sd) = settle_deadline
            && now_inst >= sd
        {
            let now_ts = now_xfer_timestamp();
            let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
            if let Ok(emitted) = engine.flush_settled_deletions(now_ts) {
                let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                if emitted > 0 || seq_after > seq_before {
                    poke_peers(&ctx, None);
                    let _ = persist_status(&ctx, &engine);
                }
            }
        }

        // 3. Periodic full scan + bin expiry (drives convergence even with watcher disabled).
        if now_inst >= next_full_scan {
            next_full_scan = now_inst + ctx.runner_cfg.scan_interval;
            if !paused {
                let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                let now_ts = now_xfer_timestamp();
                let scan_opts = ScanOptions {
                    require_second_look: ctx.runner_cfg.watch,
                    min_stable_age: Duration::ZERO,
                    ..ScanOptions::default()
                };
                if let Ok(rep) = engine.scan_with_options(now_ts, scan_opts)
                    && !rep.held_back_unstable.is_empty()
                {
                    unstable_paths.extend(rep.held_back_unstable);
                    unstable_paths.sort();
                    unstable_paths.dedup();
                    unstable_deadline = Some(Instant::now() + ctx.runner_cfg.stability_recheck);
                }
                let _ = engine.expire_bin(now_ts);
                let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                if seq_after > seq_before {
                    poke_peers(&ctx, None);
                }
                let _ = persist_status(&ctx, &engine);
            }
        }

        // 4. Periodic peer pull + backoff-ready peer retries (bounded per peer session).
        let any_backoff_due = peer_backoffs.values().any(|bo| now_inst >= bo.next_try);
        if now_inst >= next_pull || any_backoff_due {
            if now_inst >= next_pull {
                next_pull = now_inst + ctx.runner_cfg.pull_interval;
            }
            if !paused {
                let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                let pass = pull_all_peers(
                    &mut ctx,
                    &mut engine,
                    &mut peers_cache,
                    &mut peer_backoffs,
                    &mut peer_rr_offset,
                    false,
                );
                if pass.any_yielded {
                    // Resume remaining pages promptly after yielding to peers and draining commands.
                    next_pull = Instant::now() + Duration::from_millis(5);
                }
                let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                if seq_after > seq_before {
                    poke_peers(&ctx, None);
                }
                let _ = persist_status(&ctx, &engine);
            }
        }
    }

    close_all_peers(&mut peers_cache);
}

fn apply_folder_config_diff(
    ctx: &mut FolderWorkerCtx,
    engine: &mut SyncEngine<RealFs, RedbIndexStore>,
    new_entry: SyncFolderEntry,
    peers_cache: &mut HashMap<String, FleetSyncPeer>,
    peer_backoffs: &mut HashMap<String, PeerBackoff>,
) -> bool {
    let old_members: BTreeSet<String> = ctx.entry.members.keys().cloned().collect();
    let new_members: BTreeSet<String> = new_entry.members.keys().cloned().collect();
    let mut membership_added = false;
    for added in new_members.difference(&old_members) {
        if engine.add_member(added.clone()).is_ok() {
            membership_added = true;
        }
        peer_backoffs.remove(added);
    }
    for removed in old_members.difference(&new_members) {
        let _ = engine.remove_member(removed);
        peer_backoffs.remove(removed);
        if let Some(p) = peers_cache.remove(removed) {
            p.close();
        }
    }
    if new_entry.ignore != ctx.entry.ignore {
        let _ = engine.update_ignore(new_entry.ignore.clone());
    }
    engine.set_bin_retention_secs(u64::from(new_entry.bin_days).saturating_mul(86_400));
    if new_entry.paused != ctx.entry.paused {
        let _ = engine.set_paused(new_entry.paused);
    }
    ctx.entry = new_entry;
    membership_added
}

#[allow(clippy::too_many_arguments)]
fn handle_folder_cmd(
    ctx: &mut FolderWorkerCtx,
    engine: &mut SyncEngine<RealFs, RedbIndexStore>,
    cmd: FolderCmd,
    dirty: &mut DirtyAccumulator,
    debounce_deadline: &mut Option<Instant>,
    peers_cache: &mut HashMap<String, FleetSyncPeer>,
    peer_backoffs: &mut HashMap<String, PeerBackoff>,
    peer_rr_offset: &mut usize,
    next_pull: &mut Instant,
) -> bool {
    match cmd {
        FolderCmd::Shutdown => return true,
        FolderCmd::Watch(sig) => {
            if !engine.is_index_damaged() {
                dirty.push(sig);
                *debounce_deadline = Some(Instant::now() + ctx.runner_cfg.debounce);
            }
        }
        FolderCmd::UpdateConfig(new_entry) => {
            let new_entry = *new_entry;
            if engine.is_index_damaged() {
                ctx.entry = new_entry;
                let _ = persist_status(ctx, engine);
                return false;
            }
            let membership_added =
                apply_folder_config_diff(ctx, engine, new_entry, peers_cache, peer_backoffs);
            if membership_added {
                // Immediately retry locally resolvable unapplied records after membership changes.
                let _ = engine.retry_held_back_local(now_xfer_timestamp());
            }
            if !ctx.entry.paused && !engine.is_paused().unwrap_or(false) {
                let pass = pull_all_peers(
                    ctx,
                    engine,
                    peers_cache,
                    peer_backoffs,
                    peer_rr_offset,
                    true,
                );
                if pass.any_yielded {
                    *next_pull = Instant::now() + Duration::from_millis(5);
                }
            }
            let _ = persist_status(ctx, engine);
        }
        FolderCmd::SyncNow { reply } => {
            if engine.is_index_damaged() {
                let persisted = persist_status(ctx, engine);
                if let Some(tx) = reply {
                    let _ = tx.send(persisted);
                }
                return false;
            }
            peer_backoffs.clear();
            let paused = ctx.entry.paused || engine.is_paused().unwrap_or(false);
            if !paused {
                let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                let now_ts = now_xfer_timestamp();
                let _ = engine.flush_settled_deletions(now_ts);
                let _ = engine.retry_held_back_local(now_ts);
                let _ = engine.scan_with_options(
                    now_ts,
                    ScanOptions {
                        require_second_look: false,
                        min_stable_age: Duration::ZERO,
                        ..ScanOptions::default()
                    },
                );
                let pass = pull_all_peers(
                    ctx,
                    engine,
                    peers_cache,
                    peer_backoffs,
                    peer_rr_offset,
                    true,
                );
                if pass.any_yielded {
                    *next_pull = Instant::now() + Duration::from_millis(5);
                }
                let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                if seq_after > seq_before {
                    poke_peers(ctx, None);
                }
            }
            let persisted = persist_status(ctx, engine);
            if let Some(tx) = reply {
                let _ = tx.send(persisted);
            }
        }
        FolderCmd::Pause { reply } => {
            ctx.entry.paused = true;
            let res = engine
                .set_paused(true)
                .map_err(sync_error_to_body)
                .and_then(|()| persist_status(ctx, engine));
            let _ = reply.send(res);
        }
        FolderCmd::Resume { reply } => {
            ctx.entry.paused = false;
            peer_backoffs.clear();
            let res = engine
                .set_paused(false)
                .map_err(sync_error_to_body)
                .and_then(|()| {
                    let _ = engine.scan(now_xfer_timestamp());
                    let pass = pull_all_peers(
                        ctx,
                        engine,
                        peers_cache,
                        peer_backoffs,
                        peer_rr_offset,
                        true,
                    );
                    if pass.any_yielded {
                        *next_pull = Instant::now() + Duration::from_millis(5);
                    }
                    persist_status(ctx, engine)
                });
            let _ = reply.send(res);
        }
        FolderCmd::ResolveBrake { decision, reply } => {
            ctx.entry.paused = false;
            peer_backoffs.clear();
            let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
            let res = engine
                .resolve_deletion_brake(decision, now_xfer_timestamp())
                .map_err(sync_error_to_body)
                .and_then(|_| {
                    let pass = pull_all_peers(
                        ctx,
                        engine,
                        peers_cache,
                        peer_backoffs,
                        peer_rr_offset,
                        true,
                    );
                    if pass.any_yielded {
                        *next_pull = Instant::now() + Duration::from_millis(5);
                    }
                    let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                    if seq_after > seq_before {
                        poke_peers(ctx, None);
                    }
                    persist_status(ctx, engine)
                });
            let _ = reply.send(res);
        }
        FolderCmd::ResolveInboundBrake {
            peer,
            decision,
            reply,
        } => {
            peer_backoffs.remove(&peer);
            let seq_before = engine.meta().map(|m| m.local_seq).unwrap_or(0);
            let res = engine
                .resolve_inbound_deletion_brake(&peer, decision, now_xfer_timestamp())
                .map_err(sync_error_to_body)
                .and_then(|_| {
                    let pass = pull_all_peers(
                        ctx,
                        engine,
                        peers_cache,
                        peer_backoffs,
                        peer_rr_offset,
                        true,
                    );
                    if pass.any_yielded {
                        *next_pull = Instant::now() + Duration::from_millis(5);
                    }
                    let seq_after = engine.meta().map(|m| m.local_seq).unwrap_or(0);
                    if seq_after > seq_before {
                        poke_peers(ctx, None);
                    }
                    persist_status(ctx, engine)
                });
            let _ = reply.send(res);
        }
        FolderCmd::DismissProblem { kind, peer, reply } => {
            let res = engine
                .dismiss_problem(kind, peer.as_deref())
                .map_err(sync_error_to_body)
                .and_then(|count| persist_status(ctx, engine).map(|st| (count, st)));
            let _ = reply.send(res);
        }
        FolderCmd::ResolveConflict {
            conflict_path,
            choice,
            reply,
        } => {
            let res = engine
                .resolve_conflict(&conflict_path, choice, now_xfer_timestamp())
                .map_err(sync_error_to_body)
                .and_then(|()| {
                    poke_peers(ctx, None);
                    persist_status(ctx, engine)
                });
            let _ = reply.send(res);
        }
        FolderCmd::RestoreBin {
            entry_id,
            dest_path,
            strategy,
            reply,
        } => {
            let res = engine
                .restore_from_bin(&entry_id, dest_path.as_deref(), strategy, now_xfer_timestamp())
                .map_err(sync_error_to_body);
            if res.is_ok() {
                poke_peers(ctx, None);
                let _ = persist_status(ctx, engine);
            }
            let _ = reply.send(res);
        }
    }
    false
}

#[derive(Default)]
struct PullPassOutcome {
    any_yielded: bool,
}

enum PeerSyncTomlOutcome {
    Unchanged,
    LocalChanged,
    PeerUnreachable,
    Skipped,
}

fn sync_toml_with_peer_blocking(ctx: &FolderWorkerCtx, peer_id: &str) -> PeerSyncTomlOutcome {
    let paths = ctx.paths.clone();
    let fleet = ctx.fleet.clone();
    let config_store = Arc::clone(&ctx.config_store);
    let node_name = ctx.node_name.clone();
    let peer = peer_id.to_owned();
    let shutdown = ctx.shutdown.clone();
    ctx.runtime.block_on(async move {
        sync_toml_with_single_peer(&paths, &fleet, &config_store, &node_name, &peer, &shutdown)
            .await
    })
}

async fn sync_toml_with_single_peer(
    paths: &Paths,
    fleet: &Fleet,
    config_store: &ConfigStore,
    node_name: &str,
    peer: &str,
    shutdown: &CancellationToken,
) -> PeerSyncTomlOutcome {
    let session = match tokio::time::timeout(
        Duration::from_secs(3),
        fleet.session_for(config_store, node_name, peer, shutdown.clone()),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(err)) if err.code == ErrorCode::Unreachable || err.code == ErrorCode::Timeout => {
            return PeerSyncTomlOutcome::PeerUnreachable;
        }
        Ok(Err(_)) => return PeerSyncTomlOutcome::Skipped,
        Err(_) => return PeerSyncTomlOutcome::PeerUnreachable,
    };

    let read_params = CallParams {
        tool: "config.read".to_owned(),
        args: json!({ "document": "sync.toml" }),
        as_principal: None,
        on: None,
        hops: 1,
    };
    let val = match tokio::time::timeout(
        Duration::from_secs(5),
        session.call(read_params, shutdown.clone(), EventSink::discard()),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(err)) => {
            let code = err.into_error().code;
            if code == ErrorCode::Unreachable || code == ErrorCode::Timeout {
                return PeerSyncTomlOutcome::PeerUnreachable;
            }
            return PeerSyncTomlOutcome::Skipped;
        }
        Err(_) => return PeerSyncTomlOutcome::PeerUnreachable,
    };

    let remote_sync = match val.get("content").and_then(Value::as_str) {
        Some(text) => match parse::<SyncFile>(std::path::Path::new("sync.toml"), text.as_bytes()) {
            Ok(parsed) => parsed,
            Err(_) => return PeerSyncTomlOutcome::Skipped,
        },
        None => SyncFile::default(),
    };

    let paths_clone = paths.clone();
    let remote_for_merge = remote_sync.clone();
    let merged_res = tokio::task::spawn_blocking(move || {
        let local = load_sync_file(&paths_clone)?;
        let merged = legion_tools::sync::config::merge_sync_files(&local, &remote_for_merge);
        let local_changed = merged != local;
        let remote_changed = merged != remote_for_merge;
        if local_changed {
            apply_sync_merge_blocking(&paths_clone, &remote_for_merge, |_| Ok(()))?;
        }
        Ok::<_, ErrorBody>((merged, local_changed, remote_changed))
    })
    .await;

    let Ok(Ok((merged, local_changed, remote_changed))) = merged_res else {
        return PeerSyncTomlOutcome::Skipped;
    };
    if remote_changed
        && let Ok(toml_text) = merged.to_canonical_toml()
    {
        let write_params = CallParams {
            tool: "config.write".to_owned(),
            args: json!({
                "document": "sync.toml",
                "content": toml_text,
            }),
            as_principal: None,
            on: None,
            hops: 1,
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            session.call(write_params, shutdown.clone(), EventSink::discard()),
        )
        .await;
    }

    if local_changed {
        PeerSyncTomlOutcome::LocalChanged
    } else {
        PeerSyncTomlOutcome::Unchanged
    }
}

fn pull_all_peers(
    ctx: &mut FolderWorkerCtx,
    engine: &mut SyncEngine<RealFs, RedbIndexStore>,
    peers_cache: &mut HashMap<String, FleetSyncPeer>,
    peer_backoffs: &mut HashMap<String, PeerBackoff>,
    peer_rr_offset: &mut usize,
    force_all: bool,
) -> PullPassOutcome {
    let mut outcome = PullPassOutcome::default();
    let now_inst = Instant::now();
    let mut other_members: Vec<String> = ctx
        .entry
        .members
        .keys()
        .filter(|m| m.as_str() != ctx.node_name)
        .cloned()
        .collect();
    if other_members.is_empty() {
        return outcome;
    }

    // Round-robin rotation so bounded pull sessions share time fairly across every peer.
    let len = other_members.len();
    let start = *peer_rr_offset % len;
    other_members.rotate_left(start);
    *peer_rr_offset = (*peer_rr_offset + 1) % len;

    let mut membership_expanded = false;
    let mut visited_peers: Vec<String> = Vec::new();

    for peer_id in other_members {
        if ctx.cancel_flag.load(Ordering::Relaxed) || engine.is_index_damaged() {
            break;
        }
        if !ctx.entry.members.contains_key(&peer_id) {
            continue;
        }
        if !force_all
            && let Some(bo) = peer_backoffs.get(&peer_id)
            && now_inst < bo.next_try
        {
            continue;
        }

        // Membership before records: exchange `sync.toml` with `peer_id` FIRST so we learn any
        // newly added members (and share ours) before pulling records that could mention them.
        match sync_toml_with_peer_blocking(ctx, &peer_id) {
            PeerSyncTomlOutcome::LocalChanged => {
                let _ = ctx.reload_tx.send(());
                if let Ok(sf) = load_sync_file(&ctx.paths) {
                    match sf.folders.get(&ctx.folder_id) {
                        Some(updated_entry)
                            if updated_entry.members.contains_key(&ctx.node_name) =>
                        {
                            let added = apply_folder_config_diff(
                                ctx,
                                engine,
                                updated_entry.clone(),
                                peers_cache,
                                peer_backoffs,
                            );
                            if added {
                                membership_expanded = true;
                                let _ = engine.retry_held_back_local(now_xfer_timestamp());
                            }
                            if let Ok(matcher) = IgnoreMatcher::compile(&updated_entry.ignore) {
                                let mut ig = ctx
                                    .read_handle
                                    .ignore
                                    .write()
                                    .unwrap_or_else(|p| p.into_inner());
                                *ig = matcher;
                            }
                            if !updated_entry.members.contains_key(&peer_id) {
                                continue;
                            }
                            if updated_entry.paused || engine.is_paused().unwrap_or(false) {
                                break;
                            }
                        }
                        _ => {
                            // Folder deleted or this machine removed in merged sync.toml.
                            break;
                        }
                    }
                }
            }
            PeerSyncTomlOutcome::PeerUnreachable => {
                engine.mark_peer_unreachable(peer_id.clone());
                if let Some(p) = peers_cache.get(&peer_id) {
                    p.close();
                }
                record_peer_backoff(ctx, peer_backoffs, peer_id, now_inst, 8);
                continue;
            }
            PeerSyncTomlOutcome::Unchanged | PeerSyncTomlOutcome::Skipped => {}
        }

        let peer = peers_cache.entry(peer_id.clone()).or_insert_with(|| {
            FleetSyncPeer::new(
                ctx.fleet.clone(),
                ctx.config_store.clone(),
                ctx.node_name.clone(),
                peer_id.clone(),
                ctx.folder_id.clone(),
                ctx.runtime.clone(),
                ctx.shutdown.clone(),
            )
        });

        let fetch_opts = FetchOptions {
            offset: 0,
            timeout: legion_sync::DEFAULT_FETCH_TIMEOUT,
            cancel: Some(Arc::clone(&ctx.cancel_flag)),
        };

        let result = engine.pull_from_peer_bounded(
            peer,
            DEFAULT_PAGE_LIMIT,
            &fetch_opts,
            ctx.runner_cfg.pull_max_pages,
            ctx.runner_cfg.pull_max_duration,
            now_xfer_timestamp(),
        );

        visited_peers.push(peer_id.clone());

        match result {
            Ok(report) => {
                engine.mark_peer_reachable(&peer_id);
                peer_backoffs.remove(&peer_id);
                if report.yielded {
                    outcome.any_yielded = true;
                }
                // Close idle stream child after pass finishes so idle runner holds zero subprocesses.
                peer.close();
            }
            Err(SyncError::PeerUnavailable { .. } | SyncError::Timeout { .. }) => {
                engine.mark_peer_unreachable(peer_id.clone());
                peer.close();
                record_peer_backoff(ctx, peer_backoffs, peer_id, now_inst, 8);
            }
            Err(SyncError::Cancelled { .. }) => {
                peer.close();
                break;
            }
            Err(_) => {
                peer.close();
                // Back off non-fatal per-peer problems briefly so we don't spin tightly.
                record_peer_backoff(ctx, peer_backoffs, peer_id, now_inst, 5);
            }
        }
    }

    // If we learned about a new member mid-loop AND an earlier peer still has held-back records
    // (for example records authored by that newly learned member), retry them immediately.
    if membership_expanded && !ctx.cancel_flag.load(Ordering::Relaxed) && !engine.is_index_damaged()
    {
        let held_peers: BTreeSet<String> = engine
            .list_held_back()
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.from_peer)
            .collect();
        for peer_id in visited_peers {
            if !held_peers.contains(&peer_id) || !ctx.entry.members.contains_key(&peer_id) {
                continue;
            }
            if let Some(peer) = peers_cache.get(&peer_id) {
                let fetch_opts = FetchOptions {
                    offset: 0,
                    timeout: legion_sync::DEFAULT_FETCH_TIMEOUT,
                    cancel: Some(Arc::clone(&ctx.cancel_flag)),
                };
                if let Ok(rep) = engine.pull_from_peer_bounded(
                    peer,
                    DEFAULT_PAGE_LIMIT,
                    &fetch_opts,
                    ctx.runner_cfg.pull_max_pages,
                    ctx.runner_cfg.pull_max_duration,
                    now_xfer_timestamp(),
                ) && rep.yielded
                {
                    outcome.any_yielded = true;
                }
                peer.close();
            }
        }
    }

    outcome
}

fn record_peer_backoff(
    ctx: &FolderWorkerCtx,
    peer_backoffs: &mut HashMap<String, PeerBackoff>,
    peer_id: String,
    now_inst: Instant,
    max_shift: u32,
) {
    let entry = peer_backoffs.entry(peer_id).or_insert(PeerBackoff {
        failures: 0,
        next_try: now_inst,
    });
    entry.failures = entry.failures.saturating_add(1);
    let shift = entry.failures.saturating_sub(1).min(max_shift);
    let delay = ctx
        .runner_cfg
        .backoff_base
        .saturating_mul(1u32 << shift)
        .min(ctx.runner_cfg.backoff_max);
    entry.next_try = Instant::now() + delay;
}

fn close_all_peers(peers_cache: &mut HashMap<String, FleetSyncPeer>) {
    for (_, peer) in peers_cache.drain() {
        peer.close();
    }
}

fn poke_peers(ctx: &FolderWorkerCtx, skip_peer: Option<&str>) {
    let peers: Vec<String> = ctx
        .entry
        .members
        .keys()
        .filter(|m| m.as_str() != ctx.node_name && Some(m.as_str()) != skip_peer)
        .cloned()
        .collect();
    if !peers.is_empty() {
        let _ = ctx.poke_tx.send(PokeEvent {
            folder_id: ctx.folder_id.clone(),
            peers,
        });
    }
}

fn persist_status(
    ctx: &FolderWorkerCtx,
    engine: &SyncEngine<RealFs, RedbIndexStore>,
) -> Result<PersistedFolderStatus, ErrorBody> {
    let damaged = engine.is_index_damaged();
    ctx.read_handle.damaged.store(damaged, Ordering::Release);
    ctx.read_handle
        .healthy
        .store(!engine.has_folder_problem(), Ordering::Release);
    let status = engine.status().map_err(sync_error_to_body)?;
    let held_back = collect_held_back_summaries(engine, &status);
    let persisted = PersistedFolderStatus {
        folder_id: ctx.folder_id.clone(),
        folder_name: ctx.entry.name.clone(),
        local_path: ctx.local_root.display().to_string(),
        paused_by_config: ctx.entry.paused,
        index_damaged: damaged,
        held_back,
        updated_at: format_stamp_time(Timestamp::now()),
        status,
    };
    write_folder_status(&ctx.paths, &persisted)?;
    Ok(persisted)
}

fn write_damaged_index_status(
    paths: &Paths,
    folder_id: &str,
    entry: &SyncFolderEntry,
    here: &str,
    root: &std::path::Path,
) -> PersistedFolderStatus {
    let prob = SyncProblem {
        path: String::new(),
        kind: ProblemKind::IndexDamaged,
        peer: None,
        sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_owned(),
    };
    let state = MemberState::Problem {
        sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_owned(),
        problems: vec![prob],
    };
    let members = entry
        .members
        .keys()
        .map(|m| MemberStatus {
            member: m.clone(),
            state: state.clone(),
            cursor_seq: 0,
        })
        .collect();
    let persisted = PersistedFolderStatus {
        folder_id: folder_id.to_owned(),
        folder_name: entry.name.clone(),
        local_path: root.display().to_string(),
        paused_by_config: entry.paused,
        index_damaged: true,
        held_back: Vec::new(),
        updated_at: format_stamp_time(Timestamp::now()),
        status: FolderStatus {
            folder_id: folder_id.to_owned(),
            local_member: here.to_owned(),
            state,
            members,
            conflicts: Vec::new(),
            bin: Vec::new(),
            pending_brake: None,
            inbound_brakes: Vec::new(),
            deferred: Vec::new(),
        },
    };
    let _ = write_folder_status(paths, &persisted);
    persisted
}

fn write_open_problem_status(
    paths: &Paths,
    folder_id: &str,
    entry: &SyncFolderEntry,
    here: &str,
    root: &std::path::Path,
    sentence: &str,
) {
    let prob = SyncProblem {
        path: String::new(),
        kind: ProblemKind::FilesystemError,
        peer: None,
        sentence: sentence.to_owned(),
    };
    let state = MemberState::Problem {
        sentence: sentence.to_owned(),
        problems: vec![prob],
    };
    let members = entry
        .members
        .keys()
        .map(|m| MemberStatus {
            member: m.clone(),
            state: state.clone(),
            cursor_seq: 0,
        })
        .collect();
    let persisted = PersistedFolderStatus {
        folder_id: folder_id.to_owned(),
        folder_name: entry.name.clone(),
        local_path: root.display().to_string(),
        paused_by_config: entry.paused,
        index_damaged: false,
        held_back: Vec::new(),
        updated_at: format_stamp_time(Timestamp::now()),
        status: FolderStatus {
            folder_id: folder_id.to_owned(),
            local_member: here.to_owned(),
            state,
            members,
            conflicts: Vec::new(),
            bin: Vec::new(),
            pending_brake: None,
            inbound_brakes: Vec::new(),
            deferred: Vec::new(),
        },
    };
    let _ = write_folder_status(paths, &persisted);
}

fn not_active_error(folder_id: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("Sync folder {folder_id:?} is not active on this machine."),
    )
    .with_hint("Run `legion sync.list` to see configured sync folders.")
}

fn worker_stopped_error(folder_id: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!("Sync worker for folder {folder_id:?} stopped unexpectedly."),
    )
}

/// Runs the long-lived sync runner until `shutdown` fires.
pub async fn run(
    paths: Paths,
    fleet: Fleet,
    runner_cfg: RunnerConfig,
    shutdown: CancellationToken,
) -> Result<(), ErrorBody> {
    ensure_owner_dir(&sync_state_dir(&paths)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Sync state directory cannot be created: {err}."),
        )
    })?;

    // 1. Enforce single runner instance per Legion home via exclusive kernel file lock.
    let lock_path = runner_lock_path(&paths);
    let _runner_lock = match FileLock::try_acquire(&lock_path, LockMode::Exclusive) {
        Ok(lock) => lock,
        Err(OsError::WouldBlock { .. }) => {
            return Err(ErrorBody::new(
                ErrorCode::Busy,
                "Another `legion sync run` process is already running for this Legion home.",
            )
            .with_hint("Stop the existing runner before starting another."));
        }
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not lock {}: {err}.", lock_path.display()),
            ));
        }
    };

    let config_store = Arc::new(ConfigStore::new(paths.config_dir()));
    let node_name = config_store
        .get::<NodeFile>()
        .map(|f| f.name_or_host().to_string())
        .unwrap_or_else(|_| legion_core::config::NodeName::from_host().to_string());

    // 2. Write owner-only auth token and PID info.
    let auth_token = legion_tools::sync::control::generate_control_token()?;
    write_owner_file(&control_token_path(&paths), auth_token.as_bytes())?;
    let pid_info = RunnerPidInfo {
        pid: std::process::id(),
        started_at: format_stamp_time(Timestamp::now()),
        node: node_name.clone(),
    };
    if let Ok(encoded) = serde_json::to_vec_pretty(&pid_info) {
        let _ = write_owner_file(&runner_pid_path(&paths), &encoded);
    }

    // 3. Spawn background peer wake-up poker (keeps save-to-arrival latency sub-second with zero
    //    idle polling).
    let (poke_tx, mut poke_rx) = tokio::sync::mpsc::unbounded_channel::<PokeEvent>();
    let poke_fleet = fleet.clone();
    let poke_config = config_store.clone();
    let poke_here = node_name.clone();
    let poke_shutdown = shutdown.clone();
    let poke_task = tokio::spawn(async move {
        let mut last_poke: HashMap<(String, String), Instant> = HashMap::new();
        loop {
            tokio::select! {
                _ = poke_shutdown.cancelled() => break,
                maybe = poke_rx.recv() => {
                    let Some(ev) = maybe else { break };
                    for peer in ev.peers {
                        let key = (ev.folder_id.clone(), peer.clone());
                        let now = Instant::now();
                        if last_poke.get(&key).is_some_and(|t| now.duration_since(*t) < Duration::from_millis(50)) {
                            continue;
                        }
                        last_poke.insert(key, now);
                        let f = poke_fleet.clone();
                        let c = poke_config.clone();
                        let h = poke_here.clone();
                        let fid = ev.folder_id.clone();
                        let s = poke_shutdown.clone();
                        tokio::spawn(async move {
                            let _ = tokio::time::timeout(
                                Duration::from_secs(3),
                                poke_one_peer(&f, &c, &h, &peer, &fid, s),
                            )
                            .await;
                        });
                    }
                }
            }
        }
    });

    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let state = Arc::new(RunnerState {
        paths: paths.clone(),
        node_name: node_name.clone(),
        fleet: fleet.clone(),
        config_store: config_store.clone(),
        runner_cfg: runner_cfg.clone(),
        runtime: Handle::current(),
        shutdown: shutdown.clone(),
        poke_tx,
        reload_tx,
        folders: Mutex::new(BTreeMap::new()),
    });

    // 4. Bind owner-only local control socket/pipe BEFORE opening folders so `legion stream`
    //    requests immediately see the runner once ready.
    let control_guard = bind_control_endpoint(&paths)?;

    // 5. Open all currently configured local folders and reconcile immediately on start.
    state.reload_folders()?;

    // 6. Spawn periodic `sync.toml` fleet exchange loop (carries `sync.toml` along across
    //    reachable peers, plus reloads local folders whenever `sync.toml` changes on disk or
    //    when a folder worker learns new membership from a peer).
    let cfg_state = Arc::clone(&state);
    let cfg_interval = runner_cfg.config_sync_interval;
    let cfg_shutdown = shutdown.clone();
    let cfg_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // First tick fires immediately; skip immediate redundant tick because startup already ran.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cfg_shutdown.cancelled() => break,
                Some(()) = reload_rx.recv() => {
                    while reload_rx.try_recv().is_ok() {}
                    let _ = cfg_state.reload_folders();
                }
                _ = ticker.tick() => {
                    sync_toml_with_fleet(&cfg_state).await;
                    let _ = cfg_state.reload_folders();
                }
            }
        }
    });

    // 7. Serve incoming local control connections until `shutdown` fires.
    serve_control_endpoint(
        control_guard.listener,
        Arc::clone(&state),
        auth_token,
        shutdown.clone(),
    )
    .await;

    // 8. Clean shutdown: stop background tasks, cancel & join every folder worker, remove control
    //    files, and close fleet sessions.
    shutdown.cancel();
    cfg_task.abort();
    poke_task.abort();
    let state_for_join = Arc::clone(&state);
    let _ = tokio::task::spawn_blocking(move || state_for_join.shutdown_all()).await;
    cleanup_control_files(&paths, control_guard.cleanup_path);
    fleet.close().await;
    Ok(())
}

async fn poke_one_peer(
    fleet: &Fleet,
    config: &ConfigStore,
    here: &str,
    peer: &str,
    folder_id: &str,
    cancel: CancellationToken,
) {
    let Ok(session) = fleet.session_for(config, here, peer, cancel.clone()).await else {
        return;
    };
    let params = CallParams {
        tool: "sync.now".to_owned(),
        args: json!({ "id": folder_id, "wait": false }),
        as_principal: None,
        on: None,
        hops: 1,
    };
    let _ = session.call(params, cancel, EventSink::discard()).await;
}

async fn sync_toml_with_fleet(state: &RunnerState) {
    let Ok(fleet_file) = state.config_store.get::<FleetFile>() else {
        return;
    };
    let peers: Vec<String> = fleet_file
        .machines
        .keys()
        .filter(|m| m.as_str() != state.node_name)
        .cloned()
        .collect();
    if peers.is_empty() {
        return;
    }

    for peer in peers {
        if state.shutdown.is_cancelled() {
            break;
        }
        let Ok(session) = tokio::time::timeout(
            Duration::from_secs(5),
            state.fleet.session_for(
                &state.config_store,
                &state.node_name,
                &peer,
                state.shutdown.clone(),
            ),
        )
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(()) else {
            continue;
        };

        let read_params = CallParams {
            tool: "config.read".to_owned(),
            args: json!({ "document": "sync.toml" }),
            as_principal: None,
            on: None,
            hops: 1,
        };
        let Ok(Ok(val)) = tokio::time::timeout(
            Duration::from_secs(5),
            session.call(read_params, state.shutdown.clone(), EventSink::discard()),
        )
        .await else {
            continue;
        };

        let remote_sync = match val.get("content").and_then(Value::as_str) {
            Some(text) => match parse::<SyncFile>(std::path::Path::new("sync.toml"), text.as_bytes()) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            },
            None => SyncFile::default(),
        };

        let paths = state.paths.clone();
        let remote_for_merge = remote_sync.clone();
        let merged_res = tokio::task::spawn_blocking(move || {
            let local = load_sync_file(&paths)?;
            let merged = legion_tools::sync::config::merge_sync_files(&local, &remote_for_merge);
            let local_changed = merged != local;
            let remote_changed = merged != remote_for_merge;
            if local_changed {
                apply_sync_merge_blocking(&paths, &remote_for_merge, |_| Ok(()))?;
            }
            Ok::<_, ErrorBody>((merged, local_changed, remote_changed))
        })
        .await;

        let Ok(Ok((merged, _local_changed, remote_changed))) = merged_res else {
            continue;
        };
        if remote_changed
            && let Ok(toml_text) = merged.to_canonical_toml()
        {
            let write_params = CallParams {
                tool: "config.write".to_owned(),
                args: json!({
                    "document": "sync.toml",
                    "content": toml_text,
                }),
                as_principal: None,
                on: None,
                hops: 1,
            };
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                session.call(write_params, state.shutdown.clone(), EventSink::discard()),
            )
            .await;
        }
    }
}

// ============================================================================
// Platform-specific owner-only local control listener
// ============================================================================

#[cfg(unix)]
struct ControlEndpointGuard {
    listener: tokio::net::UnixListener,
    cleanup_path: Option<PathBuf>,
}

#[cfg(unix)]
fn bind_control_endpoint(paths: &Paths) -> Result<ControlEndpointGuard, ErrorBody> {
    use legion_tools::sync::control::MAX_DIRECT_UNIX_SOCK_LEN;
    use std::os::unix::fs::PermissionsExt;

    let direct = control_sock_path(paths);
    let (sock_path, cleanup_path) = if direct.as_os_str().len() <= MAX_DIRECT_UNIX_SOCK_LEN {
        let _ = std::fs::remove_file(control_sock_ref_path(paths));
        (direct, None)
    } else {
        let hash_hex = legion_xfer::hash::hash_bytes(
            paths.state_dir().to_string_lossy().as_bytes(),
        )
        .to_string();
        let hex_part = hash_hex.strip_prefix("blake3:").unwrap_or(&hash_hex);
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let short_dir = base.join(format!("lgs-{}", &hex_part[..16]));
        ensure_owner_dir(&short_dir).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not create {}: {err}.", short_dir.display()),
            )
        })?;
        let sp = short_dir.join("c.sock");
        write_owner_file(
            &control_sock_ref_path(paths),
            sp.to_string_lossy().as_bytes(),
        )?;
        (sp, Some(short_dir))
    };

    if let Some(parent) = sock_path.parent() {
        ensure_owner_dir(parent).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not create {}: {err}.", parent.display()),
            )
        })?;
    }
    let _ = std::fs::remove_file(&sock_path);
    let listener = tokio::net::UnixListener::bind(&sock_path).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not bind control socket {}: {err}.", sock_path.display()),
        )
    })?;
    let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
    Ok(ControlEndpointGuard {
        listener,
        cleanup_path,
    })
}

#[cfg(unix)]
async fn serve_control_endpoint(
    listener: tokio::net::UnixListener,
    state: Arc<RunnerState>,
    auth_token: String,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let st = Arc::clone(&state);
                let tok = auth_token.clone();
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) if n > MAX_CONTROL_LINE_BYTES => {
                                let resp = ControlResponse::Error {
                                    error: ErrorBody::new(
                                        ErrorCode::InvalidArgs,
                                        "Control request exceeded maximum length.",
                                    ),
                                };
                                let _ = write_control_response(&mut write_half, &resp).await;
                                break;
                            }
                            Ok(_) => {}
                        }
                        let resp = dispatch_control_line(&st, &tok, &line).await;
                        if write_control_response(&mut write_half, &resp).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }
}

#[cfg(windows)]
struct ControlEndpointGuard {
    listener: String,
    cleanup_path: Option<PathBuf>,
}

#[cfg(windows)]
fn bind_control_endpoint(paths: &Paths) -> Result<ControlEndpointGuard, ErrorBody> {
    let pipe_name = legion_tools::sync::control::windows_pipe_name(paths);
    Ok(ControlEndpointGuard {
        listener: pipe_name,
        cleanup_path: None,
    })
}

#[cfg(windows)]
async fn serve_control_endpoint(
    pipe_name: String,
    state: Arc<RunnerState>,
    auth_token: String,
    shutdown: CancellationToken,
) {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut first = true;
    loop {
        let mut opts = ServerOptions::new();
        opts.first_pipe_instance(first);
        first = false;
        let Ok(server) = opts.create(&pipe_name) else {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => continue,
            }
        };
        tokio::select! {
            _ = shutdown.cancelled() => break,
            res = server.connect() => {
                if res.is_err() {
                    continue;
                }
                let st = Arc::clone(&state);
                let tok = auth_token.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(server);
                    let mut line = String::new();
                    if let Ok(n) = reader.read_line(&mut line).await && n > 0 && n <= MAX_CONTROL_LINE_BYTES {
                        let resp = dispatch_control_line(&st, &tok, &line).await;
                        let mut server = reader.into_inner();
                        let _ = write_control_response(&mut server, &resp).await;
                    }
                });
            }
        }
    }
}

async fn dispatch_control_line(
    state: &RunnerState,
    expected_token: &str,
    line: &str,
) -> ControlResponse {
    let envelope: ControlEnvelope = match serde_json::from_str(line) {
        Ok(env) => env,
        Err(err) => {
            return ControlResponse::Error {
                error: ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Control request is not valid JSON: {err}."),
                ),
            };
        }
    };
    if !constant_time_eq(envelope.token.as_bytes(), expected_token.as_bytes()) {
        return ControlResponse::Error {
            error: ErrorBody::new(
                ErrorCode::Denied,
                "Invalid sync runner control authentication token.",
            ),
        };
    }
    state.handle_control_request(envelope.request).await
}

async fn write_control_response<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    resp: &ControlResponse,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(resp).map_err(io::Error::other)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn cleanup_control_files(paths: &Paths, cleanup_dir: Option<PathBuf>) {
    let _ = std::fs::remove_file(control_sock_path(paths));
    let _ = std::fs::remove_file(control_sock_ref_path(paths));
    let _ = std::fs::remove_file(control_token_path(paths));
    let _ = std::fs::remove_file(runner_pid_path(paths));
    if let Some(dir) = cleanup_dir {
        let _ = std::fs::remove_dir_all(dir);
    }
}
