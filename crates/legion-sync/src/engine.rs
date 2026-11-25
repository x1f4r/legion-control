//! High-level hubless folder synchronization engine orchestrating folder identity verification,
//! incarnation management, scanning, untrusted-peer reconciliation, crash-safe batched apply,
//! direct-acknowledgement tombstone garbage collection, bin management, conflict resolution, and
//! status reporting.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::apply::{
    ApplyOutcome, RecoveryReport, StoreIndexView, apply_decision, apply_local_rename,
    apply_new_files_group_commit, expire_bin, inspect_and_promote_modified_bin_entries,
    recover_pending, resolve_conflict, restore_from_bin,
};
use crate::error::{INDEX_DAMAGED_PROBLEM_SENTENCE, Result, SyncError};
use crate::model::{
    BinEntry, ConflictChoice, ConflictInfo, DeferredItem, DeletionBrakeConfig,
    DeletionBrakeDecision, FORCED_ROTATION_COOLDOWN_SECS, FetchOptions, FolderConfig,
    FolderDeletionLogEntry, FolderDeletionRecord, FolderMarker, FolderMeta, FolderStatus,
    HandshakeHello, HeldBackRecord, INDEX_SCHEMA_VERSION, InboundDeletionBrake,
    InboundDeletionBrakeDecision, IndexRecord, MAX_DEFERRAL_ATTEMPTS,
    MAX_FORCED_ROTATIONS_BEFORE_CONFIRM, MAX_HELD_BACK_ENTRIES, MAX_PAGE_RECORDS,
    MAX_PEER_PROBLEMS, MemberId, MemberState, MemberStatus, PeerAckView, PeerCursor, PeerHandshake,
    PeerRefusalSummary, PeerRollbackClaimDecision, ProblemKind, RecordsPage, RestoreStrategy,
    SYNC_PROTOCOL_VERSION, SyncProblem, SyncProgress, collapse_deletion_records,
    compact_deletion_log_bytes, encode_deletion_log_entries, merge_deletion_record_into_list,
    parse_deletion_log_all, random_incarnation_id, validate_member_id,
};
use crate::path::{
    IgnoreMatcher, is_descendant_of, is_internal_sync_path, path_depth, validate_sync_path,
};
use crate::reconcile::{LocalIndexView, ReconcileDecision, reconcile_record};
use crate::scan::{
    PendingStabilityTracker, ScanOptions, ScanReport, disk_stat_matches_index,
    flush_settled_deletions as scan_flush_settled_deletions, scan_folder, scan_subtree,
};
use crate::store_redb::RedbIndexStore;
use crate::traits::{IndexStore, Peer, StoreBatch, SyncFs};

/// Default number of records fetched per incremental page during [`SyncEngine::pull_from_peer`].
pub const DEFAULT_PAGE_LIMIT: usize = 1024;

/// Maximum consecutive retry attempts on a single path if local edits race with apply.
const MAX_LOCAL_CHANGE_RETRIES: usize = 4;

/// Maximum number of distinct prior incarnations remembered per peer for clone detection.
const MAX_PRIOR_INCARNATIONS: usize = 32;

/// Plain-language message shown when a folder's identity marker is missing or mismatched.
pub const FOLDER_MARKER_PROBLEM_SENTENCE: &str =
    "This folder looks empty or replaced. Is a drive not connected?";

/// Summary of one pull-only sync pass against a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullReport {
    /// Total peer records inspected across all pages.
    pub records_seen: usize,
    /// Records applied (fetched, deleted, renamed, merged, or conflicted).
    pub applied: usize,
    /// Records kept as-is (local already equal or newer).
    pub kept: usize,
    /// Records ignored by fixed or per-folder ignore rules.
    pub ignored: usize,
    /// Records held back as platform problems, unknown members, or unreadable subtrees.
    pub held_back: usize,
    /// Records deferred (with bounded retry counts).
    pub deferred: usize,
    /// Malformed or unauthorized records from this peer that were refused (not acknowledged) and
    /// reported durably.
    pub invalid_skipped: usize,
    /// Local rename optimizations performed without downloading file bytes.
    pub local_renames: usize,
    /// Previously held-back records successfully re-evaluated and applied.
    pub held_back_resolved: usize,
}

/// Core synchronization engine for one member's copy of a sync folder.
pub struct SyncEngine<F: SyncFs, S: IndexStore> {
    config: FolderConfig,
    fs: F,
    store: S,
    ignore: IgnoreMatcher,
    bin_retention_secs: u64,
    deletion_brake: DeletionBrakeConfig,
    stability: PendingStabilityTracker,
    unreachable_peers: BTreeSet<MemberId>,
    active_progress: BTreeMap<MemberId, SyncProgress>,
    last_pull_errors: BTreeMap<MemberId, SyncProblem>,
    peer_refusals_from_serve: Mutex<BTreeMap<MemberId, PeerRefusalSummary>>,
    serve_rescan_hints: Mutex<BTreeSet<String>>,
    skipped_paths: BTreeSet<String>,
    folder_problem: Option<SyncProblem>,
    index_damaged: bool,
    last_now: Timestamp,
}

fn index_damaged_problem() -> SyncProblem {
    SyncProblem {
        path: String::new(),
        kind: ProblemKind::IndexDamaged,
        peer: None,
        sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_string(),
    }
}

fn engine_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "index store panicked".to_string()
    }
}

impl<F: SyncFs> SyncEngine<F, RedbIndexStore> {
    /// Explicit recovery action for a damaged or missing `redb` index file: removes any corrupt
    /// database file at `db_path`, creates a fresh index, scans live files on disk under the join
    /// rule, restores bin metadata sidecars from `.legion-sync/bin`, and replays
    /// `.legion-sync/deletions.log` so previously deleted files are never resurrected.
    pub fn rebuild_redb_from_disk(
        config: FolderConfig,
        fs: F,
        db_path: impl AsRef<Path>,
        now: Timestamp,
    ) -> Result<(Self, ScanReport)> {
        config.validate()?;
        fs.check_not_nested()?;
        if let Some(m) = fs.read_marker()?
            && (m.folder_id != config.id || m.member_id != config.local_member)
        {
            return Err(SyncError::FolderUnavailable {
                folder_id: config.id,
                sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
            });
        }

        let db_path = db_path.as_ref();
        match std::fs::remove_file(db_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(SyncError::from_io(Some(db_path.display().to_string()), err));
            }
        }

        let store = RedbIndexStore::open(db_path)?;
        let mut engine = Self::open(config, fs, store)?;
        let report = engine.rebuild_from_disk(now)?;
        Ok((engine, report))
    }
}

