//! Crash-safe application of reconciled decisions, startup recovery, bin management, and
//! conflict resolution.
//!
//! Every filesystem mutation is bracketed by a durable [`PendingOp`] write-ahead record in the
//! [`IndexStore`]. If a crash occurs at any step — mid-download, after moving the old copy to
//! `.legion-sync/bin`, after moving a conflict loser aside, or after renaming a staged file into
//! place — [`recover_pending`] deterministically finishes or rolls back the operation on restart
//! with zero data loss and zero spurious conflicts.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use legion_xfer::{EntryKind, Hash, HashingWriter, Timestamp};

use crate::error::{FsErrorKind, Result, SyncError};
use crate::model::{
    BinEntry, BinReason, ConflictChoice, ExpectedLocalSnapshot, FolderMeta, IndexRecord, PendingOp,
    RestoreStrategy,
};
use crate::path::{ancestors, restore_copy_path, validate_sync_path};
use crate::reconcile::{LocalIndexView, ReconcileDecision};
use crate::scan::{SingleRescanOutcome, disk_matches_index, rescan_single_path};
use crate::traits::{FsMeta, IndexStore, Peer, StoreBatch, SyncFs};
use crate::version::VersionVector;

static OP_NONCE: AtomicU64 = AtomicU64::new(1);

/// Result of attempting to apply one [`ReconcileDecision`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Decision was applied and committed to the index.
    Applied,
    /// Local disk changed since the last scan; the path was rescanned locally and the caller
    /// should re-reconcile it.
    LocalChangedRetry,
    /// Local file is actively being written; defer this path until a later sync.
    DeferredUnstable,
}

/// Summary of work performed by [`recover_pending`] on startup.
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
}

/// Recovers any incomplete [`PendingOp`] left by a crash and cleans orphaned staging files from
/// `.legion-sync/tmp/`.
///
/// Must run on startup before any scan or sync pass. This routine is idempotent: crashing again
/// during recovery converges cleanly on the next restart.
pub fn recover_pending<F: SyncFs, S: IndexStore>(fs: &F, store: &mut S) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();

    if let Some(op) = store.load_pending_op()? {
        report.recovered_pending_op = true;
        match op {
            PendingOp::Place {
                target_record,
                staged_tmp,
                bin_entry,
                expected_local,
            } => {
                recover_place(
                    fs,
                    store,
                    target_record,
                    staged_tmp,
                    bin_entry,
                    expected_local,
                )?;
            }
            PendingOp::Delete {
                target_record,
                bin_entry,
                expected_local,
            } => {
                recover_delete(fs, store, target_record, bin_entry, expected_local)?;
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
        }
    }

    // Sweep any leftover files in `.legion-sync/tmp/` after recovery completes.
    for tmp_name in fs.list_tmp()? {
        fs.remove_tmp(&tmp_name)?;
        report.cleaned_tmp_files += 1;
    }

    Ok(report)
}

fn recover_place<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    mut target_record: IndexRecord,
    staged_tmp: Option<String>,
    bin_entry: Option<BinEntry>,
    expected_local: ExpectedLocalSnapshot,
) -> Result<()> {
    let path = target_record.path.clone();

    // Case 1: target is already materialized on disk with expected content -> commit & finish.
    if disk_matches_record(fs, &path, &target_record)? {
        record_disk_stat(fs, &path, &mut target_record)?;
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(store, vec![target_record], persisted_bin, vec![path])?;
        if let Some(tmp) = staged_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        return Ok(());
    }

    // Case 2: disk still holds the untouched pre-apply entry -> safely roll back.
    if disk_matches_snapshot(fs, &path, &expected_local)?
        && !matches!(expected_local, ExpectedLocalSnapshot::Missing)
    {
        if let Some(tmp) = staged_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        if let Some(ref b) = bin_entry {
            // Only remove bin sidecar if no data was moved yet.
            if b.storage_name.is_empty() || fs.stat_bin(&b.storage_name)?.is_none() {
                let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
            }
        }
        clear_pending_op(store)?;
        return Ok(());
    }

    // Case 3: path is missing on disk (crash happened right after moving old entry to bin and
    // before placing the new entry). Roll forward if staged payload is intact, else restore bin.
    if fs.stat_live(&path)?.is_none() {
        match target_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = staged_tmp
                    && tmp_matches_hash(fs, tmp, target_record.hash, target_record.size)?
                {
                    fs.commit_tmp_to_live(tmp, &path)?;
                    let _ = fs.set_live_metadata(&path, target_record.mtime, target_record.mode);
                    record_disk_stat(fs, &path, &mut target_record)?;
                    let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
                    commit_records_and_clear_pending(
                        store,
                        vec![target_record],
                        persisted_bin,
                        vec![path],
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
                    fs.commit_tmp_to_live(&restore_tmp, &path)?;
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
                    persisted_bin,
                    vec![path],
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
                        persisted_bin,
                        vec![path],
                    )?;
                    return Ok(());
                }
            }
        }
    }

    // Case 4: path on disk matches neither expected_local nor target_record (external human edit
    // while crashed). Leave human content untouched and clear the pending journal.
    if let Some(tmp) = staged_tmp {
        let _ = fs.remove_tmp(&tmp);
    }
    clear_pending_op(store)?;
    Ok(())
}

