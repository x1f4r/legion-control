//! Folder scanner converting live filesystem state into incremental index updates.
//!
//! Unchanged entries (matching kind, size, modification time, local change id, and Unix mode) are
//! skipped in a single streaming merge against the sorted index without re-hashing, unless
//! [`ScanOptions::verify_checksums`] is enabled. Unreadable directories or entries never invent
//! deletions, and the mass-deletion brake pauses before committing large-scale deletions.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::Result;
use crate::model::{DeletionBrake, DeletionBrakeConfig, IndexRecord, MAX_ANCESTRY_HASHES};
use crate::path::{
    IgnoreMatcher, is_descendant_of, is_legion_inflight_name, is_os_litter_name, path_depth,
    validate_sync_path,
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

    let held_back_paths: BTreeSet<String> = if store.held_back_count()? > 0 {
        store
            .list_held_back()?
            .into_iter()
            .map(|h| h.record.path)
            .collect()
    } else {
        BTreeSet::new()
    };

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
                if dir_holds_only_local_non_synced_files(fs, &entry.path, ignore, &held_back_paths)?
                {
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
                if !dir_holds_only_local_non_synced_files(
                    fs,
                    &entry.path,
                    ignore,
                    &held_back_paths,
                )? {
                    kept_dirs_revived.insert(entry.path.clone());
                }
            }
            let force_hash = meta.needs_rescan.contains(&entry.path);
            if handle_existing_disk_entry(
                fs,
                entry,
                rec,
                ignore,
                &held_back_paths,
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
            if dir_holds_only_local_non_synced_files(fs, &entry.path, ignore, &held_back_paths)? {
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
        meta.needs_rescan
            .retain(|p| unstable_set.contains(p.as_str()) || is_under_skipped(p, &report.skipped_errors));
        if meta.needs_rescan.len() != before {
            kept_dirs_changed = true;
        }
    }

    if subtree_root.is_none() {
        tracker.retain_paths(&still_tracking);
    }

    // Re-filter deletions against any errors added during hashing/statting.
    let mut deletes: Vec<IndexRecord> = Vec::new();
    let mut upserts: Vec<PendingUpsert> = Vec::new();
    let mut stat_refreshes: Vec<IndexRecord> = Vec::new();

    for change in changes {
        match change {
            PendingChange::Delete { old } => {
                if !is_under_skipped(&old.path, &report.skipped_errors) {
                    deletes.push(old);
                }
            }
            PendingChange::Upsert(item) => upserts.push(item),
            PendingChange::RefreshDiskStatOnly { record } => stat_refreshes.push(record),
        }
    }

    // Item B3: mass-deletion brake.
    let deleted_files = deletes.iter().filter(|r| r.kind == EntryKind::File).count();
    let total_live_files = store.live_file_count()? as usize;
    if !options.allow_mass_deletion && brake_config.should_trip(deleted_files, total_live_files) {
        let sample_paths = deletes
            .iter()
            .filter(|r| r.kind == EntryKind::File)
            .take(10)
            .map(|r| r.path.clone())
            .collect();
        meta.paused = true;
        meta.pending_brake = Some(DeletionBrake {
            deleted_count: deleted_files,
            total_live_files,
            sample_paths,
        });
        let batch = StoreBatch {
            folder_meta: Some(meta),
            ..StoreBatch::default()
        };
        store.apply_batch(batch)?;
        report.brake_tripped = true;
        report.deleted = 0;
        return Ok(report);
    }

    let finish_join_mode = subtree_root.is_none() && meta.join_mode;
    if deletes.is_empty() && upserts.is_empty() && stat_refreshes.is_empty() {
        if finish_join_mode || kept_dirs_changed {
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
    let vkey = meta.version_key();
    let join_mode_active = meta.join_mode;

    for rec in stat_refreshes {
        batch.upsert_records.push(rec);
    }

    let mut meta_dirty = kept_dirs_changed;

    for old in deletes {
        let mut version = old.version;
        let clock = meta.next_clock();
        version.bump_above(vkey.clone(), clock);
        let seq = meta.next_seq();
        meta_dirty = true;

        let mut recent_hashes = old.recent_hashes;
        if let Some(h) = old.hash {
            recent_hashes.retain(|x| *x != h);
            recent_hashes.insert(0, h);
            recent_hashes.truncate(MAX_ANCESTRY_HASHES);
        }

        if !recent_hashes.is_empty() {
            batch
                .upsert_deleted_history
                .push((old.path.clone(), recent_hashes.clone()));
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
        meta.record_seq_fingerprint(seq, rec.head_fingerprint());
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
        let clock = meta.next_clock();
        version.bump_above(vkey.clone(), clock);
        let seq = meta.next_seq();
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
            from_join: prev.is_none() && join_mode_active,
            conflict_of: None,
            conflict_dismissed,
        };
        rec.conflict_of = derive_conflict_of(&rec, &meta);
        meta.record_seq_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_records.push(rec);

        if is_kind_change {
            report.kind_changed += 1;
        } else if is_new {
            report.created += 1;
        } else {
            report.modified += 1;
        }
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

/// Rescans a single relative path on disk and commits any detected local change atomically.
pub fn rescan_single_path<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_path: &str,
    now: Timestamp,
) -> Result<SingleRescanOutcome> {
    let clean = validate_sync_path(rel_path)?;
    let mut meta = store
        .load_meta()?
        .ok_or_else(|| crate::error::SyncError::Storage {
            context: "rescan_single_path".to_string(),
            detail: "index store is not initialized".to_string(),
        })?;
    let vkey = meta.version_key();
    let prev = store.get_record(&clean)?;
    let disk_opt = fs.stat_live(&clean)?;

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
            let mut upsert_deleted_history = Vec::new();
            let mut main_record = None;
            for item in to_tombstone {
                let is_main = item.path == clean;
                let mut version = item.version;
                let clock = meta.next_clock();
                version.bump_above(vkey.clone(), clock);
                let seq = meta.next_seq();
                let mut recent_hashes = item.recent_hashes;
                if let Some(h) = item.hash {
                    recent_hashes.retain(|x| *x != h);
                    recent_hashes.insert(0, h);
                    recent_hashes.truncate(MAX_ANCESTRY_HASHES);
                }
                if !recent_hashes.is_empty() {
                    upsert_deleted_history.push((item.path.clone(), recent_hashes.clone()));
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
                meta.record_seq_fingerprint(seq, rec.head_fingerprint());
                if is_main {
                    main_record = Some(rec.clone());
                }
                upsert_records.push(rec);
            }
            let record = main_record.ok_or_else(|| crate::error::SyncError::Storage {
                context: "rescan_single_path".to_string(),
                detail: "missing main tombstone record".to_string(),
            })?;
            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records,
                upsert_deleted_history,
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(SingleRescanOutcome::Updated(Box::new(record)))
        }
        (prev_opt, Some(disk_meta)) => {
            let hash = if disk_meta.kind == EntryKind::File {
                let (h, bytes_read) = fs.hash_live(&clean)?;
                let after = fs.stat_live(&clean)?;
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

            if let Some(ref existing) = prev_opt
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
            let clock = meta.next_clock();
            version.bump_above(vkey, clock);
            let seq = meta.next_seq();
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
            meta.record_seq_fingerprint(seq, record.head_fingerprint());
            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records: vec![record.clone()],
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
    held_back_paths: &BTreeSet<String>,
    force_hash: bool,
    tracker: &mut PendingStabilityTracker,
    still_tracking: &mut BTreeSet<String>,
    now: Timestamp,
    options: ScanOptions,
    changes: &mut Vec<PendingChange>,
    report: &mut ScanReport,
) -> Result<bool> {
    // Item H: a directory that was deleted elsewhere and intentionally kept on disk only because
    // it still contains ignored or held-back files must NOT be revived as a new directory on scan.
    if rec.deleted
        && entry.meta.kind == EntryKind::Dir
        && dir_holds_only_local_non_synced_files(fs, &entry.path, ignore, held_back_paths)?
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

/// Returns `true` when `dir_path` is non-empty on disk AND every entry inside it is excluded from
/// syncing (ignored by custom/fixed rules or held back), meaning the directory only remains on
/// disk to house local-only files.
fn dir_holds_only_local_non_synced_files<F: SyncFs>(
    fs: &F,
    dir_path: &str,
    ignore: &IgnoreMatcher,
    held_back_paths: &BTreeSet<String>,
) -> Result<bool> {
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
        let is_dir = fs
            .stat_live(&child_rel)
            .ok()
            .flatten()
            .is_some_and(|m| m.kind == EntryKind::Dir);
        let ignored = ignore.is_ignored(&child_rel, &name, is_dir)
            || is_os_litter_name(&name)
            || is_legion_inflight_name(&name);
        let held = held_back_paths.contains(&child_rel)
            || held_back_paths
                .iter()
                .any(|p| is_descendant_of(p, &child_rel));
        if !ignored && !held {
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
            if let (Some(a), Some(b)) = (rec.disk_change_id, disk.change_id)
                && a != b
            {
                return false;
            }
            let mtime_matches = rec.disk_mtime == Some(disk.mtime) || rec.mtime == disk.mtime;
            mtime_matches && !mode_changed(rec.mode, disk.mode)
        }
    }
}

fn mode_changed(recorded: Option<u32>, disk: Option<u32>) -> bool {
    match (recorded, disk) {
        (_, None) | (None, Some(_)) => false,
        (Some(a), Some(b)) => (a & 0o777) != (b & 0o777),
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
                    let (actual_hash, bytes_read) = fs.hash_live(path)?;
                    Ok(bytes_read == rec.size && rec.hash == Some(actual_hash))
                }
            }
        }
    }
}