impl<F: SyncFs, S: IndexStore> SyncEngine<F, S> {
    /// Opens or initializes a sync engine for `config`, checking schema version, nesting rules,
    /// index integrity, and folder identity marker, running crash recovery, and retrying any
    /// locally resolvable unapplied records.
    ///
    /// If the index store is empty beside an existing folder marker and a non-empty folder (or
    /// active deletion log), or if the index store panics/reports corruption during open, the
    /// engine enters a stopped damaged state (`ProblemKind::IndexDamaged`) without changing
    /// anything on disk until [`Self::rebuild_from_disk`] is invoked explicitly.
    pub fn open(config: FolderConfig, fs: F, mut store: S) -> Result<Self> {
        config.validate()?;
        fs.check_not_nested()?;
        let ignore = IgnoreMatcher::compile(&config.ignore)?;

        let init_attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || -> Result<_> {
                let found_schema = store.schema_version()?;
                if found_schema > INDEX_SCHEMA_VERSION {
                    return Err(SyncError::SchemaTooNew {
                        found: found_schema,
                        supported: INDEX_SCHEMA_VERSION,
                    });
                }
                if found_schema < INDEX_SCHEMA_VERSION {
                    return Err(SyncError::IndexDamaged {
                        folder_id: config.id.clone(),
                        detail: format!(
                            "index schema version {found_schema} is older than supported version {INDEX_SCHEMA_VERSION}"
                        ),
                        sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_string(),
                    });
                }

                let mut folder_problem = None;
                let mut needs_recover = true;
                let mut index_damaged = false;

                match store.load_meta()? {
                    None => {
                        let existing_marker = fs.read_marker()?;
                        if let Some(ref m) = existing_marker
                            && (m.folder_id != config.id || m.member_id != config.local_member)
                        {
                            return Err(SyncError::FolderUnavailable {
                                folder_id: config.id.clone(),
                                sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                            });
                        }

                        if existing_marker.is_some() {
                            // Item 7: a missing index beside an existing folder marker and a non-empty
                            // folder (or active deletion log) is damaged, NOT brand new! Stop with a
                            // clear problem and change nothing on disk until rebuilt explicitly.
                            let has_live_entries =
                                !fs.walk(&IgnoreMatcher::default())?.entries.is_empty();
                            let has_del_log =
                                !parse_deletion_log_all(&fs.read_deletion_log()?).is_empty();
                            if has_live_entries || has_del_log {
                                index_damaged = true;
                                folder_problem = Some(index_damaged_problem());
                                needs_recover = false;
                            }
                        }

                        if !index_damaged {
                            let avoid_inc = existing_marker.as_ref().map(|m| m.incarnation);
                            let inc =
                                choose_fresh_incarnation(config.initial_incarnation, avoid_inc)?;
                            fs.ensure_internal_dirs()?;
                            let marker = FolderMarker::new(
                                config.id.clone(),
                                config.local_member.clone(),
                                inc,
                            );
                            fs.write_marker(&marker)?;

                            let meta = FolderMeta::new(&config, inc);
                            let batch = StoreBatch {
                                folder_meta: Some(meta),
                                ..StoreBatch::default()
                            };
                            store.apply_batch(batch)?;
                        }
                    }
                    Some(existing) => {
                        if existing.folder_id != config.id {
                            return Err(SyncError::InvalidConfig {
                                message: format!(
                                    "index store belongs to folder {:?}, not {:?}",
                                    existing.folder_id, config.id
                                ),
                                hint: Some(
                                    "Open the index database that matches this sync folder id."
                                        .to_string(),
                                ),
                            });
                        }
                        if existing.local_member != config.local_member {
                            return Err(SyncError::InvalidConfig {
                                message: format!(
                                    "index store belongs to member {:?}, not {:?}",
                                    existing.local_member, config.local_member
                                ),
                                hint: Some(
                                    "Each member must keep its own index store.".to_string(),
                                ),
                            });
                        }

                        match fs.read_marker()? {
                            None => {
                                if store.record_count()? > 0 {
                                    // Item B2: NEVER silently recreate `.legion-sync` when an index
                                    // already exists; stop with an actionable problem instead!
                                    folder_problem = Some(SyncProblem {
                                        path: String::new(),
                                        kind: ProblemKind::FolderMarkerMissing,
                                        peer: None,
                                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                                    });
                                    needs_recover = false;
                                } else {
                                    fs.ensure_internal_dirs()?;
                                    let marker = FolderMarker::new(
                                        existing.folder_id.clone(),
                                        existing.local_member.clone(),
                                        existing.incarnation,
                                    );
                                    fs.write_marker(&marker)?;
                                }
                            }
                            Some(marker) => {
                                if marker.folder_id != existing.folder_id
                                    || marker.member_id != existing.local_member
                                {
                                    folder_problem = Some(SyncProblem {
                                        path: String::new(),
                                        kind: ProblemKind::FolderMarkerMissing,
                                        peer: None,
                                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                                    });
                                    needs_recover = false;
                                } else if marker.incarnation != existing.incarnation {
                                    // Item C + Item 6(a)/(b): marker and index disagree on incarnation ->
                                    // rotate to a fresh incarnation, carry all pending tombstones and
                                    // `.legion-sync/deletions.log`, and reconcile disk by Rule D!
                                    fs.ensure_internal_dirs()?;
                                    rotate_incarnation_on_marker_mismatch(
                                        &fs,
                                        &mut store,
                                        existing,
                                        marker.incarnation,
                                    )?;
                                } else {
                                    fs.ensure_internal_dirs()?;
                                }
                            }
                        }
                    }
                }

                let skipped_paths: BTreeSet<String> = if index_damaged {
                    BTreeSet::new()
                } else {
                    store
                        .list_problems()?
                        .into_iter()
                        .filter_map(|(_, prob)| {
                            (prob.kind == ProblemKind::FilesystemError && prob.peer.is_none())
                                .then_some(prob.path)
                        })
                        .collect()
                };

                Ok((folder_problem, needs_recover, index_damaged, skipped_paths))
            },
        ));

        let (folder_problem, needs_recover, index_damaged, skipped_paths) = match init_attempt {
            Ok(Ok(tuple)) => tuple,
            Ok(Err(SyncError::IndexDamaged { .. } | SyncError::CorruptData { .. })) => {
                (Some(index_damaged_problem()), false, true, BTreeSet::new())
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => (Some(index_damaged_problem()), false, true, BTreeSet::new()),
        };

        let mut engine = Self {
            bin_retention_secs: config.bin_retention_secs,
            deletion_brake: config.deletion_brake,
            config,
            fs,
            store,
            ignore,
            stability: PendingStabilityTracker::new(),
            unreachable_peers: BTreeSet::new(),
            active_progress: BTreeMap::new(),
            last_pull_errors: BTreeMap::new(),
            peer_refusals_from_serve: Mutex::new(BTreeMap::new()),
            serve_rescan_hints: Mutex::new(BTreeSet::new()),
            skipped_paths,
            folder_problem,
            index_damaged,
            last_now: Timestamp::from_system_time(std::time::SystemTime::now()),
        };
        if needs_recover {
            match engine.recover() {
                Ok(_) => {
                    if let Ok(count) =
                        engine.guard_ref("held_back_count", |e| e.store.held_back_count())
                        && count > 0
                    {
                        let now = engine.last_now;
                        let _ = engine.retry_held_back_local(now);
                    }
                }
                Err(SyncError::IndexDamaged { .. }) => {
                    // Already transitioned cleanly into stopped damaged state.
                }
                Err(err) => return Err(err),
            }
        }
        Ok(engine)
    }

    /// Returns `true` when this folder is stopped because its sync index is missing or damaged.
    pub fn is_index_damaged(&self) -> bool {
        self.index_damaged
    }

    fn check_not_damaged(&self) -> Result<()> {
        if self.index_damaged {
            return Err(SyncError::IndexDamaged {
                folder_id: self.config.id.clone(),
                sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_string(),
                detail: "folder is stopped until its index is rebuilt from disk".to_string(),
            });
        }
        Ok(())
    }

    fn mark_index_damaged(&mut self, detail: String) -> SyncError {
        self.index_damaged = true;
        self.folder_problem = Some(index_damaged_problem());
        SyncError::IndexDamaged {
            folder_id: self.config.id.clone(),
            sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_string(),
            detail,
        }
    }

    fn guard_mut<T>(&mut self, op: &str, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.check_not_damaged()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            Ok(Ok(val)) => Ok(val),
            Ok(Err(SyncError::IndexDamaged { detail, .. })) => Err(self.mark_index_damaged(detail)),
            Ok(Err(SyncError::CorruptData { context, detail })) => {
                Err(self.mark_index_damaged(format!("corrupt {context}: {detail}")))
            }
            Ok(Err(err)) => Err(err),
            Err(payload) => Err(self.mark_index_damaged(format!(
                "panic during {op}: {}",
                engine_panic_message(&*payload)
            ))),
        }
    }

    fn guard_ref<T>(&self, op: &str, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        self.check_not_damaged()?;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self))) {
            Ok(res) => res,
            Err(payload) => Err(SyncError::IndexDamaged {
                folder_id: self.config.id.clone(),
                sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_string(),
                detail: format!("panic during {op}: {}", engine_panic_message(&*payload)),
            }),
        }
    }

    /// Explicit recovery action: rebuilds this folder's sync index from live files on disk,
    /// `.legion-sync/bin` metadata sidecars, and `.legion-sync/deletions.log` under a brand-new
    /// incarnation and the join rule so no files are lost and no recorded deletions resurrect.
    pub fn rebuild_from_disk(&mut self, now: Timestamp) -> Result<ScanReport> {
        self.last_now = now;
        self.fs.check_not_nested()?;
        let existing_marker = self.fs.read_marker()?;
        if let Some(ref m) = existing_marker
            && (m.folder_id != self.config.id || m.member_id != self.config.local_member)
        {
            return Err(SyncError::FolderUnavailable {
                folder_id: self.config.id.clone(),
                sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
            });
        }

        // Preserve every deletion from `.legion-sync/deletions.log` (and from the store if
        // readable) so rebuilding an index never resurrects deleted files.
        let mut carried_deletions = parse_deletion_log_all(&self.fs.read_deletion_log()?);
        let mut old_incarnation = existing_marker.as_ref().map(|m| m.incarnation);
        let mut all_paths = Vec::new();
        let mut all_peers = Vec::new();
        let mut old_bin_ids = Vec::new();
        let mut old_problem_keys = Vec::new();

        if !self.index_damaged {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(Some(m)) = self.store.load_meta() {
                    old_incarnation = Some(m.incarnation);
                    for pending in m.pending_deletions.values() {
                        let rec = pending_to_deletion_record(pending, &m.local_member);
                        merge_deletion_record_into_list(
                            carried_deletions.entry(rec.path.clone()).or_default(),
                            rec,
                        );
                    }
                }
                if let Ok(tombs) = self.store.list_tombstones() {
                    for tomb in tombs {
                        let rec = FolderDeletionRecord {
                            path: tomb.path.clone(),
                            kind: tomb.kind,
                            hashes: tomb.recent_hashes,
                            version: tomb.version,
                            mtime: tomb.mtime,
                            modified_by: tomb.modified_by,
                            held: false,
                        };
                        merge_deletion_record_into_list(
                            carried_deletions.entry(rec.path.clone()).or_default(),
                            rec,
                        );
                    }
                }
                let _ = self.store.scan_records_in_order(&mut |rec| {
                    all_paths.push(rec.path);
                    Ok(true)
                });
                if let Ok(cursors) = self.store.load_peer_cursors() {
                    all_peers.extend(cursors.into_keys());
                }
                if let Ok(bins) = self.store.list_bin_entries() {
                    old_bin_ids.extend(bins.into_iter().map(|b| b.id));
                }
                if let Ok(probs) = self.store.list_problems() {
                    old_problem_keys.extend(probs.into_iter().map(|(k, _)| k));
                }
            }));
        } else {
            // Even when `index_damaged` was set because the store was empty beside an existing
            // marker, clean out any partial records if the store is writable.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = self.store.scan_records_in_order(&mut |rec| {
                    all_paths.push(rec.path);
                    Ok(true)
                });
                if let Ok(cursors) = self.store.load_peer_cursors() {
                    all_peers.extend(cursors.into_keys());
                }
                if let Ok(bins) = self.store.list_bin_entries() {
                    old_bin_ids.extend(bins.into_iter().map(|b| b.id));
                }
                if let Ok(probs) = self.store.list_problems() {
                    old_problem_keys.extend(probs.into_iter().map(|(k, _)| k));
                }
            }));
        }

        // Recover self-describing bin entries from `.legion-sync/bin/*.json` sidecars.
        let mut recovered_bins = Vec::new();
        if let Ok(bin_files) = self.fs.list_bin() {
            for name in bin_files {
                if name.ends_with(".json")
                    && let Ok(Some(bytes)) = self.fs.read_bin_meta(&name)
                    && let Ok(entry) = serde_json::from_slice::<BinEntry>(&bytes)
                {
                    recovered_bins.push(entry);
                }
            }
        }

        let new_inc = choose_fresh_incarnation(None, old_incarnation)?;
        self.fs.ensure_internal_dirs()?;
        let marker = FolderMarker::new(
            self.config.id.clone(),
            self.config.local_member.clone(),
            new_inc,
        );
        self.fs.write_marker(&marker)?;

        let new_meta = FolderMeta::new(&self.config, new_inc);
        let reset_batch = StoreBatch {
            folder_meta: Some(new_meta),
            remove_records: all_paths,
            remove_peer_cursors: all_peers,
            remove_bin: old_bin_ids,
            upsert_bin: recovered_bins,
            remove_problems: old_problem_keys,
            clear_seq_fingerprints: true,
            clear_held_back: true,
            clear_deferrals: true,
            set_pending_op: Some(None),
            ..StoreBatch::default()
        };
        let apply_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.store.apply_batch(reset_batch)
        }));
        match apply_res {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err),
            Err(payload) => {
                return Err(self.mark_index_damaged(format!(
                    "panic during rebuild_from_disk: {}",
                    engine_panic_message(&*payload)
                )));
            }
        }

        self.index_damaged = false;
        self.folder_problem = None;
        self.skipped_paths.clear();
        self.active_progress.clear();
        self.last_pull_errors.clear();
        if let Ok(mut refusals) = self.peer_refusals_from_serve.lock() {
            refusals.clear();
        }
        if let Ok(mut hints) = self.serve_rescan_hints.lock() {
            hints.clear();
        }

        // Rescue any leftover files in `.legion-sync/tmp` per the core principle.
        recover_pending(&self.fs, &mut self.store)?;

        // Scan live disk under Rule D (`join_mode = true`), then re-issue every carried deletion
        // whose path is absent on disk.
        let report = scan_folder(
            &self.fs,
            &mut self.store,
            &self.ignore,
            DeletionBrakeConfig::disabled(),
            &mut self.stability,
            now,
            ScanOptions {
                allow_mass_deletion: true,
                ..ScanOptions::default()
            },
        )?;
        reissue_carried_deletions(&self.fs, &mut self.store, carried_deletions)?;
        Ok(report)
    }

    /// Runs crash recovery for any incomplete write-ahead operation and cleans `.legion-sync/tmp`.
    pub fn recover(&mut self) -> Result<RecoveryReport> {
        self.guard_mut("recover", |e| recover_pending(&e.fs, &mut e.store))
    }

    /// Borrows the underlying filesystem implementation.
    pub fn fs(&self) -> &F {
        &self.fs
    }

    /// Mutably borrows the underlying filesystem implementation.
    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    /// Borrows the underlying index store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Mutably borrows the underlying index store (primarily for fault-injection testing).
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// Returns the folder's current persistent metadata.
    pub fn meta(&self) -> Result<FolderMeta> {
        self.guard_ref("meta", |e| {
            e.store.load_meta()?.ok_or_else(|| SyncError::Storage {
                context: "load folder metadata".to_string(),
                detail: "index store is uninitialized".to_string(),
            })
        })
    }

    /// Updates the folder's custom ignore glob patterns.
    pub fn update_ignore(&mut self, patterns: Vec<String>) -> Result<()> {
        self.ignore = IgnoreMatcher::compile(&patterns)?;
        self.config.ignore = patterns;
        Ok(())
    }

    /// Updates the folder's mass-deletion brake thresholds.
    pub fn set_deletion_brake(&mut self, config: DeletionBrakeConfig) {
        self.deletion_brake = config;
        self.config.deletion_brake = config;
    }

    /// Pauses or resumes syncing for this folder.
    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        self.guard_mut("set_paused", |e| {
            let mut meta = e.meta()?;
            if meta.paused != paused {
                meta.paused = paused;
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    ..StoreBatch::default()
                };
                e.store.apply_batch(batch)?;
            }
            Ok(())
        })
    }

    /// Returns whether this folder is currently paused.
    pub fn is_paused(&self) -> Result<bool> {
        self.guard_ref("is_paused", |e| Ok(e.meta()?.paused))
    }

    /// Responds to a tripped mass-deletion brake (`ConfirmDeleted` or `Rescan`).
    pub fn resolve_deletion_brake(
        &mut self,
        decision: DeletionBrakeDecision,
        now: Timestamp,
    ) -> Result<ScanReport> {
        self.guard_mut("resolve_deletion_brake", |e| {
            e.verify_folder_marker()?;
            let mut meta = e.meta()?;
            if meta.pending_brake.is_none() {
                return Err(SyncError::Conflict {
                    path: meta.folder_id,
                    reason: "no mass-deletion brake is currently awaiting confirmation".to_string(),
                });
            }
            meta.pending_brake = None;
            meta.paused = false;
            let batch = StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            };
            e.store.apply_batch(batch)?;

            match decision {
                DeletionBrakeDecision::ConfirmDeleted => e.scan_with_options_inner(
                    now,
                    ScanOptions {
                        allow_mass_deletion: true,
                        ..ScanOptions::default()
                    },
                ),
                DeletionBrakeDecision::Rescan => {
                    e.scan_with_options_inner(now, ScanOptions::default())
                }
            }
        })
    }

    /// Promotes any pending local deletions whose settle period has elapsed at `now` without
    /// requiring a full folder walk, returning the number of deletion records emitted.
    pub fn flush_settled_deletions(&mut self, now: Timestamp) -> Result<usize> {
        self.guard_mut("flush_settled_deletions", |e| {
            e.last_now = now;
            e.verify_folder_marker()?;
            scan_flush_settled_deletions(&e.fs, &mut e.store, e.deletion_brake, now)
        })
    }

    /// Responds to a tripped inbound mass-deletion brake for `peer_id`.
    ///
    /// - [`InboundDeletionBrakeDecision::ApplyDeletions`] unpauses `peer_id` and allows its
    ///   blocked deletions up to `max_blocked_seq` to apply on the next pull.
    /// - [`InboundDeletionBrakeDecision::KeepLocalFiles`] unpauses `peer_id` and re-asserts every
    ///   blocked live local file strictly newer than the peer's deletion tombstones, returning the
    ///   number of local files re-asserted.
    pub fn resolve_inbound_deletion_brake(
        &mut self,
        peer_id: &str,
        decision: InboundDeletionBrakeDecision,
        now: Timestamp,
    ) -> Result<usize> {
        self.guard_mut("resolve_inbound_deletion_brake", |e| {
            e.last_now = now;
            e.verify_folder_marker()?;
            let mut cursors = e.store.load_peer_cursors()?;
            let Some(mut cursor) = cursors.remove(peer_id) else {
                return Err(SyncError::Conflict {
                    path: peer_id.to_string(),
                    reason: format!(
                        "no inbound mass-deletion brake is awaiting confirmation for peer '{peer_id}'"
                    ),
                });
            };
            let Some(brake) = cursor.inbound_brake.take() else {
                return Err(SyncError::Conflict {
                    path: peer_id.to_string(),
                    reason: format!(
                        "no inbound mass-deletion brake is awaiting confirmation for peer '{peer_id}'"
                    ),
                });
            };

            let remove_problems = vec![peer_problem_key(
                peer_id,
                ProblemKind::InboundDeletionBrakeTripped,
                "",
            )];
            cursor.inbound_binned_in_window.clear();
            cursor.inbound_window_start = None;
            cursor.inbound_window_start_live_files = 0;
            cursor.last_error = None;
            cursor.last_error_at = None;

            match decision {
                InboundDeletionBrakeDecision::ApplyDeletions => {
                    cursor.allow_inbound_mass_deletion_until_seq = cursor
                        .allow_inbound_mass_deletion_until_seq
                        .max(brake.max_blocked_seq);
                    e.last_pull_errors.remove(peer_id);
                    let mut batch = StoreBatch {
                        remove_problems,
                        ..StoreBatch::default()
                    };
                    batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
                    e.store.apply_batch(batch)?;
                    Ok(0)
                }
                InboundDeletionBrakeDecision::KeepLocalFiles => {
                    let mut meta = e.meta()?;
                    cursor.keep_local_files_until_seq = cursor
                        .keep_local_files_until_seq
                        .max(brake.max_blocked_seq);
                    e.last_pull_errors.remove(peer_id);
                    let mut batch = StoreBatch {
                        remove_problems,
                        ..StoreBatch::default()
                    };
                    let mut reasserted = 0usize;
                    for tomb in &brake.blocked_tombstones {
                        let Some(mut local) = e.store.get_record(&tomb.path)? else {
                            continue;
                        };
                        if local.deleted {
                            continue;
                        }
                        local.version.merge_in_place(&tomb.version);
                        meta.bump_version(&mut local.version);
                        let seq = meta.next_seq();
                        meta.record_change_mark(seq, now);
                        local.seq = seq;
                        local.modified_by = meta.local_member.clone();
                        local.from_join = false;
                        let chain = meta.advance_chain_fingerprint(seq, local.head_fingerprint());
                        batch.upsert_seq_fingerprints.push((seq, chain));
                        batch.upsert_records.push(local);
                        reasserted += 1;
                    }
                    if reasserted > 0 {
                        batch.folder_meta = Some(meta);
                    }
                    batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
                    e.store.apply_batch(batch)?;
                    Ok(reasserted)
                }
            }
        })
    }

    /// Records that `peer` is currently unreachable (reflected as `WaitingForMember` in status).
    pub fn mark_peer_unreachable(&mut self, peer: impl Into<MemberId>) {
        self.unreachable_peers.insert(peer.into());
    }

    /// Clears a peer's unreachable state.
    pub fn mark_peer_reachable(&mut self, peer: &str) {
        self.unreachable_peers.remove(peer);
    }

    /// Adds a member to this folder's member list and immediately retries locally resolvable
    /// unapplied records that were waiting for this member to become known.
    pub fn add_member(&mut self, member: impl Into<MemberId>) -> Result<()> {
        let member = member.into();
        validate_member_id(&member)?;
        self.config.members.insert(member.clone());
        self.config.former_members.remove(&member);
        self.guard_mut("add_member", |e| {
            let mut meta = e.meta()?;
            if meta.members.insert(member.clone()) {
                let was_former = meta.former_members.remove(&member);
                if was_former && member != meta.local_member {
                    meta.rejoining_peers.insert(member.clone());
                }
                e.last_pull_errors.remove(&member);
                if let Ok(mut refusals) = e.peer_refusals_from_serve.lock() {
                    refusals.remove(&member);
                }
                let remove_problems: Vec<String> = e
                    .store
                    .list_problems()?
                    .into_iter()
                    .filter(|(_, p)| p.peer.as_deref() == Some(member.as_str()))
                    .map(|(k, _)| k)
                    .collect();
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    remove_peer_cursors: vec![member.clone()],
                    remove_problems,
                    ..StoreBatch::default()
                };
                e.store.apply_batch(batch)?;
            }
            if e.store.held_back_count()? > 0 {
                let now = e.last_now;
                let _ = e.retry_held_back_local(now);
            }
            Ok(())
        })
    }

    /// Removes a member from this folder's member list, stops serving it, retains its name in
    /// `former_members` so historical version vectors remain verifiable, leaves all local files
    /// untouched, and prunes metadata acknowledged by all remaining members.
    pub fn remove_member(&mut self, member: &str) -> Result<()> {
        self.config.members.remove(member);
        self.config.former_members.insert(member.to_string());
        self.guard_mut("remove_member", |e| {
            let mut meta = e.meta()?;
            if !meta.members.remove(member) {
                return Ok(());
            }
            meta.former_members.insert(member.to_string());
            meta.rejoining_peers.remove(member);
            meta.rollback_claims_by_peer.remove(member);
            e.unreachable_peers.remove(member);
            e.active_progress.remove(member);
            e.last_pull_errors.remove(member);
            if let Ok(mut refusals) = e.peer_refusals_from_serve.lock() {
                refusals.remove(member);
            }

            let remove_held_back: Vec<(MemberId, String)> = e
                .store
                .list_held_back()?
                .into_iter()
                .filter(|h| h.from_peer == member)
                .map(|h| (h.from_peer, h.record.path))
                .collect();
            let remove_deferrals: Vec<(MemberId, String)> = e
                .store
                .list_deferrals()?
                .into_iter()
                .filter(|d| d.peer == member)
                .map(|d| (d.peer, d.path))
                .collect();
            let remove_problems: Vec<String> = e
                .store
                .list_problems()?
                .into_iter()
                .filter(|(_, p)| p.peer.as_deref() == Some(member))
                .map(|(k, _)| k)
                .collect();

            let batch = StoreBatch {
                folder_meta: Some(meta),
                remove_peer_cursors: vec![member.to_string()],
                remove_held_back,
                remove_deferrals,
                remove_problems,
                ..StoreBatch::default()
            };
            e.store.apply_batch(batch)?;

            e.prune_acknowledged_metadata_inner()
        })
    }

    /// Records a person's decision after `peer_id` has triggered
    /// [`MAX_FORCED_ROTATIONS_BEFORE_CONFIRM`] rollback rotations on this machine: either keep
    /// honouring future rollback reports from `peer_id` (resetting its counter) or ignore future
    /// rollback reports from `peer_id`.
    pub fn confirm_peer_rollback_claims(
        &mut self,
        peer_id: &str,
        decision: PeerRollbackClaimDecision,
    ) -> Result<()> {
        validate_member_id(peer_id)?;
        self.guard_mut("confirm_peer_rollback_claims", |e| {
            let mut meta = e.meta()?;
            let entry = meta
                .rollback_claims_by_peer
                .entry(peer_id.to_string())
                .or_default();
            match decision {
                PeerRollbackClaimDecision::KeepHonouring => {
                    entry.rotations_since_confirm = 0;
                    entry.ignored = false;
                }
                PeerRollbackClaimDecision::IgnoreReports => {
                    entry.ignored = true;
                }
            }
            e.last_pull_errors.remove(peer_id);
            let mut batch = StoreBatch {
                folder_meta: Some(meta),
                remove_problems: vec![
                    peer_problem_key(peer_id, ProblemKind::PeerRollbackRateLimited, ""),
                    peer_problem_key(peer_id, ProblemKind::PeerForcedRotation, ""),
                ],
                ..StoreBatch::default()
            };
            if let Some(mut cursor) = e.store.load_peer_cursors()?.remove(peer_id)
                && cursor.last_error.is_some()
            {
                cursor.last_error = None;
                cursor.last_error_at = None;
                batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
            }
            e.store.apply_batch(batch)
        })
    }

    /// Dismisses durable informational problems matching `kind` (and optional `peer`), returning
    /// the number of dismissed entries.
    pub fn dismiss_problem(&mut self, kind: ProblemKind, peer: Option<&str>) -> Result<usize> {
        self.guard_mut("dismiss_problem", |e| {
            let keys: Vec<String> = e
                .store
                .list_problems()?
                .into_iter()
                .filter(|(_, p)| p.kind == kind && (peer.is_none() || p.peer.as_deref() == peer))
                .map(|(k, _)| k)
                .collect();
            let count = keys.len();
            if count > 0 {
                e.store.apply_batch(StoreBatch {
                    remove_problems: keys,
                    ..StoreBatch::default()
                })?;
            }
            Ok(count)
        })
    }

    /// Scans the local folder using default [`ScanOptions`].
    pub fn scan(&mut self, now: Timestamp) -> Result<ScanReport> {
        self.scan_with_options(now, ScanOptions::default())
    }

    /// Scans the local folder with explicit [`ScanOptions`].
    pub fn scan_with_options(
        &mut self,
        now: Timestamp,
        options: ScanOptions,
    ) -> Result<ScanReport> {
        self.guard_mut("scan_with_options", |e| {
            e.scan_with_options_inner(now, options)
        })
    }

    /// Records that a peer observed a hash or size mismatch for bytes served from `path` so the
    /// next scan re-hashes `path` even if its stat metadata appears unchanged.
    pub fn note_serve_hash_mismatch(&self, path: &str) {
        if let Ok(clean) = validate_sync_path(path)
            && !is_internal_sync_path(&clean)
            && let Ok(mut hints) = self.serve_rescan_hints.lock()
        {
            hints.insert(clean);
        }
    }

    /// Returns paths currently queued for re-hashing due to serving staleness or fetcher hash
    /// mismatches.
    pub fn pending_serve_rescan_hints(&self) -> Vec<String> {
        self.serve_rescan_hints
            .lock()
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Rescans any paths flagged during serving (due to on-disk stat mismatch or a fetcher
    /// reporting a hash/size mismatch), returning the number of hinted paths processed.
    pub fn rescan_serve_hints(&mut self, now: Timestamp) -> Result<usize> {
        self.guard_mut("rescan_serve_hints", |e| {
            let paths = e.drain_serve_rescan_hints_to_meta()?;
            let count = paths.len();
            for path in paths {
                let _ = e.scan_subtree_inner(&path, now, ScanOptions::default())?;
            }
            Ok(count)
        })
    }

    fn drain_serve_rescan_hints_to_meta(&mut self) -> Result<Vec<String>> {
        let hinted: Vec<String> = self
            .serve_rescan_hints
            .lock()
            .map(|mut h| std::mem::take(&mut *h).into_iter().collect())
            .unwrap_or_default();
        if hinted.is_empty() {
            return Ok(Vec::new());
        }
        let mut meta = self.meta()?;
        let mut changed = false;
        for path in &hinted {
            if meta.needs_rescan.insert(path.clone()) {
                changed = true;
            }
        }
        if changed {
            self.store.apply_batch(StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            })?;
        }
        Ok(hinted)
    }

    fn scan_with_options_inner(
        &mut self,
        now: Timestamp,
        options: ScanOptions,
    ) -> Result<ScanReport> {
        self.last_now = now;
        self.verify_folder_marker()?;
        let _ = self.drain_serve_rescan_hints_to_meta()?;
        let report = scan_folder(
            &self.fs,
            &mut self.store,
            &self.ignore,
            self.deletion_brake,
            &mut self.stability,
            now,
            options,
        )?;
        let _ = inspect_and_promote_modified_bin_entries(
            &self.fs,
            &mut self.store,
            options.verify_checksums,
            now,
        )?;
        self.skipped_paths = report
            .skipped_errors
            .iter()
            .map(|(p, _)| p.clone())
            .collect();

        // Synchronize durable scan and dir_kept problems in one batch.
        let existing_problems = self.store.list_problems()?;
        let mut remove_problems = Vec::new();
        for (key, prob) in existing_problems {
            if key.starts_with(SCAN_PROBLEM_PREFIX)
                || (key.starts_with(DIR_KEPT_PROBLEM_PREFIX)
                    && self.fs.stat_live(&prob.path).ok().flatten().is_none())
            {
                remove_problems.push(key);
            }
        }
        let mut upsert_problems = Vec::new();
        for (path, reason) in &report.skipped_errors {
            upsert_problems.push((
                scan_problem_key(ProblemKind::FilesystemError, path),
                SyncProblem {
                    path: path.clone(),
                    kind: ProblemKind::FilesystemError,
                    peer: None,
                    sentence: format!("'{path}' could not be read: {reason}."),
                },
            ));
        }
        for prob in &report.held_back_problems {
            upsert_problems.push((scan_problem_key(prob.kind, &prob.path), prob.clone()));
        }
        if !remove_problems.is_empty() || !upsert_problems.is_empty() {
            self.store.apply_batch(StoreBatch {
                remove_problems,
                upsert_problems,
                ..StoreBatch::default()
            })?;
        }
        Ok(report)
    }

    /// Scans only the live entry or subtree rooted at `rel_root` without walking unrelated files
    /// (intended for incremental file-watcher notifications).
    pub fn scan_subtree(
        &mut self,
        rel_root: &str,
        now: Timestamp,
        options: ScanOptions,
    ) -> Result<ScanReport> {
        self.guard_mut("scan_subtree", |e| {
            e.scan_subtree_inner(rel_root, now, options)
        })
    }

    fn scan_subtree_inner(
        &mut self,
        rel_root: &str,
        now: Timestamp,
        options: ScanOptions,
    ) -> Result<ScanReport> {
        self.last_now = now;
        self.verify_folder_marker()?;
        let _ = self.drain_serve_rescan_hints_to_meta()?;
        let clean_root = validate_sync_path(rel_root)?;
        let report = scan_subtree(
            &self.fs,
            &mut self.store,
            &clean_root,
            &self.ignore,
            self.deletion_brake,
            &mut self.stability,
            now,
            options,
        )?;
        let _ = inspect_and_promote_modified_bin_entries(
            &self.fs,
            &mut self.store,
            options.verify_checksums,
            now,
        )?;
        self.skipped_paths
            .retain(|p| p != &clean_root && !is_descendant_of(p, &clean_root));

        let mut remove_problems = Vec::new();
        for (key, prob) in self.store.list_problems()? {
            if key.starts_with(SCAN_PROBLEM_PREFIX)
                && (prob.path == clean_root || is_descendant_of(&prob.path, &clean_root))
            {
                remove_problems.push(key);
            }
        }
        let mut upsert_problems = Vec::new();
        for (path, reason) in &report.skipped_errors {
            self.skipped_paths.insert(path.clone());
            upsert_problems.push((
                scan_problem_key(ProblemKind::FilesystemError, path),
                SyncProblem {
                    path: path.clone(),
                    kind: ProblemKind::FilesystemError,
                    peer: None,
                    sentence: format!("'{path}' could not be read: {reason}."),
                },
            ));
        }
        for prob in &report.held_back_problems {
            upsert_problems.push((scan_problem_key(prob.kind, &prob.path), prob.clone()));
        }
        if !remove_problems.is_empty() || !upsert_problems.is_empty() {
            self.store.apply_batch(StoreBatch {
                remove_problems,
                upsert_problems,
                ..StoreBatch::default()
            })?;
        }
        Ok(report)
    }

    /// Pulls incremental index changes from `peer` using [`DEFAULT_PAGE_LIMIT`] and default fetch
    /// options.
    pub fn pull_from_peer(&mut self, peer: &dyn Peer, now: Timestamp) -> Result<PullReport> {
        self.pull_from_peer_with_options(peer, DEFAULT_PAGE_LIMIT, &FetchOptions::default(), now)
    }

    /// Pulls incremental index changes from `peer` with an explicit page size.
    pub fn pull_from_peer_with_limit(
        &mut self,
        peer: &dyn Peer,
        page_limit: usize,
        now: Timestamp,
    ) -> Result<PullReport> {
        self.pull_from_peer_with_options(peer, page_limit, &FetchOptions::default(), now)
    }

    /// Pulls incremental index changes from `peer` with explicit page limit and [`FetchOptions`]
    /// (timeout and cooperative cancellation).
    pub fn pull_from_peer_with_options(
        &mut self,
        peer: &dyn Peer,
        page_limit: usize,
        fetch_opts: &FetchOptions,
        now: Timestamp,
    ) -> Result<PullReport> {
        let peer_id = peer.member_id().to_string();
        let res = self.guard_mut("pull_from_peer_with_options", |e| {
            e.pull_from_peer_with_options_inner(peer, page_limit, fetch_opts, now)
        });
        match &res {
            Ok(_) => {
                self.last_pull_errors.remove(&peer_id);
            }
            Err(err) => {
                self.active_progress.remove(&peer_id);
                if !matches!(
                    err,
                    SyncError::Paused { .. }
                        | SyncError::PeerUnavailable { .. }
                        | SyncError::Cancelled { .. }
                        | SyncError::IndexDamaged { .. }
                        | SyncError::FolderUnavailable { .. }
                ) {
                    let err_prob = SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::FilesystemError,
                        peer: Some(peer_id.clone()),
                        sentence: format!("Could not finish syncing with '{peer_id}': {err}"),
                    };
                    self.last_pull_errors
                        .insert(peer_id.clone(), err_prob.clone());
                    self.persist_peer_pull_error(&peer_id, err_prob, now);
                }
            }
        }
        res
    }

    fn pull_from_peer_with_options_inner(
        &mut self,
        peer: &dyn Peer,
        page_limit: usize,
        fetch_opts: &FetchOptions,
        now: Timestamp,
    ) -> Result<PullReport> {
        self.last_now = now;
        let meta = self.verify_folder_marker()?;
        if meta.paused {
            return Err(SyncError::Paused {
                folder_id: meta.folder_id,
            });
        }
        let peer_id = peer.member_id().to_string();
        if !meta.members.contains(&peer_id) {
            return Err(SyncError::Denied {
                member: peer_id,
                reason: "peer is not a member of this sync folder".to_string(),
            });
        }
        if peer_id == meta.local_member {
            return Ok(PullReport::default());
        }

        // Refuse syncing with a peer permanently flagged for alternating incarnations, or paused
        // by an unresolved inbound mass-deletion brake.
        let mut initial_cursors = self.store.load_peer_cursors()?;
        if let Some(existing_cursor) = initial_cursors.get(&peer_id) {
            if existing_cursor.alternation_detected {
                return Err(SyncError::Conflict {
                    path: peer_id.clone(),
                    reason: duplicate_member_sentence(&peer_id),
                });
            }
            if existing_cursor.inbound_brake.is_some() {
                return Err(SyncError::Paused {
                    folder_id: meta.folder_id,
                });
            }
        }

        // Bidirectional handshake FIRST (before any page moves in either direction!).
        let mut meta = meta;
        let mut hs = self.do_handshake(peer, &peer_id, &meta, &initial_cursors)?;

        let existing_for_peer = initial_cursors.get(&peer_id).cloned();
        let peer_expected_rejoin = hs.caller_rejoining
            && hs.caller_pull_matches != Some(false)
            && existing_for_peer.as_ref().is_some_and(|c| {
                c.incarnation == hs.incarnation
                    && hs.snapshot_seq >= c.seq
                    && !(hs.snapshot_seq == c.seq
                        && c.seq > 0
                        && hs.head_fingerprint.is_some()
                        && c.head_fingerprint.is_some()
                        && hs.head_fingerprint != c.head_fingerprint)
            })
            && match hs.caller_pulled {
                Some(ref v) => view_matches_local(&self.store, &meta, v)?,
                None => true,
            }
            && match hs.caller_acked {
                Some(ref v) => view_matches_local(&self.store, &meta, v)?,
                None => true,
            };
        let peer_regressed_below_ack = !peer_expected_rejoin
            && existing_for_peer.as_ref().is_some_and(|c| {
                c.incarnation == hs.incarnation
                    && c.acked_local_incarnation == meta.incarnation
                    && c.acked_local_seq > 0
                    && (match hs.caller_acked {
                        Some(v) if v.incarnation == meta.incarnation => v.seq < c.acked_local_seq,
                        None => true,
                        _ => false,
                    } || match hs.caller_pulled {
                        Some(v) if v.incarnation == meta.incarnation => v.seq < c.acked_local_seq,
                        None => true,
                        _ => false,
                    })
            });

        // Self-detected rollback from ONE fact: in the handshake, each side already reports the
        // highest sequence of our current incarnation that it has received (`caller_pulled` /
        // `caller_acked`). If the peer has received more from us than we have ever produced
        // (`view.seq > meta.local_seq`) or a contradicting fingerprint at a sequence we have, we
        // went back in time and rotate to a fresh incarnation under the join rule. Nothing else
        // forces a rotation from outside.
        let own_rolled_back = !peer_regressed_below_ack
            && (hs.caller_pulled.is_some_and(|v| {
                view_proves_own_rollback(&self.store, &meta, &v).unwrap_or(false)
            }) || hs.caller_acked.is_some_and(|v| {
                view_proves_own_rollback(&self.store, &meta, &v).unwrap_or(false)
            }));
        if own_rolled_back && self.recover_own_rollback(&peer_id, &meta, now)? {
            meta = self.meta()?;
            initial_cursors = self.store.load_peer_cursors()?;
            hs = self.do_handshake(peer, &peer_id, &meta, &initial_cursors)?;
        }

        // Check if the PEER went back in time BEFORE any page moves!
        if let Some(existing_cursor) = initial_cursors.get(&peer_id)
            && existing_cursor.incarnation == hs.incarnation
            && (existing_cursor.seq > 0 || peer_regressed_below_ack)
        {
            let peer_went_back = peer_regressed_below_ack
                || hs.snapshot_seq < existing_cursor.seq
                || hs.caller_pull_matches == Some(false)
                || (existing_cursor.seq > 0
                    && hs.snapshot_seq == existing_cursor.seq
                    && hs.head_fingerprint.is_some()
                    && existing_cursor.head_fingerprint.is_some()
                    && hs.head_fingerprint != existing_cursor.head_fingerprint);
            if peer_went_back {
                self.flag_duplicate_member(&peer_id, existing_cursor.clone(), false)?;
                return Err(SyncError::Conflict {
                    path: peer_id.clone(),
                    reason: peer_rollback_sentence(&peer_id),
                });
            }
        }

        // Clean non-regressed handshake: clear any stale rate-limit problem for this peer and
        // update whether this peer currently refuses any of our records.
        self.sync_peer_handshake_problems(&peer_id, hs.refused_from_caller.as_ref())?;

        let meta = if meta.join_mode {
            // Complete the initial join scan before pulling from any peer so any pre-existing
            // local files are tagged `from_join = true` and `join_mode` is cleared before
            // post-sync user edits or renames occur.
            self.scan(now)?;
            self.meta()?
        } else {
            meta
        };

        let mut observed_cursor =
            self.observe_peer_incarnation_with(&peer_id, hs.incarnation, initial_cursors)?;
        let mut cursor_dirty = false;
        if hs.snapshot_seq > observed_cursor.last_seen_snapshot_seq {
            observed_cursor.last_seen_snapshot_seq = hs.snapshot_seq;
            cursor_dirty = true;
        }

        let mut hs_ack_advanced = false;
        if peer_expected_rejoin {
            let (new_ack_inc, new_ack_seq) = match hs.caller_acked {
                Some(acked_view)
                    if acked_view.incarnation == meta.incarnation
                        && acked_view.seq <= meta.local_seq
                        && view_matches_local(&self.store, &meta, &acked_view)? =>
                {
                    (acked_view.incarnation, acked_view.seq)
                }
                _ => (0, 0),
            };
            if observed_cursor.acked_local_incarnation != new_ack_inc
                || observed_cursor.acked_local_seq != new_ack_seq
            {
                observed_cursor.acked_local_incarnation = new_ack_inc;
                observed_cursor.acked_local_seq = new_ack_seq;
                cursor_dirty = true;
            }
        } else if let Some(acked_view) = hs.caller_acked
            && acked_view.incarnation == meta.incarnation
            && acked_view.seq <= meta.local_seq
            && view_matches_local(&self.store, &meta, &acked_view)?
        {
            let prev_acked = (
                observed_cursor.acked_local_incarnation,
                observed_cursor.acked_local_seq,
            );
            if observed_cursor.acked_local_incarnation != acked_view.incarnation {
                observed_cursor.acked_local_incarnation = acked_view.incarnation;
                observed_cursor.acked_local_seq = acked_view.seq;
            } else {
                observed_cursor.acked_local_seq =
                    observed_cursor.acked_local_seq.max(acked_view.seq);
            }
            hs_ack_advanced = (
                observed_cursor.acked_local_incarnation,
                observed_cursor.acked_local_seq,
            ) != prev_acked;
            if hs_ack_advanced {
                cursor_dirty = true;
            }
        }
        if cursor_dirty {
            self.store.apply_batch(StoreBatch {
                set_peer_cursors: BTreeMap::from([(peer_id.clone(), observed_cursor.clone())]),
                ..StoreBatch::default()
            })?;
        }

        let mut cached_cursor = Some(observed_cursor);
        let mut cached_meta: Option<FolderMeta> = if cached_cursor
            .as_ref()
            .is_some_and(|c| c.seq == 0 && meta.rejoining_peers.is_empty())
        {
            None
        } else {
            Some(meta)
        };

        let limit = page_limit.clamp(1, MAX_PAGE_RECORDS);
        let mut report = PullReport::default();
        let cancel_ref = fetch_opts.cancel.as_deref();
        let mut newly_held_back_this_pull: BTreeSet<String> = BTreeSet::new();

        loop {
            if cancel_ref.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(SyncError::Cancelled {
                    operation: format!("pull from {peer_id}"),
                });
            }

            let mut cursor = match cached_cursor.take() {
                Some(c) => c,
                None => self
                    .store
                    .load_peer_cursors()?
                    .get(&peer_id)
                    .cloned()
                    .unwrap_or_default(),
            };
            if cursor.duplicate_detected {
                return Err(SyncError::Conflict {
                    path: peer_id.clone(),
                    reason: peer_identity_problem_sentence(&peer_id, cursor.alternation_detected),
                });
            }
            let cursor_before_page = cursor.clone();

            let since_inc = cursor.incarnation;
            let since_seq = cursor.seq;

            let page = match peer.records_since(since_inc, since_seq, limit, cancel_ref) {
                Ok(page) => {
                    self.unreachable_peers.remove(&peer_id);
                    page
                }
                Err(SyncError::PeerUnavailable { member, detail }) => {
                    self.unreachable_peers.insert(peer_id.clone());
                    return Err(SyncError::PeerUnavailable { member, detail });
                }
                Err(err) => return Err(err),
            };

            if page.incarnation == 0 {
                self.record_peer_problem(
                    &peer_id,
                    SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::InvalidPeerRecord,
                        peer: Some(peer_id.clone()),
                        sentence: format!(
                            "Peer '{peer_id}' sent a page with an invalid zero incarnation id."
                        ),
                    },
                )?;
                break;
            }

            if page.records.len() > MAX_PAGE_RECORDS {
                self.record_peer_problem(
                    &peer_id,
                    SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::InvalidPeerRecord,
                        peer: Some(peer_id.clone()),
                        sentence: format!(
                            "Peer '{peer_id}' sent {} records in one page, exceeding limit {MAX_PAGE_RECORDS}.",
                            page.records.len()
                        ),
                    },
                )?;
                break;
            }

            // Re-check OUR OWN rollback from the page's direct acknowledgement too.
            let local_meta = match cached_meta.take() {
                Some(m) => m,
                None => self.meta()?,
            };
            if let Some((acked_inc, acked_seq)) = page.caller_acked {
                let peer_regressed = cursor.incarnation == page.incarnation
                    && cursor.acked_local_incarnation == local_meta.incarnation
                    && acked_inc == local_meta.incarnation
                    && acked_seq < cursor.acked_local_seq;
                if peer_regressed {
                    self.flag_duplicate_member(&peer_id, cursor, false)?;
                    return Err(SyncError::Conflict {
                        path: peer_id.clone(),
                        reason: peer_rollback_sentence(&peer_id),
                    });
                }
                let page_view = PeerAckView {
                    incarnation: acked_inc,
                    seq: acked_seq,
                    fingerprint: page.caller_acked_fingerprint,
                };
                if view_proves_own_rollback(&self.store, &local_meta, &page_view)?
                    && self.recover_own_rollback(&peer_id, &local_meta, now)?
                {
                    continue;
                }
            }

            // Check if peer changed incarnation between handshake and page (or alternated!).
            if page.incarnation != cursor.incarnation {
                cursor = self.observe_peer_incarnation(&peer_id, page.incarnation)?;
            }

            // Detect peer rollback or same-seq divergence within the SAME incarnation.
            if page.incarnation == cursor.incarnation {
                if page.snapshot_seq < cursor.seq {
                    self.flag_duplicate_member(&peer_id, cursor, false)?;
                    return Err(SyncError::Conflict {
                        path: peer_id.clone(),
                        reason: peer_rollback_sentence(&peer_id),
                    });
                }
                if page.snapshot_seq == cursor.seq
                    && cursor.seq > 0
                    && let (Some(peer_fp), Some(seen_fp)) =
                        (page.head_fingerprint, cursor.head_fingerprint)
                    && peer_fp != seen_fp
                {
                    self.flag_duplicate_member(&peer_id, cursor, false)?;
                    return Err(SyncError::Conflict {
                        path: peer_id.clone(),
                        reason: peer_rollback_sentence(&peer_id),
                    });
                }
            }

            // Update direct acknowledgement of OUR OWN current incarnation ONLY.
            let mut ack_advanced = false;
            if let Some((acked_inc, acked_seq)) = page.caller_acked
                && acked_inc == local_meta.incarnation
                && acked_seq <= local_meta.local_seq
            {
                let prev_acked = (cursor.acked_local_incarnation, cursor.acked_local_seq);
                if cursor.acked_local_incarnation != acked_inc {
                    cursor.acked_local_incarnation = acked_inc;
                    cursor.acked_local_seq = acked_seq;
                } else {
                    cursor.acked_local_seq = cursor.acked_local_seq.max(acked_seq);
                }
                ack_advanced =
                    (cursor.acked_local_incarnation, cursor.acked_local_seq) != prev_acked;
            }

            report.records_seen += page.records.len();

            if page.records.is_empty() {
                cursor.incarnation = page.incarnation;
                cursor.last_seen_snapshot_seq =
                    cursor.last_seen_snapshot_seq.max(page.snapshot_seq);
                cursor.last_synced_at = Some(now);
                cursor.last_error = None;
                cursor.last_error_at = None;
                if page.snapshot_seq >= cursor.seq {
                    cursor.seq = page.snapshot_seq;
                    if let Some(&fp) = page.chain_fingerprints.get(&cursor.seq) {
                        cursor.head_fingerprint = Some(fp);
                    } else if page.head_fingerprint.is_some() {
                        cursor.head_fingerprint = page.head_fingerprint;
                    }
                }
                if cursor.applied_ack_seq != cursor.seq || self.store.held_back_count()? > 0 {
                    let held_now = self.store.list_held_back()?;
                    let defs_now = self.store.list_deferrals()?;
                    recompute_peer_applied_ack(&peer_id, &mut cursor, &held_now, &defs_now);
                }
                let clear_rejoin = !page.has_more
                    && local_meta.rejoining_peers.contains(&peer_id)
                    && self.peer_caught_up_for_rejoin(&local_meta, &cursor)?;
                if cursor != cursor_before_page || clear_rejoin {
                    self.persist_peer_cursor_and_clear_rejoin(&peer_id, cursor, clear_rejoin)?;
                }
                self.active_progress.remove(&peer_id);
                if ack_advanced || hs_ack_advanced || local_meta.unconfirmed_join_until_seq > 0 {
                    self.prune_acknowledged_metadata_inner()?;
                }
                break;
            }

            let page_incarnation = page.incarnation;
            let chain_fps = page.chain_fingerprints;

            let mut remaining_files = page.records.len() as u64;
            let mut remaining_bytes: u64 = page
                .records
                .iter()
                .filter(|r| !r.deleted && r.kind == EntryKind::File)
                .map(|r| r.size)
                .sum();
            self.active_progress.insert(
                peer_id.clone(),
                SyncProgress {
                    files_left: remaining_files,
                    bytes_left: remaining_bytes,
                },
            );

            // Order records: deletions deepest-first, live entries shallowest-first.
            let mut ordered: Vec<IndexRecord> = page.records;
            ordered.sort_by(|a, b| match (a.deleted, b.deleted) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => path_depth(&b.path)
                    .cmp(&path_depth(&a.path))
                    .then_with(|| a.seq.cmp(&b.seq)),
                (false, false) => path_depth(&a.path)
                    .cmp(&path_depth(&b.path))
                    .then_with(|| a.seq.cmp(&b.seq)),
            });

            // Detect local rename pairs (delete + create with identical content hash already
            // present locally) BEFORE executing individual deletes/fetches!
            let (rename_pairs, remaining_after_renames) =
                self.extract_local_rename_pairs(&peer_id, ordered)?;

            // Split remaining records into Phase 1 (deletions, deepest-first), Phase 2 (live
            // directories, shallowest-first), and remaining live non-directory records.
            let mut delete_records = Vec::new();
            let mut live_dir_records = Vec::new();
            let mut live_non_dir_records = Vec::new();
            for rec in remaining_after_renames {
                if rec.deleted {
                    delete_records.push(rec);
                } else if rec.kind == EntryKind::Dir {
                    live_dir_records.push(rec);
                } else {
                    live_non_dir_records.push(rec);
                }
            }

            // Item 8: matching inbound peer deletion brake! Checked BEFORE mutating disk so a
            // peer's mass deletion pauses that peer without touching local files or advancing its
            // cursor, letting the person choose ApplyDeletions or KeepLocalFiles once for the full
            // pending batch.
            if self.deletion_brake.is_enabled() && !delete_records.is_empty() {
                let window_secs = self.deletion_brake.window_secs as i64;
                cursor
                    .inbound_binned_in_window
                    .retain(|(_, t)| now.secs >= t.secs && (now.secs - t.secs) <= window_secs);
                let current_live_files = self.store.live_file_count()? as usize;
                let window_expired = cursor
                    .inbound_window_start
                    .is_none_or(|ws| now.secs < ws.secs || (now.secs - ws.secs) > window_secs);
                if cursor.inbound_binned_in_window.is_empty()
                    || cursor.inbound_window_start.is_none()
                {
                    cursor.inbound_window_start = Some(now);
                    cursor.inbound_window_start_live_files = current_live_files;
                } else if window_expired {
                    cursor.inbound_window_start = Some(now);
                    cursor.inbound_window_start_live_files =
                        current_live_files + cursor.inbound_binned_in_window.len();
                }

                let mut live_create_hashes: BTreeSet<Hash> = live_non_dir_records
                    .iter()
                    .filter(|r| !r.deleted && r.kind == EntryKind::File)
                    .filter_map(|r| r.hash)
                    .collect();
                let cur_meta = self.meta()?;
                let caps = self.fs.capabilities();
                let view = StoreIndexView::new(&self.store);
                let mut candidate_tombstones: Vec<IndexRecord> = Vec::new();
                for remote in &delete_records {
                    if remote.seq <= cursor.allow_inbound_mass_deletion_until_seq
                        || remote.seq <= cursor.keep_local_files_until_seq
                    {
                        continue;
                    }
                    let Some(local) = view.get_record(&remote.path) else {
                        continue;
                    };
                    if local.deleted || local.kind != EntryKind::File {
                        continue;
                    }
                    if local.hash.is_some_and(|h| live_create_hashes.contains(&h)) {
                        continue;
                    }
                    if matches!(
                        reconcile_record(&view, remote, &peer_id, &cur_meta, &caps, &self.ignore),
                        ReconcileDecision::DeleteToBin { .. }
                    ) {
                        candidate_tombstones.push(remote.clone());
                    }
                }

                let mut distinct_inbound_deletes: BTreeSet<String> = BTreeSet::new();
                for (p, _) in &cursor.inbound_binned_in_window {
                    distinct_inbound_deletes.insert(p.clone());
                }
                for tomb in &candidate_tombstones {
                    distinct_inbound_deletes.insert(tomb.path.clone());
                }
                let inbound_baseline = cursor
                    .inbound_window_start_live_files
                    .max(current_live_files);
                if !candidate_tombstones.is_empty()
                    && self
                        .deletion_brake
                        .should_trip(distinct_inbound_deletes.len(), inbound_baseline)
                {
                    // Count ahead read-only through all remaining pending pages of this peer so
                    // the person sees the full deletion count and confirms ONCE per batch.
                    let mut max_blocked_seq = candidate_tombstones
                        .iter()
                        .map(|r| r.seq)
                        .max()
                        .unwrap_or(page.snapshot_seq);
                    if page.has_more {
                        let mut scan_seq = delete_records
                            .iter()
                            .chain(live_dir_records.iter())
                            .chain(live_non_dir_records.iter())
                            .map(|r| r.seq)
                            .max()
                            .unwrap_or(since_seq);
                        let mut more = true;
                        let mut pages_peeked = 0usize;
                        const MAX_BRAKE_LOOKAHEAD_PAGES: usize = 256;
                        while more && pages_peeked < MAX_BRAKE_LOOKAHEAD_PAGES {
                            if cancel_ref.is_some_and(|c| c.load(Ordering::Relaxed)) {
                                break;
                            }
                            pages_peeked += 1;
                            let Ok(next_page) = peer.records_since(
                                page_incarnation,
                                scan_seq,
                                MAX_PAGE_RECORDS,
                                cancel_ref,
                            ) else {
                                break;
                            };
                            if next_page.incarnation != page_incarnation
                                || next_page.records.is_empty()
                            {
                                break;
                            }
                            let mut page_max_seq = scan_seq;
                            for r in &next_page.records {
                                page_max_seq = page_max_seq.max(r.seq);
                                if !r.deleted
                                    && r.kind == EntryKind::File
                                    && let Some(h) = r.hash
                                {
                                    live_create_hashes.insert(h);
                                }
                            }
                            for r in &next_page.records {
                                if !r.deleted
                                    || r.seq <= cursor.allow_inbound_mass_deletion_until_seq
                                    || r.seq <= cursor.keep_local_files_until_seq
                                {
                                    continue;
                                }
                                let Some(local) = view.get_record(&r.path) else {
                                    continue;
                                };
                                if local.deleted || local.kind != EntryKind::File {
                                    continue;
                                }
                                if local.hash.is_some_and(|h| live_create_hashes.contains(&h)) {
                                    continue;
                                }
                                if matches!(
                                    reconcile_record(
                                        &view,
                                        r,
                                        &peer_id,
                                        &cur_meta,
                                        &caps,
                                        &self.ignore
                                    ),
                                    ReconcileDecision::DeleteToBin { .. }
                                ) {
                                    max_blocked_seq = max_blocked_seq.max(r.seq);
                                    candidate_tombstones.push(r.clone());
                                }
                            }
                            if page_max_seq <= scan_seq {
                                break;
                            }
                            scan_seq = page_max_seq;
                            more = next_page.has_more;
                        }
                        // Re-filter against any live creates discovered on subsequent pages.
                        candidate_tombstones.retain(|tomb| {
                            view.get_record(&tomb.path)
                                .and_then(|l| l.hash)
                                .is_none_or(|h| !live_create_hashes.contains(&h))
                        });
                        distinct_inbound_deletes.clear();
                        for (p, _) in &cursor.inbound_binned_in_window {
                            distinct_inbound_deletes.insert(p.clone());
                        }
                        for tomb in &candidate_tombstones {
                            distinct_inbound_deletes.insert(tomb.path.clone());
                        }
                    }

                    let deleted_count = distinct_inbound_deletes.len();
                    let sample_paths: Vec<String> = candidate_tombstones
                        .iter()
                        .take(10)
                        .map(|r| r.path.clone())
                        .collect();
                    let blocked_paths: Vec<String> = candidate_tombstones
                        .iter()
                        .take(4096)
                        .map(|r| r.path.clone())
                        .collect();
                    let blocked_tombstones: Vec<IndexRecord> =
                        candidate_tombstones.into_iter().take(4096).collect();
                    let sentence = format!(
                        "Paused syncing from '{peer_id}' because '{peer_id}' wants to remove {deleted_count} of {inbound_baseline} files here; choose whether to apply the deletions or keep this machine's files."
                    );
                    cursor.inbound_brake = Some(InboundDeletionBrake {
                        peer: peer_id.clone(),
                        deleted_count,
                        total_live_files: inbound_baseline,
                        max_blocked_seq,
                        sample_paths,
                        blocked_paths,
                        blocked_tombstones,
                    });
                    let mut batch = StoreBatch::default();
                    batch.set_peer_cursors.insert(peer_id.clone(), cursor);
                    batch.upsert_problems.push((
                        peer_problem_key(&peer_id, ProblemKind::InboundDeletionBrakeTripped, ""),
                        SyncProblem {
                            path: String::new(),
                            kind: ProblemKind::InboundDeletionBrakeTripped,
                            peer: Some(peer_id.clone()),
                            sentence,
                        },
                    ));
                    self.store.apply_batch(batch)?;
                    self.active_progress.remove(&peer_id);
                    return Err(SyncError::Paused {
                        folder_id: local_meta.folder_id,
                    });
                }
            }

            let mut seq_results: BTreeMap<u64, bool> = BTreeMap::new();
            let mut page_held_back: BTreeMap<(MemberId, String), HeldBackRecord> = BTreeMap::new();
            let mut existing_held_by_key: BTreeMap<(MemberId, String), HeldBackRecord> =
                if self.store.held_back_count()? > 0 {
                    self.store
                        .list_held_back()?
                        .into_iter()
                        .map(|h| ((h.from_peer.clone(), h.record.path.clone()), h))
                        .collect()
                } else {
                    BTreeMap::new()
                };
            let mut known_held_keys: BTreeSet<(MemberId, String)> =
                existing_held_by_key.keys().cloned().collect();
            let mut any_deferred = false;
            let mut held_back_full = false;
            let mut deferred_files = 0u64;
            let mut deferred_bytes = 0u64;

            let keep_local_until_seq = cursor.keep_local_files_until_seq;
            for (del_raw, del_tomb, add_raw, add_target) in rename_pairs {
                let del_seq = del_raw.seq;
                let add_seq = add_raw.seq;
                match apply_local_rename(
                    &self.fs,
                    &mut self.store,
                    &peer_id,
                    del_tomb,
                    add_target,
                    now,
                )? {
                    ApplyOutcome::Applied => {
                        seq_results.insert(del_seq, true);
                        seq_results.insert(add_seq, true);
                        report.applied += 2;
                        report.local_renames += 1;
                        self.clear_deferred(&peer_id, &del_raw.path)?;
                        self.clear_deferred(&peer_id, &add_raw.path)?;
                    }
                    ApplyOutcome::LocalChangedRetry
                    | ApplyOutcome::Deferred { .. }
                    | ApplyOutcome::HoldBackLocal(_) => {
                        let ok1 = self.process_one_remote(
                            peer,
                            &del_raw,
                            page_incarnation,
                            prev_chain_fp_for(del_seq, &chain_fps, &cursor),
                            keep_local_until_seq,
                            fetch_opts,
                            now,
                            &mut report,
                            &mut page_held_back,
                            &mut existing_held_by_key,
                            &mut known_held_keys,
                            &mut newly_held_back_this_pull,
                            &mut held_back_full,
                        )?;
                        seq_results.insert(del_seq, ok1);
                        if held_back_full {
                            any_deferred = true;
                            break;
                        }
                        let ok2 = self.process_one_remote(
                            peer,
                            &add_raw,
                            page_incarnation,
                            prev_chain_fp_for(add_seq, &chain_fps, &cursor),
                            keep_local_until_seq,
                            fetch_opts,
                            now,
                            &mut report,
                            &mut page_held_back,
                            &mut existing_held_by_key,
                            &mut known_held_keys,
                            &mut newly_held_back_this_pull,
                            &mut held_back_full,
                        )?;
                        seq_results.insert(add_seq, ok2);
                        if !ok1 || !ok2 {
                            any_deferred = true;
                            deferred_files += u64::from(!ok1) + u64::from(!ok2);
                        }
                        if held_back_full {
                            break;
                        }
                    }
                    ApplyOutcome::DirKeptLocalFiles(_) => {
                        seq_results.insert(del_seq, true);
                        seq_results.insert(add_seq, true);
                    }
                }
                remaining_files = remaining_files.saturating_sub(2);
            }

            // Phase 1: apply deletions first.
            if !held_back_full {
                for remote in delete_records {
                    if cancel_ref.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return Err(SyncError::Cancelled {
                            operation: format!("pull from {peer_id}"),
                        });
                    }
                    let remote_seq = remote.seq;
                    let was_live_file = self.deletion_brake.is_enabled()
                        && remote_seq > cursor.allow_inbound_mass_deletion_until_seq
                        && remote_seq > keep_local_until_seq
                        && self
                            .store
                            .get_record(&remote.path)?
                            .is_some_and(|r| !r.deleted && r.kind == EntryKind::File);
                    let completed = self.process_one_remote(
                        peer,
                        &remote,
                        page_incarnation,
                        prev_chain_fp_for(remote_seq, &chain_fps, &cursor),
                        keep_local_until_seq,
                        fetch_opts,
                        now,
                        &mut report,
                        &mut page_held_back,
                        &mut existing_held_by_key,
                        &mut known_held_keys,
                        &mut newly_held_back_this_pull,
                        &mut held_back_full,
                    )?;
                    if was_live_file
                        && completed
                        && self
                            .store
                            .get_record(&remote.path)?
                            .is_some_and(|r| r.deleted)
                        && !cursor
                            .inbound_binned_in_window
                            .iter()
                            .any(|(p, _)| p == &remote.path)
                    {
                        cursor
                            .inbound_binned_in_window
                            .push((remote.path.clone(), now));
                    }
                    seq_results.insert(remote_seq, completed);
                    if !completed {
                        any_deferred = true;
                        deferred_files += 1;
                    }
                    remaining_files = remaining_files.saturating_sub(1);
                    self.update_progress(
                        &peer_id,
                        remaining_files + deferred_files,
                        remaining_bytes + deferred_bytes,
                    );
                    if held_back_full {
                        break;
                    }
                }
            }

            // Phase 2: apply live directory records shallowest-first before child files.
            if !held_back_full {
                for remote in live_dir_records {
                    if cancel_ref.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return Err(SyncError::Cancelled {
                            operation: format!("pull from {peer_id}"),
                        });
                    }
                    let remote_seq = remote.seq;
                    let completed = self.process_one_remote(
                        peer,
                        &remote,
                        page_incarnation,
                        prev_chain_fp_for(remote_seq, &chain_fps, &cursor),
                        keep_local_until_seq,
                        fetch_opts,
                        now,
                        &mut report,
                        &mut page_held_back,
                        &mut existing_held_by_key,
                        &mut known_held_keys,
                        &mut newly_held_back_this_pull,
                        &mut held_back_full,
                    )?;
                    seq_results.insert(remote_seq, completed);
                    if !completed {
                        any_deferred = true;
                        deferred_files += 1;
                    }
                    remaining_files = remaining_files.saturating_sub(1);
                    self.update_progress(
                        &peer_id,
                        remaining_files + deferred_files,
                        remaining_bytes + deferred_bytes,
                    );
                    if held_back_full {
                        break;
                    }
                }
            }

            // Phase 3: partition clean brand-new regular files and commit in ONE group-commit
            // transaction BEFORE Phase 4 processes any remaining live records.
            let mut batch_new_files: Vec<IndexRecord> = Vec::new();
            let mut batch_raw_by_seq: HashMap<u64, IndexRecord> = HashMap::new();
            let mut other_live_records: Vec<IndexRecord> = Vec::new();
            if !held_back_full {
                let cur_meta = self.meta()?;
                let caps = self.fs.capabilities();
                let view = StoreIndexView::new(&self.store);
                let mut seen_batch_paths: BTreeSet<String> = BTreeSet::new();
                let mut seen_batch_equiv: BTreeSet<String> = BTreeSet::new();
                let mut seen_batch_dir_equiv: BTreeSet<String> = BTreeSet::new();
                for remote in live_non_dir_records {
                    if remote.kind == EntryKind::File
                        && !self.is_blocked_by_skipped(&remote.path)
                        && !seen_batch_paths.contains(&remote.path)
                    {
                        let decision = reconcile_record(
                            &view,
                            &remote,
                            &peer_id,
                            &cur_meta,
                            &caps,
                            &self.ignore,
                        );
                        if let ReconcileDecision::Fetch { mut record, .. } = decision {
                            let local_absent =
                                view.get_record(&record.path).is_none_or(|r| r.deleted);
                            let leaf_equiv = crate::path::fs_equivalence_key(&record.path, &caps);
                            let mut anc_clash = seen_batch_equiv.contains(&leaf_equiv)
                                || seen_batch_dir_equiv.contains(&leaf_equiv);
                            let mut anc_keys = Vec::new();
                            for anc in crate::path::ancestors(&record.path) {
                                let k = crate::path::fs_equivalence_key(anc, &caps);
                                if seen_batch_equiv.contains(&k) {
                                    anc_clash = true;
                                }
                                if seen_batch_dir_equiv.contains(&k)
                                    && !seen_batch_paths.contains(anc)
                                {
                                    anc_clash = true;
                                }
                                anc_keys.push((anc.to_string(), k));
                            }
                            let disk_absent = match self.fs.stat_live(&record.path) {
                                Ok(None) => true,
                                Ok(Some(_)) => false,
                                Err(SyncError::Fs {
                                    kind: crate::error::FsErrorKind::Busy,
                                    ..
                                }) => false,
                                Err(SyncError::Fs {
                                    kind: crate::error::FsErrorKind::PermissionDenied,
                                    ..
                                }) if caps.windows_naming => false,
                                Err(err) => return Err(err),
                            };
                            if local_absent && !anc_clash && disk_absent {
                                record.seq = remote.seq;
                                batch_raw_by_seq.insert(remote.seq, remote.for_wire());
                                seen_batch_paths.insert(record.path.clone());
                                seen_batch_equiv.insert(leaf_equiv);
                                for (anc_exact, anc_k) in anc_keys {
                                    seen_batch_paths.insert(anc_exact);
                                    seen_batch_dir_equiv.insert(anc_k);
                                }
                                batch_new_files.push(record);
                                continue;
                            }
                        }
                    }
                    other_live_records.push(remote);
                }
            }

            if !held_back_full && !batch_new_files.is_empty() {
                let batch_len = batch_new_files.len() as u64;
                let batch_bytes: u64 = batch_new_files.iter().map(|r| r.size).sum();
                let batch_res = apply_new_files_group_commit(
                    &self.fs,
                    &mut self.store,
                    peer,
                    batch_new_files,
                    fetch_opts,
                    now,
                )?;
                let mut remove_deferrals = Vec::new();
                if !batch_res.applied_seqs.is_empty() {
                    let peer_def_paths: BTreeSet<String> = self
                        .store
                        .list_deferrals()?
                        .into_iter()
                        .filter(|d| d.peer == peer_id)
                        .map(|d| d.path)
                        .collect();
                    for seq in batch_res.applied_seqs {
                        seq_results.insert(seq, true);
                        report.applied += 1;
                        if !peer_def_paths.is_empty()
                            && let Some(raw) = batch_raw_by_seq.get(&seq)
                            && peer_def_paths.contains(&raw.path)
                        {
                            remove_deferrals.push((peer_id.clone(), raw.path.clone()));
                        }
                    }
                    if !remove_deferrals.is_empty() {
                        self.store.apply_batch(StoreBatch {
                            remove_deferrals,
                            ..StoreBatch::default()
                        })?;
                    }
                }
                for (seq, path, problem) in batch_res.held_back_local {
                    if let Some(raw) = batch_raw_by_seq.get(&seq) {
                        if !self.queue_held_back_bounded(
                            &peer_id,
                            raw.for_wire(),
                            page_incarnation,
                            seq,
                            prev_chain_fp_for(seq, &chain_fps, &cursor),
                            problem,
                            false,
                            &mut page_held_back,
                            &mut existing_held_by_key,
                            &mut known_held_keys,
                        )? {
                            held_back_full = true;
                            seq_results.insert(seq, false);
                            any_deferred = true;
                            deferred_files += 1;
                            break;
                        }
                        newly_held_back_this_pull.insert(path);
                        report.held_back += 1;
                        self.clear_deferred(&peer_id, &raw.path)?;
                        seq_results.insert(seq, true);
                    } else {
                        seq_results.insert(seq, false);
                        any_deferred = true;
                        deferred_files += 1;
                    }
                }
                if !held_back_full {
                    for (seq, path, reason) in batch_res.deferred {
                        if let Some(raw) = batch_raw_by_seq.get(&seq) {
                            let completed = self.record_deferral(
                                &peer_id,
                                raw,
                                page_incarnation,
                                prev_chain_fp_for(seq, &chain_fps, &cursor),
                                reason,
                                &mut report,
                                &mut page_held_back,
                                &mut existing_held_by_key,
                                &mut known_held_keys,
                                &mut held_back_full,
                            )?;
                            seq_results.insert(seq, completed);
                            if !completed {
                                any_deferred = true;
                                deferred_files += 1;
                            }
                            if held_back_full {
                                break;
                            }
                        } else {
                            seq_results.insert(seq, false);
                            any_deferred = true;
                            deferred_files += 1;
                            let _ = path;
                        }
                    }
                }
                if !held_back_full {
                    for fallback in batch_res.fallback_records {
                        let remote_seq = fallback.seq;
                        let completed = self.process_one_remote(
                            peer,
                            &fallback,
                            page_incarnation,
                            prev_chain_fp_for(remote_seq, &chain_fps, &cursor),
                            keep_local_until_seq,
                            fetch_opts,
                            now,
                            &mut report,
                            &mut page_held_back,
                            &mut existing_held_by_key,
                            &mut known_held_keys,
                            &mut newly_held_back_this_pull,
                            &mut held_back_full,
                        )?;
                        seq_results.insert(remote_seq, completed);
                        if !completed {
                            any_deferred = true;
                            deferred_files += 1;
                        }
                        if held_back_full {
                            break;
                        }
                    }
                }
                remaining_files = remaining_files.saturating_sub(batch_len);
                remaining_bytes = remaining_bytes.saturating_sub(batch_bytes);
                self.update_progress(
                    &peer_id,
                    remaining_files + deferred_files,
                    remaining_bytes + deferred_bytes,
                );
            }

            // Phase 4: apply remaining live records.
            if !held_back_full {
                for remote in other_live_records {
                    if cancel_ref.is_some_and(|c| c.load(Ordering::Relaxed)) {
                        return Err(SyncError::Cancelled {
                            operation: format!("pull from {peer_id}"),
                        });
                    }
                    let remote_seq = remote.seq;
                    let remote_bytes = if !remote.deleted && remote.kind == EntryKind::File {
                        remote.size
                    } else {
                        0
                    };

                    let completed = self.process_one_remote(
                        peer,
                        &remote,
                        page_incarnation,
                        prev_chain_fp_for(remote_seq, &chain_fps, &cursor),
                        keep_local_until_seq,
                        fetch_opts,
                        now,
                        &mut report,
                        &mut page_held_back,
                        &mut existing_held_by_key,
                        &mut known_held_keys,
                        &mut newly_held_back_this_pull,
                        &mut held_back_full,
                    )?;
                    seq_results.insert(remote_seq, completed);
                    if !completed {
                        any_deferred = true;
                        deferred_files += 1;
                        deferred_bytes += remote_bytes;
                    }

                    remaining_files = remaining_files.saturating_sub(1);
                    remaining_bytes = remaining_bytes.saturating_sub(remote_bytes);
                    self.update_progress(
                        &peer_id,
                        remaining_files + deferred_files,
                        remaining_bytes + deferred_bytes,
                    );
                    if held_back_full {
                        break;
                    }
                }
            }

            // Flush any batched held-back records from this page in ONE transaction!
            if !page_held_back.is_empty() {
                let batch = StoreBatch {
                    upsert_held_back: page_held_back.into_values().collect(),
                    ..StoreBatch::default()
                };
                self.store.apply_batch(batch)?;
            }

            // Compute highest contiguous pulled `seq`.
            let mut contiguous_seq = since_seq;
            let mut all_done = !held_back_full;
            for (seq, ok) in &seq_results {
                if *ok {
                    contiguous_seq = contiguous_seq.max(*seq);
                } else {
                    all_done = false;
                    break;
                }
            }

            let held_now = self.store.list_held_back()?;
            let defs_now = self.store.list_deferrals()?;

            cursor.incarnation = page_incarnation;
            cursor.last_seen_snapshot_seq = cursor.last_seen_snapshot_seq.max(page.snapshot_seq);
            cursor.last_synced_at = Some(now);
            cursor.last_error = None;
            cursor.last_error_at = None;
            if all_done && !page.has_more {
                cursor.seq = cursor.seq.max(page.snapshot_seq);
                if let Some(&fp) = chain_fps.get(&cursor.seq) {
                    cursor.head_fingerprint = Some(fp);
                } else if page.head_fingerprint.is_some() {
                    cursor.head_fingerprint = page.head_fingerprint;
                }
                recompute_peer_applied_ack(&peer_id, &mut cursor, &held_now, &defs_now);
                let clear_rejoin = self.peer_caught_up_for_rejoin(&local_meta, &cursor)?;
                self.persist_peer_cursor_and_clear_rejoin(&peer_id, cursor, clear_rejoin)?;
                self.active_progress.remove(&peer_id);
                self.prune_acknowledged_metadata_inner()?;
                break;
            } else {
                if contiguous_seq > cursor.seq {
                    cursor.seq = contiguous_seq;
                    if let Some(&fp) = chain_fps.get(&contiguous_seq) {
                        cursor.head_fingerprint = Some(fp);
                    }
                }
                recompute_peer_applied_ack(&peer_id, &mut cursor, &held_now, &defs_now);
                self.persist_peer_cursor_and_clear_rejoin(&peer_id, cursor, false)?;
                self.prune_acknowledged_metadata_inner()?;
                if held_back_full || any_deferred || !all_done || contiguous_seq == since_seq {
                    self.update_progress(&peer_id, deferred_files, deferred_bytes);
                    break;
                }
            }
        }

        report.held_back_resolved +=
            self.retry_held_back_for_peer(peer, fetch_opts, &newly_held_back_this_pull, now)?;
        Ok(report)
    }

    fn do_handshake(
        &mut self,
        peer: &dyn Peer,
        peer_id: &str,
        meta: &FolderMeta,
        all_cursors: &BTreeMap<MemberId, PeerCursor>,
    ) -> Result<PeerHandshake> {
        let cursor = all_cursors.get(peer_id);
        let (peer_pulled, peer_acked) = match cursor.filter(|c| c.incarnation != 0) {
            Some(c) => (
                Some(PeerAckView {
                    incarnation: c.incarnation,
                    seq: c.seq,
                    fingerprint: c.head_fingerprint,
                }),
                Some(PeerAckView {
                    incarnation: c.incarnation,
                    seq: c.applied_ack_seq,
                    fingerprint: c.acked_fingerprint,
                }),
            ),
            None => (None, None),
        };
        let hello = HandshakeHello {
            folder_id: meta.folder_id.clone(),
            member_id: meta.local_member.clone(),
            incarnation: meta.incarnation,
            snapshot_seq: meta.local_seq,
            head_fingerprint: meta.head_fingerprint,
            protocol_version: SYNC_PROTOCOL_VERSION,
            peer_pulled,
            peer_acked,
            peer_rejoining: meta.rejoining_peers.contains(peer_id),
            refused_from_peer: self.refusal_summary_for_peer(peer_id)?,
        };
        let hs = match peer.handshake(&hello) {
            Ok(hs) => {
                self.unreachable_peers.remove(peer_id);
                hs
            }
            Err(SyncError::PeerUnavailable { member, detail }) => {
                self.unreachable_peers.insert(peer_id.to_string());
                return Err(SyncError::PeerUnavailable { member, detail });
            }
            Err(err) => return Err(err),
        };
        if hs.folder_id != meta.folder_id {
            return Err(SyncError::InvalidConfig {
                message: format!(
                    "peer {peer_id:?} serves folder {:?}, expected {:?}",
                    hs.folder_id, meta.folder_id
                ),
                hint: Some("Check the sync folder configuration on both machines.".to_string()),
            });
        }
        if hs.member_id != peer_id {
            return Err(SyncError::Denied {
                member: peer_id.to_string(),
                reason: format!(
                    "peer handshake claimed member {:?}, does not match expected peer",
                    hs.member_id
                ),
            });
        }
        if hs.incarnation == 0 {
            return Err(SyncError::CorruptData {
                context: format!("handshake from {peer_id}"),
                detail: "peer presented zero incarnation id".to_string(),
            });
        }
        if hs.protocol_version == 0 || hs.protocol_version > SYNC_PROTOCOL_VERSION {
            return Err(SyncError::Unsupported {
                reason: format!(
                    "peer {peer_id:?} speaks protocol version {}, supported up to {SYNC_PROTOCOL_VERSION}",
                    hs.protocol_version
                ),
                hint: Some("Upgrade Legion on both machines to compatible versions.".to_string()),
            });
        }
        Ok(hs)
    }

    /// Pairs delete + create records within one page that share an identical file hash already
    /// present on local disk so renames execute locally without transferring bytes.
    #[allow(clippy::type_complexity)]
    fn extract_local_rename_pairs(
        &self,
        peer_id: &str,
        ordered: Vec<IndexRecord>,
    ) -> Result<(
        Vec<(IndexRecord, IndexRecord, IndexRecord, IndexRecord)>,
        Vec<IndexRecord>,
    )> {
        let meta = self.meta()?;
        let caps = self.fs.capabilities();
        let view = StoreIndexView::new(&self.store);

        let mut delete_indices_by_hash: HashMap<Hash, Vec<(usize, IndexRecord)>> = HashMap::new();
        for (idx, rec) in ordered.iter().enumerate() {
            if !rec.deleted || rec.kind != EntryKind::File {
                continue;
            }
            if self.is_blocked_by_skipped(&rec.path) {
                continue;
            }
            if let ReconcileDecision::DeleteToBin { tombstone, .. } =
                reconcile_record(&view, rec, peer_id, &meta, &caps, &self.ignore)
                && let Some(local) = view.get_record(&tombstone.path)
                && !local.deleted
                && local.kind == EntryKind::File
                && let Some(local_hash) = local.hash
            {
                delete_indices_by_hash
                    .entry(local_hash)
                    .or_default()
                    .push((idx, tombstone));
            }
        }

        if delete_indices_by_hash.is_empty() {
            return Ok((Vec::new(), ordered));
        }

        let mut paired_indices: HashMap<usize, bool> = HashMap::new();
        let mut pairs = Vec::new();

        for (add_idx, rec) in ordered.iter().enumerate() {
            if rec.deleted || rec.kind != EntryKind::File {
                continue;
            }
            if self.is_blocked_by_skipped(&rec.path) {
                continue;
            }
            let Some(h) = rec.hash else { continue };
            let Some(candidates) = delete_indices_by_hash.get_mut(&h) else {
                continue;
            };
            if candidates.is_empty() {
                continue;
            }
            if let ReconcileDecision::Fetch { record: target, .. } =
                reconcile_record(&view, rec, peer_id, &meta, &caps, &self.ignore)
                && view.get_record(&target.path).is_none_or(|r| r.deleted)
                && self.fs.stat_live(&target.path)?.is_none()
            {
                let (del_idx, tombstone) = candidates.remove(0);
                if paired_indices.contains_key(&del_idx) {
                    continue;
                }
                paired_indices.insert(del_idx, true);
                paired_indices.insert(add_idx, true);
                pairs.push((ordered[del_idx].clone(), tombstone, rec.clone(), target));
            }
        }

        let remaining = ordered
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !paired_indices.contains_key(i))
            .map(|(_, r)| r)
            .collect();

        Ok((pairs, remaining))
    }

    #[allow(clippy::too_many_arguments)]
    fn process_one_remote(
        &mut self,
        peer: &dyn Peer,
        remote: &IndexRecord,
        peer_incarnation: u64,
        prev_chain_fp: Option<Hash>,
        keep_local_files_until_seq: u64,
        fetch_opts: &FetchOptions,
        now: Timestamp,
        report: &mut PullReport,
        page_held_back: &mut BTreeMap<(MemberId, String), HeldBackRecord>,
        existing_held_by_key: &mut BTreeMap<(MemberId, String), HeldBackRecord>,
        known_held_keys: &mut BTreeSet<(MemberId, String)>,
        newly_held_back_this_pull: &mut BTreeSet<String>,
        held_back_full: &mut bool,
    ) -> Result<bool> {
        let caps = self.fs.capabilities();
        let peer_id = peer.member_id().to_string();

        // If this path lies under (or would affect) an unreadable local subtree, hold it back
        // durably without acknowledging past it.
        if let Some(blocked_by) = self.skipped_blocker(&remote.path) {
            let problem = SyncProblem {
                path: remote.path.clone(),
                kind: ProblemKind::FilesystemError,
                peer: Some(peer_id.clone()),
                sentence: format!(
                    "'{}' is waiting because local path '{}' could not be read.",
                    remote.path, blocked_by
                ),
            };
            if !self.queue_held_back_bounded(
                &peer_id,
                remote.for_wire(),
                peer_incarnation,
                remote.seq,
                prev_chain_fp,
                problem,
                false,
                page_held_back,
                existing_held_by_key,
                known_held_keys,
            )? {
                *held_back_full = true;
                return Ok(false);
            }
            report.held_back += 1;
            return Ok(true);
        }

        for _attempt in 0..MAX_LOCAL_CHANGE_RETRIES {
            let cur_meta = self.meta()?;
            let mut decision = {
                let view = StoreIndexView::new(&self.store);
                reconcile_record(&view, remote, &peer_id, &cur_meta, &caps, &self.ignore)
            };
            if remote.seq <= keep_local_files_until_seq
                && let ReconcileDecision::DeleteToBin {
                    tombstone,
                    bin_reason,
                } = decision
            {
                if let Some(mut local) = self.store.get_record(&tombstone.path)?
                    && !local.deleted
                {
                    local.version.merge_in_place(&tombstone.version);
                    local.from_join = false;
                    decision = ReconcileDecision::KeepAndSupersede { record: local };
                } else {
                    decision = ReconcileDecision::DeleteToBin {
                        tombstone,
                        bin_reason,
                    };
                }
            }

            match decision {
                ReconcileDecision::RejectInvalid { path, reason } => {
                    // Item 4 & 9: refuse invalid/forged records, store durably unapplied so they
                    // are NEVER acknowledged (while pull position can still advance past them to
                    // apply valid records), and report durably!
                    let mut wire = remote.for_wire();
                    wire.path = path.clone();
                    let problem = SyncProblem {
                        path: path.clone(),
                        kind: ProblemKind::InvalidPeerRecord,
                        peer: Some(peer_id.clone()),
                        sentence: format!(
                            "Refused invalid record '{path}' from '{peer_id}': {reason}."
                        ),
                    };
                    if !self.queue_held_back_bounded(
                        &peer_id,
                        wire,
                        peer_incarnation,
                        remote.seq,
                        prev_chain_fp,
                        problem,
                        true,
                        page_held_back,
                        existing_held_by_key,
                        known_held_keys,
                    )? {
                        *held_back_full = true;
                        return Ok(false);
                    }
                    report.invalid_skipped += 1;
                    return Ok(true);
                }
                ReconcileDecision::NotYetKnownMember {
                    path,
                    member,
                    reason,
                } => {
                    // Item 4: a record mentioning a member we don't know yet is "not yet", never
                    // "invalid"; store durably unapplied so it is retried when membership arrives
                    // and NEVER acknowledged until applied.
                    let problem = SyncProblem {
                        path: path.clone(),
                        kind: ProblemKind::UnknownMember,
                        peer: Some(peer_id.clone()),
                        sentence: format!(
                            "'{path}' is waiting until this machine knows about member '{member}' ({reason})."
                        ),
                    };
                    if !self.queue_held_back_bounded(
                        &peer_id,
                        remote.for_wire(),
                        peer_incarnation,
                        remote.seq,
                        prev_chain_fp,
                        problem,
                        false,
                        page_held_back,
                        existing_held_by_key,
                        known_held_keys,
                    )? {
                        *held_back_full = true;
                        return Ok(false);
                    }
                    report.held_back += 1;
                    self.clear_deferred(&peer_id, &remote.path)?;
                    return Ok(true);
                }
                ReconcileDecision::Ignore => {
                    report.ignored += 1;
                    let key = (peer_id.clone(), remote.path.clone());
                    page_held_back.remove(&key);
                    existing_held_by_key.remove(&key);
                    if known_held_keys.remove(&key) {
                        self.clear_held_back_if_present(&peer_id, &remote.path)?;
                    }
                    self.clear_deferred(&peer_id, &remote.path)?;
                    return Ok(true);
                }
                ReconcileDecision::Keep => {
                    report.kept += 1;
                    let key = (peer_id.clone(), remote.path.clone());
                    page_held_back.remove(&key);
                    existing_held_by_key.remove(&key);
                    if known_held_keys.remove(&key) {
                        self.clear_held_back_if_present(&peer_id, &remote.path)?;
                    }
                    self.clear_deferred(&peer_id, &remote.path)?;
                    return Ok(true);
                }
                ReconcileDecision::HoldBack { problem } => {
                    if !self.queue_held_back_bounded(
                        &peer_id,
                        remote.for_wire(),
                        peer_incarnation,
                        remote.seq,
                        prev_chain_fp,
                        problem,
                        false,
                        page_held_back,
                        existing_held_by_key,
                        known_held_keys,
                    )? {
                        *held_back_full = true;
                        return Ok(false);
                    }
                    report.held_back += 1;
                    self.clear_deferred(&peer_id, &remote.path)?;
                    return Ok(true);
                }
                other => {
                    match apply_decision(
                        &self.fs,
                        &mut self.store,
                        peer,
                        other,
                        &self.ignore,
                        fetch_opts,
                        now,
                    )? {
                        ApplyOutcome::Applied => {
                            report.applied += 1;
                            let key = (peer_id.clone(), remote.path.clone());
                            page_held_back.remove(&key);
                            existing_held_by_key.remove(&key);
                            known_held_keys.remove(&key);
                            self.clear_deferred(&peer_id, &remote.path)?;
                            return Ok(true);
                        }
                        ApplyOutcome::DirKeptLocalFiles(problem) => {
                            report.applied += 1;
                            let key = (peer_id.clone(), remote.path.clone());
                            page_held_back.remove(&key);
                            existing_held_by_key.remove(&key);
                            known_held_keys.remove(&key);
                            self.store.apply_batch(StoreBatch {
                                upsert_problems: vec![(
                                    dir_kept_problem_key(&problem.path),
                                    problem,
                                )],
                                ..StoreBatch::default()
                            })?;
                            self.clear_deferred(&peer_id, &remote.path)?;
                            return Ok(true);
                        }
                        ApplyOutcome::LocalChangedRetry => {
                            continue;
                        }
                        ApplyOutcome::Deferred { reason } => {
                            return self.record_deferral(
                                &peer_id,
                                remote,
                                peer_incarnation,
                                prev_chain_fp,
                                reason,
                                report,
                                page_held_back,
                                existing_held_by_key,
                                known_held_keys,
                                held_back_full,
                            );
                        }
                        ApplyOutcome::HoldBackLocal(problem) => {
                            if !self.queue_held_back_bounded(
                                &peer_id,
                                remote.for_wire(),
                                peer_incarnation,
                                remote.seq,
                                prev_chain_fp,
                                problem,
                                false,
                                page_held_back,
                                existing_held_by_key,
                                known_held_keys,
                            )? {
                                *held_back_full = true;
                                return Ok(false);
                            }
                            newly_held_back_this_pull.insert(remote.path.clone());
                            report.held_back += 1;
                            self.clear_deferred(&peer_id, &remote.path)?;
                            return Ok(true);
                        }
                    }
                }
            }
        }

        self.record_deferral(
            &peer_id,
            remote,
            peer_incarnation,
            prev_chain_fp,
            format!("'{}' kept changing locally during sync", remote.path),
            report,
            page_held_back,
            existing_held_by_key,
            known_held_keys,
            held_back_full,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn record_deferral(
        &mut self,
        peer_id: &str,
        remote: &IndexRecord,
        peer_incarnation: u64,
        prev_chain_fp: Option<Hash>,
        reason: String,
        report: &mut PullReport,
        page_held_back: &mut BTreeMap<(MemberId, String), HeldBackRecord>,
        existing_held_by_key: &mut BTreeMap<(MemberId, String), HeldBackRecord>,
        known_held_keys: &mut BTreeSet<(MemberId, String)>,
        held_back_full: &mut bool,
    ) -> Result<bool> {
        report.deferred += 1;
        let existing = self
            .store
            .list_deferrals()?
            .into_iter()
            .find(|d| d.peer == peer_id && d.path == remote.path);
        let attempts = existing.map_or(1, |d| d.attempts.saturating_add(1));

        // Item 4: once a record hits MAX_DEFERRAL_ATTEMPTS, promote it to a durable held-back
        // unapplied record (never abandoned and never acknowledged past its `peer_seq`!) so other
        // records in the page can still be pulled while the problem stays visible and retried.
        if attempts >= MAX_DEFERRAL_ATTEMPTS {
            let problem = SyncProblem {
                path: remote.path.clone(),
                kind: ProblemKind::DeferredLimitExceeded,
                peer: Some(peer_id.to_string()),
                sentence: format!(
                    "'{}' could not be synced after {attempts} attempts: {reason}.",
                    remote.path
                ),
            };
            if !self.queue_held_back_bounded(
                peer_id,
                remote.for_wire(),
                peer_incarnation,
                remote.seq,
                prev_chain_fp,
                problem,
                false,
                page_held_back,
                existing_held_by_key,
                known_held_keys,
            )? {
                *held_back_full = true;
                return Ok(false);
            }
            self.clear_deferred(peer_id, &remote.path)?;
            return Ok(true);
        }

        let item = DeferredItem {
            peer: peer_id.to_string(),
            path: remote.path.clone(),
            reason,
            attempts,
            peer_incarnation,
            peer_seq: remote.seq,
            prev_chain_fingerprint: prev_chain_fp,
        };
        self.store.apply_batch(StoreBatch {
            upsert_deferrals: vec![item],
            ..StoreBatch::default()
        })?;
        Ok(false)
    }

    fn clear_deferred(&mut self, peer_id: &str, path: &str) -> Result<()> {
        let has_it = self
            .store
            .list_deferrals()?
            .iter()
            .any(|d| d.peer == peer_id && d.path == path);
        if has_it {
            self.store.apply_batch(StoreBatch {
                remove_deferrals: vec![(peer_id.to_string(), path.to_string())],
                ..StoreBatch::default()
            })?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn queue_held_back_bounded(
        &mut self,
        peer_id: &str,
        record: IndexRecord,
        peer_incarnation: u64,
        peer_seq: u64,
        prev_chain_fp: Option<Hash>,
        problem: SyncProblem,
        preserve_earliest_seq: bool,
        page_held_back: &mut BTreeMap<(MemberId, String), HeldBackRecord>,
        existing_held_by_key: &mut BTreeMap<(MemberId, String), HeldBackRecord>,
        known_held_keys: &mut BTreeSet<(MemberId, String)>,
    ) -> Result<bool> {
        let key = (peer_id.to_string(), record.path.clone());
        let already_present = known_held_keys.contains(&key);
        let peer_held_count = known_held_keys.iter().filter(|(p, _)| p == peer_id).count();
        if !already_present && peer_held_count >= MAX_HELD_BACK_ENTRIES {
            // Item 4: per-peer held-back limit; NEVER drop a held-back record, and stop pulling
            // only from this peer with a visible durable problem naming it!
            self.record_peer_problem(
                peer_id,
                SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::HeldBackLimitReached,
                    peer: Some(peer_id.to_string()),
                    sentence: format!(
                        "Stopped pulling from '{peer_id}' because the unapplied problem list for '{peer_id}' reached its limit of {MAX_HELD_BACK_ENTRIES} entries; resolve existing problems to continue."
                    ),
                },
            )?;
            return Ok(false);
        }

        // If preserving the earliest unapplied invalid seq for a colliding key on the same
        // incarnation, keep the smaller `peer_seq` so `applied_ack_seq` never advances past it.
        let (effective_seq, effective_prev_fp) = if preserve_earliest_seq
            && let Some(prev) = page_held_back
                .get(&key)
                .or_else(|| existing_held_by_key.get(&key))
            && prev.peer_incarnation == peer_incarnation
            && prev.peer_seq > 0
            && prev.peer_seq < peer_seq
        {
            (prev.peer_seq, prev.prev_chain_fingerprint)
        } else {
            (peer_seq, prev_chain_fp)
        };

        let entry = HeldBackRecord {
            record,
            from_peer: peer_id.to_string(),
            problem,
            peer_incarnation,
            peer_seq: effective_seq,
            prev_chain_fingerprint: effective_prev_fp,
        };
        known_held_keys.insert(key.clone());
        existing_held_by_key.insert(key.clone(), entry.clone());
        page_held_back.insert(key, entry);
        Ok(true)
    }

    fn record_peer_problem(&mut self, peer_id: &str, problem: SyncProblem) -> Result<()> {
        let existing = self.store.list_problems()?;
        let peer_count = existing
            .iter()
            .filter(|(_, p)| p.peer.as_deref() == Some(peer_id))
            .count();
        let key = peer_problem_key(peer_id, problem.kind, &problem.path);
        let already_exists = existing.iter().any(|(k, _)| k == &key);
        if !already_exists && peer_count >= MAX_PEER_PROBLEMS {
            return Ok(());
        }
        self.store.apply_batch(StoreBatch {
            upsert_problems: vec![(key, problem)],
            ..StoreBatch::default()
        })
    }

    fn skipped_blocker(&self, path: &str) -> Option<String> {
        for skip in &self.skipped_paths {
            if skip.is_empty()
                || path == skip
                || is_descendant_of(path, skip)
                || is_descendant_of(skip, path)
            {
                return Some(skip.clone());
            }
        }
        None
    }

    fn is_blocked_by_skipped(&self, path: &str) -> bool {
        self.skipped_blocker(path).is_some()
    }

    /// Retries all durable unapplied records that can be resolved locally without fetching file
    /// bytes from a peer (deletions, metadata merges, superseding tombstones, directory revivals,
    /// and now-obsolete records), updating each affected peer's `applied_ack_seq`.
    pub fn retry_held_back_local(&mut self, now: Timestamp) -> Result<usize> {
        if self.store.held_back_count()? == 0 {
            return Ok(0);
        }
        let held = self.store.list_held_back()?;
        let caps = self.fs.capabilities();
        let mut resolved = 0usize;
        let mut touched_peers: BTreeSet<MemberId> = BTreeSet::new();

        for item in held {
            if self.is_blocked_by_skipped(&item.record.path) {
                continue;
            }
            let cur_meta = self.meta()?;
            let decision = {
                let view = StoreIndexView::new(&self.store);
                reconcile_record(
                    &view,
                    &item.record,
                    &item.from_peer,
                    &cur_meta,
                    &caps,
                    &self.ignore,
                )
            };
            match decision {
                ReconcileDecision::HoldBack { .. }
                | ReconcileDecision::RejectInvalid { .. }
                | ReconcileDecision::NotYetKnownMember { .. } => {}
                ReconcileDecision::Keep | ReconcileDecision::Ignore => {
                    self.clear_held_back_if_present(&item.from_peer, &item.record.path)?;
                    touched_peers.insert(item.from_peer);
                    resolved += 1;
                }
                local_only @ (ReconcileDecision::DeleteToBin { .. }
                | ReconcileDecision::MetadataOnly { .. }
                | ReconcileDecision::KeepAndSupersede { .. }
                | ReconcileDecision::ReviveDir { .. }) => {
                    let peer_stub = LocalOnlyPeer(&item.from_peer);
                    let fetch_opts = FetchOptions::default();
                    let effective_now = now.max(item.record.mtime);
                    match apply_decision(
                        &self.fs,
                        &mut self.store,
                        &peer_stub,
                        local_only,
                        &self.ignore,
                        &fetch_opts,
                        effective_now,
                    ) {
                        Ok(
                            outcome @ (ApplyOutcome::Applied | ApplyOutcome::DirKeptLocalFiles(_)),
                        ) => {
                            if let ApplyOutcome::DirKeptLocalFiles(prob) = outcome {
                                self.store.apply_batch(StoreBatch {
                                    upsert_problems: vec![(dir_kept_problem_key(&prob.path), prob)],
                                    ..StoreBatch::default()
                                })?;
                            }
                            self.clear_held_back_if_present(&item.from_peer, &item.record.path)?;
                            touched_peers.insert(item.from_peer);
                            resolved += 1;
                        }
                        Err(err)
                            if err.is_crash()
                                || matches!(
                                    err,
                                    SyncError::Storage { .. }
                                        | SyncError::IndexDamaged { .. }
                                        | SyncError::FolderUnavailable { .. }
                                        | SyncError::Cancelled { .. }
                                ) =>
                        {
                            return Err(err);
                        }
                        Ok(_) | Err(_) => {}
                    }
                }
                ReconcileDecision::Fetch { .. }
                | ReconcileDecision::ConflictLocalLoses { .. }
                | ReconcileDecision::ConflictRemoteLoses { .. } => {
                    // Needs peer file transfer; will resolve on next pull from `item.from_peer`.
                }
            }
        }

        if resolved > 0 {
            self.recompute_acks_for_peers(&touched_peers)?;
        }
        Ok(resolved)
    }

    fn retry_held_back_for_peer(
        &mut self,
        peer: &dyn Peer,
        fetch_opts: &FetchOptions,
        skip_paths: &BTreeSet<String>,
        now: Timestamp,
    ) -> Result<usize> {
        if self.store.held_back_count()? == 0 {
            return Ok(0);
        }
        let peer_id = peer.member_id().to_string();
        let held = self.store.list_held_back()?;
        let mut resolved = 0usize;
        let caps = self.fs.capabilities();

        for mut item in held {
            if item.from_peer != peer_id {
                continue;
            }
            if skip_paths.contains(&item.record.path) {
                continue;
            }
            if self.is_blocked_by_skipped(&item.record.path) {
                continue;
            }
            let cur_meta = self.meta()?;
            let decision = {
                let view = StoreIndexView::new(&self.store);
                reconcile_record(
                    &view,
                    &item.record,
                    &peer_id,
                    &cur_meta,
                    &caps,
                    &self.ignore,
                )
            };
            match decision {
                ReconcileDecision::HoldBack { .. }
                | ReconcileDecision::RejectInvalid { .. }
                | ReconcileDecision::NotYetKnownMember { .. } => {}
                ReconcileDecision::Keep | ReconcileDecision::Ignore => {
                    self.clear_held_back_if_present(&peer_id, &item.record.path)?;
                    resolved += 1;
                }
                other => {
                    match apply_decision(
                        &self.fs,
                        &mut self.store,
                        peer,
                        other,
                        &self.ignore,
                        fetch_opts,
                        now,
                    ) {
                        Ok(
                            outcome @ (ApplyOutcome::Applied | ApplyOutcome::DirKeptLocalFiles(_)),
                        ) => {
                            if let ApplyOutcome::DirKeptLocalFiles(prob) = outcome {
                                self.store.apply_batch(StoreBatch {
                                    upsert_problems: vec![(dir_kept_problem_key(&prob.path), prob)],
                                    ..StoreBatch::default()
                                })?;
                            }
                            self.clear_held_back_if_present(&peer_id, &item.record.path)?;
                            resolved += 1;
                        }
                        Ok(ApplyOutcome::HoldBackLocal(updated_prob)) => {
                            if item.problem != updated_prob {
                                item.problem = updated_prob;
                                self.store.apply_batch(StoreBatch {
                                    upsert_held_back: vec![item],
                                    ..StoreBatch::default()
                                })?;
                            }
                        }
                        Ok(ApplyOutcome::LocalChangedRetry | ApplyOutcome::Deferred { .. }) => {}
                        Err(err)
                            if err.is_crash()
                                || matches!(
                                    err,
                                    SyncError::Storage { .. }
                                        | SyncError::IndexDamaged { .. }
                                        | SyncError::FolderUnavailable { .. }
                                        | SyncError::Cancelled { .. }
                                ) =>
                        {
                            return Err(err);
                        }
                        Err(_) => {}
                    }
                }
            }
        }
        if resolved > 0 {
            let mut set = BTreeSet::new();
            set.insert(peer_id);
            self.recompute_acks_for_peers(&set)?;
        }
        Ok(resolved)
    }

    fn recompute_acks_for_peers(&mut self, peers: &BTreeSet<MemberId>) -> Result<()> {
        let held = self.store.list_held_back()?;
        let defs = self.store.list_deferrals()?;
        let cursors = self.store.load_peer_cursors()?;
        let mut batch = StoreBatch::default();
        for peer_id in peers {
            if let Some(mut c) = cursors.get(peer_id).cloned() {
                let before = c.clone();
                recompute_peer_applied_ack(peer_id, &mut c, &held, &defs);
                if c != before {
                    batch.set_peer_cursors.insert(peer_id.clone(), c);
                }
            }
            let peer_held = held.iter().filter(|h| h.from_peer == *peer_id).count();
            if peer_held < MAX_HELD_BACK_ENTRIES {
                batch.remove_problems.push(peer_problem_key(
                    peer_id,
                    ProblemKind::HeldBackLimitReached,
                    "",
                ));
            }
        }
        if !batch.is_empty() {
            self.store.apply_batch(batch)?;
        }
        Ok(())
    }

    fn clear_held_back_if_present(&mut self, peer_id: &str, path: &str) -> Result<()> {
        let held = self.store.list_held_back()?;
        if held
            .iter()
            .any(|h| h.from_peer == peer_id && h.record.path == path)
        {
            let mut batch = StoreBatch::default();
            batch
                .remove_held_back
                .push((peer_id.to_string(), path.to_string()));
            self.store.apply_batch(batch)?;
        }
        Ok(())
    }

    fn observe_peer_incarnation(&mut self, peer_id: &str, new_inc: u64) -> Result<PeerCursor> {
        let cursors = self.store.load_peer_cursors()?;
        self.observe_peer_incarnation_with(peer_id, new_inc, cursors)
    }

    fn observe_peer_incarnation_with(
        &mut self,
        peer_id: &str,
        new_inc: u64,
        cursors: BTreeMap<MemberId, PeerCursor>,
    ) -> Result<PeerCursor> {
        let mut cursor = cursors.get(peer_id).cloned().unwrap_or_default();
        if cursor.alternation_detected {
            return Err(SyncError::Conflict {
                path: peer_id.to_string(),
                reason: duplicate_member_sentence(peer_id),
            });
        }
        if cursor.duplicate_detected {
            if new_inc == cursor.incarnation {
                return Err(SyncError::Conflict {
                    path: peer_id.to_string(),
                    reason: peer_rollback_sentence(peer_id),
                });
            }
            if cursor.prior_incarnations.contains(&new_inc) {
                self.flag_duplicate_member(peer_id, cursor, true)?;
                return Err(SyncError::Conflict {
                    path: peer_id.to_string(),
                    reason: duplicate_member_sentence(peer_id),
                });
            }
            // A rolled-back machine that diverged just rotated to a brand-new unseen incarnation:
            // clear the one-time refusal and record its old incarnation in `prior_incarnations` so
            // any live clone returning with the old incarnation trips permanent
            // `alternation_detected` immediately!
            cursor.duplicate_detected = false;
            self.store.apply_batch(StoreBatch {
                remove_problems: vec![peer_problem_key(
                    peer_id,
                    ProblemKind::DuplicateMemberId,
                    "",
                )],
                ..StoreBatch::default()
            })?;
        }
        if cursor.incarnation == 0 {
            cursor.incarnation = new_inc;
            let mut batch = StoreBatch::default();
            batch
                .set_peer_cursors
                .insert(peer_id.to_string(), cursor.clone());
            self.store.apply_batch(batch)?;
            return Ok(cursor);
        }
        if cursor.incarnation == new_inc {
            return Ok(cursor);
        }

        // Peer presented a DIFFERENT incarnation from its current cursor!
        // If we have seen `new_inc` before, two live machines are alternating under `peer_id`!
        if cursor.prior_incarnations.contains(&new_inc) {
            self.flag_duplicate_member(peer_id, cursor, true)?;
            return Err(SyncError::Conflict {
                path: peer_id.to_string(),
                reason: duplicate_member_sentence(peer_id),
            });
        }

        // Legitimate new incarnation (reinstall or rollback recovery): a new incarnation of a peer
        // is a new peer for every purpose. Retire everything remembered from its previous life at
        // once, including its acknowledgements of us (so stale acks never count toward tombstone
        // GC and an honest "no position yet" is never read as a regression).
        let old_inc = cursor.incarnation;
        if !cursor.prior_incarnations.contains(&old_inc) {
            cursor.prior_incarnations.push(old_inc);
            if cursor.prior_incarnations.len() > MAX_PRIOR_INCARNATIONS {
                cursor.prior_incarnations.remove(0);
            }
        }
        cursor.incarnation = new_inc;
        cursor.seq = 0;
        cursor.applied_ack_seq = 0;
        cursor.acked_local_incarnation = 0;
        cursor.acked_local_seq = 0;
        cursor.head_fingerprint = None;
        cursor.acked_fingerprint = None;
        cursor.duplicate_detected = false;
        cursor.inbound_window_start = None;
        cursor.inbound_window_start_live_files = 0;
        cursor.inbound_binned_in_window.clear();
        cursor.inbound_brake = None;
        cursor.allow_inbound_mass_deletion_until_seq = 0;
        cursor.keep_local_files_until_seq = 0;
        cursor.last_seen_snapshot_seq = 0;
        cursor.last_error = None;
        cursor.last_error_at = None;

        let remove_held_back: Vec<(MemberId, String)> = self
            .store
            .list_held_back()?
            .into_iter()
            .filter(|h| h.from_peer == peer_id)
            .map(|h| (h.from_peer, h.record.path))
            .collect();
        let remove_deferrals: Vec<(MemberId, String)> = self
            .store
            .list_deferrals()?
            .into_iter()
            .filter(|d| d.peer == peer_id)
            .map(|d| (d.peer, d.path))
            .collect();

        let needs_rollback_confirm = self
            .meta()
            .ok()
            .and_then(|m| m.rollback_claims_by_peer.get(peer_id).cloned())
            .is_some_and(|s| {
                !s.ignored && s.rotations_since_confirm >= MAX_FORCED_ROTATIONS_BEFORE_CONFIRM
            });
        let mut remove_problems = vec![
            peer_problem_key(peer_id, ProblemKind::DuplicateMemberId, ""),
            peer_problem_key(peer_id, ProblemKind::HeldBackLimitReached, ""),
            peer_problem_key(peer_id, ProblemKind::InboundDeletionBrakeTripped, ""),
            peer_problem_key(peer_id, ProblemKind::PeerRefusedRecords, ""),
            peer_problem_key(peer_id, ProblemKind::InvalidPeerRecord, ""),
        ];
        if !needs_rollback_confirm {
            remove_problems.push(peer_problem_key(
                peer_id,
                ProblemKind::PeerRollbackRateLimited,
                "",
            ));
        }

        let mut batch = StoreBatch {
            remove_held_back,
            remove_deferrals,
            remove_problems,
            ..StoreBatch::default()
        };
        batch
            .set_peer_cursors
            .insert(peer_id.to_string(), cursor.clone());
        self.store.apply_batch(batch)?;
        if let Ok(mut refusals) = self.peer_refusals_from_serve.lock() {
            refusals.remove(peer_id);
        }
        Ok(cursor)
    }

    fn flag_duplicate_member(
        &mut self,
        peer_id: &str,
        mut cursor: PeerCursor,
        permanent_alternation: bool,
    ) -> Result<()> {
        cursor.duplicate_detected = true;
        if permanent_alternation {
            cursor.alternation_detected = true;
        }
        let is_alternation = cursor.alternation_detected;
        let mut batch = StoreBatch::default();
        batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
        self.store.apply_batch(batch)?;
        self.record_peer_problem(
            peer_id,
            SyncProblem {
                path: String::new(),
                kind: ProblemKind::DuplicateMemberId,
                peer: Some(peer_id.to_string()),
                sentence: peer_identity_problem_sentence(peer_id, is_alternation),
            },
        )?;
        Ok(())
    }

    /// Recovers when a peer's handshake or page shows that it has already received more from our
    /// current incarnation than we have produced (or a diverging fingerprint at a sequence we
    /// have): rate-limited strictly per claimant (at most once per day, and stopping after the
    /// third rotation until the person explicitly confirms in status whether to keep honouring or
    /// ignore that peer's reports).
    ///
    /// Returns `Ok(true)` if we rotated to a fresh incarnation, or `Ok(false)` if this peer's
    /// rollback claims are currently ignored (changing nothing).
    fn recover_own_rollback(
        &mut self,
        peer_id: &str,
        meta: &FolderMeta,
        now: Timestamp,
    ) -> Result<bool> {
        let claim_state = meta
            .rollback_claims_by_peer
            .get(peer_id)
            .cloned()
            .unwrap_or_default();

        if claim_state.ignored {
            return Ok(false);
        }

        if claim_state.rotations_since_confirm >= MAX_FORCED_ROTATIONS_BEFORE_CONFIRM {
            let sentence = peer_rollback_confirm_sentence(peer_id);
            self.record_peer_problem(
                peer_id,
                SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::PeerRollbackRateLimited,
                    peer: Some(peer_id.to_string()),
                    sentence: sentence.clone(),
                },
            )?;
            return Err(SyncError::Conflict {
                path: peer_id.to_string(),
                reason: sentence,
            });
        }

        let peer_within_day = claim_state.last_rotation_at.is_some_and(|ts| {
            now.secs >= ts.secs && (now.secs - ts.secs) < FORCED_ROTATION_COOLDOWN_SECS as i64
        });
        if peer_within_day {
            let sentence = format!(
                "{peer_id} reported again within a day that this machine's copy went back in time; ignoring the repeat claim."
            );
            self.record_peer_problem(
                peer_id,
                SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::PeerRollbackRateLimited,
                    peer: Some(peer_id.to_string()),
                    sentence: sentence.clone(),
                },
            )?;
            return Err(SyncError::Conflict {
                path: peer_id.to_string(),
                reason: sentence,
            });
        }

        // Collect EVERY permanent deletion and held deletion before resetting so NO deletion is
        // ever dropped across rotation.
        let carried_deletions = collect_deletions_for_rotation(&self.fs, &self.store)?;

        let new_inc = choose_fresh_incarnation(None, Some(meta.incarnation))?;
        let marker = FolderMarker::new(meta.folder_id.clone(), meta.local_member.clone(), new_inc);
        self.fs.write_marker(&marker)?;

        let mut all_paths = Vec::new();
        self.store.scan_records_in_order(&mut |rec| {
            all_paths.push(rec.path);
            Ok(true)
        })?;
        let all_peers: Vec<MemberId> = self.store.load_peer_cursors()?.into_keys().collect();

        let mut new_meta = meta.clone();
        new_meta.incarnation = new_inc;
        new_meta.local_seq = 0;
        new_meta.local_clock = 0;
        new_meta.join_mode = true;
        new_meta.unconfirmed_join_until_seq = 0;
        new_meta.head_fingerprint = None;
        new_meta.pending_deletions.clear();
        new_meta.unacked_change_marks.clear();
        let mut updated_state = claim_state;
        updated_state.last_rotation_at = Some(now);
        updated_state.rotations_since_confirm =
            updated_state.rotations_since_confirm.saturating_add(1);
        let reached_confirm_threshold =
            updated_state.rotations_since_confirm >= MAX_FORCED_ROTATIONS_BEFORE_CONFIRM;
        new_meta
            .rollback_claims_by_peer
            .insert(peer_id.to_string(), updated_state);

        let reset_batch = StoreBatch {
            folder_meta: Some(new_meta),
            remove_records: all_paths,
            remove_peer_cursors: all_peers,
            clear_seq_fingerprints: true,
            clear_held_back: true,
            clear_deferrals: true,
            set_pending_op: Some(None),
            ..StoreBatch::default()
        };
        self.store.apply_batch(reset_batch)?;

        // Scan the actual live disk under Rule D (`join_mode = true`), then restore every carried
        // deletion whose path is absent on disk.
        let _ = scan_folder(
            &self.fs,
            &mut self.store,
            &self.ignore,
            DeletionBrakeConfig::disabled(),
            &mut self.stability,
            now,
            ScanOptions {
                allow_mass_deletion: true,
                ..ScanOptions::default()
            },
        )?;

        reissue_carried_deletions(&self.fs, &mut self.store, carried_deletions)?;

        // Report the recovery honestly (clear backup-restore sentence), and if this was the third
        // rotation from `peer_id`, surface the confirmation prompt in status immediately.
        self.record_peer_problem(
            peer_id,
            SyncProblem {
                path: String::new(),
                kind: ProblemKind::PeerForcedRotation,
                peer: Some(peer_id.to_string()),
                sentence: own_rollback_sentence(peer_id),
            },
        )?;
        if reached_confirm_threshold {
            self.record_peer_problem(
                peer_id,
                SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::PeerRollbackRateLimited,
                    peer: Some(peer_id.to_string()),
                    sentence: peer_rollback_confirm_sentence(peer_id),
                },
            )?;
        }
        Ok(true)
    }

    fn peer_caught_up_for_rejoin(
        &self,
        local_meta: &FolderMeta,
        cursor: &PeerCursor,
    ) -> Result<bool> {
        if local_meta.local_seq == 0 {
            return Ok(true);
        }
        if cursor.acked_local_incarnation != local_meta.incarnation || cursor.acked_local_seq == 0 {
            return Ok(false);
        }
        Ok(self
            .store
            .get_seq_fingerprint(cursor.acked_local_seq)?
            .is_some())
    }

    fn persist_peer_cursor_and_clear_rejoin(
        &mut self,
        peer_id: &str,
        cursor: PeerCursor,
        caught_up: bool,
    ) -> Result<()> {
        let mut batch = StoreBatch::default();
        if caught_up {
            let mut meta = self.meta()?;
            if meta.rejoining_peers.remove(peer_id) {
                batch.folder_meta = Some(meta);
            }
        }
        batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
        self.store.apply_batch(batch)
    }

    fn update_progress(&mut self, peer_id: &str, files_left: u64, bytes_left: u64) {
        if files_left > 0 || bytes_left > 0 {
            self.active_progress.insert(
                peer_id.to_string(),
                SyncProgress {
                    files_left,
                    bytes_left,
                },
            );
        } else {
            self.active_progress.remove(peer_id);
        }
    }

    /// Compacts `.legion-sync/deletions.log` by merging only entries for the same path whose
    /// versions are ordered (the dominating version stays with the union of deleted hashes). Never
    /// drops committed deletion knowledge. Returns the number of surviving entries in the log.
    pub fn compact_deletion_log(&mut self) -> Result<usize> {
        self.guard_mut("compact_deletion_log", |e| {
            let raw = e.fs.read_deletion_log()?;
            if raw.is_empty() {
                return Ok(0);
            }
            let (compacted, count) = compact_deletion_log_bytes(&raw)?;
            e.fs.compact_deletion_log(&compacted)?;
            Ok(count)
        })
    }

    /// Prunes chained sequence fingerprints and change-age marks strictly below the minimum
    /// directly acknowledged sequence across all other current members. Never removes deletion
    /// tombstones.
    fn prune_acknowledged_metadata_inner(&mut self) -> Result<()> {
        let meta = self.meta()?;
        let cursors = self.store.load_peer_cursors()?;
        let other_members: Vec<&MemberId> = meta
            .members
            .iter()
            .filter(|m| *m != &meta.local_member)
            .collect();

        let min_direct_ack = if other_members.is_empty() {
            meta.local_seq
        } else {
            let mut floor = u64::MAX;
            for member in &other_members {
                match cursors.get(*member) {
                    Some(c) if c.acked_local_incarnation == meta.incarnation => {
                        floor = floor.min(c.acked_local_seq);
                    }
                    _ => {
                        floor = 0;
                        break;
                    }
                }
            }
            floor
        };

        if min_direct_ack == 0 {
            return Ok(());
        }

        let all_peers_pulled = other_members.iter().all(|m| {
            cursors.get(*m).is_some_and(|c| {
                c.incarnation != 0
                    && c.last_synced_at.is_some()
                    && c.applied_ack_seq >= c.last_seen_snapshot_seq
            })
        });
        let mut meta_for_marks = meta;
        let mut meta_changed = meta_for_marks.prune_change_marks_through(min_direct_ack);
        let mut confirmed_join_records = Vec::new();
        if meta_for_marks.unconfirmed_join_until_seq > 0
            && all_peers_pulled
            && self.store.held_back_count()? == 0
            && self.store.list_deferrals()?.is_empty()
        {
            self.store.scan_records_in_order(&mut |mut rec| {
                if rec.from_join && rec.seq <= min_direct_ack {
                    rec.from_join = false;
                    confirmed_join_records.push(rec);
                }
                Ok(true)
            })?;
            if min_direct_ack >= meta_for_marks.unconfirmed_join_until_seq {
                meta_for_marks.unconfirmed_join_until_seq = 0;
                meta_changed = true;
            }
        }
        let batch = StoreBatch {
            folder_meta: meta_changed.then_some(meta_for_marks),
            upsert_records: confirmed_join_records,
            prune_seq_fingerprints_below: Some(min_direct_ack),
            ..StoreBatch::default()
        };
        self.store.apply_batch(batch)
    }

    fn sync_peer_handshake_problems(
        &mut self,
        peer_id: &str,
        refused_from_caller: Option<&PeerRefusalSummary>,
    ) -> Result<()> {
        let existing = self.store.list_problems()?;
        let rate_key = peer_problem_key(peer_id, ProblemKind::PeerRollbackRateLimited, "");
        let refused_key = peer_problem_key(peer_id, ProblemKind::PeerRefusedRecords, "");
        let mut batch = StoreBatch::default();

        let needs_rollback_confirm = self
            .meta()
            .ok()
            .and_then(|m| m.rollback_claims_by_peer.get(peer_id).cloned())
            .is_some_and(|s| {
                !s.ignored && s.rotations_since_confirm >= MAX_FORCED_ROTATIONS_BEFORE_CONFIRM
            });
        if !needs_rollback_confirm && existing.iter().any(|(k, _)| k == &rate_key) {
            batch.remove_problems.push(rate_key);
        }
        match refused_from_caller {
            Some(summary) => {
                let prob = peer_refused_problem(peer_id, summary);
                let unchanged = existing
                    .iter()
                    .any(|(k, p)| k == &refused_key && p == &prob);
                if !unchanged {
                    batch.upsert_problems.push((refused_key, prob));
                }
                if let Ok(mut map) = self.peer_refusals_from_serve.lock() {
                    map.insert(peer_id.to_string(), summary.clone());
                }
            }
            None => {
                if existing.iter().any(|(k, _)| k == &refused_key) {
                    batch.remove_problems.push(refused_key);
                }
                if let Ok(mut map) = self.peer_refusals_from_serve.lock() {
                    map.remove(peer_id);
                }
            }
        }
        if !batch.is_empty() {
            self.store.apply_batch(batch)?;
        }
        Ok(())
    }

    fn refusal_summary_for_peer(&self, peer_id: &str) -> Result<Option<PeerRefusalSummary>> {
        if self.store.held_back_count()? > 0 {
            let held = self.store.list_held_back()?;
            let mut count = 0usize;
            let mut sample_path = String::new();
            let mut reason = String::new();
            for h in &held {
                if h.from_peer == peer_id {
                    if count == 0 {
                        sample_path = h.record.path.clone();
                        reason = h.problem.sentence.clone();
                    }
                    count += 1;
                }
            }
            if count > 0 {
                return Ok(Some(PeerRefusalSummary {
                    count,
                    sample_path,
                    reason,
                }));
            }
        }
        let problems = self.store.list_problems()?;
        for (_, p) in problems {
            if p.peer.as_deref() == Some(peer_id)
                && matches!(
                    p.kind,
                    ProblemKind::InvalidPeerRecord | ProblemKind::HeldBackLimitReached
                )
            {
                return Ok(Some(PeerRefusalSummary {
                    count: 1,
                    sample_path: p.path,
                    reason: p.sentence,
                }));
            }
        }
        Ok(None)
    }

    /// Serves this member's bidirectional handshake identity to `caller_member`, checking
    /// `caller.peer_pulled` against our own chained sequence fingerprint history and returning
    /// both what we have pulled from `caller_member` and what we have fully applied/acknowledged.
    pub fn serve_handshake(
        &self,
        caller_member: &str,
        caller: &HandshakeHello,
    ) -> Result<PeerHandshake> {
        self.guard_ref("serve_handshake", |e| {
            e.serve_handshake_inner(caller_member, caller)
        })
    }

    fn serve_handshake_inner(
        &self,
        caller_member: &str,
        caller: &HandshakeHello,
    ) -> Result<PeerHandshake> {
        let meta = self.check_serve_allowed(caller_member)?;
        if let Ok(mut map) = self.peer_refusals_from_serve.lock() {
            match &caller.refused_from_peer {
                Some(summary) => {
                    map.insert(caller_member.to_string(), summary.clone());
                }
                None => {
                    map.remove(caller_member);
                }
            }
        }
        let cursors = self.store.load_peer_cursors()?;
        let caller_cursor = cursors.get(caller_member);
        let (caller_pulled, caller_acked) = match caller_cursor.filter(|c| c.incarnation != 0) {
            Some(c) => (
                Some(PeerAckView {
                    incarnation: c.incarnation,
                    seq: c.seq,
                    fingerprint: c.head_fingerprint,
                }),
                Some(PeerAckView {
                    incarnation: c.incarnation,
                    seq: c.applied_ack_seq,
                    fingerprint: c.acked_fingerprint,
                }),
            ),
            None => (None, None),
        };
        let prev_acked_by_caller = caller_cursor
            .filter(|c| c.acked_local_incarnation == meta.incarnation)
            .map_or(0, |c| c.acked_local_seq);
        let caller_same_inc = caller_cursor.is_some_and(|c| c.incarnation == caller.incarnation);
        let caller_expected_rejoin = caller.peer_rejoining
            && caller_cursor.is_some_and(|c| {
                c.incarnation == caller.incarnation
                    && caller.snapshot_seq >= c.seq
                    && !(caller.snapshot_seq == c.seq
                        && c.seq > 0
                        && caller.head_fingerprint.is_some()
                        && c.head_fingerprint.is_some()
                        && caller.head_fingerprint != c.head_fingerprint)
            });
        let caller_pull_matches = match caller.peer_pulled {
            Some(ref view) if view.incarnation == meta.incarnation && view.seq > 0 => {
                if (caller_same_inc && view.seq < prev_acked_by_caller && !caller_expected_rejoin)
                    || (meta.rejoining_peers.contains(caller_member)
                        && view.seq <= meta.local_seq
                        && self.store.get_seq_fingerprint(view.seq)?.is_none())
                {
                    None
                } else {
                    Some(view_matches_local(&self.store, &meta, view)?)
                }
            }
            Some(ref view) => Some(view_matches_local(&self.store, &meta, view)?),
            None => None,
        };
        let caller_rejoining = meta.rejoining_peers.contains(caller_member);
        let refused_from_caller = self.refusal_summary_for_peer(caller_member)?;
        Ok(PeerHandshake {
            folder_id: meta.folder_id,
            member_id: meta.local_member,
            incarnation: meta.incarnation,
            snapshot_seq: meta.local_seq,
            head_fingerprint: meta.head_fingerprint,
            protocol_version: SYNC_PROTOCOL_VERSION,
            caller_pulled,
            caller_acked,
            caller_pull_matches,
            caller_rejoining,
            refused_from_caller,
        })
    }

    /// Serves incremental records after `(since_incarnation, since_seq)` to `caller_member`,
    /// carrying ONLY our direct acknowledgement of `caller_member`'s own `(incarnation, seq)`.
    pub fn serve_records_since(
        &self,
        caller_member: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage> {
        self.guard_ref("serve_records_since", |e| {
            let _meta = e.check_serve_allowed(caller_member)?;
            let capped_limit = limit.clamp(1, MAX_PAGE_RECORDS);
            let mut page =
                e.store
                    .records_since(caller_member, since_incarnation, since_seq, capped_limit)?;
            page.records.retain(|r| !is_internal_sync_path(&r.path));
            Ok(page)
        })
    }

    /// Serves a live file's bytes from byte `offset` to `caller_member` after checking membership,
    /// folder marker integrity, path safety, non-ignored status, on-disk stat freshness, and
    /// expected hash.
    pub fn serve_fetch_file_range(
        &self,
        caller_member: &str,
        path: &str,
        expected_hash: Hash,
        offset: u64,
    ) -> Result<Box<dyn Read + Send>> {
        self.guard_ref("serve_fetch_file_range", |e| {
            let _meta = e.check_serve_allowed(caller_member)?;
            let clean = validate_sync_path(path)?;
            if is_internal_sync_path(&clean) || e.ignore.is_path_or_ancestor_ignored(&clean, false)
            {
                return Err(SyncError::Denied {
                    member: caller_member.to_string(),
                    reason: format!("'{clean}' is excluded from syncing"),
                });
            }

            let Some(rec) = e.store.get_record(&clean)? else {
                return Err(SyncError::NotFound {
                    what: format!("file {clean:?}"),
                });
            };
            if rec.deleted || rec.kind != EntryKind::File {
                return Err(SyncError::NotFound {
                    what: format!("live file {clean:?}"),
                });
            }
            if let Some(indexed_hash) = rec.hash
                && indexed_hash != expected_hash
            {
                return Err(SyncError::HashMismatch {
                    path: clean,
                    expected: expected_hash,
                    actual: indexed_hash,
                });
            }

            // Check live stat against index before serving so changed files trigger a rescan
            // instead of streaming stale bytes.
            if !disk_stat_matches_index(&e.fs, &clean, Some(&rec))? {
                e.note_serve_hash_mismatch(&clean);
                if e.fs.stat_live(&clean)?.is_none() {
                    return Err(SyncError::NotFound {
                        what: format!("live file {clean:?}"),
                    });
                }
                return Err(SyncError::Conflict {
                    path: clean,
                    reason: "file changed on disk since last scan".to_string(),
                });
            }

            e.fs.open_live_read_at(&clean, offset)
        })
    }

    /// Serves a live file's bytes from `offset`, rescanning `path` immediately if its on-disk size
    /// or modification time differs from the index before refusing or serving.
    pub fn serve_fetch_file_range_checked(
        &mut self,
        caller_member: &str,
        path: &str,
        expected_hash: Hash,
        offset: u64,
        now: Timestamp,
    ) -> Result<Box<dyn Read + Send>> {
        self.guard_mut("serve_fetch_file_range_checked", |e| {
            let _meta = e.check_serve_allowed(caller_member)?;
            let clean = validate_sync_path(path)?;
            if is_internal_sync_path(&clean) || e.ignore.is_path_or_ancestor_ignored(&clean, false)
            {
                return Err(SyncError::Denied {
                    member: caller_member.to_string(),
                    reason: format!("'{clean}' is excluded from syncing"),
                });
            }
            if let Some(rec) = e.store.get_record(&clean)?
                && !rec.deleted
                && rec.kind == EntryKind::File
                && !disk_stat_matches_index(&e.fs, &clean, Some(&rec))?
            {
                let _ = e.scan_subtree_inner(&clean, now, ScanOptions::default())?;
            }
            e.serve_fetch_file_range(caller_member, &clean, expected_hash, offset)
        })
    }

    /// Convenience wrapper serving a live file from offset `0`.
    pub fn serve_fetch_file(
        &self,
        caller_member: &str,
        path: &str,
        expected_hash: Hash,
    ) -> Result<Box<dyn Read + Send>> {
        self.serve_fetch_file_range(caller_member, path, expected_hash, 0)
    }

    fn check_serve_allowed(&self, caller_member: &str) -> Result<FolderMeta> {
        let meta = self.meta()?;
        if !meta.members.contains(caller_member) {
            return Err(SyncError::Denied {
                member: caller_member.to_string(),
                reason: "caller is not a member of this sync folder".to_string(),
            });
        }
        if self.folder_problem.is_some() {
            return Err(SyncError::FolderUnavailable {
                folder_id: meta.folder_id,
                sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
            });
        }
        // Check folder marker on disk when index is non-empty so an unmounted drive is never served.
        if self.store.record_count()? > 0 {
            match self.fs.read_marker()? {
                Some(m) if m.folder_id == meta.folder_id && m.member_id == meta.local_member => {}
                _ => {
                    return Err(SyncError::FolderUnavailable {
                        folder_id: meta.folder_id,
                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                    });
                }
            }
        }
        Ok(meta)
    }

    fn verify_folder_marker(&mut self) -> Result<FolderMeta> {
        let meta = self.meta()?;
        match self.fs.read_marker()? {
            None => {
                if self.store.record_count()? > 0 {
                    self.folder_problem = Some(SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::FolderMarkerMissing,
                        peer: None,
                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                    });
                    return Err(SyncError::FolderUnavailable {
                        folder_id: meta.folder_id,
                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                    });
                }
                self.fs.ensure_internal_dirs()?;
                let marker = FolderMarker::new(
                    meta.folder_id.clone(),
                    meta.local_member.clone(),
                    meta.incarnation,
                );
                self.fs.write_marker(&marker)?;
                self.folder_problem = None;
                Ok(meta)
            }
            Some(marker) => {
                if marker.folder_id != meta.folder_id || marker.member_id != meta.local_member {
                    self.folder_problem = Some(SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::FolderMarkerMissing,
                        peer: None,
                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                    });
                    return Err(SyncError::FolderUnavailable {
                        folder_id: meta.folder_id,
                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                    });
                }
                if marker.incarnation != meta.incarnation {
                    self.fs.ensure_internal_dirs()?;
                    rotate_incarnation_on_marker_mismatch(
                        &self.fs,
                        &mut self.store,
                        meta,
                        marker.incarnation,
                    )?;
                    self.folder_problem = None;
                    return self.meta();
                }
                self.folder_problem = None;
                Ok(meta)
            }
        }
    }

    /// Lists all entries currently stored in `.legion-sync/bin`.
    pub fn list_bin(&self) -> Result<Vec<BinEntry>> {
        self.guard_ref("list_bin", |e| {
            let mut entries = e.store.list_bin_entries()?;
            entries.sort_by(|a, b| {
                b.deleted_at
                    .cmp(&a.deleted_at)
                    .then_with(|| a.original_path.cmp(&b.original_path))
                    .then_with(|| a.id.cmp(&b.id))
            });
            Ok(entries)
        })
    }

    /// Removes entries from `.legion-sync/bin` whose age at `now` meets or exceeds the folder's
    /// configured bin retention (30 days by default), refusing to run when `now` is implausible.
    pub fn expire_bin(&mut self, now: Timestamp) -> Result<Vec<BinEntry>> {
        self.guard_mut("expire_bin", |e| {
            e.last_now = now;
            e.verify_folder_marker()?;
            expire_bin(&e.fs, &mut e.store, e.bin_retention_secs, now)
        })
    }

    /// Restores a bin entry back to the live folder (defaulting to its original path) and bumps
    /// its version so it syncs out to all peers.
    pub fn restore_from_bin(
        &mut self,
        entry_id: &str,
        dest_path: Option<&str>,
        strategy: RestoreStrategy,
        now: Timestamp,
    ) -> Result<String> {
        self.guard_mut("restore_from_bin", |e| {
            e.last_now = now;
            e.verify_folder_marker()?;
            restore_from_bin(&e.fs, &mut e.store, entry_id, dest_path, strategy, now)
        })
    }

    /// Lists all files currently having two versions (unresolved conflicts).
    pub fn list_conflicts(&self) -> Result<Vec<ConflictInfo>> {
        self.guard_ref("list_conflicts", |e| {
            let mut out = Vec::new();
            for rec in e.store.list_conflicts()? {
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
        })
    }

    /// Resolves one conflict copy according to `choice`.
    pub fn resolve_conflict(
        &mut self,
        conflict_path: &str,
        choice: ConflictChoice,
        now: Timestamp,
    ) -> Result<()> {
        self.guard_mut("resolve_conflict", |e| {
            e.last_now = now;
            e.verify_folder_marker()?;
            resolve_conflict(&e.fs, &mut e.store, conflict_path, choice, now)
        })
    }

    fn persist_peer_pull_error(&mut self, peer_id: &str, err_prob: SyncProblem, now: Timestamp) {
        if self.index_damaged {
            return;
        }
        let Ok(Some(meta)) = self.store.load_meta() else {
            return;
        };
        if !meta.members.contains(peer_id) || peer_id == meta.local_member {
            return;
        }
        let Ok(cursors) = self.store.load_peer_cursors() else {
            return;
        };
        let mut cursor = cursors.get(peer_id).cloned().unwrap_or_default();
        cursor.last_error = Some(err_prob);
        cursor.last_error_at = Some(now);
        let mut batch = StoreBatch::default();
        batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
        let _ = self.store.apply_batch(batch);
    }

    fn damaged_status(&self) -> FolderStatus {
        let prob = self
            .folder_problem
            .clone()
            .unwrap_or_else(index_damaged_problem);
        let summary = prob.sentence.clone();
        let state = MemberState::Problem {
            sentence: summary.clone(),
            problems: vec![prob],
        };
        let members = self
            .config
            .members
            .iter()
            .map(|m| MemberStatus {
                member: m.clone(),
                state: if m == &self.config.local_member {
                    state.clone()
                } else {
                    MemberState::Paused
                },
                cursor_seq: 0,
                pending_outgoing: 0,
                pending_outgoing_since: None,
                pending_incoming: 0,
                last_synced_at: None,
                last_error: None,
            })
            .collect();
        FolderStatus {
            folder_id: self.config.id.clone(),
            local_member: self.config.local_member.clone(),
            state,
            summary,
            up_to_date_as_of: None,
            members,
            conflicts: Vec::new(),
            bin: Vec::new(),
            pending_brake: None,
            inbound_brakes: Vec::new(),
            deferred: Vec::new(),
            pending_rollback_confirmations: Vec::new(),
        }
    }

    /// Returns the complete [`FolderStatus`] formatted as of the engine's latest logical timestamp,
    /// together with per-member outbound/inbound pending counts, last successful exchange time,
    /// persisted exchange failures, conflicts, bin contents, any tripped deletion brake, and
    /// durable deferred items.
    pub fn status(&self) -> Result<FolderStatus> {
        self.status_at(self.last_now)
    }

    /// Returns the complete [`FolderStatus`] with relative ages in [`FolderStatus::summary`]
    /// formatted against `now`.
    pub fn status_at(&self, now: Timestamp) -> Result<FolderStatus> {
        if self.index_damaged {
            return Ok(self.damaged_status());
        }
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.status_inner(now))) {
            Ok(Ok(st)) => Ok(st),
            Ok(Err(SyncError::IndexDamaged { .. } | SyncError::CorruptData { .. })) => {
                Ok(self.damaged_status())
            }
            Ok(Err(err)) => Err(err),
            Err(_) => Ok(self.damaged_status()),
        }
    }

    fn status_inner(&self, now: Timestamp) -> Result<FolderStatus> {
        let meta = self.meta()?;
        let cursors = self.store.load_peer_cursors()?;
        let held = self.store.list_held_back()?;
        let stored_problems: Vec<SyncProblem> = self
            .store
            .list_problems()?
            .into_iter()
            .map(|(_, p)| p)
            .collect();
        let deferred = self.store.list_deferrals()?;
        let conflicts = self.list_conflicts()?;
        let bin = self.list_bin()?;

        let serve_refusals: BTreeMap<MemberId, PeerRefusalSummary> = self
            .peer_refusals_from_serve
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();

        let mut all_problems: Vec<SyncProblem> = Vec::new();
        if let Some(ref fp) = self.folder_problem {
            all_problems.push(fp.clone());
        }
        if let Some(ref brake) = meta.pending_brake {
            all_problems.push(SyncProblem {
                path: String::new(),
                kind: ProblemKind::DeletionBrakeTripped,
                peer: None,
                sentence: format!(
                    "Sync paused because {} of {} files disappeared at once; confirm whether they were really deleted or rescan.",
                    brake.deleted_count, brake.total_live_files
                ),
            });
        }
        all_problems.extend(stored_problems.iter().cloned());
        all_problems.extend(held.iter().map(|h| h.problem.clone()));
        for (member, summary) in &serve_refusals {
            if meta.members.contains(member)
                && !all_problems.iter().any(|p| {
                    p.kind == ProblemKind::PeerRefusedRecords
                        && p.peer.as_deref() == Some(member.as_str())
                })
            {
                all_problems.push(peer_refused_problem(member, summary));
            }
        }
        for (member, err_prob) in &self.last_pull_errors {
            if meta.members.contains(member)
                && !all_problems
                    .iter()
                    .any(|p| p.peer.as_deref() == Some(member.as_str()))
            {
                all_problems.push(err_prob.clone());
            }
        }
        let mut inbound_brakes: Vec<InboundDeletionBrake> = Vec::new();
        for (member, c) in &cursors {
            if !meta.members.contains(member) {
                continue;
            }
            if let Some(ref err_prob) = c.last_error
                && !all_problems
                    .iter()
                    .any(|p| p.peer.as_deref() == Some(member.as_str()))
            {
                all_problems.push(err_prob.clone());
            }
            if c.duplicate_detected
                && !all_problems.iter().any(|p| {
                    p.kind == ProblemKind::DuplicateMemberId
                        && p.peer.as_deref() == Some(member.as_str())
                })
            {
                all_problems.push(SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::DuplicateMemberId,
                    peer: Some(member.clone()),
                    sentence: peer_identity_problem_sentence(member, c.alternation_detected),
                });
            }
            if let Some(ref ib) = c.inbound_brake {
                inbound_brakes.push(ib.clone());
                if !all_problems.iter().any(|p| {
                    p.kind == ProblemKind::InboundDeletionBrakeTripped
                        && p.peer.as_deref() == Some(member.as_str())
                }) {
                    all_problems.push(SyncProblem {
                        path: String::new(),
                        kind: ProblemKind::InboundDeletionBrakeTripped,
                        peer: Some(member.clone()),
                        sentence: format!(
                            "Paused syncing from '{member}' because '{member}' wants to remove {} of {} files here; choose whether to apply the deletions or keep this machine's files.",
                            ib.deleted_count, ib.total_live_files
                        ),
                    });
                }
            }
        }
        all_problems.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.peer.cmp(&b.peer))
                .then_with(|| a.sentence.cmp(&b.sentence))
        });
        all_problems.dedup();

        // Build each other member's honest status first so overall folder state and summary are
        // derived directly from per-member outbound/inbound counts, last sync time, and problems.
        let pending_del_count = meta.pending_deletions.len() as u64;
        let oldest_pending_del_at = meta.pending_deletions.values().map(|p| p.detected_at).min();

        let mut peer_statuses: BTreeMap<MemberId, MemberStatus> = BTreeMap::new();
        let mut total_files_left: u64 = self.active_progress.values().map(|p| p.files_left).sum();
        let total_bytes_left: u64 = self.active_progress.values().map(|p| p.bytes_left).sum();

        for member in &meta.members {
            if member == &meta.local_member {
                continue;
            }
            let cursor_opt = cursors.get(member);
            let mut peer_problems: Vec<SyncProblem> = held
                .iter()
                .filter(|h| h.from_peer == *member)
                .map(|h| h.problem.clone())
                .collect();
            peer_problems.extend(
                stored_problems
                    .iter()
                    .filter(|p| p.peer.as_deref() == Some(member.as_str()))
                    .cloned(),
            );
            if let Some(summary) = serve_refusals.get(member)
                && !peer_problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::PeerRefusedRecords)
            {
                peer_problems.push(peer_refused_problem(member, summary));
            }
            if let Some(c) = cursor_opt
                && c.duplicate_detected
                && !peer_problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::DuplicateMemberId)
            {
                peer_problems.push(SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::DuplicateMemberId,
                    peer: Some(member.clone()),
                    sentence: peer_identity_problem_sentence(member, c.alternation_detected),
                });
            }
            let persisted_err = cursor_opt.and_then(|c| c.last_error.as_ref());
            let last_error_str = self
                .last_pull_errors
                .get(member)
                .or(persisted_err)
                .map(|p| p.sentence.clone());
            if peer_problems.is_empty()
                && let Some(err_prob) = self.last_pull_errors.get(member).or(persisted_err)
            {
                peer_problems.push(err_prob.clone());
            }
            peer_problems.sort_by(|a, b| {
                a.path
                    .cmp(&b.path)
                    .then_with(|| a.kind.cmp(&b.kind))
                    .then_with(|| a.sentence.cmp(&b.sentence))
            });
            peer_problems.dedup();

            let peer_acked_seq = cursor_opt
                .filter(|c| c.acked_local_incarnation == meta.incarnation)
                .map_or(0, |c| c.acked_local_seq);
            let seq_gap = meta.local_seq.saturating_sub(peer_acked_seq);
            let emitted_outgoing = if seq_gap == 0
                || (meta.local_clock == 0
                    && meta.members.len() <= 2
                    && cursor_opt.is_some_and(|c| c.incarnation != 0 && c.seq > 0))
            {
                0
            } else if cursor_opt.is_none_or(|c| c.incarnation == 0) || seq_gap > 4096 {
                seq_gap
            } else {
                let peer_inc = cursor_opt.map_or(0, |c| c.incarnation);
                let page =
                    self.store
                        .records_since(member, meta.incarnation, peer_acked_seq, 4096)?;
                let peer_vkey = crate::version::VersionKey::new(member.clone(), peer_inc);
                page.records
                    .iter()
                    .filter(|r| !(r.modified_by == *member && r.version.get(&peer_vkey) > 0))
                    .count() as u64
            };
            let pending_outgoing = emitted_outgoing.saturating_add(pending_del_count);
            let pending_outgoing_since = if pending_outgoing == 0 {
                None
            } else {
                let emitted_since = if emitted_outgoing > 0 {
                    meta.oldest_change_since(peer_acked_seq)
                } else {
                    None
                };
                match (emitted_since, oldest_pending_del_at) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                }
            };

            let peer_held_count = held.iter().filter(|h| h.from_peer == *member).count() as u64;
            let peer_deferred_count = deferred.iter().filter(|d| d.peer == *member).count() as u64;
            let seq_unapplied = cursor_opt.map_or(0, |c| {
                c.last_seen_snapshot_seq
                    .max(c.seq)
                    .saturating_sub(c.applied_ack_seq)
            });
            let pending_incoming =
                seq_unapplied.max(peer_held_count.saturating_add(peer_deferred_count));

            let has_active = self
                .active_progress
                .get(member)
                .is_some_and(|p| p.files_left > 0 || p.bytes_left > 0);
            if !has_active
                && (peer_deferred_count > 0 || (peer_problems.is_empty() && pending_incoming > 0))
            {
                total_files_left =
                    total_files_left.saturating_add(peer_deferred_count.max(pending_incoming));
            }

            let last_synced_at = cursor_opt.and_then(|c| c.last_synced_at);
            let peer_state = if !peer_problems.is_empty() {
                MemberState::Problem {
                    sentence: summarize_problems(&peer_problems),
                    problems: peer_problems,
                }
            } else if meta.paused {
                MemberState::Paused
            } else if let Some(prog) = self.active_progress.get(member)
                && (prog.files_left > 0 || prog.bytes_left > 0)
            {
                MemberState::Syncing {
                    files_left: prog.files_left,
                    bytes_left: prog.bytes_left,
                }
            } else if peer_deferred_count > 0 || pending_incoming > 0 {
                MemberState::Syncing {
                    files_left: peer_deferred_count.max(pending_incoming),
                    bytes_left: 0,
                }
            } else if self.unreachable_peers.contains(member) {
                MemberState::WaitingForMember {
                    members: vec![member.clone()],
                }
            } else if last_synced_at.is_none() {
                MemberState::NeverReached
            } else if pending_outgoing > 0 {
                MemberState::WaitingForMember {
                    members: vec![member.clone()],
                }
            } else {
                MemberState::UpToDate
            };

            peer_statuses.insert(
                member.clone(),
                MemberStatus {
                    member: member.clone(),
                    state: peer_state,
                    cursor_seq: cursor_opt.map_or(0, |c| c.seq),
                    pending_outgoing,
                    pending_outgoing_since,
                    pending_incoming,
                    last_synced_at,
                    last_error: last_error_str,
                },
            );
        }

        let waiting_peers: Vec<MemberId> = peer_statuses
            .values()
            .filter(|ms| {
                matches!(
                    ms.state,
                    MemberState::WaitingForMember { .. } | MemberState::NeverReached
                )
            })
            .map(|ms| ms.member.clone())
            .collect();
        let all_others_never_reached = !peer_statuses.is_empty()
            && peer_statuses
                .values()
                .all(|ms| matches!(ms.state, MemberState::NeverReached));
        let total_pending_outgoing: u64 =
            peer_statuses.values().map(|ms| ms.pending_outgoing).sum();

        let local_state = if !all_problems.is_empty() {
            MemberState::Problem {
                sentence: summarize_problems(&all_problems),
                problems: all_problems.clone(),
            }
        } else if meta.paused {
            MemberState::Paused
        } else if total_files_left > 0 || total_bytes_left > 0 {
            MemberState::Syncing {
                files_left: total_files_left,
                bytes_left: total_bytes_left,
            }
        } else if all_others_never_reached
            && self.unreachable_peers.is_empty()
            && total_pending_outgoing == 0
        {
            MemberState::NeverReached
        } else if !waiting_peers.is_empty() {
            MemberState::WaitingForMember {
                members: waiting_peers,
            }
        } else {
            MemberState::UpToDate
        };

        let up_to_date_as_of = if local_state == MemberState::UpToDate && !peer_statuses.is_empty()
        {
            peer_statuses
                .values()
                .filter_map(|m| m.last_synced_at)
                .min()
        } else {
            None
        };
        let self_last_synced_at = if !peer_statuses.is_empty()
            && peer_statuses.values().all(|m| m.last_synced_at.is_some())
        {
            peer_statuses
                .values()
                .filter_map(|m| m.last_synced_at)
                .min()
        } else {
            None
        };

        let mut members_vec = Vec::with_capacity(meta.members.len());
        for member in &meta.members {
            if member == &meta.local_member {
                members_vec.push(MemberStatus {
                    member: member.clone(),
                    state: local_state.clone(),
                    cursor_seq: meta.local_seq,
                    pending_outgoing: 0,
                    pending_outgoing_since: None,
                    pending_incoming: 0,
                    last_synced_at: self_last_synced_at,
                    last_error: None,
                });
            } else if let Some(ms) = peer_statuses.get(member) {
                members_vec.push(ms.clone());
            }
        }

        let summary = summarize_folder_status(
            &local_state,
            &meta.local_member,
            &members_vec,
            now,
            up_to_date_as_of,
        );

        let pending_rollback_confirmations: Vec<MemberId> = meta
            .members
            .iter()
            .filter(|m| {
                meta.rollback_claims_by_peer.get(*m).is_some_and(|s| {
                    !s.ignored && s.rotations_since_confirm >= MAX_FORCED_ROTATIONS_BEFORE_CONFIRM
                })
            })
            .cloned()
            .collect();

        Ok(FolderStatus {
            folder_id: meta.folder_id,
            local_member: meta.local_member,
            state: local_state,
            summary,
            up_to_date_as_of,
            members: members_vec,
            conflicts,
            bin,
            pending_brake: meta.pending_brake,
            inbound_brakes,
            deferred,
            pending_rollback_confirmations,
        })
    }
}