fn recover_delete<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    target_record: IndexRecord,
    bin_entry: Option<BinEntry>,
    expected_local: ExpectedLocalSnapshot,
) -> Result<()> {
    let path = target_record.path.clone();
    if fs.stat_live(&path)?.is_none() {
        let persisted_bin = bin_entry_if_present(fs, bin_entry)?;
        commit_records_and_clear_pending(store, vec![target_record], persisted_bin, vec![path])?;
        return Ok(());
    }

    if disk_matches_snapshot(fs, &path, &expected_local)?
        && let Some(ref b) = bin_entry
        && (b.storage_name.is_empty() || fs.stat_bin(&b.storage_name)?.is_none())
    {
        let _ = fs.remove_bin_file(&bin_meta_name(&b.id));
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

    // Both winner and loser are already in place -> commit both records atomically.
    if winner_done && loser_done {
        record_disk_stat(fs, &orig_path, &mut winner_record)?;
        record_disk_stat(fs, &conflict_path, &mut loser_record)?;
        commit_records_and_clear_pending(
            store,
            vec![loser_record, winner_record],
            None,
            vec![orig_path, conflict_path],
        )?;
        if let Some(tmp) = winner_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        return Ok(());
    }

    // Loser was moved to conflict_path and orig_path is now missing -> complete placing winner!
    if loser_done && fs.stat_live(&orig_path)?.is_none() {
        let placed = match winner_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = winner_tmp
                    && tmp_matches_hash(fs, tmp, winner_record.hash, winner_record.size)?
                {
                    fs.commit_tmp_to_live(tmp, &orig_path)?;
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
                None,
                vec![orig_path, conflict_path],
            )?;
            return Ok(());
        }

        // Staged winner lost externally; restore local file from conflict copy back to orig_path.
        if !conflict_already_present && loser_record.kind == EntryKind::File {
            fs.rename_live_to_live(&conflict_path, &orig_path)?;
        }
        clear_pending_op(store)?;
        return Ok(());
    }

    // Crash happened before touching orig_path -> roll back cleanly.
    if disk_matches_snapshot(fs, &orig_path, &expected_local)? {
        if let Some(tmp) = winner_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        clear_pending_op(store)?;
        return Ok(());
    }

    if let Some(tmp) = winner_tmp {
        let _ = fs.remove_tmp(&tmp);
    }
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
            None,
            vec![orig_path, conflict_path],
        )?;
        if let Some(tmp) = loser_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        return Ok(());
    }

    if let Some(tmp) = loser_tmp {
        let _ = fs.remove_tmp(&tmp);
    }
    clear_pending_op(store)?;
    Ok(())
}

/// Applies one reconciled decision from `peer` crash-safely.
pub fn apply_decision<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    decision: ReconcileDecision,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    match decision {
        ReconcileDecision::RejectUnsafe { path, reason } => {
            Err(SyncError::InvalidPath { path, reason })
        }
        ReconcileDecision::Ignore | ReconcileDecision::Keep => Ok(ApplyOutcome::Applied),
        ReconcileDecision::HoldBack { .. } => Ok(ApplyOutcome::Applied),
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
            commit_records_and_clear_pending(store, vec![record], None, vec![path])?;
            Ok(ApplyOutcome::Applied)
        }
        ReconcileDecision::ReviveDir { path, base_version } => {
            let mut meta = load_meta_required(store)?;
            let mut version = base_version;
            let clock = meta.next_clock();
            version.bump_above(meta.local_member.clone(), clock);
            let seq = meta.next_seq();
            let mut matrix = store.load_causal_matrix()?;
            matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);
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
                tombstone_deps: BTreeMap::new(),
                conflict_of: None,
            };
            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
                upsert_records: vec![revived],
                remove_held_back: vec![path],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(ApplyOutcome::Applied)
        }
        ReconcileDecision::Fetch { record } => apply_fetch(fs, store, peer, record, now),
        ReconcileDecision::DeleteToBin { tombstone } => {
            apply_delete(fs, store, peer.member_id(), tombstone, now)
        }
        ReconcileDecision::ConflictLocalLoses {
            winner_record,
            loser_record,
        } => apply_conflict_local_loses(fs, store, peer, winner_record, loser_record, now),
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
            now,
        ),
    }
}

