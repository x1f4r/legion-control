//! Pure pull-only reconciliation logic and untrusted-peer record validation.
//!
//! Given a local index lookup, folder metadata, and a remote peer's [`IndexRecord`],
//! [`reconcile_record`] validates every untrusted field and produces a deterministic
//! [`ReconcileDecision`] without performing any disk or network I/O.

use std::cmp::Ordering;

use legion_xfer::{EntryKind, Hash};

use crate::model::{
    BinReason, FolderMeta, IndexRecord, MAX_ANCESTRY_HASHES, MAX_LINK_TARGET_BYTES,
    MAX_VERSION_COUNTER, MAX_VV_INCARNATIONS_PER_MEMBER, MemberId, ProblemKind, SyncProblem,
    validate_member_id,
};
use crate::path::{
    IgnoreMatcher, ancestors, check_fs_compatibility, check_symlink_target_contained,
    conflict_copy_path, fold_case, fs_equivalence_key, parse_conflict_copy_path,
    validate_sync_path,
};
use crate::traits::FsCapabilities;
use crate::version::{VersionOrder, VersionVector};

/// Read-only view of the local index needed to make a reconciliation decision.
pub trait LocalIndexView {
    /// Looks up the local record for an exact relative path.
    fn get_record(&self, path: &str) -> Option<IndexRecord>;

    /// Returns `true` if any live record in the local index is a strict descendant of `dir_path`.
    fn has_live_descendant(&self, dir_path: &str) -> bool;

    /// Returns all live exact paths in the local index matching `folded_path`.
    fn live_paths_for_folded(&self, folded_path: &str) -> Vec<String>;

    /// Returns historically deleted content hashes recorded for `path`.
    fn get_deleted_history(&self, path: &str) -> Vec<Hash>;

    /// Returns the merged deletion version vector stored alongside `deleted_history` for `path`,
    /// if any.
    fn get_deleted_history_version(&self, path: &str) -> Option<VersionVector> {
        let _ = path;
        None
    }
}

/// Action decided by [`reconcile_record`] for one incoming peer record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// Incoming record violated safety, size, counter, or structural invariants and must be
    /// refused (not acknowledged) and reported durably.
    RejectInvalid {
        /// Raw path sent by the peer (truncated if huge).
        path: String,
        /// Explanation of why the record was refused.
        reason: String,
    },
    /// Incoming record is structurally valid but mentions a syntactically valid member name that
    /// this machine has not learned about yet ("not yet", never "invalid"). Stored durably as
    /// unapplied and retried once membership catches up; never acknowledged until applied.
    NotYetKnownMember {
        /// Validated relative path of the record.
        path: String,
        /// Unknown member name referenced by the record.
        member: MemberId,
        /// Human-readable explanation.
        reason: String,
    },
    /// Path is excluded by fixed or per-folder ignore rules.
    Ignore,
    /// Local index is already equal or strictly newer; nothing to change.
    Keep,
    /// Local state wins over a stale incoming join/re-add record; bump the local clock above
    /// `record.version` so the peer pulls the current live version or tombstone and moves its
    /// stale file to its local bin.
    KeepAndSupersede {
        /// Record to commit after bumping the local clock and assigning a fresh `seq`.
        record: IndexRecord,
    },
    /// Index metadata should be updated without transferring or replacing file bytes (quiet merge
    /// of identical content, absorbing a remote deletion for an absent path, or keeping a local
    /// live file in a delete-vs-change race with merged version vector).
    MetadataOnly {
        /// Updated record ready to commit (caller assigns a fresh local `seq`).
        record: IndexRecord,
        /// Whether file mode/mtime on disk should be updated if live.
        update_disk_metadata: bool,
    },
    /// Remote live entry wins and should be fetched/created at `record.path`.
    Fetch {
        /// Target live record to place and commit (caller assigns a fresh local `seq`).
        record: IndexRecord,
        /// Bin reason for any replaced local file (`ReplacedByPeer` or `StaleOnJoin`).
        bin_reason: BinReason,
    },
    /// Remote deletion wins and the local live entry should be moved to `.legion-sync/bin`.
    DeleteToBin {
        /// Target tombstone record to commit (caller assigns a fresh local `seq`).
        tombstone: IndexRecord,
        /// Bin reason for the deleted local file (`DeletedByPeer` or `StaleOnJoin`).
        bin_reason: BinReason,
    },
    /// Remote attempted to delete a directory that still has live local descendants; keep the
    /// directory alive with a strictly dominating version vector.
    ReviveDir {
        /// Directory path to keep alive.
        path: String,
        /// Merged version vector that the caller must bump with a fresh local clock tick.
        base_version: VersionVector,
    },
    /// Concurrent edit with different content where the local entry loses and moves to
    /// `loser_record.path`, while the remote winner takes `winner_record.path`.
    ConflictLocalLoses {
        /// Remote winning entry taking the original path.
        winner_record: IndexRecord,
        /// Local losing entry moving to the conflict copy path (assigned a fresh independent
        /// version vector when applied).
        loser_record: IndexRecord,
    },
    /// Concurrent edit with different content where the local entry keeps `winner_record.path`
    /// and the remote losing entry is written to `loser_record.path`.
    ConflictRemoteLoses {
        /// Local winning entry updated with the merged version vector.
        winner_record: IndexRecord,
        /// Remote losing entry placed at the conflict copy path (assigned a fresh independent
        /// version vector when applied).
        loser_record: IndexRecord,
        /// `true` when the winner requires a fresh local clock bump on `winner_record.version`
        /// (for example a live directory with children or a live conflict copy refusing replacement).
        bump_winner_clock: bool,
    },
    /// Entry cannot be represented on the local filesystem (case clash, Windows naming rules,
    /// over-long path, or symlink on Windows); hold back and report as a problem.
    HoldBack {
        /// Human-readable problem explaining why the record is held back.
        problem: SyncProblem,
    },
}