const SCAN_PROBLEM_PREFIX: &str = "scan\0";
const DIR_KEPT_PROBLEM_PREFIX: &str = "dir_kept\0";

fn scan_problem_key(kind: ProblemKind, path: &str) -> String {
    format!("{SCAN_PROBLEM_PREFIX}{kind:?}\0{path}")
}

fn dir_kept_problem_key(path: &str) -> String {
    format!("{DIR_KEPT_PROBLEM_PREFIX}{path}")
}

fn peer_problem_key(peer: &str, kind: ProblemKind, path: &str) -> String {
    format!("peer\0{peer}\0{kind:?}\0{path}")
}

fn prev_chain_fp_for(
    seq: u64,
    chain_fps: &BTreeMap<u64, Hash>,
    cursor: &PeerCursor,
) -> Option<Hash> {
    if seq <= 1 {
        return None;
    }
    let prev = seq - 1;
    chain_fps.get(&prev).copied().or_else(|| {
        (cursor.seq == prev)
            .then_some(cursor.head_fingerprint)
            .flatten()
    })
}

fn recompute_peer_applied_ack(
    peer_id: &str,
    cursor: &mut PeerCursor,
    held_back: &[HeldBackRecord],
    deferrals: &[DeferredItem],
) {
    if cursor.incarnation == 0 {
        cursor.applied_ack_seq = 0;
        cursor.acked_fingerprint = None;
        return;
    }

    let mut oldest: Option<(u64, Option<Hash>)> = None;
    for h in held_back {
        if h.from_peer == peer_id
            && h.peer_incarnation == cursor.incarnation
            && h.peer_seq > 0
            && oldest.is_none_or(|(min_seq, _)| h.peer_seq < min_seq)
        {
            oldest = Some((h.peer_seq, h.prev_chain_fingerprint));
        }
    }
    for d in deferrals {
        if d.peer == peer_id
            && d.peer_incarnation == cursor.incarnation
            && d.peer_seq > 0
            && oldest.is_none_or(|(min_seq, _)| d.peer_seq < min_seq)
        {
            oldest = Some((d.peer_seq, d.prev_chain_fingerprint));
        }
    }

    match oldest {
        None => {
            cursor.applied_ack_seq = cursor.seq;
            cursor.acked_fingerprint = cursor.head_fingerprint;
        }
        Some((min_unapplied_seq, prev_fp)) => {
            let candidate = cursor.seq.min(min_unapplied_seq.saturating_sub(1));
            if candidate == 0 {
                cursor.applied_ack_seq = 0;
                cursor.acked_fingerprint = None;
            } else if candidate == cursor.seq && cursor.head_fingerprint.is_some() {
                cursor.applied_ack_seq = candidate;
                cursor.acked_fingerprint = cursor.head_fingerprint;
            } else if candidate == min_unapplied_seq.saturating_sub(1) && prev_fp.is_some() {
                cursor.applied_ack_seq = candidate;
                cursor.acked_fingerprint = prev_fp;
            } else if cursor.applied_ack_seq > candidate {
                cursor.applied_ack_seq = 0;
                cursor.acked_fingerprint = None;
            }
        }
    }
}

