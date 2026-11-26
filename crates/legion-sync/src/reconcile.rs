//! Pure pull-only reconciliation logic and untrusted-peer record validation.
//!
//! Given a local index lookup, folder metadata, and a remote peer's [`IndexRecord`],
//! [`reconcile_record`] validates every untrusted field and produces a deterministic
//! [`ReconcileDecision`] without performing any disk or network I/O.

use std::cmp::Ordering;

use legion_xfer::EntryKind;

use crate::model::{
    BinReason, FolderMeta, IndexRecord, MAX_ANCESTRY_HASHES, MAX_LINK_TARGET_BYTES,
    MAX_VERSION_COUNTER, MAX_VV_INCARNATIONS_PER_MEMBER, MemberId, ProblemKind, RecordProvenance,
    SyncProblem, validate_member_id,
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
        /// `true` when the fetched unversioned file is being promoted to an authored version
        /// dominating a local deletion of different content.
        bump_clock: bool,
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
        /// `true` when `winner_record` must receive a fresh local clock bump (e.g. an unversioned
        /// remote file winning against a versioned local file).
        bump_winner_clock: bool,
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
    match &remote.provenance {
        RecordProvenance::Unversioned => {
            if remote.deleted {
                return Err(RecordValidationError::Invalid(
                    "deletion tombstone cannot be unversioned".to_string(),
                ));
            }
            if remote.base_hash.is_some() {
                return Err(RecordValidationError::Invalid(
                    "unversioned record must not carry a base_hash".to_string(),
                ));
            }
        }
        RecordProvenance::Versioned(vv) => {
            for (key, counter) in vv.iter() {
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

    let local_opt = local_view.get_record(&clean_path);
    match local_opt {
        None => reconcile_without_local(local_view, remote, &clean_path, peer_member, meta, caps),
        Some(local) => reconcile_with_local(
            local_view,
            &local,
            remote,
            &clean_path,
            peer_member,
            meta,
            caps,
        ),
    }
}

fn reconcile_without_local(
    local_view: &dyn LocalIndexView,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if remote.deleted {
        let mut tombstone = sanitize_remote(remote, clean_path, meta);
        tombstone.conflict_of = None;
        return ReconcileDecision::MetadataOnly {
            record: tombstone,
            update_disk_metadata: false,
        };
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

    // Preserves `remote.provenance` verbatim: an unversioned file offered by a joiner spreads to
    // peers with no record for the path, still unversioned.
    let target = sanitize_remote(remote, clean_path, meta);
    ReconcileDecision::Fetch {
        record: target,
        bin_reason: BinReason::ReplacedByPeer,
        bump_clock: false,
    }
}

fn record_matches_unversioned_hash(
    versioned: &IndexRecord,
    unversioned_hash: legion_xfer::Hash,
) -> bool {
    versioned.base_hash == Some(unversioned_hash)
        || versioned.recent_hashes.contains(&unversioned_hash)
}

#[allow(clippy::too_many_arguments)]
fn reconcile_with_local(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    match (&local.provenance, &remote.provenance) {
        // Decision 3: two unversioned files. Equal content -> nothing happens and nobody adds a
        // key; different content -> keep both under the existing conflict rule without adding a
        // key to the winner.
        (RecordProvenance::Unversioned, RecordProvenance::Unversioned) => {
            if local.content_equal(remote) {
                return ReconcileDecision::Keep;
            }
            reconcile_live_conflict(
                local_view,
                local,
                remote,
                clean_path,
                peer_member,
                meta,
                caps,
                RecordProvenance::Unversioned,
                false,
                false,
            )
        }

        // Decision 3: local is unversioned (provenance unknown), remote has versioned history.
        (RecordProvenance::Unversioned, RecordProvenance::Versioned(rv)) => {
            if remote.deleted {
                let matches_deleted = match (local.kind, remote.kind) {
                    (EntryKind::File, EntryKind::File) => local
                        .hash
                        .is_some_and(|lh| record_matches_unversioned_hash(remote, lh)),
                    (EntryKind::Dir, EntryKind::Dir) => true,
                    _ => false,
                };
                if matches_deleted {
                    if local.kind == EntryKind::Dir && local_view.has_live_descendant(clean_path) {
                        return ReconcileDecision::ReviveDir {
                            path: clean_path.to_string(),
                            base_version: rv.clone(),
                        };
                    }
                    let mut tombstone = sanitize_remote(remote, clean_path, meta);
                    if let Some(lh) = local.hash {
                        if tombstone.base_hash.is_none() {
                            tombstone.base_hash = Some(lh);
                        }
                        tombstone.push_ancestry_hash(lh);
                    }
                    tombstone.conflict_of = None;
                    return ReconcileDecision::DeleteToBin {
                        tombstone,
                        bin_reason: BinReason::StaleOnJoin,
                    };
                }
                // Content differs from what was deleted -> stays and becomes an ordinary authored
                // file dominating the deletion.
                let mut authored = local.clone();
                authored.provenance = RecordProvenance::Versioned(rv.clone());
                authored.base_hash = None;
                authored.merge_ancestry_from(remote);
                return ReconcileDecision::KeepAndSupersede { record: authored };
            }

            // Remote is versioned and live.
            if local.content_equal(remote) {
                let mut adopted = sanitize_remote(remote, clean_path, meta);
                adopted.disk_mtime = local.disk_mtime;
                adopted.disk_change_id = local.disk_change_id;
                adopted.conflict_of = derive_conflict_of(&adopted, meta);
                let update_disk_metadata =
                    local.mtime != adopted.mtime || local.mode != adopted.mode;
                return ReconcileDecision::MetadataOnly {
                    record: adopted,
                    update_disk_metadata,
                };
            }

            // Decision 4 / ancestry: if remote was made on top of local's exact content, adopt
            // remote's exact version without a conflict copy and move stale local copy to bin.
            if local.kind == EntryKind::File
                && remote.kind == EntryKind::File
                && let Some(lh) = local.hash
                && record_matches_unversioned_hash(remote, lh)
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
                let target = sanitize_remote(remote, clean_path, meta);
                return ReconcileDecision::Fetch {
                    record: target,
                    bin_reason: BinReason::StaleOnJoin,
                    bump_clock: false,
                };
            }

            // Differing content not made on top of local -> existing join conflict rule.
            // If local wins `clean_path`, bump local clock on top of `rv` so the winner is
            // authored and strictly dominates `remote`; if remote wins, it keeps exact `rv`.
            reconcile_live_conflict(
                local_view,
                local,
                remote,
                clean_path,
                peer_member,
                meta,
                caps,
                RecordProvenance::Versioned(rv.clone()),
                true,
                false,
            )
        }

        // Decision 3: local has versioned history, remote is unversioned.
        (RecordProvenance::Versioned(lv), RecordProvenance::Unversioned) => {
            if local.deleted {
                let matches_deleted = match (local.kind, remote.kind) {
                    (EntryKind::File, EntryKind::File) => remote
                        .hash
                        .is_some_and(|rh| record_matches_unversioned_hash(local, rh)),
                    (EntryKind::Dir, EntryKind::Dir) => true,
                    _ => false,
                };
                if matches_deleted {
                    // Keep our tombstone untouched with zero key growth; when the unversioned peer
                    // pulls from us it will bin its copy and adopt our exact tombstone.
                    return ReconcileDecision::Keep;
                }
                // Remote unversioned file has different content from what we deleted -> survives
                // and becomes an ordinary authored file dominating our deletion.
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
                revived.provenance = RecordProvenance::Versioned(lv.clone());
                revived.base_hash = None;
                revived.merge_ancestry_from(local);
                return ReconcileDecision::Fetch {
                    record: revived,
                    bin_reason: BinReason::ReplacedByPeer,
                    bump_clock: true,
                };
            }

            // Local is versioned and live.
            if local.content_equal(remote) {
                // Zero key growth; when remote pulls from us it adopts our exact version.
                return ReconcileDecision::Keep;
            }

            // Decision 4 / ancestry: if our live version was made on top of remote's content,
            // keep ours untouched; remote will adopt it without a conflict copy on pull.
            if local.kind == EntryKind::File
                && remote.kind == EntryKind::File
                && let Some(rh) = remote.hash
                && record_matches_unversioned_hash(local, rh)
            {
                return ReconcileDecision::Keep;
            }

            // Differing content -> keep both; if remote wins `clean_path`, bump local clock on
            // top of `lv` so we never downgrade a versioned local path back to unversioned.
            reconcile_live_conflict(
                local_view,
                local,
                remote,
                clean_path,
                peer_member,
                meta,
                caps,
                RecordProvenance::Versioned(lv.clone()),
                false,
                true,
            )
        }

        // Both sides carry version vectors -> causal comparison, then Decision 4 before conflicts.
        (RecordProvenance::Versioned(lv), RecordProvenance::Versioned(rv)) => {
            match lv.compare(rv) {
                VersionOrder::Newer => ReconcileDecision::Keep,
                VersionOrder::Equal => {
                    if (local.deleted && remote.deleted) || local.content_equal(remote) {
                        return ReconcileDecision::Keep;
                    }
                    // Defensive fallback: identical versions with divergent content should never
                    // occur, but treating them as concurrent guarantees neither version is lost.
                    reconcile_concurrent_versioned(
                        local_view,
                        local,
                        remote,
                        lv,
                        rv,
                        clean_path,
                        peer_member,
                        meta,
                        caps,
                    )
                }
                VersionOrder::Older => reconcile_remote_newer(
                    local_view,
                    local,
                    remote,
                    lv,
                    rv,
                    clean_path,
                    peer_member,
                    meta,
                    caps,
                ),
                VersionOrder::Concurrent => reconcile_concurrent_versioned(
                    local_view,
                    local,
                    remote,
                    lv,
                    rv,
                    clean_path,
                    peer_member,
                    meta,
                    caps,
                ),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_remote_newer(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    lv: &VersionVector,
    rv: &VersionVector,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if remote.deleted {
        if local.deleted {
            let mut tombstone = sanitize_remote(remote, clean_path, meta);
            tombstone.merge_ancestry_from(local);
            if tombstone.base_hash.is_none() {
                tombstone.base_hash = local.base_hash;
            }
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
                base_version: lv.merge(rv),
            };
        }

        let mut tombstone = sanitize_remote(remote, clean_path, meta);
        if let Some(lh) = local.hash {
            if tombstone.base_hash.is_none() {
                tombstone.base_hash = Some(lh);
            }
            tombstone.push_ancestry_hash(lh);
        }
        tombstone.merge_ancestry_from(local);
        tombstone.conflict_of = None;
        return ReconcileDecision::DeleteToBin {
            tombstone,
            bin_reason: BinReason::DeletedByPeer,
        };
    }

    // Remote is live and strictly newer.
    if !local.deleted && local.content_equal(remote) {
        let mut updated = local.clone();
        updated.provenance = remote.provenance.clone();
        updated.base_hash = remote.base_hash.or(local.base_hash);
        updated.mtime = remote.mtime;
        updated.modified_by = remote.modified_by.clone();
        if remote.mode.is_some() {
            updated.mode = remote.mode;
        }
        updated.merge_ancestry_from(remote);
        updated.conflict_of = derive_conflict_of(&updated, meta);
        let update_disk_metadata = local.mtime != remote.mtime || local.mode != updated.mode;
        return ReconcileDecision::MetadataOnly {
            record: updated,
            update_disk_metadata,
        };
    }

    // Sync NEVER replaces a live conflict copy with different content. Keep the live conflict
    // copy untouched and place the incoming different version at its own conflict copy path.
    if !local.deleted && local.conflict_of.is_some() && !local.conflict_dismissed {
        return build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
            meta,
            caps,
            RecordProvenance::Versioned(lv.merge(rv)),
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
            RecordProvenance::Versioned(lv.merge(rv)),
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
    ReconcileDecision::Fetch {
        record: target,
        bin_reason: BinReason::ReplacedByPeer,
        bump_clock: false,
    }
}

#[allow(clippy::too_many_arguments)]
fn reconcile_concurrent_versioned(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    lv: &VersionVector,
    rv: &VersionVector,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    let merged_version = lv.merge(rv);
    let merged_prov = RecordProvenance::Versioned(merged_version);

    // Concurrent deletions merge quietly.
    if local.deleted && remote.deleted {
        let mut merged = if compare_live_winner(local, remote) != Ordering::Less {
            local.clone()
        } else {
            sanitize_remote(remote, clean_path, meta)
        };
        merged.path = clean_path.to_string();
        merged.deleted = true;
        merged.provenance = merged_prov;
        merged.base_hash = merged.base_hash.or(local.base_hash).or(remote.base_hash);
        merged.merge_ancestry_from(local);
        merged.merge_ancestry_from(remote);
        merged.conflict_of = None;
        return ReconcileDecision::MetadataOnly {
            record: merged,
            update_disk_metadata: false,
        };
    }

    // Decision 4 (BEFORE any conflict rule) — deletion vs live:
    // When two concurrent versioned lineages share no causal history (e.g. a true joiner who
    // deleted or edited before first exchange), a deletion whose `base_hash` equals the other
    // side's live content hash was made on top of that exact content, so the deletion wins, takes
    // over both sides' version keys, and moves the other copy to the bin. When both sides share
    // causal history and diverged (e.g. an intentional re-creation after an earlier deletion), a
    // real live change continues to beat a concurrent deletion.
    let disjoint_histories = !lv.shares_any_key(rv);
    let local_matches_conflict_copy = derive_conflict_of(local, meta).is_some();
    let remote_matches_conflict_copy = derive_conflict_of(remote, meta).is_some();
    if !local.deleted && remote.deleted {
        if disjoint_histories
            && !local_matches_conflict_copy
            && local.kind == EntryKind::File
            && remote.kind == EntryKind::File
            && local.hash.is_some()
            && local.base_hash != local.hash
            && remote.base_hash == local.hash
        {
            let mut tombstone = sanitize_remote(remote, clean_path, meta);
            tombstone.provenance = merged_prov;
            if let Some(lh) = local.hash {
                tombstone.push_ancestry_hash(lh);
            }
            tombstone.merge_ancestry_from(local);
            tombstone.conflict_of = None;
            return ReconcileDecision::DeleteToBin {
                tombstone,
                bin_reason: BinReason::DeletedByPeer,
            };
        }
        // Otherwise existing rule: a real change beats a deletion.
        let mut kept = local.clone();
        kept.provenance = merged_prov;
        kept.merge_ancestry_from(remote);
        return ReconcileDecision::MetadataOnly {
            record: kept,
            update_disk_metadata: false,
        };
    }
    if local.deleted && !remote.deleted {
        if disjoint_histories
            && !remote_matches_conflict_copy
            && local.kind == EntryKind::File
            && remote.kind == EntryKind::File
            && remote.hash.is_some()
            && remote.base_hash != remote.hash
            && local.base_hash == remote.hash
        {
            let mut tombstone = local.clone();
            tombstone.provenance = merged_prov;
            if let Some(rh) = remote.hash {
                tombstone.push_ancestry_hash(rh);
            }
            tombstone.merge_ancestry_from(remote);
            tombstone.conflict_of = None;
            return ReconcileDecision::MetadataOnly {
                record: tombstone,
                update_disk_metadata: false,
            };
        }
        // Otherwise existing rule: a real change beats a deletion.
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
        revived.provenance = merged_prov;
        revived.merge_ancestry_from(local);
        return ReconcileDecision::Fetch {
            record: revived,
            bin_reason: BinReason::ReplacedByPeer,
            bump_clock: false,
        };
    }

    // Both live and concurrent with identical content -> merge quietly.
    if local.content_equal(remote) {
        let winner = if compare_live_winner(local, remote) != Ordering::Less {
            local
        } else {
            remote
        };
        let mut merged = local.clone();
        merged.provenance = merged_prov;
        merged.base_hash = winner.base_hash.or(local.base_hash).or(remote.base_hash);
        merged.mtime = winner.mtime;
        merged.modified_by = winner.modified_by.clone();
        if winner.mode.is_some() {
            merged.mode = winner.mode;
        }
        merged.merge_ancestry_from(remote);
        merged.conflict_of = derive_conflict_of(&merged, meta);
        let update_disk_metadata = local.mtime != merged.mtime || local.mode != merged.mode;
        return ReconcileDecision::MetadataOnly {
            record: merged,
            update_disk_metadata,
        };
    }

    // Decision 4 (BEFORE any conflict rule) — live vs live with different content:
    // If one side's edit was made directly on top of the other side's current content hash
    // (`base_hash == other.hash`), the other side holds nothing this edit has not seen: the edit
    // wins without a conflict copy and takes over both sides' version keys.
    // (Live conflict copies remain protected against silent overwrite.)
    if disjoint_histories && !local_matches_conflict_copy && !remote_matches_conflict_copy {
        let remote_built_on_local = local.kind == EntryKind::File
            && remote.kind == EntryKind::File
            && local.hash.is_some()
            && local.base_hash != local.hash
            && remote.base_hash == local.hash;
        let local_built_on_remote = local.kind == EntryKind::File
            && remote.kind == EntryKind::File
            && remote.hash.is_some()
            && remote.base_hash != remote.hash
            && local.base_hash == remote.hash;
        if remote_built_on_local && !local_built_on_remote {
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
            target.provenance = merged_prov;
            if let Some(lh) = local.hash {
                target.push_ancestry_hash(lh);
            }
            target.merge_ancestry_from(local);
            return ReconcileDecision::Fetch {
                record: target,
                bin_reason: BinReason::ReplacedByPeer,
                bump_clock: false,
            };
        }
        if local_built_on_remote && !remote_built_on_local {
            let mut kept = local.clone();
            kept.provenance = merged_prov;
            if let Some(rh) = remote.hash {
                kept.push_ancestry_hash(rh);
            }
            kept.merge_ancestry_from(remote);
            return ReconcileDecision::MetadataOnly {
                record: kept,
                update_disk_metadata: false,
            };
        }
    }

    // Two real concurrent changes with different content -> keep both!
    reconcile_live_conflict(
        local_view,
        local,
        remote,
        clean_path,
        peer_member,
        meta,
        caps,
        merged_prov,
        false,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn reconcile_live_conflict(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
    winner_provenance: RecordProvenance,
    bump_if_local_wins: bool,
    bump_if_remote_wins: bool,
) -> ReconcileDecision {
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
            winner_provenance,
            bump_if_local_wins,
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
            winner_provenance,
            bump_if_remote_wins,
        );
    }

    // Never replace a live conflict copy with different content; enforce symmetrically using the
    // canonical conflict-copy name/hash match so every member picks the exact same winner.
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
            winner_provenance,
            bump_if_local_wins,
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
            winner_provenance,
            bump_if_remote_wins,
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
            winner_provenance,
            bump_if_local_wins,
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
            winner_provenance,
            bump_if_remote_wins,
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
    winner_provenance: RecordProvenance,
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
    winner.provenance = winner_provenance;
    winner.merge_ancestry_from(remote);

    // Conflict copy is a new independent file; its version is assigned fresh by apply.
    let mut loser = sanitize_remote(remote, &conflict_path, meta);
    loser.provenance = RecordProvenance::Versioned(VersionVector::new());
    loser.base_hash = None;
    loser.recent_hashes.clear();
    loser.conflict_dismissed = false;
    loser.conflict_of = derive_conflict_of(&loser, meta).or_else(|| Some(clean_path.to_string()));

    ReconcileDecision::ConflictRemoteLoses {
        winner_record: winner,
        loser_record: loser,
        bump_winner_clock: needs_winner_bump,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_local_loses_conflict(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    meta: &FolderMeta,
    caps: &FsCapabilities,
    winner_provenance: RecordProvenance,
    needs_winner_bump: bool,
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
    winner.provenance = winner_provenance;
    winner.merge_ancestry_from(local);

    // Conflict copy is a new independent file; its version is assigned fresh by apply.
    let mut loser = local.clone();
    loser.path = conflict_path;
    loser.provenance = RecordProvenance::Versioned(VersionVector::new());
    loser.base_hash = None;
    loser.recent_hashes.clear();
    loser.conflict_dismissed = false;
    loser.conflict_of = derive_conflict_of(&loser, meta).or_else(|| Some(clean_path.to_string()));

    ReconcileDecision::ConflictLocalLoses {
        winner_record: winner,
        loser_record: loser,
        bump_winner_clock: needs_winner_bump,
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
