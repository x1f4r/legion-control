//! High-level hubless folder synchronization engine orchestrating folder identity verification,
//! incarnation management, scanning, untrusted-peer reconciliation, crash-safe batched apply,
//! direct-acknowledgement tombstone garbage collection, bin management, conflict resolution, and
//! status reporting.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::sync::atomic::Ordering;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::apply::{
    ApplyOutcome, RecoveryReport, StoreIndexView, apply_decision, apply_local_rename,
    apply_new_files_group_commit, expire_bin, recover_pending, resolve_conflict, restore_from_bin,
};
use crate::error::{Result, SyncError};
use crate::model::{
    BinEntry, ConflictChoice, ConflictInfo, DeferredItem, DeletionBrakeConfig,
    DeletionBrakeDecision, FetchOptions, FolderConfig, FolderMarker, FolderMeta, FolderStatus,
    HeldBackRecord, INDEX_SCHEMA_VERSION, IndexRecord, MAX_DEFERRAL_ATTEMPTS,
    MAX_HELD_BACK_ENTRIES, MAX_PAGE_RECORDS, MAX_PEER_PROBLEMS, MemberId, MemberState,
    MemberStatus, PeerCursor, PeerHandshake, ProblemKind, RecordsPage, RestoreStrategy,
    SYNC_PROTOCOL_VERSION, SyncProblem, SyncProgress, random_incarnation_id, validate_member_id,
};
use crate::path::{
    IgnoreMatcher, is_descendant_of, is_internal_sync_path, path_depth, validate_sync_path,
};
use crate::reconcile::{LocalIndexView, ReconcileDecision, derive_conflict_of, reconcile_record};
use crate::scan::{PendingStabilityTracker, ScanOptions, ScanReport, scan_folder, scan_subtree};
use crate::traits::{IndexStore, Peer, StoreBatch, SyncFs};
use crate::version::VersionVector;

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
    /// Records held back as platform problems or unreadable subtrees.
    pub held_back: usize,
    /// Records deferred (with bounded retry counts).
    pub deferred: usize,
    /// Malformed or unauthorized records from this peer that were skipped and reported.
    pub invalid_skipped: usize,
    /// Local rename optimizations performed without downloading file bytes.
    pub local_renames: usize,
    /// Tombstones garbage-collected using direct acknowledgements only.
    pub tombstones_gc: usize,
    /// Previously held-back records successfully re-evaluated and applied.
    pub held_back_resolved: usize,
}

/// Core synchronization engine for one member's copy of a sync folder.
pub struct SyncEngine<F: SyncFs, S: IndexStore> {
    fs: F,
    store: S,
    ignore: IgnoreMatcher,
    bin_retention_secs: u64,
    deletion_brake: DeletionBrakeConfig,
    stability: PendingStabilityTracker,
    unreachable_peers: BTreeSet<MemberId>,
    active_progress: BTreeMap<MemberId, SyncProgress>,
    scan_problems: Vec<SyncProblem>,
    skipped_paths: BTreeSet<String>,
    folder_problem: Option<SyncProblem>,
    peer_problems: BTreeMap<MemberId, Vec<SyncProblem>>,
    dir_kept_problems: BTreeMap<String, SyncProblem>,
    deferred_items: BTreeMap<(MemberId, String), DeferredItem>,
}

