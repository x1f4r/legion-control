//! Core domain types for hubless folder synchronization.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use legion_xfer::{EntryKind, Hash, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SyncError};
use crate::version::VersionVector;

/// Stable identifier of a sync folder (for example `"documents"` or a UUID).
pub type FolderId = String;

/// Identifier of a machine participating in a sync folder (for example `"tower"`, `"pi"`,
/// `"laptop"`).
pub type MemberId = String;

/// Default retention duration for files moved to `.legion-sync/bin` (30 days).
pub const DEFAULT_BIN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Validates a sync folder identifier.
pub fn validate_folder_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        return Err(SyncError::InvalidConfig {
            message: format!("sync folder id {id:?} must be between 1 and 128 bytes"),
            hint: Some("Choose a short non-empty folder identifier.".to_string()),
        });
    }
    if id == "." || id == ".." {
        return Err(SyncError::InvalidConfig {
            message: format!("sync folder id {id:?} cannot be '.' or '..'"),
            hint: Some("Choose a descriptive identifier such as 'documents'.".to_string()),
        });
    }
    if id
        .bytes()
        .any(|b| b == 0 || b < 0x20 || b == b'/' || b == b'\\')
    {
        return Err(SyncError::InvalidConfig {
            message: format!("sync folder id {id:?} contains disallowed characters"),
            hint: Some(
                "Use printable characters without path separators or control characters."
                    .to_string(),
            ),
        });
    }
    Ok(())
}

/// Validates a member (machine) identifier.
///
/// Member identifiers appear inside conflict copy filenames
/// (`name (changed on <machine> <date>).ext`), so characters forbidden in cross-platform
/// filenames are refused here.
pub fn validate_member_id(member: &str) -> Result<()> {
    if member.is_empty() || member.len() > 64 {
        return Err(SyncError::InvalidConfig {
            message: format!("member id {member:?} must be between 1 and 64 bytes"),
            hint: Some("Use a machine name between 1 and 64 characters long.".to_string()),
        });
    }
    if member == "."
        || member == ".."
        || member.ends_with('.')
        || member.ends_with(' ')
        || member.starts_with(' ')
    {
        return Err(SyncError::InvalidConfig {
            message: format!("member id {member:?} has invalid leading or trailing characters"),
            hint: Some("Use a plain machine name such as 'tower' or 'laptop'.".to_string()),
        });
    }
    if member.chars().any(|c| {
        c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
    }) {
        return Err(SyncError::InvalidConfig {
            message: format!("member id {member:?} contains disallowed filename characters"),
            hint: Some(
                "Use letters, digits, hyphens, or underscores for machine names.".to_string(),
            ),
        });
    }
    Ok(())
}

/// Configuration for initializing or opening a [`crate::SyncEngine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderConfig {
    /// Stable identifier shared by all members of this sync folder.
    pub id: FolderId,
    /// Name of this machine.
    pub local_member: MemberId,
    /// Current members of the folder (must include `local_member` while participating).
    pub members: BTreeSet<MemberId>,
    /// Per-folder ignore globs (trailing `/` restricts to directories).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// How long deleted or replaced files remain in `.legion-sync/bin` before expiry.
    #[serde(default = "default_bin_retention_secs")]
    pub bin_retention_secs: u64,
}

fn default_bin_retention_secs() -> u64 {
    DEFAULT_BIN_RETENTION.as_secs()
}

impl FolderConfig {
    /// Creates a configuration with default 30-day bin retention and no custom ignore rules.
    pub fn new(
        id: impl Into<FolderId>,
        local_member: impl Into<MemberId>,
        members: impl IntoIterator<Item = MemberId>,
    ) -> Self {
        let local = local_member.into();
        let mut set: BTreeSet<MemberId> = members.into_iter().collect();
        set.insert(local.clone());
        Self {
            id: id.into(),
            local_member: local,
            members: set,
            ignore: Vec::new(),
            bin_retention_secs: DEFAULT_BIN_RETENTION.as_secs(),
        }
    }

    /// Validates all identifiers in this configuration.
    pub fn validate(&self) -> Result<()> {
        validate_folder_id(&self.id)?;
        validate_member_id(&self.local_member)?;
        for member in &self.members {
            validate_member_id(member)?;
        }
        Ok(())
    }

    /// Returns the configured bin retention duration.
    pub fn bin_retention(&self) -> Duration {
        Duration::from_secs(self.bin_retention_secs)
    }
}