fn apply_fetch<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    mut target: IndexRecord,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let path = target.path.clone();

    // 1. Fetch & verify into `.legion-sync/tmp/` before touching the live tree.
    let staged_tmp = if target.kind == EntryKind::File {
        let expected_hash = target.hash.ok_or_else(|| SyncError::CorruptData {
            context: format!("remote record for {path}"),
            detail: "live file record missing BLAKE3 hash".to_string(),
        })?;
        match fetch_and_verify_to_tmp(fs, peer, &path, target.size, expected_hash) {
            Ok(tmp) => Some(tmp),
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::DeferredUnstable);
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    // 2. Ensure ancestors are live directories (resolving any ancestor file/symlink conflict or
    // reviving deleted ancestor directories).
    match ensure_ancestors_are_dirs(fs, store, &path, now) {
        Ok(true) => {}
        Ok(false) => {
            if let Some(tmp) = staged_tmp {
                let _ = fs.remove_tmp(&tmp);
            }
            return Ok(ApplyOutcome::DeferredUnstable);
        }
        Err(err) => {
            if let Some(tmp) = staged_tmp {
                let _ = fs.remove_tmp(&tmp);
            }
            return Err(err);
        }
    }

    // 3. Re-check local disk against our index before replacing anything.
    let existing = store.get_record(&path)?;
    if !disk_matches_index(fs, &path, existing.as_ref())? {
        if let Some(tmp) = staged_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        return handle_local_changed(fs, store, &path, now);
    }

    // If replacing a live directory with a file/symlink, check whether the directory on disk
    // actually has unscanned children!
    if let Some(ref loc) = existing
        && !loc.deleted
        && loc.kind == EntryKind::Dir
        && target.kind != EntryKind::Dir
        && !fs.list_dir_live(&path)?.is_empty()
    {
        if let Some(tmp) = staged_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        // Scan children before trying again so reconcile handles the ancestor conflict cleanly.
        return Ok(ApplyOutcome::LocalChangedRetry);
    }

    let expected_local = snapshot_local(existing.as_ref());
    let bin_entry = build_bin_entry(
        existing.as_ref(),
        BinReason::ReplacedByPeer,
        peer.member_id(),
        now,
    );

    // 4. Persist write-ahead intent BEFORE mutating live files.
    let pending = PendingOp::Place {
        target_record: target.clone(),
        staged_tmp: staged_tmp.clone(),
        bin_entry: bin_entry.clone(),
        expected_local,
    };
    write_pending_op(store, pending)?;

    // 5. Move existing live entry to `.legion-sync/bin` (if any).
    move_existing_to_bin(fs, existing.as_ref(), bin_entry.as_ref())?;

    // 6. Place the new entry into the live working tree.
    match target.kind {
        EntryKind::File => {
            if let Some(ref tmp) = staged_tmp {
                fs.commit_tmp_to_live(tmp, &path)?;
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
            fs.create_symlink_live(&path, link_target)?;
        }
    }

    record_disk_stat(fs, &path, &mut target)?;

    // 7. Atomically commit the new record + bin metadata and clear the write-ahead intent.
    commit_records_and_clear_pending(store, vec![target], bin_entry, vec![path])?;
    Ok(ApplyOutcome::Applied)
}

fn apply_delete<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer_member: &str,
    tombstone: IndexRecord,
    now: Timestamp,
) -> Result<ApplyOutcome> {
    let path = tombstone.path.clone();
    let existing = store.get_record(&path)?;

    if !disk_matches_index(fs, &path, existing.as_ref())? {
        return handle_local_changed(fs, store, &path, now);
    }

    // If deleting a directory, double-check the directory on disk is empty (an unscanned local
    // child file must not be lost).
    if let Some(ref loc) = existing
        && !loc.deleted
        && loc.kind == EntryKind::Dir
        && (store.has_live_descendant(&path)? || !fs.list_dir_live(&path)?.is_empty())
    {
        return Ok(ApplyOutcome::LocalChangedRetry);
    }

    let expected_local = snapshot_local(existing.as_ref());
    let bin_entry = build_bin_entry(
        existing.as_ref(),
        BinReason::DeletedByPeer,
        peer_member,
        now,
    );

    let pending = PendingOp::Delete {
        target_record: tombstone.clone(),
        bin_entry: bin_entry.clone(),
        expected_local,
    };
    write_pending_op(store, pending)?;

    move_existing_to_bin(fs, existing.as_ref(), bin_entry.as_ref())?;

    commit_records_and_clear_pending(store, vec![tombstone], bin_entry, vec![path])?;
    Ok(ApplyOutcome::Applied)
}

