//! Pure pull-only reconciliation logic.
//!
//! Given a local index lookup and a remote peer's [`IndexRecord`], [`reconcile_record`] produces a
//! deterministic [`ReconcileDecision`] without performing any disk or network I/O.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use legion_xfer::EntryKind;

use crate::model::{IndexRecord, MemberId, ProblemKind, SyncProblem};
use crate::path::{
    IgnoreMatcher, ancestors, check_fs_compatibility, conflict_copy_path, fold_case,
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
    /// Incoming path violated universal safety rules (escape, traversal, `.legion-sync`).
    RejectUnsafe {
        /// Raw unsafe path sent by the peer.
        path: String,
        /// Explanation of why the path was refused.
        reason: String,
    },
    /// Path is excluded by fixed or per-folder ignore rules.
    Ignore,
    /// Local index is already equal or strictly newer; nothing to change.
    Keep,
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
    },
    /// Remote deletion wins and the local live entry should be moved to `.legion-sync/bin`.
    DeleteToBin {
        /// Target tombstone record to commit (caller assigns a fresh local `seq`).
        tombstone: IndexRecord,
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
        /// Local losing entry moving to the conflict copy path.
        loser_record: IndexRecord,
    },
    /// Concurrent edit with different content where the local entry keeps `winner_record.path`
    /// and the remote losing entry is written to `loser_record.path`.
    ConflictRemoteLoses {
        /// Local winning entry updated with the merged version vector.
        winner_record: IndexRecord,
        /// Remote losing entry placed at the conflict copy path.
        loser_record: IndexRecord,
        /// `true` when the winner is a local directory that had to beat a strictly newer remote
        /// file/symlink because live descendants exist under the directory, requiring a fresh
        /// local clock bump on `winner_record.version`.
        bump_winner_clock: bool,
    },
    /// Entry cannot be represented on the local filesystem (case clash, Windows naming rules,
    /// over-long path, or symlink on Windows); hold back and report as a problem.
    HoldBack {
        /// Human-readable problem explaining why the record is held back.
        problem: SyncProblem,
    },
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
        // Alphabetically smaller member name wins mtime ties.
        .then_with(|| b.modified_by.cmp(&a.modified_by))
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| a.hash.cmp(&b.hash))
        .then_with(|| a.link_target.cmp(&b.link_target))
}

/// Merges two tombstone dependency maps via componentwise maximum.
pub fn merge_tombstone_deps(
    a: &BTreeMap<MemberId, u64>,
    b: &BTreeMap<MemberId, u64>,
) -> BTreeMap<MemberId, u64> {
    let mut out = a.clone();
    for (member, &seq) in b {
        let entry = out.entry(member.clone()).or_insert(0);
        if seq > *entry {
            *entry = seq;
        }
    }
    out
}

/// Ensures a tombstone carries at least one origin `(member, seq)` dependency so it is never
/// prematurely garbage-collected.
pub fn effective_tombstone_deps(
    remote: &IndexRecord,
    peer_member: &str,
) -> BTreeMap<MemberId, u64> {
    if !remote.tombstone_deps.is_empty() {
        return remote.tombstone_deps.clone();
    }
    let mut deps = BTreeMap::new();
    if remote.seq > 0 {
        deps.insert(peer_member.to_string(), remote.seq);
    }
    deps
}

/// Decides what the local member should do with an incoming `remote` record from `peer_member`.
pub fn reconcile_record(
    local_view: &dyn LocalIndexView,
    remote: &IndexRecord,
    peer_member: &str,
    caps: &FsCapabilities,
    ignore: &IgnoreMatcher,
) -> ReconcileDecision {
    let clean_path = match validate_sync_path(&remote.path) {
        Ok(p) => p,
        Err(err) => {
            return ReconcileDecision::RejectUnsafe {
                path: remote.path.clone(),
                reason: err.to_string(),
            };
        }
    };

    if ignore.is_path_or_ancestor_ignored(&clean_path, remote.kind == EntryKind::Dir) {
        return ReconcileDecision::Ignore;
    }

    let local_opt = local_view.get_record(&clean_path);
    match local_opt {
        None => reconcile_without_local(local_view, remote, &clean_path, peer_member, caps),
        Some(local) => {
            reconcile_with_local(local_view, &local, remote, &clean_path, peer_member, caps)
        }
    }
}