/// Checks whether a peer's claimed view of OUR history (`(incarnation, seq, fingerprint)`) matches
/// our own chained sequence fingerprint history.
///
/// Returns `Ok(false)` when `view` names our current incarnation with `seq > 0` and either:
/// - `seq > meta.local_seq`, or
/// - `seq` is missing/pruned from our sequence fingerprint table ("cannot check -> went back"), or
/// - the stored chained fingerprint at `seq` disagrees with `view.fingerprint`.
fn view_matches_local<S: IndexStore>(
    store: &S,
    meta: &FolderMeta,
    view: &PeerAckView,
) -> Result<bool> {
    if view.incarnation != meta.incarnation || view.seq == 0 {
        return Ok(true);
    }
    if view.seq > meta.local_seq {
        return Ok(false);
    }
    let Some(expected_fp) = store.get_seq_fingerprint(view.seq)? else {
        return Ok(false);
    };
    match view.fingerprint {
        Some(fp) => Ok(fp == expected_fp),
        None => Ok(false),
    }
}

/// Returns `true` when `view` proves our own index went back in time: `view` names our current
/// incarnation and either reports a sequence strictly greater than our `local_seq` (the peer has
/// already received more from us than we have produced) or carries a chained fingerprint that
/// contradicts what we stored at `view.seq`.
fn view_proves_own_rollback<S: IndexStore>(
    store: &S,
    meta: &FolderMeta,
    view: &PeerAckView,
) -> Result<bool> {
    if view.incarnation != meta.incarnation || view.seq == 0 {
        return Ok(false);
    }
    if view.seq > meta.local_seq {
        return Ok(true);
    }
    view_contradicts_stored_fingerprint(store, meta, view)
}