impl<F: SyncFs, S: IndexStore> SyncEngine<F, S> {
    /// Opens or initializes a sync engine for `config`, checking schema version, nesting rules,
    /// and folder identity marker, and running crash recovery when the folder is valid.
    pub fn open(config: FolderConfig, fs: F, mut store: S) -> Result<Self> {
        config.validate()?;
        let found_schema = store.schema_version()?;
        if found_schema > INDEX_SCHEMA_VERSION {
            return Err(SyncError::SchemaTooNew {
                found: found_schema,
                supported: INDEX_SCHEMA_VERSION,
            });
        }
        fs.check_not_nested()?;

        let ignore = IgnoreMatcher::compile(&config.ignore)?;
        let mut folder_problem = None;
        let mut needs_recover = true;

        match store.load_meta()? {
            None => {
                // Brand-new store or reinstall with index wiped.
                let existing_marker = fs.read_marker()?;
                if let Some(ref m) = existing_marker
                    && (m.folder_id != config.id || m.member_id != config.local_member)
                {
                    return Err(SyncError::FolderUnavailable {
                        folder_id: config.id.clone(),
                        sentence: FOLDER_MARKER_PROBLEM_SENTENCE.to_string(),
                    });
                }

                let avoid_inc = existing_marker.as_ref().map(|m| m.incarnation);
                let inc = choose_fresh_incarnation(config.initial_incarnation, avoid_inc)?;
                fs.ensure_internal_dirs()?;
                let marker = FolderMarker::new(config.id.clone(), config.local_member.clone(), inc);
                fs.write_marker(&marker)?;

                let meta = FolderMeta::new(&config, inc);
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    ..StoreBatch::default()
                };
                store.apply_batch(batch)?;
            }
            Some(existing) => {
                if existing.folder_id != config.id {
                    return Err(SyncError::InvalidConfig {
                        message: format!(
                            "index store belongs to folder {:?}, not {:?}",
                            existing.folder_id, config.id
                        ),
                        hint: Some(
                            "Open the index database that matches this sync folder id.".to_string(),
                        ),
                    });
                }
                if existing.local_member != config.local_member {
                    return Err(SyncError::InvalidConfig {
                        message: format!(
                            "index store belongs to member {:?}, not {:?}",
                            existing.local_member, config.local_member
                        ),
                        hint: Some("Each member must keep its own index store.".to_string()),
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
                            // Item C: marker and index disagree on incarnation -> rotate to a new
                            // incarnation and reconcile disk by Rule D instead of reusing counters
                            // or inventing deletions!
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

        let mut engine = Self {
            fs,
            store,
            ignore,
            bin_retention_secs: config.bin_retention_secs,
            deletion_brake: config.deletion_brake,
            stability: PendingStabilityTracker::new(),
            unreachable_peers: BTreeSet::new(),
            active_progress: BTreeMap::new(),
            scan_problems: Vec::new(),
            skipped_paths: BTreeSet::new(),
            folder_problem,
            peer_problems: BTreeMap::new(),
            dir_kept_problems: BTreeMap::new(),
            deferred_items: BTreeMap::new(),
        };
        if needs_recover {
            engine.recover()?;
        }
        Ok(engine)
    }

    /// Runs crash recovery for any incomplete write-ahead operation and cleans `.legion-sync/tmp`.
    pub fn recover(&mut self) -> Result<RecoveryReport> {
        recover_pending(&self.fs, &mut self.store)
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
        self.store.load_meta()?.ok_or_else(|| SyncError::Storage {
            context: "load folder metadata".to_string(),
            detail: "index store is uninitialized".to_string(),
        })
    }

    /// Updates the folder's custom ignore glob patterns.
    pub fn update_ignore(&mut self, patterns: Vec<String>) -> Result<()> {
        self.ignore = IgnoreMatcher::compile(&patterns)?;
        Ok(())
    }

    /// Updates the folder's mass-deletion brake thresholds.
    pub fn set_deletion_brake(&mut self, config: DeletionBrakeConfig) {
        self.deletion_brake = config;
    }

    /// Pauses or resumes syncing for this folder.
    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        let mut meta = self.meta()?;
        if meta.paused != paused {
            meta.paused = paused;
            let batch = StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            };
            self.store.apply_batch(batch)?;
        }
        Ok(())
    }

    /// Returns whether this folder is currently paused.
    pub fn is_paused(&self) -> Result<bool> {
        Ok(self.meta()?.paused)
    }

    /// Responds to a tripped mass-deletion brake (`ConfirmDeleted` or `Rescan`).
    pub fn resolve_deletion_brake(
        &mut self,
        decision: DeletionBrakeDecision,
        now: Timestamp,
    ) -> Result<ScanReport> {
        self.verify_folder_marker()?;
        let mut meta = self.meta()?;
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
        self.store.apply_batch(batch)?;

        match decision {
            DeletionBrakeDecision::ConfirmDeleted => self.scan_with_options(
                now,
                ScanOptions {
                    allow_mass_deletion: true,
                    ..ScanOptions::default()
                },
            ),
            DeletionBrakeDecision::Rescan => self.scan(now),
        }
    }

    /// Records that `peer` is currently unreachable (reflected as `WaitingForMember` in status).
    pub fn mark_peer_unreachable(&mut self, peer: impl Into<MemberId>) {
        self.unreachable_peers.insert(peer.into());
    }

    /// Clears a peer's unreachable state.
    pub fn mark_peer_reachable(&mut self, peer: &str) {
        self.unreachable_peers.remove(peer);
    }

    /// Adds a member to this folder's member list.
    ///
    /// If the member was previously removed (re-added) or the folder already contains indexed
    /// records, the member is flagged in `rejoining_peers` so Rule D applies on its first sync.
    pub fn add_member(&mut self, member: impl Into<MemberId>) -> Result<()> {
        let member = member.into();
        validate_member_id(&member)?;
        let mut meta = self.meta()?;
        if meta.members.insert(member.clone()) {
            // Only flag a member in `rejoining_peers` when it was previously removed (so it may
            // still carry an old unwiped index from before removal). Brand-new members already tag
            // all pre-existing files with `from_join = true` on their initial scan, leaving
            // post-join edits/renames free to recreate previously deleted content hashes.
            let was_former = meta.former_members.remove(&member);
            if was_former && member != meta.local_member {
                meta.rejoining_peers.insert(member.clone());
            }
            let batch = StoreBatch {
                folder_meta: Some(meta),
                remove_peer_cursors: vec![member.clone()],
                ..StoreBatch::default()
            };
            self.store.apply_batch(batch)?;
            self.peer_problems.remove(&member);
        }
        Ok(())
    }

    /// Removes a member from this folder's member list, stops serving it, retains its name in
    /// `former_members` so historical version vectors remain verifiable, leaves all local files
    /// untouched, and runs tombstone garbage collection.
    pub fn remove_member(&mut self, member: &str) -> Result<usize> {
        let mut meta = self.meta()?;
        if !meta.members.remove(member) {
            return Ok(0);
        }
        meta.former_members.insert(member.to_string());
        meta.rejoining_peers.remove(member);
        self.unreachable_peers.remove(member);
        self.active_progress.remove(member);
        self.peer_problems.remove(member);
        self.deferred_items.retain(|(p, _), _| p != member);

        let batch = StoreBatch {
            folder_meta: Some(meta),
            remove_peer_cursors: vec![member.to_string()],
            ..StoreBatch::default()
        };
        self.store.apply_batch(batch)?;

        self.gc_tombstones()
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
        self.verify_folder_marker()?;
        let report = scan_folder(
            &self.fs,
            &mut self.store,
            &self.ignore,
            self.deletion_brake,
            &mut self.stability,
            now,
            options,
        )?;
        self.skipped_paths = report
            .skipped_errors
            .iter()
            .map(|(p, _)| p.clone())
            .collect();
        self.scan_problems = report
            .skipped_errors
            .iter()
            .map(|(path, reason)| SyncProblem {
                path: path.clone(),
                kind: ProblemKind::FilesystemError,
                peer: None,
                sentence: format!("'{path}' could not be read: {reason}."),
            })
            .collect();
        // Clean up any DirectoryKeptLocalFiles notices for paths that no longer exist on disk.
        let mut still_kept = BTreeMap::new();
        for (path, prob) in std::mem::take(&mut self.dir_kept_problems) {
            if self.fs.stat_live(&path).ok().flatten().is_some() {
                still_kept.insert(path, prob);
            }
        }
        self.dir_kept_problems = still_kept;
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
        self.verify_folder_marker()?;
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
        self.skipped_paths
            .retain(|p| p != &clean_root && !is_descendant_of(p, &clean_root));
        self.scan_problems
            .retain(|p| p.path != clean_root && !is_descendant_of(&p.path, &clean_root));
        for (path, reason) in &report.skipped_errors {
            self.skipped_paths.insert(path.clone());
            self.scan_problems.push(SyncProblem {
                path: path.clone(),
                kind: ProblemKind::FilesystemError,
                peer: None,
                sentence: format!("'{path}' could not be read: {reason}."),
            });
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

        // Refuse syncing with a peer permanently flagged for alternating incarnations.
        // One-time same-seq divergences are checked against `hs.incarnation` in
        // `observe_peer_incarnation` below so a rolled-back machine that rotates to a fresh
        // incarnation can recover while a live clone returning on an old incarnation trips
        // `alternation_detected` permanently.
        let initial_cursors = self.store.load_peer_cursors()?;
        if let Some(existing_cursor) = initial_cursors.get(&peer_id)
            && existing_cursor.alternation_detected
        {
            return Err(SyncError::Conflict {
                path: peer_id.clone(),
                reason: duplicate_member_sentence(&peer_id),
            });
        }

        // Handshake first: check folder ID, member ID, protocol version, and incarnation.
        let hs = match peer.handshake() {
            Ok(hs) => {
                self.unreachable_peers.remove(&peer_id);
                hs
            }
            Err(SyncError::PeerUnavailable { member, detail }) => {
                self.unreachable_peers.insert(peer_id.clone());
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
                member: peer_id,
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

        let meta = if meta.join_mode {
            // Complete the initial join scan before pulling from any peer so any pre-existing
            // local files are tagged `from_join = true` and `join_mode` is cleared before
            // post-sync user edits or renames occur.
            self.scan(now)?;
            self.meta()?
        } else {
            meta
        };

        let mut cached_cursor =
            Some(self.observe_peer_incarnation_with(&peer_id, hs.incarnation, initial_cursors)?);
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
                    reason: duplicate_member_sentence(&peer_id),
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
                );
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
                );
                break;
            }

            // Item C: detect OUR OWN rollback/clone from the peer's direct acknowledgement!
            let local_meta = match cached_meta.take() {
                Some(m) => m,
                None => self.meta()?,
            };
            if self.detect_and_recover_own_rollback_with(&page, &local_meta)? {
                // We rotated to a brand-new incarnation and reset peer cursors; loop again from 0!
                continue;
            }

            // Check if peer changed incarnation between handshake and page (or alternated!).
            if page.incarnation != cursor.incarnation {
                cursor = self.observe_peer_incarnation(&peer_id, page.incarnation)?;
            }

            // Item C: detect peer rollback or same-seq clone within the SAME incarnation.
            if page.incarnation == cursor.incarnation {
                if page.snapshot_seq < cursor.seq {
                    // Peer rolled back and has not yet rotated its incarnation; refuse to lower
                    // our cursor and wait for it to pull from us and rotate.
                    break;
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
                        reason: duplicate_member_sentence(&peer_id),
                    });
                }
            }

            // Item E: update direct acknowledgement of OUR OWN current incarnation ONLY.
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
                if page.snapshot_seq >= cursor.seq {
                    cursor.seq = page.snapshot_seq;
                    if page.head_fingerprint.is_some() {
                        cursor.head_fingerprint = page.head_fingerprint;
                    }
                }
                let clear_rejoin = !page.has_more && local_meta.rejoining_peers.contains(&peer_id);
                if cursor != cursor_before_page || clear_rejoin {
                    self.persist_peer_cursor_and_clear_rejoin(&peer_id, cursor, clear_rejoin)?;
                }
                self.active_progress.remove(&peer_id);
                if ack_advanced {
                    report.tombstones_gc += self.gc_tombstones()?;
                }
                break;
            }

