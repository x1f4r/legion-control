//! Folder scanner converting live filesystem state into incremental index updates.
//!
//! Unchanged entries (matching kind, size, modification time, and Unix mode) are skipped in a
//! single streaming merge against the sorted index without re-hashing. Files that change between
//! two observations (either across consecutive scans when two-look confirmation is enabled, or
//! between the walk stat and the post-hash stat) are held back until stable.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use legion_xfer::{EntryKind, Timestamp};

use crate::error::Result;
use crate::model::IndexRecord;
use crate::path::{IgnoreMatcher, path_depth, validate_sync_path};
use crate::traits::{FsMeta, FsWalkEntry, IndexStore, StoreBatch, SyncFs};
use crate::version::VersionVector;

/// Options controlling how [`scan_folder`] detects in-flight writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanOptions {
    /// When `true`, newly created or modified regular files must show the same `(size, mtime)`
    /// across two consecutive scans before being hashed and committed to the index.
    pub require_second_look: bool,
    /// Minimum age (`now - mtime`) a new or modified regular file must reach before being
    /// committed. Files newer than this window are held back as still changing.
    pub min_stable_age: Duration,
}

/// Summary of one folder scan.
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
    /// Entries that matched the index and required no hashing or writes.
    pub unchanged: usize,
    /// Regular files whose bytes were streamed through BLAKE3 during this scan.
    pub hashed: usize,
    /// Paths held back because they were still changing between looks.
    pub held_back_unstable: Vec<String>,
    /// Paths that could not be inspected during the walk (`(path, reason)`).
    pub skipped_errors: Vec<(String, String)>,
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

    /// Removes observations for paths no longer awaiting confirmation.
    fn retain_paths(&mut self, active: &BTreeSet<String>) {
        self.observed.retain(|k, _| active.contains(k));
    }

    /// Checks whether `path` has already been seen with identical `(size, mtime)`.
    ///
    /// Returns `true` if stable; otherwise records the latest observation and returns `false`.
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
    hash: Option<legion_xfer::Hash>,
    prev: Option<IndexRecord>,
    is_kind_change: bool,
}

enum PendingChange {
    Delete { old: IndexRecord },
    Upsert(PendingUpsert),
    RefreshDiskMtimeOnly { record: IndexRecord },
}

/// Scans `fs` against `store`, returning a [`ScanReport`] and committing all index updates in one
/// atomic transaction.
pub fn scan_folder<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    ignore: &IgnoreMatcher,
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
    let mut matrix = store.load_causal_matrix()?;

    let walk = fs.walk(ignore)?;
    let disk_entries = walk.entries;
    let mut report = ScanReport {
        skipped_errors: walk.skipped,
        ..ScanReport::default()
    };

    let mut changes: Vec<PendingChange> = Vec::new();
    let mut still_tracking: BTreeSet<String> = BTreeSet::new();
    let mut disk_idx = 0usize;

    store.scan_records_in_order(&mut |rec| {
        while disk_idx < disk_entries.len() && disk_entries[disk_idx].path < rec.path {
            handle_new_disk_entry(
                fs,
                &disk_entries[disk_idx],
                tracker,
                &mut still_tracking,
                now,
                options,
                &mut changes,
                &mut report,
            )?;
            disk_idx += 1;
        }

        if disk_idx < disk_entries.len() && disk_entries[disk_idx].path == rec.path {
            handle_existing_disk_entry(
                fs,
                &disk_entries[disk_idx],
                rec,
                tracker,
                &mut still_tracking,
                now,
                options,
                &mut changes,
                &mut report,
            )?;
            disk_idx += 1;
        } else if !rec.deleted
            && !ignore.is_path_or_ancestor_ignored(&rec.path, rec.kind == EntryKind::Dir)
        {
            changes.push(PendingChange::Delete { old: rec });
        }

        Ok(true)
    })?;

    while disk_idx < disk_entries.len() {
        handle_new_disk_entry(
            fs,
            &disk_entries[disk_idx],
            tracker,
            &mut still_tracking,
            now,
            options,
            &mut changes,
            &mut report,
        )?;
        disk_idx += 1;
    }

    tracker.retain_paths(&still_tracking);

    if changes.is_empty() {
        return Ok(report);
    }

    let mut deletes: Vec<IndexRecord> = Vec::new();
    let mut upserts: Vec<PendingUpsert> = Vec::new();
    let mut mtime_refreshes: Vec<IndexRecord> = Vec::new();

    for change in changes {
        match change {
            PendingChange::Delete { old } => deletes.push(old),
            PendingChange::Upsert(item) => upserts.push(item),
            PendingChange::RefreshDiskMtimeOnly { record } => mtime_refreshes.push(record),
        }
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

    for rec in mtime_refreshes {
        batch.upsert_records.push(rec);
    }

    let mut seq_bumped = false;

    for old in deletes {
        let mut version = old.version;
        let clock = meta.next_clock();
        version.bump_above(meta.local_member.clone(), clock);
        let seq = meta.next_seq();
        seq_bumped = true;

        let mut deps = BTreeMap::new();
        deps.insert(meta.local_member.clone(), seq);

        batch.upsert_records.push(IndexRecord {
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
            tombstone_deps: deps,
            conflict_of: None,
        });
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
        version.bump_above(meta.local_member.clone(), clock);
        let seq = meta.next_seq();
        seq_bumped = true;

        // Preserve Unix permission bits when scanning on Windows (where `fmeta.mode` is `None`).
        let mode = fmeta.mode.or_else(|| {
            prev.as_ref()
                .filter(|p| !p.deleted && p.kind == fmeta.kind)
                .and_then(|p| p.mode)
        });
        // Preserve conflict tag if a conflict copy was edited locally (unless kind changed).
        let conflict_of = prev
            .as_ref()
            .filter(|p| !p.deleted && !is_kind_change)
            .and_then(|p| p.conflict_of.clone());

        batch.upsert_records.push(IndexRecord {
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
            tombstone_deps: BTreeMap::new(),
            conflict_of,
        });

        if is_kind_change {
            report.kind_changed += 1;
        } else if is_new {
            report.created += 1;
        } else {
            report.modified += 1;
        }
    }

    if seq_bumped {
        matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);
        batch.folder_meta = Some(meta);
        batch.causal_matrix = Some(matrix);
    }

    store.apply_batch(batch)?;
    Ok(report)
}