/// Persistent folder-level metadata stored in the index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderMeta {
    /// Stable folder identifier.
    pub folder_id: FolderId,
    /// Local machine identifier.
    pub local_member: MemberId,
    /// Current member set for this folder.
    pub members: BTreeSet<MemberId>,
    /// Strictly monotonic per-member sequence number assigned to every index record update.
    pub local_seq: u64,
    /// Strictly monotonic per-member logical clock used for version vector counters.
    ///
    /// Never resets even if tombstones are garbage-collected, guaranteeing a member never reuses
    /// a lower version counter after a deletion has been pruned.
    pub local_clock: u64,
    /// Whether syncing is currently paused on this member.
    pub paused: bool,
}

impl FolderMeta {
    /// Creates initial folder metadata from a [`FolderConfig`].
    pub fn new(config: &FolderConfig) -> Self {
        Self {
            folder_id: config.id.clone(),
            local_member: config.local_member.clone(),
            members: config.members.clone(),
            local_seq: 0,
            local_clock: 0,
            paused: false,
        }
    }

    /// Allocates and returns the next sequence number.
    pub fn next_seq(&mut self) -> u64 {
        self.local_seq = self.local_seq.saturating_add(1);
        self.local_seq
    }

    /// Allocates and returns the next local logical clock value.
    pub fn next_clock(&mut self) -> u64 {
        self.local_clock = self.local_clock.saturating_add(1);
        self.local_clock
    }
}

/// One path's indexed state on a member, exchanged incrementally by sequence number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRecord {
    /// Forward-slash relative path inside the sync folder.
    pub path: String,
    /// Filesystem entry kind (`File`, `Dir`, or `Symlink`).
    pub kind: EntryKind,
    /// Byte length for live regular files; `0` for directories, symlinks, and deletions.
    pub size: u64,
    /// Logical modification time carried across members for deterministic conflict tie-breaking.
    pub mtime: Timestamp,
    /// Actual modification time observed on the local filesystem after writing or scanning.
    ///
    /// Stored locally to avoid spurious re-hashes on filesystems that truncate sub-second
    /// timestamps (such as HFS+ or FAT), and omitted on the wire so every member sees the exact
    /// same logical [`Self::mtime`].
    #[serde(skip)]
    pub disk_mtime: Option<Timestamp>,
    /// Optional local-only change fingerprint from `lstat` (inode/ctime on Unix), used to detect
    /// same-size, same-mtime overwrites or renames without re-hashing unchanged files.
    #[serde(skip)]
    pub disk_change_id: Option<u64>,
    /// Unix permission bits (`0o777` mask) when known; preserved through Windows relays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// BLAKE3 digest for live regular files; `None` for directories, symlinks, and deletions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Hash>,
    /// Target string for live symlinks; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    /// `true` when this record is a deletion tombstone.
    pub deleted: bool,
    /// Causal version vector for this path.
    pub version: VersionVector,
    /// Member that authored the latest modification or deletion of this version.
    pub modified_by: MemberId,
    /// Monotonic local sequence number assigned by the member serving this record.
    pub seq: u64,
    /// Origin `(member, seq)` dependencies for deletion tombstones.
    ///
    /// Empty for live records. A tombstone may be garbage-collected once every current member in
    /// the folder has transitively observed every `(origin, seq)` in this map.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tombstone_deps: BTreeMap<MemberId, u64>,
    /// If this live record was created as a conflict copy of another path, names that original
    /// path so every member reports the same two-versions list until resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_of: Option<String>,
}

impl IndexRecord {
    /// Returns `true` when `self` and `other` represent the exact same filesystem content.
    ///
    /// Two deletions have equal content. Two live entries have equal content iff they share the
    /// same kind and, for files, the same BLAKE3 hash (and size), or for symlinks, the same target.
    pub fn content_equal(&self, other: &Self) -> bool {
        if self.deleted != other.deleted {
            return false;
        }
        if self.deleted {
            return true;
        }
        if self.kind != other.kind {
            return false;
        }
        match self.kind {
            EntryKind::Dir => true,
            EntryKind::File => {
                self.size == other.size && self.hash.is_some() && self.hash == other.hash
            }
            EntryKind::Symlink => self.link_target == other.link_target,
        }
    }

    /// Returns a copy suitable for sending over the wire (stripping local-only disk metadata).
    pub fn for_wire(&self) -> Self {
        let mut wire = self.clone();
        wire.disk_mtime = None;
        wire.disk_change_id = None;
        wire
    }
}