            // Record per-seq fingerprints from this page so partial cursor advances preserve an
            // accurate head_fingerprint.
            let mut seq_fingerprints: HashMap<u64, Hash> =
                HashMap::with_capacity(page.records.len());
            for r in &page.records {
                seq_fingerprints.insert(r.seq, r.head_fingerprint());
            }

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

            // Item K: detect local rename pairs (delete + create with identical content hash
            // already present locally) BEFORE executing individual deletes/fetches!
            let (rename_pairs, remaining_after_renames) =
                self.extract_local_rename_pairs(&peer_id, ordered)?;

            let mut seq_results: BTreeMap<u64, bool> = BTreeMap::new();
            let mut page_held_back: BTreeMap<String, HeldBackRecord> = BTreeMap::new();
            let mut known_held_paths: BTreeSet<String> = if self.store.held_back_count()? > 0 {
                self.store
                    .list_held_back()?
                    .into_iter()
                    .map(|h| h.record.path)
                    .collect()
            } else {
                BTreeSet::new()
            };
            let mut any_deferred = false;
            let mut deferred_files = 0u64;
            let mut deferred_bytes = 0u64;

            for (del_raw, del_tomb, add_raw, add_target) in rename_pairs {
                let del_seq = del_raw.seq;
                let add_seq = add_raw.seq;
                match apply_local_rename(&self.fs, &mut self.store, del_tomb, add_target, now)? {
                    ApplyOutcome::Applied => {
                        seq_results.insert(del_seq, true);
                        seq_results.insert(add_seq, true);
                        report.applied += 2;
                        report.local_renames += 1;
                        self.clear_deferred(&peer_id, &del_raw.path);
                        self.clear_deferred(&peer_id, &add_raw.path);
                    }
                    ApplyOutcome::LocalChangedRetry | ApplyOutcome::Deferred { .. } => {
                        let ok1 = self.process_one_remote(
                            peer,
                            &del_raw,
                            fetch_opts,
                            now,
                            &mut report,
                            &mut page_held_back,
                            &mut known_held_paths,
                        )?;
                        seq_results.insert(del_seq, ok1);
                        let ok2 = self.process_one_remote(
                            peer,
                            &add_raw,
                            fetch_opts,
                            now,
                            &mut report,
                            &mut page_held_back,
                            &mut known_held_paths,
                        )?;
                        seq_results.insert(add_seq, ok2);
                        if !ok1 || !ok2 {
                            any_deferred = true;
                            deferred_files += u64::from(!ok1) + u64::from(!ok2);
                        }
                    }
                    ApplyOutcome::DirKeptLocalFiles(_) => {
                        seq_results.insert(del_seq, true);
                        seq_results.insert(add_seq, true);
                    }
                }
                remaining_files = remaining_files.saturating_sub(2);
            }

