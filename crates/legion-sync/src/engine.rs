//! High-level hubless folder synchronization engine orchestrating scan, reconcile, apply,
//! tombstone garbage collection, bin management, conflict resolution, and status reporting.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::apply::{
    ApplyOutcome, RecoveryReport, StoreIndexView, apply_decision, expire_bin, recover_pending,
    resolve_conflict, restore_from_bin,
};
use crate::error::{Result, SyncError};
use crate::model::{
    BinEntry, ConflictChoice, ConflictInfo, FolderConfig, FolderMeta, FolderStatus, HeldBackRecord,
    IndexRecord, MemberId, MemberState, MemberStatus, ProblemKind, RecordsPage, RestoreStrategy,
    SyncProblem, SyncProgress,
};
use crate::path::{IgnoreMatcher, is_internal_sync_path, path_depth, validate_sync_path};
use crate::reconcile::{ReconcileDecision, reconcile_record};
use crate::scan::{PendingStabilityTracker, ScanOptions, ScanReport, scan_folder};
use crate::traits::{IndexStore, Peer, StoreBatch, SyncFs};

/// Default number of records fetched per incremental page during [`SyncEngine::pull_from_peer`].
pub const DEFAULT_PAGE_LIMIT: usize = 1024;

/// Maximum consecutive retry attempts on a single path if local edits race with apply.
const MAX_LOCAL_CHANGE_RETRIES: usize = 4;

/// Summary of one pull-only sync pass against a peer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullReport {
    /// Total peer records inspected across all pages.
    pub records_seen: usize,
    /// Records applied (fetched, deleted, merged, or conflicted).
    pub applied: usize,
    /// Records kept as-is (local already equal or newer).
    pub kept: usize,
    /// Records ignored by fixed or per-folder ignore rules.
    pub ignored: usize,
    /// Records held back as platform problems (case clash, Windows name, symlink, length).
    pub held_back: usize,
    /// Records deferred because a local file was actively changing.
    pub deferred: usize,
    /// Tombstones garbage-collected after updating the causal matrix.
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
    stability: PendingStabilityTracker,
    unreachable_peers: BTreeSet<MemberId>,
    active_progress: BTreeMap<MemberId, SyncProgress>,
    scan_problems: Vec<SyncProblem>,
}

impl<F: SyncFs, S: IndexStore> SyncEngine<F, S> {
    /// Opens or initializes a sync engine for `config`, running crash recovery immediately.
    pub fn open(config: FolderConfig, fs: F, mut store: S) -> Result<Self> {
        config.validate()?;
        let ignore = IgnoreMatcher::compile(&config.ignore)?;

        match store.load_meta()? {
            None => {
                let meta = FolderMeta::new(&config);
                let mut matrix = crate::model::CausalMatrix::new();
                matrix.ensure_members(&meta.members);
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    causal_matrix: Some(matrix),
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
            }
        }

        let mut engine = Self {
            fs,
            store,
            ignore,
            bin_retention_secs: config.bin_retention_secs,
            stability: PendingStabilityTracker::new(),
            unreachable_peers: BTreeSet::new(),
            active_progress: BTreeMap::new(),
            scan_problems: Vec::new(),
        };
        engine.recover()?;
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
    /// Existing tombstones remain protected until the new member has also acknowledged them.
    pub fn add_member(&mut self, member: impl Into<MemberId>) -> Result<()> {
        let member = member.into();
        crate::model::validate_member_id(&member)?;
        let mut meta = self.meta()?;
        if meta.members.insert(member.clone()) {
            let mut matrix = self.store.load_causal_matrix()?;
            matrix.ensure_members(&meta.members);
            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
                ..StoreBatch::default()
            };
            self.store.apply_batch(batch)?;
        }
        Ok(())
    }