/// Pairwise transitive causal knowledge matrix used for safe tombstone garbage collection.
///
/// `rows[observer][origin] = seq` records that `observer` has causally absorbed every index change
/// `origin` made up to `origin`'s sequence number `seq`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalMatrix {
    /// Map from observer member -> (origin member -> highest absorbed sequence number).
    pub rows: BTreeMap<MemberId, BTreeMap<MemberId, u64>>,
}

impl CausalMatrix {
    /// Creates an empty causal matrix.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the highest `origin` sequence number known to have been absorbed by `observer`.
    pub fn get(&self, observer: &str, origin: &str) -> u64 {
        self.rows
            .get(observer)
            .and_then(|row| row.get(origin))
            .copied()
            .unwrap_or(0)
    }

    /// Raises `rows[observer][origin]` to at least `seq`.
    pub fn record_seen(&mut self, observer: &str, origin: &str, seq: u64) {
        if seq == 0 {
            return;
        }
        let row = self.rows.entry(observer.to_string()).or_default();
        let entry = row.entry(origin.to_string()).or_insert(0);
        if seq > *entry {
            *entry = seq;
        }
    }

    /// Ensures every member in `members` has a row in the matrix.
    pub fn ensure_members(&mut self, members: &BTreeSet<MemberId>) {
        for member in members {
            self.rows.entry(member.clone()).or_default();
        }
    }

    /// Updates the matrix after `local` finishes absorbing *all* records of `peer` up to
    /// `peer_snapshot_seq`, where `peer_matrix` was captured at the exact same snapshot.
    pub fn absorb_peer_full(
        &mut self,
        local: &str,
        peer: &str,
        peer_snapshot_seq: u64,
        peer_matrix: &CausalMatrix,
    ) {
        self.record_seen(local, peer, peer_snapshot_seq);
        if let Some(peer_row) = peer_matrix.rows.get(peer) {
            for (origin, &seq) in peer_row {
                self.record_seen(local, origin, seq);
            }
        }
        self.merge_other_rows(local, peer_matrix);
    }

    /// Updates the matrix after `local` absorbs a partial prefix of `peer`'s records up to
    /// `processed_seq` (without yet reaching `peer_snapshot_seq`).
    ///
    /// Does NOT copy `peer`'s own transitive row into `local`'s row because records between
    /// `processed_seq` and `peer_snapshot_seq` have not yet been applied.
    pub fn absorb_peer_partial(
        &mut self,
        local: &str,
        peer: &str,
        processed_seq: u64,
        peer_matrix: &CausalMatrix,
    ) {
        self.record_seen(local, peer, processed_seq);
        self.merge_other_rows(local, peer_matrix);
    }

    fn merge_other_rows(&mut self, local: &str, peer_matrix: &CausalMatrix) {
        for (observer, row) in &peer_matrix.rows {
            if observer == local {
                continue;
            }
            for (origin, &seq) in row {
                self.record_seen(observer, origin, seq);
            }
        }
    }

    /// Returns `true` when every member in `current_members` has causally observed every
    /// `(origin, seq)` pair in `deps`.
    pub fn all_members_seen(
        &self,
        current_members: &BTreeSet<MemberId>,
        deps: &BTreeMap<MemberId, u64>,
    ) -> bool {
        if current_members.is_empty() || deps.is_empty() {
            return false;
        }
        for (origin, &required_seq) in deps {
            for member in current_members {
                if self.get(member, origin) < required_seq {
                    return false;
                }
            }
        }
        true
    }

    /// Removes the observer row for a member that left the folder so it no longer blocks
    /// tombstone garbage collection.
    pub fn remove_observer(&mut self, member: &str) {
        self.rows.remove(member);
    }

    /// Prunes origin columns belonging to members that are neither current members nor referenced
    /// by any remaining tombstone.
    pub fn prune_unused_origins(
        &mut self,
        current_members: &BTreeSet<MemberId>,
        active_dep_origins: &BTreeSet<MemberId>,
    ) {
        for row in self.rows.values_mut() {
            row.retain(|origin, _| {
                current_members.contains(origin) || active_dep_origins.contains(origin)
            });
        }
    }
}

/// Page of records returned by [`crate::Peer::records_since`] from one consistent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordsPage {
    /// Records with `seq > since_seq`, ordered by ascending `seq`.
    pub records: Vec<IndexRecord>,
    /// Peer's `local_seq` at the consistent snapshot where this page was read.
    pub snapshot_seq: u64,
    /// `true` if more records exist beyond this page in the snapshot.
    pub has_more: bool,
    /// Peer's causal matrix captured at the exact same snapshot.
    pub matrix: CausalMatrix,
}