fn apply_conflict_local_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    mut winner_record: IndexRecord,
    mut loser_record: IndexRecord,
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
        match fetch_and_verify_to_tmp(fs, peer, &orig_path, winner_record.size, expected_hash) {
            Ok(tmp) => Some(tmp),
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::DeferredUnstable);
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    // 2. Re-check local disk at `orig_path` did not change since scan.
    let existing = store.get_record(&orig_path)?;
    if !disk_matches_index(fs, &orig_path, existing.as_ref())? {
        if let Some(tmp) = winner_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        return handle_local_changed(fs, store, &orig_path, now);
    }

    // 3. Finalize loser version vector: use deterministic `loser.version`, bumping local clock
    // only if an unrelated tombstone already exists at `conflict_path` not dominated by it.
    finalize_conflict_loser_version(store, &mut loser_record)?;

    let conflict_already_present = disk_matches_record(fs, &conflict_path, &loser_record)?;
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

    // 5. Move local loser to `conflict_path` FIRST so its bytes are never missing.
    if !conflict_already_present {
        match loser_record.kind {
            EntryKind::File => {
                fs.rename_live_to_live(&orig_path, &conflict_path)?;
                apply_live_metadata(fs, &conflict_path, loser_record.mtime, loser_record.mode)?;
            }
            EntryKind::Symlink => {
                if let Some(ref target) = loser_record.link_target {
                    fs.create_symlink_live(&conflict_path, target)?;
                }
                fs.remove_symlink_live(&orig_path)?;
            }
            EntryKind::Dir => {
                // Directories always win against files/symlinks, so loser is never a Dir.
                fs.create_dir_all_live(&conflict_path)?;
                let _ = fs.remove_empty_dir_live(&orig_path);
            }
        }
    } else {
        // Conflict copy already exists with identical content; remove old orig before placing winner.
        match loser_record.kind {
            EntryKind::Symlink => fs.remove_symlink_live(&orig_path)?,
            EntryKind::Dir => {
                let _ = fs.remove_empty_dir_live(&orig_path);
            }
            EntryKind::File => {
                // `commit_tmp_to_live` replaces atomically for File winners; for Dir/Symlink
                // winners, move aside to conflict_path (identical bytes).
                if winner_record.kind != EntryKind::File {
                    fs.rename_live_to_live(&orig_path, &conflict_path)?;
                }
            }
        }
    }

    // 6. Place remote winner at `orig_path`.
    match winner_record.kind {
        EntryKind::File => {
            if let Some(ref tmp) = winner_tmp {
                fs.commit_tmp_to_live(tmp, &orig_path)?;
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
            fs.create_symlink_live(&orig_path, link_target)?;
        }
    }

    record_disk_stat(fs, &orig_path, &mut winner_record)?;
    record_disk_stat(fs, &conflict_path, &mut loser_record)?;

    // 7. Commit BOTH records atomically and clear pending op.
    commit_records_and_clear_pending(
        store,
        vec![loser_record, winner_record],
        None,
        vec![orig_path, conflict_path],
    )?;
    Ok(ApplyOutcome::Applied)
}

#[allow(clippy::too_many_arguments)]
fn apply_conflict_remote_loses<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    peer: &dyn Peer,
    mut winner_record: IndexRecord,
    mut loser_record: IndexRecord,
    bump_winner_clock: bool,
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
        match fetch_and_verify_to_tmp(fs, peer, &orig_path, loser_record.size, expected_hash) {
            Ok(tmp) => Some(tmp),
            Err(err) if is_peer_file_changed_error(&err) => {
                return Ok(ApplyOutcome::DeferredUnstable);
            }
            Err(err) => return Err(err),
        }
    } else {
        None
    };

    // 2. Re-check local winner at `orig_path` did not change since scan.
    let existing = store.get_record(&orig_path)?;
    if !disk_matches_index(fs, &orig_path, existing.as_ref())? {
        if let Some(tmp) = loser_tmp {
            let _ = fs.remove_tmp(&tmp);
        }
        return handle_local_changed(fs, store, &orig_path, now);
    }

    if bump_winner_clock {
        let mut meta = load_meta_required(store)?;
        let clock = meta.next_clock();
        winner_record
            .version
            .bump_above(meta.local_member.clone(), clock);
        let batch = StoreBatch {
            folder_meta: Some(meta),
            ..StoreBatch::default()
        };
        store.apply_batch(batch)?;
    }

    finalize_conflict_loser_version(store, &mut loser_record)?;

    let conflict_already_present = disk_matches_record(fs, &conflict_path, &loser_record)?;
    let expected_local = snapshot_local(existing.as_ref());

    let pending = PendingOp::ConflictRemoteLoses {
        winner_record: winner_record.clone(),
        loser_record: loser_record.clone(),
        loser_tmp: loser_tmp.clone(),
        conflict_already_present,
        expected_local,
    };
    write_pending_op(store, pending)?;

    if !conflict_already_present {
        match loser_record.kind {
            EntryKind::File => {
                if let Some(ref tmp) = loser_tmp {
                    fs.commit_tmp_to_live(tmp, &conflict_path)?;
                }
                apply_live_metadata(fs, &conflict_path, loser_record.mtime, loser_record.mode)?;
            }
            EntryKind::Dir => {
                fs.create_dir_all_live(&conflict_path)?;
            }
            EntryKind::Symlink => {
                let link_target =
                    loser_record
                        .link_target
                        .as_deref()
                        .ok_or_else(|| SyncError::CorruptData {
                            context: format!("remote loser symlink for {orig_path}"),
                            detail: "symlink record missing link_target".to_string(),
                        })?;
                fs.create_symlink_live(&conflict_path, link_target)?;
            }
        }
    } else if let Some(tmp) = loser_tmp {
        let _ = fs.remove_tmp(&tmp);
    }

    record_disk_stat(fs, &conflict_path, &mut loser_record)?;

    commit_records_and_clear_pending(
        store,
        vec![loser_record, winner_record],
        None,
        vec![orig_path, conflict_path],
    )?;
    Ok(ApplyOutcome::Applied)
}

