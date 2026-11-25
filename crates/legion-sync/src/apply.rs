//! Crash-safe application of reconciled decisions, startup recovery, bin management, and
//! conflict resolution.
//!
//! Every working-tree replacement or deletion uses non-replacing renames or atomic swaps,
//! verifies the displaced file's BLAKE3 digest before moving it to `.legion-sync/bin`, and is
//! bracketed by a durable [`PendingOp`] write-ahead record. Non-crash errors mid-apply recover
//! immediately instead of waiting for the next restart.

use std::cell::Cell;
use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::time::{Duration, Instant, SystemTime};

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::{FsErrorKind, Result, SyncError};
use crate::model::{
    BinEntry, BinReason, ConflictChoice, ExpectedLocalSnapshot, FetchClock, FetchOptions,
    FolderDeletionLogEntry, FolderMeta, IndexRecord, MAX_ANCESTRY_HASHES, MIN_PLAUSIBLE_CLOCK_SECS,
    MemberId, PendingOp, ProblemKind, RestoreStrategy, SyncProblem, encode_deletion_log_entries,
    random_hex_token,
};
use crate::path::{
    IgnoreMatcher, ancestors, check_symlink_target_contained, conflict_copy_path,
    is_legion_inflight_name, is_os_litter_name, parent_path, restore_copy_path, validate_sync_path,
};
use crate::reconcile::{
    LocalIndexView, ReconcileDecision, check_local_representable, choose_conflict_path,
    derive_conflict_of,
};
use crate::scan::{
    SingleRescanOutcome, dir_holds_only_local_non_synced_files, disk_matches_index,
    disk_stat_matches_index, rescan_single_path, rescan_single_path_with_force,
};
use crate::traits::{FsMeta, IndexStore, Peer, StoreBatch, SyncFs, SyncWrite};
use crate::version::VersionVector;

