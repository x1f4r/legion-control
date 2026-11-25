//! Folder scanner converting live filesystem state into incremental index updates.
//!
//! Unchanged entries (matching kind, size, modification time, local change id, and Unix mode) are
//! skipped in a single streaming merge against the sorted index without re-hashing, unless
//! [`ScanOptions::verify_checksums`] is enabled. Unreadable directories or entries never invent
//! deletions, and the mass-deletion brake pauses before committing large-scale deletions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::{FsErrorKind, Result, SyncError};
use crate::model::{
    DeletionBrake, DeletionBrakeConfig, DeletionWindowState, FolderDeletionLogEntry, IndexRecord,
    MAX_ANCESTRY_HASHES, PendingLocalDeletion, ProblemKind, SyncProblem,
    encode_deletion_log_entries,
};
use crate::path::{
    IgnoreMatcher, check_symlink_target_contained, is_descendant_of, is_legion_inflight_name,
    is_os_litter_name, path_depth, validate_sync_path,
};
use crate::reconcile::derive_conflict_of;
use crate::traits::{FsMeta, FsWalkEntry, IndexStore, StoreBatch, SyncFs};
use crate::version::VersionVector;

/// Options controlling how [`scan_folder`] and [`scan_subtree`] inspect the local disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// When `true`, newly created or modified regular files must show the same `(size, mtime)`
    /// across two consecutive scans before being hashed and committed to the index.
    pub require_second_look: bool,
    /// Minimum age (`now - mtime`) a new or modified regular file must reach before being
    /// committed. Files newer than this window are held back as still changing.
    pub min_stable_age: Duration,
    /// When `true`, compute BLAKE3 checksums for *every* regular file even if `(size, mtime,
    /// change_id)` appear unchanged (for periodic integrity audits or detecting same-size
    /// same-mtime overwrites on filesystems without reliable change ids).
    pub verify_checksums: bool,
    /// Internal override set when the user explicitly confirms a tripped mass-deletion brake.
    pub allow_mass_deletion: bool,
}

/// Summary of one folder or subtree scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Newly discovered live entries added to the index.
    pub created: usize,
    /// Existing entries whose content, link target, or Unix mode changed.
    pub modified: usize,
    /// Previously live entries missing from disk and recorded as tombstones.
    pub deleted: usize,
    /// Entries whose kind changed (for example file replaced by directory or back).
    pub kind_changed: usize,
    /// Entries that matched the index and required no writes.
    pub unchanged: usize,
    /// Regular files whose bytes were streamed through BLAKE3 during this scan.
    pub hashed: usize,
    /// Paths held back because they were still changing between looks.
    pub held_back_unstable: Vec<String>,
    /// Paths that could not be inspected during the walk (`(path, reason)`).
    pub skipped_errors: Vec<(String, String)>,
    /// Local entries held back by outbound policy (for example absolute or escaping symbolic links).
    pub held_back_problems: Vec<SyncProblem>,
    /// `true` when the mass-deletion brake tripped, pausing the folder and recording 0 deletions.
    pub brake_tripped: bool,
}

/// Tracks `(size, mtime)` of files awaiting a second stable look across consecutive scans.
#[derive(Debug, Default, Clone)]
pub struct PendingStabilityTracker {
    observed: BTreeMap<String, (u64, Timestamp)>,
}

impl PendingStabilityTracker {
    /// Creates an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    fn retain_paths(&mut self, active: &BTreeSet<String>) {
        self.observed.retain(|k, _| active.contains(k));
    }

    fn confirm(&mut self, path: &str, size: u64, mtime: Timestamp) -> bool {
        if self.observed.get(path) == Some(&(size, mtime)) {
            self.observed.remove(path);
            true
        } else {
            self.observed.insert(path.to_string(), (size, mtime));
            false
        }
    }
}

/// Outcome of inspecting a single live path during apply's pre-replace re-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SingleRescanOutcome {
    /// Index was updated with a new local record (created, modified, kind-changed, or deleted).
    Updated(Box<IndexRecord>),
    /// Disk already matched the stored index record.
    Unchanged,
    /// File is actively changing between looks and must wait.
    Unstable,
}

struct PendingUpsert {
    path: String,
    meta: FsMeta,
    hash: Option<Hash>,
    prev: Option<IndexRecord>,
    is_kind_change: bool,
}

enum PendingChange {
    Delete { old: IndexRecord },
    Upsert(PendingUpsert),
    RefreshDiskStatOnly { record: IndexRecord },
}

fn is_busy_live_error<F: SyncFs>(fs: &F, err: &SyncError) -> bool {
    matches!(
        err,
        SyncError::Fs {
            kind: FsErrorKind::Busy,
            ..
        }
    ) || (fs.capabilities().windows_naming
        && matches!(
            err,
            SyncError::Fs {
                kind: FsErrorKind::PermissionDenied,
                ..
            }
        ))
}

/// Returns `true` when `path` is confirmed absent on disk (`Ok(None)`). Transient sharing or
/// delete-pending errors treat the entry as still present so scans never invent a deletion.
fn is_confirmed_absent_live<F: SyncFs>(fs: &F, path: &str) -> Result<bool> {
    match fs.stat_live(path) {
        Ok(None) => Ok(true),
        Ok(Some(_)) => Ok(false),
        Err(err) if is_busy_live_error(fs, &err) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Returns `true` if `candidate` equals or lies under any skipped/unreadable path.
pub fn is_under_skipped(candidate: &str, skipped: &[(String, String)]) -> bool {
    skipped.iter().any(|(skip_path, _)| {
        skip_path.is_empty() || candidate == skip_path || is_descendant_of(candidate, skip_path)
    })
}

/// Scans the entire live folder `fs` against `store`, protecting unreadable subtrees and enforcing
/// the mass-deletion brake, and commits all index updates in one atomic transaction.
#[allow(clippy::too_many_arguments)]
pub fn scan_folder<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    ignore: &IgnoreMatcher,
    brake_config: DeletionBrakeConfig,
    tracker: &mut PendingStabilityTracker,
    now: Timestamp,
    options: ScanOptions,
) -> Result<ScanReport> {
    let walk = fs.walk(ignore)?;
    scan_walk_against_store(
        fs,
        store,
        None,
        walk.entries,
        walk.skipped,
        ignore,
        brake_config,
        tracker,
        now,
        options,
    )
}

/// Scans only the entry or subtree rooted at `rel_root` against `store` without walking unrelated
/// parts of the folder (intended for file watchers).
#[allow(clippy::too_many_arguments)]
pub fn scan_subtree<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_root: &str,
    ignore: &IgnoreMatcher,
    brake_config: DeletionBrakeConfig,
    tracker: &mut PendingStabilityTracker,
    now: Timestamp,
    options: ScanOptions,
) -> Result<ScanReport> {
    let clean_root = validate_sync_path(rel_root)?;
    let walk = fs.walk_subtree(&clean_root, ignore)?;
    scan_walk_against_store(
        fs,
        store,
        Some(&clean_root),
        walk.entries,
        walk.skipped,
        ignore,
        brake_config,
        tracker,
        now,
        options,
    )
}