/// Ensures every strict ancestor of `rel_path` exists as a live directory both on disk and in the
/// index.
///
/// Re-checks each ancestor on disk first so an unscanned local edit, deletion, or creation at an
/// ancestor path is never overwritten or missed. Returns `Ok(false)` if an ancestor file is
/// currently unstable (still being written).
fn ensure_ancestors_are_dirs<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    rel_path: &str,
    now: Timestamp,
) -> Result<bool> {
    for anc in ancestors(rel_path) {
        // Re-check disk against index at `anc` first so unscanned edits/creates/deletes are
        // reflected accurately before mutating the ancestor.
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
                // Ancestor is a live file or symlink; convert it to a directory and preserve the
                // file/symlink at its deterministic conflict copy path.
                let view = StoreIndexView::new(store);
                let conflict_path = crate::reconcile::choose_conflict_path(&view, anc, &rec);
                let mut loser = rec.clone();
                loser.path = conflict_path.clone();
                loser.conflict_of = Some(anc.to_string());
                loser.tombstone_deps.clear();
                finalize_conflict_loser_version(store, &mut loser)?;

                let mut meta = load_meta_required(store)?;
                let mut dir_version = rec.version.clone();
                let clock = meta.next_clock();
                dir_version.bump_above(meta.local_member.clone(), clock);
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
                    tombstone_deps: BTreeMap::new(),
                    conflict_of: None,
                };
                // Save clock bump before crash-sensitive rename/mkdir.
                let meta_batch = StoreBatch {
                    folder_meta: Some(meta),
                    ..StoreBatch::default()
                };
                store.apply_batch(meta_batch)?;

                let already = disk_matches_record(fs, &conflict_path, &loser)?;
                let pending = PendingOp::ConflictLocalLoses {
                    winner_record: dir_record.clone(),
                    loser_record: loser.clone(),
                    winner_tmp: None,
                    conflict_already_present: already,
                    expected_local: snapshot_local(Some(&rec)),
                };
                write_pending_op(store, pending)?;

                if !already {
                    match loser.kind {
                        EntryKind::File => {
                            fs.rename_live_to_live(anc, &conflict_path)?;
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
                } else {
                    match loser.kind {
                        EntryKind::File => fs.rename_live_to_live(anc, &conflict_path)?,
                        EntryKind::Symlink => fs.remove_symlink_live(anc)?,
                        EntryKind::Dir => {}
                    }
                }
                fs.create_dir_all_live(anc)?;
                record_disk_stat(fs, &conflict_path, &mut loser)?;
                record_disk_stat(fs, anc, &mut dir_record)?;
                commit_records_and_clear_pending(
                    store,
                    vec![loser, dir_record],
                    None,
                    vec![anc.to_string(), conflict_path],
                )?;
            }
            prev_deleted_or_none => {
                // Ancestor is absent or deleted (tombstone): create directory and record it in
                // the index so the tree stays consistent.
                fs.create_dir_all_live(anc)?;
                let mut meta = load_meta_required(store)?;
                let mut version = prev_deleted_or_none
                    .as_ref()
                    .map_or_else(VersionVector::new, |p| p.version.clone());
                let clock = meta.next_clock();
                version.bump_above(meta.local_member.clone(), clock);
                let seq = meta.next_seq();
                let mut matrix = store.load_causal_matrix()?;
                matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);
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
                    tombstone_deps: BTreeMap::new(),
                    conflict_of: None,
                };
                let batch = StoreBatch {
                    folder_meta: Some(meta),
                    causal_matrix: Some(matrix),
                    upsert_records: vec![dir_rec],
                    ..StoreBatch::default()
                };
                store.apply_batch(batch)?;
            }
        }
    }
    Ok(true)
}