/// Detailed result of validating an incoming peer [`IndexRecord`].
enum RecordValidationError {
    /// Structurally invalid or unauthorized record (forged counters, invalid syntax, etc.).
    Invalid(String),
    /// Syntactically valid record referencing a member this machine does not know yet.
    UnknownMember {
        path: String,
        member: MemberId,
        reason: String,
    },
}

fn validate_incoming_record_detailed(
    remote: &IndexRecord,
    meta: &FolderMeta,
) -> std::result::Result<String, RecordValidationError> {
    let clean_path = validate_sync_path(&remote.path)
        .map_err(|err| RecordValidationError::Invalid(err.to_string()))?;

    if validate_member_id(&remote.modified_by).is_err() {
        return Err(RecordValidationError::Invalid(format!(
            "record modifier {:?} is not a valid member identifier",
            remote.modified_by
        )));
    }

    if remote.mtime.nanos >= 1_000_000_000 {
        return Err(RecordValidationError::Invalid(format!(
            "record has invalid nanosecond timestamp {}",
            remote.mtime.nanos
        )));
    }

    if remote.recent_hashes.len() > MAX_ANCESTRY_HASHES {
        return Err(RecordValidationError::Invalid(format!(
            "record ancestry list has {} entries, exceeding limit of {MAX_ANCESTRY_HASHES}",
            remote.recent_hashes.len()
        )));
    }

    let own_key = meta.version_key();
    let mut prev_member: Option<&str> = None;
    let mut member_inc_count: usize = 0;
    let mut first_unknown_member: Option<(MemberId, String)> = None;
    for (key, counter) in remote.version.iter() {
        if key.incarnation == 0 {
            return Err(RecordValidationError::Invalid(format!(
                "version vector entry for {:?} has zero incarnation",
                key.member
            )));
        }
        if validate_member_id(&key.member).is_err() {
            return Err(RecordValidationError::Invalid(format!(
                "version vector references invalid member identifier {:?}",
                key.member
            )));
        }
        if counter > MAX_VERSION_COUNTER {
            return Err(RecordValidationError::Invalid(format!(
                "version vector counter {counter} for {:?} exceeds maximum allowed value ({MAX_VERSION_COUNTER})",
                key.member
            )));
        }
        if *key == own_key && counter > meta.local_clock {
            return Err(RecordValidationError::Invalid(format!(
                "peer raised this machine's own version counter to {counter}, above locally issued {}",
                meta.local_clock
            )));
        }
        if prev_member == Some(key.member.as_str()) {
            member_inc_count += 1;
        } else {
            prev_member = Some(key.member.as_str());
            member_inc_count = 1;
        }
        if member_inc_count > MAX_VV_INCARNATIONS_PER_MEMBER {
            return Err(RecordValidationError::Invalid(format!(
                "version vector has more than {MAX_VV_INCARNATIONS_PER_MEMBER} incarnations for member {:?}; keep that machine's index instead of rebuilding it, and once this limit is reached the folder has to be set up afresh from one healthy member",
                key.member
            )));
        }
        if !meta.is_known_member(&key.member) && first_unknown_member.is_none() {
            first_unknown_member = Some((
                key.member.clone(),
                format!(
                    "version history mentions member {:?} which this machine does not know yet",
                    key.member
                ),
            ));
        }
    }

    if remote.deleted {
        if remote.size != 0 || remote.hash.is_some() || remote.link_target.is_some() {
            return Err(RecordValidationError::Invalid(
                "deletion tombstone must have zero size and no file hash or symlink target"
                    .to_string(),
            ));
        }
    } else {
        match remote.kind {
            EntryKind::File => {
                if remote.hash.is_none() {
                    return Err(RecordValidationError::Invalid(
                        "live file record is missing its BLAKE3 hash".to_string(),
                    ));
                }
                if remote.link_target.is_some() {
                    return Err(RecordValidationError::Invalid(
                        "live file record must not carry a symlink target".to_string(),
                    ));
                }
            }
            EntryKind::Dir => {
                if remote.size != 0 || remote.hash.is_some() || remote.link_target.is_some() {
                    return Err(RecordValidationError::Invalid(
                        "directory record must have zero size and no file hash or symlink target"
                            .to_string(),
                    ));
                }
            }
            EntryKind::Symlink => {
                if remote.size != 0 || remote.hash.is_some() {
                    return Err(RecordValidationError::Invalid(
                        "symlink record must have zero size and no file hash".to_string(),
                    ));
                }
                let Some(ref target) = remote.link_target else {
                    return Err(RecordValidationError::Invalid(
                        "live symlink record is missing link_target".to_string(),
                    ));
                };
                if target.is_empty() || target.len() > MAX_LINK_TARGET_BYTES {
                    return Err(RecordValidationError::Invalid(format!(
                        "symlink target length {} is out of bounds (1..={MAX_LINK_TARGET_BYTES})",
                        target.len()
                    )));
                }
                if target.as_bytes().contains(&0) {
                    return Err(RecordValidationError::Invalid(
                        "symlink target contains a NUL byte".to_string(),
                    ));
                }
            }
        }
    }

    if !meta.is_known_member(&remote.modified_by) {
        return Err(RecordValidationError::UnknownMember {
            path: clean_path,
            member: remote.modified_by.clone(),
            reason: format!(
                "record was last changed by member {:?} which this machine does not know yet",
                remote.modified_by
            ),
        });
    }
    if let Some((member, reason)) = first_unknown_member {
        return Err(RecordValidationError::UnknownMember {
            path: clean_path,
            member,
            reason,
        });
    }

    Ok(clean_path)
}