/// Why an entry was moved into `.legion-sync/bin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinReason {
    /// Deleted on a peer member.
    DeletedByPeer,
    /// Replaced by a newer version from a peer member.
    ReplacedByPeer,
    /// Replaced while resolving a conflict or restoring from the bin.
    ReplacedLocally,
}

/// Self-describing metadata for one item stored in `.legion-sync/bin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinEntry {
    /// Unique identifier for this bin entry.
    pub id: String,
    /// Original relative path inside the sync folder.
    pub original_path: String,
    /// Original entry kind (`File` or `Symlink`).
    pub kind: EntryKind,
    /// Byte length for regular files (`0` for symlinks).
    pub size: u64,
    /// Original modification time of the entry before it was moved to the bin.
    pub original_mtime: Timestamp,
    /// Original Unix permission bits, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<u32>,
    /// Verified BLAKE3 digest for regular files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<Hash>,
    /// Target string for symlinks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
    /// Timestamp when this entry was placed in the bin.
    pub deleted_at: Timestamp,
    /// Reason the entry was moved to the bin.
    pub reason: BinReason,
    /// Member responsible for the deletion or replacement.
    pub by_member: MemberId,
    /// Flat filename inside `.legion-sync/bin/` holding the payload bytes (empty for symlinks).
    pub storage_name: String,
}

/// Policy when restoring a [`BinEntry`] onto a path that currently holds a live entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestoreStrategy {
    /// Refuse with [`SyncError::Exists`] if a live entry already exists at the destination.
    #[default]
    FailIfExists,
    /// Move the existing live entry into `.legion-sync/bin` first, then restore.
    Replace,
    /// Restore alongside the existing file under `name (restored <date>).ext`.
    Rename,
}

/// Classification of a held-back path problem that requires user attention or disappears when the
/// conflicting path is renamed or deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// Two live paths differ only in letter case on a case-insensitive filesystem.
    CaseClash,
    /// Filename contains characters forbidden by Windows (`< > : " | ? *` or control bytes).
    ForbiddenWindowsChar,
    /// Filename ends with a trailing dot or space, forbidden on Windows.
    TrailingDotOrSpace,
    /// Filename uses a reserved Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`, ...).
    ReservedWindowsName,
    /// A single path segment exceeds the filesystem's maximum length.
    SegmentTooLong,
    /// The full path exceeds the filesystem's maximum length.
    PathTooLong,
    /// Symbolic links are held back on Windows.
    UnsupportedSymlink,
    /// An entry could not be read or written because of local permissions or filesystem error.
    FilesystemError,
}

/// Human-readable problem reported in [`FolderStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProblem {
    /// Relative path affected by the problem.
    pub path: String,
    /// Machine-readable problem classification.
    pub kind: ProblemKind,
    /// Peer that offered this record, if the problem came from pulling a remote record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<MemberId>,
    /// Plain-language sentence explaining why this item is held back.
    pub sentence: String,
}

/// A peer record that could not be materialized on the local filesystem and is held back until
/// the local or remote condition clears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldBackRecord {
    /// The incoming peer record that was held back.
    pub record: IndexRecord,
    /// Peer from which the record was received.
    pub from_peer: MemberId,
    /// Specific problem describing why the record was held back.
    pub problem: SyncProblem,
}

/// An unresolved conflict ("file with two versions") reported in [`FolderStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictInfo {
    /// Original (winning) path.
    pub original_path: String,
    /// Conflict copy (losing) path.
    pub conflict_path: String,
    /// Member whose version became the conflict copy.
    pub conflict_member: MemberId,
    /// Modification time of the conflict copy.
    pub conflict_mtime: Timestamp,
    /// Size of the conflict copy in bytes.
    pub conflict_size: u64,
    /// BLAKE3 digest of the conflict copy (when it is a regular file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_hash: Option<Hash>,
}

/// How a person resolves a file with two versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictChoice {
    /// Keep the original path and move the conflict copy to the bin.
    KeepOriginal,
    /// Move the original path to the bin and promote the conflict copy to the original path.
    KeepConflict,
    /// Keep both files as ordinary files and clear the conflict flag.
    KeepBoth,
}

/// High-level state of a sync member, matching the exact five states in the design.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemberState {
    /// All known changes are applied and no problems are open.
    UpToDate,
    /// Actively syncing changes with remaining file and byte counts.
    Syncing {
        /// Number of entries left to apply.
        files_left: u64,
        /// Number of bytes left to transfer.
        bytes_left: u64,
    },
    /// Up to date with reachable members, waiting for one or more offline members.
    WaitingForMember {
        /// Members currently unreachable.
        members: Vec<MemberId>,
    },
    /// Syncing is paused for this folder.
    Paused,
    /// One or more items are held back or failed and need attention.
    Problem {
        /// One plain-language sentence summarizing the problem.
        sentence: String,
        /// All currently open problems.
        problems: Vec<SyncProblem>,
    },
}

