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
use std::time::Instant;

use legion_xfer::{EntryKind, Hash, Timestamp};

use crate::error::{FsErrorKind, Result, SyncError};
use crate::model::{
    BinEntry, BinReason, ConflictChoice, ExpectedLocalSnapshot, FetchOptions, FolderMeta,
    IndexRecord, MAX_ANCESTRY_HASHES, MIN_PLAUSIBLE_CLOCK_SECS, PendingOp, ProblemKind,
    RestoreStrategy, SyncProblem, random_hex_token,
};
use crate::path::{
    IgnoreMatcher, ancestors, is_descendant_of, is_legion_inflight_name, is_os_litter_name,
    parent_path, restore_copy_path, validate_sync_path,
};
use crate::reconcile::{
    LocalIndexView, ReconcileDecision, check_local_representable, choose_conflict_path,
    derive_conflict_of,
};
use crate::scan::{SingleRescanOutcome, disk_matches_index, rescan_single_path};
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
}

enum SafeReplaceResult {
    Replaced,
    RaceChanged,
    RaceChangedAfterBin,
}

enum SafeDeleteResult {
    Deleted,
    RaceChanged,
}

/// Recovers any incomplete [`PendingOp`] left by a crash or error and cleans orphaned staging
/// files from `.legion-sync/tmp/`.
///
/// Idempotent: crashing again during recovery converges cleanly on the next restart.
pub fn recover_pending<F: SyncFs, S: IndexStore>(fs: &F, store: &mut S) -> Result<RecoveryReport> {
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
                )?;
            }
        }
    }

    // Sweep any leftover files in `.legion-sync/tmp/` after recovery completes.
    for tmp_name in fs.list_tmp()? {
        let _ = fs.remove_tmp(&tmp_name);
        report.cleaned_tmp_files += 1;
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
) -> Result<()> {
    let path = target_record.path.clone();

    // Case 1: target is already materialized on disk with expected content.
    if disk_matches_record(fs, &path, &target_record)? {
        // Check if an interrupted exchange/replace left the displaced old file in `staged_tmp`
        // or `claim_tmp` without finishing the move to bin.
        let displaced_tmp = find_present_tmp(fs, &[claim_tmp.as_deref(), staged_tmp.as_deref()])?;
        if let Some(ref disp) = displaced_tmp
            && let Some(ref b) = bin_entry
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_none()
        {
            if tmp_matches_snapshot(fs, disp, &expected_local)? {
                write_bin_meta_file(fs, b)?;
                fs.move_tmp_to_bin_no_replace(disp, &b.storage_name)?;
            } else {
                // Human edited `path` right before exchange and we crashed before swapping back!
                // Swap the human's bytes right back to `path` and roll back!
                if !fs.exchange_tmp_with_live(disp, &path)? {
                    let backup = next_tmp_name("swapback");
                    let _ = fs.replace_live_with_tmp_backup(disp, &path, &backup);
                    let _ = fs.remove_tmp(&backup);
                }
                let _ = fs.remove_tmp(disp);
                clear_pending_op(store)?;
                return Ok(());
            }
        }
        record_disk_stat(fs, &path, &mut target_record)?;
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(
            store,
            vec![target_record],
            persisted_bin.into_iter().collect(),
            vec![path],
            Vec::new(),
        )?;
        cleanup_tmps(fs, &[staged_tmp.as_deref(), claim_tmp.as_deref()]);
        return Ok(());
    }

    // Case 2: disk still holds the untouched pre-apply entry -> safely roll back.
    if disk_matches_snapshot(fs, &path, &expected_local)?
        && !matches!(expected_local, ExpectedLocalSnapshot::Missing)
    {
        cleanup_tmps(fs, &[staged_tmp.as_deref(), claim_tmp.as_deref()]);
        if let Some(ref b) = bin_entry
            && (b.storage_name.is_empty() || fs.stat_bin(&b.storage_name)?.is_none())
        {
            let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
        }
        clear_pending_op(store)?;
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
                // Human edit was claimed right before crash — put it right back!
                let _ = fs.commit_tmp_to_live_no_replace(claim, &path);
                cleanup_tmps(fs, &[staged_tmp.as_deref()]);
                clear_pending_op(store)?;
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
                    fs.commit_tmp_to_live_no_replace(tmp, &path)?;
                    let _ = fs.set_live_metadata(&path, target_record.mtime, target_record.mode);
                    record_disk_stat(fs, &path, &mut target_record)?;
                    let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                    commit_records_and_clear_pending(
                        store,
                        vec![target_record],
                        persisted_bin.into_iter().collect(),
                        vec![path],
                        Vec::new(),
                    )?;
                    return Ok(());
                }
                // Staged payload missing; restore the moved-to-bin copy if present so nothing is lost.
                if let Some(ref b) = bin_entry
                    && !b.storage_name.is_empty()
                    && fs.stat_bin(&b.storage_name)?.is_some()
                {
                    let restore_tmp = next_tmp_name("recover");
                    fs.copy_bin_to_tmp(&b.storage_name, &restore_tmp)?;
                    fs.commit_tmp_to_live_no_replace(&restore_tmp, &path)?;
                    let _ = fs.set_live_metadata(&path, b.original_mtime, b.mode);
                    let _ = fs.remove_bin_file(&b.storage_name);
                    let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
                }
                clear_pending_op(store)?;
                return Ok(());
            }
            EntryKind::Dir => {
                fs.create_dir_all_live(&path)?;
                record_disk_stat(fs, &path, &mut target_record)?;
                let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                commit_records_and_clear_pending(
                    store,
                    vec![target_record],
                    persisted_bin.into_iter().collect(),
                    vec![path],
                    Vec::new(),
                )?;
                return Ok(());
            }
            EntryKind::Symlink => {
                if let Some(ref target) = target_record.link_target {
                    fs.create_symlink_live(&path, target)?;
                    record_disk_stat(fs, &path, &mut target_record)?;
                    let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                    commit_records_and_clear_pending(
                        store,
                        vec![target_record],
                        persisted_bin.into_iter().collect(),
                        vec![path],
                        Vec::new(),
                    )?;
                    return Ok(());
                }
            }
        }
    }

    // Case 4: path on disk matches neither expected_local nor target_record (external human edit
    // while crashed). Leave human content untouched and clear the pending journal.
    cleanup_tmps(fs, &[staged_tmp.as_deref(), claim_tmp.as_deref()]);
    clear_pending_op(store)?;
    Ok(())
}

fn recover_delete<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    target_record: IndexRecord,
    claim_tmp: Option<String>,
    bin_entry: Option<BinEntry>,
    expected_local: ExpectedLocalSnapshot,
) -> Result<()> {
    let path = target_record.path.clone();

    if fs.stat_live(&path)?.is_none() {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            if !tmp_matches_snapshot(fs, claim, &expected_local)? {
                // Human edit was claimed right before crash — put it right back!
                let _ = fs.commit_tmp_to_live_no_replace(claim, &path);
                clear_pending_op(store)?;
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
            store,
            vec![target_record],
            persisted_bin.into_iter().collect(),
            vec![path],
            Vec::new(),
        )?;
        return Ok(());
    }

    if disk_matches_snapshot(fs, &path, &expected_local)?
        && let Some(ref b) = bin_entry
        && (b.storage_name.is_empty() || fs.stat_bin(&b.storage_name)?.is_none())
    {
        let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
    }
    cleanup_tmps(fs, &[claim_tmp.as_deref()]);
    clear_pending_op(store)?;
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
            store,
            vec![from_record, to_record],
            Vec::new(),
            vec![from_path, to_path],
            Vec::new(),
        )?;
        return Ok(());
    }

    if disk_matches_snapshot(fs, &from_path, &expected_from)? {
        clear_pending_op(store)?;
        return Ok(());
    }

    clear_pending_op(store)?;
    Ok(())
}