/// Validates every untrusted field of an incoming peer [`IndexRecord`] against `meta`.
///
/// Returns the canonical validated relative path on success, or a plain-language rejection reason.
pub fn validate_incoming_record(
    remote: &IndexRecord,
    meta: &FolderMeta,
) -> std::result::Result<String, String> {
    match validate_incoming_record_detailed(remote, meta) {
        Ok(p) => Ok(p),
        Err(RecordValidationError::Invalid(reason))
        | Err(RecordValidationError::UnknownMember { reason, .. }) => Err(reason),
    }
}

/// Computes the 6-lowercase-hex-character short content hash tag for `rec`.
pub fn record_short_hash6(rec: &IndexRecord) -> String {
    if let Some(hash) = rec.hash {
        let hex = hash.to_hex();
        return hex[..6.min(hex.len())].to_string();
    }
    if let Some(ref target) = rec.link_target {
        let hex = legion_xfer::hash::hash_bytes(target.as_bytes()).to_hex();
        return hex[..6.min(hex.len())].to_string();
    }
    let hex = legion_xfer::hash::hash_bytes(b"dir").to_hex();
    hex[..6.min(hex.len())].to_string()
}

/// Derives `conflict_of` locally from the record's filename, content hash, and this folder's known
/// member list. Never trusts any peer-asserted `conflict_of` value.
pub fn derive_conflict_of(rec: &IndexRecord, meta: &FolderMeta) -> Option<String> {
    if rec.deleted || rec.conflict_dismissed || rec.kind == EntryKind::Dir {
        return None;
    }
    let parsed = parse_conflict_copy_path(&rec.path)?;
    if !meta.is_known_member(&parsed.member) {
        return None;
    }
    if record_short_hash6(rec) != parsed.hash6 {
        return None;
    }
    Some(parsed.original_path)
}

/// Deterministic total ordering between two live [`IndexRecord`] values used to decide which
/// version keeps the original filename in a conflict (and which metadata wins a quiet merge).
///
/// Newer modification time wins; ties are broken by member name (alphabetically first member
/// wins, so `"alpha"` beats `"beta"`), then by entry kind, hash, and symlink target so the
/// comparison is strictly symmetric and associative across every member regardless of sync order.
pub fn compare_live_winner(a: &IndexRecord, b: &IndexRecord) -> Ordering {
    a.mtime
        .cmp(&b.mtime)
        .then_with(|| b.modified_by.cmp(&a.modified_by))
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| a.hash.cmp(&b.hash))
        .then_with(|| a.link_target.cmp(&b.link_target))
}