/// Returns `true` when `view` names our current incarnation at a sequence we have
/// (`1 <= view.seq <= meta.local_seq`) and carries a fingerprint that contradicts what we stored.
fn view_contradicts_stored_fingerprint<S: IndexStore>(
    store: &S,
    meta: &FolderMeta,
    view: &PeerAckView,
) -> Result<bool> {
    if view.incarnation != meta.incarnation || view.seq == 0 || view.seq > meta.local_seq {
        return Ok(false);
    }
    let Some(claimed_fp) = view.fingerprint else {
        return Ok(false);
    };
    let Some(expected_fp) = store.get_seq_fingerprint(view.seq)? else {
        return Ok(false);
    };
    Ok(claimed_fp != expected_fp)
}

/// Minimal [`Peer`] stub used by [`SyncEngine::retry_held_back_local`] for local-only decisions
/// (deletions, metadata merges, superseding tombstones, directory revivals) that never fetch bytes.
struct LocalOnlyPeer<'a>(&'a str);

impl Peer for LocalOnlyPeer<'_> {
    fn member_id(&self) -> &str {
        self.0
    }

    fn handshake(&self, _caller: &HandshakeHello) -> Result<PeerHandshake> {
        Err(SyncError::PeerUnavailable {
            member: self.0.to_string(),
            detail: "local-only retry does not contact peer".to_string(),
        })
    }

    fn records_since(
        &self,
        _since_incarnation: u64,
        _since_seq: u64,
        _limit: usize,
        _cancel: Option<&std::sync::atomic::AtomicBool>,
    ) -> Result<RecordsPage> {
        Err(SyncError::PeerUnavailable {
            member: self.0.to_string(),
            detail: "local-only retry does not contact peer".to_string(),
        })
    }

    fn fetch_file(
        &self,
        _path: &str,
        _expected_hash: Hash,
        _options: &FetchOptions,
    ) -> Result<Box<dyn Read + Send>> {
        Err(SyncError::PeerUnavailable {
            member: self.0.to_string(),
            detail: "local-only retry does not fetch files".to_string(),
        })
    }
}