            // Split remaining records into Phase 1 (deletions, deepest-first), Phase 2 (live
            // directories, shallowest-first, so every peer-authored directory receives the peer's
            // exact version vector before child files run), and remaining live non-directory
            // records.
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

            // Phase 1: apply deletions first.
            for remote in delete_records {
                if cancel_ref.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    return Err(SyncError::Cancelled {
                        operation: format!("pull from {peer_id}"),
                    });
                }
                let remote_seq = remote.seq;
                let completed = self.process_one_remote(
                    peer,
                    &remote,
                    fetch_opts,
                    now,
                    &mut report,
                    &mut page_held_back,
                    &mut known_held_paths,
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
            }

            // Phase 2: apply live directory records shallowest-first before child files.
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
                    fetch_opts,
                    now,
                    &mut report,
                    &mut page_held_back,
                    &mut known_held_paths,
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
            }

            // Phase 3: partition clean brand-new regular files (tracking full path + ancestor
            // equivalence keys so no two colliding names enter the same batch) and commit them in
            // ONE group-commit transaction BEFORE Phase 4 processes any remaining live records.
            let mut batch_new_files: Vec<IndexRecord> = Vec::new();
            let mut other_live_records: Vec<IndexRecord> = Vec::new();
            {
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
                            if local_absent
                                && !anc_clash
                                && self.fs.stat_live(&record.path)?.is_none()
                            {
                                record.seq = remote.seq;
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

            if !batch_new_files.is_empty() {
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
                for seq in batch_res.applied_seqs {
                    seq_results.insert(seq, true);
                    report.applied += 1;
                }
                for (seq, path, reason) in batch_res.deferred {
                    let completed = self.record_deferral(&peer_id, &path, reason, &mut report)?;
                    seq_results.insert(seq, completed);
                    if !completed {
                        any_deferred = true;
                        deferred_files += 1;
                    }
                }
                for fallback in batch_res.fallback_records {
                    let remote_seq = fallback.seq;
                    let completed = self.process_one_remote(
                        peer,
                        &fallback,
                        fetch_opts,
                        now,
                        &mut report,
                        &mut page_held_back,
                        &mut known_held_paths,
                    )?;
                    seq_results.insert(remote_seq, completed);
                    if !completed {
                        any_deferred = true;
                        deferred_files += 1;
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

            // Phase 4: apply remaining live records (symlinks, replacements, conflicts,
            // and any case-clash candidates now checked against committed Phase 2/3 records).
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
                    fetch_opts,
                    now,
                    &mut report,
                    &mut page_held_back,
                    &mut known_held_paths,
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
            }

            // Flush any batched held-back records from this page in ONE transaction!
            if !page_held_back.is_empty() {
                let batch = StoreBatch {
                    upsert_held_back: page_held_back.into_values().collect(),
                    ..StoreBatch::default()
                };
                self.store.apply_batch(batch)?;
            }

            // Compute highest contiguous processed `seq`.
            let mut contiguous_seq = since_seq;
            let mut all_done = true;
            for (seq, ok) in &seq_results {
                if *ok {
                    contiguous_seq = contiguous_seq.max(*seq);
                } else {
                    all_done = false;
                    break;
                }
            }

            cursor.incarnation = page.incarnation;
            if all_done && !page.has_more {
                cursor.seq = cursor.seq.max(page.snapshot_seq);
                if page.head_fingerprint.is_some() {
                    cursor.head_fingerprint = page.head_fingerprint;
                } else if let Some(&fp) = seq_fingerprints.get(&cursor.seq) {
                    cursor.head_fingerprint = Some(fp);
                }
                self.persist_peer_cursor_and_clear_rejoin(&peer_id, cursor, true)?;
                self.active_progress.remove(&peer_id);
                report.tombstones_gc += self.gc_tombstones()?;
                break;
            } else {
                if contiguous_seq > cursor.seq {
                    cursor.seq = contiguous_seq;
                    if let Some(&fp) = seq_fingerprints.get(&contiguous_seq) {
                        cursor.head_fingerprint = Some(fp);
                    }
                }
                self.persist_peer_cursor_and_clear_rejoin(&peer_id, cursor, false)?;
                report.tombstones_gc += self.gc_tombstones()?;
                if any_deferred || !all_done || contiguous_seq == since_seq {
                    self.update_progress(&peer_id, deferred_files, deferred_bytes);
                    break;
                }
            }
        }

        report.held_back_resolved += self.retry_held_back_for_peer(peer, fetch_opts, now)?;
        Ok(report)
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
        fetch_opts: &FetchOptions,
        now: Timestamp,
        report: &mut PullReport,
        page_held_back: &mut BTreeMap<String, HeldBackRecord>,
        known_held_paths: &mut BTreeSet<String>,
    ) -> Result<bool> {
        let caps = self.fs.capabilities();
        let peer_id = peer.member_id().to_string();

        // Item B1: if this path lies under (or would affect) an unreadable local subtree, hold
        // it back so that subtree waits while the rest of the folder keeps syncing!
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
            self.queue_held_back_bounded(
                &peer_id,
                remote.for_wire(),
                problem,
                page_held_back,
                known_held_paths,
            );
            report.held_back += 1;
            return Ok(true);
        }

        for _attempt in 0..MAX_LOCAL_CHANGE_RETRIES {
            let cur_meta = self.meta()?;
            let decision = {
                let view = StoreIndexView::new(&self.store);
                reconcile_record(&view, remote, &peer_id, &cur_meta, &caps, &self.ignore)
            };

            match decision {
                ReconcileDecision::RejectInvalid { path, reason } => {
                    // Item F / Probe B2: skip and report bad individual records without blocking
                    // the peer forever!
                    self.record_peer_problem(
                        &peer_id,
                        SyncProblem {
                            path: path.clone(),
                            kind: ProblemKind::InvalidPeerRecord,
                            peer: Some(peer_id.clone()),
                            sentence: format!(
                                "Skipped invalid record '{path}' from '{peer_id}': {reason}."
                            ),
                        },
                    );
                    report.invalid_skipped += 1;
                    return Ok(true);
                }
                ReconcileDecision::Ignore => {
                    report.ignored += 1;
                    self.clear_deferred(&peer_id, &remote.path);
                    return Ok(true);
                }
                ReconcileDecision::Keep => {
                    report.kept += 1;
                    page_held_back.remove(&remote.path);
                    if known_held_paths.remove(&remote.path) {
                        self.clear_held_back_if_present(&remote.path)?;
                    }
                    self.clear_deferred(&peer_id, &remote.path);
                    return Ok(true);
                }
                ReconcileDecision::HoldBack { problem } => {
                    self.queue_held_back_bounded(
                        &peer_id,
                        remote.for_wire(),
                        problem,
                        page_held_back,
                        known_held_paths,
                    );
                    report.held_back += 1;
                    self.clear_deferred(&peer_id, &remote.path);
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
                            page_held_back.remove(&remote.path);
                            known_held_paths.remove(&remote.path);
                            self.clear_deferred(&peer_id, &remote.path);
                            return Ok(true);
                        }
                        ApplyOutcome::DirKeptLocalFiles(problem) => {
                            report.applied += 1;
                            page_held_back.remove(&remote.path);
                            known_held_paths.remove(&remote.path);
                            self.dir_kept_problems
                                .entry(problem.path.clone())
                                .or_insert(problem);
                            self.clear_deferred(&peer_id, &remote.path);
                            return Ok(true);
                        }
                        ApplyOutcome::LocalChangedRetry => {
                            continue;
                        }
                        ApplyOutcome::Deferred { reason } => {
                            return self.record_deferral(&peer_id, &remote.path, reason, report);
                        }
                    }
                }
            }
        }

        self.record_deferral(
            &peer_id,
            &remote.path,
            format!("'{}' kept changing locally during sync", remote.path),
            report,
        )
    }

    fn record_deferral(
        &mut self,
        peer_id: &str,
        path: &str,
        reason: String,
        report: &mut PullReport,
    ) -> Result<bool> {
        report.deferred += 1;
        let key = (peer_id.to_string(), path.to_string());
        let attempts = {
            let entry = self
                .deferred_items
                .entry(key.clone())
                .or_insert_with(|| DeferredItem {
                    peer: peer_id.to_string(),
                    path: path.to_string(),
                    reason: reason.clone(),
                    attempts: 0,
                });
            entry.attempts = entry.attempts.saturating_add(1);
            entry.reason = reason.clone();
            entry.attempts
        };

        // Item H: nothing defers forever! Once a record hits MAX_DEFERRAL_ATTEMPTS, promote it to
        // a visible problem and advance past it.
        if attempts >= MAX_DEFERRAL_ATTEMPTS {
            self.deferred_items.remove(&key);
            self.record_peer_problem(
                peer_id,
                SyncProblem {
                    path: path.to_string(),
                    kind: ProblemKind::DeferredLimitExceeded,
                    peer: Some(peer_id.to_string()),
                    sentence: format!(
                        "'{path}' could not be synced after {attempts} attempts: {reason}."
                    ),
                },
            );
            return Ok(true);
        }
        Ok(false)
    }

    fn clear_deferred(&mut self, peer_id: &str, path: &str) {
        self.deferred_items
            .remove(&(peer_id.to_string(), path.to_string()));
    }

    fn queue_held_back_bounded(
        &mut self,
        peer_id: &str,
        record: IndexRecord,
        problem: SyncProblem,
        page_held_back: &mut BTreeMap<String, HeldBackRecord>,
        known_held_paths: &mut BTreeSet<String>,
    ) {
        let already_present = known_held_paths.contains(&record.path);
        if !already_present && known_held_paths.len() >= MAX_HELD_BACK_ENTRIES {
            self.record_peer_problem(
                peer_id,
                SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::HeldBackLimitReached,
                    peer: Some(peer_id.to_string()),
                    sentence: format!(
                        "Held-back problem list reached its limit of {MAX_HELD_BACK_ENTRIES} entries; resolve existing problems to track more."
                    ),
                },
            );
            return;
        }
        known_held_paths.insert(record.path.clone());
        page_held_back.insert(
            record.path.clone(),
            HeldBackRecord {
                record,
                from_peer: peer_id.to_string(),
                problem,
            },
        );
    }

    fn record_peer_problem(&mut self, peer_id: &str, problem: SyncProblem) {
        let list = self.peer_problems.entry(peer_id.to_string()).or_default();
        if list
            .iter()
            .any(|p| p.path == problem.path && p.kind == problem.kind)
        {
            return;
        }
        if list.len() < MAX_PEER_PROBLEMS {
            list.push(problem);
        }
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

    fn retry_held_back_for_peer(
        &mut self,
        peer: &dyn Peer,
        fetch_opts: &FetchOptions,
        now: Timestamp,
    ) -> Result<usize> {
        if self.store.held_back_count()? == 0 {
            return Ok(0);
        }
        let peer_id = peer.member_id().to_string();
        let held = self.store.list_held_back()?;
        let mut resolved = 0usize;
        let caps = self.fs.capabilities();

        for item in held {
            if item.from_peer != peer_id {
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
                ReconcileDecision::HoldBack { .. } | ReconcileDecision::RejectInvalid { .. } => {}
                ReconcileDecision::Keep | ReconcileDecision::Ignore => {
                    self.clear_held_back_if_present(&item.record.path)?;
                    resolved += 1;
                }
                other => {
                    if let Ok(ApplyOutcome::Applied | ApplyOutcome::DirKeptLocalFiles(_)) =
                        apply_decision(
                            &self.fs,
                            &mut self.store,
                            peer,
                            other,
                            &self.ignore,
                            fetch_opts,
                            now,
                        )
                    {
                        self.clear_held_back_if_present(&item.record.path)?;
                        resolved += 1;
                    }
                }
            }
        }
        Ok(resolved)
    }

    fn clear_held_back_if_present(&mut self, path: &str) -> Result<()> {
        let held = self.store.list_held_back()?;
        if held.iter().any(|h| h.record.path == path) {
            let mut batch = StoreBatch::default();
            batch.remove_held_back.push(path.to_string());
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
                    reason: duplicate_member_sentence(peer_id),
                });
            }
            if cursor.prior_incarnations.contains(&new_inc) {
                self.flag_duplicate_member(peer_id, cursor, true)?;
                return Err(SyncError::Conflict {
                    path: peer_id.to_string(),
                    reason: duplicate_member_sentence(peer_id),
                });
            }
            // A rolled-back machine that diverged at the same seq just rotated to a brand-new
            // unseen incarnation: clear the one-time same-seq refusal and record its old
            // incarnation in `prior_incarnations` so any live clone returning with the old
            // incarnation will trip permanent `alternation_detected` immediately!
            cursor.duplicate_detected = false;
            if let Some(list) = self.peer_problems.get_mut(peer_id) {
                list.retain(|p| p.kind != ProblemKind::DuplicateMemberId);
                if list.is_empty() {
                    self.peer_problems.remove(peer_id);
                }
            }
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

        // Legitimate new incarnation (reinstall or rollback recovery): remember the old
        // incarnation and reset the cursor to 0. All pre-existing files on the rotated peer
        // already carry `from_join = true` for Rule D, while post-rotation user edits keep
        // `from_join = false`.
        let old_inc = cursor.incarnation;
        if !cursor.prior_incarnations.contains(&old_inc) {
            cursor.prior_incarnations.push(old_inc);
            if cursor.prior_incarnations.len() > MAX_PRIOR_INCARNATIONS {
                cursor.prior_incarnations.remove(0);
            }
        }
        cursor.incarnation = new_inc;
        cursor.seq = 0;
        cursor.head_fingerprint = None;

        let mut batch = StoreBatch::default();
        batch
            .set_peer_cursors
            .insert(peer_id.to_string(), cursor.clone());
        self.store.apply_batch(batch)?;
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
        let mut batch = StoreBatch::default();
        batch.set_peer_cursors.insert(peer_id.to_string(), cursor);
        self.store.apply_batch(batch)?;
        self.record_peer_problem(
            peer_id,
            SyncProblem {
                path: String::new(),
                kind: ProblemKind::DuplicateMemberId,
                peer: Some(peer_id.to_string()),
                sentence: duplicate_member_sentence(peer_id),
            },
        );
        Ok(())
    }

    fn detect_and_recover_own_rollback_with(
        &mut self,
        page: &RecordsPage,
        meta: &FolderMeta,
    ) -> Result<bool> {
        let Some((acked_inc, acked_seq)) = page.caller_acked else {
            return Ok(false);
        };
        if acked_inc != meta.incarnation {
            return Ok(false);
        }

        let seq_ahead = acked_seq > meta.local_seq;
        let head_diverged = acked_seq == meta.local_seq
            && acked_seq > 0
            && page.caller_acked_fingerprint.is_some()
            && meta.head_fingerprint.is_some()
            && page.caller_acked_fingerprint != meta.head_fingerprint;
        let history_diverged = page.caller_acked_fingerprint.is_some()
            && meta
                .recent_seq_fingerprints
                .get(&acked_seq)
                .is_some_and(|fp| Some(*fp) != page.caller_acked_fingerprint);

        if !seq_ahead && !head_diverged && !history_diverged {
            return Ok(false);
        }

        // Item C: WE rolled back (backup restore or clone)! Start a brand-new random incarnation,
        // update our marker, re-tag every currently live file under the new incarnation with
        // `from_join = true` (Rule D), drop rolled-back tombstones, and reset peer cursors so we
        // pull current state from peers from seq 0!
        let new_inc = choose_fresh_incarnation(None, Some(meta.incarnation))?;
        let marker = FolderMarker::new(meta.folder_id.clone(), meta.local_member.clone(), new_inc);
        self.fs.write_marker(&marker)?;

        let mut new_meta = meta.clone();
        new_meta.incarnation = new_inc;
        new_meta.local_seq = 0;
        new_meta.local_clock = 0;
        new_meta.join_mode = false;
        new_meta.head_fingerprint = None;
        new_meta.recent_seq_fingerprints.clear();

        let vkey = new_meta.version_key();
        let mut live_records = Vec::new();
        let mut remove_paths = Vec::new();
        self.store.scan_records_in_order(&mut |rec| {
            if rec.deleted {
                remove_paths.push(rec.path);
            } else {
                live_records.push(rec);
            }
            Ok(true)
        })?;

        let mut batch = StoreBatch {
            remove_records: remove_paths,
            remove_peer_cursors: self.store.load_peer_cursors()?.into_keys().collect(),
            ..StoreBatch::default()
        };

        for mut rec in live_records {
            let clock = new_meta.next_clock();
            let seq = new_meta.next_seq();
            rec.version = VersionVector::singleton(vkey.clone(), clock);
            rec.seq = seq;
            rec.modified_by = new_meta.local_member.clone();
            rec.from_join = true;
            rec.conflict_of = derive_conflict_of(&rec, &new_meta);
            new_meta.record_seq_fingerprint(seq, rec.head_fingerprint());
            batch.upsert_records.push(rec);
        }

        batch.folder_meta = Some(new_meta);
        self.store.apply_batch(batch)?;
        Ok(true)
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

    /// Garbage-collects tombstones ONLY when every other current member has *directly* reported
    /// acknowledging our current `(incarnation, seq)` past the tombstone's `seq` (Item E: zero
    /// trust in third-party hearsay).
    pub fn gc_tombstones(&mut self) -> Result<usize> {
        let tombstones = self.store.list_tombstones()?;
        if tombstones.is_empty() {
            return Ok(0);
        }
        let meta = self.meta()?;

        let cursors = self.store.load_peer_cursors()?;
        let other_members: Vec<&MemberId> = meta
            .members
            .iter()
            .filter(|m| *m != &meta.local_member)
            .collect();

        // Compute the minimum directly acknowledged seq across every other active member for our
        // current incarnation. If any member has never directly acknowledged this incarnation,
        // `min_direct_ack` is 0 and no tombstone is collected.
        let min_direct_ack = if other_members.is_empty() {
            u64::MAX
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
            return Ok(0);
        }

        let mut to_remove = Vec::new();
        for ts in tombstones {
            if ts.seq <= min_direct_ack {
                to_remove.push(ts.path);
            }
        }

        let removed = to_remove.len();
        if removed > 0 {
            let batch = StoreBatch {
                remove_records: to_remove,
                ..StoreBatch::default()
            };
            self.store.apply_batch(batch)?;
        }
        Ok(removed)
    }

    /// Serves this member's handshake identity (folder ID, member ID, active incarnation,
    /// current `local_seq`, protocol version) to `caller_member`.
    pub fn serve_handshake(&self, caller_member: &str) -> Result<PeerHandshake> {
        let meta = self.check_serve_allowed(caller_member)?;
        Ok(PeerHandshake {
            folder_id: meta.folder_id,
            member_id: meta.local_member,
            incarnation: meta.incarnation,
            snapshot_seq: meta.local_seq,
            protocol_version: SYNC_PROTOCOL_VERSION,
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
        let _meta = self.check_serve_allowed(caller_member)?;
        let capped_limit = limit.clamp(1, MAX_PAGE_RECORDS);
        let mut page =
            self.store
                .records_since(caller_member, since_incarnation, since_seq, capped_limit)?;
        page.records.retain(|r| !is_internal_sync_path(&r.path));
        Ok(page)
    }

    /// Serves a live file's bytes from byte `offset` to `caller_member` after checking membership,
    /// folder marker integrity, path safety, non-ignored status, and expected hash.
    pub fn serve_fetch_file_range(
        &self,
        caller_member: &str,
        path: &str,
        expected_hash: Hash,
        offset: u64,
    ) -> Result<Box<dyn Read + Send>> {
        let _meta = self.check_serve_allowed(caller_member)?;
        let clean = validate_sync_path(path)?;
        if is_internal_sync_path(&clean) || self.ignore.is_path_or_ancestor_ignored(&clean, false) {
            return Err(SyncError::Denied {
                member: caller_member.to_string(),
                reason: format!("'{clean}' is excluded from syncing"),
            });
        }

        let Some(rec) = self.store.get_record(&clean)? else {
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

        self.fs.open_live_read_at(&clean, offset)
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
        let mut entries = self.store.list_bin_entries()?;
        entries.sort_by(|a, b| {
            b.deleted_at
                .cmp(&a.deleted_at)
                .then_with(|| a.original_path.cmp(&b.original_path))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(entries)
    }

    /// Removes entries from `.legion-sync/bin` whose age at `now` meets or exceeds the folder's
    /// configured bin retention (30 days by default), refusing to run when `now` is implausible.
    pub fn expire_bin(&mut self, now: Timestamp) -> Result<Vec<BinEntry>> {
        self.verify_folder_marker()?;
        expire_bin(&self.fs, &mut self.store, self.bin_retention_secs, now)
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
        self.verify_folder_marker()?;
        restore_from_bin(
            &self.fs,
            &mut self.store,
            entry_id,
            dest_path,
            strategy,
            now,
        )
    }

    /// Lists all files currently having two versions (unresolved conflicts).
    pub fn list_conflicts(&self) -> Result<Vec<ConflictInfo>> {
        let mut out = Vec::new();
        for rec in self.store.list_conflicts()? {
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

    /// Resolves one conflict copy according to `choice`.
    pub fn resolve_conflict(
        &mut self,
        conflict_path: &str,
        choice: ConflictChoice,
        now: Timestamp,
    ) -> Result<()> {
        self.verify_folder_marker()?;
        resolve_conflict(&self.fs, &mut self.store, conflict_path, choice, now)
    }

    /// Returns the complete [`FolderStatus`] matching the exact five states from the design,
    /// together with conflicts, bin contents, any tripped deletion brake, and deferred items.
    pub fn status(&self) -> Result<FolderStatus> {
        let meta = self.meta()?;
        let cursors = self.store.load_peer_cursors()?;
        let held = self.store.list_held_back()?;
        let conflicts = self.list_conflicts()?;
        let bin = self.list_bin()?;

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
        all_problems.extend(self.scan_problems.clone());
        all_problems.extend(self.dir_kept_problems.values().cloned());
        all_problems.extend(held.iter().map(|h| h.problem.clone()));
        for probs in self.peer_problems.values() {
            all_problems.extend(probs.iter().cloned());
        }
        for (member, c) in &cursors {
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
                    sentence: duplicate_member_sentence(member),
                });
            }
        }
        all_problems.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.sentence.cmp(&b.sentence))
        });

        let total_files_left: u64 = self.active_progress.values().map(|p| p.files_left).sum();
        let total_bytes_left: u64 = self.active_progress.values().map(|p| p.bytes_left).sum();
        let offline: Vec<MemberId> = self
            .unreachable_peers
            .iter()
            .filter(|m| meta.members.contains(*m) && *m != &meta.local_member)
            .cloned()
            .collect();

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
        } else if !offline.is_empty() {
            MemberState::WaitingForMember {
                members: offline.clone(),
            }
        } else {
            MemberState::UpToDate
        };

        let mut members_vec = Vec::with_capacity(meta.members.len());
        for member in &meta.members {
            if member == &meta.local_member {
                members_vec.push(MemberStatus {
                    member: member.clone(),
                    state: local_state.clone(),
                    cursor_seq: meta.local_seq,
                });
                continue;
            }

            let mut peer_problems: Vec<SyncProblem> = held
                .iter()
                .filter(|h| h.from_peer == *member)
                .map(|h| h.problem.clone())
                .collect();
            if let Some(extra) = self.peer_problems.get(member) {
                peer_problems.extend(extra.iter().cloned());
            }
            if cursors.get(member).is_some_and(|c| c.duplicate_detected)
                && !peer_problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::DuplicateMemberId)
            {
                peer_problems.push(SyncProblem {
                    path: String::new(),
                    kind: ProblemKind::DuplicateMemberId,
                    peer: Some(member.clone()),
                    sentence: duplicate_member_sentence(member),
                });
            }

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
            } else if self.unreachable_peers.contains(member) {
                MemberState::WaitingForMember {
                    members: vec![member.clone()],
                }
            } else {
                MemberState::UpToDate
            };

            members_vec.push(MemberStatus {
                member: member.clone(),
                state: peer_state,
                cursor_seq: cursors.get(member).map_or(0, |c| c.seq),
            });
        }

        let deferred = self.deferred_items.values().cloned().collect();

        Ok(FolderStatus {
            folder_id: meta.folder_id,
            local_member: meta.local_member,
            state: local_state,
            members: members_vec,
            conflicts,
            bin,
            pending_brake: meta.pending_brake,
            deferred,
        })
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
/// Clears old index entries without fabricating deletions and enters `join_mode = true` so the
/// next scan indexes the actual files on disk under Rule D and peers supply anything missing.
fn rotate_incarnation_on_marker_mismatch<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut meta: FolderMeta,
    marker_inc: u64,
) -> Result<()> {
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
    meta.recent_seq_fingerprints.clear();

    let batch = StoreBatch {
        folder_meta: Some(meta),
        remove_records: all_paths,
        remove_peer_cursors: all_peers,
        set_pending_op: Some(None),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)
}

fn duplicate_member_sentence(peer_id: &str) -> String {
    format!(
        "Two different machines appear to be using the same member name '{peer_id}'. Rename or remove one before syncing with it again."
    )
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