/// Decides what the local member should do with an incoming `remote` record from `peer_member`.
pub fn reconcile_record(
    local_view: &dyn LocalIndexView,
    remote: &IndexRecord,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
    ignore: &IgnoreMatcher,
) -> ReconcileDecision {
    let clean_path = match validate_incoming_record_detailed(remote, meta) {
        Ok(p) => p,
        Err(RecordValidationError::Invalid(reason)) => {
            return ReconcileDecision::RejectInvalid {
                path: remote.path.chars().take(128).collect(),
                reason,
            };
        }
        Err(RecordValidationError::UnknownMember {
            path,
            member,
            reason,
        }) => {
            return ReconcileDecision::NotYetKnownMember {
                path,
                member,
                reason,
            };
        }
    };

    if ignore.is_path_or_ancestor_ignored(&clean_path, remote.kind == EntryKind::Dir) {
        return ReconcileDecision::Ignore;
    }

    let peer_is_rejoining = meta.rejoining_peers.contains(peer_member);
    let local_opt = local_view.get_record(&clean_path);
    match local_opt {
        None => reconcile_without_local(
            local_view,
            remote,
            &clean_path,
            peer_member,
            peer_is_rejoining,
            meta,
            caps,
        ),
        Some(local) => reconcile_with_local(
            local_view,
            &local,
            remote,
            &clean_path,
            peer_member,
            peer_is_rejoining,
            meta,
            caps,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_without_local(
    local_view: &dyn LocalIndexView,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    peer_is_rejoining: bool,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if remote.deleted {
        let mut tombstone = sanitize_remote(remote, clean_path, meta);
        tombstone.from_join = false;
        tombstone.conflict_of = None;
        return ReconcileDecision::MetadataOnly {
            record: tombstone,
            update_disk_metadata: false,
        };
    }

    // Content-based deletion dominance (after tombstone GC): if a peer presents a live file whose
    // exact content hash was previously deleted at this path and the peer's version does NOT
    // strictly dominate the recorded deletion version (i.e. it is NOT an intentional restore or
    // re-creation made after seeing the deletion), do NOT resurrect the deleted file — regardless
    // of whether `from_join` or `rejoining_peers` is set (covering two joiners or two restored
    // members meeting before a member that saw the deletion).
    let _ = peer_is_rejoining;
    if let Some(remote_hash) = remote.hash {
        let deleted_hist = local_view.get_deleted_history(clean_path);
        if deleted_hist.contains(&remote_hash) {
            let del_vv_opt = local_view.get_deleted_history_version(clean_path);
            let del_cmp = del_vv_opt
                .as_ref()
                .map(|del_vv| del_vv.compare(&remote.version));
            let remote_dominates_deletion = del_cmp == Some(VersionOrder::Older);
            let deletion_dominates_remote =
                matches!(del_cmp, Some(VersionOrder::Newer | VersionOrder::Equal));
            let is_join_or_stale_backup =
                remote.from_join || peer_is_rejoining || deletion_dominates_remote;
            if is_join_or_stale_backup && !remote_dominates_deletion {
                let mut recent_hashes = Vec::with_capacity(MAX_ANCESTRY_HASHES);
                recent_hashes.push(remote_hash);
                for h in deleted_hist {
                    if recent_hashes.len() >= MAX_ANCESTRY_HASHES {
                        break;
                    }
                    if !recent_hashes.contains(&h) {
                        recent_hashes.push(h);
                    }
                }
                let version = match del_vv_opt {
                    Some(del_vv) => del_vv.merge(&remote.version),
                    None => remote.version.clone(),
                };
                let tombstone = IndexRecord {
                    path: clean_path.to_string(),
                    kind: remote.kind,
                    size: 0,
                    mtime: remote.mtime,
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
                return ReconcileDecision::KeepAndSupersede { record: tombstone };
            }
        }
    }

    if let Some(problem) = check_local_representable(
        local_view,
        clean_path,
        remote.kind,
        remote.link_target.as_deref(),
        peer_member,
        caps,
        None,
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut target = sanitize_remote(remote, clean_path, meta);
    target.from_join = remote.from_join;
    ReconcileDecision::Fetch {
        record: target,
        bin_reason: BinReason::ReplacedByPeer,
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_with_local(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    peer_is_rejoining: bool,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    match local.version.compare(&remote.version) {
        VersionOrder::Newer => ReconcileDecision::Keep,
        VersionOrder::Equal => {
            if local.deleted && remote.deleted {
                return ReconcileDecision::Keep;
            }
            if local.content_equal(remote) {
                return ReconcileDecision::Keep;
            }
            // Defensive fallback: identical versions with divergent content should never occur,
            // but treating them as concurrent guarantees neither version is ever lost.
            reconcile_concurrent(
                local_view,
                local,
                remote,
                clean_path,
                peer_member,
                peer_is_rejoining,
                meta,
                caps,
            )
        }
        VersionOrder::Older => reconcile_remote_newer(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
        ),
        VersionOrder::Concurrent => reconcile_concurrent(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            peer_is_rejoining,
            meta,
            caps,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_remote_newer(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if remote.deleted {
        if local.deleted {
            let mut tombstone = sanitize_remote(remote, clean_path, meta);
            tombstone.merge_ancestry_from(local);
            tombstone.from_join = false;
            tombstone.conflict_of = None;
            return ReconcileDecision::MetadataOnly {
                record: tombstone,
                update_disk_metadata: false,
            };
        }

        // Cannot delete a directory that still holds live descendants locally; keep the directory
        // alive with a bumped version so the tree remains valid everywhere.
        if local.kind == EntryKind::Dir && local_view.has_live_descendant(clean_path) {
            return ReconcileDecision::ReviveDir {
                path: clean_path.to_string(),
                base_version: local.version.merge(&remote.version),
            };
        }

        let mut tombstone = sanitize_remote(remote, clean_path, meta);
        if let Some(lh) = local.hash {
            tombstone.push_ancestry_hash(lh);
        }
        tombstone.merge_ancestry_from(local);
        tombstone.from_join = false;
        tombstone.conflict_of = None;
        let bin_reason = if local.from_join {
            BinReason::StaleOnJoin
        } else {
            BinReason::DeletedByPeer
        };
        return ReconcileDecision::DeleteToBin {
            tombstone,
            bin_reason,
        };
    }

    // Remote is live and strictly newer.
    if !local.deleted && local.content_equal(remote) {
        let mut updated = local.clone();
        updated.version = remote.version.clone();
        updated.mtime = remote.mtime;
        updated.modified_by = remote.modified_by.clone();
        if remote.mode.is_some() {
            updated.mode = remote.mode;
        }
        updated.merge_ancestry_from(remote);
        updated.from_join = local.from_join && remote.from_join;
        updated.conflict_of = derive_conflict_of(&updated, meta);
        let update_disk_metadata = local.mtime != remote.mtime || local.mode != updated.mode;
        return ReconcileDecision::MetadataOnly {
            record: updated,
            update_disk_metadata,
        };
    }

    // Decision A: sync NEVER replaces a live conflict copy with different content. Keep the live
    // conflict copy untouched and place the incoming different version at its own conflict copy
    // path so nothing flagged as one of two versions is ever overwritten without a person acting.
    if !local.deleted && local.conflict_of.is_some() && !local.conflict_dismissed {
        return build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
            true,
        );
    }

    // Sequential directory -> file/symlink replacement when live local descendants still exist
    // under the directory: keep the directory at `clean_path` and preserve the incoming file as a
    // conflict copy so neither the file nor the directory's children are lost.
    if !local.deleted
        && local.kind == EntryKind::Dir
        && remote.kind != EntryKind::Dir
        && local_view.has_live_descendant(clean_path)
    {
        return build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
            true,
        );
    }

    if let Some(problem) = check_local_representable(
        local_view,
        clean_path,
        remote.kind,
        remote.link_target.as_deref(),
        peer_member,
        caps,
        if local.deleted {
            None
        } else {
            Some(clean_path)
        },
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut target = sanitize_remote(remote, clean_path, meta);
    if !local.deleted
        && let Some(lh) = local.hash
    {
        target.push_ancestry_hash(lh);
    }
    target.merge_ancestry_from(local);
    target.from_join = remote.from_join;
    let bin_reason = if local.from_join {
        BinReason::StaleOnJoin
    } else {
        BinReason::ReplacedByPeer
    };
    ReconcileDecision::Fetch {
        record: target,
        bin_reason,
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_concurrent(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    peer_is_rejoining: bool,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    let merged_version = local.version.merge(&remote.version);

    // Concurrent deletions merge quietly.
    if local.deleted && remote.deleted {
        let mut merged = if compare_live_winner(local, remote) != Ordering::Less {
            local.clone()
        } else {
            sanitize_remote(remote, clean_path, meta)
        };
        merged.path = clean_path.to_string();
        merged.deleted = true;
        merged.version = merged_version;
        merged.merge_ancestry_from(local);
        merged.merge_ancestry_from(remote);
        merged.from_join = false;
        merged.conflict_of = None;
        return ReconcileDecision::MetadataOnly {
            record: merged,
            update_disk_metadata: false,
        };
    }

    // Rule D (deletion dominance for joining/reinstalling/restored copies): when the live side is
    // a join-origin copy (including two joiners that met each other and preserved `from_join =
    // true`) and the concurrent tombstone's ancestry or deleted history records that exact content
    // hash, the deletion wins and moves the stale copy safely to the bin. Genuine post-join
    // concurrent edits or intentional re-creations (`from_join = false`) still win delete-vs-live.
    if local.from_join
        && !local.deleted
        && remote.deleted
        && let Some(local_hash) = local.hash
        && (remote.recent_hashes.contains(&local_hash)
            || local_view
                .get_deleted_history(clean_path)
                .contains(&local_hash))
    {
        let mut tombstone = sanitize_remote(remote, clean_path, meta);
        tombstone.version = merged_version;
        tombstone.push_ancestry_hash(local_hash);
        tombstone.from_join = false;
        tombstone.conflict_of = None;
        return ReconcileDecision::DeleteToBin {
            tombstone,
            bin_reason: BinReason::StaleOnJoin,
        };
    }

    if (remote.from_join || peer_is_rejoining)
        && local.deleted
        && !remote.deleted
        && let Some(remote_hash) = remote.hash
    {
        let in_history = local.recent_hashes.contains(&remote_hash)
            || local_view
                .get_deleted_history(clean_path)
                .contains(&remote_hash);
        if in_history {
            let mut tombstone = local.clone();
            tombstone.version = merged_version;
            tombstone.push_ancestry_hash(remote_hash);
            tombstone.from_join = false;
            tombstone.conflict_of = None;
            return ReconcileDecision::KeepAndSupersede { record: tombstone };
        }
    }

    // Rule D (live vs live when local side is joining/reinstalling/rolled-back): if our
    // pre-existing local file's hash is already in the peer's live ancestry, accept the newer
    // live version and move our stale copy to the bin without fabricating a conflict copy.
    if local.from_join
        && !local.deleted
        && !remote.deleted
        && !local.content_equal(remote)
        && let Some(local_hash) = local.hash
        && remote.recent_hashes.contains(&local_hash)
    {
        if let Some(problem) = check_local_representable(
            local_view,
            clean_path,
            remote.kind,
            remote.link_target.as_deref(),
            peer_member,
            caps,
            Some(clean_path),
        ) {
            return ReconcileDecision::HoldBack { problem };
        }
        let mut target = sanitize_remote(remote, clean_path, meta);
        target.version = merged_version;
        target.push_ancestry_hash(local_hash);
        target.from_join = false;
        return ReconcileDecision::Fetch {
            record: target,
            bin_reason: BinReason::StaleOnJoin,
        };
    }

    // Rule D (live vs live when remote side is joining/re-added/rolled-back): if the remote
    // pre-existing file's hash is in our live ancestry, keep our live file and supersede remote's
    // version so the peer moves its stale copy to its own local bin.
    if (remote.from_join || peer_is_rejoining)
        && !local.deleted
        && !remote.deleted
        && !local.content_equal(remote)
        && let Some(remote_hash) = remote.hash
        && local.recent_hashes.contains(&remote_hash)
    {
        let mut kept = local.clone();
        kept.version = merged_version;
        kept.push_ancestry_hash(remote_hash);
        kept.from_join = false;
        return ReconcileDecision::KeepAndSupersede { record: kept };
    }

    // Delete vs change (for genuine concurrent changes or unknown content): the change always wins.
    if !local.deleted && remote.deleted {
        let mut kept = local.clone();
        kept.version = merged_version;
        kept.merge_ancestry_from(remote);
        kept.from_join = false;
        return ReconcileDecision::MetadataOnly {
            record: kept,
            update_disk_metadata: false,
        };
    }
    if local.deleted && !remote.deleted {
        if let Some(problem) = check_local_representable(
            local_view,
            clean_path,
            remote.kind,
            remote.link_target.as_deref(),
            peer_member,
            caps,
            None,
        ) {
            return ReconcileDecision::HoldBack { problem };
        }
        let mut revived = sanitize_remote(remote, clean_path, meta);
        revived.version = merged_version;
        revived.merge_ancestry_from(local);
        revived.from_join = false;
        return ReconcileDecision::Fetch {
            record: revived,
            bin_reason: BinReason::ReplacedByPeer,
        };
    }

    // Both live and concurrent with identical content -> merge quietly.
    //
    // Join provenance rules:
    // - If one side is `from_join = true` and the other is an authored `from_join = false` record
    //   of identical content, adopt the authored version cleanly WITHOUT polluting it with the
    //   join-scan incarnation key (so stale backup versions stay strictly older than later
    //   deletions and unchanged files across rebuilds accumulate zero extra incarnation keys).
    // - If BOTH sides are `from_join = true` (two joiners or two restored members carrying the
    //   same old copy meeting before the deleter), preserve `from_join = true` on the merged
    //   record so the deletion still dominates when they later meet the rest of the folder.
    if local.content_equal(remote) {
        if !local.from_join && remote.from_join {
            return ReconcileDecision::Keep;
        }
        if local.from_join && !remote.from_join {
            let mut adopted = sanitize_remote(remote, clean_path, meta);
            adopted.version = remote.version.clone();
            adopted.disk_mtime = local.disk_mtime;
            adopted.disk_change_id = local.disk_change_id;
            adopted.from_join = false;
            adopted.conflict_of = derive_conflict_of(&adopted, meta);
            let update_disk_metadata = local.mtime != adopted.mtime || local.mode != adopted.mode;
            return ReconcileDecision::MetadataOnly {
                record: adopted,
                update_disk_metadata,
            };
        }
        let winner = if compare_live_winner(local, remote) != Ordering::Less {
            local
        } else {
            remote
        };
        let mut merged = local.clone();
        merged.version = merged_version;
        merged.mtime = winner.mtime;
        merged.modified_by = winner.modified_by.clone();
        if winner.mode.is_some() {
            merged.mode = winner.mode;
        }
        merged.merge_ancestry_from(remote);
        merged.from_join = local.from_join && remote.from_join;
        merged.conflict_of = derive_conflict_of(&merged, meta);
        let update_disk_metadata = local.mtime != merged.mtime || local.mode != merged.mode;
        return ReconcileDecision::MetadataOnly {
            record: merged,
            update_disk_metadata,
        };
    }

    // Both live and concurrent with different content -> keep both!
    // Directory always keeps the original path when conflicting with a file or symlink so all
    // descendants keep their paths without recursive renames (symmetric across all members).
    if local.kind == EntryKind::Dir && remote.kind != EntryKind::Dir {
        return build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
            false,
        );
    }
    if local.kind != EntryKind::Dir && remote.kind == EntryKind::Dir {
        return build_local_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
        );
    }

    // Never replace a live conflict copy with different content; enforce symmetrically using the
    // canonical conflict-copy name/hash match so every member picks the exact same winner for the
    // merged version vector (preventing equal-VV split-brain divergence).
    let local_matches_conflict_copy = derive_conflict_of(local, meta).is_some();
    let remote_matches_conflict_copy = derive_conflict_of(remote, meta).is_some();
    if local_matches_conflict_copy && !remote_matches_conflict_copy {
        return build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
            false,
        );
    }
    if !local_matches_conflict_copy && remote_matches_conflict_copy {
        return build_local_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
        );
    }

    if compare_live_winner(local, remote) != Ordering::Less {
        build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
            false,
        )
    } else {
        build_local_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
        )
    }
}

fn sanitize_remote(remote: &IndexRecord, clean_path: &str, meta: &FolderMeta) -> IndexRecord {
    let mut rec = remote.for_wire();
    rec.path = clean_path.to_string();
    if let Some(canon) = meta.canonical_member_name(&remote.modified_by) {
        rec.modified_by = canon.to_string();
    }
    rec.conflict_dismissed = false;
    rec.conflict_of = derive_conflict_of(&rec, meta);
    rec
}

#[allow(clippy::too_many_arguments)]
fn build_remote_loses_conflict(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
    needs_winner_bump: bool,
) -> ReconcileDecision {
    let loser_member = meta
        .canonical_member_name(&remote.modified_by)
        .unwrap_or(peer_member);
    let conflict_path = choose_conflict_path(local_view, clean_path, remote, loser_member);
    if let Some(problem) = check_local_representable(
        local_view,
        &conflict_path,
        remote.kind,
        remote.link_target.as_deref(),
        peer_member,
        caps,
        Some(&conflict_path),
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut winner = local.clone();
    winner.version = local.version.merge(&remote.version);
    winner.merge_ancestry_from(remote);
    winner.from_join = false;

    // Decision A: conflict copy is a new independent file; its version is assigned fresh by apply.
    let mut loser = sanitize_remote(remote, &conflict_path, meta);
    loser.version = VersionVector::new();
    loser.recent_hashes.clear();
    loser.from_join = false;
    loser.conflict_dismissed = false;
    loser.conflict_of = derive_conflict_of(&loser, meta).or_else(|| Some(clean_path.to_string()));

    ReconcileDecision::ConflictRemoteLoses {
        winner_record: winner,
        loser_record: loser,
        bump_winner_clock: needs_winner_bump,
    }
}

fn build_local_loses_conflict(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if let Some(problem) = check_local_representable(
        local_view,
        clean_path,
        remote.kind,
        remote.link_target.as_deref(),
        peer_member,
        caps,
        Some(clean_path),
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let loser_member = meta
        .canonical_member_name(&local.modified_by)
        .unwrap_or(meta.local_member.as_str());
    let conflict_path = choose_conflict_path(local_view, clean_path, local, loser_member);
    if let Some(problem) = check_local_representable(
        local_view,
        &conflict_path,
        local.kind,
        local.link_target.as_deref(),
        peer_member,
        caps,
        Some(&conflict_path),
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut winner = sanitize_remote(remote, clean_path, meta);
    winner.version = local.version.merge(&remote.version);
    winner.merge_ancestry_from(local);
    winner.from_join = false;

    // Decision A: conflict copy is a new independent file; its version is assigned fresh by apply.
    let mut loser = local.clone();
    loser.path = conflict_path;
    loser.version = VersionVector::new();
    loser.recent_hashes.clear();
    loser.from_join = false;
    loser.conflict_dismissed = false;
    loser.conflict_of = derive_conflict_of(&loser, meta).or_else(|| Some(clean_path.to_string()));

    ReconcileDecision::ConflictLocalLoses {
        winner_record: winner,
        loser_record: loser,
    }
}

/// Computes the deterministic conflict copy path for `loser` using the canonical folder member
/// name and mandatory 6-hex-character content hash (`name (changed on <member> <date>, <hash6>).ext`),
/// adding a numeric disambiguator only in the rare event of a live hash-prefix collision with
/// different content.
pub fn choose_conflict_path(
    local_view: &dyn LocalIndexView,
    original_path: &str,
    loser: &IndexRecord,
    canonical_member: &str,
) -> String {
    let hash6 = record_short_hash6(loser);
    let primary = conflict_copy_path(original_path, canonical_member, loser.mtime, &hash6, None);
    match local_view.get_record(&primary) {
        None => primary,
        Some(existing) if existing.deleted || existing.content_equal(loser) => primary,
        Some(_) => {
            for n in 2u32..10_000 {
                let tag = n.to_string();
                let alt = conflict_copy_path(
                    original_path,
                    canonical_member,
                    loser.mtime,
                    &hash6,
                    Some(&tag),
                );
                match local_view.get_record(&alt) {
                    None => return alt,
                    Some(e) if e.deleted || e.content_equal(loser) => return alt,
                    Some(_) => continue,
                }
            }
            primary
        }
    }
}

/// Checks platform naming/length/symlink rules (including strict relative in-folder symlink
/// containment) and case-insensitive / Unicode-normalizing collisions against the local index.
#[allow(clippy::too_many_arguments)]
pub fn check_local_representable(
    local_view: &dyn LocalIndexView,
    rel_path: &str,
    kind: EntryKind,
    link_target: Option<&str>,
    peer_member: &str,
    caps: &FsCapabilities,
    replacing_exact: Option<&str>,
) -> Option<SyncProblem> {
    if let Err((prob_kind, sentence)) = check_fs_compatibility(rel_path, kind, caps) {
        return Some(SyncProblem {
            path: rel_path.to_string(),
            kind: prob_kind,
            peer: Some(peer_member.to_string()),
            sentence,
        });
    }

    if kind == EntryKind::Symlink
        && let Err(reason) =
            check_symlink_target_contained(rel_path, link_target.unwrap_or_default())
    {
        return Some(SyncProblem {
            path: rel_path.to_string(),
            kind: ProblemKind::UnsupportedSymlink,
            peer: Some(peer_member.to_string()),
            sentence: format!("'{rel_path}' was held back because {reason}."),
        });
    }

    if caps.case_insensitive || caps.unicode_normalizing {
        let mut to_check = ancestors(rel_path);
        to_check.push(rel_path);
        for candidate in to_check {
            let folded = fold_case(candidate);
            let cand_equiv = fs_equivalence_key(candidate, caps);
            for existing in local_view.live_paths_for_folded(&folded) {
                if existing != candidate
                    && replacing_exact != Some(existing.as_str())
                    && fs_equivalence_key(&existing, caps) == cand_equiv
                {
                    return Some(SyncProblem {
                        path: rel_path.to_string(),
                        kind: ProblemKind::CaseClash,
                        peer: Some(peer_member.to_string()),
                        sentence: format!(
                            "'{rel_path}' was held back because its name clashes with '{existing}' on this file system."
                        ),
                    });
                }
            }
        }
    }

    None
}