fn pending_to_deletion_record(
    pending: &crate::model::PendingLocalDeletion,
    local_member: &str,
) -> FolderDeletionRecord {
    match FolderDeletionLogEntry::from_pending_deletion(pending, local_member) {
        FolderDeletionLogEntry::Deleted {
            path,
            kind,
            hashes,
            version,
            mtime,
            modified_by,
            held,
        } => FolderDeletionRecord {
            path,
            kind,
            hashes,
            version,
            mtime,
            modified_by,
            held,
        },
        FolderDeletionLogEntry::Revived { .. } => unreachable!(),
    }
}

fn collect_deletions_for_rotation<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &S,
) -> Result<BTreeMap<String, Vec<FolderDeletionRecord>>> {
    let mut map = parse_deletion_log_all(&fs.read_deletion_log()?);
    if let Some(meta) = store.load_meta()? {
        for pending in meta.pending_deletions.values() {
            let rec = pending_to_deletion_record(pending, &meta.local_member);
            merge_deletion_record_into_list(map.entry(rec.path.clone()).or_default(), rec);
        }
    }
    for tomb in store.list_tombstones()? {
        let rec = FolderDeletionRecord {
            path: tomb.path.clone(),
            kind: tomb.kind,
            hashes: tomb.recent_hashes,
            version: tomb.version,
            mtime: tomb.mtime,
            modified_by: tomb.modified_by,
            held: false,
        };
        merge_deletion_record_into_list(map.entry(rec.path.clone()).or_default(), rec);
    }
    Ok(map)
}