/// Rescans a single relative path on disk and commits any detected local change atomically.
///
/// Called by `apply` whenever its pre-replace re-check discovers that the live entry changed
/// between the last scan and the moment of replacement or deletion.
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
    let mut matrix = store.load_causal_matrix()?;
    let prev = store.get_record(&clean)?;
    let disk_opt = fs.stat_live(&clean)?;

    match (prev, disk_opt) {
        (None, None) => Ok(SingleRescanOutcome::Unchanged),
        (Some(ref rec), None) if rec.deleted => Ok(SingleRescanOutcome::Unchanged),
        (Some(old), None) => {
            let mut version = old.version;
            let clock = meta.next_clock();
            version.bump_above(meta.local_member.clone(), clock);
            let seq = meta.next_seq();
            let mut deps = BTreeMap::new();
            deps.insert(meta.local_member.clone(), seq);
            let record = IndexRecord {
                path: clean,
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
                tombstone_deps: deps,
                conflict_of: None,
            };
            matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);
            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
                upsert_records: vec![record.clone()],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(SingleRescanOutcome::Updated(Box::new(record)))
        }
        (prev_opt, Some(disk_meta)) => {
            let hash = if disk_meta.kind == EntryKind::File {
                let (h, bytes_read) = fs.hash_live(&clean)?;
                // Look-2 stability check: ensure the file did not mutate while hashing.
                let after = fs.stat_live(&clean)?;
                let stable = after.as_ref().is_some_and(|m| {
                    m.kind == EntryKind::File
                        && m.size == disk_meta.size
                        && m.mtime == disk_meta.mtime
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
                if existing.disk_mtime != Some(disk_meta.mtime)
                    || existing.disk_change_id != disk_meta.change_id
                {
                    let mut refreshed = existing.clone();
                    refreshed.disk_mtime = Some(disk_meta.mtime);
                    refreshed.disk_change_id = disk_meta.change_id;
                    let mut batch = StoreBatch::default();
                    batch.upsert_records.push(refreshed);
                    store.apply_batch(batch)?;
                }
                return Ok(SingleRescanOutcome::Unchanged);
            }

            let is_kind_change = prev_opt
                .as_ref()
                .is_some_and(|p| !p.deleted && p.kind != disk_meta.kind);
            let mut version = prev_opt
                .as_ref()
                .map_or_else(VersionVector::new, |p| p.version.clone());
            let clock = meta.next_clock();
            version.bump_above(meta.local_member.clone(), clock);
            let seq = meta.next_seq();
            let mode = disk_meta.mode.or_else(|| {
                prev_opt
                    .as_ref()
                    .filter(|p| !p.deleted && p.kind == disk_meta.kind)
                    .and_then(|p| p.mode)
            });
            let conflict_of = prev_opt
                .as_ref()
                .filter(|p| !p.deleted && !is_kind_change)
                .and_then(|p| p.conflict_of.clone());

            let record = IndexRecord {
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
                tombstone_deps: BTreeMap::new(),
                conflict_of,
            };
            matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);
            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
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
) -> Result<()> {
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
                None => return Ok(()),
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
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_existing_disk_entry<F: SyncFs>(
    fs: &F,
    entry: &FsWalkEntry,
    rec: IndexRecord,
    tracker: &mut PendingStabilityTracker,
    still_tracking: &mut BTreeSet<String>,
    now: Timestamp,
    options: ScanOptions,
    changes: &mut Vec<PendingChange>,
    report: &mut ScanReport,
) -> Result<()> {
    if !rec.deleted && is_stat_unchanged(&rec, &entry.meta) {
        report.unchanged += 1;
        return Ok(());
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
                None => return Ok(()),
            }
        }
        EntryKind::Dir | EntryKind::Symlink => None,
    };

    // If only mtime changed on a regular file (for example `touch`), refresh `disk_mtime` without
    // bumping the causal version vector or churning peers.
    if !rec.deleted
        && !is_kind_change
        && entry.meta.kind == EntryKind::File
        && rec.hash == hash
        && rec.size == entry.meta.size
        && !mode_changed(rec.mode, entry.meta.mode)
    {
        let mut refreshed = rec;
        refreshed.disk_mtime = Some(entry.meta.mtime);
        refreshed.disk_change_id = entry.meta.change_id;
        changes.push(PendingChange::RefreshDiskMtimeOnly { record: refreshed });
        report.unchanged += 1;
        return Ok(());
    }

    changes.push(PendingChange::Upsert(PendingUpsert {
        path: entry.path.clone(),
        meta: entry.meta.clone(),
        hash,
        prev: Some(rec),
        is_kind_change,
    }));
    Ok(())
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
) -> Result<Option<legion_xfer::Hash>> {
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

    // Look-2 stability verification: if the file vanished or changed while streaming its bytes,
    // hold it back rather than publishing torn content.
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
        // Windows provides `None` for disk mode; never treat that as a mode change.
        (_, None) => false,
        (None, Some(_)) => false,
        (Some(a), Some(b)) => (a & 0o777) != (b & 0o777),
    }
}

/// Checks whether a live path on disk still matches the index record `expected` before apply
/// overwrites or deletes it.
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