#[allow(clippy::too_many_arguments)]
fn scan_walk_against_store<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    subtree_root: Option<&str>,
    disk_entries: Vec<FsWalkEntry>,
    walk_skipped: Vec<(String, String)>,
    ignore: &IgnoreMatcher,
    brake_config: DeletionBrakeConfig,
    tracker: &mut PendingStabilityTracker,
    now: Timestamp,
    options: ScanOptions,
) -> Result<ScanReport> {
    let mut meta = match store.load_meta()? {
        Some(m) => m,
        None => {
            return Err(crate::error::SyncError::Storage {
                context: "scan_folder".to_string(),
                detail: "index store is not initialized with folder metadata".to_string(),
            });
        }
    };

    if meta.pending_brake.is_some() && !options.allow_mass_deletion {
        return Ok(ScanReport {
            skipped_errors: walk_skipped,
            brake_tripped: true,
            ..ScanReport::default()
        });
    }

    let mut report = ScanReport {
        skipped_errors: walk_skipped.clone(),
        ..ScanReport::default()
    };

    let mut changes: Vec<PendingChange> = Vec::new();
    let mut still_tracking: BTreeSet<String> = BTreeSet::new();
    let mut kept_dirs_seen: BTreeSet<String> = BTreeSet::new();
    let mut kept_dirs_revived: BTreeSet<String> = BTreeSet::new();
    let mut cleared_rescan_paths: BTreeSet<String> = BTreeSet::new();
    let mut disk_idx = 0usize;

    let mut visit_record = |rec: IndexRecord| -> Result<bool> {
        while disk_idx < disk_entries.len() && disk_entries[disk_idx].path < rec.path {
            let entry = &disk_entries[disk_idx];
            if entry.meta.kind == EntryKind::Dir && meta.kept_deleted_dirs.contains(&entry.path) {
                kept_dirs_seen.insert(entry.path.clone());
                if dir_holds_only_local_non_synced_files(fs, &entry.path, ignore)? {
                    report.unchanged += 1;
                    disk_idx += 1;
                    continue;
                }
                kept_dirs_revived.insert(entry.path.clone());
            }
            if handle_new_disk_entry(
                fs,
                entry,
                tracker,
                &mut still_tracking,
                now,
                options,
                &mut changes,
                &mut report,
            )? && meta.needs_rescan.contains(&entry.path)
            {
                cleared_rescan_paths.insert(entry.path.clone());
            }
            disk_idx += 1;
        }

        if disk_idx < disk_entries.len() && disk_entries[disk_idx].path == rec.path {
            let entry = &disk_entries[disk_idx];
            if entry.meta.kind == EntryKind::Dir && meta.kept_deleted_dirs.contains(&entry.path) {
                kept_dirs_seen.insert(entry.path.clone());
                if !dir_holds_only_local_non_synced_files(fs, &entry.path, ignore)? {
                    kept_dirs_revived.insert(entry.path.clone());
                }
            }
            let force_hash = meta.needs_rescan.contains(&entry.path);
            if handle_existing_disk_entry(
                fs,
                entry,
                rec,
                ignore,
                force_hash,
                tracker,
                &mut still_tracking,
                now,
                options,
                &mut changes,
                &mut report,
            )? && force_hash
            {
                cleared_rescan_paths.insert(entry.path.clone());
            }
            disk_idx += 1;
        } else if !rec.deleted
            && !ignore.is_path_or_ancestor_ignored(&rec.path, rec.kind == EntryKind::Dir)
            && !is_under_skipped(&rec.path, &walk_skipped)
        {
            if meta.needs_rescan.contains(&rec.path) {
                cleared_rescan_paths.insert(rec.path.clone());
            }
            changes.push(PendingChange::Delete { old: rec });
        } else if rec.deleted && meta.needs_rescan.contains(&rec.path) {
            cleared_rescan_paths.insert(rec.path.clone());
        }

        Ok(true)
    };

    match subtree_root {
        None => store.scan_records_in_order(&mut visit_record)?,
        Some(root) => store.scan_subtree_in_order(root, &mut visit_record)?,
    }

    while disk_idx < disk_entries.len() {
        let entry = &disk_entries[disk_idx];
        if entry.meta.kind == EntryKind::Dir && meta.kept_deleted_dirs.contains(&entry.path) {
            kept_dirs_seen.insert(entry.path.clone());
            if dir_holds_only_local_non_synced_files(fs, &entry.path, ignore)? {
                report.unchanged += 1;
                disk_idx += 1;
                continue;
            }
            kept_dirs_revived.insert(entry.path.clone());
        }
        if handle_new_disk_entry(
            fs,
            entry,
            tracker,
            &mut still_tracking,
            now,
            options,
            &mut changes,
            &mut report,
        )? && meta.needs_rescan.contains(&entry.path)
        {
            cleared_rescan_paths.insert(entry.path.clone());
        }
        disk_idx += 1;
    }

    let mut kept_dirs_changed = false;
    for p in kept_dirs_revived {
        if meta.kept_deleted_dirs.remove(&p) {
            kept_dirs_changed = true;
        }
    }
    if subtree_root.is_none() && !meta.kept_deleted_dirs.is_empty() {
        let before = meta.kept_deleted_dirs.len();
        meta.kept_deleted_dirs
            .retain(|p| kept_dirs_seen.contains(p) || is_under_skipped(p, &walk_skipped));
        if meta.kept_deleted_dirs.len() != before {
            kept_dirs_changed = true;
        }
    }
    for p in cleared_rescan_paths {
        if meta.needs_rescan.remove(&p) {
            kept_dirs_changed = true;
        }
    }
    if subtree_root.is_none() && !meta.needs_rescan.is_empty() {
        let before = meta.needs_rescan.len();
        // Clear any needs_rescan paths that are neither on disk nor unstable/skipped (e.g.
        // disappeared before they were ever indexed).
        let unstable_set: BTreeSet<&str> = report
            .held_back_unstable
            .iter()
            .map(String::as_str)
            .collect();
        meta.needs_rescan.retain(|p| {
            unstable_set.contains(p.as_str()) || is_under_skipped(p, &report.skipped_errors)
        });
        if meta.needs_rescan.len() != before {
            kept_dirs_changed = true;
        }
    }

    if subtree_root.is_none() {
        tracker.retain_paths(&still_tracking);
    }

    // Re-filter deletions against any errors added during hashing/statting.
    let mut raw_deletes: Vec<IndexRecord> = Vec::new();
    let mut upserts: Vec<PendingUpsert> = Vec::new();
    let mut stat_refreshes: Vec<IndexRecord> = Vec::new();

    for change in changes {
        match change {
            PendingChange::Delete { old } => {
                if !is_under_skipped(&old.path, &report.skipped_errors) {
                    raw_deletes.push(old);
                }
            }
            PendingChange::Upsert(item) => upserts.push(item),
            PendingChange::RefreshDiskStatOnly { record } => stat_refreshes.push(record),
        }
    }

    // Any path present on disk in this scan cancels a previously pending local deletion for it.
    let seen_live_paths: BTreeSet<&str> = disk_entries.iter().map(|e| e.path.as_str()).collect();
    let mut brake_meta_dirty = false;
    let mut held_del_log_entries: Vec<FolderDeletionLogEntry> = Vec::new();
    if !meta.pending_deletions.is_empty() {
        let mut revived_pending = Vec::new();
        meta.pending_deletions.retain(|p, _| {
            if seen_live_paths.contains(p.as_str()) {
                revived_pending.push(p.clone());
                false
            } else {
                true
            }
        });
        if !revived_pending.is_empty() {
            brake_meta_dirty = true;
            for p in revived_pending {
                held_del_log_entries.push(FolderDeletionLogEntry::Revived { path: p });
            }
        }
    }

    // Item 8: move-aware, windowed outbound mass-deletion brake (30s settle + 10m rolling window).
    const MAX_RECENT_CREATED_HASHES: usize = 50_000;
    let settle_secs = brake_config.settle_secs as i64;
    let window_secs = brake_config.window_secs as i64;
    let current_live_files = store.live_file_count()? as usize;

    let had_window = meta.deletion_window.is_some();
    let mut window = meta
        .deletion_window
        .take()
        .unwrap_or_else(|| DeletionWindowState {
            window_start: now,
            window_start_live_files: current_live_files,
            emitted_deletions: Vec::new(),
            recent_created_hashes: Vec::new(),
        });

    let before_recent = window.recent_created_hashes.len();
    window
        .recent_created_hashes
        .retain(|(_, _, t)| now.secs >= t.secs && (now.secs - t.secs) <= settle_secs);
    if window.recent_created_hashes.len() != before_recent {
        brake_meta_dirty = true;
    }

    let before_emitted = window.emitted_deletions.len();
    window.emitted_deletions.retain(|(p, t)| {
        now.secs >= t.secs
            && (now.secs - t.secs) <= window_secs
            && !seen_live_paths.contains(p.as_str())
    });
    if window.emitted_deletions.len() != before_emitted {
        brake_meta_dirty = true;
    }

    if window.emitted_deletions.is_empty() && meta.pending_deletions.is_empty() {
        window.window_start = now;
        window.window_start_live_files = current_live_files;
    } else if now.secs < window.window_start.secs
        || (now.secs - window.window_start.secs) > window_secs
    {
        let mut oldest = now;
        for (_, t) in &window.emitted_deletions {
            if t.secs < oldest.secs {
                oldest = *t;
            }
        }
        for pd in meta.pending_deletions.values() {
            if pd.detected_at.secs < oldest.secs {
                oldest = pd.detected_at;
            }
        }
        window.window_start = oldest;
        window.window_start_live_files = current_live_files + window.emitted_deletions.len();
        brake_meta_dirty = true;
    }

    // Build per-hash list of newly created/modified paths in this scan for move matching.
    let mut upsert_paths_by_hash: BTreeMap<Hash, Vec<String>> = BTreeMap::new();
    for u in &upserts {
        if u.meta.kind == EntryKind::File
            && let Some(h) = u.hash
            && u.prev
                .as_ref()
                .is_none_or(|p| p.deleted || p.hash != Some(h))
        {
            upsert_paths_by_hash
                .entry(h)
                .or_default()
                .push(u.path.clone());
        }
    }

    let mut matched_move_deletes: Vec<IndexRecord> = Vec::new();

    // Cross-scan move recognition: if a previously pending deletion's content hash just appeared
    // at a DIFFERENT path within the settle period, promote it as a move (never counted by the
    // brake).
    if !meta.pending_deletions.is_empty() && !upsert_paths_by_hash.is_empty() {
        let mut moved_paths: Vec<String> = Vec::new();
        for (p, pd) in &meta.pending_deletions {
            if pd.old_record.kind == EntryKind::File
                && let Some(h) = pd.old_record.hash
                && let Some(paths) = upsert_paths_by_hash.get_mut(&h)
                && let Some(idx) = paths.iter().position(|up| up != p)
            {
                paths.swap_remove(idx);
                moved_paths.push(p.clone());
            }
        }
        for p in moved_paths {
            if let Some(pd) = meta.pending_deletions.remove(&p) {
                brake_meta_dirty = true;
                if is_confirmed_absent_live(fs, &p)? {
                    matched_move_deletes.push(pd.old_record);
                }
            }
        }
    }

    // Partition this scan's deletions into moves (matching a DIFFERENT path in this scan's upserts
    // or recent created files within `settle_secs`), unmatched regular-file deletions, and
    // dir/symlink deletions.
    let mut unmatched_file_deletes: Vec<IndexRecord> = Vec::new();
    let mut dir_and_other_deletes: Vec<IndexRecord> = Vec::new();
    for old in raw_deletes {
        if old.kind == EntryKind::File {
            let mut is_move = false;
            if let Some(h) = old.hash {
                if let Some(paths) = upsert_paths_by_hash.get_mut(&h)
                    && let Some(idx) = paths.iter().position(|up| up != &old.path)
                {
                    paths.swap_remove(idx);
                    is_move = true;
                } else if let Some(pos) = window
                    .recent_created_hashes
                    .iter()
                    .position(|(rp, rh, _)| *rh == h && rp != &old.path)
                {
                    window.recent_created_hashes.swap_remove(pos);
                    brake_meta_dirty = true;
                    is_move = true;
                }
            }
            // Drop any stale recent-created record for the deleted path itself so a second
            // deletion of another identical file cannot match a vanished path.
            let prev_len = window.recent_created_hashes.len();
            window
                .recent_created_hashes
                .retain(|(rp, _, _)| rp != &old.path);
            if window.recent_created_hashes.len() != prev_len {
                brake_meta_dirty = true;
            }
            if is_move {
                matched_move_deletes.push(old);
            } else {
                unmatched_file_deletes.push(old);
            }
        } else {
            dir_and_other_deletes.push(old);
        }
    }

    // Record remaining unmatched new file `(path, hash)` entries so a follow-up subtree scan that
    // notices the vanished old path within `settle_secs` recognizes the move too.
    if brake_config.is_enabled() && settle_secs > 0 && !upsert_paths_by_hash.is_empty() {
        for (h, paths) in upsert_paths_by_hash {
            for p in paths {
                if window.recent_created_hashes.len() >= MAX_RECENT_CREATED_HASHES {
                    break;
                }
                window.recent_created_hashes.push((p, h, now));
                brake_meta_dirty = true;
            }
        }
    }

    let effective_baseline = window.window_start_live_files.max(current_live_files);
    let brake_active = !options.allow_mass_deletion
        && brake_config.is_enabled()
        && effective_baseline > brake_config.min_deleted_files;

    let mut newly_held_deletions: Vec<PendingLocalDeletion> = Vec::new();
    let mut deletes: Vec<IndexRecord> = matched_move_deletes;
    if !brake_active {
        // Either the user confirmed the mass deletion, the brake is disabled, or the folder is
        // small enough that the brake can never trip: emit all deletions immediately.
        deletes.extend(unmatched_file_deletes);
        deletes.extend(dir_and_other_deletes);
        if !meta.pending_deletions.is_empty() {
            let drained = std::mem::take(&mut meta.pending_deletions);
            brake_meta_dirty = true;
            for (p, pd) in drained {
                if !is_under_skipped(&p, &report.skipped_errors)
                    && is_confirmed_absent_live(fs, &p)?
                    && !deletes.iter().any(|d| d.path == p)
                {
                    deletes.push(pd.old_record);
                }
            }
        }
        if options.allow_mass_deletion {
            let confirmed_files = deletes.iter().filter(|d| d.kind == EntryKind::File).count();
            window.emitted_deletions.clear();
            window.window_start = now;
            window.window_start_live_files = current_live_files.saturating_sub(confirmed_files);
            brake_meta_dirty = true;
        }
    } else {
        // Hold unmatched regular-file deletions pending for the settle period before peers see
        // them, preserving each path's first `detected_at`.
        for old in unmatched_file_deletes {
            let path = old.path.clone();
            if let std::collections::btree_map::Entry::Vacant(e) =
                meta.pending_deletions.entry(path)
            {
                let pd = PendingLocalDeletion {
                    old_record: old,
                    detected_at: now,
                };
                newly_held_deletions.push(pd.clone());
                e.insert(pd);
                brake_meta_dirty = true;
            }
        }
        for old in dir_and_other_deletes {
            if old.kind == EntryKind::Dir {
                let has_pending_child = meta
                    .pending_deletions
                    .keys()
                    .any(|k| is_descendant_of(k, &old.path));
                if has_pending_child {
                    let path = old.path.clone();
                    if let std::collections::btree_map::Entry::Vacant(e) =
                        meta.pending_deletions.entry(path)
                    {
                        let pd = PendingLocalDeletion {
                            old_record: old,
                            detected_at: now,
                        };
                        newly_held_deletions.push(pd.clone());
                        e.insert(pd);
                        brake_meta_dirty = true;
                    }
                } else {
                    deletes.push(old);
                }
            } else {
                deletes.push(old);
            }
        }

        // Count distinct file deletions across BOTH pending deletions and already-emitted
        // deletions in the rolling window against the window-start live file count!
        let mut distinct_deleted_files: BTreeSet<&str> = BTreeSet::new();
        for (p, _) in &window.emitted_deletions {
            distinct_deleted_files.insert(p.as_str());
        }
        for (p, pd) in &meta.pending_deletions {
            if pd.old_record.kind == EntryKind::File {
                distinct_deleted_files.insert(p.as_str());
            }
        }
        let total_window_deleted = distinct_deleted_files.len();
        if brake_config.should_trip(total_window_deleted, effective_baseline) {
            let mut sample_paths: Vec<String> = meta
                .pending_deletions
                .values()
                .filter(|pd| pd.old_record.kind == EntryKind::File)
                .take(10)
                .map(|pd| pd.old_record.path.clone())
                .collect();
            if sample_paths.len() < 10 {
                for (p, _) in &window.emitted_deletions {
                    if sample_paths.len() >= 10 {
                        break;
                    }
                    if !sample_paths.contains(p) {
                        sample_paths.push(p.clone());
                    }
                }
            }
            meta.paused = true;
            meta.pending_brake = Some(DeletionBrake {
                deleted_count: total_window_deleted,
                total_live_files: effective_baseline,
                sample_paths,
            });
            meta.deletion_window = Some(window);
            let batch = StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            report.brake_tripped = true;
            report.deleted = 0;
            return Ok(report);
        }

        // Promote any pending deletions whose settle period has elapsed (checking disk first).
        let mut ready_files: Vec<String> = Vec::new();
        for (p, pd) in &meta.pending_deletions {
            if pd.old_record.kind != EntryKind::Dir
                && !is_under_skipped(p, &report.skipped_errors)
                && (settle_secs == 0
                    || (now.secs >= pd.detected_at.secs
                        && (now.secs - pd.detected_at.secs) >= settle_secs))
            {
                ready_files.push(p.clone());
            }
        }
        for p in ready_files {
            if let Some(pd) = meta.pending_deletions.remove(&p) {
                brake_meta_dirty = true;
                if is_confirmed_absent_live(fs, &p)? {
                    if pd.old_record.kind == EntryKind::File
                        && !window.emitted_deletions.iter().any(|(ep, _)| ep == &p)
                    {
                        window.emitted_deletions.push((p, now));
                    }
                    deletes.push(pd.old_record);
                }
            }
        }

        // Promote pending directories once no descendant remains pending (either settled or moved).
        let ready_dirs: Vec<String> = meta
            .pending_deletions
            .iter()
            .filter(|(p, pd)| {
                pd.old_record.kind == EntryKind::Dir
                    && !is_under_skipped(p, &report.skipped_errors)
                    && !meta
                        .pending_deletions
                        .keys()
                        .any(|other| is_descendant_of(other, p))
            })
            .map(|(p, _)| p.clone())
            .collect();
        for p in ready_dirs {
            if let Some(pd) = meta.pending_deletions.remove(&p) {
                brake_meta_dirty = true;
                if is_confirmed_absent_live(fs, &p)? {
                    deletes.push(pd.old_record);
                }
            }
        }
    }

    // Record newly held deletions in `.legion-sync/deletions.log` now that we know the brake did
    // NOT trip, so rebuilding from disk after index damage during the settle window preserves them.
    for pd in newly_held_deletions {
        if meta.pending_deletions.contains_key(&pd.old_record.path) {
            held_del_log_entries.push(FolderDeletionLogEntry::from_pending_deletion(
                &pd,
                &meta.local_member,
            ));
        }
    }

    let keep_window = !window.emitted_deletions.is_empty()
        || !window.recent_created_hashes.is_empty()
        || !meta.pending_deletions.is_empty();
    if keep_window {
        meta.deletion_window = Some(window);
        if !had_window {
            brake_meta_dirty = true;
        }
    } else if had_window {
        brake_meta_dirty = true;
    }

    let finish_join_mode = subtree_root.is_none() && meta.join_mode;
    if deletes.is_empty() && upserts.is_empty() && stat_refreshes.is_empty() {
        if !held_del_log_entries.is_empty() {
            let encoded = encode_deletion_log_entries(&held_del_log_entries)?;
            fs.append_deletion_log(&encoded)?;
        }
        if finish_join_mode || kept_dirs_changed || brake_meta_dirty {
            if finish_join_mode {
                meta.join_mode = false;
            }
            let batch = StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
        }
        return Ok(report);
    }

    // Emit deletions deepest-first so children receive smaller seq numbers than their parent
    // directories (and before any directory-to-file replacement on the same path).
    deletes.sort_by(|a, b| {
        path_depth(&b.path)
            .cmp(&path_depth(&a.path))
            .then_with(|| a.path.cmp(&b.path))
    });

    // Emit live creates/updates shallowest-first so parent directories always precede children.
    upserts.sort_by(|a, b| {
        path_depth(&a.path)
            .cmp(&path_depth(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut batch = StoreBatch::default();
    let join_mode_active = meta.join_mode;

    for rec in stat_refreshes {
        batch.upsert_records.push(rec);
    }

    let mut meta_dirty = kept_dirs_changed || brake_meta_dirty;
    let mut del_log_entries: Vec<FolderDeletionLogEntry> = held_del_log_entries;

    for old in deletes {
        let mut version = old.version;
        meta.bump_version(&mut version);
        let seq = meta.next_seq();
        meta.record_change_mark(seq, now);
        meta_dirty = true;

        let mut recent_hashes = old.recent_hashes;
        if let Some(h) = old.hash {
            recent_hashes.retain(|x| *x != h);
            recent_hashes.insert(0, h);
            recent_hashes.truncate(MAX_ANCESTRY_HASHES);
        }

        let rec = IndexRecord {
            path: old.path,
            kind: old.kind,
            size: 0,
            mtime: now,
            disk_mtime: None,
            disk_change_id: None,
            mode: None,
            hash: None,
            link_target: None,
            deleted: true,
            version,
            modified_by: meta.local_member.clone(),
            seq,
            recent_hashes,
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        };
        let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        del_log_entries.push(FolderDeletionLogEntry::from_tombstone(&rec));
        batch.upsert_records.push(rec);
        report.deleted += 1;
    }

    for PendingUpsert {
        path,
        meta: fmeta,
        hash,
        prev,
        is_kind_change,
    } in upserts
    {
        let is_new = prev.as_ref().is_none_or(|p| p.deleted);
        let mut version = prev
            .as_ref()
            .map_or_else(VersionVector::new, |p| p.version.clone());
        meta.bump_version(&mut version);
        let seq = meta.next_seq();
        meta.record_change_mark(seq, now);
        meta_dirty = true;

        let mode = fmeta.mode.or_else(|| {
            prev.as_ref()
                .filter(|p| !p.deleted && p.kind == fmeta.kind)
                .and_then(|p| p.mode)
        });

        let mut recent_hashes = prev
            .as_ref()
            .map_or_else(Vec::new, |p| p.recent_hashes.clone());
        if let Some(ref p) = prev
            && !p.deleted
            && let Some(old_h) = p.hash
            && Some(old_h) != hash
        {
            recent_hashes.retain(|h| *h != old_h);
            recent_hashes.insert(0, old_h);
            recent_hashes.truncate(MAX_ANCESTRY_HASHES);
        }

        let conflict_dismissed = prev
            .as_ref()
            .is_some_and(|p| !p.deleted && !is_kind_change && p.conflict_dismissed);

        let from_join = prev.is_none() && join_mode_active;
        if from_join {
            meta.unconfirmed_join_until_seq = meta.unconfirmed_join_until_seq.max(seq);
        }

        let mut rec = IndexRecord {
            path,
            kind: fmeta.kind,
            size: if fmeta.kind == EntryKind::File {
                fmeta.size
            } else {
                0
            },
            mtime: fmeta.mtime,
            disk_mtime: Some(fmeta.mtime),
            disk_change_id: fmeta.change_id,
            mode,
            hash,
            link_target: fmeta.link_target,
            deleted: false,
            version,
            modified_by: meta.local_member.clone(),
            seq,
            recent_hashes,
            from_join,
            conflict_of: None,
            conflict_dismissed,
        };
        rec.conflict_of = derive_conflict_of(&rec, &meta);
        let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        batch.upsert_records.push(rec);

        if is_kind_change {
            report.kind_changed += 1;
        } else if is_new {
            report.created += 1;
        } else {
            report.modified += 1;
        }
    }

    if !del_log_entries.is_empty() {
        let encoded = encode_deletion_log_entries(&del_log_entries)?;
        fs.append_deletion_log(&encoded)?;
    }

    if finish_join_mode {
        meta.join_mode = false;
        meta_dirty = true;
    }
    if meta_dirty {
        batch.folder_meta = Some(meta);
    }

    store.apply_batch(batch)?;
    Ok(report)
}

/// Promotes any pending local deletions whose settle period has elapsed at `now` without needing
/// a full folder walk, verifying each path is still absent on disk and still enforcing the rolling
/// window mass-deletion brake.
pub fn flush_settled_deletions<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    brake_config: DeletionBrakeConfig,
    now: Timestamp,
) -> Result<usize> {
    let Some(mut meta) = store.load_meta()? else {
        return Ok(0);
    };
    if meta.paused || meta.pending_brake.is_some() || meta.pending_deletions.is_empty() {
        return Ok(0);
    }

    let settle_secs = brake_config.settle_secs as i64;
    let window_secs = brake_config.window_secs as i64;
    let current_live_files = store.live_file_count()? as usize;

    let mut window = meta
        .deletion_window
        .take()
        .unwrap_or_else(|| DeletionWindowState {
            window_start: now,
            window_start_live_files: current_live_files,
            emitted_deletions: Vec::new(),
            recent_created_hashes: Vec::new(),
        });
    window
        .emitted_deletions
        .retain(|(_, t)| now.secs >= t.secs && (now.secs - t.secs) <= window_secs);
    window
        .recent_created_hashes
        .retain(|(_, _, t)| now.secs >= t.secs && (now.secs - t.secs) <= settle_secs);

    let effective_baseline = window.window_start_live_files.max(current_live_files);
    if brake_config.is_enabled() {
        let mut distinct: BTreeSet<&str> = BTreeSet::new();
        for (p, _) in &window.emitted_deletions {
            distinct.insert(p.as_str());
        }
        for (p, pd) in &meta.pending_deletions {
            if pd.old_record.kind == EntryKind::File {
                distinct.insert(p.as_str());
            }
        }
        if brake_config.should_trip(distinct.len(), effective_baseline) {
            let sample_paths = meta
                .pending_deletions
                .values()
                .filter(|pd| pd.old_record.kind == EntryKind::File)
                .take(10)
                .map(|pd| pd.old_record.path.clone())
                .collect();
            meta.paused = true;
            meta.pending_brake = Some(DeletionBrake {
                deleted_count: distinct.len(),
                total_live_files: effective_baseline,
                sample_paths,
            });
            meta.deletion_window = Some(window);
            store.apply_batch(StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            })?;
            return Ok(0);
        }
    }

    let mut deletes = Vec::new();
    let mut revived_paths = Vec::new();
    let mut dirty = false;
    let ready_files: Vec<String> = meta
        .pending_deletions
        .iter()
        .filter(|(_, pd)| {
            pd.old_record.kind != EntryKind::Dir
                && (settle_secs == 0
                    || (now.secs >= pd.detected_at.secs
                        && (now.secs - pd.detected_at.secs) >= settle_secs))
        })
        .map(|(p, _)| p.clone())
        .collect();
    for p in ready_files {
        let absent = match fs.stat_live(&p) {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(err) if is_busy_live_error(fs, &err) => continue,
            Err(err) => return Err(err),
        };
        if let Some(pd) = meta.pending_deletions.remove(&p) {
            dirty = true;
            if absent {
                if pd.old_record.kind == EntryKind::File
                    && !window.emitted_deletions.iter().any(|(ep, _)| ep == &p)
                {
                    window.emitted_deletions.push((p, now));
                }
                deletes.push(pd.old_record);
            } else {
                revived_paths.push(p);
            }
        }
    }

    let ready_dirs: Vec<String> = meta
        .pending_deletions
        .iter()
        .filter(|(p, pd)| {
            pd.old_record.kind == EntryKind::Dir
                && !meta
                    .pending_deletions
                    .keys()
                    .any(|other| is_descendant_of(other, p))
        })
        .map(|(p, _)| p.clone())
        .collect();
    for p in ready_dirs {
        let absent = match fs.stat_live(&p) {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(err) if is_busy_live_error(fs, &err) => continue,
            Err(err) => return Err(err),
        };
        if let Some(pd) = meta.pending_deletions.remove(&p) {
            dirty = true;
            if absent {
                deletes.push(pd.old_record);
            } else {
                revived_paths.push(p);
            }
        }
    }

    let keep_window = !window.emitted_deletions.is_empty()
        || !window.recent_created_hashes.is_empty()
        || !meta.pending_deletions.is_empty();
    meta.deletion_window = keep_window.then_some(window);

    if deletes.is_empty() {
        if !revived_paths.is_empty() {
            let entries: Vec<FolderDeletionLogEntry> = revived_paths
                .into_iter()
                .map(|path| FolderDeletionLogEntry::Revived { path })
                .collect();
            let encoded = encode_deletion_log_entries(&entries)?;
            fs.append_deletion_log(&encoded)?;
        }
        if dirty {
            store.apply_batch(StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            })?;
        }
        return Ok(0);
    }

    deletes.sort_by(|a, b| {
        path_depth(&b.path)
            .cmp(&path_depth(&a.path))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut batch = StoreBatch::default();
    let mut del_log_entries: Vec<FolderDeletionLogEntry> = revived_paths
        .into_iter()
        .map(|path| FolderDeletionLogEntry::Revived { path })
        .collect();
    let count = deletes.len();
    for old in deletes {
        let mut version = old.version;
        meta.bump_version(&mut version);
        let seq = meta.next_seq();
        meta.record_change_mark(seq, now);
        let mut recent_hashes = old.recent_hashes;
        if let Some(h) = old.hash {
            recent_hashes.retain(|x| *x != h);
            recent_hashes.insert(0, h);
            recent_hashes.truncate(MAX_ANCESTRY_HASHES);
        }
        let rec = IndexRecord {
            path: old.path,
            kind: old.kind,
            size: 0,
            mtime: now,
            disk_mtime: None,
            disk_change_id: None,
            mode: None,
            hash: None,
            link_target: None,
            deleted: true,
            version,
            modified_by: meta.local_member.clone(),
            seq,
            recent_hashes,
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        };
        let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        del_log_entries.push(FolderDeletionLogEntry::from_tombstone(&rec));
        batch.upsert_records.push(rec);
    }
    if !del_log_entries.is_empty() {
        let encoded = encode_deletion_log_entries(&del_log_entries)?;
        fs.append_deletion_log(&encoded)?;
    }
    batch.folder_meta = Some(meta);
    store.apply_batch(batch)?;
    Ok(count)
}

/// Rescans a single relative path on disk and commits any detected local change atomically.
pub fn rescan_single_path<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_path: &str,
    now: Timestamp,
) -> Result<SingleRescanOutcome> {
    rescan_single_path_with_force(fs, store, rel_path, now, false)
}

/// Rescans a single relative path on disk; when `force_bump` is `true` (used after the engine
/// already displaced the previously indexed file and a person concurrently placed a new file at
/// `rel_path`), a live entry on disk always receives a bumped version even if its content hash
/// matches the displaced record's hash (for example when the person renamed a conflict copy back
/// into place or undid an edit).
pub fn rescan_single_path_with_force<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_path: &str,
    now: Timestamp,
    force_bump: bool,
) -> Result<SingleRescanOutcome> {
    let clean = validate_sync_path(rel_path)?;
    let mut meta = store
        .load_meta()?
        .ok_or_else(|| crate::error::SyncError::Storage {
            context: "rescan_single_path".to_string(),
            detail: "index store is not initialized".to_string(),
        })?;
    let prev = store.get_record(&clean)?;
    let disk_opt = match fs.stat_live(&clean) {
        Ok(opt) => opt,
        Err(err) if is_busy_live_error(fs, &err) => return Ok(SingleRescanOutcome::Unstable),
        Err(err) => return Err(err),
    };

    match (prev, disk_opt) {
        (None, None) => {
            if meta.needs_rescan.remove(&clean) {
                store.apply_batch(StoreBatch {
                    folder_meta: Some(meta),
                    ..StoreBatch::default()
                })?;
            }
            Ok(SingleRescanOutcome::Unchanged)
        }
        (Some(ref rec), None) if rec.deleted => {
            if meta.needs_rescan.remove(&clean) {
                store.apply_batch(StoreBatch {
                    folder_meta: Some(meta),
                    ..StoreBatch::default()
                })?;
            }
            Ok(SingleRescanOutcome::Unchanged)
        }
        (Some(old), None) => {
            meta.needs_rescan.remove(&clean);
            // If an indexed directory disappeared from disk, any live indexed descendants under
            // it have necessarily vanished too; tombstone them deepest-first in the same batch so
            // stale ghost subdirectories are never served before the next full scan.
            let mut to_tombstone: Vec<IndexRecord> = Vec::new();
            if old.kind == EntryKind::Dir {
                store.scan_subtree_in_order(&clean, &mut |rec| {
                    if !rec.deleted && rec.path != clean {
                        to_tombstone.push(rec);
                    }
                    Ok(true)
                })?;
                to_tombstone.sort_by(|a, b| {
                    path_depth(&b.path)
                        .cmp(&path_depth(&a.path))
                        .then_with(|| a.path.cmp(&b.path))
                });
            }
            to_tombstone.push(old);

            let mut upsert_records = Vec::with_capacity(to_tombstone.len());
            let mut upsert_seq_fingerprints = Vec::with_capacity(to_tombstone.len());
            let mut del_log_entries = Vec::with_capacity(to_tombstone.len());
            let mut main_record = None;
            for item in to_tombstone {
                let is_main = item.path == clean;
                let mut version = item.version;
                meta.bump_version(&mut version);
                let seq = meta.next_seq();
                meta.record_change_mark(seq, now);
                let mut recent_hashes = item.recent_hashes;
                if let Some(h) = item.hash {
                    recent_hashes.retain(|x| *x != h);
                    recent_hashes.insert(0, h);
                    recent_hashes.truncate(MAX_ANCESTRY_HASHES);
                }
                let rec = IndexRecord {
                    path: item.path,
                    kind: item.kind,
                    size: 0,
                    mtime: now,
                    disk_mtime: None,
                    disk_change_id: None,
                    mode: None,
                    hash: None,
                    link_target: None,
                    deleted: true,
                    version,
                    modified_by: meta.local_member.clone(),
                    seq,
                    recent_hashes,
                    from_join: false,
                    conflict_of: None,
                    conflict_dismissed: false,
                };
                let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
                upsert_seq_fingerprints.push((seq, chain));
                del_log_entries.push(FolderDeletionLogEntry::from_tombstone(&rec));
                if is_main {
                    main_record = Some(rec.clone());
                }
                upsert_records.push(rec);
            }
            let record = main_record.ok_or_else(|| crate::error::SyncError::Storage {
                context: "rescan_single_path".to_string(),
                detail: "missing main tombstone record".to_string(),
            })?;
            if !del_log_entries.is_empty() {
                let encoded = encode_deletion_log_entries(&del_log_entries)?;
                fs.append_deletion_log(&encoded)?;
            }
            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records,
                upsert_seq_fingerprints,
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(SingleRescanOutcome::Updated(Box::new(record)))
        }
        (prev_opt, Some(disk_meta)) => {
            if disk_meta.kind == EntryKind::Symlink
                && check_symlink_target_contained(
                    &clean,
                    disk_meta.link_target.as_deref().unwrap_or_default(),
                )
                .is_err()
            {
                return Ok(SingleRescanOutcome::Unchanged);
            }
            let hash = if disk_meta.kind == EntryKind::File {
                let (h, bytes_read) = match fs.hash_live(&clean) {
                    Ok(pair) => pair,
                    Err(SyncError::Fs {
                        kind: FsErrorKind::NotFound,
                        ..
                    })
                    | Err(SyncError::Conflict { .. }) => {
                        return Ok(SingleRescanOutcome::Unstable);
                    }
                    Err(err) if is_busy_live_error(fs, &err) => {
                        return Ok(SingleRescanOutcome::Unstable);
                    }
                    Err(err) => return Err(err),
                };
                let after = match fs.stat_live(&clean) {
                    Ok(opt) => opt,
                    Err(err) if is_busy_live_error(fs, &err) => {
                        return Ok(SingleRescanOutcome::Unstable);
                    }
                    Err(err) => return Err(err),
                };
                let stable = after.as_ref().is_some_and(|m| {
                    m.kind == EntryKind::File
                        && m.size == disk_meta.size
                        && m.mtime == disk_meta.mtime
                        && m.change_id == disk_meta.change_id
                        && m.file_identity == disk_meta.file_identity
                        && bytes_read == disk_meta.size
                });
                if !stable {
                    return Ok(SingleRescanOutcome::Unstable);
                }
                Some(h)
            } else {
                None
            };

            if !force_bump
                && let Some(ref existing) = prev_opt
                && !existing.deleted
                && existing.kind == disk_meta.kind
                && existing.hash == hash
                && existing.link_target == disk_meta.link_target
                && !mode_changed(existing.mode, disk_meta.mode)
            {
                let cleared = meta.needs_rescan.remove(&clean);
                let stat_diff = existing.disk_mtime != Some(disk_meta.mtime)
                    || existing.disk_change_id != disk_meta.change_id;
                if stat_diff || cleared {
                    let mut batch = StoreBatch::default();
                    if stat_diff {
                        let mut refreshed = existing.clone();
                        refreshed.disk_mtime = Some(disk_meta.mtime);
                        refreshed.disk_change_id = disk_meta.change_id;
                        batch.upsert_records.push(refreshed);
                    }
                    if cleared {
                        batch.folder_meta = Some(meta);
                    }
                    store.apply_batch(batch)?;
                }
                return Ok(SingleRescanOutcome::Unchanged);
            }

            meta.needs_rescan.remove(&clean);

            let is_kind_change = prev_opt
                .as_ref()
                .is_some_and(|p| !p.deleted && p.kind != disk_meta.kind);
            let mut version = prev_opt
                .as_ref()
                .map_or_else(VersionVector::new, |p| p.version.clone());
            meta.bump_version(&mut version);
            let seq = meta.next_seq();
            meta.record_change_mark(seq, now);
            let mode = disk_meta.mode.or_else(|| {
                prev_opt
                    .as_ref()
                    .filter(|p| !p.deleted && p.kind == disk_meta.kind)
                    .and_then(|p| p.mode)
            });

            let mut recent_hashes = prev_opt
                .as_ref()
                .map_or_else(Vec::new, |p| p.recent_hashes.clone());
            if let Some(ref p) = prev_opt
                && !p.deleted
                && let Some(old_h) = p.hash
                && Some(old_h) != hash
            {
                recent_hashes.retain(|h| *h != old_h);
                recent_hashes.insert(0, old_h);
                recent_hashes.truncate(MAX_ANCESTRY_HASHES);
            }
            let conflict_dismissed = prev_opt
                .as_ref()
                .is_some_and(|p| !p.deleted && !is_kind_change && p.conflict_dismissed);

            let mut record = IndexRecord {
                path: clean,
                kind: disk_meta.kind,
                size: if disk_meta.kind == EntryKind::File {
                    disk_meta.size
                } else {
                    0
                },
                mtime: disk_meta.mtime,
                disk_mtime: Some(disk_meta.mtime),
                disk_change_id: disk_meta.change_id,
                mode,
                hash,
                link_target: disk_meta.link_target,
                deleted: false,
                version,
                modified_by: meta.local_member.clone(),
                seq,
                recent_hashes,
                from_join: false,
                conflict_of: None,
                conflict_dismissed,
            };
            record.conflict_of = derive_conflict_of(&record, &meta);
            let chain = meta.advance_chain_fingerprint(seq, record.head_fingerprint());
            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records: vec![record.clone()],
                upsert_seq_fingerprints: vec![(seq, chain)],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(SingleRescanOutcome::Updated(Box::new(record)))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_new_disk_entry<F: SyncFs>(
    fs: &F,
    entry: &FsWalkEntry,
    tracker: &mut PendingStabilityTracker,
    still_tracking: &mut BTreeSet<String>,
    now: Timestamp,
    options: ScanOptions,
    changes: &mut Vec<PendingChange>,
    report: &mut ScanReport,
) -> Result<bool> {
    if entry.meta.kind == EntryKind::Symlink
        && let Err(reason) = check_symlink_target_contained(
            &entry.path,
            entry.meta.link_target.as_deref().unwrap_or_default(),
        )
    {
        report.held_back_problems.push(SyncProblem {
            path: entry.path.clone(),
            kind: ProblemKind::UnsupportedSymlink,
            peer: None,
            sentence: format!("'{}' was held back because {reason}.", entry.path),
        });
        return Ok(true);
    }

    let hash = match entry.meta.kind {
        EntryKind::File => {
            match check_and_hash_file(
                fs,
                &entry.path,
                &entry.meta,
                tracker,
                still_tracking,
                now,
                options,
                report,
            )? {
                Some(h) => Some(h),
                None => return Ok(false),
            }
        }
        EntryKind::Dir | EntryKind::Symlink => None,
    };

    changes.push(PendingChange::Upsert(PendingUpsert {
        path: entry.path.clone(),
        meta: entry.meta.clone(),
        hash,
        prev: None,
        is_kind_change: false,
    }));
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn handle_existing_disk_entry<F: SyncFs>(
    fs: &F,
    entry: &FsWalkEntry,
    rec: IndexRecord,
    ignore: &IgnoreMatcher,
    force_hash: bool,
    tracker: &mut PendingStabilityTracker,
    still_tracking: &mut BTreeSet<String>,
    now: Timestamp,
    options: ScanOptions,
    changes: &mut Vec<PendingChange>,
    report: &mut ScanReport,
) -> Result<bool> {
    if entry.meta.kind == EntryKind::Symlink
        && let Err(reason) = check_symlink_target_contained(
            &entry.path,
            entry.meta.link_target.as_deref().unwrap_or_default(),
        )
    {
        report.held_back_problems.push(SyncProblem {
            path: entry.path.clone(),
            kind: ProblemKind::UnsupportedSymlink,
            peer: None,
            sentence: format!("'{}' was held back because {reason}.", entry.path),
        });
        return Ok(true);
    }
    // Item H: a directory that was deleted elsewhere and intentionally kept on disk only because
    // it still contains local non-synced entries (ignored files or unsupported local symlinks)
    // must NOT be revived as a new directory on scan.
    if rec.deleted
        && entry.meta.kind == EntryKind::Dir
        && dir_holds_only_local_non_synced_files(fs, &entry.path, ignore)?
    {
        report.unchanged += 1;
        return Ok(true);
    }

    if !rec.deleted
        && !force_hash
        && !(options.verify_checksums && entry.meta.kind == EntryKind::File)
        && is_stat_unchanged(&rec, &entry.meta)
    {
        report.unchanged += 1;
        return Ok(true);
    }

    let is_kind_change = !rec.deleted && rec.kind != entry.meta.kind;
    let hash = match entry.meta.kind {
        EntryKind::File => {
            match check_and_hash_file(
                fs,
                &entry.path,
                &entry.meta,
                tracker,
                still_tracking,
                now,
                options,
                report,
            )? {
                Some(h) => Some(h),
                None => return Ok(false),
            }
        }
        EntryKind::Dir | EntryKind::Symlink => None,
    };

    // If only mtime/change_id changed (or verify_checksums confirmed identical bytes), refresh
    // local stat fields without bumping the causal version vector or churning peers.
    if !rec.deleted
        && !is_kind_change
        && entry.meta.kind == EntryKind::File
        && rec.hash == hash
        && rec.size == entry.meta.size
        && !mode_changed(rec.mode, entry.meta.mode)
    {
        if rec.disk_mtime != Some(entry.meta.mtime) || rec.disk_change_id != entry.meta.change_id {
            let mut refreshed = rec;
            refreshed.disk_mtime = Some(entry.meta.mtime);
            refreshed.disk_change_id = entry.meta.change_id;
            changes.push(PendingChange::RefreshDiskStatOnly { record: refreshed });
        }
        report.unchanged += 1;
        return Ok(true);
    }

    changes.push(PendingChange::Upsert(PendingUpsert {
        path: entry.path.clone(),
        meta: entry.meta.clone(),
        hash,
        prev: Some(rec),
        is_kind_change,
    }));
    Ok(true)
}

/// Returns `true` when `dir_path` is non-empty on disk AND every entry inside it is a local
/// non-synced entry (ignored by custom/fixed rules, in-flight temporary, an unsupported escaping
/// local symlink, or a non-empty subdirectory of such entries).
///
/// Remote records held back in the index (for example because a peer's file failed `NoSpace` or
/// `PermissionDenied` earlier) are intentionally NOT counted here: a normal synced file created on
/// local disk at the same path WILL be indexed by scan, so its parent directory must revive with
/// it rather than remaining a tombstone.
pub(crate) fn dir_holds_only_local_non_synced_files<F: SyncFs>(
    fs: &F,
    dir_path: &str,
    ignore: &IgnoreMatcher,
) -> Result<bool> {
    dir_holds_only_local_non_synced_files_depth(fs, dir_path, ignore, 0)
}

fn dir_holds_only_local_non_synced_files_depth<F: SyncFs>(
    fs: &F,
    dir_path: &str,
    ignore: &IgnoreMatcher,
    depth: usize,
) -> Result<bool> {
    if depth >= 64 {
        return Ok(false);
    }
    let children = match fs.list_dir_live(dir_path) {
        Ok(c) => c,
        Err(err) if err.is_crash() => return Err(err),
        Err(_) => return Ok(false),
    };
    if children.is_empty() {
        return Ok(false);
    }
    for name in children {
        let child_rel = format!("{dir_path}/{name}");
        let child_meta = fs.stat_live(&child_rel).ok().flatten();
        let is_dir = child_meta
            .as_ref()
            .is_some_and(|m| m.kind == EntryKind::Dir);
        let ignored = ignore.is_ignored(&child_rel, &name, is_dir)
            || is_os_litter_name(&name)
            || is_legion_inflight_name(&name);
        let unsupported_symlink = child_meta.as_ref().is_some_and(|m| {
            m.kind == EntryKind::Symlink
                && check_symlink_target_contained(
                    &child_rel,
                    m.link_target.as_deref().unwrap_or_default(),
                )
                .is_err()
        });
        let local_only_subdir = is_dir
            && dir_holds_only_local_non_synced_files_depth(fs, &child_rel, ignore, depth + 1)?;
        if !ignored && !unsupported_symlink && !local_only_subdir {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn check_and_hash_file<F: SyncFs>(
    fs: &F,
    path: &str,
    meta1: &FsMeta,
    tracker: &mut PendingStabilityTracker,
    still_tracking: &mut BTreeSet<String>,
    now: Timestamp,
    options: ScanOptions,
    report: &mut ScanReport,
) -> Result<Option<Hash>> {
    if !options.min_stable_age.is_zero() {
        let age_secs = now.secs.saturating_sub(meta1.mtime.secs);
        let required_secs = i64::try_from(options.min_stable_age.as_secs()).unwrap_or(i64::MAX);
        if age_secs < required_secs {
            still_tracking.insert(path.to_string());
            tracker.confirm(path, meta1.size, meta1.mtime);
            report.held_back_unstable.push(path.to_string());
            return Ok(None);
        }
    }

    if options.require_second_look && !tracker.confirm(path, meta1.size, meta1.mtime) {
        still_tracking.insert(path.to_string());
        report.held_back_unstable.push(path.to_string());
        return Ok(None);
    }

    let (hash, bytes_read) = match fs.hash_live(path) {
        Ok(res) => res,
        Err(err) if err.is_crash() => return Err(err),
        Err(err) => {
            report
                .skipped_errors
                .push((path.to_string(), err.to_string()));
            return Ok(None);
        }
    };
    report.hashed += 1;

    let meta2 = match fs.stat_live(path) {
        Ok(m) => m,
        Err(err) if err.is_crash() => return Err(err),
        Err(err) => {
            report
                .skipped_errors
                .push((path.to_string(), err.to_string()));
            return Ok(None);
        }
    };

    let stable = meta2.as_ref().is_some_and(|m| {
        m.kind == EntryKind::File
            && m.size == meta1.size
            && m.mtime == meta1.mtime
            && m.change_id == meta1.change_id
            && m.file_identity == meta1.file_identity
            && bytes_read == meta1.size
    });
    if !stable {
        report.held_back_unstable.push(path.to_string());
        return Ok(None);
    }

    Ok(Some(hash))
}

fn is_stat_unchanged(rec: &IndexRecord, disk: &FsMeta) -> bool {
    if rec.kind != disk.kind {
        return false;
    }
    match rec.kind {
        EntryKind::Dir => true,
        EntryKind::Symlink => rec.link_target == disk.link_target,
        EntryKind::File => {
            if rec.size != disk.size {
                return false;
            }
            match (rec.disk_change_id, disk.change_id) {
                (Some(a), Some(b)) if a != b => return false,
                (None, Some(_)) => return false,
                _ => {}
            }
            let Some(dm) = rec.disk_mtime else {
                return false;
            };
            dm == disk.mtime && !mode_changed(rec.mode, disk.mode)
        }
    }
}

fn mode_changed(recorded: Option<u32>, disk: Option<u32>) -> bool {
    match (recorded, disk) {
        (_, None) | (None, Some(_)) => false,
        (Some(a), Some(b)) => (a & 0o777) != (b & 0o777),
    }
}

/// Fast metadata-only pre-check matching a live path against `expected` without reading file
/// contents.
///
/// Used before atomic replace/delete primitives (`safe_replace_live_file` /
/// `safe_delete_live_file`) that always hash the displaced file atomically after swap or claim,
/// keeping displaced-file disk reads bounded to at most 1–2 reads per operation.
pub fn disk_stat_matches_index<F: SyncFs>(
    fs: &F,
    path: &str,
    expected: Option<&IndexRecord>,
) -> Result<bool> {
    let disk_opt = fs.stat_live(path)?;
    match (expected, disk_opt) {
        (None, None) => Ok(true),
        (Some(rec), None) => Ok(rec.deleted),
        (None, Some(_)) => Ok(false),
        (Some(rec), Some(_)) if rec.deleted => Ok(false),
        (Some(rec), Some(disk)) => Ok(is_stat_unchanged(rec, &disk)),
    }
}

/// Checks whether a live path on disk still matches the index record `expected` before apply,
/// restore, or conflict resolution mutates it.
///
/// For regular files, verifies both `stat` metadata and the BLAKE3 hash so even same-size,
/// same-timestamp edits between scan and apply are caught and never overwritten.
pub fn disk_matches_index<F: SyncFs>(
    fs: &F,
    path: &str,
    expected: Option<&IndexRecord>,
) -> Result<bool> {
    let disk_opt = fs.stat_live(path)?;
    match (expected, disk_opt) {
        (None, None) => Ok(true),
        (Some(rec), None) => Ok(rec.deleted),
        (None, Some(_)) => Ok(false),
        (Some(rec), Some(_)) if rec.deleted => Ok(false),
        (Some(rec), Some(disk)) => {
            if rec.kind != disk.kind {
                return Ok(false);
            }
            match rec.kind {
                EntryKind::Dir => Ok(true),
                EntryKind::Symlink => Ok(rec.link_target == disk.link_target),
                EntryKind::File => {
                    if rec.size != disk.size {
                        return Ok(false);
                    }
                    let mtime_ok = rec.disk_mtime == Some(disk.mtime) || rec.mtime == disk.mtime;
                    if !mtime_ok || mode_changed(rec.mode, disk.mode) {
                        return Ok(false);
                    }
                    let (actual_hash, bytes_read) = match fs.hash_live(path) {
                        Ok(pair) => pair,
                        Err(SyncError::Fs {
                            kind: FsErrorKind::NotFound,
                            ..
                        })
                        | Err(SyncError::Conflict { .. }) => return Ok(false),
                        Err(err) => return Err(err),
                    };
                    Ok(bytes_read == rec.size && rec.hash == Some(actual_hash))
                }
            }
        }
    }
}