fn recover_conflict_local_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    winner_tmp: Option<String>,
    conflict_already_present: bool,
    expected_local: ExpectedLocalSnapshot,
) -> Result<()> {
    let orig_path = winner_record.path.clone();
    let conflict_path = loser_record.path.clone();

    let winner_done = disk_matches_record(fs, &orig_path, &winner_record)?;
    let loser_done = disk_matches_record(fs, &conflict_path, &loser_record)?;

    if winner_done && loser_done {
        record_disk_stat(fs, &orig_path, &mut winner_record)?;
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        commit_records_and_clear_pending(
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            vec![orig_path, conflict_path],
            Vec::new(),
        )?;
        cleanup_tmps(fs, &[winner_tmp.as_deref()]);
        return Ok(());
    }

    if loser_done && fs.stat_live(&orig_path)?.is_none() {
        let placed = match winner_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = winner_tmp
                    && tmp_matches_hash(fs, tmp, winner_record.hash, winner_record.size)?
                {
                    fs.commit_tmp_to_live_no_replace(tmp, &orig_path)?;
                    let _ =
                        fs.set_live_metadata(&orig_path, winner_record.mtime, winner_record.mode);
                    true
                } else {
                    false
                }
            }
            EntryKind::Dir => {
                fs.create_dir_all_live(&orig_path)?;
                true
            }
            EntryKind::Symlink => {
                if let Some(ref target) = winner_record.link_target {
                    fs.create_symlink_live(&orig_path, target)?;
                    true
                } else {
                    false
                }
            }
        };

        if placed {
            record_disk_stat(fs, &orig_path, &mut winner_record)?;
            record_disk_stat(fs, &conflict_path, &mut loser_record)?;
            commit_records_and_clear_pending(
                store,
                vec![loser_record, winner_record],
                Vec::new(),
                vec![orig_path, conflict_path],
                Vec::new(),
            )?;
            return Ok(());
        }

        if !conflict_already_present && loser_record.kind == EntryKind::File {
            let _ = fs.rename_live_to_live_no_replace(&conflict_path, &orig_path);
        }
        clear_pending_op(store)?;
        return Ok(());
    }

    if disk_matches_snapshot(fs, &orig_path, &expected_local)? {
        cleanup_tmps(fs, &[winner_tmp.as_deref()]);
        clear_pending_op(store)?;
        return Ok(());
    }

    cleanup_tmps(fs, &[winner_tmp.as_deref()]);
    clear_pending_op(store)?;
    Ok(())
}