fn reconcile_without_local(
    local_view: &dyn LocalIndexView,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if remote.deleted {
        let mut tombstone = remote.for_wire();
        tombstone.path = clean_path.to_string();
        tombstone.tombstone_deps = effective_tombstone_deps(remote, peer_member);
        tombstone.conflict_of = None;
        return ReconcileDecision::MetadataOnly {
            record: tombstone,
            update_disk_metadata: false,
        };
    }

    if let Some(problem) =
        check_local_representable(local_view, clean_path, remote.kind, peer_member, caps, None)
    {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut target = remote.for_wire();
    target.path = clean_path.to_string();
    target.tombstone_deps.clear();
    ReconcileDecision::Fetch { record: target }
}

fn reconcile_with_local(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    match local.version.compare(&remote.version) {
        VersionOrder::Newer => ReconcileDecision::Keep,
        VersionOrder::Equal => {
            if local.deleted && remote.deleted {
                let remote_deps = effective_tombstone_deps(remote, peer_member);
                let merged_deps = merge_tombstone_deps(&local.tombstone_deps, &remote_deps);
                if merged_deps != local.tombstone_deps {
                    let mut updated = local.clone();
                    updated.tombstone_deps = merged_deps;
                    return ReconcileDecision::MetadataOnly {
                        record: updated,
                        update_disk_metadata: false,
                    };
                }
                return ReconcileDecision::Keep;
            }
            if local.content_equal(remote) {
                return ReconcileDecision::Keep;
            }
            // Defensive fallback: identical versions with divergent content should never occur,
            // but treating them as concurrent guarantees neither version is ever lost.
            reconcile_concurrent(local_view, local, remote, clean_path, peer_member, caps)
        }
        VersionOrder::Older => {
            reconcile_remote_newer(local_view, local, remote, clean_path, peer_member, caps)
        }
        VersionOrder::Concurrent => {
            reconcile_concurrent(local_view, local, remote, clean_path, peer_member, caps)
        }
    }
}

fn reconcile_remote_newer(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if remote.deleted {
        let remote_deps = effective_tombstone_deps(remote, peer_member);
        if local.deleted {
            let mut tombstone = remote.for_wire();
            tombstone.path = clean_path.to_string();
            tombstone.tombstone_deps = merge_tombstone_deps(&local.tombstone_deps, &remote_deps);
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

        let mut tombstone = remote.for_wire();
        tombstone.path = clean_path.to_string();
        tombstone.tombstone_deps = remote_deps;
        tombstone.conflict_of = None;
        return ReconcileDecision::DeleteToBin { tombstone };
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
        updated.conflict_of = remote.conflict_of.clone();
        updated.tombstone_deps.clear();
        let update_disk_metadata = local.mtime != remote.mtime || local.mode != updated.mode;
        return ReconcileDecision::MetadataOnly {
            record: updated,
            update_disk_metadata,
        };
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
            caps,
            true,
        );
    }

    if let Some(problem) = check_local_representable(
        local_view,
        clean_path,
        remote.kind,
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

    let mut target = remote.for_wire();
    target.path = clean_path.to_string();
    target.tombstone_deps.clear();
    ReconcileDecision::Fetch { record: target }
}

fn reconcile_concurrent(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    let merged_version = local.version.merge(&remote.version);

    // Concurrent deletions merge quietly.
    if local.deleted && remote.deleted {
        let remote_deps = effective_tombstone_deps(remote, peer_member);
        let mut merged = if compare_live_winner(local, remote) != Ordering::Less {
            local.clone()
        } else {
            remote.for_wire()
        };
        merged.path = clean_path.to_string();
        merged.deleted = true;
        merged.version = merged_version;
        merged.tombstone_deps = merge_tombstone_deps(&local.tombstone_deps, &remote_deps);
        merged.conflict_of = None;
        return ReconcileDecision::MetadataOnly {
            record: merged,
            update_disk_metadata: false,
        };
    }

    // Delete vs change: the change always wins.
    if !local.deleted && remote.deleted {
        let mut kept = local.clone();
        kept.version = merged_version;
        kept.tombstone_deps.clear();
        return ReconcileDecision::MetadataOnly {
            record: kept,
            update_disk_metadata: false,
        };
    }
    if local.deleted && !remote.deleted {
        if let Some(problem) =
            check_local_representable(local_view, clean_path, remote.kind, peer_member, caps, None)
        {
            return ReconcileDecision::HoldBack { problem };
        }
        let mut revived = remote.for_wire();
        revived.path = clean_path.to_string();
        revived.version = merged_version;
        revived.tombstone_deps.clear();
        return ReconcileDecision::Fetch { record: revived };
    }

    // Both live and concurrent with identical content -> merge quietly.
    if local.content_equal(remote) {
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
        merged.conflict_of = local
            .conflict_of
            .clone()
            .or_else(|| remote.conflict_of.clone());
        merged.tombstone_deps.clear();
        let update_disk_metadata = local.mtime != merged.mtime || local.mode != merged.mode;
        return ReconcileDecision::MetadataOnly {
            record: merged,
            update_disk_metadata,
        };
    }

    // Both live and concurrent with different content -> keep both!
    // Directory always keeps the original path when conflicting with a file or symlink so all
    // descendants keep their paths without recursive renames.
    if local.kind == EntryKind::Dir && remote.kind != EntryKind::Dir {
        return build_remote_loses_conflict(
            local_view,
            local,
            remote,
            clean_path,
            peer_member,
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
            caps,
            false,
        )
    } else {
        build_local_loses_conflict(local_view, local, remote, clean_path, peer_member, caps)
    }
}

fn build_remote_loses_conflict(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    caps: &FsCapabilities,
    needs_dir_bump: bool,
) -> ReconcileDecision {
    let conflict_path = choose_conflict_path(local_view, clean_path, remote);
    if let Some(problem) = check_local_representable(
        local_view,
        &conflict_path,
        remote.kind,
        peer_member,
        caps,
        Some(&conflict_path),
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut winner = local.clone();
    winner.version = local.version.merge(&remote.version);
    winner.tombstone_deps.clear();

    let mut loser = remote.for_wire();
    loser.path = conflict_path;
    loser.conflict_of = Some(clean_path.to_string());
    loser.tombstone_deps.clear();

    ReconcileDecision::ConflictRemoteLoses {
        winner_record: winner,
        loser_record: loser,
        bump_winner_clock: needs_dir_bump,
    }
}

fn build_local_loses_conflict(
    local_view: &dyn LocalIndexView,
    local: &IndexRecord,
    remote: &IndexRecord,
    clean_path: &str,
    peer_member: &str,
    caps: &FsCapabilities,
) -> ReconcileDecision {
    if let Some(problem) = check_local_representable(
        local_view,
        clean_path,
        remote.kind,
        peer_member,
        caps,
        Some(clean_path),
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let conflict_path = choose_conflict_path(local_view, clean_path, local);
    if let Some(problem) = check_local_representable(
        local_view,
        &conflict_path,
        local.kind,
        peer_member,
        caps,
        Some(&conflict_path),
    ) {
        return ReconcileDecision::HoldBack { problem };
    }

    let mut winner = remote.for_wire();
    winner.path = clean_path.to_string();
    winner.version = local.version.merge(&remote.version);
    winner.tombstone_deps.clear();

    let mut loser = local.clone();
    loser.path = conflict_path;
    loser.conflict_of = Some(clean_path.to_string());
    loser.tombstone_deps.clear();

    ReconcileDecision::ConflictLocalLoses {
        winner_record: winner,
        loser_record: loser,
    }
}

/// Computes the deterministic conflict copy path for `loser`, falling back to a short hash suffix
/// only if the primary `name (changed on <machine> <date>).ext` path already holds live content
/// that differs from `loser`.
pub fn choose_conflict_path(
    local_view: &dyn LocalIndexView,
    original_path: &str,
    loser: &IndexRecord,
) -> String {
    let primary = conflict_copy_path(original_path, &loser.modified_by, loser.mtime, None);
    match local_view.get_record(&primary) {
        None => primary,
        Some(existing) if existing.deleted || existing.content_equal(loser) => primary,
        Some(_) => {
            let short = loser_short_tag(loser);
            let candidate =
                conflict_copy_path(original_path, &loser.modified_by, loser.mtime, Some(&short));
            match local_view.get_record(&candidate) {
                None => candidate,
                Some(existing) if existing.deleted || existing.content_equal(loser) => candidate,
                Some(_) => {
                    for n in 2u32..10_000 {
                        let tag = format!("{short}-{n}");
                        let alt = conflict_copy_path(
                            original_path,
                            &loser.modified_by,
                            loser.mtime,
                            Some(&tag),
                        );
                        match local_view.get_record(&alt) {
                            None => return alt,
                            Some(e) if e.deleted || e.content_equal(loser) => return alt,
                            Some(_) => continue,
                        }
                    }
                    candidate
                }
            }
        }
    }
}

fn loser_short_tag(loser: &IndexRecord) -> String {
    if let Some(hash) = loser.hash {
        let hex = hash.to_hex();
        return hex[..8.min(hex.len())].to_string();
    }
    if let Some(ref target) = loser.link_target {
        let h = legion_xfer::hash::hash_bytes(target.as_bytes()).to_hex();
        return h[..8.min(h.len())].to_string();
    }
    format!("{}", loser.mtime.secs)
}

/// Checks platform naming/length/symlink rules and case-insensitive collisions against the local
/// index.
pub fn check_local_representable(
    local_view: &dyn LocalIndexView,
    rel_path: &str,
    kind: EntryKind,
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

    if caps.case_insensitive {
        // Check exact path and every ancestor prefix for a case-only collision with a different
        // live path in the local index.
        let mut to_check = ancestors(rel_path);
        to_check.push(rel_path);
        for candidate in to_check {
            let folded = fold_case(candidate);
            for existing in local_view.live_paths_for_folded(&folded) {
                if existing != candidate && replacing_exact != Some(existing.as_str()) {
                    return Some(SyncProblem {
                        path: rel_path.to_string(),
                        kind: ProblemKind::CaseClash,
                        peer: Some(peer_member.to_string()),
                        sentence: format!(
                            "'{rel_path}' was held back because its name differs only in letter case from '{existing}' on this file system."
                        ),
                    });
                }
            }
        }
    }

    None
}