/// Status entry for one member of a sync folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberStatus {
    /// Member identifier.
    pub member: MemberId,
    /// Current state from the local member's perspective.
    pub state: MemberState,
    /// Highest sequence number pulled from this peer (or `local_seq` for self).
    pub cursor_seq: u64,
}

/// Complete status snapshot for a sync folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderStatus {
    /// Folder identifier.
    pub folder_id: FolderId,
    /// Local machine identifier.
    pub local_member: MemberId,
    /// Overall state of this member.
    pub state: MemberState,
    /// Status per folder member (including self).
    pub members: BTreeMap<MemberId, MemberStatus>,
    /// Files currently having two versions.
    pub conflicts: Vec<ConflictInfo>,
    /// Current contents of `.legion-sync/bin`.
    pub bin: Vec<BinEntry>,
}

/// Snapshot of what the local live disk held immediately before a crash-safe apply began.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpectedLocalSnapshot {
    /// No live entry existed at the path.
    Missing,
    /// A regular file with the recorded size, mtime, and BLAKE3 hash existed.
    File {
        /// File size in bytes.
        size: u64,
        /// Logical or disk modification time.
        mtime: Timestamp,
        /// Verified BLAKE3 digest.
        hash: Hash,
    },
    /// A directory existed.
    Dir,
    /// A symbolic link existed with the given target.
    Symlink {
        /// Verbatim symlink target.
        link_target: String,
    },
}

/// Write-ahead intent persisted in the index store before mutating the live working tree.
///
/// If the process crashes at any step between moving an old file to `.legion-sync/bin`, creating a
/// conflict copy, renaming a staged file into place, and committing the index record, startup
/// recovery inspects this journal entry alongside the filesystem and deterministically completes
/// or rolls back the operation without losing data or creating spurious conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PendingOp {
    /// Placing a fetched file, directory, or symlink at `target_record.path`, optionally moving an
    /// existing entry to `.legion-sync/bin` first.
    Place {
        /// Target index record to commit on completion.
        target_record: IndexRecord,
        /// Staged filename inside `.legion-sync/tmp/`, when `target_record.kind == EntryKind::File`.
        staged_tmp: Option<String>,
        /// Metadata for the previous live entry moved to `.legion-sync/bin/`, if any.
        bin_entry: Option<BinEntry>,
        /// What the live disk held before touching it.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Deleting an existing live entry to `.legion-sync/bin/` and committing a tombstone.
    Delete {
        /// Target deletion tombstone to commit on completion.
        target_record: IndexRecord,
        /// Metadata for the deleted entry moved to `.legion-sync/bin/`, if any.
        bin_entry: Option<BinEntry>,
        /// What the live disk held before deleting it.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Resolving a concurrent conflict where the local entry loses and moves to `loser_record.path`
    /// while the remote winner takes `winner_record.path`.
    ConflictLocalLoses {
        /// Winning record taking the original path.
        winner_record: IndexRecord,
        /// Losing record moving to the conflict copy path.
        loser_record: IndexRecord,
        /// Staged filename inside `.legion-sync/tmp/` for the winning file, if it is a file.
        winner_tmp: Option<String>,
        /// Whether the conflict copy already existed with identical content before apply.
        conflict_already_present: bool,
        /// What the live disk held at the original path before touching it.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Resolving a concurrent conflict where the local entry keeps `winner_record.path` and the
    /// remote loser is written to `loser_record.path`.
    ConflictRemoteLoses {
        /// Updated winning record (merged version vector) at the original path.
        winner_record: IndexRecord,
        /// Losing record placed at the conflict copy path.
        loser_record: IndexRecord,
        /// Staged filename inside `.legion-sync/tmp/` for the losing file, if it is a file.
        loser_tmp: Option<String>,
        /// Whether the conflict copy already existed with identical content before apply.
        conflict_already_present: bool,
        /// What the live disk held at the original path before touching it.
        expected_local: ExpectedLocalSnapshot,
    },
}

/// Progress snapshot reported during or between peer sync passes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProgress {
    /// Number of entries still waiting to be applied.
    pub files_left: u64,
    /// Number of bytes still waiting to be transferred.
    pub bytes_left: u64,
}