fn recover_conflict_remote_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    loser_tmp: Option<String>,
    _conflict_already_present: bool,
    _expected_local: ExpectedLocalSnapshot,
) -> Result<()> {
    let orig_path = winner_record.path.clone();
    let conflict_path = loser_record.path.clone();

    if disk_matches_record(fs, &conflict_path, &loser_record)? {
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        commit_records_and_clear_pending(
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            vec![orig_path, conflict_path],
            Vec::new(),
        )?;
        cleanup_tmps(fs, &[loser_tmp.as_deref()]);
        return Ok(());
    }

    cleanup_tmps(fs, &[loser_tmp.as_deref()]);
    clear_pending_op(store)?;
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
) -> Result<()> {
    let path = target_record.path.clone();
    if disk_matches_record(fs, &path, &target_record)? {
        let displaced_tmp = find_present_tmp(fs, &[claim_tmp.as_deref(), staged_tmp.as_deref()])?;
        if let Some(ref disp) = displaced_tmp
            && let Some(ref b) = replacement_bin
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_none()
        {
            if tmp_matches_snapshot(fs, disp, &expected_local)? {
                write_bin_meta_file(fs, b)?;
                fs.move_tmp_to_bin_no_replace(disp, &b.storage_name)?;
            } else {
                if !fs.exchange_tmp_with_live(disp, &path)? {
                    let backup = next_tmp_name("swapback");
                    let _ = fs.replace_live_with_tmp_backup(disp, &path, &backup);
                    let _ = fs.remove_tmp(&backup);
                }
                let _ = fs.remove_tmp(disp);
                clear_pending_op(store)?;
                return Ok(());
            }
        }
        record_disk_stat(fs, &path, &mut target_record)?;
        let persisted_rep = bin_entry_if_present(fs, replacement_bin)?;
        commit_records_and_clear_pending(
            store,
            vec![target_record],
            persisted_rep.into_iter().collect(),
            vec![path],
            vec![restored_bin_id.clone()],
        )?;
        if !restored_storage_name.is_empty() {
            let _ = fs.remove_bin_file(&restored_storage_name);
        }
        let _ = fs.remove_bin_file(&bin_meta_name(&restored_bin_id));
        cleanup_tmps(fs, &[staged_tmp.as_deref(), claim_tmp.as_deref()]);
        return Ok(());
    }

    if fs.stat_live(&path)?.is_none() {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            let _ = fs.commit_tmp_to_live_no_replace(claim, &path);
        } else if let Some(ref b) = replacement_bin
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_some()
        {
            let rt = next_tmp_name("recover-restore");
            if fs.copy_bin_to_tmp(&b.storage_name, &rt).is_ok()
                && fs.commit_tmp_to_live_no_replace(&rt, &path).is_ok()
            {
                let _ = fs.set_live_metadata(&path, b.original_mtime, b.mode);
                let _ = fs.remove_bin_file(&b.storage_name);
                let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
            }
        }
    }

    cleanup_tmps(fs, &[staged_tmp.as_deref(), claim_tmp.as_deref()]);
    clear_pending_op(store)?;
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
) -> Result<()> {
    let orig_path = promoted_record.path.clone();
    let conflict_path = deleted_conflict_record.path.clone();

    if disk_matches_record(fs, &orig_path, &promoted_record)?
        && fs.stat_live(&conflict_path)?.is_none()
    {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
            && let Some(ref b) = bin_entry
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_none()
            && tmp_matches_snapshot(fs, claim, &expected_orig)?
        {
            write_bin_meta_file(fs, b)?;
            fs.move_tmp_to_bin_no_replace(claim, &b.storage_name)?;
        }
        record_disk_stat(fs, &orig_path, &mut promoted_record)?;
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(
            store,
            vec![deleted_conflict_record, promoted_record],
            persisted_bin.into_iter().collect(),
            vec![orig_path, conflict_path],
            Vec::new(),
        )?;
        cleanup_tmps(fs, &[claim_tmp.as_deref()]);
        return Ok(());
    }

    // Roll back: if orig_path is missing and claim_tmp or bin holds the original file, restore it.
    if fs.stat_live(&orig_path)?.is_none() {
        if let Some(ref claim) = claim_tmp
            && fs.stat_tmp(claim)?.is_some()
        {
            let _ = fs.commit_tmp_to_live_no_replace(claim, &orig_path);
        } else if let Some(ref b) = bin_entry
            && !b.storage_name.is_empty()
            && fs.stat_bin(&b.storage_name)?.is_some()
        {
            let rt = next_tmp_name("recover-resolve");
            if fs.copy_bin_to_tmp(&b.storage_name, &rt).is_ok()
                && fs.commit_tmp_to_live_no_replace(&rt, &orig_path).is_ok()
            {
                let _ = fs.set_live_metadata(&orig_path, b.original_mtime, b.mode);
                let _ = fs.remove_bin_file(&b.storage_name);
                let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
            }
        }
    }

    cleanup_tmps(fs, &[claim_tmp.as_deref()]);
    clear_pending_op(store)?;
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
        ReconcileDecision::Ignore | ReconcileDecision::Keep => Ok(ApplyOutcome::Applied),
        ReconcileDecision::HoldBack { .. } => Ok(ApplyOutcome::Applied),
        ReconcileDecision::KeepAndSupersede { mut record } => {
            let mut meta = load_meta_required(store)?;
            let vkey = meta.version_key();
            let clock = meta.next_clock();
            record.version.bump_above(vkey, clock);
            let seq = meta.next_seq();
            record.seq = seq;
            record.modified_by = meta.local_member.clone();
            record.from_join = false;
            record.conflict_of = derive_conflict_of(&record, &meta);
            meta.record_seq_fingerprint(seq, record.head_fingerprint());
            let mut batch = StoreBatch {
                folder_meta: Some(meta),
                remove_held_back: vec![record.path.clone()],
                ..StoreBatch::default()
            };
            if record.deleted && !record.recent_hashes.is_empty() {
                batch
                    .upsert_deleted_history
                    .push((record.path.clone(), record.recent_hashes.clone()));
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
            if !record.deleted && !disk_matches_index(fs, &record.path, existing.as_ref())? {
                return handle_local_changed(fs, store, &record.path, now);
            }
            if update_disk_metadata && !record.deleted {
                apply_live_metadata(fs, &record.path, record.mtime, record.mode)?;
                let p = record.path.clone();
                record_disk_stat(fs, &p, &mut record)?;
            }
            let path = record.path.clone();
            commit_records_and_clear_pending(
                store,
                vec![record],
                Vec::new(),
                vec![path],
                Vec::new(),
            )?;
            Ok(ApplyOutcome::Applied)
        }
        ReconcileDecision::ReviveDir { path, base_version } => {
            let mut meta = load_meta_required(store)?;
            let vkey = meta.version_key();
            let mut version = base_version;
            let clock = meta.next_clock();
            version.bump_above(vkey, clock);
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
            meta.record_seq_fingerprint(seq, revived.head_fingerprint());
            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records: vec![revived],
                remove_held_back: vec![path],
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
    from_record: IndexRecord,
    mut to_record: IndexRecord,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let from_path = from_record.path.clone();
    let to_path = to_record.path.clone();

    if !ensure_ancestors_are_dirs(fs, store, &to_path, now)? {
        return Ok(ApplyOutcome::Deferred {
            reason: format!("ancestor of '{to_path}' is currently being written"),
        });
    }

    let existing_from = store.get_record(&from_path)?;
    if !disk_matches_index(fs, &from_path, existing_from.as_ref())? {
        return handle_local_changed(fs, store, &from_path, now);
    }
    if fs.stat_live(&to_path)?.is_some() {
        return handle_local_changed(fs, store, &to_path, now);
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
            }) => {
                clear_pending_op(store)?;
                return handle_local_changed(fs, store, &from_path, now);
            }
            Err(err) => return Err(err),
        }

        // Verify the renamed file still holds the expected hash/size (if a person edited
        // `from_path` right before rename, move it back immediately and rescan).
        if let Some(expected_hash) = to_record.hash {
            let (actual_hash, actual_size) = fs.hash_live(&to_path)?;
            if actual_hash != expected_hash || actual_size != to_record.size {
                let _ = fs.rename_live_to_live_no_replace(&to_path, &from_path);
                clear_pending_op(store)?;
                return handle_local_changed(fs, store, &from_path, now);
            }
        }

        apply_live_metadata(fs, &to_path, to_record.mtime, to_record.mode)?;
        record_disk_stat(fs, &to_path, &mut to_record)?;
        commit_records_and_clear_pending(
            store,
            vec![from_record, to_record],
            Vec::new(),
            vec![from_path, to_path],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    recover_on_non_crash_error(fs, store, res)
}

/// Applies a batch of clean brand-new regular files (no existing live entry at any target path),
/// staging and BLAKE3-verifying chunks of files, syncing every file before any non-replacing
/// rename, syncing touched parent directories and `.legion-sync/tmp` once per page, and committing
/// all records in ONE single `IndexStore::apply_batch` group commit.
///
/// Every file's bytes are BLAKE3-verified and synced (`finish_sync`) in `.legion-sync/tmp` before
/// its non-replacing rename, and every touched parent directory plus `.legion-sync/tmp` is synced
/// before the single ACID index commit.
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
    let mut staged: Vec<(u64, IndexRecord, String, Box<dyn SyncWrite>)> =
        Vec::with_capacity(GROUP_STAGE_MAX_FILES.min(records.len()));
    let mut staged_bytes: u64 = 0;

    for target in records {
        let remote_seq = target.seq;
        let path = target.path.clone();

        // Ensure parent directories exist cleanly; if any ancestor isn't a live dir yet, create
        // it cleanly (if ancestor is a file/symlink, fall back to single-record path).
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
                        None => {
                            fs.create_dir_all_live(anc)?;
                            created_ancestors.insert(anc.to_string());
                            continue;
                        }
                        Some(_) => {
                            // Unscanned non-directory (file/symlink) on disk at `anc`: fall back
                            // to single-record path so `ensure_ancestors_are_dirs` rescans it and
                            // preserves it as a conflict copy without overwriting!
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
            if !ensure_ancestors_are_dirs(fs, store, &path, now)? {
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

        let (tmp, writer) = match fetch_and_verify_to_tmp_staged(
            fs,
            peer,
            &path,
            target.size,
            expected_hash,
            fetch_opts,
        ) {
            Ok(pair) => pair,
            Err(err) if is_peer_file_changed_error(&err) => {
                out.deferred.push((remote_seq, path, err.to_string()));
                continue;
            }
            Err(err) => {
                cleanup_staged_writers(fs, staged);
                if !err.is_crash() && !placed.is_empty() {
                    let _ = flush_group_commit_records(
                        fs,
                        store,
                        &mut placed,
                        &synced_parents,
                        &mut out,
                    );
                }
                return Err(err);
            }
        };

        staged_bytes = staged_bytes.saturating_add(target.size);
        staged.push((remote_seq, target, tmp, writer));

        if staged.len() >= GROUP_STAGE_MAX_FILES || staged_bytes >= GROUP_STAGE_MAX_BYTES {
            let chunk = std::mem::take(&mut staged);
            staged_bytes = 0;
            place_staged_chunk(fs, store, chunk, &mut placed, &mut synced_parents, &mut out)?;
        }
    }

    if !staged.is_empty() {
        place_staged_chunk(
            fs,
            store,
            staged,
            &mut placed,
            &mut synced_parents,
            &mut out,
        )?;
    }

    if !placed.is_empty() {
        flush_group_commit_records(fs, store, &mut placed, &synced_parents, &mut out)?;
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

fn place_staged_chunk<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    staged: Vec<(u64, IndexRecord, String, Box<dyn SyncWrite>)>,
    placed: &mut Vec<(u64, IndexRecord)>,
    synced_parents: &mut HashSet<String>,
    out: &mut BatchApplyResult,
) -> Result<()> {
    // Phase 1: sync EVERY staged file in the chunk to durable storage BEFORE any rename so no
    // un-synced data ever enters the live working tree.
    let mut synced: Vec<(u64, IndexRecord, String)> = Vec::with_capacity(staged.len());
    let mut iter = staged.into_iter();
    while let Some((remote_seq, target, tmp, writer)) = iter.next() {
        if let Err(err) = writer.finish_sync() {
            let _ = fs.remove_tmp(&tmp);
            for (_, _, rem_tmp, rem_w) in iter {
                drop(rem_w);
                let _ = fs.remove_tmp(&rem_tmp);
            }
            for (_, _, done_tmp) in synced {
                let _ = fs.remove_tmp(&done_tmp);
            }
            if !err.is_crash() && !placed.is_empty() {
                let _ = flush_group_commit_records(fs, store, placed, synced_parents, out);
            }
            return Err(err);
        }
        synced.push((remote_seq, target, tmp));
    }

    // Phase 2: non-replacing rename of every synced file into the live tree (parent directory
    // and `.legion-sync/tmp` fsyncs are coalesced once in `flush_group_commit_records` before the
    // single ACID index commit).
    let mut synced_iter = synced.into_iter();
    while let Some((remote_seq, mut target, tmp)) = synced_iter.next() {
        let path = target.path.clone();
        match fs.commit_tmp_to_live_no_replace_deferred_dir_sync(&tmp, &path) {
            Ok(()) => {}
            Err(SyncError::Fs {
                kind: FsErrorKind::AlreadyExists,
                ..
            }) => {
                let _ = fs.remove_tmp(&tmp);
                out.fallback_records.push(target);
                continue;
            }
            Err(err) => {
                let _ = fs.remove_tmp(&tmp);
                for (_, _, rem_tmp) in synced_iter {
                    let _ = fs.remove_tmp(&rem_tmp);
                }
                if !err.is_crash() && !placed.is_empty() {
                    let _ = flush_group_commit_records(fs, store, placed, synced_parents, out);
                }
                return Err(err);
            }
        }

        if let Err(err) = apply_live_metadata(fs, &path, target.mtime, target.mode)
            .and_then(|()| record_disk_stat(fs, &path, &mut target))
        {
            for (_, _, rem_tmp) in synced_iter {
                let _ = fs.remove_tmp(&rem_tmp);
            }
            if !err.is_crash() && !placed.is_empty() {
                let _ = flush_group_commit_records(fs, store, placed, synced_parents, out);
            }
            return Err(err);
        }

        if let Some(parent) = parent_path(&path) {
            synced_parents.insert(parent.to_string());
        } else {
            synced_parents.insert(String::new());
        }
        placed.push((remote_seq, target));
    }

    Ok(())
}

fn flush_group_commit_records<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    placed: &mut Vec<(u64, IndexRecord)>,
    synced_parents: &HashSet<String>,
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
    for (remote_seq, mut rec) in drained {
        let seq = meta.next_seq();
        rec.seq = seq;
        rec.conflict_of = derive_conflict_of(&rec, &meta);
        meta.record_seq_fingerprint(seq, rec.head_fingerprint());
        batch.remove_held_back.push(rec.path.clone());
        batch.upsert_records.push(rec);
        out.applied_seqs.push(remote_seq);
    }
    batch.folder_meta = Some(meta);
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

    // 1. Fetch & verify into `.legion-sync/tmp/` before touching the live tree.
    let staged_tmp = if target.kind == EntryKind::File {
        let expected_hash = target.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote record for {path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(fs, peer, &path, target.size, expected_hash, fetch_opts) {
            Ok(tmp) => Some(tmp),
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: format!("file changed on peer while fetching: {err}"),
                });
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    // 2. Ensure ancestors are live directories.
    match ensure_ancestors_are_dirs(fs, store, &path, now) {
        Ok(true) => {}
        Ok(false) => {
            cleanup_tmps(fs, &[staged_tmp.as_deref()]);
            return Ok(ApplyOutcome::Deferred {
                reason: format!("ancestor of '{path}' is actively changing"),
            });
        }
        Err(err) => {
            cleanup_tmps(fs, &[staged_tmp.as_deref()]);
            return Err(err);
        }
    }

    // 3. Re-check local disk against our index before replacing anything.
    let existing = store.get_record(&path)?;
    if !disk_matches_index(fs, &path, existing.as_ref())? {
        cleanup_tmps(fs, &[staged_tmp.as_deref()]);
        return handle_local_changed(fs, store, &path, now);
    }

    if let Some(ref loc) = existing
        && !loc.deleted
        && loc.kind == EntryKind::Dir
        && target.kind != EntryKind::Dir
        && !fs.list_dir_live(&path)?.is_empty()
    {
        cleanup_tmps(fs, &[staged_tmp.as_deref()]);
        return Ok(ApplyOutcome::LocalChangedRetry);
    }

    let expected_local = snapshot_local(existing.as_ref());
    let bin_entry = build_bin_entry(existing.as_ref(), bin_reason, peer.member_id(), now);
    let claim_tmp = if existing
        .as_ref()
        .is_some_and(|r| !r.deleted && r.kind == EntryKind::File)
    {
        Some(next_tmp_name("claim"))
    } else {
        None
    };

    // 4. Persist write-ahead intent BEFORE mutating live files.
    let pending = PendingOp::Place {
        target_record: target.clone(),
        staged_tmp: staged_tmp.clone(),
        claim_tmp: claim_tmp.clone(),
        bin_entry: bin_entry.clone(),
        expected_local,
    };
    write_pending_op(store, pending)?;

    let res: Result<ApplyOutcome> = (|| {
        let existing_live = existing.as_ref().filter(|r| !r.deleted);

        // Fast & atomic path: replacing an existing regular file with a new regular file!
        if let (Some(old_rec), EntryKind::File, Some(tmp), Some(claim), Some(b_entry)) = (
            existing_live.filter(|r| r.kind == EntryKind::File),
            target.kind,
            staged_tmp.as_deref(),
            claim_tmp.as_deref(),
            bin_entry.as_ref(),
        ) {
            let old_hash = old_rec.hash.ok_or_else(|| SyncError::CorruptData {
                context: format!("local record for {path}"),
                detail: "live file missing BLAKE3 hash".to_string(),
            })?;
            match safe_replace_live_file(fs, tmp, claim, &path, old_hash, old_rec.size, b_entry)? {
                SafeReplaceResult::Replaced => {
                    apply_live_metadata(fs, &path, target.mtime, target.mode)?;
                    record_disk_stat(fs, &path, &mut target)?;
                    commit_records_and_clear_pending(
                        store,
                        vec![target],
                        vec![b_entry.clone()],
                        vec![path],
                        Vec::new(),
                    )?;
                    return Ok(ApplyOutcome::Applied);
                }
                SafeReplaceResult::RaceChanged => {
                    cleanup_tmps(fs, &[Some(tmp), Some(claim)]);
                    clear_pending_op(store)?;
                    return handle_local_changed(fs, store, &path, now);
                }
                SafeReplaceResult::RaceChangedAfterBin => {
                    cleanup_tmps(fs, &[Some(tmp), Some(claim)]);
                    // Record the bin entry we already wrote so it stays visible, then clear pending.
                    let batch = StoreBatch {
                        upsert_bin: vec![b_entry.clone()],
                        set_pending_op: Some(None),
                        ..StoreBatch::default()
                    };
                    store.apply_batch(batch)?;
                    return handle_local_changed(fs, store, &path, now);
                }
            }
        }

        // General path: either creating a brand-new entry or changing entry kinds (File<->Dir<->Symlink).
        let mut persisted_bins = Vec::new();
        if let Some(old_rec) = existing_live {
            match old_rec.kind {
                EntryKind::File => {
                    if let (Some(claim), Some(b_entry), Some(old_hash)) =
                        (claim_tmp.as_deref(), bin_entry.as_ref(), old_rec.hash)
                    {
                        match safe_delete_live_file(
                            fs,
                            &path,
                            claim,
                            old_hash,
                            old_rec.size,
                            b_entry,
                        )? {
                            SafeDeleteResult::Deleted => persisted_bins.push(b_entry.clone()),
                            SafeDeleteResult::RaceChanged => {
                                cleanup_tmps(fs, &[staged_tmp.as_deref(), Some(claim)]);
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &path, now);
                            }
                        }
                    }
                }
                EntryKind::Symlink => {
                    if let Some(ref b_entry) = bin_entry {
                        write_bin_meta_file(fs, b_entry)?;
                        persisted_bins.push(b_entry.clone());
                    }
                    fs.remove_symlink_live(&path)?;
                }
                EntryKind::Dir => match fs.remove_empty_dir_live(&path) {
                    Ok(()) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::NotFound,
                        ..
                    }) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::DirectoryNotEmpty,
                        ..
                    }) => {
                        cleanup_tmps(fs, &[staged_tmp.as_deref(), claim_tmp.as_deref()]);
                        clear_pending_op(store)?;
                        return Ok(ApplyOutcome::LocalChangedRetry);
                    }
                    Err(err) => return Err(err),
                },
            }
        }

        // Place new entry using non-replacing primitives!
        match target.kind {
            EntryKind::File => {
                if let Some(ref tmp) = staged_tmp {
                    match fs.commit_tmp_to_live_no_replace(tmp, &path) {
                        Ok(()) => {}
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            let _ = fs.remove_tmp(tmp);
                            let batch = StoreBatch {
                                upsert_bin: persisted_bins,
                                set_pending_op: Some(None),
                                ..StoreBatch::default()
                            };
                            store.apply_batch(batch)?;
                            return handle_local_changed(fs, store, &path, now);
                        }
                        Err(err) => return Err(err),
                    }
                }
                apply_live_metadata(fs, &path, target.mtime, target.mode)?;
            }
            EntryKind::Dir => {
                fs.create_dir_all_live(&path)?;
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
                    Err(SyncError::Fs {
                        kind: FsErrorKind::AlreadyExists,
                        ..
                    }) => {
                        let batch = StoreBatch {
                            upsert_bin: persisted_bins,
                            set_pending_op: Some(None),
                            ..StoreBatch::default()
                        };
                        store.apply_batch(batch)?;
                        return handle_local_changed(fs, store, &path, now);
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        record_disk_stat(fs, &path, &mut target)?;
        commit_records_and_clear_pending(
            store,
            vec![target],
            persisted_bins,
            vec![path],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    recover_on_non_crash_error(fs, store, res)
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

    if !disk_matches_index(fs, &path, existing.as_ref())? {
        return handle_local_changed(fs, store, &path, now);
    }

    // Item H: inspect directories before deleting!
    if let Some(ref loc) = existing
        && !loc.deleted
        && loc.kind == EntryKind::Dir
    {
        if store.has_live_descendant(&path)? {
            return Ok(ApplyOutcome::LocalChangedRetry);
        }
        let children = fs.list_dir_live(&path)?;
        if !children.is_empty() {
            let held_back_paths: BTreeSet<String> = store
                .list_held_back()?
                .into_iter()
                .map(|h| h.record.path)
                .collect();

            let mut litter_files: Vec<(String, FsMeta)> = Vec::new();
            let mut has_local_only = false;
            let mut unscanned_synced = Vec::new();

            for name in children {
                let child_rel = format!("{path}/{name}");
                let child_meta = fs.stat_live(&child_rel)?;
                let is_dir = child_meta
                    .as_ref()
                    .is_some_and(|m| m.kind == EntryKind::Dir);

                if is_os_litter_name(&name)
                    && let Some(cm) = child_meta.filter(|m| m.kind == EntryKind::File)
                {
                    litter_files.push((child_rel, cm));
                } else if ignore.is_custom_ignored(&child_rel, &name, is_dir)
                    || is_legion_inflight_name(&name)
                    || held_back_paths.contains(&child_rel)
                    || held_back_paths
                        .iter()
                        .any(|p| is_descendant_of(p, &child_rel))
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
                meta.record_seq_fingerprint(seq, tomb.head_fingerprint());
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    upsert_records: vec![tomb],
                    remove_held_back: vec![path.clone()],
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
                let (h, sz) = fs.hash_live(&child_rel)?;
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
                fs.move_live_to_bin_no_replace(&child_rel, &entry.storage_name)?;
                litter_bins.push(entry);
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
                }) => {
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
                Err(err) => return Err(err),
            }

            commit_records_and_clear_pending(
                store,
                vec![tombstone],
                litter_bins,
                vec![path],
                Vec::new(),
            )?;
            return Ok(ApplyOutcome::Applied);
        }
    }

    let expected_local = snapshot_local(existing.as_ref());
    let bin_entry = build_bin_entry(existing.as_ref(), bin_reason, peer_member, now);
    let claim_tmp = if existing
        .as_ref()
        .is_some_and(|r| !r.deleted && r.kind == EntryKind::File)
    {
        Some(next_tmp_name("delclaim"))
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
                        (claim_tmp.as_deref(), bin_entry.as_ref(), loc.hash)
                    {
                        match safe_delete_live_file(fs, &path, claim, old_hash, loc.size, b_entry)?
                        {
                            SafeDeleteResult::Deleted => persisted_bins.push(b_entry.clone()),
                            SafeDeleteResult::RaceChanged => {
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &path, now);
                            }
                        }
                    }
                }
                EntryKind::Symlink => {
                    if let Some(ref b_entry) = bin_entry {
                        write_bin_meta_file(fs, b_entry)?;
                        persisted_bins.push(b_entry.clone());
                    }
                    fs.remove_symlink_live(&path)?;
                }
                EntryKind::Dir => match fs.remove_empty_dir_live(&path) {
                    Ok(()) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::NotFound,
                        ..
                    }) => {}
                    Err(SyncError::Fs {
                        kind: FsErrorKind::DirectoryNotEmpty,
                        ..
                    }) => {
                        clear_pending_op(store)?;
                        return Ok(ApplyOutcome::LocalChangedRetry);
                    }
                    Err(err) => return Err(err),
                },
            }
        }

        commit_records_and_clear_pending(
            store,
            vec![tombstone],
            persisted_bins,
            vec![path],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    recover_on_non_crash_error(fs, store, res)
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

    // 1. Fetch remote winner into `.legion-sync/tmp/` first.
    let winner_tmp = if winner_record.kind == EntryKind::File {
        let expected_hash = winner_record.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote winner for {orig_path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(
            fs,
            peer,
            &orig_path,
            winner_record.size,
            expected_hash,
            fetch_opts,
        ) {
            Ok(tmp) => Some(tmp),
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: format!("remote winner changed while fetching: {err}"),
                });
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    // 2. Re-check local disk at `orig_path` did not change since scan.
    let existing = store.get_record(&orig_path)?;
    if !disk_matches_index(fs, &orig_path, existing.as_ref())? {
        cleanup_tmps(fs, &[winner_tmp.as_deref()]);
        return handle_local_changed(fs, store, &orig_path, now);
    }

    // 3. Assign a fresh independent version vector to the conflict copy (Item A).
    finalize_independent_conflict_version(store, &mut loser_record)?;

    let conflict_already_present = disk_matches_record(fs, &conflict_path, &loser_record)?;
    if !conflict_already_present && fs.stat_live(&conflict_path)?.is_some() {
        // An unscanned or different file appeared at `conflict_path`! Never overwrite it.
        cleanup_tmps(fs, &[winner_tmp.as_deref()]);
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
    write_pending_op(store, pending)?;

    let res: Result<ApplyOutcome> = (|| {
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
                            cleanup_tmps(fs, &[winner_tmp.as_deref()]);
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &conflict_path, now);
                        }
                        Err(SyncError::Fs {
                            kind: FsErrorKind::NotFound,
                            ..
                        }) => {
                            cleanup_tmps(fs, &[winner_tmp.as_deref()]);
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                    // Verify the file we just moved into `conflict_path` still matches the
                    // expected local bytes (if a person edited `orig_path` in the race window,
                    // put it right back so its real content gets scanned with the right hash!).
                    if let Some(expected_hash) = loser_record.hash {
                        let (actual_hash, actual_size) = fs.hash_live(&conflict_path)?;
                        if actual_hash != expected_hash || actual_size != loser_record.size {
                            let _ = fs.rename_live_to_live_no_replace(&conflict_path, &orig_path);
                            cleanup_tmps(fs, &[winner_tmp.as_deref()]);
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                    }
                    apply_live_metadata(fs, &conflict_path, loser_record.mtime, loser_record.mode)?;
                }
                EntryKind::Symlink => {
                    if let Some(ref target) = loser_record.link_target {
                        match fs.create_symlink_live(&conflict_path, target) {
                            Ok(()) => {}
                            Err(SyncError::Fs {
                                kind: FsErrorKind::AlreadyExists,
                                ..
                            }) => {
                                cleanup_tmps(fs, &[winner_tmp.as_deref()]);
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &conflict_path, now);
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    fs.remove_symlink_live(&orig_path)?;
                }
                EntryKind::Dir => {
                    fs.create_dir_all_live(&conflict_path)?;
                    let _ = fs.remove_empty_dir_live(&orig_path);
                }
            }
            // Capture `conflict_path`'s disk stat immediately after placing and verifying it,
            // before Step 6 runs, so any concurrent edit to `conflict_path` during Step 6 is
            // detected on the next scan.
            record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        } else {
            // Identical conflict copy already exists; safely remove old orig via claim!
            match loser_record.kind {
                EntryKind::Symlink => fs.remove_symlink_live(&orig_path)?,
                EntryKind::Dir => {
                    let _ = fs.remove_empty_dir_live(&orig_path);
                }
                EntryKind::File => {
                    let claim = next_tmp_name("confclaim");
                    match fs.move_live_to_tmp_no_replace(&orig_path, &claim) {
                        Ok(()) => {}
                        Err(SyncError::Fs {
                            kind: FsErrorKind::NotFound,
                            ..
                        }) => {
                            cleanup_tmps(fs, &[winner_tmp.as_deref()]);
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                    let expected_old = loser_record.hash;
                    if !tmp_matches_hash(fs, &claim, expected_old, loser_record.size)? {
                        // Human edited orig_path in race window! Put it right back!
                        let _ = fs.commit_tmp_to_live_no_replace(&claim, &orig_path);
                        cleanup_tmps(fs, &[winner_tmp.as_deref()]);
                        clear_pending_op(store)?;
                        return handle_local_changed(fs, store, &orig_path, now);
                    }
                    let _ = fs.remove_tmp(&claim);
                }
            }
        }

        // 6. Place remote winner at `orig_path` using non-replacing rename!
        match winner_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = winner_tmp {
                    match fs.commit_tmp_to_live_no_replace(tmp, &orig_path) {
                        Ok(()) => {}
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            // Human created a new file at `orig_path` in the microsecond after
                            // Step 5 moved the old loser to `conflict_path`! Commit the safely
                            // moved conflict copy, clean up `winner_tmp`, and rescan `orig_path`
                            // so all three versions survive cleanly.
                            cleanup_tmps(fs, &[Some(tmp.as_str())]);
                            commit_records_and_clear_pending(
                                store,
                                vec![loser_record],
                                Vec::new(),
                                vec![conflict_path],
                                Vec::new(),
                            )?;
                            return handle_local_changed(fs, store, &orig_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                }
                apply_live_metadata(fs, &orig_path, winner_record.mtime, winner_record.mode)?;
            }
            EntryKind::Dir => {
                fs.create_dir_all_live(&orig_path)?;
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
                    Err(SyncError::Fs {
                        kind: FsErrorKind::AlreadyExists,
                        ..
                    }) => {
                        commit_records_and_clear_pending(
                            store,
                            vec![loser_record],
                            Vec::new(),
                            vec![conflict_path],
                            Vec::new(),
                        )?;
                        return handle_local_changed(fs, store, &orig_path, now);
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        record_disk_stat(fs, &orig_path, &mut winner_record)?;

        // 7. Commit BOTH records atomically and clear pending op.
        commit_records_and_clear_pending(
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            vec![orig_path, conflict_path],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    recover_on_non_crash_error(fs, store, res)
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

    // 1. Fetch remote loser into `.legion-sync/tmp/` (fetching from `orig_path` on the peer!).
    let loser_tmp = if loser_record.kind == EntryKind::File {
        let expected_hash = loser_record.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote loser for {orig_path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(
            fs,
            peer,
            &orig_path,
            loser_record.size,
            expected_hash,
            fetch_opts,
        ) {
            Ok(tmp) => Some(tmp),
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::Deferred {
                    reason: format!("remote loser changed while fetching: {err}"),
                });
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    // 2. Re-check local winner at `orig_path` did not change since scan.
    let existing = store.get_record(&orig_path)?;
    if !disk_matches_index(fs, &orig_path, existing.as_ref())? {
        cleanup_tmps(fs, &[loser_tmp.as_deref()]);
        return handle_local_changed(fs, store, &orig_path, now);
    }

    if bump_winner_clock {
        let mut meta = load_meta_required(store)?;
        let vkey = meta.version_key();
        let clock = meta.next_clock();
        winner_record.version.bump_above(vkey, clock);
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
        cleanup_tmps(fs, &[loser_tmp.as_deref()]);
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
    write_pending_op(store, pending)?;

    let res: Result<ApplyOutcome> = (|| {
        if !conflict_already_present {
            match loser_record.kind {
                EntryKind::File => {
                    if let Some(ref tmp) = loser_tmp {
                        match fs.commit_tmp_to_live_no_replace(tmp, &conflict_path) {
                            Ok(()) => {}
                            Err(SyncError::Fs {
                                kind: FsErrorKind::AlreadyExists,
                                ..
                            }) => {
                                let _ = fs.remove_tmp(tmp);
                                clear_pending_op(store)?;
                                return handle_local_changed(fs, store, &conflict_path, now);
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    apply_live_metadata(fs, &conflict_path, loser_record.mtime, loser_record.mode)?;
                }
                EntryKind::Dir => {
                    fs.create_dir_all_live(&conflict_path)?;
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
                        Err(SyncError::Fs {
                            kind: FsErrorKind::AlreadyExists,
                            ..
                        }) => {
                            clear_pending_op(store)?;
                            return handle_local_changed(fs, store, &conflict_path, now);
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
            record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        } else {
            cleanup_tmps(fs, &[loser_tmp.as_deref()]);
        }

        commit_records_and_clear_pending(
            store,
            vec![loser_record, winner_record],
            Vec::new(),
            vec![orig_path, conflict_path],
            Vec::new(),
        )?;
        Ok(ApplyOutcome::Applied)
    })();

    recover_on_non_crash_error(fs, store, res)
}

/// Ensures every strict ancestor of `rel_path` exists as a live directory both on disk and in the
/// index.
fn ensure_ancestors_are_dirs<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_path: &str,
    now: Timestamp,
) -> Result<bool> {
    for anc in ancestors(rel_path) {
        let mut existing = store.get_record(anc)?;
        if !disk_matches_index(fs, anc, existing.as_ref())? {
            match rescan_single_path(fs, store, anc, now)? {
                SingleRescanOutcome::Unstable => return Ok(false),
                SingleRescanOutcome::Updated(rec) => existing = Some(*rec),
                SingleRescanOutcome::Unchanged => existing = store.get_record(anc)?,
            }
        }

        match existing {
            Some(ref rec) if !rec.deleted && rec.kind == EntryKind::Dir => {
                if fs.stat_live(anc)?.is_none() {
                    fs.create_dir_all_live(anc)?;
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
                let vkey = meta.version_key();
                let mut dir_version = rec.version.clone();
                let clock = meta.next_clock();
                dir_version.bump_above(vkey, clock);
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

                let anc_res: Result<bool> = (|| {
                    if !already {
                        match loser.kind {
                            EntryKind::File => {
                                match fs.rename_live_to_live_no_replace(anc, &conflict_path) {
                                    Ok(()) => {}
                                    Err(SyncError::Fs {
                                        kind: FsErrorKind::AlreadyExists | FsErrorKind::NotFound,
                                        ..
                                    }) => {
                                        clear_pending_op(store)?;
                                        let _ = rescan_single_path(fs, store, anc, now)?;
                                        return Ok(false);
                                    }
                                    Err(err) => return Err(err),
                                }
                                if let Some(expected_hash) = loser.hash {
                                    let (actual_hash, actual_size) =
                                        fs.hash_live(&conflict_path)?;
                                    if actual_hash != expected_hash || actual_size != loser.size {
                                        let _ =
                                            fs.rename_live_to_live_no_replace(&conflict_path, anc);
                                        clear_pending_op(store)?;
                                        let _ = rescan_single_path(fs, store, anc, now)?;
                                        return Ok(false);
                                    }
                                }
                                apply_live_metadata(fs, &conflict_path, loser.mtime, loser.mode)?;
                            }
                            EntryKind::Symlink => {
                                if let Some(ref target) = loser.link_target {
                                    fs.create_symlink_live(&conflict_path, target)?;
                                }
                                fs.remove_symlink_live(anc)?;
                            }
                            EntryKind::Dir => {}
                        }
                        record_disk_stat(fs, &conflict_path, &mut loser)?;
                    } else {
                        match loser.kind {
                            EntryKind::File => {
                                let claim = next_tmp_name("ancclaim");
                                match fs.move_live_to_tmp_no_replace(anc, &claim) {
                                    Ok(()) => {}
                                    Err(SyncError::Fs {
                                        kind: FsErrorKind::NotFound,
                                        ..
                                    }) => {
                                        clear_pending_op(store)?;
                                        let _ = rescan_single_path(fs, store, anc, now)?;
                                        return Ok(false);
                                    }
                                    Err(err) => return Err(err),
                                }
                                if !tmp_matches_hash(fs, &claim, loser.hash, loser.size)? {
                                    let _ = fs.commit_tmp_to_live_no_replace(&claim, anc);
                                    clear_pending_op(store)?;
                                    let _ = rescan_single_path(fs, store, anc, now)?;
                                    return Ok(false);
                                }
                                let _ = fs.remove_tmp(&claim);
                            }
                            EntryKind::Symlink => fs.remove_symlink_live(anc)?,
                            EntryKind::Dir => {}
                        }
                    }
                    fs.create_dir_all_live(anc)?;
                    record_disk_stat(fs, anc, &mut dir_record)?;
                    commit_records_and_clear_pending(
                        store,
                        vec![loser, dir_record],
                        Vec::new(),
                        vec![anc.to_string(), conflict_path],
                        Vec::new(),
                    )?;
                    Ok(true)
                })();
                if !recover_on_non_crash_error(fs, store, anc_res)? {
                    return Ok(false);
                }
            }
            prev_deleted_or_none => {
                fs.create_dir_all_live(anc)?;
                let mut meta = load_meta_required(store)?;
                let vkey = meta.version_key();
                let mut version = prev_deleted_or_none
                    .as_ref()
                    .map_or_else(VersionVector::new, |p| p.version.clone());
                let clock = meta.next_clock();
                version.bump_above(vkey, clock);
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
                meta.record_seq_fingerprint(seq, dir_rec.head_fingerprint());
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    upsert_records: vec![dir_rec],
                    ..StoreBatch::default()
                };
                store.apply_batch(batch)?;
            }
        }
    }
    Ok(true)
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
    let vkey = meta.version_key();

    if let Some(existing) = store.get_record(&loser_record.path)? {
        if !existing.deleted && existing.content_equal(loser_record) {
            loser_record.version = existing.version;
            return Ok(());
        }
        if existing.deleted {
            let mut version = existing.version;
            let clock = meta.next_clock();
            version.bump_above(vkey, clock);
            loser_record.version = version;
            let batch = StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            return Ok(());
        }
    }

    let clock = meta.next_clock();
    loser_record.version = VersionVector::singleton(vkey, clock);
    let batch = StoreBatch {
        folder_meta: Some(meta),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)
}

/// Expires bin entries older than `retention_secs`, refusing to run when `now` is implausible
/// (earlier than [`MIN_PLAUSIBLE_CLOCK_SECS`] or earlier than the newest bin entry).
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
            if !entry.storage_name.is_empty() {
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

    if !ensure_ancestors_are_dirs(fs, store, &target_path, now)? {
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
    let replacement_bin = if existing_live.is_some() {
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

    let staged_tmp = match entry.kind {
        EntryKind::File => {
            let expected_hash = entry.hash.ok_or_else(|| SyncError::CorruptData {
                context: format!("bin entry {}", entry.id),
                detail: "file bin entry missing hash".to_string(),
            })?;
            let (actual_hash, actual_size) = fs.hash_bin(&entry.storage_name)?;
            if actual_hash != expected_hash {
                return Err(SyncError::HashMismatch {
                    path: entry.original_path.clone(),
                    expected: expected_hash,
                    actual: actual_hash,
                });
            }
            if actual_size != entry.size {
                return Err(SyncError::SizeMismatch {
                    path: entry.original_path.clone(),
                    expected: entry.size,
                    actual: actual_size,
                });
            }
            let tmp_name = next_tmp_name("restore");
            fs.copy_bin_to_tmp(&entry.storage_name, &tmp_name)?;
            Some(tmp_name)
        }
        EntryKind::Symlink | EntryKind::Dir => None,
    };

    let claim_tmp = if existing_live.is_some_and(|r| r.kind == EntryKind::File) {
        Some(next_tmp_name("restoreclaim"))
    } else {
        None
    };

    let mut meta = load_meta_required(store)?;
    let vkey = meta.version_key();
    let mut version = existing
        .as_ref()
        .map_or_else(VersionVector::new, |p| p.version.clone());
    let clock = meta.next_clock();
    version.bump_above(vkey, clock);

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
        ..StoreBatch::default()
    };
    store.apply_batch(pre_batch)?;

    let res: Result<String> = (|| {
        let mut persisted_bins = Vec::new();

        // Atomic swap/replace when replacing an existing live file with a restored file.
        if let (Some(old_rec), EntryKind::File, Some(tmp), Some(claim), Some(rep_bin)) = (
            existing_live.filter(|r| r.kind == EntryKind::File),
            entry.kind,
            staged_tmp.as_deref(),
            claim_tmp.as_deref(),
            replacement_bin.as_ref(),
        ) {
            let old_hash = old_rec.hash.ok_or_else(|| SyncError::CorruptData {
                context: format!("local record for {target_path}"),
                detail: "live file missing BLAKE3 hash".to_string(),
            })?;
            match safe_replace_live_file(
                fs,
                tmp,
                claim,
                &target_path,
                old_hash,
                old_rec.size,
                rep_bin,
            )? {
                SafeReplaceResult::Replaced => {
                    persisted_bins.push(rep_bin.clone());
                }
                SafeReplaceResult::RaceChanged => {
                    cleanup_tmps(fs, &[Some(tmp), Some(claim)]);
                    clear_pending_op(store)?;
                    let _ = rescan_single_path(fs, store, &target_path, now)?;
                    return Err(SyncError::Conflict {
                        path: target_path,
                        reason: "file on disk changed during restore; review it and retry"
                            .to_string(),
                    });
                }
                SafeReplaceResult::RaceChangedAfterBin => {
                    cleanup_tmps(fs, &[Some(tmp), Some(claim)]);
                    let batch = StoreBatch {
                        upsert_bin: vec![rep_bin.clone()],
                        set_pending_op: Some(None),
                        ..StoreBatch::default()
                    };
                    store.apply_batch(batch)?;
                    let _ = rescan_single_path(fs, store, &target_path, now)?;
                    return Err(SyncError::Conflict {
                        path: target_path,
                        reason: "file on disk changed during restore; review it and retry"
                            .to_string(),
                    });
                }
            }
            apply_live_metadata(fs, &target_path, entry.original_mtime, entry.mode)?;
        } else {
            // Move aside existing non-file or cross-kind entry safely first.
            if let Some(old_rec) = existing_live {
                match old_rec.kind {
                    EntryKind::File => {
                        if let (Some(claim), Some(rep_bin), Some(old_hash)) =
                            (claim_tmp.as_deref(), replacement_bin.as_ref(), old_rec.hash)
                        {
                            match safe_delete_live_file(
                                fs,
                                &target_path,
                                claim,
                                old_hash,
                                old_rec.size,
                                rep_bin,
                            )? {
                                SafeDeleteResult::Deleted => persisted_bins.push(rep_bin.clone()),
                                SafeDeleteResult::RaceChanged => {
                                    cleanup_tmps(fs, &[staged_tmp.as_deref(), Some(claim)]);
                                    clear_pending_op(store)?;
                                    let _ = rescan_single_path(fs, store, &target_path, now)?;
                                    return Err(SyncError::Conflict {
                                        path: target_path,
                                        reason: "file on disk changed during restore".to_string(),
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
                                let batch = StoreBatch {
                                    upsert_bin: persisted_bins,
                                    set_pending_op: Some(None),
                                    ..StoreBatch::default()
                                };
                                store.apply_batch(batch)?;
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
                    apply_live_metadata(fs, &target_path, entry.original_mtime, entry.mode)?;
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

        record_disk_stat(fs, &target_path, &mut restored_record)?;
        commit_records_and_clear_pending(
            store,
            vec![restored_record],
            persisted_bins,
            vec![target_path.clone()],
            vec![entry.id.clone()],
        )?;

        if !entry.storage_name.is_empty() {
            let _ = fs.remove_bin_file(&entry.storage_name);
        }
        let _ = fs.remove_bin_file(&bin_meta_name(&entry.id));

        Ok(target_path)
    })();

    recover_on_non_crash_error(fs, store, res)
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
            let vkey = meta.version_key();
            let bin_entry = build_bin_entry(
                Some(&conflict_rec),
                BinReason::ReplacedLocally,
                &meta.local_member,
                now,
            );
            let claim_tmp = if conflict_rec.kind == EntryKind::File {
                Some(next_tmp_name("resdel"))
            } else {
                None
            };

            let mut version = conflict_rec.version.clone();
            let clock = meta.next_clock();
            version.bump_above(vkey, clock);
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
                            (claim_tmp.as_deref(), bin_entry.as_ref(), conflict_rec.hash)
                        {
                            match safe_delete_live_file(
                                fs,
                                &clean_conflict,
                                claim,
                                old_hash,
                                conflict_rec.size,
                                b_entry,
                            )? {
                                SafeDeleteResult::Deleted => persisted_bins.push(b_entry.clone()),
                                SafeDeleteResult::RaceChanged => {
                                    clear_pending_op(store)?;
                                    let _ = rescan_single_path(fs, store, &clean_conflict, now)?;
                                    return Err(SyncError::Conflict {
                                        path: clean_conflict,
                                        reason: "conflict copy changed on disk during resolution"
                                            .to_string(),
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
                    store,
                    vec![tombstone],
                    persisted_bins,
                    vec![clean_conflict],
                    Vec::new(),
                )?;
                Ok(())
            })();

            recover_on_non_crash_error(fs, store, res)
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
            let vkey = meta.version_key();
            let orig_live = orig_rec.as_ref().filter(|r| !r.deleted);
            let bin_entry = build_bin_entry(
                orig_live,
                BinReason::ReplacedLocally,
                &meta.local_member,
                now,
            );
            let claim_tmp = if orig_live.is_some_and(|r| r.kind == EntryKind::File) {
                Some(next_tmp_name("reskeepclaim"))
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
            let c1 = meta.next_clock();
            orig_version.bump_above(vkey.clone(), c1);

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
            let c2 = meta.next_clock();
            conflict_version.bump_above(vkey, c2);
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
                                (claim_tmp.as_deref(), bin_entry.as_ref(), orig.hash)
                            {
                                match safe_delete_live_file(
                                    fs, &orig_path, claim, old_hash, orig.size, b_entry,
                                )? {
                                    SafeDeleteResult::Deleted => {
                                        persisted_bins.push(b_entry.clone());
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
                                let batch = StoreBatch {
                                    upsert_bin: persisted_bins,
                                    set_pending_op: Some(None),
                                    ..StoreBatch::default()
                                };
                                store.apply_batch(batch)?;
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
                    store,
                    vec![deleted_conflict, promoted],
                    persisted_bins,
                    vec![orig_path, clean_conflict],
                    Vec::new(),
                )?;
                Ok(())
            })();

            recover_on_non_crash_error(fs, store, res)
        }
        ConflictChoice::KeepBoth => {
            let mut meta = load_meta_required(store)?;
            let vkey = meta.version_key();
            let mut updated = conflict_rec;
            let clock = meta.next_clock();
            updated.version.bump_above(vkey, clock);
            let seq = meta.next_seq();
            updated.seq = seq;
            updated.conflict_dismissed = true;
            updated.conflict_of = None;
            meta.record_seq_fingerprint(seq, updated.head_fingerprint());

            let batch = StoreBatch {
                folder_meta: Some(meta),
                upsert_records: vec![updated],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(())
        }
    }
}

fn safe_replace_live_file<F: SyncFs>(
    fs: &F,
    staged_tmp: &str,
    claim_tmp: &str,
    live_path: &str,
    expected_old_hash: Hash,
    expected_old_size: u64,
    bin_entry: &BinEntry,
) -> Result<SafeReplaceResult> {
    // Path 1: atomic exchange (`renameat2(RENAME_EXCHANGE)` / `renamex_np(RENAME_SWAP)`).
    match fs.exchange_tmp_with_live(staged_tmp, live_path) {
        Ok(true) => {
            if tmp_matches_hash(fs, staged_tmp, Some(expected_old_hash), expected_old_size)? {
                write_bin_meta_file(fs, bin_entry)?;
                fs.move_tmp_to_bin_no_replace(staged_tmp, &bin_entry.storage_name)?;
                return Ok(SafeReplaceResult::Replaced);
            }
            // Human modified `live_path` in the race window! Swap their file RIGHT BACK!
            let _ = fs.exchange_tmp_with_live(staged_tmp, live_path);
            return Ok(SafeReplaceResult::RaceChanged);
        }
        Ok(false) => {}
        Err(SyncError::Fs {
            kind: FsErrorKind::NotFound,
            ..
        }) => return Ok(SafeReplaceResult::RaceChanged),
        Err(err) => return Err(err),
    }

    // Path 2: atomic replace with backup (`ReplaceFileW` on Windows).
    match fs.replace_live_with_tmp_backup(staged_tmp, live_path, claim_tmp) {
        Ok(true) => {
            if tmp_matches_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)? {
                write_bin_meta_file(fs, bin_entry)?;
                fs.move_tmp_to_bin_no_replace(claim_tmp, &bin_entry.storage_name)?;
                return Ok(SafeReplaceResult::Replaced);
            }
            let undo_tmp = next_tmp_name("undo");
            let _ = fs.replace_live_with_tmp_backup(claim_tmp, live_path, &undo_tmp);
            let _ = fs.remove_tmp(&undo_tmp);
            return Ok(SafeReplaceResult::RaceChanged);
        }
        Ok(false) => {}
        Err(SyncError::Fs {
            kind: FsErrorKind::NotFound,
            ..
        }) => return Ok(SafeReplaceResult::RaceChanged),
        Err(err) => return Err(err),
    }

    // Path 3: claim-then-rename fallback (safe against race edits AND race creates).
    match fs.move_live_to_tmp_no_replace(live_path, claim_tmp) {
        Ok(()) => {}
        Err(SyncError::Fs {
            kind: FsErrorKind::NotFound,
            ..
        }) => return Ok(SafeReplaceResult::RaceChanged),
        Err(err) => return Err(err),
    }

    if !tmp_matches_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)? {
        let _ = fs.commit_tmp_to_live_no_replace(claim_tmp, live_path);
        return Ok(SafeReplaceResult::RaceChanged);
    }

    write_bin_meta_file(fs, bin_entry)?;
    fs.move_tmp_to_bin_no_replace(claim_tmp, &bin_entry.storage_name)?;

    match fs.commit_tmp_to_live_no_replace(staged_tmp, live_path) {
        Ok(()) => Ok(SafeReplaceResult::Replaced),
        Err(SyncError::Fs {
            kind: FsErrorKind::AlreadyExists,
            ..
        }) => Ok(SafeReplaceResult::RaceChangedAfterBin),
        Err(err) => Err(err),
    }
}

fn safe_delete_live_file<F: SyncFs>(
    fs: &F,
    live_path: &str,
    claim_tmp: &str,
    expected_old_hash: Hash,
    expected_old_size: u64,
    bin_entry: &BinEntry,
) -> Result<SafeDeleteResult> {
    match fs.move_live_to_tmp_no_replace(live_path, claim_tmp) {
        Ok(()) => {}
        Err(SyncError::Fs {
            kind: FsErrorKind::NotFound,
            ..
        }) => return Ok(SafeDeleteResult::RaceChanged),
        Err(err) => return Err(err),
    }

    if !tmp_matches_hash(fs, claim_tmp, Some(expected_old_hash), expected_old_size)? {
        let _ = fs.commit_tmp_to_live_no_replace(claim_tmp, live_path);
        return Ok(SafeDeleteResult::RaceChanged);
    }

    write_bin_meta_file(fs, bin_entry)?;
    fs.move_tmp_to_bin_no_replace(claim_tmp, &bin_entry.storage_name)?;
    Ok(SafeDeleteResult::Deleted)
}

fn recover_on_non_crash_error<F: SyncFs, S: IndexStore, T>(
    fs: &F,
    store: &mut S,
    result: Result<T>,
) -> Result<T> {
    match result {
        Ok(val) => Ok(val),
        Err(err) => {
            if !err.is_crash() {
                let _ = recover_pending(fs, store);
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

/// Fetches `remote_path` into `.legion-sync/tmp/`, capping reads strictly at
/// `expected_size + 1` bytes, enforcing timeouts & cancellation, supporting ranged resume on
/// mid-stream interruptions, and verifying the full-file BLAKE3 digest.
fn fetch_and_verify_to_tmp<F: SyncFs>(
    fs: &F,
    peer: &dyn Peer,
    remote_path: &str,
    expected_size: u64,
    expected_hash: Hash,
    base_opts: &FetchOptions,
) -> Result<String> {
    let (tmp_name, writer) = fetch_and_verify_to_tmp_staged(
        fs,
        peer,
        remote_path,
        expected_size,
        expected_hash,
        base_opts,
    )?;
    if let Err(err) = writer.finish_sync() {
        let _ = fs.remove_tmp(&tmp_name);
        return Err(err);
    }
    Ok(tmp_name)
}

fn fetch_and_verify_to_tmp_staged<F: SyncFs>(
    fs: &F,
    peer: &dyn Peer,
    remote_path: &str,
    expected_size: u64,
    expected_hash: Hash,
    base_opts: &FetchOptions,
) -> Result<(String, Box<dyn SyncWrite>)> {
    let tmp_name = next_tmp_name("fetch");
    let started = Instant::now();
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
            let _ = fs.remove_tmp(&tmp_name);
            return Err(SyncError::Cancelled {
                operation: format!("fetch {remote_path}"),
            });
        }
        let elapsed = started.elapsed();
        if elapsed > base_opts.timeout {
            let _ = fs.remove_tmp(&tmp_name);
            return Err(SyncError::Timeout {
                operation: format!("fetch {remote_path}"),
                detail: format!("exceeded {:?}", base_opts.timeout),
            });
        }
        let remaining_timeout = base_opts.timeout.saturating_sub(elapsed);
        let opts =
            FetchOptions::at_offset(bytes_written, remaining_timeout, base_opts.cancel.clone());

        let mut reader = match peer.fetch_file(remote_path, expected_hash, &opts) {
            Ok(r) => r,
            Err(err) => {
                let _ = fs.remove_tmp(&tmp_name);
                return Err(err);
            }
        };

        let mut writer = if bytes_written == 0 {
            fs.create_tmp(&tmp_name)?
        } else {
            match fs.open_tmp_append(&tmp_name) {
                Ok(w) => w,
                Err(err) => {
                    let _ = fs.remove_tmp(&tmp_name);
                    return Err(err);
                }
            }
        };

        loop {
            if base_opts.is_cancelled() {
                drop(writer);
                let _ = fs.remove_tmp(&tmp_name);
                return Err(SyncError::Cancelled {
                    operation: format!("fetch {remote_path}"),
                });
            }
            if started.elapsed() > base_opts.timeout {
                drop(writer);
                let _ = fs.remove_tmp(&tmp_name);
                return Err(SyncError::Timeout {
                    operation: format!("fetch {remote_path}"),
                    detail: format!("exceeded {:?}", base_opts.timeout),
                });
            }

            let remaining_cap = max_bytes.saturating_sub(bytes_written);
            if remaining_cap == 0 {
                // Already read `expected_size + 1` bytes — stop immediately without reading more!
                drop(writer);
                let _ = fs.remove_tmp(&tmp_name);
                return Err(SyncError::SizeMismatch {
                    path: remote_path.to_string(),
                    expected: expected_size,
                    actual: bytes_written,
                });
            }

            let to_read = (remaining_cap as usize).min(buf.len());
            match reader.read(&mut buf[..to_read]) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(err) = writer.write_all(&buf[..n]) {
                        drop(writer);
                        let _ = fs.remove_tmp(&tmp_name);
                        return Err(SyncError::from_io(Some(remote_path.to_string()), err));
                    }
                    hasher.update(&buf[..n]);
                    bytes_written = bytes_written.saturating_add(n as u64);
                    if bytes_written > expected_size {
                        drop(writer);
                        let _ = fs.remove_tmp(&tmp_name);
                        return Err(SyncError::SizeMismatch {
                            path: remote_path.to_string(),
                            expected: expected_size,
                            actual: bytes_written,
                        });
                    }
                }
                Err(err) => {
                    if bytes_written > 0 && resume_attempts < MAX_FETCH_RESUMES {
                        // Sync partial bytes and attempt a ranged resume from `bytes_written`.
                        let _ = writer.finish_sync();
                        resume_attempts += 1;
                        continue 'outer;
                    }
                    drop(writer);
                    let _ = fs.remove_tmp(&tmp_name);
                    return Err(SyncError::from_io(Some(remote_path.to_string()), err));
                }
            }
        }

        if let Err(err) = writer.flush() {
            drop(writer);
            let _ = fs.remove_tmp(&tmp_name);
            return Err(SyncError::from_io(Some(remote_path.to_string()), err));
        }
        break writer;
    };

    if bytes_written != expected_size {
        drop(final_writer);
        let _ = fs.remove_tmp(&tmp_name);
        return Err(SyncError::SizeMismatch {
            path: remote_path.to_string(),
            expected: expected_size,
            actual: bytes_written,
        });
    }

    let actual_hash = Hash::from_bytes(*hasher.finalize().as_bytes());
    if actual_hash != expected_hash {
        drop(final_writer);
        let _ = fs.remove_tmp(&tmp_name);
        return Err(SyncError::HashMismatch {
            path: remote_path.to_string(),
            expected: expected_hash,
            actual: actual_hash,
        });
    }

    Ok((tmp_name, final_writer))
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
            let (actual_hash, read_len) = fs.hash_live(path)?;
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
            let (actual_hash, read_len) = fs.hash_live(path)?;
            Ok(read_len == rec.size && rec.hash == Some(actual_hash))
        }
    }
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
    if fs.stat_tmp(tmp_name)?.is_none() {
        return Ok(false);
    }
    let (actual_hash, actual_size) = fs.hash_tmp(tmp_name)?;
    Ok(actual_hash == expected && actual_size == expected_size)
}

fn find_present_tmp<F: SyncFs>(fs: &F, candidates: &[Option<&str>]) -> Result<Option<String>> {
    for cand in candidates.iter().flatten() {
        if fs.stat_tmp(cand)?.is_some() {
            return Ok(Some((*cand).to_string()));
        }
    }
    Ok(None)
}

fn cleanup_tmps<F: SyncFs>(fs: &F, candidates: &[Option<&str>]) {
    for cand in candidates.iter().flatten() {
        let _ = fs.remove_tmp(cand);
    }
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

fn commit_records_and_clear_pending<S: IndexStore>(
    store: &mut S,
    records: Vec<IndexRecord>,
    bin_entries: Vec<BinEntry>,
    clear_held_back_paths: Vec<String>,
    remove_bin_ids: Vec<String>,
) -> Result<()> {
    let mut meta = load_meta_required(store)?;
    let mut batch = StoreBatch::default();

    for mut rec in records {
        let seq = meta.next_seq();
        rec.seq = seq;
        if rec.deleted && !rec.recent_hashes.is_empty() {
            batch
                .upsert_deleted_history
                .push((rec.path.clone(), rec.recent_hashes.clone()));
        }
        rec.conflict_of = derive_conflict_of(&rec, &meta);
        meta.record_seq_fingerprint(seq, rec.head_fingerprint());
        batch.upsert_records.push(rec);
    }

    batch.folder_meta = Some(meta);
    batch.upsert_bin = bin_entries;
    batch.remove_bin = remove_bin_ids;
    batch.remove_held_back = clear_held_back_paths;
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