/// Maximum number of ranged resume attempts on a transient read error during one file fetch.
const MAX_FETCH_RESUMES: usize = 2;
/// Maximum number of open staged writers held before syncing and placing a group-commit chunk.
const GROUP_STAGE_MAX_FILES: usize = 128;
/// Maximum total staged bytes held before syncing and placing a group-commit chunk.
const GROUP_STAGE_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Result of attempting to apply one [`ReconcileDecision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Decision was applied and committed to the index.
    Applied,
    /// A deleted directory was kept locally because it still holds folder-ignored or held-back
    /// files; the tombstone was committed and the problem should be reported once.
    DirKeptLocalFiles(SyncProblem),
    /// Local disk changed since the last scan (or raced during swap/claim); the path was
    /// rescanned locally and the caller should re-reconcile it.
    LocalChangedRetry,
    /// Operation was deferred for an explicit human-readable reason (tracked with a retry bound).
    Deferred {
        /// Plain-language reason why this record was deferred.
        reason: String,
    },
    /// Single record failed local placement or free-space check (for example disk full, cross-device
    /// rename, or local permission error); hold back durably without acknowledging past it and
    /// continue pulling unrelated records.
    HoldBackLocal(SyncProblem),
}

/// Result of applying a clean new-file batch with group commit.
#[derive(Debug, Default)]
pub struct BatchApplyResult {
    /// Sequence numbers cleanly placed and committed in the group commit.
    pub applied_seqs: Vec<u64>,
    /// Records that raced with a local file creation and must fall back to full single-record
    /// reconciliation.
    pub fallback_records: Vec<IndexRecord>,
    /// Records deferred during fetch (`(seq, path, reason)`).
    pub deferred: Vec<(u64, String, String)>,
    /// Single records held back due to local space or placement failure (`(seq, path, problem)`).
    pub held_back_local: Vec<(u64, String, SyncProblem)>,
}

/// Summary of work performed by [`recover_pending`] on startup or after an immediate mid-apply
/// error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `true` if a [`PendingOp`] was found and completed or rolled back.
    pub recovered_pending_op: bool,
    /// Number of orphaned staging files cleaned from `.legion-sync/tmp/`.
    pub cleaned_tmp_files: usize,
}

/// IndexStore adapter implementing [`LocalIndexView`] for [`crate::reconcile::reconcile_record`].
pub struct StoreIndexView<'a, S: IndexStore> {
    store: &'a S,
}

impl<'a, S: IndexStore> StoreIndexView<'a, S> {
    /// Wraps `store` as a read-only [`LocalIndexView`].
    pub fn new(store: &'a S) -> Self {
        Self { store }
    }
}

impl<S: IndexStore> LocalIndexView for StoreIndexView<'_, S> {
    fn get_record(&self, path: &str) -> Option<IndexRecord> {
        self.store.get_record(path).ok().flatten()
    }

    fn has_live_descendant(&self, dir_path: &str) -> bool {
        self.store.has_live_descendant(dir_path).unwrap_or(false)
    }

    fn live_paths_for_folded(&self, folded_path: &str) -> Vec<String> {
        self.store
            .live_paths_for_folded(folded_path)
            .unwrap_or_default()
    }

    fn get_deleted_history(&self, path: &str) -> Vec<Hash> {
        self.store.get_deleted_history(path).unwrap_or_default()
    }

    fn get_deleted_history_version(&self, path: &str) -> Option<VersionVector> {
        self.store.get_deleted_history_version(path).ok().flatten()
    }
}

enum SafeReplaceResult {
    /// Staged file was placed and verified at `live_path`; `binned` reports whether the displaced
    /// file matched the indexed old version and was moved to `.legion-sync/bin` (otherwise the
    /// displaced unseen save was preserved as a conflict copy).
    Replaced { binned: bool },
    /// Displaced file was handled (binned or preserved as a conflict copy), but `live_path` now
    /// holds something other than the staged file (person replaced, edited, or moved it).
    LiveChangedAfterDisplace { binned: bool },
    /// Path changed before placement; `staged_tmp` is still our un-exchanged staged download.
    RaceBeforePlace,
    /// Target path is currently locked, in use, or pending deletion; no user bytes were lost and
    /// the record should be deferred cleanly without failing the rest of the page.
    Deferred { reason: String, binned: bool },
}

enum SafeDeleteResult {
    Deleted { binned: bool },
    RaceChanged,
    Deferred(String),
}

enum AncestorOutcome {
    Ready,
    ChangedRetry,
    Unstable,
    Busy(String),
}

fn is_mid_apply_race_error(err: &SyncError) -> bool {
    matches!(
        err,
        SyncError::Conflict { .. }
            | SyncError::Fs {
                kind: FsErrorKind::NotFound
                    | FsErrorKind::AlreadyExists
                    | FsErrorKind::DirectoryNotEmpty,
                ..
            }
    )
}

fn is_live_busy_error<F: SyncFs>(fs: &F, err: &SyncError) -> bool {
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

fn is_replace_partial_failure(err: &SyncError) -> bool {
    matches!(
        err,
        SyncError::Fs {
            kind: FsErrorKind::ReplaceUnableToRemoveReplaced
                | FsErrorKind::ReplaceUnableToMoveReplacement
                | FsErrorKind::ReplaceUnableToMoveReplacement2,
            ..
        }
    )
}

fn busy_defer_reason(path: &str) -> String {
    format!("'{path}' is currently in use by another program")
}

/// Computes the minimum free space required before downloading a file of `file_size` bytes:
/// the file's own size plus safety headroom (`min(max(10%, 4 KiB), 16 MiB)`).
fn required_free_space_bytes(file_size: u64) -> u64 {
    if file_size == 0 {
        return 0;
    }
    let headroom = (file_size / 10).clamp(4 * 1024, 16 * 1024 * 1024);
    file_size.saturating_add(headroom)
}

fn format_human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if b < MIB {
        format!("{:.1} KiB", b / KIB)
    } else if b < GIB {
        format!("{:.1} MiB", b / MIB)
    } else {
        format!("{:.2} GiB", b / GIB)
    }
}

fn check_free_space_for_file<F: SyncFs>(
    fs: &F,
    path: &str,
    size: u64,
    peer_id: &str,
) -> Result<Option<SyncProblem>> {
    if size == 0 {
        return Ok(None);
    }
    let Some(avail) = fs.available_space_for(path)? else {
        return Ok(None);
    };
    if avail < required_free_space_bytes(size) {
        return Ok(Some(SyncProblem {
            path: path.to_string(),
            kind: ProblemKind::NoSpace,
            peer: Some(peer_id.to_string()),
            sentence: format!(
                "Not enough space for '{path}' ({}, {} available).",
                format_human_bytes(size),
                format_human_bytes(avail)
            ),
        }));
    }
    Ok(None)
}

fn classify_local_placement_problem<F: SyncFs>(
    fs: &F,
    path: &str,
    size: u64,
    peer_id: &str,
    err: &SyncError,
) -> Option<SyncProblem> {
    if err.is_crash() {
        return None;
    }
    match err {
        SyncError::Fs {
            kind: FsErrorKind::NoSpace,
            ..
        } => Some(SyncProblem {
            path: path.to_string(),
            kind: ProblemKind::NoSpace,
            peer: Some(peer_id.to_string()),
            sentence: format!(
                "Not enough space for '{path}' ({}).",
                format_human_bytes(size)
            ),
        }),
        SyncError::Fs {
            kind: FsErrorKind::CrossDevice,
            detail,
            ..
        } => Some(SyncProblem {
            path: path.to_string(),
            kind: ProblemKind::FilesystemError,
            peer: Some(peer_id.to_string()),
            sentence: format!("Could not place '{path}' across file systems: {detail}."),
        }),
        SyncError::Fs {
            kind: FsErrorKind::PermissionDenied,
            detail,
            ..
        } if !fs.capabilities().windows_naming => Some(SyncProblem {
            path: path.to_string(),
            kind: ProblemKind::FilesystemError,
            peer: Some(peer_id.to_string()),
            sentence: format!("Permission denied placing '{path}': {detail}."),
        }),
        SyncError::InvalidPath { reason, .. } => Some(SyncProblem {
            path: path.to_string(),
            kind: ProblemKind::FilesystemError,
            peer: Some(peer_id.to_string()),
            sentence: format!("'{path}' could not be placed on this file system: {reason}."),
        }),
        _ => None,
    }
}

fn map_local_placement_result<F: SyncFs>(
    fs: &F,
    path: &str,
    size: u64,
    peer_id: &str,
    res: Result<ApplyOutcome>,
) -> Result<ApplyOutcome> {
    match res {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            if let Some(prob) = classify_local_placement_problem(fs, path, size, peer_id, &err) {
                Ok(ApplyOutcome::HoldBackLocal(prob))
            } else {
                Err(err)
            }
        }
    }
}

/// Recovers any incomplete [`PendingOp`] left by a crash or error and cleans only proven
/// engine-written staged downloads from `.legion-sync/tmp/`, rescuing any unknown bytes into the
/// live tree as conflict copies.
///
/// Idempotent: crashing again during recovery converges cleanly on the next restart.
pub fn recover_pending<F: SyncFs, S: IndexStore>(fs: &F, store: &mut S) -> Result<RecoveryReport> {
    recover_pending_at(fs, store, current_timestamp())
}

fn recover_pending_at<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    now: Timestamp,
) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();

    if let Some(op) = store.load_pending_op()? {
        report.recovered_pending_op = true;
        match op {
            PendingOp::Place {
                target_record,
                staged_tmp,
                claim_tmp,
                bin_entry,
                expected_local,
            } => {
                recover_place(
                    fs,
                    store,
                    target_record,
                    staged_tmp,
                    claim_tmp,
                    bin_entry,
                    expected_local,
                    now,
                )?;
            }
            PendingOp::Delete {
                target_record,
                claim_tmp,
                bin_entry,
                expected_local,
            } => {
                recover_delete(
                    fs,
                    store,
                    target_record,
                    claim_tmp,
                    bin_entry,
                    expected_local,
                    now,
                )?;
            }
            PendingOp::LocalRename {
                from_record,
                to_record,
                expected_from,
            } => {
                recover_local_rename(fs, store, from_record, to_record, expected_from)?;
            }
            PendingOp::ConflictLocalLoses {
                winner_record,
                loser_record,
                winner_tmp,
                conflict_already_present,
                expected_local,
            } => {
                recover_conflict_local_loses(
                    fs,
                    store,
                    winner_record,
                    loser_record,
                    winner_tmp,
                    conflict_already_present,
                    expected_local,
                    now,
                )?;
            }
            PendingOp::ConflictRemoteLoses {
                winner_record,
                loser_record,
                loser_tmp,
                conflict_already_present,
                expected_local,
            } => {
                recover_conflict_remote_loses(
                    fs,
                    store,
                    winner_record,
                    loser_record,
                    loser_tmp,
                    conflict_already_present,
                    expected_local,
                    now,
                )?;
            }
            PendingOp::RestoreFromBin {
                target_record,
                restored_bin_id,
                restored_storage_name,
                staged_tmp,
                claim_tmp,
                replacement_bin,
                expected_local,
            } => {
                recover_restore_from_bin(
                    fs,
                    store,
                    target_record,
                    restored_bin_id,
                    restored_storage_name,
                    staged_tmp,
                    claim_tmp,
                    replacement_bin,
                    expected_local,
                    now,
                )?;
            }
            PendingOp::ResolveKeepConflict {
                promoted_record,
                deleted_conflict_record,
                claim_tmp,
                bin_entry,
                expected_orig,
                expected_conflict,
            } => {
                recover_resolve_keep_conflict(
                    fs,
                    store,
                    promoted_record,
                    deleted_conflict_record,
                    claim_tmp,
                    bin_entry,
                    expected_orig,
                    expected_conflict,
                    now,
                )?;
            }
        }
    }

    // Core principle: NEVER delete bytes the engine cannot prove it wrote itself.
    // Only files explicitly recorded in `STAGED_TMPS` are unlinked; any other file remaining in
    // `.legion-sync/tmp/` is preserved as a conflict copy in the live tree.
    let staged_tmps = store.list_staged_tmps()?;
    let mut cleared_staged = Vec::new();
    for tmp_name in fs.list_tmp()? {
        if staged_tmps.contains(&tmp_name) {
            let _ = fs.remove_tmp(&tmp_name);
            cleared_staged.push(tmp_name);
            report.cleaned_tmp_files += 1;
        } else if store.load_meta()?.is_some() {
            rescue_orphan_tmp(fs, store, &tmp_name, now)?;
        }
    }
    // Clear any remaining `STAGED_TMPS` records (including beside-destination `.legion-*.tmp`
    // files staged on separate subvolumes/mounts that do not appear in `list_tmp()`).
    for tmp_name in staged_tmps {
        if !cleared_staged.contains(&tmp_name) {
            if fs.stat_tmp(&tmp_name).ok().flatten().is_some() {
                let _ = fs.remove_tmp(&tmp_name);
                report.cleaned_tmp_files += 1;
            }
            cleared_staged.push(tmp_name);
        }
    }
    if !cleared_staged.is_empty() {
        store.apply_batch(StoreBatch {
            remove_staged_tmps: cleared_staged,
            ..StoreBatch::default()
        })?;
    }

    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn recover_place<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut target_record: IndexRecord,
    staged_tmp: Option<String>,
    claim_tmp: Option<String>,
    bin_entry: Option<BinEntry>,
    expected_local: ExpectedLocalSnapshot,
    now: Timestamp,
) -> Result<()> {
    let path = target_record.path.clone();
    let expected_staged = target_record.hash.map(|h| (h, target_record.size));

    // Case 1: target is already materialized on disk with expected content.
    if disk_matches_record(fs, &path, &target_record)? {
        for cand in [claim_tmp.as_deref(), staged_tmp.as_deref()]
            .into_iter()
            .flatten()
        {
            let _ = salvage_pending_tmp(
                fs,
                store,
                cand,
                &path,
                &expected_local,
                bin_entry.as_ref(),
                expected_staged,
                now,
            )?;
        }
        record_disk_stat(fs, &path, &mut target_record)?;
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![target_record],
            persisted_bin.into_iter().collect(),
            Vec::new(),
            Vec::new(),
        )?;
        return Ok(());
    }

    // Case 2: disk still holds the untouched pre-apply entry -> safely roll back without deleting
    // any non-staged tmp bytes.
    if disk_matches_snapshot(fs, &path, &expected_local)?
        && !matches!(expected_local, ExpectedLocalSnapshot::Missing)
    {
        let mut displaced_binned = false;
        for cand in [claim_tmp.as_deref(), staged_tmp.as_deref()]
            .into_iter()
            .flatten()
        {
            if salvage_pending_tmp(
                fs,
                store,
                cand,
                &path,
                &expected_local,
                bin_entry.as_ref(),
                expected_staged,
                now,
            )? {
                displaced_binned = true;
            }
        }
        let persisted_bin = bin_entry_if_present(fs, bin_entry.clone())?;
        if persisted_bin.is_none()
            && let Some(ref b) = bin_entry
        {
            let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
        }
        if displaced_binned {
            mark_needs_rescan_and_clear_pending(
                store,
                &[&path],
                persisted_bin.into_iter().collect(),
            )?;
        } else {
            let batch = StoreBatch {
                upsert_bin: persisted_bin.into_iter().collect(),
                set_pending_op: Some(None),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
        }
        return Ok(());
    }

    // Case 3: `path` is missing on disk (crash happened after claiming/binning old file and
    // before placing the new entry).
    if fs.stat_live(&path)?.is_none() {
        // First check if `claim_tmp` still holds the claimed old live file.
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            if !tmp_matches_snapshot(fs, claim, &expected_local)? {
                // Unseen human edit was claimed right before crash — put it right back (or
                // preserve as conflict copy if `path` appeared concurrently).
                put_back_or_preserve_tmp(fs, store, claim, &path, None, now)?;
                if let Some(ref st) = staged_tmp {
                    let _ = salvage_pending_tmp(
                        fs,
                        store,
                        st,
                        &path,
                        &expected_local,
                        bin_entry.as_ref(),
                        expected_staged,
                        now,
                    )?;
                }
                mark_needs_rescan_and_clear_pending(store, &[&path], Vec::new())?;
                return Ok(());
            }
            if let Some(ref b) = bin_entry
                && !b.storage_name.is_empty()
                && fs.stat_bin(&b.storage_name)?.is_none()
            {
                write_bin_meta_file(fs, b)?;
                fs.move_tmp_to_bin_no_replace(claim, &b.storage_name)?;
            }
        }

        match target_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = staged_tmp
                    && tmp_matches_hash(fs, tmp, target_record.hash, target_record.size)?
                {
                    let staged_meta = prepare_staged_tmp_metadata(
                        fs,
                        tmp,
                        target_record.mtime,
                        target_record.mode,
                    )?;
                    match fs.commit_tmp_to_live_no_replace(tmp, &path) {
                        Ok(()) => {
                            let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                            if verify_and_record_placed_stat(
                                fs,
                                &path,
                                &mut target_record,
                                staged_meta.as_ref(),
                                true,
                                now,
                            )? {
                                commit_records_and_clear_pending(
                                    fs,
                                    store,
                                    vec![target_record],
                                    persisted_bin.into_iter().collect(),
                                    Vec::new(),
                                    Vec::new(),
                                )?;
                            } else {
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[&path],
                                    persisted_bin.into_iter().collect(),
                                )?;
                            }
                            return Ok(());
                        }
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            let _ = fs.remove_tmp(tmp);
                            let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&path],
                                persisted_bin.into_iter().collect(),
                            )?;
                            return Ok(());
                        }
                        Err(err) => return Err(err),
                    }
                }
                // Staged payload missing or holds displaced bytes; salvage it first!
                if let Some(ref tmp) = staged_tmp {
                    let _ = salvage_pending_tmp(
                        fs,
                        store,
                        tmp,
                        &path,
                        &expected_local,
                        bin_entry.as_ref(),
                        expected_staged,
                        now,
                    )?;
                }
                // Restore the moved-to-bin copy if present so nothing vanishes on disk.
                if let Some(ref b) = bin_entry
                    && !b.storage_name.is_empty()
                    && fs.stat_bin(&b.storage_name)?.is_some()
                {
                    let restore_tmp = alloc_tmp_name_for(fs, "recover", &path)?;
                    register_staged_tmp(store, &restore_tmp)?;
                    fs.copy_bin_to_tmp(&b.storage_name, &restore_tmp)?;
                    let _ = prepare_staged_tmp_metadata(fs, &restore_tmp, b.original_mtime, b.mode);
                    match fs.commit_tmp_to_live_no_replace(&restore_tmp, &path) {
                        Ok(()) => {
                            unregister_staged_tmp(store, &restore_tmp)?;
                            let _ = fs.remove_bin_file(&b.storage_name);
                            let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
                        }
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            let _ = fs.remove_tmp(&restore_tmp);
                            unregister_staged_tmp(store, &restore_tmp)?;
                            let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&path],
                                persisted_bin.into_iter().collect(),
                            )?;
                            return Ok(());
                        }
                        Err(err) => {
                            let _ = fs.remove_tmp(&restore_tmp);
                            unregister_staged_tmp(store, &restore_tmp)?;
                            return Err(err);
                        }
                    }
                }
                mark_needs_rescan_and_clear_pending(store, &[&path], Vec::new())?;
                return Ok(());
            }
            EntryKind::Dir => match fs.create_dir_all_live(&path) {
                Ok(()) => {
                    record_disk_stat(fs, &path, &mut target_record)?;
                    let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                    commit_records_and_clear_pending(
                        fs,
                        store,
                        vec![target_record],
                        persisted_bin.into_iter().collect(),
                        Vec::new(),
                        Vec::new(),
                    )?;
                    return Ok(());
                }
                Err(err) if is_mid_apply_race_error(&err) => {}
                Err(err) => return Err(err),
            },
            EntryKind::Symlink => {
                if let Some(ref target) = target_record.link_target {
                    match fs.create_symlink_live(&path, target) {
                        Ok(()) => {
                            record_disk_stat(fs, &path, &mut target_record)?;
                            let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                            commit_records_and_clear_pending(
                                fs,
                                store,
                                vec![target_record],
                                persisted_bin.into_iter().collect(),
                                Vec::new(),
                                Vec::new(),
                            )?;
                            return Ok(());
                        }
                        Err(err) if is_mid_apply_race_error(&err) => {}
                        Err(err) => return Err(err),
                    }
                }
            }
        }
    }

    // Case 4: path on disk matches neither expected_local nor target_record (external edit or
    // recreation while crashed). Salvage every pending tmp file by hash (bin if expected_local,
    // conflict copy if unseen save, unlink ONLY if staged download) and mark path for rescan!
    for cand in [claim_tmp.as_deref(), staged_tmp.as_deref()]
        .into_iter()
        .flatten()
    {
        let _ = salvage_pending_tmp(
            fs,
            store,
            cand,
            &path,
            &expected_local,
            bin_entry.as_ref(),
            expected_staged,
            now,
        )?;
    }
    let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
    mark_needs_rescan_and_clear_pending(store, &[&path], persisted_bin.into_iter().collect())?;
    Ok(())
}

fn recover_delete<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    target_record: IndexRecord,
    claim_tmp: Option<String>,
    bin_entry: Option<BinEntry>,
    expected_local: ExpectedLocalSnapshot,
    now: Timestamp,
) -> Result<()> {
    let path = target_record.path.clone();

    if fs.stat_live(&path)?.is_none() {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            if !tmp_matches_snapshot(fs, claim, &expected_local)? {
                // Human edit was claimed right before crash — put it right back (or preserve as
                // conflict copy if `path` appeared concurrently).
                put_back_or_preserve_tmp(fs, store, claim, &path, None, now)?;
                mark_needs_rescan_and_clear_pending(store, &[&path], Vec::new())?;
                return Ok(());
            }
            if let Some(ref b) = bin_entry
                && !b.storage_name.is_empty()
                && fs.stat_bin(&b.storage_name)?.is_none()
            {
                write_bin_meta_file(fs, b)?;
                fs.move_tmp_to_bin_no_replace(claim, &b.storage_name)?;
            }
        }
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![target_record],
            persisted_bin.into_iter().collect(),
            Vec::new(),
            Vec::new(),
        )?;
        return Ok(());
    }

    // `path` exists on disk (either crash happened before claim, or the person created a new file
    // at `path` after we claimed the old file!). Always salvage `claim_tmp` by hash before
    // clearing `PendingOp` so the claimed prior version reaches `.legion-sync/bin` (or becomes a
    // conflict copy if it was an unseen save).
    if let Some(ref claim) = claim_tmp {
        let _ = salvage_pending_tmp(
            fs,
            store,
            claim,
            &path,
            &expected_local,
            bin_entry.as_ref(),
            None,
            now,
        )?;
    }
    let persisted_bin = bin_entry_if_present(fs, bin_entry.clone())?;
    if persisted_bin.is_none()
        && disk_matches_snapshot(fs, &path, &expected_local)?
        && let Some(ref b) = bin_entry
    {
        let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
    }
    mark_needs_rescan_and_clear_pending(store, &[&path], persisted_bin.into_iter().collect())?;
    Ok(())
}

fn recover_local_rename<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    from_record: IndexRecord,
    mut to_record: IndexRecord,
    expected_from: ExpectedLocalSnapshot,
) -> Result<()> {
    let from_path = from_record.path.clone();
    let to_path = to_record.path.clone();

    if fs.stat_live(&from_path)?.is_none() && disk_matches_record(fs, &to_path, &to_record)? {
        record_disk_stat(fs, &to_path, &mut to_record)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![from_record, to_record],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        return Ok(());
    }

    if disk_matches_snapshot(fs, &from_path, &expected_from)? {
        clear_pending_op(store)?;
        return Ok(());
    }

    mark_needs_rescan_and_clear_pending(store, &[&from_path, &to_path], Vec::new())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover_conflict_local_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    winner_tmp: Option<String>,
    conflict_already_present: bool,
    expected_local: ExpectedLocalSnapshot,
    now: Timestamp,
) -> Result<()> {
    let orig_path = winner_record.path.clone();
    let conflict_path = loser_record.path.clone();
    let expected_staged = winner_record.hash.map(|h| (h, winner_record.size));

    let winner_done = disk_matches_record(fs, &orig_path, &winner_record)?;
    let loser_done = disk_matches_record(fs, &conflict_path, &loser_record)?;

    if winner_done && loser_done {
        if let Some(ref wt) = winner_tmp {
            let _ = salvage_pending_tmp(
                fs,
                store,
                wt,
                &orig_path,
                &expected_local,
                None,
                expected_staged,
                now,
            )?;
        }
        record_disk_stat(fs, &orig_path, &mut winner_record)?;
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        return Ok(());
    }

    if loser_done && fs.stat_live(&orig_path)?.is_none() {
        let mut staged_meta = None;
        let placed = match winner_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = winner_tmp
                    && tmp_matches_hash(fs, tmp, winner_record.hash, winner_record.size)?
                {
                    staged_meta = prepare_staged_tmp_metadata(
                        fs,
                        tmp,
                        winner_record.mtime,
                        winner_record.mode,
                    )?;
                    match fs.commit_tmp_to_live_no_replace(tmp, &orig_path) {
                        Ok(()) => {
                            unregister_staged_tmp(store, tmp)?;
                            true
                        }
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            let _ = fs.remove_tmp(tmp);
                            unregister_staged_tmp(store, tmp)?;
                            false
                        }
                        Err(err) => return Err(err),
                    }
                } else {
                    false
                }
            }
            EntryKind::Dir => match fs.create_dir_all_live(&orig_path) {
                Ok(()) => true,
                Err(err) if is_mid_apply_race_error(&err) => false,
                Err(err) => return Err(err),
            },
            EntryKind::Symlink => {
                if let Some(ref target) = winner_record.link_target {
                    match fs.create_symlink_live(&orig_path, target) {
                        Ok(()) => true,
                        Err(err) if is_mid_apply_race_error(&err) => false,
                        Err(err) => return Err(err),
                    }
                } else {
                    false
                }
            }
        };

        if placed {
            let winner_ok = if winner_record.kind == EntryKind::File {
                verify_and_record_placed_stat(
                    fs,
                    &orig_path,
                    &mut winner_record,
                    staged_meta.as_ref(),
                    true,
                    now,
                )?
            } else {
                record_disk_stat(fs, &orig_path, &mut winner_record)?;
                true
            };
            record_disk_stat(fs, &conflict_path, &mut loser_record)?;
            if winner_ok {
                commit_records_and_clear_pending(
                    fs,
                    store,
                    vec![loser_record, winner_record],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )?;
            } else {
                commit_records_and_clear_pending(
                    fs,
                    store,
                    vec![loser_record],
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )?;
                mark_needs_rescan(store, &[&orig_path])?;
            }
            return Ok(());
        }

        if let Some(ref wt) = winner_tmp {
            let _ = salvage_pending_tmp(
                fs,
                store,
                wt,
                &orig_path,
                &expected_local,
                None,
                expected_staged,
                now,
            )?;
        }
        if !conflict_already_present && loser_record.kind == EntryKind::File {
            match fs.rename_live_to_live_no_replace(&conflict_path, &orig_path) {
                Ok(()) => {}
                Err(SyncError::Fs {
                    kind: FsErrorKind::AlreadyExists | FsErrorKind::NotFound,
                    ..
                }) => {
                    // Leave `conflict_path` intact and let rescan pick up both files.
                }
                Err(err) => return Err(err),
            }
        }
        mark_needs_rescan_and_clear_pending(store, &[&orig_path, &conflict_path], Vec::new())?;
        return Ok(());
    }

    if let Some(ref wt) = winner_tmp {
        let _ = salvage_pending_tmp(
            fs,
            store,
            wt,
            &orig_path,
            &expected_local,
            None,
            expected_staged,
            now,
        )?;
    }
    if disk_matches_snapshot(fs, &orig_path, &expected_local)? {
        clear_pending_op(store)?;
        return Ok(());
    }

    mark_needs_rescan_and_clear_pending(store, &[&orig_path, &conflict_path], Vec::new())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover_conflict_remote_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    loser_tmp: Option<String>,
    _conflict_already_present: bool,
    expected_local: ExpectedLocalSnapshot,
    now: Timestamp,
) -> Result<()> {
    let orig_path = winner_record.path.clone();
    let conflict_path = loser_record.path.clone();
    let expected_staged = loser_record.hash.map(|h| (h, loser_record.size));

    if let Some(ref lt) = loser_tmp {
        let _ = salvage_pending_tmp(
            fs,
            store,
            lt,
            &conflict_path,
            &expected_local,
            None,
            expected_staged,
            now,
        )?;
    }

    if disk_matches_record(fs, &conflict_path, &loser_record)? {
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )?;
        return Ok(());
    }

    mark_needs_rescan_and_clear_pending(store, &[&orig_path, &conflict_path], Vec::new())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover_restore_from_bin<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut target_record: IndexRecord,
    restored_bin_id: String,
    restored_storage_name: String,
    staged_tmp: Option<String>,
    claim_tmp: Option<String>,
    replacement_bin: Option<BinEntry>,
    expected_local: ExpectedLocalSnapshot,
    now: Timestamp,
) -> Result<()> {
    let path = target_record.path.clone();
    let expected_staged = target_record.hash.map(|h| (h, target_record.size));

    if disk_matches_record(fs, &path, &target_record)? {
        for cand in [claim_tmp.as_deref(), staged_tmp.as_deref()]
            .into_iter()
            .flatten()
        {
            let _ = salvage_pending_tmp(
                fs,
                store,
                cand,
                &path,
                &expected_local,
                replacement_bin.as_ref(),
                expected_staged,
                now,
            )?;
        }
        record_disk_stat(fs, &path, &mut target_record)?;
        let persisted_rep = bin_entry_if_present(fs, replacement_bin)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![target_record],
            persisted_rep.into_iter().collect(),
            Vec::new(),
            vec![restored_bin_id.clone()],
        )?;
        if !restored_storage_name.is_empty() {
            let _ = fs.remove_bin_file(&restored_storage_name);
        }
        let _ = fs.remove_bin_file(&bin_meta_name(&restored_bin_id));
        return Ok(());
    }

    if fs.stat_live(&path)?.is_none() {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            put_back_or_preserve_tmp(fs, store, claim, &path, None, now)?;
        } else if let Some(ref b) = replacement_bin
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_some()
        {
            let rt = alloc_tmp_name_for(fs, "recover-restore", &path)?;
            register_staged_tmp(store, &rt)?;
            if fs.copy_bin_to_tmp(&b.storage_name, &rt).is_ok() {
                let _ = prepare_staged_tmp_metadata(fs, &rt, b.original_mtime, b.mode);
                if fs.commit_tmp_to_live_no_replace(&rt, &path).is_ok() {
                    let _ = fs.remove_bin_file(&b.storage_name);
                    let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
                } else {
                    let _ = fs.remove_tmp(&rt);
                }
            } else {
                let _ = fs.remove_tmp(&rt);
            }
            unregister_staged_tmp(store, &rt)?;
        }
    }

    for cand in [claim_tmp.as_deref(), staged_tmp.as_deref()]
        .into_iter()
        .flatten()
    {
        let _ = salvage_pending_tmp(
            fs,
            store,
            cand,
            &path,
            &expected_local,
            replacement_bin.as_ref(),
            expected_staged,
            now,
        )?;
    }
    let persisted_rep = bin_entry_if_present(fs, replacement_bin)?;
    mark_needs_rescan_and_clear_pending(store, &[&path], persisted_rep.into_iter().collect())?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn recover_resolve_keep_conflict<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut promoted_record: IndexRecord,
    deleted_conflict_record: IndexRecord,
    claim_tmp: Option<String>,
    bin_entry: Option<BinEntry>,
    expected_orig: ExpectedLocalSnapshot,
    _expected_conflict: ExpectedLocalSnapshot,
    now: Timestamp,
) -> Result<()> {
    let orig_path = promoted_record.path.clone();
    let conflict_path = deleted_conflict_record.path.clone();

    if disk_matches_record(fs, &orig_path, &promoted_record)?
        && fs.stat_live(&conflict_path)?.is_none()
    {
        if let Some(ref claim) = claim_tmp {
            let _ = salvage_pending_tmp(
                fs,
                store,
                claim,
                &orig_path,
                &expected_orig,
                bin_entry.as_ref(),
                None,
                now,
            )?;
        }
        record_disk_stat(fs, &orig_path, &mut promoted_record)?;
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(
            fs,
            store,
            vec![deleted_conflict_record, promoted_record],
            persisted_bin.into_iter().collect(),
            Vec::new(),
            Vec::new(),
        )?;
        return Ok(());
    }

    // Roll back: if orig_path is missing and claim_tmp or bin holds the original file, restore it.
    if fs.stat_live(&orig_path)?.is_none() {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            put_back_or_preserve_tmp(fs, store, claim, &orig_path, None, now)?;
        } else if let Some(ref b) = bin_entry
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_some()
        {
            let rt = alloc_tmp_name_for(fs, "recover-resolve", &orig_path)?;
            register_staged_tmp(store, &rt)?;
            if fs.copy_bin_to_tmp(&b.storage_name, &rt).is_ok() {
                let _ = prepare_staged_tmp_metadata(fs, &rt, b.original_mtime, b.mode);
                if fs.commit_tmp_to_live_no_replace(&rt, &orig_path).is_ok() {
                    let _ = fs.remove_bin_file(&b.storage_name);
                    let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
                } else {
                    let _ = fs.remove_tmp(&rt);
                }
            } else {
                let _ = fs.remove_tmp(&rt);
            }
            unregister_staged_tmp(store, &rt)?;
        }
    }

    if let Some(ref claim) = claim_tmp {
        let _ = salvage_pending_tmp(
            fs,
            store,
            claim,
            &orig_path,
            &expected_orig,
            bin_entry.as_ref(),
            None,
            now,
        )?;
    }
    let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
    mark_needs_rescan_and_clear_pending(
        store,
        &[&orig_path, &conflict_path],
        persisted_bin.into_iter().collect(),
    )?;
    Ok(())
}

/// Applies one reconciled decision from `peer` crash-safely, recovering immediately on any
/// non-crash error.
#[allow(clippy::too_many_arguments)]
pub fn apply_decision<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    decision: ReconcileDecision,
    ignore: &IgnoreMatcher,
    fetch_opts: &FetchOptions,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    match decision {
        ReconcileDecision::RejectInvalid { path, reason } => {
            Err(SyncError::InvalidPath { path, reason })
        }
        ReconcileDecision::NotYetKnownMember { .. } => Ok(ApplyOutcome::Applied),
        ReconcileDecision::Ignore | ReconcileDecision::Keep => Ok(ApplyOutcome::Applied),
        ReconcileDecision::HoldBack { .. } => Ok(ApplyOutcome::Applied),
        ReconcileDecision::KeepAndSupersede { mut record } => {
            let mut meta = load_meta_required(store)?;
            meta.bump_version(&mut record.version);
            let seq = meta.next_seq();
            record.seq = seq;
            record.modified_by = meta.local_member.clone();
            record.from_join = false;
            record.conflict_of = derive_conflict_of(&record, &meta);
            let chain = meta.advance_chain_fingerprint(seq, record.head_fingerprint());
            if record.deleted {
                let encoded =
                    encode_deletion_log_entries(&[FolderDeletionLogEntry::from_tombstone(
                        &record,
                    )])?;
                fs.append_deletion_log(&encoded)?;
            }
            let mut batch = StoreBatch {
                folder_meta: Some(meta.clone()),
                remove_held_back: vec![(peer.member_id().to_string(), record.path.clone())],
                upsert_seq_fingerprints: vec![(seq, chain)],
                ..StoreBatch::default()
            };
            if record.deleted && !record.recent_hashes.is_empty() {
                let mut merged_dh = record.recent_hashes.clone();
                for h in store.get_deleted_history(&record.path)? {
                    if merged_dh.len() >= MAX_ANCESTRY_HASHES {
                        break;
                    }
                    if !merged_dh.contains(&h) {
                        merged_dh.push(h);
                    }
                }
                batch
                    .upsert_deleted_history
                    .push((record.path.clone(), merged_dh));
                let del_vv = store
                    .get_deleted_history_version(&record.path)?
                    .unwrap_or_default()
                    .merge(&record.version)
                    .pruned(&meta.pruned_incarnations);
                batch
                    .upsert_deleted_history_versions
                    .push((record.path.clone(), del_vv));
            }
            batch.upsert_records.push(record);
            store.apply_batch(batch)?;
            Ok(ApplyOutcome::Applied)
        }
        ReconcileDecision::MetadataOnly {
            mut record,
            update_disk_metadata,
        } => {
            let existing = store.get_record(&record.path)?;
            if !record.deleted {
                match disk_matches_index(fs, &record.path, existing.as_ref()) {
                    Ok(true) => {}
                    Ok(false) => return handle_local_changed(fs, store, &record.path, now),
                    Err(err) if is_live_busy_error(fs, &err) => {
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&record.path),
                        });
                    }
                    Err(err) => return Err(err),
                }
            }
            if update_disk_metadata && !record.deleted {
                apply_live_metadata(fs, &record.path, record.mtime, record.mode)?;
                let p = record.path.clone();
                if fs.stat_live(&p)?.is_none() {
                    return handle_local_changed(fs, store, &p, now);
                }
                record_disk_stat(fs, &p, &mut record)?;
            } else if !record.deleted {
                if let Some(ref ex) = existing {
                    if record.disk_mtime.is_none() {
                        record.disk_mtime = ex.disk_mtime;
                    }
                    if record.disk_change_id.is_none() {
                        record.disk_change_id = ex.disk_change_id;
                    }
                }
                if record.disk_mtime.is_none() {
                    let p = record.path.clone();
                    record_disk_stat(fs, &p, &mut record)?;
                }
            }
            let path = record.path.clone();
            commit_records_and_clear_pending(
                fs,
                store,
                vec![record],
                Vec::new(),
                vec![(peer.member_id().to_string(), path)],
                Vec::new(),
            )?;
            Ok(ApplyOutcome::Applied)
        }
        ReconcileDecision::ReviveDir { path, base_version } => {
            let mut meta = load_meta_required(store)?;
            let mut version = base_version;
            meta.bump_version(&mut version);
            let seq = meta.next_seq();
            let stat = fs.stat_live(&path)?;
            let mtime = stat.as_ref().map_or(now, |m| m.mtime);
            let revived = IndexRecord {
                path: path.clone(),
                kind: EntryKind::Dir,
                size: 0,
                mtime,
                disk_mtime: Some(mtime),
                disk_change_id: stat.and_then(|m| m.change_id),
                mode: None,
                hash: None,
                link_target: None,
                deleted: false,
                version,
                modified_by: meta.local_member.clone(),
                seq,
                recent_hashes: Vec::new(),
                from_join: false,
                conflict_of: None,
                conflict_dismissed: false,
            };
            let chain = meta.advance_chain_fingerprint(seq, revived.head_fingerprint());
            let encoded = encode_deletion_log_entries(&[FolderDeletionLogEntry::Revived {
                path: path.clone(),
            }])?;
            fs.append_deletion_log(&encoded)?;
            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records: vec![revived],
                remove_held_back: vec![(peer.member_id().to_string(), path)],
                upsert_seq_fingerprints: vec![(seq, chain)],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(ApplyOutcome::Applied)
        }
        ReconcileDecision::Fetch { record, bin_reason } => {
            apply_fetch(fs, store, peer, record, bin_reason, fetch_opts, now)
        }
        ReconcileDecision::DeleteToBin {
            tombstone,
            bin_reason,
        } => apply_delete(
            fs,
            store,
            peer.member_id(),
            tombstone,
            bin_reason,
            ignore,
            now,
        ),
        ReconcileDecision::ConflictLocalLoses {
            winner_record,
            loser_record,
        } => apply_conflict_local_loses(
            fs,
            store,
            peer,
            winner_record,
            loser_record,
            fetch_opts,
            now,
        ),
        ReconcileDecision::ConflictRemoteLoses {
            winner_record,
            loser_record,
            bump_winner_clock,
        } => apply_conflict_remote_loses(
            fs,
            store,
            peer,
            winner_record,
            loser_record,
            bump_winner_clock,
            fetch_opts,
            now,
        ),
    }
}

/// Applies a local rename optimization when a page deletes `from_record.path` and creates
/// `to_record.path` with the exact same content hash already present on local disk (zero network
/// download, zero duplicate copy).
pub fn apply_local_rename<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer_member: &str,
    from_record: IndexRecord,
    mut to_record: IndexRecord,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let from_path = from_record.path.clone();
    let to_path = to_record.path.clone();
    let to_size = to_record.size;

    match ensure_ancestors_are_dirs(fs, store, &to_path, now)? {
        AncestorOutcome::Ready => {}
        AncestorOutcome::ChangedRetry => return Ok(ApplyOutcome::LocalChangedRetry),
        AncestorOutcome::Unstable => {
            return Ok(ApplyOutcome::Deferred {
                reason: format!("ancestor of '{to_path}' is currently being written"),
            });
        }
        AncestorOutcome::Busy(reason) => return Ok(ApplyOutcome::Deferred { reason }),
    }

    let existing_from = store.get_record(&from_path)?;
    match disk_matches_index(fs, &from_path, existing_from.as_ref()) {
        Ok(true) => {}
        Ok(false) => return handle_local_changed(fs, store, &from_path, now),
        Err(err) if is_live_busy_error(fs, &err) => {
            return Ok(ApplyOutcome::Deferred {
                reason: busy_defer_reason(&from_path),
            });
        }
        Err(err) => return Err(err),
    }
    match fs.stat_live(&to_path) {
        Ok(Some(_)) => return handle_local_changed(fs, store, &to_path, now),
        Ok(None) => {}
        Err(err) if is_live_busy_error(fs, &err) => {
            return Ok(ApplyOutcome::Deferred {
                reason: busy_defer_reason(&to_path),
            });
        }
        Err(err) => return Err(err),
    }

    let expected_from = snapshot_local(existing_from.as_ref());
    let pending = PendingOp::LocalRename {
        from_record: from_record.clone(),
        to_record: to_record.clone(),
        expected_from,
    };
    write_pending_op(store, pending)?;

    let res: Result<ApplyOutcome> = (|| {
        match fs.rename_live_to_live_no_replace(&from_path, &to_path) {
            Ok(()) => {}
            Err(SyncError::Fs {
                kind: FsErrorKind::AlreadyExists,
                ..
            }) => {
                clear_pending_op(store)?;
                return handle_local_changed(fs, store, &to_path, now);
            }
            Err(SyncError::Fs {
                kind: FsErrorKind::NotFound,
                ..
            })
            | Err(SyncError::Conflict { .. }) => {
                mark_needs_rescan_and_clear_pending(store, &[&from_path, &to_path], Vec::new())?;
                return handle_local_changed(fs, store, &from_path, now);
            }
            Err(err) if is_live_busy_error(fs, &err) => {
                clear_pending_op(store)?;
                return Ok(ApplyOutcome::Deferred {
                    reason: busy_defer_reason(&from_path),
                });
            }
            Err(err) => return Err(err),
        }

        // Verify the renamed file still holds the expected hash/size (if a person edited
        // `from_path` right before rename, put it back without ignoring errors).
        if let Some(expected_hash) = to_record.hash {
            let (actual_hash, actual_size) = match fs.hash_live(&to_path) {
                Ok(pair) => pair,
                Err(err) if is_mid_apply_race_error(&err) => {
                    mark_needs_rescan_and_clear_pending(
                        store,
                        &[&from_path, &to_path],
                        Vec::new(),
                    )?;
                    let _ = rescan_single_path(fs, store, &from_path, now)?;
                    return handle_local_changed(fs, store, &to_path, now);
                }
                Err(err) => return Err(err),
            };
            if actual_hash != expected_hash || actual_size != to_record.size {
                match fs.rename_live_to_live_no_replace(&to_path, &from_path) {
                    Ok(()) => {
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &from_path, now);
                    }
                    Err(err) if is_mid_apply_race_error(&err) => {
                        // A person already created a new file at `from_path` (or moved `to_path`);
                        // keep both intact and mark both for rescan.
                        mark_needs_rescan_and_clear_pending(
                            store,
                            &[&from_path, &to_path],
                            Vec::new(),
                        )?;
                        let _ = rescan_single_path(fs, store, &to_path, now)?;
                        return handle_local_changed(fs, store, &from_path, now);
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        apply_live_metadata(fs, &to_path, to_record.mtime, to_record.mode)?;
        if !verify_and_record_placed_stat(fs, &to_path, &mut to_record, None, true, now)? {
            mark_needs_rescan_and_clear_pending(store, &[&from_path, &to_path], Vec::new())?;
            return handle_local_changed(fs, store, &to_path, now);
        }
        commit_records_and_clear_pending(
            fs,
            store,
            vec![from_record, to_record],
            Vec::new(),
            vec![
                (peer_member.to_string(), from_path),
                (peer_member.to_string(), to_path.clone()),
            ],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    let res = recover_on_non_crash_error(fs, store, res, now);
    map_local_placement_result(fs, &to_path, to_size, peer_member, res)
}

/// Applies a batch of clean brand-new regular files (no existing live entry at any target path),
/// staging and BLAKE3-verifying chunks of files, syncing every file before any non-replacing
/// rename, syncing touched parent directories and `.legion-sync/tmp` once per page, and committing
/// all records in ONE single `IndexStore::apply_batch` group commit.
///
/// Every staged tmp filename is registered in `STAGED_TMPS` per chunk *before* creation, metadata
/// (mtime + mode) is applied on the staged file *before* placement, post-placement stat is
/// recorded only when identity matches the staged file, and every touched parent directory plus
/// `.legion-sync/tmp` is synced before the single ACID index commit.
pub fn apply_new_files_group_commit<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    records: Vec<IndexRecord>,
    fetch_opts: &FetchOptions,
    now: Timestamp,
) -> Result<BatchApplyResult> {
    let mut out = BatchApplyResult::default();
    if records.is_empty() {
        return Ok(out);
    }

    let mut placed: Vec<(u64, IndexRecord)> = Vec::with_capacity(records.len());
    let mut synced_parents: HashSet<String> = HashSet::new();
    let mut created_ancestors: BTreeSet<String> = BTreeSet::new();
    let mut tmps_to_unregister: Vec<String> = Vec::with_capacity(records.len());
    let mut needs_rescan_paths: Vec<String> = Vec::new();

    let mut rec_iter = records.into_iter().peekable();
    while rec_iter.peek().is_some() {
        // Prepare a chunk of up to `GROUP_STAGE_MAX_FILES` / `GROUP_STAGE_MAX_BYTES`, checking
        // ancestors first and batch-registering all chunk tmp names in ONE store transaction
        // before creating any tmp file.
        let mut chunk_specs: Vec<(u64, IndexRecord, Hash, String)> =
            Vec::with_capacity(GROUP_STAGE_MAX_FILES);
        let mut chunk_bytes: u64 = 0;

        while chunk_specs.len() < GROUP_STAGE_MAX_FILES
            && (chunk_specs.is_empty() || chunk_bytes < GROUP_STAGE_MAX_BYTES)
        {
            let Some(target) = rec_iter.next() else {
                break;
            };
            let remote_seq = target.seq;
            let path = target.path.clone();

            let mut ancestor_ok = true;
            for anc in ancestors(&path) {
                if created_ancestors.contains(anc) {
                    continue;
                }
                if let Some(rec) = store.get_record(anc)? {
                    if !rec.deleted && rec.kind == EntryKind::Dir {
                        match fs.stat_live(anc)? {
                            Some(m) if m.kind == EntryKind::Dir => {
                                created_ancestors.insert(anc.to_string());
                                continue;
                            }
                            None => match fs.create_dir_all_live(anc) {
                                Ok(()) => {
                                    created_ancestors.insert(anc.to_string());
                                    continue;
                                }
                                Err(err) if is_mid_apply_race_error(&err) => {
                                    ancestor_ok = false;
                                    break;
                                }
                                Err(err) => return Err(err),
                            },
                            Some(_) => {
                                ancestor_ok = false;
                                break;
                            }
                        }
                    }
                    if !rec.deleted && rec.kind != EntryKind::Dir {
                        ancestor_ok = false;
                        break;
                    }
                }
                if fs.stat_live(anc)?.is_some_and(|m| m.kind != EntryKind::Dir) {
                    ancestor_ok = false;
                    break;
                }
                if !matches!(
                    ensure_ancestors_are_dirs(fs, store, &path, now)?,
                    AncestorOutcome::Ready
                ) {
                    ancestor_ok = false;
                    break;
                }
                created_ancestors.insert(anc.to_string());
            }
            if !ancestor_ok {
                out.fallback_records.push(target);
                continue;
            }

            let Some(expected_hash) = target.hash else {
                out.fallback_records.push(target);
                continue;
            };

            if let Some(prob) = check_free_space_for_file(fs, &path, target.size, peer.member_id())?
            {
                out.held_back_local.push((remote_seq, path, prob));
                continue;
            }

            let tmp_name = alloc_tmp_name_for(fs, "fetch", &path)?;
            chunk_bytes = chunk_bytes.saturating_add(target.size);
            chunk_specs.push((remote_seq, target, expected_hash, tmp_name));
        }

        if chunk_specs.is_empty() {
            continue;
        }

        // Write-ahead prove EVERY staged download filename in this chunk in ONE transaction!
        let chunk_tmp_names: Vec<String> = chunk_specs.iter().map(|s| s.3.clone()).collect();
        store.apply_batch(StoreBatch {
            add_staged_tmps: chunk_tmp_names.clone(),
            ..StoreBatch::default()
        })?;
        tmps_to_unregister.extend(chunk_tmp_names);

        let mut staged: Vec<(u64, IndexRecord, String, Box<dyn SyncWrite>)> =
            Vec::with_capacity(chunk_specs.len());
        for (remote_seq, target, expected_hash, tmp_name) in chunk_specs {
            let path = target.path.clone();
            if let Some(prob) = check_free_space_for_file(fs, &path, target.size, peer.member_id())?
            {
                out.held_back_local.push((remote_seq, path, prob));
                continue;
            }
            match fetch_and_verify_into_named_tmp_staged(
                fs,
                peer,
                &path,
                &tmp_name,
                target.size,
                expected_hash,
                fetch_opts,
            ) {
                Ok(writer) => {
                    staged.push((remote_seq, target, tmp_name, writer));
                }
                Err(err) if is_peer_file_changed_error(&err) => {
                    out.deferred.push((remote_seq, path, err.to_string()));
                }
                Err(err) => {
                    let _ = fs.remove_tmp(&tmp_name);
                    if let Some(prob) = classify_local_placement_problem(
                        fs,
                        &path,
                        target.size,
                        peer.member_id(),
                        &err,
                    ) {
                        out.held_back_local.push((remote_seq, path, prob));
                        continue;
                    }
                    cleanup_staged_writers(fs, staged);
                    if !err.is_crash() {
                        let _ = flush_group_commit_records(
                            fs,
                            store,
                            peer.member_id(),
                            &mut placed,
                            &synced_parents,
                            &mut tmps_to_unregister,
                            &mut needs_rescan_paths,
                            &mut out,
                        );
                    }
                    return Err(err);
                }
            }
        }

        if !staged.is_empty() {
            place_staged_chunk(
                fs,
                store,
                peer.member_id(),
                staged,
                &mut placed,
                &mut synced_parents,
                &mut tmps_to_unregister,
                &mut needs_rescan_paths,
                &mut out,
                now,
            )?;
        }
    }

    if !placed.is_empty() || !tmps_to_unregister.is_empty() || !needs_rescan_paths.is_empty() {
        flush_group_commit_records(
            fs,
            store,
            peer.member_id(),
            &mut placed,
            &synced_parents,
            &mut tmps_to_unregister,
            &mut needs_rescan_paths,
            &mut out,
        )?;
    }

    Ok(out)
}

fn cleanup_staged_writers<F: SyncFs>(
    fs: &F,
    staged: Vec<(u64, IndexRecord, String, Box<dyn SyncWrite>)>,
) {
    for (_, _, tmp, writer) in staged {
        drop(writer);
        let _ = fs.remove_tmp(&tmp);
    }
}

#[allow(clippy::too_many_arguments)]
fn place_staged_chunk<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer_id: &str,
    staged: Vec<(u64, IndexRecord, String, Box<dyn SyncWrite>)>,
    placed: &mut Vec<(u64, IndexRecord)>,
    synced_parents: &mut HashSet<String>,
    tmps_to_unregister: &mut Vec<String>,
    needs_rescan_paths: &mut Vec<String>,
    out: &mut BatchApplyResult,
    now: Timestamp,
) -> Result<()> {
    // Phase 1: sync EVERY staged file in the chunk to durable storage and apply mtime + mode on
    // the staged file BEFORE any rename into the live tree.
    let mut synced: Vec<(u64, IndexRecord, String, Option<FsMeta>)> =
        Vec::with_capacity(staged.len());
    let mut iter = staged.into_iter();
    while let Some((remote_seq, target, tmp, writer)) = iter.next() {
        let path = target.path.clone();
        if let Err(err) = writer.finish_sync() {
            let _ = fs.remove_tmp(&tmp);
            if let Some(prob) =
                classify_local_placement_problem(fs, &path, target.size, peer_id, &err)
            {
                out.held_back_local.push((remote_seq, path, prob));
                continue;
            }
            for (_, _, rem_tmp, rem_w) in iter {
                drop(rem_w);
                let _ = fs.remove_tmp(&rem_tmp);
            }
            for (_, _, done_tmp, _) in synced {
                let _ = fs.remove_tmp(&done_tmp);
            }
            if !err.is_crash() {
                let _ = flush_group_commit_records(
                    fs,
                    store,
                    peer_id,
                    placed,
                    synced_parents,
                    tmps_to_unregister,
                    needs_rescan_paths,
                    out,
                );
            }
            return Err(err);
        }
        let staged_meta = match prepare_staged_tmp_metadata(fs, &tmp, target.mtime, target.mode) {
            Ok(m) => m,
            Err(err) => {
                let _ = fs.remove_tmp(&tmp);
                if let Some(prob) =
                    classify_local_placement_problem(fs, &path, target.size, peer_id, &err)
                {
                    out.held_back_local.push((remote_seq, path, prob));
                    continue;
                }
                for (_, _, rem_tmp, rem_w) in iter {
                    drop(rem_w);
                    let _ = fs.remove_tmp(&rem_tmp);
                }
                for (_, _, done_tmp, _) in synced {
                    let _ = fs.remove_tmp(&done_tmp);
                }
                if !err.is_crash() {
                    let _ = flush_group_commit_records(
                        fs,
                        store,
                        peer_id,
                        placed,
                        synced_parents,
                        tmps_to_unregister,
                        needs_rescan_paths,
                        out,
                    );
                }
                return Err(err);
            }
        };
        synced.push((remote_seq, target, tmp, staged_meta));
    }

    // Phase 2: non-replacing rename of every synced file into the live tree, recording stat ONLY
    // when the live entry still matches our staged file identity/mtime.
    let mut synced_iter = synced.into_iter();
    while let Some((remote_seq, mut target, tmp, staged_meta)) = synced_iter.next() {
        let path = target.path.clone();
        match fs.commit_tmp_to_live_no_replace_deferred_dir_sync(&tmp, &path) {
            Ok(()) => {}
            Err(err) if is_mid_apply_race_error(&err) => {
                let _ = fs.remove_tmp(&tmp);
                out.fallback_records.push(target);
                continue;
            }
            Err(err) if is_live_busy_error(fs, &err) => {
                let _ = fs.remove_tmp(&tmp);
                out.deferred
                    .push((remote_seq, path.clone(), busy_defer_reason(&path)));
                continue;
            }
            Err(err) => {
                let _ = fs.remove_tmp(&tmp);
                if let Some(prob) =
                    classify_local_placement_problem(fs, &path, target.size, peer_id, &err)
                {
                    out.held_back_local.push((remote_seq, path, prob));
                    continue;
                }
                for (_, _, rem_tmp, _) in synced_iter {
                    let _ = fs.remove_tmp(&rem_tmp);
                }
                if !err.is_crash() {
                    let _ = flush_group_commit_records(
                        fs,
                        store,
                        peer_id,
                        placed,
                        synced_parents,
                        tmps_to_unregister,
                        needs_rescan_paths,
                        out,
                    );
                }
                return Err(err);
            }
        }

        let verified = match verify_and_record_placed_stat(
            fs,
            &path,
            &mut target,
            staged_meta.as_ref(),
            false,
            now,
        ) {
            Ok(v) => v,
            Err(err) => {
                for (_, _, rem_tmp, _) in synced_iter {
                    let _ = fs.remove_tmp(&rem_tmp);
                }
                if !err.is_crash() {
                    let _ = flush_group_commit_records(
                        fs,
                        store,
                        peer_id,
                        placed,
                        synced_parents,
                        tmps_to_unregister,
                        needs_rescan_paths,
                        out,
                    );
                }
                return Err(err);
            }
        };

        if let Some(parent) = parent_path(&path) {
            synced_parents.insert(parent.to_string());
        } else {
            synced_parents.insert(String::new());
        }

        if verified {
            placed.push((remote_seq, target));
        } else {
            // A person replaced, modified, or moved the file right after placement; do not record
            // the remote hash for it, mark for rescan, and fall back to normal reconciliation.
            needs_rescan_paths.push(path);
            out.fallback_records.push(target);
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn flush_group_commit_records<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer_id: &str,
    placed: &mut Vec<(u64, IndexRecord)>,
    synced_parents: &HashSet<String>,
    tmps_to_unregister: &mut Vec<String>,
    needs_rescan_paths: &mut Vec<String>,
    out: &mut BatchApplyResult,
) -> Result<()> {
    // Sync each touched parent directory and `.legion-sync/tmp` once before the single
    // group-commit transaction so every directory entry is durable before the index commits.
    for parent in synced_parents {
        let probe = if parent.is_empty() {
            "x".to_string()
        } else {
            format!("{parent}/x")
        };
        fs.sync_parent_dir_live(&probe);
    }
    fs.sync_tmp_dir();

    let mut meta = load_meta_required(store)?;
    let mut batch = StoreBatch::default();
    let drained = std::mem::take(placed);
    let mut meta_dirty = !drained.is_empty();
    let mut del_log_entries = Vec::new();
    for (remote_seq, mut rec) in drained {
        if store.get_record(&rec.path)?.is_some_and(|p| p.deleted) {
            del_log_entries.push(FolderDeletionLogEntry::Revived {
                path: rec.path.clone(),
            });
        }
        let seq = meta.next_seq();
        rec.seq = seq;
        rec.conflict_of = derive_conflict_of(&rec, &meta);
        let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        batch
            .remove_held_back
            .push((peer_id.to_string(), rec.path.clone()));
        batch.upsert_records.push(rec);
        out.applied_seqs.push(remote_seq);
    }
    if !del_log_entries.is_empty() {
        let encoded = encode_deletion_log_entries(&del_log_entries)?;
        fs.append_deletion_log(&encoded)?;
    }
    for p in needs_rescan_paths.drain(..) {
        if meta.needs_rescan.insert(p) {
            meta_dirty = true;
        }
    }
    if meta_dirty {
        batch.folder_meta = Some(meta);
    }
    batch.remove_staged_tmps = std::mem::take(tmps_to_unregister);
    store.apply_batch(batch)
}

#[allow(clippy::too_many_arguments)]
fn apply_fetch<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    mut target: IndexRecord,
    bin_reason: BinReason,
    fetch_opts: &FetchOptions,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let path = target.path.clone();

    // 1. Pre-check free space and fetch & verify into `.legion-sync/tmp/` (or beside the target on
    // a separate mount/subvolume) before touching the live tree, setting target mtime + mode on
    // the staged file BEFORE placement.
    let (staged_tmp, staged_meta) = if target.kind == EntryKind::File {
        if let Some(prob) = check_free_space_for_file(fs, &path, target.size, peer.member_id())? {
            return Ok(ApplyOutcome::HoldBackLocal(prob));
        }
        let expected_hash = target.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote record for {path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(
            fs,
            store,
            peer,
            &path,
            &path,
            target.size,
            expected_hash,
            fetch_opts,
        ) {
            Ok(tmp) => match prepare_staged_tmp_metadata(fs, &tmp, target.mtime, target.mode) {
                Ok(sm) => (Some(tmp), sm),
                Err(err) => {
                    if !err.is_crash() {
                        let _ = cleanup_staged_tmps(fs, store, &[Some(tmp.as_str())]);
                    }
                    return map_local_placement_result(
                        fs,
                        &path,
                        target.size,
                        peer.member_id(),
                        Err(err),
                    );
                }
            },
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: format!("file changed on peer while fetching: {err}"),
                });
            }
            Err(err) => {
                return map_local_placement_result(
                    fs,
                    &path,
                    target.size,
                    peer.member_id(),
                    Err(err),
                );
            }
        }
    } else {
        (None, None)
    };

    // 2. Ensure ancestors are live directories.
    match ensure_ancestors_are_dirs(fs, store, &path, now) {
        Ok(AncestorOutcome::Ready) => {}
        Ok(AncestorOutcome::ChangedRetry) => {
            cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()])?;
            return Ok(ApplyOutcome::LocalChangedRetry);
        }
        Ok(AncestorOutcome::Unstable) => {
            cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()])?;
            return Ok(ApplyOutcome::Deferred {
                reason: format!("ancestor of '{path}' is actively changing"),
            });
        }
        Ok(AncestorOutcome::Busy(reason)) => {
            cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()])?;
            return Ok(ApplyOutcome::Deferred { reason });
        }
        Err(err) => {
            if !err.is_crash() {
                let _ = cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()]);
            }
            return map_local_placement_result(fs, &path, target.size, peer.member_id(), Err(err));
        }
    }

    // 3. Re-check local disk against our index before replacing anything.
    // For File -> File replacement, `safe_replace_live_file` always hashes the displaced file
    // atomically after swap/claim, so a stat-only pre-check here keeps displaced-file reads at
    // <= 2 (1 read normally, 1-2 on a race).
    let existing = store.get_record(&path)?;
    let use_stat_precheck = target.kind == EntryKind::File
        && existing
            .as_ref()
            .is_some_and(|r| !r.deleted && r.kind == EntryKind::File);
    let precheck_res = if use_stat_precheck {
        disk_stat_matches_index(fs, &path, existing.as_ref())
    } else {
        disk_matches_index(fs, &path, existing.as_ref())
    };
    match precheck_res {
        Ok(true) => {}
        Ok(false) => {
            cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()])?;
            return handle_local_changed(fs, store, &path, now);
        }
        Err(err) if is_live_busy_error(fs, &err) => {
            cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()])?;
            return Ok(ApplyOutcome::Deferred {
                reason: busy_defer_reason(&path),
            });
        }
        Err(err) => {
            if !err.is_crash() {
                let _ = cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()]);
            }
            return map_local_placement_result(fs, &path, target.size, peer.member_id(), Err(err));
        }
    }

    if let Some(ref loc) = existing
        && !loc.deleted
        && loc.kind == EntryKind::Dir
        && target.kind != EntryKind::Dir
        && !fs.list_dir_live(&path)?.is_empty()
    {
        cleanup_staged_tmps(fs, store, &[staged_tmp.as_deref()])?;
        return Ok(ApplyOutcome::LocalChangedRetry);
    }

    let expected_local = snapshot_local(existing.as_ref());
    let mut bin_entry = build_bin_entry(existing.as_ref(), bin_reason, peer.member_id(), now);
    let claim_tmp = if existing
        .as_ref()
        .is_some_and(|r| !r.deleted && r.kind == EntryKind::File)
    {
        Some(alloc_tmp_name_for(fs, "claim", &path)?)
    } else {
        None
    };

    // 4. Persist write-ahead intent BEFORE mutating live files, atomically removing `staged_tmp`
    // from `STAGED_TMPS` so if `exchange_tmp_with_live` swaps a user's file into `staged_tmp`
    // before a crash, recovery will NEVER sweep it unconditionally — `recover_place` inspects it
    // by hash instead.
    let pending = PendingOp::Place {
        target_record: target.clone(),
        staged_tmp: staged_tmp.clone(),
        claim_tmp: claim_tmp.clone(),
        bin_entry: bin_entry.clone(),
        expected_local,
    };
    write_pending_op_and_unregister_tmps(store, pending, staged_tmp.iter().cloned().collect())?;

    let target_size = target.size;
    let res: Result<ApplyOutcome> = (|| {
        let existing_live = existing.as_ref().filter(|r| !r.deleted);

        // Fast & atomic path: replacing an existing regular file with a new regular file!
        if let (Some(old_rec), EntryKind::File, Some(tmp), Some(claim), Some(b_entry)) = (
            existing_live.filter(|r| r.kind == EntryKind::File),
            target.kind,
            staged_tmp.as_deref(),
            claim_tmp.as_deref(),
            bin_entry.as_mut(),
        ) {
            let old_hash = old_rec.hash.ok_or_else(|| SyncError::CorruptData {
                context: format!("local record for {path}"),
                detail: "live file missing BLAKE3 hash".to_string(),
            })?;
            match safe_replace_live_file(
                fs,
                store,
                tmp,
                claim,
                &path,
                old_hash,
                old_rec.size,
                b_entry,
                &mut target,
                staged_meta.as_ref(),
                now,
            )? {
                SafeReplaceResult::Replaced { binned } => {
                    let bins = if binned {
                        vec![b_entry.clone()]
                    } else {
                        Vec::new()
                    };
                    commit_records_and_clear_pending(
                        fs,
                        store,
                        vec![target],
                        bins,
                        vec![(peer.member_id().to_string(), path.clone())],
                        Vec::new(),
                    )?;
                    return Ok(ApplyOutcome::Applied);
                }
                SafeReplaceResult::LiveChangedAfterDisplace { binned } => {
                    let bins = if binned {
                        vec![b_entry.clone()]
                    } else {
                        Vec::new()
                    };
                    mark_needs_rescan_and_clear_pending(store, &[&path], bins)?;
                    return handle_local_changed_after_displace(fs, store, &path, now);
                }
                SafeReplaceResult::RaceBeforePlace => {
                    // `tmp` was never exchanged and still holds our staged download.
                    let _ = fs.remove_tmp(tmp);
                    clear_pending_op(store)?;
                    return handle_local_changed(fs, store, &path, now);
                }
                SafeReplaceResult::Deferred { reason, binned } => {
                    if binned {
                        mark_needs_rescan_and_clear_pending(
                            store,
                            &[&path],
                            vec![b_entry.clone()],
                        )?;
                    } else {
                        clear_pending_op(store)?;
                    }
                    return Ok(ApplyOutcome::Deferred { reason });
                }
            }
        }

        // General path: either creating a brand-new entry or changing entry kinds (File<->Dir<->Symlink).
        let mut persisted_bins = Vec::new();
        if let Some(old_rec) = existing_live {
            match old_rec.kind {
                EntryKind::File => {
                    if let (Some(claim), Some(b_entry), Some(old_hash)) =
                        (claim_tmp.as_deref(), bin_entry.as_mut(), old_rec.hash)
                    {
                        match safe_delete_live_file(
                            fs,
                            store,
                            &path,
                            claim,
                            old_hash,
                            old_rec.size,
                            b_entry,
                            now,
                        )? {
                            SafeDeleteResult::Deleted { binned } => {
                                if binned {
                                    persisted_bins.push(b_entry.clone());
                                }
                            }
                            SafeDeleteResult::RaceChanged => {
                                if let Some(ref tmp) = staged_tmp {
                                    let _ = fs.remove_tmp(tmp);
                                }
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &path, now);
                            }
                            SafeDeleteResult::Deferred(reason) => {
                                if let Some(ref tmp) = staged_tmp {
                                    let _ = fs.remove_tmp(tmp);
                                }
                                clear_pending_op(store)?;
                                return Ok(ApplyOutcome::Deferred { reason });
                            }
                        }
                    }
                }
                EntryKind::Symlink => match fs.remove_symlink_live(&path) {
                    Ok(()) => {
                        if let Some(ref b_entry) = bin_entry {
                            write_bin_meta_file(fs, b_entry)?;
                            persisted_bins.push(b_entry.clone());
                        }
                    }
                    Err(err) if is_mid_apply_race_error(&err) => {
                        if let Some(ref tmp) = staged_tmp {
                            let _ = fs.remove_tmp(tmp);
                        }
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &path, now);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        if let Some(ref tmp) = staged_tmp {
                            let _ = fs.remove_tmp(tmp);
                        }
                        clear_pending_op(store)?;
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&path),
                        });
                    }
                    Err(err) => return Err(err),
                },
                EntryKind::Dir => match fs.remove_empty_dir_live(&path) {
                    Ok(()) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::NotFound,
                        ..
                    }) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::DirectoryNotEmpty,
                        ..
                    })
                    | Err(SyncError::Conflict { .. }) => {
                        if let Some(ref tmp) = staged_tmp {
                            let _ = fs.remove_tmp(tmp);
                        }
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &path, now);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        if let Some(ref tmp) = staged_tmp {
                            let _ = fs.remove_tmp(tmp);
                        }
                        clear_pending_op(store)?;
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&path),
                        });
                    }
                    Err(err) => return Err(err),
                },
            }
        }

        // Place new entry using non-replacing primitives!
        let displaced_existing = existing_live.is_some();
        let on_place_race = |store: &mut S, bins: Vec<BinEntry>| -> Result<ApplyOutcome> {
            mark_needs_rescan_and_clear_pending(store, &[&path], bins)?;
            if displaced_existing {
                handle_local_changed_after_displace(fs, store, &path, now)
            } else {
                handle_local_changed(fs, store, &path, now)
            }
        };
        match target.kind {
            EntryKind::File => {
                if let Some(ref tmp) = staged_tmp {
                    match fs.commit_tmp_to_live_no_replace(tmp, &path) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            let _ = fs.remove_tmp(tmp);
                            return on_place_race(store, persisted_bins);
                        }
                        Err(err) if is_live_busy_error(fs, &err) => {
                            let _ = fs.remove_tmp(tmp);
                            if persisted_bins.is_empty() {
                                clear_pending_op(store)?;
                            } else {
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[&path],
                                    persisted_bins,
                                )?;
                            }
                            return Ok(ApplyOutcome::Deferred {
                                reason: busy_defer_reason(&path),
                            });
                        }
                        Err(err) => return Err(err),
                    }
                }
                if !verify_and_record_placed_stat(
                    fs,
                    &path,
                    &mut target,
                    staged_meta.as_ref(),
                    true,
                    now,
                )? {
                    return on_place_race(store, persisted_bins);
                }
            }
            EntryKind::Dir => {
                match fs.create_dir_all_live(&path) {
                    Ok(()) => {}
                    Err(err) if is_mid_apply_race_error(&err) => {
                        return on_place_race(store, persisted_bins);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        if persisted_bins.is_empty() {
                            clear_pending_op(store)?;
                        } else {
                            mark_needs_rescan_and_clear_pending(store, &[&path], persisted_bins)?;
                        }
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&path),
                        });
                    }
                    Err(err) => return Err(err),
                }
                record_disk_stat(fs, &path, &mut target)?;
            }
            EntryKind::Symlink => {
                let link_target =
                    target
                        .link_target
                        .as_deref()
                        .ok_or_else(|| SyncError::CorruptData {
                            context: format!("remote symlink record for {path}"),
                            detail: "symlink record missing link_target".to_string(),
                        })?;
                match fs.create_symlink_live(&path, link_target) {
                    Ok(()) => {}
                    Err(err) if is_mid_apply_race_error(&err) => {
                        return on_place_race(store, persisted_bins);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        if persisted_bins.is_empty() {
                            clear_pending_op(store)?;
                        } else {
                            mark_needs_rescan_and_clear_pending(store, &[&path], persisted_bins)?;
                        }
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&path),
                        });
                    }
                    Err(err) => return Err(err),
                }
                record_disk_stat(fs, &path, &mut target)?;
            }
        }

        commit_records_and_clear_pending(
            fs,
            store,
            vec![target],
            persisted_bins,
            vec![(peer.member_id().to_string(), path.clone())],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    let res = recover_on_non_crash_error(fs, store, res, now);
    map_local_placement_result(fs, &path, target_size, peer.member_id(), res)
}

fn apply_delete<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer_member: &str,
    tombstone: IndexRecord,
    bin_reason: BinReason,
    ignore: &IgnoreMatcher,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let path = tombstone.path.clone();
    let existing = store.get_record(&path)?;

    let use_stat_precheck = existing
        .as_ref()
        .is_some_and(|r| !r.deleted && r.kind == EntryKind::File);
    let precheck_res = if use_stat_precheck {
        disk_stat_matches_index(fs, &path, existing.as_ref())
    } else {
        disk_matches_index(fs, &path, existing.as_ref())
    };
    match precheck_res {
        Ok(true) => {}
        Ok(false) => return handle_local_changed(fs, store, &path, now),
        Err(err) if is_live_busy_error(fs, &err) => {
            return Ok(ApplyOutcome::Deferred {
                reason: busy_defer_reason(&path),
            });
        }
        Err(err) => return map_local_placement_result(fs, &path, 0, peer_member, Err(err)),
    }

    // Item H: inspect directories before deleting!
    if let Some(ref loc) = existing
        && !loc.deleted
        && loc.kind == EntryKind::Dir
    {
        if store.has_live_descendant(&path)? {
            return Ok(ApplyOutcome::LocalChangedRetry);
        }
        let children = match fs.list_dir_live(&path) {
            Ok(c) => c,
            Err(err) if is_mid_apply_race_error(&err) => {
                return handle_local_changed(fs, store, &path, now);
            }
            Err(err) if is_live_busy_error(fs, &err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: busy_defer_reason(&path),
                });
            }
            Err(err) => return Err(err),
        };
        if !children.is_empty() {
            let mut litter_files: Vec<(String, FsMeta)> = Vec::new();
            let mut has_local_only = false;
            let mut unscanned_synced = Vec::new();

            for name in children {
                let child_rel = format!("{path}/{name}");
                let child_meta = fs.stat_live(&child_rel)?;
                let is_dir = child_meta
                    .as_ref()
                    .is_some_and(|m| m.kind == EntryKind::Dir);
                let is_unsupported_symlink = child_meta.as_ref().is_some_and(|m| {
                    m.kind == EntryKind::Symlink
                        && check_symlink_target_contained(
                            &child_rel,
                            m.link_target.as_deref().unwrap_or_default(),
                        )
                        .is_err()
                });

                if is_os_litter_name(&name)
                    && let Some(cm) = child_meta.filter(|m| m.kind == EntryKind::File)
                {
                    litter_files.push((child_rel, cm));
                } else if ignore.is_custom_ignored(&child_rel, &name, is_dir)
                    || is_legion_inflight_name(&name)
                    || is_unsupported_symlink
                    || (is_dir && dir_holds_only_local_non_synced_files(fs, &child_rel, ignore)?)
                {
                    has_local_only = true;
                } else {
                    unscanned_synced.push(child_rel);
                }
            }

            if !unscanned_synced.is_empty() {
                // Rescan unscanned synced children so we never lose user data!
                for child_path in unscanned_synced {
                    let _ = rescan_single_path(fs, store, &child_path, now)?;
                }
                return Ok(ApplyOutcome::LocalChangedRetry);
            }

            if has_local_only {
                // Keep directory on disk, commit tombstone, remember in `kept_deleted_dirs` so
                // rescans never revive it even after tombstone GC or restarts, report once, and
                // advance the cursor!
                let mut meta = load_meta_required(store)?;
                meta.kept_deleted_dirs.insert(path.clone());
                let seq = meta.next_seq();
                let mut tomb = tombstone;
                tomb.seq = seq;
                let chain = meta.advance_chain_fingerprint(seq, tomb.head_fingerprint());
                let encoded =
                    encode_deletion_log_entries(&[FolderDeletionLogEntry::from_tombstone(&tomb)])?;
                fs.append_deletion_log(&encoded)?;
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    upsert_records: vec![tomb],
                    remove_held_back: vec![(peer_member.to_string(), path.clone())],
                    upsert_seq_fingerprints: vec![(seq, chain)],
                    set_pending_op: Some(None),
                    ..StoreBatch::default()
                };
                store.apply_batch(batch)?;
                return Ok(ApplyOutcome::DirKeptLocalFiles(SyncProblem {
                    path: path.clone(),
                    kind: ProblemKind::DirectoryKeptLocalFiles,
                    peer: Some(peer_member.to_string()),
                    sentence: format!(
                        "'{path}' was kept here because it still holds files that are not synced."
                    ),
                }));
            }

            // Directory holds ONLY fixed OS litter (`.DS_Store`, `Thumbs.db`, `desktop.ini`):
            // move each litter file to the local bin and remove the directory!
            let mut litter_bins = Vec::new();
            for (child_rel, cm) in litter_files {
                let (h, sz) = match fs.hash_live(&child_rel) {
                    Ok(pair) => pair,
                    Err(err) if is_mid_apply_race_error(&err) => {
                        if !litter_bins.is_empty() {
                            let _ = store.apply_batch(StoreBatch {
                                upsert_bin: litter_bins,
                                ..StoreBatch::default()
                            });
                        }
                        return Ok(ApplyOutcome::LocalChangedRetry);
                    }
                    Err(err) => return Err(err),
                };
                let entry = build_raw_bin_entry(
                    &child_rel,
                    EntryKind::File,
                    sz,
                    cm.mtime,
                    cm.mode,
                    Some(h),
                    None,
                    BinReason::OsLitterInDeletedDir,
                    peer_member,
                    now,
                );
                write_bin_meta_file(fs, &entry)?;
                match fs.move_live_to_bin_no_replace(&child_rel, &entry.storage_name) {
                    Ok(()) => litter_bins.push(entry),
                    Err(err) if is_mid_apply_race_error(&err) => {
                        let _ = fs.remove_bin_file(&bin_meta_name(&entry.id));
                        if !litter_bins.is_empty() {
                            let _ = store.apply_batch(StoreBatch {
                                upsert_bin: litter_bins,
                                ..StoreBatch::default()
                            });
                        }
                        return Ok(ApplyOutcome::LocalChangedRetry);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        let _ = fs.remove_bin_file(&bin_meta_name(&entry.id));
                        if !litter_bins.is_empty() {
                            let _ = store.apply_batch(StoreBatch {
                                upsert_bin: litter_bins,
                                ..StoreBatch::default()
                            });
                        }
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&child_rel),
                        });
                    }
                    Err(err) => return Err(err),
                }
            }

            match fs.remove_empty_dir_live(&path) {
                Ok(()) => {}
                Err(SyncError::Fs {
                    kind: FsErrorKind::NotFound,
                    ..
                }) => {}
                Err(SyncError::Fs {
                    kind: FsErrorKind::DirectoryNotEmpty,
                    ..
                })
                | Err(SyncError::Conflict { .. }) => {
                    // Another file appeared concurrently; save litter bin records and retry.
                    if !litter_bins.is_empty() {
                        let batch = StoreBatch {
                            upsert_bin: litter_bins,
                            ..StoreBatch::default()
                        };
                        store.apply_batch(batch)?;
                    }
                    return Ok(ApplyOutcome::LocalChangedRetry);
                }
                Err(err) if is_live_busy_error(fs, &err) => {
                    if !litter_bins.is_empty() {
                        let _ = store.apply_batch(StoreBatch {
                            upsert_bin: litter_bins,
                            ..StoreBatch::default()
                        });
                    }
                    return Ok(ApplyOutcome::Deferred {
                        reason: busy_defer_reason(&path),
                    });
                }
                Err(err) => return Err(err),
            }

            commit_records_and_clear_pending(
                fs,
                store,
                vec![tombstone],
                litter_bins,
                vec![(peer_member.to_string(), path)],
                Vec::new(),
            )?;
            return Ok(ApplyOutcome::Applied);
        }
    }

    let expected_local = snapshot_local(existing.as_ref());
    let mut bin_entry = build_bin_entry(existing.as_ref(), bin_reason, peer_member, now);
    let claim_tmp = if existing
        .as_ref()
        .is_some_and(|r| !r.deleted && r.kind == EntryKind::File)
    {
        Some(alloc_tmp_name_for(fs, "delclaim", &path)?)
    } else {
        None
    };

    let pending = PendingOp::Delete {
        target_record: tombstone.clone(),
        claim_tmp: claim_tmp.clone(),
        bin_entry: bin_entry.clone(),
        expected_local,
    };
    write_pending_op(store, pending)?;

    let res: Result<ApplyOutcome> = (|| {
        let mut persisted_bins = Vec::new();
        if let Some(ref loc) = existing
            && !loc.deleted
        {
            match loc.kind {
                EntryKind::File => {
                    if let (Some(claim), Some(b_entry), Some(old_hash)) =
                        (claim_tmp.as_deref(), bin_entry.as_mut(), loc.hash)
                    {
                        match safe_delete_live_file(
                            fs, store, &path, claim, old_hash, loc.size, b_entry, now,
                        )? {
                            SafeDeleteResult::Deleted { binned } => {
                                if binned {
                                    persisted_bins.push(b_entry.clone());
                                }
                            }
                            SafeDeleteResult::RaceChanged => {
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &path, now);
                            }
                            SafeDeleteResult::Deferred(reason) => {
                                clear_pending_op(store)?;
                                return Ok(ApplyOutcome::Deferred { reason });
                            }
                        }
                    }
                }
                EntryKind::Symlink => match fs.remove_symlink_live(&path) {
                    Ok(()) => {
                        if let Some(ref b_entry) = bin_entry {
                            write_bin_meta_file(fs, b_entry)?;
                            persisted_bins.push(b_entry.clone());
                        }
                    }
                    Err(err) if is_mid_apply_race_error(&err) => {
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &path, now);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        clear_pending_op(store)?;
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&path),
                        });
                    }
                    Err(err) => return Err(err),
                },
                EntryKind::Dir => match fs.remove_empty_dir_live(&path) {
                    Ok(()) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::NotFound,
                        ..
                    }) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::DirectoryNotEmpty,
                        ..
                    })
                    | Err(SyncError::Conflict { .. }) => {
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &path, now);
                    }
                    Err(err) if is_live_busy_error(fs, &err) => {
                        clear_pending_op(store)?;
                        return Ok(ApplyOutcome::Deferred {
                            reason: busy_defer_reason(&path),
                        });
                    }
                    Err(err) => return Err(err),
                },
            }
        }

        commit_records_and_clear_pending(
            fs,
            store,
            vec![tombstone],
            persisted_bins,
            vec![(peer_member.to_string(), path.clone())],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    let res = recover_on_non_crash_error(fs, store, res, now);
    map_local_placement_result(fs, &path, 0, peer_member, res)
}

#[allow(clippy::too_many_arguments)]
fn apply_conflict_local_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    mut winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    fetch_opts: &FetchOptions,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let orig_path = winner_record.path.clone();
    let conflict_path = loser_record.path.clone();
    let winner_size = winner_record.size;

    // 1. Pre-check free space and fetch remote winner into `.legion-sync/tmp/` (or beside the
    // destination on a separate mount) and prepare metadata before placement.
    let (winner_tmp, winner_staged_meta) = if winner_record.kind == EntryKind::File {
        if let Some(prob) =
            check_free_space_for_file(fs, &orig_path, winner_record.size, peer.member_id())?
        {
            return Ok(ApplyOutcome::HoldBackLocal(prob));
        }
        let expected_hash = winner_record.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote winner for {orig_path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(
            fs,
            store,
            peer,
            &orig_path,
            &orig_path,
            winner_record.size,
            expected_hash,
            fetch_opts,
        ) {
            Ok(tmp) => {
                match prepare_staged_tmp_metadata(fs, &tmp, winner_record.mtime, winner_record.mode)
                {
                    Ok(sm) => (Some(tmp), sm),
                    Err(err) => {
                        if !err.is_crash() {
                            let _ = cleanup_staged_tmps(fs, store, &[Some(tmp.as_str())]);
                        }
                        return map_local_placement_result(
                            fs,
                            &orig_path,
                            winner_size,
                            peer.member_id(),
                            Err(err),
                        );
                    }
                }
            }
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: format!("remote winner changed while fetching: {err}"),
                });
            }
            Err(err) => {
                return map_local_placement_result(
                    fs,
                    &orig_path,
                    winner_size,
                    peer.member_id(),
                    Err(err),
                );
            }
        }
    } else {
        (None, None)
    };

    // 2. Re-check local disk at `orig_path` did not change since scan.
    let existing = store.get_record(&orig_path)?;
    match disk_matches_index(fs, &orig_path, existing.as_ref()) {
        Ok(true) => {}
        Ok(false) => {
            cleanup_staged_tmps(fs, store, &[winner_tmp.as_deref()])?;
            return handle_local_changed(fs, store, &orig_path, now);
        }
        Err(err) if is_live_busy_error(fs, &err) => {
            cleanup_staged_tmps(fs, store, &[winner_tmp.as_deref()])?;
            return Ok(ApplyOutcome::Deferred {
                reason: busy_defer_reason(&orig_path),
            });
        }
        Err(err) => {
            if !err.is_crash() {
                let _ = cleanup_staged_tmps(fs, store, &[winner_tmp.as_deref()]);
            }
            return Err(err);
        }
    }

    // 3. Assign a fresh independent version vector to the conflict copy (Item A).
    finalize_independent_conflict_version(store, &mut loser_record)?;

    let conflict_already_present = disk_matches_record(fs, &conflict_path, &loser_record)?;
    if !conflict_already_present && fs.stat_live(&conflict_path)?.is_some() {
        // An unscanned or different file appeared at `conflict_path`! Never overwrite it.
        cleanup_staged_tmps(fs, store, &[winner_tmp.as_deref()])?;
        return handle_local_changed(fs, store, &conflict_path, now);
    }
    if conflict_already_present {
        // Capture `conflict_path`'s disk stat right after verifying its hash so a concurrent edit
        // to `conflict_path` during Steps 5-6 is never masked on the next scan.
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
    }

    let expected_local = snapshot_local(existing.as_ref());

    // 4. Write-ahead intent before touching working tree.
    let pending = PendingOp::ConflictLocalLoses {
        winner_record: winner_record.clone(),
        loser_record: loser_record.clone(),
        winner_tmp: winner_tmp.clone(),
        conflict_already_present,
        expected_local,
    };
    write_pending_op_and_unregister_tmps(store, pending, winner_tmp.iter().cloned().collect())?;

    let res: Result<ApplyOutcome> = (|| {
        let mut persisted_bins = Vec::new();
        // 5. Move local loser to `conflict_path` FIRST via non-replacing rename!
        if !conflict_already_present {
            match loser_record.kind {
                EntryKind::File => {
                    match fs.rename_live_to_live_no_replace(&orig_path, &conflict_path) {
                        Ok(()) => {}
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &conflict_path, now);
                        }
                        Err(err) if is_mid_apply_race_error(&err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&orig_path, &conflict_path],
                                Vec::new(),
                            )?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                        Err(err) if is_live_busy_error(fs, &err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&orig_path, &conflict_path],
                                Vec::new(),
                            )?;
                            return Ok(ApplyOutcome::Deferred {
                                reason: busy_defer_reason(&orig_path),
                            });
                        }
                        Err(err) => return Err(err),
                    }
                    // Verify the file we just moved into `conflict_path` still matches the
                    // expected local bytes (if a person edited `orig_path` in the race window,
                    // put it back without ignoring errors).
                    if let Some(expected_hash) = loser_record.hash {
                        let (actual_hash, actual_size) = match fs.hash_live(&conflict_path) {
                            Ok(pair) => pair,
                            Err(err) if is_mid_apply_race_error(&err) => {
                                if let Some(ref wt) = winner_tmp {
                                    let _ = fs.remove_tmp(wt);
                                }
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[&orig_path, &conflict_path],
                                    Vec::new(),
                                )?;
                                let _ = rescan_single_path(fs, store, &conflict_path, now)?;
                                return handle_local_changed(fs, store, &orig_path, now);
                            }
                            Err(err) => return Err(err),
                        };
                        if actual_hash != expected_hash || actual_size != loser_record.size {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            match fs.rename_live_to_live_no_replace(&conflict_path, &orig_path) {
                                Ok(()) => {
                                    clear_pending_op(store)?;
                                    return handle_local_changed(fs, store, &orig_path, now);
                                }
                                Err(err) if is_mid_apply_race_error(&err) => {
                                    mark_needs_rescan_and_clear_pending(
                                        store,
                                        &[&orig_path, &conflict_path],
                                        Vec::new(),
                                    )?;
                                    let _ = rescan_single_path(fs, store, &conflict_path, now)?;
                                    return handle_local_changed(fs, store, &orig_path, now);
                                }
                                Err(err) => return Err(err),
                            }
                        }
                    }
                    apply_live_metadata(fs, &conflict_path, loser_record.mtime, loser_record.mode)?;
                }
                EntryKind::Symlink => {
                    if let Some(ref target) = loser_record.link_target {
                        match fs.create_symlink_live(&conflict_path, target) {
                            Ok(()) => {}
                            Err(err) if is_mid_apply_race_error(&err) => {
                                if let Some(ref wt) = winner_tmp {
                                    let _ = fs.remove_tmp(wt);
                                }
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &conflict_path, now);
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    match fs.remove_symlink_live(&orig_path) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&orig_path, &conflict_path],
                                Vec::new(),
                            )?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                        Err(err) if is_live_busy_error(fs, &err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&orig_path, &conflict_path],
                                Vec::new(),
                            )?;
                            return Ok(ApplyOutcome::Deferred {
                                reason: busy_defer_reason(&orig_path),
                            });
                        }
                        Err(err) => return Err(err),
                    }
                }
                EntryKind::Dir => {
                    match fs.create_dir_all_live(&conflict_path) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &conflict_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                    let _ = fs.remove_empty_dir_live(&orig_path);
                }
            }
            // Capture `conflict_path`'s disk stat immediately after placing and verifying it,
            // before Step 6 runs, so any concurrent edit to `conflict_path` during Step 6 is
            // detected on the next scan.
            record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        } else {
            // Identical conflict copy already exists; safely move old orig to bin (never unlink
            // bytes moved out of the person's tree!).
            match loser_record.kind {
                EntryKind::Symlink => match fs.remove_symlink_live(&orig_path) {
                    Ok(()) => {}
                    Err(err) if is_mid_apply_race_error(&err) => {
                        if let Some(ref wt) = winner_tmp {
                            let _ = fs.remove_tmp(wt);
                        }
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &orig_path, now);
                    }
                    Err(err) => return Err(err),
                },
                EntryKind::Dir => {
                    let _ = fs.remove_empty_dir_live(&orig_path);
                }
                EntryKind::File => {
                    let claim = alloc_tmp_name_for(fs, "confclaim", &orig_path)?;
                    match fs.move_live_to_tmp_no_replace(&orig_path, &claim) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                        Err(err) if is_live_busy_error(fs, &err) => {
                            if let Some(ref wt) = winner_tmp {
                                let _ = fs.remove_tmp(wt);
                            }
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&orig_path, &conflict_path],
                                Vec::new(),
                            )?;
                            return Ok(ApplyOutcome::Deferred {
                                reason: busy_defer_reason(&orig_path),
                            });
                        }
                        Err(err) => return Err(err),
                    }
                    let expected_old = loser_record.hash;
                    let (matched, precomputed) =
                        check_tmp_hash(fs, &claim, expected_old, loser_record.size)?;
                    if !matched {
                        // Human edited orig_path in race window! Put back or preserve as conflict copy.
                        put_back_or_preserve_tmp(fs, store, &claim, &orig_path, precomputed, now)?;
                        if let Some(ref wt) = winner_tmp {
                            let _ = fs.remove_tmp(wt);
                        }
                        mark_needs_rescan_and_clear_pending(store, &[&orig_path], Vec::new())?;
                        return handle_local_changed(fs, store, &orig_path, now);
                    }
                    if let Some(mut b_entry) = build_bin_entry(
                        existing.as_ref(),
                        BinReason::ReplacedByPeer,
                        peer.member_id(),
                        now,
                    ) {
                        if finalize_binned_tmp_and_rescue(
                            fs,
                            store,
                            &claim,
                            &orig_path,
                            &mut b_entry,
                            now,
                        )? {
                            persisted_bins.push(b_entry);
                        }
                    } else {
                        preserve_tmp_as_conflict_copy_with_hash(
                            fs,
                            store,
                            &claim,
                            &orig_path,
                            precomputed,
                            now,
                        )?;
                    }
                }
            }
        }

        let commit_loser_if_still_on_disk = |store: &mut S,
                                             mut loser: IndexRecord,
                                             bins: Vec<BinEntry>,
                                             peer_id: &str|
         -> Result<()> {
            if disk_matches_record(fs, &conflict_path, &loser)? {
                record_disk_stat(fs, &conflict_path, &mut loser)?;
                commit_records_and_clear_pending(
                    fs,
                    store,
                    vec![loser],
                    bins,
                    vec![(peer_id.to_string(), conflict_path.clone())],
                    Vec::new(),
                )?;
                mark_needs_rescan(store, &[&orig_path])?;
            } else {
                // A person moved, deleted, or modified `conflict_path` before Step 6 ran; do
                // NOT commit a phantom live record for `conflict_path` (which would cause a
                // phantom tombstone on the next scan).
                mark_needs_rescan_and_clear_pending(store, &[&orig_path, &conflict_path], bins)?;
                let _ = rescan_single_path(fs, store, &conflict_path, now)?;
            }
            Ok(())
        };

        // 6. Place remote winner at `orig_path` using non-replacing rename!
        match winner_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = winner_tmp {
                    match fs.commit_tmp_to_live_no_replace(tmp, &orig_path) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            let _ = fs.remove_tmp(tmp);
                            commit_loser_if_still_on_disk(
                                store,
                                loser_record,
                                persisted_bins,
                                peer.member_id(),
                            )?;
                            return handle_local_changed_after_displace(fs, store, &orig_path, now);
                        }
                        Err(err) if is_live_busy_error(fs, &err) => {
                            let _ = fs.remove_tmp(tmp);
                            commit_loser_if_still_on_disk(
                                store,
                                loser_record,
                                persisted_bins,
                                peer.member_id(),
                            )?;
                            return Ok(ApplyOutcome::Deferred {
                                reason: busy_defer_reason(&orig_path),
                            });
                        }
                        Err(err) => return Err(err),
                    }
                }
                if !verify_and_record_placed_stat(
                    fs,
                    &orig_path,
                    &mut winner_record,
                    winner_staged_meta.as_ref(),
                    true,
                    now,
                )? {
                    commit_loser_if_still_on_disk(
                        store,
                        loser_record,
                        persisted_bins,
                        peer.member_id(),
                    )?;
                    return handle_local_changed_after_displace(fs, store, &orig_path, now);
                }
            }
            EntryKind::Dir => {
                match fs.create_dir_all_live(&orig_path) {
                    Ok(()) => {}
                    Err(err) if is_mid_apply_race_error(&err) => {
                        commit_loser_if_still_on_disk(
                            store,
                            loser_record,
                            persisted_bins,
                            peer.member_id(),
                        )?;
                        return handle_local_changed_after_displace(fs, store, &orig_path, now);
                    }
                    Err(err) => return Err(err),
                }
                record_disk_stat(fs, &orig_path, &mut winner_record)?;
            }
            EntryKind::Symlink => {
                let link_target =
                    winner_record
                        .link_target
                        .as_deref()
                        .ok_or_else(|| SyncError::CorruptData {
                            context: format!("remote winner symlink for {orig_path}"),
                            detail: "symlink record missing link_target".to_string(),
                        })?;
                match fs.create_symlink_live(&orig_path, link_target) {
                    Ok(()) => {}
                    Err(err) if is_mid_apply_race_error(&err) => {
                        commit_loser_if_still_on_disk(
                            store,
                            loser_record,
                            persisted_bins,
                            peer.member_id(),
                        )?;
                        return handle_local_changed_after_displace(fs, store, &orig_path, now);
                    }
                    Err(err) => return Err(err),
                }
                record_disk_stat(fs, &orig_path, &mut winner_record)?;
            }
        }

        // 7. Commit records atomically and clear pending op, verifying `conflict_path` is still
        // intact on disk before committing `loser_record` so a concurrent move never becomes a
        // phantom live record + phantom deletion.
        if disk_matches_record(fs, &conflict_path, &loser_record)? {
            record_disk_stat(fs, &conflict_path, &mut loser_record)?;
            commit_records_and_clear_pending(
                fs,
                store,
                vec![loser_record, winner_record],
                persisted_bins,
                vec![
                    (peer.member_id().to_string(), orig_path.clone()),
                    (peer.member_id().to_string(), conflict_path.clone()),
                ],
                Vec::new(),
            )?;
        } else {
            commit_records_and_clear_pending(
                fs,
                store,
                vec![winner_record],
                persisted_bins,
                vec![(peer.member_id().to_string(), orig_path.clone())],
                Vec::new(),
            )?;
            mark_needs_rescan(store, &[&conflict_path])?;
        }
        Ok(ApplyOutcome::Applied)
    })();

    let res = recover_on_non_crash_error(fs, store, res, now);
    map_local_placement_result(fs, &orig_path, winner_size, peer.member_id(), res)
}

#[allow(clippy::too_many_arguments)]
fn apply_conflict_remote_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    mut winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    bump_winner_clock: bool,
    fetch_opts: &FetchOptions,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let orig_path = winner_record.path.clone();
    let conflict_path = loser_record.path.clone();
    let loser_size = loser_record.size;

    // 1. Pre-check free space and fetch remote loser into `.legion-sync/tmp/` (or beside the
    // conflict path on a separate mount) and set metadata before placement.
    let (loser_tmp, loser_staged_meta) = if loser_record.kind == EntryKind::File {
        if let Some(prob) =
            check_free_space_for_file(fs, &conflict_path, loser_record.size, peer.member_id())?
        {
            return Ok(ApplyOutcome::HoldBackLocal(prob));
        }
        let expected_hash = loser_record.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote loser for {orig_path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(
            fs,
            store,
            peer,
            &orig_path,
            &conflict_path,
            loser_record.size,
            expected_hash,
            fetch_opts,
        ) {
            Ok(tmp) => {
                match prepare_staged_tmp_metadata(fs, &tmp, loser_record.mtime, loser_record.mode) {
                    Ok(sm) => (Some(tmp), sm),
                    Err(err) => {
                        if !err.is_crash() {
                            let _ = cleanup_staged_tmps(fs, store, &[Some(tmp.as_str())]);
                        }
                        return map_local_placement_result(
                            fs,
                            &conflict_path,
                            loser_size,
                            peer.member_id(),
                            Err(err),
                        );
                    }
                }
            }
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: format!("remote loser changed while fetching: {err}"),
                });
            }
            Err(err) => {
                return map_local_placement_result(
                    fs,
                    &conflict_path,
                    loser_size,
                    peer.member_id(),
                    Err(err),
                );
            }
        }
    } else {
        (None, None)
    };

    // 2. Re-check local winner at `orig_path` did not change since scan.
    let existing = store.get_record(&orig_path)?;
    match disk_matches_index(fs, &orig_path, existing.as_ref()) {
        Ok(true) => {}
        Ok(false) => {
            cleanup_staged_tmps(fs, store, &[loser_tmp.as_deref()])?;
            return handle_local_changed(fs, store, &orig_path, now);
        }
        Err(err) if is_live_busy_error(fs, &err) => {
            cleanup_staged_tmps(fs, store, &[loser_tmp.as_deref()])?;
            return Ok(ApplyOutcome::Deferred {
                reason: busy_defer_reason(&orig_path),
            });
        }
        Err(err) => {
            if !err.is_crash() {
                let _ = cleanup_staged_tmps(fs, store, &[loser_tmp.as_deref()]);
            }
            return Err(err);
        }
    }

    if bump_winner_clock {
        let mut meta = load_meta_required(store)?;
        meta.bump_version(&mut winner_record.version);
        let batch = StoreBatch {
            folder_meta: Some(meta),
            ..StoreBatch::default()
        };
        store.apply_batch(batch)?;
    }

    // 3. Assign a fresh independent version vector to the conflict copy (Item A).
    finalize_independent_conflict_version(store, &mut loser_record)?;

    let conflict_already_present = disk_matches_record(fs, &conflict_path, &loser_record)?;
    if !conflict_already_present && fs.stat_live(&conflict_path)?.is_some() {
        cleanup_staged_tmps(fs, store, &[loser_tmp.as_deref()])?;
        return handle_local_changed(fs, store, &conflict_path, now);
    }
    if conflict_already_present {
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
    }

    let expected_local = snapshot_local(existing.as_ref());

    let pending = PendingOp::ConflictRemoteLoses {
        winner_record: winner_record.clone(),
        loser_record: loser_record.clone(),
        loser_tmp: loser_tmp.clone(),
        conflict_already_present,
        expected_local,
    };
    write_pending_op_and_unregister_tmps(store, pending, loser_tmp.iter().cloned().collect())?;

    let res: Result<ApplyOutcome> = (|| {
        if !conflict_already_present {
            match loser_record.kind {
                EntryKind::File => {
                    if let Some(ref tmp) = loser_tmp {
                        match fs.commit_tmp_to_live_no_replace(tmp, &conflict_path) {
                            Ok(()) => {}
                            Err(err) if is_mid_apply_race_error(&err) => {
                                let _ = fs.remove_tmp(tmp);
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[&conflict_path],
                                    Vec::new(),
                                )?;
                                return handle_local_changed(fs, store, &conflict_path, now);
                            }
                            Err(err) if is_live_busy_error(fs, &err) => {
                                let _ = fs.remove_tmp(tmp);
                                clear_pending_op(store)?;
                                return Ok(ApplyOutcome::Deferred {
                                    reason: busy_defer_reason(&conflict_path),
                                });
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    if !verify_and_record_placed_stat(
                        fs,
                        &conflict_path,
                        &mut loser_record,
                        loser_staged_meta.as_ref(),
                        true,
                        now,
                    )? {
                        mark_needs_rescan_and_clear_pending(store, &[&conflict_path], Vec::new())?;
                        return handle_local_changed(fs, store, &conflict_path, now);
                    }
                }
                EntryKind::Dir => {
                    match fs.create_dir_all_live(&conflict_path) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            mark_needs_rescan_and_clear_pending(
                                store,
                                &[&conflict_path],
                                Vec::new(),
                            )?;
                            return handle_local_changed(fs, store, &conflict_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                    record_disk_stat(fs, &conflict_path, &mut loser_record)?;
                }
                EntryKind::Symlink => {
                    let link_target = loser_record.link_target.as_deref().ok_or_else(|| {
                        SyncError::CorruptData {
                            context: format!("remote loser symlink for {orig_path}"),
                            detail: "symlink record missing link_target".to_string(),
                        }
                    })?;
                    match fs.create_symlink_live(&conflict_path, link_target) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &conflict_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                    record_disk_stat(fs, &conflict_path, &mut loser_record)?;
                }
            }
        } else if let Some(ref lt) = loser_tmp {
            let _ = fs.remove_tmp(lt);
        }

        commit_records_and_clear_pending(
            fs,
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            vec![
                (peer.member_id().to_string(), orig_path.clone()),
                (peer.member_id().to_string(), conflict_path.clone()),
            ],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    let res = recover_on_non_crash_error(fs, store, res, now);
    map_local_placement_result(fs, &conflict_path, loser_size, peer.member_id(), res)
}

/// Ensures every strict ancestor of `rel_path` exists as a live directory both on disk and in the
/// index.
fn ensure_ancestors_are_dirs<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_path: &str,
    now: Timestamp,
) -> Result<AncestorOutcome> {
    for anc in ancestors(rel_path) {
        let mut existing = store.get_record(anc)?;
        match disk_matches_index(fs, anc, existing.as_ref()) {
            Ok(true) => {}
            Ok(false) => match rescan_single_path(fs, store, anc, now)? {
                SingleRescanOutcome::Unstable => return Ok(AncestorOutcome::Unstable),
                SingleRescanOutcome::Updated(rec) => existing = Some(*rec),
                SingleRescanOutcome::Unchanged => existing = store.get_record(anc)?,
            },
            Err(err) if is_live_busy_error(fs, &err) => {
                return Ok(AncestorOutcome::Busy(busy_defer_reason(anc)));
            }
            Err(err) => return Err(err),
        }

        match existing {
            Some(ref rec) if !rec.deleted && rec.kind == EntryKind::Dir => {
                if fs.stat_live(anc)?.is_none() {
                    match fs.create_dir_all_live(anc) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            let _ = rescan_single_path(fs, store, anc, now)?;
                            return Ok(AncestorOutcome::ChangedRetry);
                        }
                        Err(err) if is_live_busy_error(fs, &err) => {
                            return Ok(AncestorOutcome::Busy(busy_defer_reason(anc)));
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
            Some(rec) if !rec.deleted && rec.kind != EntryKind::Dir => {
                let meta_ro = load_meta_required(store)?;
                let loser_member = meta_ro
                    .canonical_member_name(&rec.modified_by)
                    .unwrap_or(meta_ro.local_member.as_str())
                    .to_string();
                let view = StoreIndexView::new(store);
                let conflict_path = choose_conflict_path(&view, anc, &rec, &loser_member);
                let mut loser = rec.clone();
                loser.path = conflict_path.clone();
                loser.version = VersionVector::new();
                loser.recent_hashes.clear();
                loser.from_join = false;
                loser.conflict_dismissed = false;
                loser.conflict_of =
                    derive_conflict_of(&loser, &meta_ro).or_else(|| Some(anc.to_string()));
                finalize_independent_conflict_version(store, &mut loser)?;

                let mut meta = load_meta_required(store)?;
                let mut dir_version = rec.version.clone();
                meta.bump_version(&mut dir_version);
                let mut dir_record = IndexRecord {
                    path: anc.to_string(),
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime: now,
                    disk_mtime: Some(now),
                    disk_change_id: None,
                    mode: None,
                    hash: None,
                    link_target: None,
                    deleted: false,
                    version: dir_version,
                    modified_by: meta.local_member.clone(),
                    seq: 0,
                    recent_hashes: Vec::new(),
                    from_join: false,
                    conflict_of: None,
                    conflict_dismissed: false,
                };
                let meta_batch = StoreBatch {
                    folder_meta: Some(meta),
                    ..StoreBatch::default()
                };
                store.apply_batch(meta_batch)?;

                let already = disk_matches_record(fs, &conflict_path, &loser)?;
                if already {
                    record_disk_stat(fs, &conflict_path, &mut loser)?;
                }
                let pending = PendingOp::ConflictLocalLoses {
                    winner_record: dir_record.clone(),
                    loser_record: loser.clone(),
                    winner_tmp: None,
                    conflict_already_present: already,
                    expected_local: snapshot_local(Some(&rec)),
                };
                write_pending_op(store, pending)?;

                let anc_res: Result<AncestorOutcome> = (|| {
                    let mut persisted_bins = Vec::new();
                    if !already {
                        match loser.kind {
                            EntryKind::File => {
                                match fs.rename_live_to_live_no_replace(anc, &conflict_path) {
                                    Ok(()) => {}
                                    Err(err) if is_mid_apply_race_error(&err) => {
                                        clear_pending_op(store)?;
                                        let _ = rescan_single_path(fs, store, anc, now)?;
                                        return Ok(AncestorOutcome::ChangedRetry);
                                    }
                                    Err(err) if is_live_busy_error(fs, &err) => {
                                        clear_pending_op(store)?;
                                        return Ok(AncestorOutcome::Busy(busy_defer_reason(anc)));
                                    }
                                    Err(err) => return Err(err),
                                }
                                if let Some(expected_hash) = loser.hash {
                                    let (actual_hash, actual_size) = match fs
                                        .hash_live(&conflict_path)
                                    {
                                        Ok(pair) => pair,
                                        Err(err) if is_mid_apply_race_error(&err) => {
                                            mark_needs_rescan_and_clear_pending(
                                                store,
                                                &[anc, &conflict_path],
                                                Vec::new(),
                                            )?;
                                            let _ =
                                                rescan_single_path(fs, store, &conflict_path, now)?;
                                            let _ = rescan_single_path(fs, store, anc, now)?;
                                            return Ok(AncestorOutcome::ChangedRetry);
                                        }
                                        Err(err) => return Err(err),
                                    };
                                    if actual_hash != expected_hash || actual_size != loser.size {
                                        match fs.rename_live_to_live_no_replace(&conflict_path, anc)
                                        {
                                            Ok(()) => {
                                                clear_pending_op(store)?;
                                                let _ = rescan_single_path(fs, store, anc, now)?;
                                                return Ok(AncestorOutcome::ChangedRetry);
                                            }
                                            Err(err) if is_mid_apply_race_error(&err) => {
                                                mark_needs_rescan_and_clear_pending(
                                                    store,
                                                    &[anc, &conflict_path],
                                                    Vec::new(),
                                                )?;
                                                let _ = rescan_single_path(
                                                    fs,
                                                    store,
                                                    &conflict_path,
                                                    now,
                                                )?;
                                                let _ = rescan_single_path(fs, store, anc, now)?;
                                                return Ok(AncestorOutcome::ChangedRetry);
                                            }
                                            Err(err) => return Err(err),
                                        }
                                    }
                                }
                                apply_live_metadata(fs, &conflict_path, loser.mtime, loser.mode)?;
                            }
                            EntryKind::Symlink => {
                                if let Some(ref target) = loser.link_target {
                                    match fs.create_symlink_live(&conflict_path, target) {
                                        Ok(()) => {}
                                        Err(err) if is_mid_apply_race_error(&err) => {
                                            clear_pending_op(store)?;
                                            let _ = rescan_single_path(fs, store, anc, now)?;
                                            return Ok(AncestorOutcome::ChangedRetry);
                                        }
                                        Err(err) => return Err(err),
                                    }
                                }
                                match fs.remove_symlink_live(anc) {
                                    Ok(()) => {}
                                    Err(err) if is_mid_apply_race_error(&err) => {
                                        mark_needs_rescan_and_clear_pending(
                                            store,
                                            &[anc, &conflict_path],
                                            Vec::new(),
                                        )?;
                                        let _ = rescan_single_path(fs, store, anc, now)?;
                                        return Ok(AncestorOutcome::ChangedRetry);
                                    }
                                    Err(err) => return Err(err),
                                }
                            }
                            EntryKind::Dir => {}
                        }
                        record_disk_stat(fs, &conflict_path, &mut loser)?;
                    } else {
                        match loser.kind {
                            EntryKind::File => {
                                let claim = alloc_tmp_name_for(fs, "ancclaim", anc)?;
                                match fs.move_live_to_tmp_no_replace(anc, &claim) {
                                    Ok(()) => {}
                                    Err(err) if is_mid_apply_race_error(&err) => {
                                        clear_pending_op(store)?;
                                        let _ = rescan_single_path(fs, store, anc, now)?;
                                        return Ok(AncestorOutcome::ChangedRetry);
                                    }
                                    Err(err) if is_live_busy_error(fs, &err) => {
                                        clear_pending_op(store)?;
                                        return Ok(AncestorOutcome::Busy(busy_defer_reason(anc)));
                                    }
                                    Err(err) => return Err(err),
                                }
                                let (matched, precomputed) =
                                    check_tmp_hash(fs, &claim, loser.hash, loser.size)?;
                                if !matched {
                                    put_back_or_preserve_tmp(
                                        fs,
                                        store,
                                        &claim,
                                        anc,
                                        precomputed,
                                        now,
                                    )?;
                                    mark_needs_rescan_and_clear_pending(store, &[anc], Vec::new())?;
                                    let _ = rescan_single_path(fs, store, anc, now)?;
                                    return Ok(AncestorOutcome::ChangedRetry);
                                }
                                if let Some(mut b_entry) = build_bin_entry(
                                    Some(&rec),
                                    BinReason::ReplacedByPeer,
                                    &meta_ro.local_member,
                                    now,
                                ) {
                                    if finalize_binned_tmp_and_rescue(
                                        fs,
                                        store,
                                        &claim,
                                        anc,
                                        &mut b_entry,
                                        now,
                                    )? {
                                        persisted_bins.push(b_entry);
                                    }
                                } else {
                                    preserve_tmp_as_conflict_copy_with_hash(
                                        fs,
                                        store,
                                        &claim,
                                        anc,
                                        precomputed,
                                        now,
                                    )?;
                                }
                            }
                            EntryKind::Symlink => match fs.remove_symlink_live(anc) {
                                Ok(()) => {}
                                Err(err) if is_mid_apply_race_error(&err) => {
                                    clear_pending_op(store)?;
                                    let _ = rescan_single_path(fs, store, anc, now)?;
                                    return Ok(AncestorOutcome::ChangedRetry);
                                }
                                Err(err) => return Err(err),
                            },
                            EntryKind::Dir => {}
                        }
                    }
                    match fs.create_dir_all_live(anc) {
                        Ok(()) => {}
                        Err(err) if is_mid_apply_race_error(&err) => {
                            // A person wrote a new non-directory at `anc` right after we moved the
                            // previous entry aside! Commit `loser` only if still intact on disk,
                            // rescan `anc`, and retry so both entries survive.
                            if disk_matches_record(fs, &conflict_path, &loser)? {
                                record_disk_stat(fs, &conflict_path, &mut loser)?;
                                commit_records_and_clear_pending(
                                    fs,
                                    store,
                                    vec![loser],
                                    persisted_bins,
                                    Vec::new(),
                                    Vec::new(),
                                )?;
                                mark_needs_rescan(store, &[anc])?;
                            } else {
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[anc, &conflict_path],
                                    persisted_bins,
                                )?;
                                let _ = rescan_single_path(fs, store, &conflict_path, now)?;
                            }
                            let _ = rescan_single_path(fs, store, anc, now)?;
                            return Ok(AncestorOutcome::ChangedRetry);
                        }
                        Err(err) => return Err(err),
                    }
                    record_disk_stat(fs, anc, &mut dir_record)?;
                    if disk_matches_record(fs, &conflict_path, &loser)? {
                        record_disk_stat(fs, &conflict_path, &mut loser)?;
                        commit_records_and_clear_pending(
                            fs,
                            store,
                            vec![loser, dir_record],
                            persisted_bins,
                            Vec::new(),
                            Vec::new(),
                        )?;
                    } else {
                        commit_records_and_clear_pending(
                            fs,
                            store,
                            vec![dir_record],
                            persisted_bins,
                            Vec::new(),
                            Vec::new(),
                        )?;
                        mark_needs_rescan(store, &[&conflict_path])?;
                    }
                    Ok(AncestorOutcome::Ready)
                })();
                match recover_on_non_crash_error(fs, store, anc_res, now)? {
                    AncestorOutcome::Ready => {}
                    other => return Ok(other),
                }
            }
            prev_deleted_or_none => {
                match fs.create_dir_all_live(anc) {
                    Ok(()) => {}
                    Err(err) if is_mid_apply_race_error(&err) => {
                        let _ = rescan_single_path(fs, store, anc, now)?;
                        return Ok(AncestorOutcome::ChangedRetry);
                    }
                    Err(err) => return Err(err),
                }
                let mut meta = load_meta_required(store)?;
                let was_deleted = prev_deleted_or_none.is_some();
                let mut version = prev_deleted_or_none
                    .as_ref()
                    .map_or_else(VersionVector::new, |p| p.version.clone());
                meta.bump_version(&mut version);
                let seq = meta.next_seq();
                let dir_stat = fs.stat_live(anc)?;
                let dir_mtime = dir_stat.as_ref().map_or(now, |m| m.mtime);
                let dir_rec = IndexRecord {
                    path: anc.to_string(),
                    kind: EntryKind::Dir,
                    size: 0,
                    mtime: dir_mtime,
                    disk_mtime: Some(dir_mtime),
                    disk_change_id: dir_stat.and_then(|m| m.change_id),
                    mode: None,
                    hash: None,
                    link_target: None,
                    deleted: false,
                    version,
                    modified_by: meta.local_member.clone(),
                    seq,
                    recent_hashes: Vec::new(),
                    from_join: false,
                    conflict_of: None,
                    conflict_dismissed: false,
                };
                let chain = meta.advance_chain_fingerprint(seq, dir_rec.head_fingerprint());
                if was_deleted {
                    let encoded =
                        encode_deletion_log_entries(&[FolderDeletionLogEntry::Revived {
                            path: anc.to_string(),
                        }])?;
                    fs.append_deletion_log(&encoded)?;
                }
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    upsert_records: vec![dir_rec],
                    upsert_seq_fingerprints: vec![(seq, chain)],
                    ..StoreBatch::default()
                };
                store.apply_batch(batch)?;
            }
        }
    }
    Ok(AncestorOutcome::Ready)
}

/// Assigns a fresh independent version vector to `loser_record` (Decision A).
///
/// A conflict copy is a new independent file: it NEVER inherits the loser's original version
/// vector. If two members independently derive the same conflict copy (same name + same content),
/// their fresh singleton versions are concurrent with identical content and merge quietly.
fn finalize_independent_conflict_version<S: IndexStore>(
    store: &mut S,
    loser_record: &mut IndexRecord,
) -> Result<()> {
    let mut meta = load_meta_required(store)?;

    if let Some(existing) = store.get_record(&loser_record.path)?
        && (existing.deleted || existing.content_equal(loser_record))
    {
        // Bump the existing record's version even when the same conflict copy is still live
        // locally: a new conflict re-asserting this path must causally outrank any unseen or
        // held-back deletion of an earlier copy (e.g. after a peer ran `KeepConflict`).
        let mut version = existing.version;
        meta.bump_version(&mut version);
        loser_record.version = version;
        let batch = StoreBatch {
            folder_meta: Some(meta),
            ..StoreBatch::default()
        };
        store.apply_batch(batch)?;
        return Ok(());
    }

    let vkey = meta.version_key();
    let clock = meta.next_clock();
    loser_record.version = VersionVector::singleton(vkey, clock);
    let batch = StoreBatch {
        folder_meta: Some(meta),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)
}

/// Inspects regular-file entries in `.legion-sync/bin` and immediately promotes any entry whose
/// bytes were modified after it was binned (for example through an open file handle held across an
/// atomic swap or rename on Unix) into the live folder as a conflict copy so the late save is
/// indexed, synced, and never lost on bin expiry.
///
/// Uses a fast stat check (`size`, `binned_mtime`, `binned_change_id`) unless `verify_all` is
/// `true`, only hashing entries whose stat changed or has not yet been recorded.
pub fn inspect_and_promote_modified_bin_entries<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    verify_all: bool,
    now: Timestamp,
) -> Result<usize> {
    let entries = store.list_bin_entries()?;
    if entries.is_empty() {
        return Ok(0);
    }

    let mut promoted = 0usize;
    let mut refreshed_entries = Vec::new();

    for mut entry in entries {
        if entry.kind != EntryKind::File || entry.storage_name.is_empty() {
            continue;
        }
        let Some(st) = fs.stat_bin(&entry.storage_name)? else {
            continue;
        };
        let stat_changed = verify_all
            || st.size != entry.size
            || entry.binned_mtime.is_none_or(|bm| bm != st.mtime)
            || match (entry.binned_change_id, st.change_id) {
                (Some(a), Some(b)) => a != b,
                (None, None) => false,
                _ => true,
            };
        if !stat_changed {
            continue;
        }

        let (actual_hash, actual_size) = match fs.hash_bin(&entry.storage_name) {
            Ok(pair) => pair,
            Err(SyncError::Fs {
                kind: FsErrorKind::NotFound,
                ..
            }) => continue,
            Err(err) => return Err(err),
        };
        if entry.hash != Some(actual_hash) || actual_size != entry.size {
            let _ = promote_modified_bin_entry(
                fs,
                store,
                &entry,
                Some((actual_hash, actual_size)),
                now,
            )?;
            promoted += 1;
        } else if entry.binned_mtime != Some(st.mtime) || entry.binned_change_id != st.change_id {
            entry.binned_mtime = Some(st.mtime);
            entry.binned_change_id = st.change_id;
            write_bin_meta_file(fs, &entry)?;
            refreshed_entries.push(entry);
        }
    }

    if !refreshed_entries.is_empty() {
        store.apply_batch(StoreBatch {
            upsert_bin: refreshed_entries,
            ..StoreBatch::default()
        })?;
    }

    Ok(promoted)
}

/// Promotes a bin file whose on-disk bytes no longer match its recorded `(hash, size)` into the
/// live tree as a conflict copy beside `entry.original_path`, removes its bin entry, and indexes
/// the promoted copy with a fresh independent version vector so it syncs out to all peers.
fn promote_modified_bin_entry<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    entry: &BinEntry,
    precomputed_hash: Option<(Hash, u64)>,
    now: Timestamp,
) -> Result<String> {
    let bin_stat = fs.stat_bin(&entry.storage_name)?;
    let (actual_hash, actual_size) = match precomputed_hash {
        Some(pair) => pair,
        None => fs.hash_bin(&entry.storage_name)?,
    };
    let mut meta = load_meta_required(store)?;
    let hex = actual_hash.to_hex();
    let hash6 = &hex[..6.min(hex.len())];
    let file_mtime = bin_stat.as_ref().map_or(now, |m| m.mtime);
    let file_mode = entry
        .mode
        .or_else(|| bin_stat.as_ref().and_then(|m| m.mode));

    let mut chosen_path = None;
    let mut base_path = entry.original_path.as_str();
    let basename = entry
        .original_path
        .rsplit('/')
        .next()
        .unwrap_or("recovered-file");
    for attempt in 0u32..10_000 {
        let disambig = if attempt == 0 {
            None
        } else {
            Some((attempt + 1).to_string())
        };
        let cand = conflict_copy_path(
            base_path,
            &meta.local_member,
            file_mtime,
            hash6,
            disambig.as_deref(),
        );
        match fs.move_bin_to_live_no_replace(&entry.storage_name, &cand) {
            Ok(()) => {
                chosen_path = Some(cand);
                break;
            }
            Err(SyncError::Fs {
                kind: FsErrorKind::AlreadyExists,
                ..
            }) => continue,
            Err(err) if is_mid_apply_race_error(&err) && base_path != basename => {
                base_path = basename;
                continue;
            }
            Err(err) => return Err(err),
        }
    }

    let conflict_path = match chosen_path {
        Some(p) => p,
        None => {
            let token = random_hex_token();
            let cand = conflict_copy_path(
                base_path,
                &meta.local_member,
                file_mtime,
                hash6,
                Some(&token[..8]),
            );
            fs.move_bin_to_live_no_replace(&entry.storage_name, &cand)?;
            cand
        }
    };

    let _ = fs.remove_bin_file(&bin_meta_name(&entry.id));

    let live_stat = fs.stat_live(&conflict_path)?;
    let vkey = meta.version_key();
    let clock = meta.next_clock();
    let seq = meta.next_seq();
    let mut rec = IndexRecord {
        path: conflict_path.clone(),
        kind: EntryKind::File,
        size: actual_size,
        mtime: file_mtime,
        disk_mtime: live_stat.as_ref().map(|m| m.mtime).or(Some(file_mtime)),
        disk_change_id: live_stat.as_ref().and_then(|m| m.change_id),
        mode: file_mode,
        hash: Some(actual_hash),
        link_target: None,
        deleted: false,
        version: VersionVector::singleton(vkey, clock),
        modified_by: meta.local_member.clone(),
        seq,
        recent_hashes: Vec::new(),
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    };
    rec.conflict_of = derive_conflict_of(&rec, &meta).or_else(|| Some(entry.original_path.clone()));
    let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
    let batch = StoreBatch {
        folder_meta: Some(meta),
        upsert_records: vec![rec],
        remove_bin: vec![entry.id.clone()],
        upsert_seq_fingerprints: vec![(seq, chain)],
        ..StoreBatch::default()
    };
    store.apply_batch(batch)?;
    Ok(conflict_path)
}

/// Moves verified `tmp_name` into `.legion-sync/bin/<bin_entry.storage_name>`, and immediately
/// promotes the file to a live conflict copy if an open writer modified it while it was moving
/// into the bin.
///
/// `binned_mtime` and `binned_change_id` intentionally stay `None` until the first post-bin scan
/// verifies the binned file's hash (catching even same-size overwrites that land inside the same
/// coarse filesystem timestamp tick as the move into the bin).
///
/// Returns `Ok(true)` if the entry remains in `.legion-sync/bin`, or `Ok(false)` if it was
/// promoted to a live conflict copy instead.
fn finalize_binned_tmp_and_rescue<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    tmp_name: &str,
    _live_path: &str,
    bin_entry: &mut BinEntry,
    now: Timestamp,
) -> Result<bool> {
    let pre_stat = fs.stat_tmp(tmp_name)?;
    bin_entry.binned_mtime = None;
    bin_entry.binned_change_id = None;
    write_bin_meta_file(fs, bin_entry)?;
    fs.move_tmp_to_bin_no_replace(tmp_name, &bin_entry.storage_name)?;

    if let Some(post_stat) = fs.stat_bin(&bin_entry.storage_name)? {
        let mtime_or_size_changed = post_stat.size != bin_entry.size
            || post_stat.mtime != bin_entry.original_mtime
            || pre_stat
                .as_ref()
                .is_some_and(|pre| pre.mtime != post_stat.mtime);
        if mtime_or_size_changed {
            let (actual_hash, actual_size) = fs.hash_bin(&bin_entry.storage_name)?;
            if bin_entry.hash != Some(actual_hash) || actual_size != bin_entry.size {
                let _ = promote_modified_bin_entry(
                    fs,
                    store,
                    bin_entry,
                    Some((actual_hash, actual_size)),
                    now,
                )?;
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Expires bin entries older than `retention_secs`, refusing to run when `now` is implausible
/// (earlier than [`MIN_PLAUSIBLE_CLOCK_SECS`] or earlier than the newest bin entry) and ALWAYS
/// verifying each file's bytes against its recorded hash before deleting so a late write through a
/// held handle is promoted into the live tree as a conflict copy instead of ever being discarded.
pub fn expire_bin<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    retention_secs: u64,
    now: Timestamp,
) -> Result<Vec<BinEntry>> {
    let entries = store.list_bin_entries()?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // Item G: clock sanity guard — never expire bin entries when the wall clock is implausible
    // or earlier than the newest entry stored in the bin.
    let newest_deleted_at = entries
        .iter()
        .map(|e| e.deleted_at.secs)
        .max()
        .unwrap_or(i64::MIN);
    if now.secs < MIN_PLAUSIBLE_CLOCK_SECS || now.secs < newest_deleted_at {
        return Ok(Vec::new());
    }

    let retention = i64::try_from(retention_secs).unwrap_or(i64::MAX);
    let mut expired = Vec::new();

    for entry in entries {
        if now.secs.saturating_sub(entry.deleted_at.secs) >= retention {
            if entry.kind == EntryKind::File && !entry.storage_name.is_empty() {
                if fs.stat_bin(&entry.storage_name)?.is_some() {
                    let (actual_hash, actual_size) = fs.hash_bin(&entry.storage_name)?;
                    if entry.hash != Some(actual_hash) || actual_size != entry.size {
                        let _ = promote_modified_bin_entry(
                            fs,
                            store,
                            &entry,
                            Some((actual_hash, actual_size)),
                            now,
                        )?;
                        continue;
                    }
                }
                fs.remove_bin_file(&entry.storage_name)?;
            }
            fs.remove_bin_file(&bin_meta_name(&entry.id))?;
            expired.push(entry);
        }
    }

    if !expired.is_empty() {
        let mut batch = StoreBatch::default();
        for entry in &expired {
            batch.remove_bin.push(entry.id.clone());
        }
        store.apply_batch(batch)?;
    }

    Ok(expired)
}

/// Restores a [`BinEntry`] from `.legion-sync/bin` back into the live tree with full path-safety
/// validation, pre-replace disk re-check (never overwriting unscanned files), and write-ahead
/// crash safety (`PendingOp::RestoreFromBin`).
pub fn restore_from_bin<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    entry_id: &str,
    dest_path: Option<&str>,
    strategy: RestoreStrategy,
    now: Timestamp,
) -> Result<String> {
    let entry = store
        .get_bin_entry(entry_id)?
        .ok_or_else(|| SyncError::NotFound {
            what: format!("bin entry {entry_id:?}"),
        })?;

    let base_dest = match dest_path {
        Some(p) => validate_sync_path(p)?,
        None => validate_sync_path(&entry.original_path)?,
    };

    let target_path = resolve_restore_destination(fs, store, &base_dest, strategy, now)?;

    // Item F: apply full platform/representability checks to restore destinations as well.
    let caps = fs.capabilities();
    let local_member = load_meta_required(store)?.local_member;
    {
        let view = StoreIndexView::new(store);
        let replacing = (strategy == RestoreStrategy::Replace).then_some(target_path.as_str());
        if let Some(problem) = check_local_representable(
            &view,
            &target_path,
            entry.kind,
            entry.link_target.as_deref(),
            &local_member,
            &caps,
            replacing,
        ) {
            return Err(SyncError::InvalidPath {
                path: target_path,
                reason: problem.sentence,
            });
        }
    }

    if !matches!(
        ensure_ancestors_are_dirs(fs, store, &target_path, now)?,
        AncestorOutcome::Ready
    ) {
        return Err(SyncError::Conflict {
            path: target_path,
            reason: "an ancestor file is still being written; retry restoring".to_string(),
        });
    }

    // Item G / Probe A6: re-check disk against the index BEFORE mutating anything!
    // Never overwrite a file the scanner has not seen.
    let existing = store.get_record(&target_path)?;
    if !disk_matches_index(fs, &target_path, existing.as_ref())? {
        let _ = rescan_single_path(fs, store, &target_path, now)?;
        return Err(SyncError::Conflict {
            path: target_path,
            reason: "file on disk changed since the last scan; review it before restoring"
                .to_string(),
        });
    }

    let existing_live = existing.as_ref().filter(|r| !r.deleted);
    let mut replacement_bin = if existing_live.is_some() {
        if strategy != RestoreStrategy::Replace {
            return Err(SyncError::Exists { path: target_path });
        }
        build_bin_entry(
            existing_live,
            BinReason::ReplacedLocally,
            &local_member,
            now,
        )
    } else {
        None
    };

    let (staged_tmp, staged_meta) = match entry.kind {
        EntryKind::File => {
            let expected_hash = entry.hash.ok_or_else(|| SyncError::CorruptData {
                context: format!("bin entry {}", entry.id),
                detail: "file bin entry missing hash".to_string(),
            })?;
            let (actual_hash, actual_size) = fs.hash_bin(&entry.storage_name)?;
            if actual_hash != expected_hash || actual_size != entry.size {
                // Late writes happened through an open handle after this file entered the bin!
                // Promote the modified file into the live tree as a conflict copy so the user's
                // newer bytes are immediately rescued instead of failing with HashMismatch.
                return promote_modified_bin_entry(
                    fs,
                    store,
                    &entry,
                    Some((actual_hash, actual_size)),
                    now,
                );
            }
            let tmp_name = alloc_tmp_name_for(fs, "restore", &target_path)?;
            register_staged_tmp(store, &tmp_name)?;
            if let Err(err) = fs.copy_bin_to_tmp(&entry.storage_name, &tmp_name) {
                if !err.is_crash() {
                    let _ = fs.remove_tmp(&tmp_name);
                    let _ = unregister_staged_tmp(store, &tmp_name);
                }
                return Err(err);
            }
            let sm = prepare_staged_tmp_metadata(fs, &tmp_name, entry.original_mtime, entry.mode)?;
            (Some(tmp_name), sm)
        }
        EntryKind::Symlink | EntryKind::Dir => (None, None),
    };

    let claim_tmp = if existing_live.is_some_and(|r| r.kind == EntryKind::File) {
        Some(alloc_tmp_name_for(fs, "restoreclaim", &target_path)?)
    } else {
        None
    };

    let mut meta = load_meta_required(store)?;
    let mut version = match existing.as_ref() {
        Some(p) => p.version.clone(),
        None => store
            .get_deleted_history_version(&target_path)?
            .unwrap_or_default(),
    };
    meta.bump_version(&mut version);

    let mut recent_hashes = existing
        .as_ref()
        .map_or_else(Vec::new, |p| p.recent_hashes.clone());
    if let Some(old_rec) = existing_live
        && let Some(old_h) = old_rec.hash
        && Some(old_h) != entry.hash
    {
        recent_hashes.retain(|h| *h != old_h);
        recent_hashes.insert(0, old_h);
        recent_hashes.truncate(MAX_ANCESTRY_HASHES);
    }

    let mut restored_record = IndexRecord {
        path: target_path.clone(),
        kind: entry.kind,
        size: entry.size,
        mtime: entry.original_mtime,
        disk_mtime: Some(entry.original_mtime),
        disk_change_id: None,
        mode: entry.mode,
        hash: entry.hash,
        link_target: entry.link_target.clone(),
        deleted: false,
        version,
        modified_by: meta.local_member.clone(),
        seq: 0,
        recent_hashes,
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    };
    restored_record.conflict_of = derive_conflict_of(&restored_record, &meta);

    let expected_local = snapshot_local(existing.as_ref());

    // Persist clock bump + write-ahead PendingOp::RestoreFromBin BEFORE touching disk!
    let pending = PendingOp::RestoreFromBin {
        target_record: restored_record.clone(),
        restored_bin_id: entry.id.clone(),
        restored_storage_name: entry.storage_name.clone(),
        staged_tmp: staged_tmp.clone(),
        claim_tmp: claim_tmp.clone(),
        replacement_bin: replacement_bin.clone(),
        expected_local,
    };
    let pre_batch = StoreBatch {
        folder_meta: Some(meta),
        set_pending_op: Some(Some(pending)),
        remove_staged_tmps: staged_tmp.iter().cloned().collect(),
        ..StoreBatch::default()
    };
    store.apply_batch(pre_batch)?;

    let res: Result<String> = (|| {
        let mut persisted_bins = Vec::new();
        let mut stat_recorded = false;

        // Atomic swap/replace when replacing an existing live file with a restored file.
        if let (Some(old_rec), EntryKind::File, Some(tmp), Some(claim), Some(rep_bin)) = (
            existing_live.filter(|r| r.kind == EntryKind::File),
            entry.kind,
            staged_tmp.as_deref(),
            claim_tmp.as_deref(),
            replacement_bin.as_mut(),
        ) {
            let old_hash = old_rec.hash.ok_or_else(|| SyncError::CorruptData {
                context: format!("local record for {target_path}"),
                detail: "live file missing BLAKE3 hash".to_string(),
            })?;
            match safe_replace_live_file(
                fs,
                store,
                tmp,
                claim,
                &target_path,
                old_hash,
                old_rec.size,
                rep_bin,
                &mut restored_record,
                staged_meta.as_ref(),
                now,
            )? {
                SafeReplaceResult::Replaced { binned } => {
                    if binned {
                        persisted_bins.push(rep_bin.clone());
                    }
                    stat_recorded = true;
                }
                SafeReplaceResult::LiveChangedAfterDisplace { binned } => {
                    let bins = if binned {
                        vec![rep_bin.clone()]
                    } else {
                        Vec::new()
                    };
                    mark_needs_rescan_and_clear_pending(store, &[&target_path], bins)?;
                    let _ = rescan_single_path(fs, store, &target_path, now)?;
                    return Err(SyncError::Conflict {
                        path: target_path,
                        reason: "file on disk changed during restore; review it and retry"
                            .to_string(),
                    });
                }
                SafeReplaceResult::RaceBeforePlace => {
                    let _ = fs.remove_tmp(tmp);
                    clear_pending_op(store)?;
                    let _ = rescan_single_path(fs, store, &target_path, now)?;
                    return Err(SyncError::Conflict {
                        path: target_path,
                        reason: "file on disk changed during restore; review it and retry"
                            .to_string(),
                    });
                }
                SafeReplaceResult::Deferred { reason, binned } => {
                    if binned {
                        mark_needs_rescan_and_clear_pending(
                            store,
                            &[&target_path],
                            vec![rep_bin.clone()],
                        )?;
                    } else {
                        clear_pending_op(store)?;
                    }
                    return Err(SyncError::Conflict {
                        path: target_path,
                        reason,
                    });
                }
            }
        } else {
            // Move aside existing non-file or cross-kind entry safely first.
            if let Some(old_rec) = existing_live {
                match old_rec.kind {
                    EntryKind::File => {
                        if let (Some(claim), Some(rep_bin), Some(old_hash)) =
                            (claim_tmp.as_deref(), replacement_bin.as_mut(), old_rec.hash)
                        {
                            match safe_delete_live_file(
                                fs,
                                store,
                                &target_path,
                                claim,
                                old_hash,
                                old_rec.size,
                                rep_bin,
                                now,
                            )? {
                                SafeDeleteResult::Deleted { binned } => {
                                    if binned {
                                        persisted_bins.push(rep_bin.clone());
                                    }
                                }
                                SafeDeleteResult::RaceChanged => {
                                    if let Some(ref st) = staged_tmp {
                                        let _ = fs.remove_tmp(st);
                                    }
                                    clear_pending_op(store)?;
                                    let _ = rescan_single_path(fs, store, &target_path, now)?;
                                    return Err(SyncError::Conflict {
                                        path: target_path,
                                        reason: "file on disk changed during restore".to_string(),
                                    });
                                }
                                SafeDeleteResult::Deferred(reason) => {
                                    if let Some(ref st) = staged_tmp {
                                        let _ = fs.remove_tmp(st);
                                    }
                                    clear_pending_op(store)?;
                                    return Err(SyncError::Conflict {
                                        path: target_path,
                                        reason,
                                    });
                                }
                            }
                        }
                    }
                    EntryKind::Symlink => {
                        if let Some(ref rep_bin) = replacement_bin {
                            write_bin_meta_file(fs, rep_bin)?;
                            persisted_bins.push(rep_bin.clone());
                        }
                        fs.remove_symlink_live(&target_path)?;
                    }
                    EntryKind::Dir => {
                        fs.remove_empty_dir_live(&target_path)?;
                    }
                }
            }

            match entry.kind {
                EntryKind::File => {
                    if let Some(ref tmp) = staged_tmp {
                        match fs.commit_tmp_to_live_no_replace(tmp, &target_path) {
                            Ok(()) => {}
                            Err(SyncError::Fs {
                                kind: FsErrorKind::AlreadyExists,
                                ..
                            }) => {
                                let _ = fs.remove_tmp(tmp);
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[&target_path],
                                    persisted_bins,
                                )?;
                                let _ = rescan_single_path(fs, store, &target_path, now)?;
                                return Err(SyncError::Conflict {
                                    path: target_path,
                                    reason: "a new file appeared on disk during restore"
                                        .to_string(),
                                });
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    if !verify_and_record_placed_stat(
                        fs,
                        &target_path,
                        &mut restored_record,
                        staged_meta.as_ref(),
                        true,
                        now,
                    )? {
                        mark_needs_rescan_and_clear_pending(
                            store,
                            &[&target_path],
                            persisted_bins,
                        )?;
                        let _ = rescan_single_path(fs, store, &target_path, now)?;
                        return Err(SyncError::Conflict {
                            path: target_path,
                            reason: "file on disk changed during restore; review it and retry"
                                .to_string(),
                        });
                    }
                    stat_recorded = true;
                }
                EntryKind::Symlink => {
                    let link_target =
                        entry
                            .link_target
                            .as_deref()
                            .ok_or_else(|| SyncError::CorruptData {
                                context: format!("bin entry {}", entry.id),
                                detail: "symlink bin entry missing link_target".to_string(),
                            })?;
                    fs.create_symlink_live(&target_path, link_target)?;
                }
                EntryKind::Dir => {
                    fs.create_dir_all_live(&target_path)?;
                }
            }
        }

        if !stat_recorded {
            record_disk_stat(fs, &target_path, &mut restored_record)?;
        }
        commit_records_and_clear_pending(
            fs,
            store,
            vec![restored_record],
            persisted_bins,
            Vec::new(),
            vec![entry.id.clone()],
        )?;

        if !entry.storage_name.is_empty() {
            let _ = fs.remove_bin_file(&entry.storage_name);
        }
        let _ = fs.remove_bin_file(&bin_meta_name(&entry.id));

        Ok(target_path)
    })();

    recover_on_non_crash_error(fs, store, res, now)
}

/// Resolves a file with two versions according to `choice`, checking path safety, verifying that
/// neither the conflict copy nor the original path changed on disk since the last scan, and
/// bracketing filesystem mutations with a durable [`PendingOp`].
pub fn resolve_conflict<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    conflict_path: &str,
    choice: ConflictChoice,
    now: Timestamp,
) -> Result<()> {
    let clean_conflict = validate_sync_path(conflict_path)?;
    let conflict_rec = store
        .get_record(&clean_conflict)?
        .filter(|r| !r.deleted)
        .ok_or_else(|| SyncError::NotFound {
            what: format!("live conflict copy {clean_conflict:?}"),
        })?;
    let orig_path_raw = conflict_rec
        .conflict_of
        .clone()
        .ok_or_else(|| SyncError::Conflict {
            path: clean_conflict.clone(),
            reason: "entry is not marked as a conflict copy".to_string(),
        })?;
    let orig_path = validate_sync_path(&orig_path_raw)?;

    // Item G: verify conflict copy itself hasn't changed on disk since last scan!
    if !disk_matches_index(fs, &clean_conflict, Some(&conflict_rec))? {
        let _ = rescan_single_path(fs, store, &clean_conflict, now)?;
        return Err(SyncError::Conflict {
            path: clean_conflict,
            reason: "conflict copy changed on disk since the last scan; review it before resolving"
                .to_string(),
        });
    }

    match choice {
        ConflictChoice::KeepOriginal => {
            let mut meta = load_meta_required(store)?;
            let mut bin_entry = build_bin_entry(
                Some(&conflict_rec),
                BinReason::ReplacedLocally,
                &meta.local_member,
                now,
            );
            let claim_tmp = if conflict_rec.kind == EntryKind::File {
                Some(alloc_tmp_name_for(fs, "resdel", &clean_conflict)?)
            } else {
                None
            };

            let mut version = conflict_rec.version.clone();
            meta.bump_version(&mut version);
            let mut recent_hashes = conflict_rec.recent_hashes.clone();
            if let Some(h) = conflict_rec.hash {
                recent_hashes.retain(|x| *x != h);
                recent_hashes.insert(0, h);
                recent_hashes.truncate(MAX_ANCESTRY_HASHES);
            }
            let tombstone = IndexRecord {
                path: clean_conflict.clone(),
                kind: conflict_rec.kind,
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
                seq: 0,
                recent_hashes,
                from_join: false,
                conflict_of: None,
                conflict_dismissed: false,
            };

            let pending = PendingOp::Delete {
                target_record: tombstone.clone(),
                claim_tmp: claim_tmp.clone(),
                bin_entry: bin_entry.clone(),
                expected_local: snapshot_local(Some(&conflict_rec)),
            };
            let pre_batch = StoreBatch {
                folder_meta: Some(meta),
                set_pending_op: Some(Some(pending)),
                ..StoreBatch::default()
            };
            store.apply_batch(pre_batch)?;

            let res: Result<()> = (|| {
                let mut persisted_bins = Vec::new();
                match conflict_rec.kind {
                    EntryKind::File => {
                        if let (Some(claim), Some(b_entry), Some(old_hash)) =
                            (claim_tmp.as_deref(), bin_entry.as_mut(), conflict_rec.hash)
                        {
                            match safe_delete_live_file(
                                fs,
                                store,
                                &clean_conflict,
                                claim,
                                old_hash,
                                conflict_rec.size,
                                b_entry,
                                now,
                            )? {
                                SafeDeleteResult::Deleted { binned } => {
                                    if binned {
                                        persisted_bins.push(b_entry.clone());
                                    }
                                }
                                SafeDeleteResult::RaceChanged => {
                                    clear_pending_op(store)?;
                                    let _ = rescan_single_path(fs, store, &clean_conflict, now)?;
                                    return Err(SyncError::Conflict {
                                        path: clean_conflict,
                                        reason: "conflict copy changed on disk during resolution"
                                            .to_string(),
                                    });
                                }
                                SafeDeleteResult::Deferred(reason) => {
                                    clear_pending_op(store)?;
                                    return Err(SyncError::Conflict {
                                        path: clean_conflict,
                                        reason,
                                    });
                                }
                            }
                        }
                    }
                    EntryKind::Symlink => {
                        if let Some(ref b_entry) = bin_entry {
                            write_bin_meta_file(fs, b_entry)?;
                            persisted_bins.push(b_entry.clone());
                        }
                        fs.remove_symlink_live(&clean_conflict)?;
                    }
                    EntryKind::Dir => {
                        let _ = fs.remove_empty_dir_live(&clean_conflict);
                    }
                }
                commit_records_and_clear_pending(
                    fs,
                    store,
                    vec![tombstone],
                    persisted_bins,
                    Vec::new(),
                    Vec::new(),
                )?;
                Ok(())
            })();

            recover_on_non_crash_error(fs, store, res, now)
        }
        ConflictChoice::KeepConflict => {
            // Item F: verify target `orig_path` is representable on this filesystem.
            let caps = fs.capabilities();
            let local_member = load_meta_required(store)?.local_member;
            {
                let view = StoreIndexView::new(store);
                if let Some(problem) = check_local_representable(
                    &view,
                    &orig_path,
                    conflict_rec.kind,
                    conflict_rec.link_target.as_deref(),
                    &local_member,
                    &caps,
                    Some(&orig_path),
                ) {
                    return Err(SyncError::InvalidPath {
                        path: orig_path,
                        reason: problem.sentence,
                    });
                }
            }

            // Item G / Probe A7: re-check disk at `orig_path` BEFORE replacing it!
            let orig_rec = store.get_record(&orig_path)?;
            if !disk_matches_index(fs, &orig_path, orig_rec.as_ref())? {
                let _ = rescan_single_path(fs, store, &orig_path, now)?;
                return Err(SyncError::Conflict {
                    path: orig_path,
                    reason:
                        "file on disk at the original path changed since the last scan; review it before resolving"
                            .to_string(),
                });
            }

            let mut meta = load_meta_required(store)?;
            let orig_live = orig_rec.as_ref().filter(|r| !r.deleted);
            let mut bin_entry = build_bin_entry(
                orig_live,
                BinReason::ReplacedLocally,
                &meta.local_member,
                now,
            );
            let claim_tmp = if orig_live.is_some_and(|r| r.kind == EntryKind::File) {
                Some(alloc_tmp_name_for(fs, "reskeepclaim", &orig_path)?)
            } else {
                None
            };

            // Never merge `conflict_rec.version` into `orig_version`: conflict copies are
            // independent paths with their own version vectors, so merging another path's
            // counters here would forge false causal dominance over unseen versions of
            // `orig_path` authored by the same member.
            let mut orig_version = orig_rec
                .as_ref()
                .map_or_else(VersionVector::new, |r| r.version.clone());
            meta.bump_version(&mut orig_version);

            let mut orig_recent = orig_rec
                .as_ref()
                .map_or_else(Vec::new, |r| r.recent_hashes.clone());
            if let Some(o) = orig_live
                && let Some(oh) = o.hash
                && Some(oh) != conflict_rec.hash
            {
                orig_recent.retain(|h| *h != oh);
                orig_recent.insert(0, oh);
                orig_recent.truncate(MAX_ANCESTRY_HASHES);
            }

            let mut conflict_version = conflict_rec.version.clone();
            meta.bump_version(&mut conflict_version);
            let mut conflict_recent = conflict_rec.recent_hashes.clone();
            if let Some(ch) = conflict_rec.hash {
                conflict_recent.retain(|h| *h != ch);
                conflict_recent.insert(0, ch);
                conflict_recent.truncate(MAX_ANCESTRY_HASHES);
            }

            let mut promoted = IndexRecord {
                path: orig_path.clone(),
                kind: conflict_rec.kind,
                size: conflict_rec.size,
                mtime: conflict_rec.mtime,
                disk_mtime: Some(conflict_rec.mtime),
                disk_change_id: None,
                mode: conflict_rec.mode,
                hash: conflict_rec.hash,
                link_target: conflict_rec.link_target.clone(),
                deleted: false,
                version: orig_version,
                modified_by: meta.local_member.clone(),
                seq: 0,
                recent_hashes: orig_recent,
                from_join: false,
                conflict_of: None,
                conflict_dismissed: false,
            };
            let deleted_conflict = IndexRecord {
                path: clean_conflict.clone(),
                kind: conflict_rec.kind,
                size: 0,
                mtime: now,
                disk_mtime: None,
                disk_change_id: None,
                mode: None,
                hash: None,
                link_target: None,
                deleted: true,
                version: conflict_version,
                modified_by: meta.local_member.clone(),
                seq: 0,
                recent_hashes: conflict_recent,
                from_join: false,
                conflict_of: None,
                conflict_dismissed: false,
            };

            let pending = PendingOp::ResolveKeepConflict {
                promoted_record: promoted.clone(),
                deleted_conflict_record: deleted_conflict.clone(),
                claim_tmp: claim_tmp.clone(),
                bin_entry: bin_entry.clone(),
                expected_orig: snapshot_local(orig_rec.as_ref()),
                expected_conflict: snapshot_local(Some(&conflict_rec)),
            };
            let pre_batch = StoreBatch {
                folder_meta: Some(meta),
                set_pending_op: Some(Some(pending)),
                ..StoreBatch::default()
            };
            store.apply_batch(pre_batch)?;

            let res: Result<()> = (|| {
                let mut persisted_bins = Vec::new();
                if let Some(orig) = orig_live {
                    match orig.kind {
                        EntryKind::File => {
                            if let (Some(claim), Some(b_entry), Some(old_hash)) =
                                (claim_tmp.as_deref(), bin_entry.as_mut(), orig.hash)
                            {
                                match safe_delete_live_file(
                                    fs, store, &orig_path, claim, old_hash, orig.size, b_entry, now,
                                )? {
                                    SafeDeleteResult::Deleted { binned } => {
                                        if binned {
                                            persisted_bins.push(b_entry.clone());
                                        }
                                    }
                                    SafeDeleteResult::RaceChanged => {
                                        clear_pending_op(store)?;
                                        let _ = rescan_single_path(fs, store, &orig_path, now)?;
                                        return Err(SyncError::Conflict {
                                            path: orig_path,
                                            reason:
                                                "original file changed on disk during resolution"
                                                    .to_string(),
                                        });
                                    }
                                    SafeDeleteResult::Deferred(reason) => {
                                        clear_pending_op(store)?;
                                        return Err(SyncError::Conflict {
                                            path: orig_path,
                                            reason,
                                        });
                                    }
                                }
                            }
                        }
                        EntryKind::Symlink => {
                            if let Some(ref b_entry) = bin_entry {
                                write_bin_meta_file(fs, b_entry)?;
                                persisted_bins.push(b_entry.clone());
                            }
                            fs.remove_symlink_live(&orig_path)?;
                        }
                        EntryKind::Dir => {
                            fs.remove_empty_dir_live(&orig_path)?;
                        }
                    }
                }

                match conflict_rec.kind {
                    EntryKind::File => {
                        match fs.rename_live_to_live_no_replace(&clean_conflict, &orig_path) {
                            Ok(()) => {}
                            Err(SyncError::Fs {
                                kind: FsErrorKind::AlreadyExists,
                                ..
                            }) => {
                                mark_needs_rescan_and_clear_pending(
                                    store,
                                    &[&orig_path],
                                    persisted_bins,
                                )?;
                                let _ = rescan_single_path(fs, store, &orig_path, now)?;
                                return Err(SyncError::Conflict {
                                    path: orig_path,
                                    reason:
                                        "a new file appeared at the original path during resolution"
                                            .to_string(),
                                });
                            }
                            Err(err) => return Err(err),
                        }
                        apply_live_metadata(fs, &orig_path, conflict_rec.mtime, conflict_rec.mode)?;
                    }
                    EntryKind::Symlink => {
                        if let Some(ref target) = conflict_rec.link_target {
                            fs.create_symlink_live(&orig_path, target)?;
                        }
                        fs.remove_symlink_live(&clean_conflict)?;
                    }
                    EntryKind::Dir => {
                        fs.create_dir_all_live(&orig_path)?;
                        let _ = fs.remove_empty_dir_live(&clean_conflict);
                    }
                }

                record_disk_stat(fs, &orig_path, &mut promoted)?;
                commit_records_and_clear_pending(
                    fs,
                    store,
                    vec![deleted_conflict, promoted],
                    persisted_bins,
                    Vec::new(),
                    Vec::new(),
                )?;
                Ok(())
            })();

            recover_on_non_crash_error(fs, store, res, now)
        }
        ConflictChoice::KeepBoth => {
            let mut meta = load_meta_required(store)?;
            let mut updated = conflict_rec;
            meta.bump_version(&mut updated.version);
            let seq = meta.next_seq();
            updated.seq = seq;
            updated.conflict_dismissed = true;
            updated.conflict_of = None;
            let chain = meta.advance_chain_fingerprint(seq, updated.head_fingerprint());

            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records: vec![updated],
                upsert_seq_fingerprints: vec![(seq, chain)],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn safe_replace_live_file<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    staged_tmp: &str,
    claim_tmp: &str,
    live_path: &str,
    expected_old_hash: Hash,
    expected_old_size: u64,
    bin_entry: &mut BinEntry,
    target: &mut IndexRecord,
    staged_meta: Option<&FsMeta>,
    now: Timestamp,
) -> Result<SafeReplaceResult> {
    // Path 1: atomic exchange (`renameat2(RENAME_EXCHANGE)` / `renamex_np(RENAME_SWAP)`).
    // Decision 1 & 2: NO blind swap-back. Identify displaced bytes in `staged_tmp` by hash: if
    // they match `expected_old_hash`, move to bin; otherwise they are a person's unseen save and
    // become a conflict copy beside `live_path`, NEVER deleted. Then inspect `live_path` and
    // record its stat ONLY if it is still our staged file.
    match fs.exchange_tmp_with_live(staged_tmp, live_path) {
        Ok(true) => {
            let (matched, precomputed) =
                check_tmp_hash(fs, staged_tmp, Some(expected_old_hash), expected_old_size)?;
            let binned = if matched {
                finalize_binned_tmp_and_rescue(fs, store, staged_tmp, live_path, bin_entry, now)?
            } else {
                preserve_tmp_as_conflict_copy_with_hash(
                    fs,
                    store,
                    staged_tmp,
                    live_path,
                    precomputed,
                    now,
                )?;
                false
            };
            if verify_and_record_placed_stat(fs, live_path, target, staged_meta, true, now)? {
                return Ok(SafeReplaceResult::Replaced { binned });
            }
            return Ok(SafeReplaceResult::LiveChangedAfterDisplace { binned });
        }
        Ok(false) => {}
        Err(err) if is_mid_apply_race_error(&err) => return Ok(SafeReplaceResult::RaceBeforePlace),
        Err(err) if is_live_busy_error(fs, &err) => {
            if fs.stat_tmp(staged_tmp)?.is_some()
                && tmp_matches_hash(fs, staged_tmp, target.hash, target.size)?
            {
                let _ = fs.remove_tmp(staged_tmp);
            }
            return Ok(SafeReplaceResult::Deferred {
                reason: busy_defer_reason(live_path),
                binned: false,
            });
        }
        Err(err) => return Err(err),
    }

    // Path 2: atomic replace with backup (`ReplaceFileW` on Windows).
    // Same principle: NO blind swap-back! Identify `claim_tmp` by hash -> bin or conflict copy,
    // then verify `live_path` (re-hashing since Windows `file_identity` is `None`).
    // Explicitly handles all three documented `ReplaceFileW` partial-failure codes
    // (`ERROR_UNABLE_TO_REMOVE_REPLACED` 1175, `ERROR_UNABLE_TO_MOVE_REPLACEMENT` 1176,
    // `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` 1177) plus sharing/delete-pending errors so user bytes
    // are never lost and a locked file defers cleanly without aborting the page.
    match fs.replace_live_with_tmp_backup(staged_tmp, live_path, claim_tmp) {
        Ok(true) => {
            let (matched, precomputed) =
                check_tmp_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)?;
            let binned = if matched {
                finalize_binned_tmp_and_rescue(fs, store, claim_tmp, live_path, bin_entry, now)?
            } else {
                preserve_tmp_as_conflict_copy_with_hash(
                    fs,
                    store,
                    claim_tmp,
                    live_path,
                    precomputed,
                    now,
                )?;
                false
            };
            if verify_and_record_placed_stat(fs, live_path, target, staged_meta, true, now)? {
                return Ok(SafeReplaceResult::Replaced { binned });
            }
            return Ok(SafeReplaceResult::LiveChangedAfterDisplace { binned });
        }
        Ok(false) => {}
        Err(err) if is_replace_partial_failure(&err) || is_live_busy_error(fs, &err) => {
            return recover_replace_partial_failure(
                fs,
                store,
                staged_tmp,
                claim_tmp,
                live_path,
                expected_old_hash,
                expected_old_size,
                bin_entry,
                target,
                now,
            );
        }
        Err(err) if is_mid_apply_race_error(&err) => return Ok(SafeReplaceResult::RaceBeforePlace),
        Err(err) => return Err(err),
    }

    // Path 3: claim-then-rename fallback (safe against race edits AND race creates).
    match fs.move_live_to_tmp_no_replace(live_path, claim_tmp) {
        Ok(()) => {}
        Err(err) if is_mid_apply_race_error(&err) => return Ok(SafeReplaceResult::RaceBeforePlace),
        Err(err) if is_live_busy_error(fs, &err) => {
            if fs.stat_tmp(staged_tmp)?.is_some()
                && tmp_matches_hash(fs, staged_tmp, target.hash, target.size)?
            {
                let _ = fs.remove_tmp(staged_tmp);
            }
            return Ok(SafeReplaceResult::Deferred {
                reason: busy_defer_reason(live_path),
                binned: false,
            });
        }
        Err(err) => return Err(err),
    }

    let (matched, precomputed) =
        check_tmp_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)?;
    if !matched {
        // Put-back result is ALWAYS handled: if the person already created a newer file at
        // `live_path` (or `live_path` is locked/delete-pending), preserve `claim_tmp` as a
        // conflict copy instead of leaving it in tmp.
        put_back_or_preserve_tmp(fs, store, claim_tmp, live_path, precomputed, now)?;
        return Ok(SafeReplaceResult::RaceBeforePlace);
    }

    let binned = finalize_binned_tmp_and_rescue(fs, store, claim_tmp, live_path, bin_entry, now)?;

    match fs.commit_tmp_to_live_no_replace(staged_tmp, live_path) {
        Ok(()) => {
            if verify_and_record_placed_stat(fs, live_path, target, staged_meta, true, now)? {
                Ok(SafeReplaceResult::Replaced { binned })
            } else {
                Ok(SafeReplaceResult::LiveChangedAfterDisplace { binned })
            }
        }
        Err(err) if is_mid_apply_race_error(&err) => {
            // `staged_tmp` was never placed; it is still our staged download.
            let _ = fs.remove_tmp(staged_tmp);
            Ok(SafeReplaceResult::LiveChangedAfterDisplace { binned })
        }
        Err(err) if is_live_busy_error(fs, &err) => {
            if fs.stat_tmp(staged_tmp)?.is_some()
                && tmp_matches_hash(fs, staged_tmp, target.hash, target.size)?
            {
                let _ = fs.remove_tmp(staged_tmp);
            }
            // Old file was moved to bin before `live_path` became blocked; try restoring it so
            // `live_path` is not left absent on disk.
            if binned
                && !bin_entry.storage_name.is_empty()
                && fs.stat_bin(&bin_entry.storage_name)?.is_some()
            {
                let rt = alloc_tmp_name_for(fs, "putback", live_path)?;
                register_staged_tmp(store, &rt)?;
                let restored = if fs.copy_bin_to_tmp(&bin_entry.storage_name, &rt).is_ok() {
                    let _ = prepare_staged_tmp_metadata(
                        fs,
                        &rt,
                        bin_entry.original_mtime,
                        bin_entry.mode,
                    );
                    match fs.commit_tmp_to_live_no_replace(&rt, live_path) {
                        Ok(()) => {
                            let _ = fs.remove_bin_file(&bin_entry.storage_name);
                            let _ = fs.remove_bin_file(&bin_meta_name(&bin_entry.id));
                            Some(Ok(()))
                        }
                        Err(e) if is_mid_apply_race_error(&e) => {
                            let _ = fs.remove_tmp(&rt);
                            Some(Err(true))
                        }
                        Err(_) => {
                            let _ = fs.remove_tmp(&rt);
                            Some(Err(false))
                        }
                    }
                } else {
                    let _ = fs.remove_tmp(&rt);
                    None
                };
                unregister_staged_tmp(store, &rt)?;
                match restored {
                    Some(Ok(())) => {
                        return Ok(SafeReplaceResult::Deferred {
                            reason: busy_defer_reason(live_path),
                            binned: false,
                        });
                    }
                    Some(Err(true)) => {
                        return Ok(SafeReplaceResult::LiveChangedAfterDisplace { binned: true });
                    }
                    _ => {}
                }
            }
            Ok(SafeReplaceResult::Deferred {
                reason: busy_defer_reason(live_path),
                binned,
            })
        }
        Err(err) => Err(err),
    }
}

/// Inspects on-disk state after a Windows `ReplaceFileW` partial failure (`1175`, `1176`, `1177`)
/// or sharing violation, ensuring displaced bytes in `claim_tmp` are restored to `live_path`,
/// binned if a newer file took `live_path`, or preserved as a live conflict copy — never lost.
#[allow(clippy::too_many_arguments)]
fn recover_replace_partial_failure<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    staged_tmp: &str,
    claim_tmp: &str,
    live_path: &str,
    expected_old_hash: Hash,
    expected_old_size: u64,
    bin_entry: &mut BinEntry,
    target: &IndexRecord,
    now: Timestamp,
) -> Result<SafeReplaceResult> {
    if fs.stat_tmp(staged_tmp)?.is_some() {
        let (matched, precomputed) = check_tmp_hash(fs, staged_tmp, target.hash, target.size)?;
        if matched {
            let _ = fs.remove_tmp(staged_tmp);
        } else {
            preserve_tmp_as_conflict_copy_with_hash(
                fs,
                store,
                staged_tmp,
                live_path,
                precomputed,
                now,
            )?;
        }
    }

    if fs.stat_tmp(claim_tmp)?.is_none() {
        return Ok(SafeReplaceResult::Deferred {
            reason: busy_defer_reason(live_path),
            binned: false,
        });
    }

    let (matches_expected_old, precomputed_claim) =
        check_tmp_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)?;

    match fs.commit_tmp_to_live_no_replace(claim_tmp, live_path) {
        Ok(()) => {
            if matches_expected_old {
                Ok(SafeReplaceResult::Deferred {
                    reason: busy_defer_reason(live_path),
                    binned: false,
                })
            } else {
                Ok(SafeReplaceResult::RaceBeforePlace)
            }
        }
        Err(err) if is_mid_apply_race_error(&err) => {
            let binned = if matches_expected_old {
                finalize_binned_tmp_and_rescue(fs, store, claim_tmp, live_path, bin_entry, now)?
            } else {
                preserve_tmp_as_conflict_copy_with_hash(
                    fs,
                    store,
                    claim_tmp,
                    live_path,
                    precomputed_claim,
                    now,
                )?;
                false
            };
            Ok(SafeReplaceResult::LiveChangedAfterDisplace { binned })
        }
        Err(err) if is_live_busy_error(fs, &err) => {
            // `live_path` cannot accept the put-back (for example delete-pending on Windows).
            // Preserve `claim_tmp` as a live conflict copy so zero user bytes can be lost; if
            // `claim_tmp` itself is locked, propagating `Err` keeps `PendingOp::Place` armed.
            preserve_tmp_as_conflict_copy_with_hash(
                fs,
                store,
                claim_tmp,
                live_path,
                precomputed_claim,
                now,
            )?;
            Ok(SafeReplaceResult::Deferred {
                reason: busy_defer_reason(live_path),
                binned: false,
            })
        }
        Err(err) => Err(err),
    }
}

#[allow(clippy::too_many_arguments)]
fn safe_delete_live_file<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    live_path: &str,
    claim_tmp: &str,
    expected_old_hash: Hash,
    expected_old_size: u64,
    bin_entry: &mut BinEntry,
    now: Timestamp,
) -> Result<SafeDeleteResult> {
    match fs.move_live_to_tmp_no_replace(live_path, claim_tmp) {
        Ok(()) => {}
        Err(err) if is_mid_apply_race_error(&err) => return Ok(SafeDeleteResult::RaceChanged),
        Err(err) if is_live_busy_error(fs, &err) => {
            return Ok(SafeDeleteResult::Deferred(busy_defer_reason(live_path)));
        }
        Err(err) => return Err(err),
    }

    let (matched, precomputed) =
        check_tmp_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)?;
    if !matched {
        put_back_or_preserve_tmp(fs, store, claim_tmp, live_path, precomputed, now)?;
        return Ok(SafeDeleteResult::RaceChanged);
    }

    let binned = finalize_binned_tmp_and_rescue(fs, store, claim_tmp, live_path, bin_entry, now)?;
    Ok(SafeDeleteResult::Deleted { binned })
}

fn recover_on_non_crash_error<F: SyncFs, S: IndexStore, T>(
    fs: &F,
    store: &mut S,
    result: Result<T>,
    now: Timestamp,
) -> Result<T> {
    match result {
        Ok(val) => Ok(val),
        Err(err) => {
            if !err.is_crash() {
                let _ = recover_pending_at(fs, store, now);
            }
            Err(err)
        }
    }
}

fn resolve_restore_destination<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &S,
    base_dest: &str,
    strategy: RestoreStrategy,
    now: Timestamp,
) -> Result<String> {
    let exists = fs.stat_live(base_dest)?.is_some()
        || store.get_record(base_dest)?.is_some_and(|r| !r.deleted);
    if !exists {
        return Ok(base_dest.to_string());
    }
    match strategy {
        RestoreStrategy::FailIfExists => Err(SyncError::Exists {
            path: base_dest.to_string(),
        }),
        RestoreStrategy::Replace => Ok(base_dest.to_string()),
        RestoreStrategy::Rename => {
            let primary = restore_copy_path(base_dest, now, None);
            if fs.stat_live(&primary)?.is_none()
                && store.get_record(&primary)?.is_none_or(|r| r.deleted)
            {
                return Ok(primary);
            }
            for n in 2u32..10_000 {
                let tag = n.to_string();
                let alt = restore_copy_path(base_dest, now, Some(&tag));
                if fs.stat_live(&alt)?.is_none()
                    && store.get_record(&alt)?.is_none_or(|r| r.deleted)
                {
                    return Ok(alt);
                }
            }
            Err(SyncError::Exists { path: primary })
        }
    }
}

fn is_peer_file_changed_error(err: &SyncError) -> bool {
    matches!(
        err,
        SyncError::NotFound { .. }
            | SyncError::HashMismatch { .. }
            | SyncError::SizeMismatch { .. }
            | SyncError::Conflict { .. }
            | SyncError::Fs {
                kind: FsErrorKind::NotFound,
                ..
            }
    )
}

/// Fetches `remote_path` into `.legion-sync/tmp/` (or beside `local_dest_path` when it resides
/// on a different mount/subvolume), registering the tmp filename in `STAGED_TMPS` BEFORE
/// creation, capping reads strictly at `expected_size + 1` bytes, enforcing idle timeouts &
/// cancellation, supporting ranged resume on mid-stream interruptions, and verifying the
/// full-file BLAKE3 digest.
#[allow(clippy::too_many_arguments)]
fn fetch_and_verify_to_tmp<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    remote_path: &str,
    local_dest_path: &str,
    expected_size: u64,
    expected_hash: Hash,
    base_opts: &FetchOptions,
) -> Result<String> {
    let tmp_name = alloc_tmp_name_for(fs, "fetch", local_dest_path)?;
    register_staged_tmp(store, &tmp_name)?;
    let writer = match fetch_and_verify_into_named_tmp_staged(
        fs,
        peer,
        remote_path,
        &tmp_name,
        expected_size,
        expected_hash,
        base_opts,
    ) {
        Ok(w) => w,
        Err(err) => {
            if !err.is_crash() {
                let _ = unregister_staged_tmp(store, &tmp_name);
            }
            return Err(err);
        }
    };
    if let Err(err) = writer.finish_sync() {
        let _ = fs.remove_tmp(&tmp_name);
        if !err.is_crash() {
            let _ = unregister_staged_tmp(store, &tmp_name);
        }
        return Err(err);
    }
    Ok(tmp_name)
}

enum ProgressClock<'a> {
    Real(Instant),
    Virtual {
        clock: &'a FetchClock,
        last: Duration,
    },
}

impl<'a> ProgressClock<'a> {
    fn start(clock: Option<&'a FetchClock>) -> Self {
        match clock {
            Some(c) => Self::Virtual {
                clock: c,
                last: c.now(),
            },
            None => Self::Real(Instant::now()),
        }
    }

    fn elapsed(&self) -> Duration {
        match self {
            Self::Real(t) => t.elapsed(),
            Self::Virtual { clock, last } => clock.now().saturating_sub(*last),
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Real(t) => *t = Instant::now(),
            Self::Virtual { clock, last } => *last = clock.now(),
        }
    }
}

fn fetch_and_verify_into_named_tmp_staged<F: SyncFs>(
    fs: &F,
    peer: &dyn Peer,
    remote_path: &str,
    tmp_name: &str,
    expected_size: u64,
    expected_hash: Hash,
    base_opts: &FetchOptions,
) -> Result<Box<dyn SyncWrite>> {
    let mut last_progress = ProgressClock::start(base_opts.clock.as_ref());
    let max_bytes = expected_size.saturating_add(1);
    let mut hasher = blake3::Hasher::new();
    let mut bytes_written: u64 = 0;
    let mut resume_attempts: usize = 0;
    let mut small_buf = [0u8; 512];
    let mut large_buf: Vec<u8> = if max_bytes > small_buf.len() as u64 {
        vec![0u8; (max_bytes as usize).min(64 * 1024)]
    } else {
        Vec::new()
    };
    let buf: &mut [u8] = if large_buf.is_empty() {
        &mut small_buf
    } else {
        &mut large_buf
    };

    let final_writer = 'outer: loop {
        if base_opts.is_cancelled() {
            let _ = fs.remove_tmp(tmp_name);
            return Err(SyncError::Cancelled {
                operation: format!("fetch {remote_path}"),
            });
        }
        let idle = last_progress.elapsed();
        if idle > base_opts.timeout {
            let _ = fs.remove_tmp(tmp_name);
            return Err(SyncError::Timeout {
                operation: format!("fetch {remote_path}"),
                detail: format!("stalled for more than {:?}", base_opts.timeout),
            });
        }
        let remaining_timeout = base_opts.timeout.saturating_sub(idle);
        let mut opts =
            FetchOptions::at_offset(bytes_written, remaining_timeout, base_opts.cancel.clone());
        opts.clock = base_opts.clock.clone();

        let mut reader = match peer.fetch_file(remote_path, expected_hash, &opts) {
            Ok(r) => r,
            Err(err) => {
                let _ = fs.remove_tmp(tmp_name);
                return Err(err);
            }
        };

        let mut writer = if bytes_written == 0 {
            fs.create_tmp(tmp_name)?
        } else {
            match fs.open_tmp_append(tmp_name) {
                Ok(w) => w,
                Err(err) => {
                    let _ = fs.remove_tmp(tmp_name);
                    return Err(err);
                }
            }
        };

        loop {
            if base_opts.is_cancelled() {
                drop(writer);
                let _ = fs.remove_tmp(tmp_name);
                return Err(SyncError::Cancelled {
                    operation: format!("fetch {remote_path}"),
                });
            }
            if last_progress.elapsed() > base_opts.timeout {
                drop(writer);
                let _ = fs.remove_tmp(tmp_name);
                return Err(SyncError::Timeout {
                    operation: format!("fetch {remote_path}"),
                    detail: format!("stalled for more than {:?}", base_opts.timeout),
                });
            }

            let remaining_cap = max_bytes.saturating_sub(bytes_written);
            if remaining_cap == 0 {
                // Already read `expected_size + 1` bytes — stop immediately without reading more!
                drop(writer);
                let _ = fs.remove_tmp(tmp_name);
                peer.report_hash_mismatch(remote_path);
                return Err(SyncError::SizeMismatch {
                    path: remote_path.to_string(),
                    expected: expected_size,
                    actual: bytes_written,
                });
            }

            let to_read = (remaining_cap as usize).min(buf.len());
            match reader.read(&mut buf[..to_read]) {
                Ok(0) => {
                    if last_progress.elapsed() > base_opts.timeout {
                        drop(writer);
                        let _ = fs.remove_tmp(tmp_name);
                        return Err(SyncError::Timeout {
                            operation: format!("fetch {remote_path}"),
                            detail: format!("stalled for more than {:?}", base_opts.timeout),
                        });
                    }
                    break;
                }
                Ok(n) => {
                    if last_progress.elapsed() > base_opts.timeout {
                        drop(writer);
                        let _ = fs.remove_tmp(tmp_name);
                        return Err(SyncError::Timeout {
                            operation: format!("fetch {remote_path}"),
                            detail: format!("stalled for more than {:?}", base_opts.timeout),
                        });
                    }
                    last_progress.reset();
                    if let Err(err) = writer.write_all(&buf[..n]) {
                        drop(writer);
                        let _ = fs.remove_tmp(tmp_name);
                        return Err(SyncError::from_io(Some(remote_path.to_string()), err));
                    }
                    hasher.update(&buf[..n]);
                    bytes_written = bytes_written.saturating_add(n as u64);
                    if bytes_written > expected_size {
                        drop(writer);
                        let _ = fs.remove_tmp(tmp_name);
                        peer.report_hash_mismatch(remote_path);
                        return Err(SyncError::SizeMismatch {
                            path: remote_path.to_string(),
                            expected: expected_size,
                            actual: bytes_written,
                        });
                    }
                }
                Err(err) => {
                    if last_progress.elapsed() > base_opts.timeout {
                        drop(writer);
                        let _ = fs.remove_tmp(tmp_name);
                        return Err(SyncError::Timeout {
                            operation: format!("fetch {remote_path}"),
                            detail: format!("stalled for more than {:?}", base_opts.timeout),
                        });
                    }
                    if bytes_written > 0 && resume_attempts < MAX_FETCH_RESUMES {
                        // Sync partial bytes and attempt a ranged resume from `bytes_written`.
                        let _ = writer.finish_sync();
                        resume_attempts += 1;
                        continue 'outer;
                    }
                    drop(writer);
                    let _ = fs.remove_tmp(tmp_name);
                    return Err(SyncError::PeerUnavailable {
                        member: peer.member_id().to_string(),
                        detail: format!("failed reading '{remote_path}': {err}"),
                    });
                }
            }
        }

        if let Err(err) = writer.flush() {
            drop(writer);
            let _ = fs.remove_tmp(tmp_name);
            return Err(SyncError::from_io(Some(remote_path.to_string()), err));
        }
        break writer;
    };

    if bytes_written != expected_size {
        drop(final_writer);
        let _ = fs.remove_tmp(tmp_name);
        peer.report_hash_mismatch(remote_path);
        return Err(SyncError::SizeMismatch {
            path: remote_path.to_string(),
            expected: expected_size,
            actual: bytes_written,
        });
    }

    let actual_hash = Hash::from_bytes(*hasher.finalize().as_bytes());
    if actual_hash != expected_hash {
        drop(final_writer);
        let _ = fs.remove_tmp(tmp_name);
        peer.report_hash_mismatch(remote_path);
        return Err(SyncError::HashMismatch {
            path: remote_path.to_string(),
            expected: expected_hash,
            actual: actual_hash,
        });
    }

    Ok(final_writer)
}

fn handle_local_changed<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    path: &str,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    match rescan_single_path(fs, store, path, now)? {
        SingleRescanOutcome::Updated(_) | SingleRescanOutcome::Unchanged => {
            Ok(ApplyOutcome::LocalChangedRetry)
        }
        SingleRescanOutcome::Unstable => Ok(ApplyOutcome::Deferred {
            reason: format!("'{path}' is actively changing on disk"),
        }),
    }
}

fn handle_local_changed_after_displace<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    path: &str,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    match rescan_single_path_with_force(fs, store, path, now, true)? {
        SingleRescanOutcome::Updated(_) | SingleRescanOutcome::Unchanged => {
            Ok(ApplyOutcome::LocalChangedRetry)
        }
        SingleRescanOutcome::Unstable => Ok(ApplyOutcome::Deferred {
            reason: format!("'{path}' is actively changing on disk"),
        }),
    }
}

fn build_bin_entry(
    existing: Option<&IndexRecord>,
    reason: BinReason,
    by_member: &str,
    now: Timestamp,
) -> Option<BinEntry> {
    let rec = existing?;
    if rec.deleted || rec.kind == EntryKind::Dir {
        return None;
    }
    Some(build_raw_bin_entry(
        &rec.path,
        rec.kind,
        rec.size,
        rec.mtime,
        rec.mode,
        rec.hash,
        rec.link_target.clone(),
        reason,
        by_member,
        now,
    ))
}

#[allow(clippy::too_many_arguments)]
fn build_raw_bin_entry(
    path: &str,
    kind: EntryKind,
    size: u64,
    mtime: Timestamp,
    mode: Option<u32>,
    hash: Option<Hash>,
    link_target: Option<String>,
    reason: BinReason,
    by_member: &str,
    now: Timestamp,
) -> BinEntry {
    // Item G: random token ensures bin IDs are unique across restarts and clock jumps.
    let token = random_hex_token();
    let short = hash
        .map(|h| h.to_hex()[..8].to_string())
        .unwrap_or_else(|| "link".to_string());
    let id = format!("{}-{token}-{short}", now.secs);
    let storage_name = if kind == EntryKind::File {
        format!("{id}.data")
    } else {
        String::new()
    };
    BinEntry {
        id,
        original_path: path.to_string(),
        kind,
        size,
        original_mtime: mtime,
        mode,
        hash,
        link_target,
        deleted_at: now,
        reason,
        by_member: by_member.to_string(),
        storage_name,
        binned_mtime: None,
        binned_change_id: None,
    }
}

fn write_bin_meta_file<F: SyncFs>(fs: &F, entry: &BinEntry) -> Result<()> {
    let meta_bytes = serde_json::to_vec(entry).map_err(|err| SyncError::Storage {
        context: "serialize bin metadata".to_string(),
        detail: err.to_string(),
    })?;
    fs.write_bin_meta(&bin_meta_name(&entry.id), &meta_bytes)
}

fn snapshot_local(existing: Option<&IndexRecord>) -> ExpectedLocalSnapshot {
    let Some(rec) = existing else {
        return ExpectedLocalSnapshot::Missing;
    };
    if rec.deleted {
        return ExpectedLocalSnapshot::Missing;
    }
    match rec.kind {
        EntryKind::File => match rec.hash {
            Some(hash) => ExpectedLocalSnapshot::File {
                size: rec.size,
                mtime: rec.disk_mtime.unwrap_or(rec.mtime),
                hash,
            },
            None => ExpectedLocalSnapshot::Missing,
        },
        EntryKind::Dir => ExpectedLocalSnapshot::Dir,
        EntryKind::Symlink => ExpectedLocalSnapshot::Symlink {
            link_target: rec.link_target.clone().unwrap_or_default(),
        },
    }
}

fn disk_matches_snapshot<F: SyncFs>(
    fs: &F,
    path: &str,
    snapshot: &ExpectedLocalSnapshot,
) -> Result<bool> {
    let meta = fs.stat_live(path)?;
    match (snapshot, meta) {
        (ExpectedLocalSnapshot::Missing, None) => Ok(true),
        (
            ExpectedLocalSnapshot::Dir,
            Some(FsMeta {
                kind: EntryKind::Dir,
                ..
            }),
        ) => Ok(true),
        (
            ExpectedLocalSnapshot::Symlink { link_target },
            Some(FsMeta {
                kind: EntryKind::Symlink,
                link_target: Some(ref actual),
                ..
            }),
        ) => Ok(actual == link_target),
        (
            ExpectedLocalSnapshot::File { size, hash, .. },
            Some(FsMeta {
                kind: EntryKind::File,
                size: actual_size,
                ..
            }),
        ) => {
            if actual_size != *size {
                return Ok(false);
            }
            let (actual_hash, read_len) = match fs.hash_live(path) {
                Ok(pair) => pair,
                Err(err) if is_mid_apply_race_error(&err) => return Ok(false),
                Err(err) => return Err(err),
            };
            Ok(read_len == *size && actual_hash == *hash)
        }
        _ => Ok(false),
    }
}

fn tmp_matches_snapshot<F: SyncFs>(
    fs: &F,
    tmp_name: &str,
    snapshot: &ExpectedLocalSnapshot,
) -> Result<bool> {
    match snapshot {
        ExpectedLocalSnapshot::File { size, hash, .. } => {
            tmp_matches_hash(fs, tmp_name, Some(*hash), *size)
        }
        _ => Ok(false),
    }
}

fn disk_matches_record<F: SyncFs>(fs: &F, path: &str, rec: &IndexRecord) -> Result<bool> {
    let meta = fs.stat_live(path)?;
    if rec.deleted {
        return Ok(meta.is_none());
    }
    let Some(m) = meta else {
        return Ok(false);
    };
    if m.kind != rec.kind {
        return Ok(false);
    }
    match rec.kind {
        EntryKind::Dir => Ok(true),
        EntryKind::Symlink => Ok(m.link_target == rec.link_target),
        EntryKind::File => {
            if m.size != rec.size {
                return Ok(false);
            }
            let (actual_hash, read_len) = match fs.hash_live(path) {
                Ok(pair) => pair,
                Err(err) if is_mid_apply_race_error(&err) => return Ok(false),
                Err(err) => return Err(err),
            };
            Ok(read_len == rec.size && rec.hash == Some(actual_hash))
        }
    }
}

fn check_tmp_hash<F: SyncFs>(
    fs: &F,
    tmp_name: &str,
    expected_hash: Option<Hash>,
    expected_size: u64,
) -> Result<(bool, Option<(Hash, u64)>)> {
    if fs.stat_tmp(tmp_name)?.is_none() {
        return Ok((false, None));
    }
    let (actual_hash, actual_size) = fs.hash_tmp(tmp_name)?;
    let matched = expected_hash == Some(actual_hash) && actual_size == expected_size;
    Ok((matched, Some((actual_hash, actual_size))))
}

fn tmp_matches_hash<F: SyncFs>(
    fs: &F,
    tmp_name: &str,
    expected_hash: Option<Hash>,
    expected_size: u64,
) -> Result<bool> {
    let Some(expected) = expected_hash else {
        return Ok(false);
    };
    Ok(check_tmp_hash(fs, tmp_name, Some(expected), expected_size)?.0)
}

fn cleanup_staged_tmps<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    candidates: &[Option<&str>],
) -> Result<()> {
    let mut removed = Vec::new();
    for cand in candidates.iter().flatten() {
        let _ = fs.remove_tmp(cand);
        removed.push((*cand).to_string());
    }
    if !removed.is_empty() {
        store.apply_batch(StoreBatch {
            remove_staged_tmps: removed,
            ..StoreBatch::default()
        })?;
    }
    Ok(())
}

fn register_staged_tmp<S: IndexStore>(store: &mut S, tmp_name: &str) -> Result<()> {
    store.apply_batch(StoreBatch {
        add_staged_tmps: vec![tmp_name.to_string()],
        ..StoreBatch::default()
    })
}

fn unregister_staged_tmp<S: IndexStore>(store: &mut S, tmp_name: &str) -> Result<()> {
    store.apply_batch(StoreBatch {
        remove_staged_tmps: vec![tmp_name.to_string()],
        ..StoreBatch::default()
    })
}

fn write_pending_op_and_unregister_tmps<S: IndexStore>(
    store: &mut S,
    op: PendingOp,
    remove_staged_tmps: Vec<String>,
) -> Result<()> {
    let batch = StoreBatch {
        set_pending_op: Some(Some(op)),
        remove_staged_tmps,
        ..StoreBatch::default()
    };
    store.apply_batch(batch)
}

fn mark_needs_rescan<S: IndexStore>(store: &mut S, paths: &[&str]) -> Result<()> {
    let mut meta = load_meta_required(store)?;
    let mut changed = false;
    for &p in paths {
        if meta.needs_rescan.insert(p.to_string()) {
            changed = true;
        }
    }
    if changed {
        store.apply_batch(StoreBatch {
            folder_meta: Some(meta),
            ..StoreBatch::default()
        })?;
    }
    Ok(())
}

fn mark_needs_rescan_and_clear_pending<S: IndexStore>(
    store: &mut S,
    paths: &[&str],
    upsert_bin: Vec<BinEntry>,
) -> Result<()> {
    let mut meta = load_meta_required(store)?;
    for &p in paths {
        meta.needs_rescan.insert(p.to_string());
    }
    store.apply_batch(StoreBatch {
        folder_meta: Some(meta),
        upsert_bin,
        set_pending_op: Some(None),
        ..StoreBatch::default()
    })
}

fn prepare_staged_tmp_metadata<F: SyncFs>(
    fs: &F,
    tmp_name: &str,
    mtime: Timestamp,
    mode: Option<u32>,
) -> Result<Option<FsMeta>> {
    match fs.set_tmp_metadata(tmp_name, mtime, mode) {
        Ok(()) => {}
        Err(err) if err.is_crash() => return Err(err),
        Err(_) => {}
    }
    fs.stat_tmp(tmp_name)
}

fn is_within_recent_wall_clock(ts: Timestamp, window_secs: i64) -> bool {
    let wall = current_timestamp();
    ts.secs.abs_diff(wall.secs) <= window_secs.unsigned_abs()
}

fn verify_and_record_placed_stat<F: SyncFs>(
    fs: &F,
    live_path: &str,
    target: &mut IndexRecord,
    staged_meta: Option<&FsMeta>,
    verify_recent_mtime: bool,
    now: Timestamp,
) -> Result<bool> {
    let Some(live_meta) = fs.stat_live(live_path)? else {
        return Ok(false);
    };
    if live_meta.kind != EntryKind::File || live_meta.size != target.size {
        return Ok(false);
    }
    let Some(expected_hash) = target.hash else {
        return Ok(false);
    };

    let mut need_hash_check = true;
    if let Some(sm) = staged_meta
        && let (Some(staged_id), Some(live_id)) = (sm.file_identity, live_meta.file_identity)
    {
        if live_id != staged_id || live_meta.mtime != sm.mtime {
            return Ok(false);
        }
        // Same device+inode and unchanged mtime after pre-placement metadata. If `mtime` is
        // close to current clock time (where an immediate in-place overwrite of the same
        // inode could share the same coarse kernel mtime tick), confirm the bytes by hash too.
        need_hash_check = verify_recent_mtime
            && (live_meta.mtime.secs.abs_diff(now.secs) <= 2
                || is_within_recent_wall_clock(live_meta.mtime, 2));
    }

    if need_hash_check {
        let (actual_hash, actual_size) = match fs.hash_live(live_path) {
            Ok(pair) => pair,
            Err(err) if err.is_crash() => return Err(err),
            Err(_) => return Ok(false),
        };
        if actual_hash != expected_hash || actual_size != target.size {
            return Ok(false);
        }
        let Some(after_meta) = fs.stat_live(live_path)? else {
            return Ok(false);
        };
        if after_meta.kind != EntryKind::File
            || after_meta.size != live_meta.size
            || after_meta.mtime != live_meta.mtime
            || after_meta.change_id != live_meta.change_id
            || after_meta.file_identity != live_meta.file_identity
        {
            return Ok(false);
        }
    }

    target.disk_mtime = Some(live_meta.mtime);
    target.disk_change_id = live_meta.change_id;
    Ok(true)
}

/// Core principle helper: puts `tmp_name` back at `live_path` via non-replacing rename; if a
/// person already wrote a newer file at `live_path`, preserves `tmp_name` as a conflict copy
/// beside `live_path` so neither save is ever lost or deleted.
fn put_back_or_preserve_tmp<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    tmp_name: &str,
    live_path: &str,
    precomputed_hash: Option<(Hash, u64)>,
    now: Timestamp,
) -> Result<()> {
    match fs.commit_tmp_to_live_no_replace(tmp_name, live_path) {
        Ok(()) => Ok(()),
        Err(err) if is_mid_apply_race_error(&err) || is_live_busy_error(fs, &err) => {
            preserve_tmp_as_conflict_copy_with_hash(
                fs,
                store,
                tmp_name,
                live_path,
                precomputed_hash,
                now,
            )?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Moves an unseen user file from `.legion-sync/tmp/<tmp_name>` into the live tree as a conflict
/// copy beside `original_path` (`name (changed on <local_member> <date>, <hash6>).ext`), indexes
/// it with a fresh independent version vector, and NEVER deletes its bytes.
fn preserve_tmp_as_conflict_copy<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    tmp_name: &str,
    original_path: &str,
    now: Timestamp,
) -> Result<IndexRecord> {
    preserve_tmp_as_conflict_copy_with_hash(fs, store, tmp_name, original_path, None, now)
}

fn preserve_tmp_as_conflict_copy_with_hash<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    tmp_name: &str,
    original_path: &str,
    precomputed_hash: Option<(Hash, u64)>,
    now: Timestamp,
) -> Result<IndexRecord> {
    let tmp_meta = fs.stat_tmp(tmp_name)?;
    let (actual_hash, actual_size) = match precomputed_hash {
        Some(pair) => pair,
        None => fs.hash_tmp(tmp_name)?,
    };
    let mut meta = load_meta_required(store)?;
    let hex = actual_hash.to_hex();
    let hash6 = &hex[..6.min(hex.len())];
    let file_mtime = tmp_meta.as_ref().map_or(now, |m| m.mtime);
    let file_mode = tmp_meta.as_ref().and_then(|m| m.mode);

    let mut chosen_path = None;
    let mut base_path = original_path;
    let basename = original_path.rsplit('/').next().unwrap_or("recovered-file");
    for attempt in 0u32..10_000 {
        let disambig = if attempt == 0 {
            None
        } else {
            Some((attempt + 1).to_string())
        };
        let cand = conflict_copy_path(
            base_path,
            &meta.local_member,
            file_mtime,
            hash6,
            disambig.as_deref(),
        );
        match fs.commit_tmp_to_live_no_replace(tmp_name, &cand) {
            Ok(()) => {
                chosen_path = Some(cand);
                break;
            }
            Err(SyncError::Fs {
                kind: FsErrorKind::AlreadyExists,
                ..
            }) => continue,
            Err(err) if is_mid_apply_race_error(&err) && base_path != basename => {
                // An ancestor of `original_path` was replaced with a non-directory concurrently;
                // fall back to placing the rescued conflict copy at the folder root so bytes are
                // never lost.
                base_path = basename;
                continue;
            }
            Err(err) => return Err(err),
        }
    }

    let conflict_path = match chosen_path {
        Some(p) => p,
        None => {
            let token = random_hex_token();
            let cand = conflict_copy_path(
                base_path,
                &meta.local_member,
                file_mtime,
                hash6,
                Some(&token[..8]),
            );
            fs.commit_tmp_to_live_no_replace(tmp_name, &cand)?;
            cand
        }
    };

    let live_stat = fs.stat_live(&conflict_path)?;
    let vkey = meta.version_key();
    let clock = meta.next_clock();
    let seq = meta.next_seq();
    let mut rec = IndexRecord {
        path: conflict_path.clone(),
        kind: EntryKind::File,
        size: actual_size,
        mtime: file_mtime,
        disk_mtime: live_stat.as_ref().map(|m| m.mtime).or(Some(file_mtime)),
        disk_change_id: live_stat.as_ref().and_then(|m| m.change_id),
        mode: file_mode,
        hash: Some(actual_hash),
        link_target: None,
        deleted: false,
        version: VersionVector::singleton(vkey, clock),
        modified_by: meta.local_member.clone(),
        seq,
        recent_hashes: Vec::new(),
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    };
    rec.conflict_of = derive_conflict_of(&rec, &meta).or_else(|| Some(original_path.to_string()));
    let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
    let batch = StoreBatch {
        folder_meta: Some(meta),
        upsert_records: vec![rec.clone()],
        remove_staged_tmps: vec![tmp_name.to_string()],
        upsert_seq_fingerprints: vec![(seq, chain)],
        ..StoreBatch::default()
    };
    store.apply_batch(batch)?;
    Ok(rec)
}

/// Applies the core principle to one named tmp file during crash recovery:
/// - Matches `expected_local` -> move to `.legion-sync/bin` (returns `Ok(true)` if binned).
/// - Matches `expected_staged` (engine-written staged download) -> unlink & unregister.
/// - Matches neither -> unseen person's save -> preserve as a conflict copy beside `live_path`.
#[allow(clippy::too_many_arguments)]
fn salvage_pending_tmp<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    tmp_name: &str,
    live_path: &str,
    expected_local: &ExpectedLocalSnapshot,
    bin_entry: Option<&BinEntry>,
    expected_staged: Option<(Hash, u64)>,
    now: Timestamp,
) -> Result<bool> {
    if fs.stat_tmp(tmp_name)?.is_none() {
        return Ok(false);
    }
    let (actual_hash, actual_size) = fs.hash_tmp(tmp_name)?;
    if matches!(
        expected_local,
        ExpectedLocalSnapshot::File { hash, size, .. }
            if *hash == actual_hash && *size == actual_size
    ) && let Some(b) = bin_entry
        && !b.storage_name.is_empty()
        && fs.stat_bin(&b.storage_name)?.is_none()
    {
        write_bin_meta_file(fs, b)?;
        fs.move_tmp_to_bin_no_replace(tmp_name, &b.storage_name)?;
        return Ok(true);
    }
    if expected_staged == Some((actual_hash, actual_size)) {
        let _ = fs.remove_tmp(tmp_name);
        let _ = unregister_staged_tmp(store, tmp_name);
        return Ok(false);
    }
    preserve_tmp_as_conflict_copy_with_hash(
        fs,
        store,
        tmp_name,
        live_path,
        Some((actual_hash, actual_size)),
        now,
    )?;
    Ok(false)
}

/// Rescues any unexpected file left in `.legion-sync/tmp/` that is NOT proven to be an
/// engine-written staged download by moving it into the live tree as a conflict copy.
fn rescue_orphan_tmp<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    tmp_name: &str,
    now: Timestamp,
) -> Result<()> {
    if fs.stat_tmp(tmp_name)?.is_none() {
        return Ok(());
    }
    let _ = preserve_tmp_as_conflict_copy(fs, store, tmp_name, "recovered-file", now)?;
    Ok(())
}

fn current_timestamp() -> Timestamp {
    Timestamp::from_system_time(SystemTime::now())
}

fn bin_entry_if_present<F: SyncFs>(
    fs: &F,
    bin_entry: Option<BinEntry>,
) -> Result<Option<BinEntry>> {
    let Some(entry) = bin_entry else {
        return Ok(None);
    };
    if !entry.storage_name.is_empty() {
        if fs.stat_bin(&entry.storage_name)?.is_some() {
            Ok(Some(entry))
        } else {
            Ok(None)
        }
    } else if fs.read_bin_meta(&bin_meta_name(&entry.id))?.is_some() {
        Ok(Some(entry))
    } else {
        Ok(None)
    }
}

fn write_pending_op<S: IndexStore>(store: &mut S, op: PendingOp) -> Result<()> {
    let batch = StoreBatch {
        set_pending_op: Some(Some(op)),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)
}

fn clear_pending_op<S: IndexStore>(store: &mut S) -> Result<()> {
    let batch = StoreBatch {
        set_pending_op: Some(None),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)
}

fn commit_records_and_clear_pending<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    records: Vec<IndexRecord>,
    bin_entries: Vec<BinEntry>,
    clear_held_back: Vec<(MemberId, String)>,
    remove_bin_ids: Vec<String>,
) -> Result<()> {
    let mut meta = load_meta_required(store)?;
    let own_key = meta.version_key();
    let mut batch = StoreBatch::default();
    let mut del_log_entries = Vec::new();

    for mut rec in records {
        let own_counter = rec.version.get(&own_key);
        if own_counter > meta.local_clock {
            meta.local_clock = own_counter;
        }
        if rec.deleted {
            del_log_entries.push(FolderDeletionLogEntry::from_tombstone(&rec));
        } else if store.get_record(&rec.path)?.is_some_and(|p| p.deleted) {
            del_log_entries.push(FolderDeletionLogEntry::Revived {
                path: rec.path.clone(),
            });
        }
        let seq = meta.next_seq();
        rec.seq = seq;
        if rec.deleted && !rec.recent_hashes.is_empty() {
            let mut merged_dh = rec.recent_hashes.clone();
            for h in store.get_deleted_history(&rec.path)? {
                if merged_dh.len() >= MAX_ANCESTRY_HASHES {
                    break;
                }
                if !merged_dh.contains(&h) {
                    merged_dh.push(h);
                }
            }
            batch
                .upsert_deleted_history
                .push((rec.path.clone(), merged_dh));
            let del_vv = store
                .get_deleted_history_version(&rec.path)?
                .unwrap_or_default()
                .merge(&rec.version)
                .pruned(&meta.pruned_incarnations);
            batch
                .upsert_deleted_history_versions
                .push((rec.path.clone(), del_vv));
        }
        rec.conflict_of = derive_conflict_of(&rec, &meta);
        let chain = meta.advance_chain_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_seq_fingerprints.push((seq, chain));
        batch.upsert_records.push(rec);
    }

    if !del_log_entries.is_empty() {
        let encoded = encode_deletion_log_entries(&del_log_entries)?;
        fs.append_deletion_log(&encoded)?;
    }

    batch.folder_meta = Some(meta);
    batch.upsert_bin = bin_entries;
    batch.remove_bin = remove_bin_ids;
    batch.remove_held_back = clear_held_back;
    batch.set_pending_op = Some(None);
    store.apply_batch(batch)
}

fn load_meta_required<S: IndexStore>(store: &S) -> Result<FolderMeta> {
    store.load_meta()?.ok_or_else(|| SyncError::Storage {
        context: "load folder metadata".to_string(),
        detail: "sync index store is not initialized".to_string(),
    })
}

fn record_disk_stat<F: SyncFs>(fs: &F, path: &str, rec: &mut IndexRecord) -> Result<()> {
    if let Some(m) = fs.stat_live(path)? {
        rec.disk_mtime = Some(m.mtime);
        rec.disk_change_id = m.change_id;
    }
    Ok(())
}

fn apply_live_metadata<F: SyncFs>(
    fs: &F,
    path: &str,
    mtime: Timestamp,
    mode: Option<u32>,
) -> Result<()> {
    match fs.set_live_metadata(path, mtime, mode) {
        Ok(()) => Ok(()),
        Err(err) if err.is_crash() => Err(err),
        Err(_) => Ok(()),
    }
}

fn alloc_tmp_name_for<F: SyncFs>(fs: &F, prefix: &str, live_rel: &str) -> Result<String> {
    let flat = next_tmp_name(prefix);
    if fs.needs_local_staging(live_rel)? {
        match parent_path(live_rel) {
            Some(parent) => Ok(format!("{parent}/{flat}")),
            None => Ok(flat),
        }
    } else {
        Ok(flat)
    }
}

fn next_tmp_name(prefix: &str) -> String {
    thread_local! {
        static LOCAL_NONCE: Cell<u64> = const { Cell::new(1) };
    }
    let nonce = LOCAL_NONCE.with(|c| {
        let n = c.get();
        c.set(n.wrapping_add(1));
        n
    });
    let token = random_hex_token();
    format!(".legion-sync-{prefix}-{nonce}-{token}.tmp")
}

fn bin_meta_name(id: &str) -> String {
    format!("{id}.json")
}