fn reissue_carried_deletions<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    deletions: BTreeMap<String, Vec<FolderDeletionRecord>>,
) -> Result<()> {
    if deletions.is_empty() {
        return Ok(());
    }
    let Some(mut meta) = store.load_meta()? else {
        return Ok(());
    };
    let mut batch = StoreBatch::default();
    let mut log_entries_to_persist = Vec::new();
    let mut restored_any = false;

    for (path, recs) in deletions {
        let Some(rec) = collapse_deletion_records(recs.clone()) else {
            continue;
        };
        // Keep all committed deletion records in `.legion-sync/deletions.log` even if a path was
        // later recreated live on disk — compaction never drops committed knowledge.
        let mut persist_recs = recs;

        // Never overwrite a path that currently exists live on disk.
        if fs.stat_live(&path)?.is_some() || store.get_record(&path)?.is_some_and(|r| !r.deleted) {
            for r in persist_recs {
                if !r.held {
                    log_entries_to_persist.push(FolderDeletionLogEntry::Deleted {
                        path: r.path,
                        kind: r.kind,
                        hashes: r.hashes,
                        version: r.version,
                        mtime: r.mtime,
                        modified_by: r.modified_by,
                        held: false,
                    });
                }
            }
            continue;
        }

        let (version, modified_by) = if rec.held {
            // An unsettled local deletion inside the 30s settle window had the live record's
            // pre-deletion version; bump once under the new incarnation to turn it into a
            // committed tombstone.
            let mut v = rec.version;
            meta.bump_version(&mut v);
            let m = meta.local_member.clone();
            persist_recs = vec![FolderDeletionRecord {
                path: path.clone(),
                kind: rec.kind,
                hashes: rec.hashes.clone(),
                version: v.clone(),
                mtime: rec.mtime,
                modified_by: m.clone(),
                held: false,
            }];
            (v, m)
        } else {
            // Committed deletion: restore its exact version vector and author into the index
            // cache WITHOUT adding the rebuilt incarnation's key!
            let author = if meta.is_known_member(&rec.modified_by) {
                rec.modified_by
            } else {
                meta.local_member.clone()
            };
            (rec.version, author)
        };

        let seq = meta.next_seq();
        meta.record_change_mark(seq, rec.mtime);
        let tomb = IndexRecord {
            path,
            kind: rec.kind,
            size: 0,
            mtime: rec.mtime,
            disk_mtime: None,
            disk_change_id: None,
            mode: None,
            hash: None,
            link_target: None,
            deleted: true,
            version,
            modified_by,
            seq,
            recent_hashes: rec.hashes,
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        };
        let chain = meta.advance_chain_fingerprint(seq, tomb.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        batch.upsert_records.push(tomb);
        restored_any = true;

        for r in persist_recs {
            log_entries_to_persist.push(FolderDeletionLogEntry::Deleted {
                path: r.path,
                kind: r.kind,
                hashes: r.hashes,
                version: r.version,
                mtime: r.mtime,
                modified_by: r.modified_by,
                held: false,
            });
        }
    }

    if !log_entries_to_persist.is_empty() {
        // Persist durably to `.legion-sync/deletions.log` BEFORE committing the index batch!
        let encoded = encode_deletion_log_entries(&log_entries_to_persist)?;
        let (compacted, _) = compact_deletion_log_bytes(&encoded)?;
        fs.compact_deletion_log(&compacted)?;
    }

    if restored_any {
        batch.folder_meta = Some(meta);
        store.apply_batch(batch)?;
    }
    Ok(())
}

fn peer_refused_problem(peer_id: &str, summary: &PeerRefusalSummary) -> SyncProblem {
    let sentence = if summary.count <= 1 {
        format!(
            "'{peer_id}' could not apply '{}' from this machine: {}",
            summary.sample_path, summary.reason
        )
    } else {
        format!(
            "'{peer_id}' could not apply {} items from this machine (for example '{}'): {}",
            summary.count, summary.sample_path, summary.reason
        )
    };
    SyncProblem {
        path: summary.sample_path.clone(),
        kind: ProblemKind::PeerRefusedRecords,
        peer: Some(peer_id.to_string()),
        sentence,
    }
}

fn choose_fresh_incarnation(explicit: Option<u64>, avoid: Option<u64>) -> Result<u64> {
    if let Some(inc) = explicit
        && Some(inc) != avoid
    {
        return Ok(inc);
    }
    for _ in 0..16 {
        let candidate = random_incarnation_id()?;
        if Some(candidate) != avoid {
            return Ok(candidate);
        }
    }
    Ok(avoid.unwrap_or(0).wrapping_add(1).max(1))
}

/// Rotates to a fresh incarnation when `.legion-sync/marker.json` and the local index disagree on
/// incarnation (e.g. one was restored from backup without the other).
///
/// Carries every pending tombstone and `.legion-sync/deletions.log` into the new incarnation
/// (never dropping deletions), clears old live records without fabricating deletions, and enters
/// `join_mode = true` so the next scan indexes the actual files on disk under Rule D.
fn rotate_incarnation_on_marker_mismatch<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut meta: FolderMeta,
    marker_inc: u64,
) -> Result<()> {
    let carried_deletions = collect_deletions_for_rotation(fs, store)?;

    let mut new_inc = random_incarnation_id()?;
    while new_inc == meta.incarnation || new_inc == marker_inc {
        new_inc = random_incarnation_id()?;
    }

    let marker = FolderMarker::new(meta.folder_id.clone(), meta.local_member.clone(), new_inc);
    fs.write_marker(&marker)?;

    let mut all_paths = Vec::new();
    store.scan_records_in_order(&mut |rec| {
        all_paths.push(rec.path);
        Ok(true)
    })?;
    let all_peers: Vec<MemberId> = store.load_peer_cursors()?.into_keys().collect();

    meta.incarnation = new_inc;
    meta.local_seq = 0;
    meta.local_clock = 0;
    meta.join_mode = true;
    meta.head_fingerprint = None;
    meta.pending_deletions.clear();
    meta.unacked_change_marks.clear();

    let batch = StoreBatch {
        folder_meta: Some(meta),
        remove_records: all_paths,
        remove_peer_cursors: all_peers,
        clear_seq_fingerprints: true,
        clear_held_back: true,
        clear_deferrals: true,
        set_pending_op: Some(None),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)?;

    reissue_carried_deletions(fs, store, carried_deletions)
}