/// Ensures `loser_record.version` beats any unrelated pre-existing tombstone at `loser_record.path`
/// while preserving the exact deterministic `loser.version` in the normal case.
fn finalize_conflict_loser_version<S: IndexStore>(
    store: &mut S,
    loser_record: &mut IndexRecord,
) -> Result<()> {
    if let Some(existing) = store.get_record(&loser_record.path)? {
        if existing.deleted && !loser_record.version.dominates_or_equal(&existing.version) {
            let mut meta = load_meta_required(store)?;
            loser_record.version.merge_in_place(&existing.version);
            let clock = meta.next_clock();
            loser_record
                .version
                .bump_above(meta.local_member.clone(), clock);
            let batch = StoreBatch {
                folder_meta: Some(meta),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
        } else if !existing.deleted && existing.content_equal(loser_record) {
            loser_record.version.merge_in_place(&existing.version);
        }
    }
    Ok(())
}

/// Expires bin entries older than `retention_secs`, removing both disk files and index metadata.
pub fn expire_bin<F: SyncFs, S: IndexStore>(
    fs: &F,
    store: &mut S,
    retention_secs: u64,
    now: Timestamp,
) -> Result<Vec<BinEntry>> {
    let entries = store.list_bin_entries()?;
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

/// Restores a [`BinEntry`] from `.legion-sync/bin` back into the live tree and bumps its index
/// version so the restored entry propagates to all peers.
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
    if !ensure_ancestors_are_dirs(fs, store, &target_path, now)? {
        return Err(SyncError::Conflict {
            path: target_path,
            reason: "an ancestor file is still being written; retry restoring".to_string(),
        });
    }

    let existing = store.get_record(&target_path)?;
    let replacement_bin =
        if existing.as_ref().is_some_and(|r| !r.deleted) || fs.stat_live(&target_path)?.is_some() {
            if strategy != RestoreStrategy::Replace {
                return Err(SyncError::Exists { path: target_path });
            }
            let local_member = load_meta_required(store)?.local_member;
            build_bin_entry(
                existing.as_ref(),
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
        EntryKind::Symlink => None,
        EntryKind::Dir => None,
    };

    if let Some(ref old_rec) = existing
        && !old_rec.deleted
    {
        move_existing_to_bin(fs, Some(old_rec), replacement_bin.as_ref())?;
    }

    match entry.kind {
        EntryKind::File => {
            if let Some(ref tmp) = staged_tmp {
                fs.commit_tmp_to_live(tmp, &target_path)?;
            }
            let _ = fs.set_live_metadata(&target_path, entry.original_mtime, entry.mode);
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

    let restored_stat = fs.stat_live(&target_path)?;
    let disk_mtime = restored_stat
        .as_ref()
        .map_or(entry.original_mtime, |m| m.mtime);

    let mut meta = load_meta_required(store)?;
    let mut version = existing
        .as_ref()
        .map_or_else(VersionVector::new, |p| p.version.clone());
    let clock = meta.next_clock();
    version.bump_above(meta.local_member.clone(), clock);
    let seq = meta.next_seq();
    let mut matrix = store.load_causal_matrix()?;
    matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);

    let restored_record = IndexRecord {
        path: target_path.clone(),
        kind: entry.kind,
        size: entry.size,
        mtime: entry.original_mtime,
        disk_mtime: Some(disk_mtime),
        disk_change_id: restored_stat.and_then(|m| m.change_id),
        mode: entry.mode,
        hash: entry.hash,
        link_target: entry.link_target.clone(),
        deleted: false,
        version,
        modified_by: meta.local_member.clone(),
        seq,
        tombstone_deps: BTreeMap::new(),
        conflict_of: None,
    };

    let batch = StoreBatch {
        folder_meta: Some(meta),
        causal_matrix: Some(matrix),
        upsert_records: vec![restored_record],
        remove_bin: vec![entry.id.clone()],
        upsert_bin: replacement_bin.into_iter().collect(),
        ..StoreBatch::default()
    };
    store.apply_batch(batch)?;

    if !entry.storage_name.is_empty() {
        let _ = fs.remove_bin_file(&entry.storage_name);
    }
    let _ = fs.remove_bin_file(&bin_meta_name(&entry.id));

    Ok(target_path)
}

/// Resolves a file with two versions according to `choice`.
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
    let orig_path = conflict_rec
        .conflict_of
        .clone()
        .ok_or_else(|| SyncError::Conflict {
            path: clean_conflict.clone(),
            reason: "entry is not marked as a conflict copy".to_string(),
        })?;

    match choice {
        ConflictChoice::KeepOriginal => {
            let mut meta = load_meta_required(store)?;
            let bin_entry = build_bin_entry(
                Some(&conflict_rec),
                BinReason::ReplacedLocally,
                &meta.local_member,
                now,
            );
            move_existing_to_bin(fs, Some(&conflict_rec), bin_entry.as_ref())?;

            let mut version = conflict_rec.version;
            let clock = meta.next_clock();
            version.bump_above(meta.local_member.clone(), clock);
            let seq = meta.next_seq();
            let mut deps = BTreeMap::new();
            deps.insert(meta.local_member.clone(), seq);
            let mut matrix = store.load_causal_matrix()?;
            matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);

            let tombstone = IndexRecord {
                path: clean_conflict,
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
                seq,
                tombstone_deps: deps,
                conflict_of: None,
            };
            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
                upsert_records: vec![tombstone],
                upsert_bin: bin_entry.into_iter().collect(),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(())
        }
        ConflictChoice::KeepConflict => {
            let orig_rec = store.get_record(&orig_path)?;
            let mut meta = load_meta_required(store)?;
            let bin_entry = build_bin_entry(
                orig_rec.as_ref(),
                BinReason::ReplacedLocally,
                &meta.local_member,
                now,
            );
            if let Some(ref orig) = orig_rec
                && !orig.deleted
            {
                move_existing_to_bin(fs, Some(orig), bin_entry.as_ref())?;
            }

            match conflict_rec.kind {
                EntryKind::File => {
                    fs.rename_live_to_live(&clean_conflict, &orig_path)?;
                    let _ = fs.set_live_metadata(&orig_path, conflict_rec.mtime, conflict_rec.mode);
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

            let promoted_stat = fs.stat_live(&orig_path)?;
            let disk_mtime = promoted_stat
                .as_ref()
                .map_or(conflict_rec.mtime, |m| m.mtime);

            let mut orig_version = orig_rec
                .as_ref()
                .map_or_else(VersionVector::new, |r| r.version.clone());
            orig_version.merge_in_place(&conflict_rec.version);
            let c1 = meta.next_clock();
            orig_version.bump_above(meta.local_member.clone(), c1);
            let s1 = meta.next_seq();

            let mut conflict_version = conflict_rec.version.clone();
            let c2 = meta.next_clock();
            conflict_version.bump_above(meta.local_member.clone(), c2);
            let s2 = meta.next_seq();
            let mut deps = BTreeMap::new();
            deps.insert(meta.local_member.clone(), s2);

            let mut matrix = store.load_causal_matrix()?;
            matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);

            let promoted = IndexRecord {
                path: orig_path,
                kind: conflict_rec.kind,
                size: conflict_rec.size,
                mtime: conflict_rec.mtime,
                disk_mtime: Some(disk_mtime),
                disk_change_id: promoted_stat.and_then(|m| m.change_id),
                mode: conflict_rec.mode,
                hash: conflict_rec.hash,
                link_target: conflict_rec.link_target,
                deleted: false,
                version: orig_version,
                modified_by: meta.local_member.clone(),
                seq: s1,
                tombstone_deps: BTreeMap::new(),
                conflict_of: None,
            };
            let deleted_conflict = IndexRecord {
                path: clean_conflict,
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
                seq: s2,
                tombstone_deps: deps,
                conflict_of: None,
            };

            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
                upsert_records: vec![deleted_conflict, promoted],
                upsert_bin: bin_entry.into_iter().collect(),
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(())
        }
        ConflictChoice::KeepBoth => {
            let mut meta = load_meta_required(store)?;
            let mut updated = conflict_rec;
            let clock = meta.next_clock();
            updated.version.bump_above(meta.local_member.clone(), clock);
            updated.seq = meta.next_seq();
            updated.conflict_of = None;
            let mut matrix = store.load_causal_matrix()?;
            matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);

            let batch = StoreBatch {
                folder_meta: Some(meta),
                causal_matrix: Some(matrix),
                upsert_records: vec![updated],
                ..StoreBatch::default()
            };
            store.apply_batch(batch)?;
            Ok(())
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

/// Returns `true` when fetching a file from a peer failed because the peer's copy changed or
/// vanished between the peer's index snapshot and our transfer.
///
/// Never matches simulated crashes or permission denials.
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

fn fetch_and_verify_to_tmp<F: SyncFs>(
    fs: &F,
    peer: &dyn Peer,
    remote_path: &str,
    expected_size: u64,
    expected_hash: Hash,
) -> Result<String> {
    let tmp_name = next_tmp_name("fetch");
    let mut reader = peer.fetch_file(remote_path, expected_hash)?;
    let writer = fs.create_tmp(&tmp_name)?;
    let mut hashing = HashingWriter::new(writer);

    let copy_res = io::copy(&mut reader, &mut hashing);
    if let Err(err) = copy_res {
        drop(hashing);
        let _ = fs.remove_tmp(&tmp_name);
        return Err(SyncError::from_io(Some(remote_path.to_string()), err));
    }

    let (inner, actual_hash, bytes_written) = hashing.finish();
    if let Err(err) = inner.finish_sync() {
        let _ = fs.remove_tmp(&tmp_name);
        return Err(err);
    }

    if bytes_written != expected_size {
        let _ = fs.remove_tmp(&tmp_name);
        return Err(SyncError::SizeMismatch {
            path: remote_path.to_string(),
            expected: expected_size,
            actual: bytes_written,
        });
    }

    if actual_hash != expected_hash {
        let _ = fs.remove_tmp(&tmp_name);
        return Err(SyncError::HashMismatch {
            path: remote_path.to_string(),
            expected: expected_hash,
            actual: actual_hash,
        });
    }

    Ok(tmp_name)
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
        SingleRescanOutcome::Unstable => Ok(ApplyOutcome::DeferredUnstable),
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
    let nonce = OP_NONCE.fetch_add(1, Ordering::Relaxed);
    let short = rec
        .hash
        .map(|h| h.to_hex()[..8].to_string())
        .unwrap_or_else(|| "link".to_string());
    let id = format!("{}-{nonce}-{short}", now.secs);
    let storage_name = if rec.kind == EntryKind::File {
        format!("{id}.data")
    } else {
        String::new()
    };
    Some(BinEntry {
        id,
        original_path: rec.path.clone(),
        kind: rec.kind,
        size: rec.size,
        original_mtime: rec.mtime,
        mode: rec.mode,
        hash: rec.hash,
        link_target: rec.link_target.clone(),
        deleted_at: now,
        reason,
        by_member: by_member.to_string(),
        storage_name,
    })
}

fn move_existing_to_bin<F: SyncFs>(
    fs: &F,
    existing: Option<&IndexRecord>,
    bin_entry: Option<&BinEntry>,
) -> Result<()> {
    let Some(rec) = existing else {
        return Ok(());
    };
    if rec.deleted {
        return Ok(());
    }
    match rec.kind {
        EntryKind::File => {
            if let Some(entry) = bin_entry {
                let meta_bytes =
                    serde_json::to_vec_pretty(entry).map_err(|err| SyncError::Storage {
                        context: "serialize bin metadata".to_string(),
                        detail: err.to_string(),
                    })?;
                fs.write_bin_meta(&bin_meta_name(&entry.id), &meta_bytes)?;
                fs.move_live_to_bin(&rec.path, &entry.storage_name)?;
            }
        }
        EntryKind::Symlink => {
            if let Some(entry) = bin_entry {
                let meta_bytes =
                    serde_json::to_vec_pretty(entry).map_err(|err| SyncError::Storage {
                        context: "serialize bin metadata".to_string(),
                        detail: err.to_string(),
                    })?;
                fs.write_bin_meta(&bin_meta_name(&entry.id), &meta_bytes)?;
            }
            fs.remove_symlink_live(&rec.path)?;
        }
        EntryKind::Dir => match fs.remove_empty_dir_live(&rec.path) {
            Ok(()) => {}
            Err(SyncError::Fs {
                kind: FsErrorKind::NotFound,
                ..
            }) => {}
            Err(err) => return Err(err),
        },
    }
    Ok(())
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
    bin_entry: Option<BinEntry>,
    clear_held_back_paths: Vec<String>,
) -> Result<()> {
    let mut meta = load_meta_required(store)?;
    let mut matrix = store.load_causal_matrix()?;
    let mut batch = StoreBatch::default();

    for mut rec in records {
        rec.seq = meta.next_seq();
        if rec.deleted && rec.tombstone_deps.is_empty() {
            rec.tombstone_deps
                .insert(meta.local_member.clone(), rec.seq);
        }
        batch.upsert_records.push(rec);
    }

    matrix.record_seen(&meta.local_member, &meta.local_member, meta.local_seq);
    batch.folder_meta = Some(meta);
    batch.causal_matrix = Some(matrix);
    if let Some(entry) = bin_entry {
        batch.upsert_bin.push(entry);
    }
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
        // Non-fatal metadata errors (for example read-only mtime on exotic filesystems) should not
        // abort an already-placed file; subsequent scans record the actual disk mtime.
        Err(_) => Ok(()),
    }
}

fn next_tmp_name(prefix: &str) -> String {
    let nonce = OP_NONCE.fetch_add(1, Ordering::Relaxed);
    format!(".legion-sync-{prefix}-{nonce}.tmp")
}

fn bin_meta_name(id: &str) -> String {
    format!("{id}.json")
}