    /// Removes a member from this folder's member list, stops serving it, leaves all local files
    /// untouched, and runs tombstone garbage collection for deletions that were only waiting on it.
    ///
    /// Returns the number of tombstones garbage-collected.
    pub fn remove_member(&mut self, member: &str) -> Result<usize> {
        let mut meta = self.meta()?;
        if !meta.members.remove(member) {
            return Ok(0);
        }
        self.unreachable_peers.remove(member);
        self.active_progress.remove(member);

        let mut matrix = self.store.load_causal_matrix()?;
        matrix.remove_observer(member);

        let batch = StoreBatch {
            folder_meta: Some(meta),
            causal_matrix: Some(matrix),
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
        let report = scan_folder(
            &self.fs,
            &mut self.store,
            &self.ignore,
            &mut self.stability,
            now,
            options,
        )?;
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
        Ok(report)
    }

    /// Pulls incremental index changes from `peer` using [`DEFAULT_PAGE_LIMIT`] and applies them
    /// crash-safely.
    pub fn pull_from_peer(&mut self, peer: &dyn Peer, now: Timestamp) -> Result<PullReport> {
        self.pull_from_peer_with_limit(peer, DEFAULT_PAGE_LIMIT, now)
    }

    /// Pulls incremental index changes from `peer` with an explicit page size.
    pub fn pull_from_peer_with_limit(
        &mut self,
        peer: &dyn Peer,
        page_limit: usize,
        now: Timestamp,
    ) -> Result<PullReport> {
        let meta = self.meta()?;
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

        let limit = page_limit.max(1);
        let mut report = PullReport::default();

        loop {
            let cursors = self.store.load_peer_cursors()?;
            let since_seq = cursors.get(&peer_id).copied().unwrap_or(0);

            let page = match peer.records_since(since_seq, limit) {
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

            report.records_seen += page.records.len();

            if page.records.is_empty() {
                // Even with 0 new records, absorb the peer's causal matrix so tombstone GC
                // learns when every member has seen prior deletions.
                self.advance_peer_state_full(&peer_id, page.snapshot_seq, &page.matrix)?;
                self.active_progress.remove(&peer_id);
                report.tombstones_gc += self.gc_tombstones()?;
                break;
            }

            // Estimate remaining files and bytes for `MemberState::Syncing`.
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

            // Order records in the page: deletions deepest-first so removed children precede
            // directory removals and dir->file replacements, then live entries shallowest-first so
            // parent directories precede children.
            let mut ordered: Vec<(usize, IndexRecord)> =
                page.records.into_iter().enumerate().collect();
            ordered.sort_by(|(_, a), (_, b)| match (a.deleted, b.deleted) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (true, true) => path_depth(&b.path)
                    .cmp(&path_depth(&a.path))
                    .then_with(|| a.seq.cmp(&b.seq)),
                (false, false) => path_depth(&a.path)
                    .cmp(&path_depth(&b.path))
                    .then_with(|| a.seq.cmp(&b.seq)),
            });

            // Track which original sequence numbers in the page completed cleanly so we advance
            // `peer_cursor` only over a contiguous completed prefix.
            let mut seq_results: BTreeMap<u64, bool> = BTreeMap::new();
            let mut any_deferred = false;
            let mut deferred_files = 0u64;
            let mut deferred_bytes = 0u64;

            for (_, remote) in ordered {
                let remote_seq = remote.seq;
                let remote_bytes = if !remote.deleted && remote.kind == EntryKind::File {
                    remote.size
                } else {
                    0
                };

                let completed = self.process_one_remote(peer, &remote, now, &mut report)?;
                seq_results.insert(remote_seq, completed);
                if !completed {
                    any_deferred = true;
                    deferred_files += 1;
                    deferred_bytes += remote_bytes;
                }

                remaining_files = remaining_files.saturating_sub(1);
                remaining_bytes = remaining_bytes.saturating_sub(remote_bytes);
                let files_left = remaining_files + deferred_files;
                let bytes_left = remaining_bytes + deferred_bytes;
                if files_left > 0 {
                    self.active_progress.insert(
                        peer_id.clone(),
                        SyncProgress {
                            files_left,
                            bytes_left,
                        },
                    );
                } else {
                    self.active_progress.remove(&peer_id);
                }
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

            if all_done && !page.has_more {
                self.advance_peer_state_full(&peer_id, page.snapshot_seq, &page.matrix)?;
                self.active_progress.remove(&peer_id);
                report.tombstones_gc += self.gc_tombstones()?;
                break;
            } else {
                self.advance_peer_state_partial(&peer_id, contiguous_seq, &page.matrix)?;
                report.tombstones_gc += self.gc_tombstones()?;
                if any_deferred || !all_done || contiguous_seq == since_seq {
                    if deferred_files > 0 {
                        self.active_progress.insert(
                            peer_id.clone(),
                            SyncProgress {
                                files_left: deferred_files,
                                bytes_left: deferred_bytes,
                            },
                        );
                    }
                    break;
                }
            }
        }

        // Re-evaluate previously held-back records from this peer after applying incoming records
        // so a deletion or rename in this pull immediately unblocks any resolved case clash.
        report.held_back_resolved += self.retry_held_back_for_peer(peer, now)?;

        Ok(report)
    }

    fn process_one_remote(
        &mut self,
        peer: &dyn Peer,
        remote: &IndexRecord,
        now: Timestamp,
        report: &mut PullReport,
    ) -> Result<bool> {
        let caps = self.fs.capabilities();
        let peer_id = peer.member_id();

        for _attempt in 0..MAX_LOCAL_CHANGE_RETRIES {
            let decision = {
                let view = StoreIndexView::new(&self.store);
                reconcile_record(&view, remote, peer_id, &caps, &self.ignore)
            };

            match decision {
                ReconcileDecision::RejectUnsafe { path, reason } => {
                    return Err(SyncError::InvalidPath { path, reason });
                }
                ReconcileDecision::Ignore => {
                    report.ignored += 1;
                    return Ok(true);
                }
                ReconcileDecision::Keep => {
                    report.kept += 1;
                    // If a previously held-back record is now superseded or equal locally, clear it.
                    self.clear_held_back_if_present(&remote.path)?;
                    return Ok(true);
                }
                ReconcileDecision::HoldBack { problem } => {
                    let mut batch = StoreBatch::default();
                    batch.upsert_held_back.push(HeldBackRecord {
                        record: remote.for_wire(),
                        from_peer: peer_id.to_string(),
                        problem,
                    });
                    self.store.apply_batch(batch)?;
                    report.held_back += 1;
                    return Ok(true);
                }
                other => match apply_decision(&self.fs, &mut self.store, peer, other, now)? {
                    ApplyOutcome::Applied => {
                        report.applied += 1;
                        return Ok(true);
                    }
                    ApplyOutcome::LocalChangedRetry => {
                        continue;
                    }
                    ApplyOutcome::DeferredUnstable => {
                        report.deferred += 1;
                        return Ok(false);
                    }
                },
            }
        }

        report.deferred += 1;
        Ok(false)
    }

    fn retry_held_back_for_peer(&mut self, peer: &dyn Peer, now: Timestamp) -> Result<usize> {
        let peer_id = peer.member_id();
        let held = self.store.list_held_back()?;
        let mut resolved = 0usize;
        let caps = self.fs.capabilities();

        for item in held {
            if item.from_peer != peer_id {
                continue;
            }
            let decision = {
                let view = StoreIndexView::new(&self.store);
                reconcile_record(&view, &item.record, peer_id, &caps, &self.ignore)
            };
            match decision {
                ReconcileDecision::HoldBack { .. } | ReconcileDecision::RejectUnsafe { .. } => {
                    // Still blocked by the same filesystem rule.
                }
                ReconcileDecision::Keep | ReconcileDecision::Ignore => {
                    self.clear_held_back_if_present(&item.record.path)?;
                    resolved += 1;
                }
                other => {
                    if let Ok(ApplyOutcome::Applied) =
                        apply_decision(&self.fs, &mut self.store, peer, other, now)
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

    fn advance_peer_state_full(
        &mut self,
        peer_id: &str,
        snapshot_seq: u64,
        peer_matrix: &crate::model::CausalMatrix,
    ) -> Result<()> {
        let meta = self.meta()?;
        let mut matrix = self.store.load_causal_matrix()?;
        matrix.absorb_peer_full(&meta.local_member, peer_id, snapshot_seq, peer_matrix);

        let mut batch = StoreBatch::default();
        batch
            .set_peer_cursors
            .insert(peer_id.to_string(), snapshot_seq);
        batch.causal_matrix = Some(matrix);
        self.store.apply_batch(batch)
    }

    fn advance_peer_state_partial(
        &mut self,
        peer_id: &str,
        processed_seq: u64,
        peer_matrix: &crate::model::CausalMatrix,
    ) -> Result<()> {
        let meta = self.meta()?;
        let mut matrix = self.store.load_causal_matrix()?;
        matrix.absorb_peer_partial(&meta.local_member, peer_id, processed_seq, peer_matrix);

        let mut batch = StoreBatch::default();
        batch
            .set_peer_cursors
            .insert(peer_id.to_string(), processed_seq);
        batch.causal_matrix = Some(matrix);
        self.store.apply_batch(batch)
    }

    /// Garbage-collects tombstones that every current member has causally acknowledged.
    ///
    /// Returns the number of tombstones removed.
    pub fn gc_tombstones(&mut self) -> Result<usize> {
        let meta = self.meta()?;
        let mut matrix = self.store.load_causal_matrix()?;
        let tombstones = self.store.list_tombstones()?;
        if tombstones.is_empty() {
            return Ok(0);
        }

        let mut to_remove = Vec::new();
        let mut remaining_dep_origins: BTreeSet<MemberId> = BTreeSet::new();

        for ts in tombstones {
            if matrix.all_members_seen(&meta.members, &ts.tombstone_deps) {
                to_remove.push(ts.path);
            } else {
                for origin in ts.tombstone_deps.keys() {
                    remaining_dep_origins.insert(origin.clone());
                }
            }
        }

        let removed = to_remove.len();
        matrix.prune_unused_origins(&meta.members, &remaining_dep_origins);

        if removed > 0 {
            let batch = StoreBatch {
                remove_records: to_remove,
                causal_matrix: Some(matrix),
                ..StoreBatch::default()
            };
            self.store.apply_batch(batch)?;
        }
        Ok(removed)
    }

    /// Serves incremental records after `since_seq` to `caller_member`.
    ///
    /// Refuses callers not listed in this folder's `members` and never emits any path inside
    /// `.legion-sync/` (bin or tmp).
    pub fn serve_records_since(
        &self,
        caller_member: &str,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage> {
        let meta = self.meta()?;
        if !meta.members.contains(caller_member) {
            return Err(SyncError::Denied {
                member: caller_member.to_string(),
                reason: "caller is not a member of this sync folder".to_string(),
            });
        }
        let mut page = self.store.records_since(since_seq, limit.max(1))?;
        // Defense in depth: never expose `.legion-sync/` even if corrupted data somehow entered.
        page.records.retain(|r| !is_internal_sync_path(&r.path));
        Ok(page)
    }

    /// Serves a live file's bytes to `caller_member` after checking membership, path safety,
    /// non-ignored status, and expected hash.
    pub fn serve_fetch_file(
        &self,
        caller_member: &str,
        path: &str,
        expected_hash: Hash,
    ) -> Result<Box<dyn Read + Send>> {
        let meta = self.meta()?;
        if !meta.members.contains(caller_member) {
            return Err(SyncError::Denied {
                member: caller_member.to_string(),
                reason: "caller is not a member of this sync folder".to_string(),
            });
        }
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

        self.fs.open_live_read(&clean)
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
    /// configured bin retention (30 days by default).
    pub fn expire_bin(&mut self, now: Timestamp) -> Result<Vec<BinEntry>> {
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
            if rec.deleted {
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
        resolve_conflict(&self.fs, &mut self.store, conflict_path, choice, now)
    }

    /// Returns the complete [`FolderStatus`] matching the exact five states from the design,
    /// together with the list of files with two versions and the bin's contents.
    pub fn status(&self) -> Result<FolderStatus> {
        let meta = self.meta()?;
        let cursors = self.store.load_peer_cursors()?;
        let held = self.store.list_held_back()?;
        let conflicts = self.list_conflicts()?;
        let bin = self.list_bin()?;

        let mut all_problems: Vec<SyncProblem> = self.scan_problems.clone();
        all_problems.extend(held.iter().map(|h| h.problem.clone()));
        all_problems.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
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

        let local_state = if meta.paused {
            MemberState::Paused
        } else if !all_problems.is_empty() {
            MemberState::Problem {
                sentence: summarize_problems(&all_problems),
                problems: all_problems.clone(),
            }
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

        let mut members_map = BTreeMap::new();
        for member in &meta.members {
            if member == &meta.local_member {
                members_map.insert(
                    member.clone(),
                    MemberStatus {
                        member: member.clone(),
                        state: local_state.clone(),
                        cursor_seq: meta.local_seq,
                    },
                );
                continue;
            }

            let peer_problems: Vec<SyncProblem> = held
                .iter()
                .filter(|h| h.from_peer == *member)
                .map(|h| h.problem.clone())
                .collect();
            let peer_state = if meta.paused {
                MemberState::Paused
            } else if !peer_problems.is_empty() {
                MemberState::Problem {
                    sentence: summarize_problems(&peer_problems),
                    problems: peer_problems,
                }
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

            members_map.insert(
                member.clone(),
                MemberStatus {
                    member: member.clone(),
                    state: peer_state,
                    cursor_seq: cursors.get(member).copied().unwrap_or(0),
                },
            );
        }

        Ok(FolderStatus {
            folder_id: meta.folder_id,
            local_member: meta.local_member,
            state: local_state,
            members: members_map,
            conflicts,
            bin,
        })
    }
}

fn summarize_problems(problems: &[SyncProblem]) -> String {
    if problems.len() == 1 {
        return problems[0].sentence.clone();
    }
    format!(
        "{} files could not be synced on this machine (for example: {})",
        problems.len(),
        problems[0].sentence
    )
}