fn duplicate_member_sentence(peer_id: &str) -> String {
    format!(
        "Two different machines appear to be using the same member name '{peer_id}'. Rename or remove one before syncing with it again."
    )
}

fn peer_rollback_sentence(peer_id: &str) -> String {
    format!(
        "{peer_id}'s copy of the folder is older than what this machine has already received from it; it looks like it was restored from a backup. Waiting for {peer_id} to rejoin the folder. Nothing is deleted."
    )
}

fn own_rollback_sentence(peer_id: &str) -> String {
    format!(
        "This machine's copy of the folder is older than what {peer_id} has already received from it; it looks like it was restored from a backup. Legion is joining the folder again. Nothing is deleted."
    )
}

fn peer_rollback_confirm_sentence(peer_id: &str) -> String {
    format!(
        "{peer_id} has reported three times that this machine's copy went back in time. If you restored this machine from a backup, that is expected. Otherwise {peer_id} may be faulty."
    )
}

fn peer_identity_problem_sentence(peer_id: &str, alternation_detected: bool) -> String {
    if alternation_detected {
        duplicate_member_sentence(peer_id)
    } else {
        peer_rollback_sentence(peer_id)
    }
}

fn summarize_problems(problems: &[SyncProblem]) -> String {
    if problems.len() == 1 {
        return problems[0].sentence.clone();
    }
    format!(
        "{} sync problems need attention (for example: {})",
        problems.len(),
        problems[0].sentence
    )
}

fn format_relative_age(now: Timestamp, then: Timestamp) -> String {
    let diff = now.secs.saturating_sub(then.secs).max(0);
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3_600 {
        let m = diff / 60;
        if m == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{m} minutes ago")
        }
    } else if diff < 86_400 {
        let h = diff / 3_600;
        if h == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{h} hours ago")
        }
    } else {
        let d = diff / 86_400;
        if d == 1 {
            "1 day ago".to_string()
        } else {
            format!("{d} days ago")
        }
    }
}

fn summarize_folder_status(
    local_state: &MemberState,
    local_member: &str,
    members: &[MemberStatus],
    now: Timestamp,
    up_to_date_as_of: Option<Timestamp>,
) -> String {
    match local_state {
        MemberState::Problem { sentence, .. } => sentence.clone(),
        MemberState::Paused => "Sync is paused.".to_string(),
        MemberState::Syncing {
            files_left,
            bytes_left,
        } => {
            let item_word = if *files_left == 1 { "item" } else { "items" };
            if *bytes_left > 0 {
                format!("Syncing {files_left} {item_word} ({bytes_left} bytes left)")
            } else {
                format!("Syncing {files_left} {item_word}")
            }
        }
        MemberState::UpToDate => match up_to_date_as_of {
            Some(as_of) => format!("Up to date as of {}", format_relative_age(now, as_of)),
            None => "Up to date".to_string(),
        },
        MemberState::NeverReached | MemberState::WaitingForMember { .. } => {
            let mut clauses = Vec::new();
            for m in members {
                if m.member == local_member || m.state == MemberState::UpToDate {
                    continue;
                }
                let change_word = if m.pending_outgoing == 1 {
                    "change"
                } else {
                    "changes"
                };
                match (m.last_synced_at, m.pending_outgoing) {
                    (None, 0) => clauses.push(format!("{} has never been reached", m.member)),
                    (None, n) => clauses.push(format!(
                        "{n} {change_word} waiting for {}, never reached",
                        m.member
                    )),
                    (Some(last), 0) => clauses.push(format!(
                        "Waiting for {}, last reached {}",
                        m.member,
                        format_relative_age(now, last)
                    )),
                    (Some(last), n) => clauses.push(format!(
                        "{n} {change_word} waiting for {}, last reached {}",
                        m.member,
                        format_relative_age(now, last)
                    )),
                }
            }
            if clauses.is_empty() {
                "Waiting for another member".to_string()
            } else {
                clauses.join("; ")
            }
        }
    }
}
