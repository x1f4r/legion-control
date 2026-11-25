//! Core domain types for hubless folder synchronization.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use legion_xfer::{EntryKind, Hash, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SyncError};
use crate::version::{VersionKey, VersionVector};

/// Stable identifier of a sync folder (for example `"documents"` or a UUID).
pub type FolderId = String;

/// Identifier of a machine participating in a sync folder (for example `"tower"`, `"pi"`,
/// `"laptop"`).
pub type MemberId = String;

/// Current durable index schema version. Opening an index with a higher version is refused
/// cleanly with [`SyncError::SchemaTooNew`].
pub const INDEX_SCHEMA_VERSION: u32 = 2;

/// Sync wire/trait protocol version advertised in [`PeerHandshake`].
pub const SYNC_PROTOCOL_VERSION: u32 = 2;

/// Default retention duration for files moved to `.legion-sync/bin` (30 days).
pub const DEFAULT_BIN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default fetch timeout when pulling a file from a peer.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of recent content hashes retained in [`IndexRecord::recent_hashes`].
pub const MAX_ANCESTRY_HASHES: usize = 16;

/// Maximum number of records accepted in one [`RecordsPage`].
pub const MAX_PAGE_RECORDS: usize = 4_096;

/// Maximum number of distinct incarnations per member accepted in one incoming [`VersionVector`].
pub const MAX_VV_INCARNATIONS_PER_MEMBER: usize = 8;

/// Maximum byte length of a symbolic link target accepted from a peer.
pub const MAX_LINK_TARGET_BYTES: usize = 4_096;

/// Maximum number of entries retained in the durable held-back table.
pub const MAX_HELD_BACK_ENTRIES: usize = 4_096;

/// Maximum number of invalid-record problems retained per peer.
pub const MAX_PEER_PROBLEMS: usize = 256;

/// Maximum number of consecutive deferrals for one `(peer, path)` before promoting it to a
/// visible problem and advancing past it so no single record stalls a peer forever.
pub const MAX_DEFERRAL_ATTEMPTS: u32 = 8;

/// Maximum number of deleted `(path -> hashes)` history entries retained for join/re-add safety.
pub const MAX_DELETED_HISTORY_PATHS: usize = 100_000;

/// Minimum plausible wall-clock timestamp (2026-01-01T00:00:00Z). Bin expiry never runs when the
/// local clock is earlier than this floor or earlier than the newest bin entry.
pub const MIN_PLAUSIBLE_CLOCK_SECS: i64 = 1_767_225_600;

/// Default minimum count of deleted files in one scan required to trip the mass-deletion brake.
pub const DEFAULT_BRAKE_MIN_FILES: usize = 50;

/// Default minimum percentage (1..=100) of live files deleted in one scan required to trip the
/// mass-deletion brake (more than a quarter).
pub const DEFAULT_BRAKE_MIN_PERCENT: u8 = 25;

/// Generates a cryptographically random non-zero 64-bit incarnation identifier.
pub fn random_incarnation_id() -> Result<u64> {
    let mut buf = [0u8; 8];
    for _ in 0..16 {
        getrandom::fill(&mut buf).map_err(|err| SyncError::Storage {
            context: "generate incarnation id".to_string(),
            detail: err.to_string(),
        })?;
        let val = u64::from_le_bytes(buf);
        if val != 0 {
            return Ok(val);
        }
    }
    Ok(1)
}

/// Generates a 16-hex-character random token for bin and temporary identifiers (unique across
/// process restarts and clock jumps).
///
/// Each thread seeds a local SplitMix64 state once from OS randomness (`getrandom`), so high
/// throughput apply batches avoid per-file OS RNG syscalls while still producing unique tokens
/// across restarts, threads, and clock jumps.
pub fn random_hex_token() -> String {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = Cell::new({
            let mut buf = [0u8; 8];
            if getrandom::fill(&mut buf).is_ok() {
                u64::from_le_bytes(buf)
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9e37_79b9_7f4a_7c15)
            }
        });
    }
    let val = STATE.with(|cell| {
        let s = cell.get().wrapping_add(0x9e37_79b9_7f4a_7c15);
        cell.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    });
    format!("{val:016x}")
}

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
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
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
/// (`name (changed on <machine> <date>, <hash6>).ext`), so characters forbidden in cross-platform
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
        c.is_control()
            || matches!(
                c,
                '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '#'
            )
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

/// Threshold configuration for the mass-deletion brake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionBrakeConfig {
    /// Minimum number of live files that must vanish in one scan to trip the brake (default 50).
    /// Setting this to `usize::MAX` disables the brake.
    pub min_deleted_files: usize,
    /// Minimum percentage (`1..=100`) of currently live files that must vanish in one scan
    /// together with `min_deleted_files` to trip the brake (default 25, i.e. more than a quarter).
    pub min_deleted_percent: u8,
}

impl Default for DeletionBrakeConfig {
    fn default() -> Self {
        Self {
            min_deleted_files: DEFAULT_BRAKE_MIN_FILES,
            min_deleted_percent: DEFAULT_BRAKE_MIN_PERCENT,
        }
    }
}

impl DeletionBrakeConfig {
    /// Returns `true` if deleting `deleted_files` out of `total_live_files` trips the brake.
    pub fn should_trip(&self, deleted_files: usize, total_live_files: usize) -> bool {
        if deleted_files == 0 || total_live_files == 0 {
            return false;
        }
        if deleted_files <= self.min_deleted_files {
            return false;
        }
        let lhs = (deleted_files as u128).saturating_mul(100);
        let rhs = (total_live_files as u128).saturating_mul(u128::from(self.min_deleted_percent));
        lhs > rhs
    }
}

/// Persisted state describing a tripped mass-deletion brake awaiting a person's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionBrake {
    /// Number of live files that disappeared in the scan that tripped the brake.
    pub deleted_count: usize,
    /// Number of live files indexed before the scan.
    pub total_live_files: usize,
    /// Up to 10 sample missing paths shown to the person.
    pub sample_paths: Vec<String>,
}

/// A person's response to a tripped mass-deletion brake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionBrakeDecision {
    /// "They are really deleted": unpause and proceed to record and propagate the deletions.
    ConfirmDeleted,
    /// "They are not": unpause and rescan with the brake still armed.
    Rescan,
}

/// Identity marker written inside `.legion-sync/marker.json` when a member joins a sync folder.
///
/// Checked before every scan and apply so an unmounted drive or replaced directory stops with a
/// clear problem instead of recording mass deletions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderMarker {
    /// Marker format version (`1`).
    pub version: u32,
    /// Sync folder identifier.
    pub folder_id: FolderId,
    /// Member identifier.
    pub member_id: MemberId,
    /// Active index incarnation identifier.
    pub incarnation: u64,
}

impl FolderMarker {
    /// Creates a v1 folder marker for `(folder_id, member_id, incarnation)`.
    pub fn new(folder_id: FolderId, member_id: MemberId, incarnation: u64) -> Self {
        Self {
            version: 1,
            folder_id,
            member_id,
            incarnation,
        }
    }
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
    /// Members that previously belonged to this folder (from the shared folder definition so a
    /// newly joined member can still verify historical version vectors and conflict labels
    /// authored before those members left).
    #[serde(default)]
    pub former_members: BTreeSet<MemberId>,
    /// Per-folder ignore globs (trailing `/` restricts to directories).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// How long deleted or replaced files remain in `.legion-sync/bin` before expiry.
    #[serde(default = "default_bin_retention_secs")]
    pub bin_retention_secs: u64,
    /// Mass-deletion brake thresholds.
    #[serde(default)]
    pub deletion_brake: DeletionBrakeConfig,
    /// Optional explicit initial incarnation (used by deterministic tests; `None` picks a random
    /// non-zero 64-bit value via `getrandom`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_incarnation: Option<u64>,
}

fn default_bin_retention_secs() -> u64 {
    DEFAULT_BIN_RETENTION.as_secs()
}

impl FolderConfig {
    /// Creates a configuration with default 30-day bin retention, default deletion brake, and no
    /// custom ignore rules.
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
            former_members: BTreeSet::new(),
            ignore: Vec::new(),
            bin_retention_secs: DEFAULT_BIN_RETENTION.as_secs(),
            deletion_brake: DeletionBrakeConfig::default(),
            initial_incarnation: None,
        }
    }

    /// Validates all identifiers in this configuration.
    pub fn validate(&self) -> Result<()> {
        validate_folder_id(&self.id)?;
        validate_member_id(&self.local_member)?;
        for member in &self.members {
            validate_member_id(member)?;
        }
        for member in &self.former_members {
            validate_member_id(member)?;
        }
        if let Some(inc) = self.initial_incarnation
            && inc == 0
        {
            return Err(SyncError::InvalidConfig {
                message: "initial_incarnation must be non-zero".to_string(),
                hint: Some("Omit initial_incarnation to generate a random ID.".to_string()),
            });
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
    /// Current random non-zero incarnation of this member's index.
    pub incarnation: u64,
    /// Current active member set for this folder.
    pub members: BTreeSet<MemberId>,
    /// Members that previously belonged to this folder (validated names retained so historical
    /// version vectors and conflict labels remain verifiable without trusting arbitrary peer text).
    #[serde(default)]
    pub former_members: BTreeSet<MemberId>,
    /// Peers recently added or re-added whose pre-existing files should be checked against deleted
    /// history on first sync.
    #[serde(default)]
    pub rejoining_peers: BTreeSet<MemberId>,
    /// Strictly monotonic per-incarnation sequence number assigned to every index record update.
    pub local_seq: u64,
    /// Strictly monotonic per-incarnation logical clock used for version vector counters.
    pub local_clock: u64,
    /// Whether syncing is currently paused on this member.
    pub paused: bool,
    /// Active mass-deletion brake state awaiting user confirmation, if tripped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_brake: Option<DeletionBrake>,
    /// When `true`, files discovered by the initial scan on disk (join, reinstall, or rollback)
    /// are tagged `from_join = true` so stale copies are binned rather than causing conflicts or
    /// resurrecting deletions.
    #[serde(default)]
    pub join_mode: bool,
    /// Fingerprint of the latest record written at `local_seq` (for same-seq clone detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Rolling window of recent `(seq -> record_fingerprint)` entries (up to 64) used to detect
    /// rollback/clones even after local sequence numbers advance past what a peer acknowledged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recent_seq_fingerprints: BTreeMap<u64, Hash>,
    /// Directories deleted remotely that were intentionally kept on disk only because they still
    /// hold ignored or held-back local files, persisted across tombstone GC and restarts so scans
    /// never revive them unless synced files are later added inside.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub kept_deleted_dirs: BTreeSet<String>,
}

impl FolderMeta {
    /// Creates initial folder metadata from a [`FolderConfig`] and `incarnation`.
    pub fn new(config: &FolderConfig, incarnation: u64) -> Self {
        let mut former = config.former_members.clone();
        for m in &config.members {
            former.remove(m);
        }
        Self {
            folder_id: config.id.clone(),
            local_member: config.local_member.clone(),
            incarnation,
            members: config.members.clone(),
            former_members: former,
            rejoining_peers: BTreeSet::new(),
            local_seq: 0,
            local_clock: 0,
            paused: false,
            pending_brake: None,
            join_mode: true,
            head_fingerprint: None,
            recent_seq_fingerprints: BTreeMap::new(),
            kept_deleted_dirs: BTreeSet::new(),
        }
    }

    /// Records the fingerprint of the record assigned `seq`, keeping the newest 64 entries.
    pub fn record_seq_fingerprint(&mut self, seq: u64, fingerprint: Hash) {
        self.head_fingerprint = Some(fingerprint);
        self.recent_seq_fingerprints.insert(seq, fingerprint);
        while self.recent_seq_fingerprints.len() > 64 {
            if let Some(oldest) = self.recent_seq_fingerprints.keys().next().copied() {
                self.recent_seq_fingerprints.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Returns this member's active `(local_member, incarnation)` [`VersionKey`].
    pub fn version_key(&self) -> VersionKey {
        VersionKey::new(self.local_member.clone(), self.incarnation)
    }

    /// Allocates and returns the next sequence number within this incarnation.
    pub fn next_seq(&mut self) -> u64 {
        self.local_seq = self.local_seq.saturating_add(1);
        self.local_seq
    }

    /// Allocates and returns the next local logical clock value within this incarnation.
    pub fn next_clock(&mut self) -> u64 {
        self.local_clock = self.local_clock.saturating_add(1);
        self.local_clock
    }

    /// Returns `true` if `member` is a current or known former member of this folder.
    pub fn is_known_member(&self, member: &str) -> bool {
        self.members.contains(member) || self.former_members.contains(member)
    }

    /// Resolves `candidate` to the canonical member identifier stored in this folder's own member
    /// lists, never returning unvalidated peer text.
    pub fn canonical_member_name(&self, candidate: &str) -> Option<&str> {
        if let Some(m) = self.members.get(candidate) {
            return Some(m.as_str());
        }
        self.former_members.get(candidate).map(String::as_str)
    }
}

/// Direct synchronization cursor and acknowledgement state for one peer member.
///
/// Only direct knowledge received straight from `peer` is stored here; third-party claims about
/// other members are never accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerCursor {
    /// Incarnation ID last observed for this peer (`0` before first contact).
    pub incarnation: u64,
    /// Highest contiguous sequence number processed from `(peer, incarnation)`.
    pub seq: u64,
    /// Our own incarnation that `peer` directly reported having pulled up to `acked_local_seq`.
    pub acked_local_incarnation: u64,
    /// Highest `local_seq` of our own `acked_local_incarnation` that `peer` directly confirmed.
    pub acked_local_seq: u64,
    /// Distinct earlier incarnations previously seen for this peer (bounded); seeing an earlier
    /// incarnation return after switching away flags a duplicate member ID (cloned machine).
    #[serde(default)]
    pub prior_incarnations: Vec<u64>,
    /// Fingerprint of the peer's latest record at `(incarnation, seq)` for same-seq clone
    /// detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Set when a duplicate member ID or same-seq divergence was detected for this peer.
    #[serde(default)]
    pub duplicate_detected: bool,
    /// Set permanently when two distinct live incarnations alternated under this member ID.
    #[serde(default)]
    pub alternation_detected: bool,
}

/// Handshake summary returned by a peer before pulling records or files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHandshake {
    /// Sync folder identifier served by the peer.
    pub folder_id: FolderId,
    /// Member identifier of the peer.
    pub member_id: MemberId,
    /// Peer's active incarnation identifier.
    pub incarnation: u64,
    /// Peer's current `local_seq` in `incarnation`.
    pub snapshot_seq: u64,
    /// Sync protocol version supported by the peer.
    pub protocol_version: u32,
}

/// Per-fetch transfer options supporting ranged resume, timeouts, and cooperative cancellation.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Byte offset to start or resume reading from (`0` for the start of the file).
    pub offset: u64,
    /// Maximum wall-clock duration allowed for this transfer.
    pub timeout: Duration,
    /// Optional cancellation flag checked during streaming reads.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            timeout: DEFAULT_FETCH_TIMEOUT,
            cancel: None,
        }
    }
}

impl FetchOptions {
    /// Creates options starting at `offset` with default timeout and optional cancellation flag.
    pub fn at_offset(offset: u64, timeout: Duration, cancel: Option<Arc<AtomicBool>>) -> Self {
        Self {
            offset,
            timeout,
            cancel,
        }
    }

    /// Returns `true` if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
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
    #[serde(skip)]
    pub disk_mtime: Option<Timestamp>,
    /// Local-only change fingerprint (inode/ctime on Unix, NTFS change time + file ID on Windows).
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
    /// Causal version vector for this path keyed by `(member, incarnation)`.
    pub version: VersionVector,
    /// Member that authored the latest modification or deletion of this version.
    pub modified_by: MemberId,
    /// Monotonic local sequence number assigned by the member serving this record.
    pub seq: u64,
    /// Short ancestry of the most recent previous content hashes for this path (up to
    /// [`MAX_ANCESTRY_HASHES`]). For deletion records, `recent_hashes[0]` is the hash of the file
    /// that was deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_hashes: Vec<Hash>,
    /// `true` when this live record came from pre-existing files discovered on disk during join,
    /// rollback, or reinstall before reconciling with peers.
    #[serde(default, skip_serializing_if = "is_false")]
    pub from_join: bool,
    /// Locally derived original path when this live file is a conflict copy.
    ///
    /// Never accepted from a peer on the wire; always derived locally from the canonical conflict
    /// filename syntax and local conflict state.
    #[serde(skip)]
    pub conflict_of: Option<String>,
    /// Local-only flag set when a person chooses [`ConflictChoice::KeepBoth`] so the file is no
    /// longer listed as an open conflict locally.
    #[serde(skip)]
    pub conflict_dismissed: bool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl IndexRecord {
    /// Returns `true` when `self` and `other` represent the exact same filesystem content.
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

    /// Returns a copy suitable for sending over the wire (stripping all local-only fields,
    /// including `conflict_of` which receivers derive locally).
    pub fn for_wire(&self) -> Self {
        let mut wire = self.clone();
        wire.disk_mtime = None;
        wire.disk_change_id = None;
        wire.conflict_of = None;
        wire.conflict_dismissed = false;
        wire
    }

    /// Prepends `hash` to [`Self::recent_hashes`], deduplicating and truncating to
    /// [`MAX_ANCESTRY_HASHES`].
    pub fn push_ancestry_hash(&mut self, hash: Hash) {
        if self.hash == Some(hash) && !self.deleted {
            return;
        }
        self.recent_hashes.retain(|h| *h != hash);
        self.recent_hashes.insert(0, hash);
        self.recent_hashes.truncate(MAX_ANCESTRY_HASHES);
    }

    /// Merges ancestry hashes from `other` into `self.recent_hashes`, capped to
    /// [`MAX_ANCESTRY_HASHES`].
    pub fn merge_ancestry_from(&mut self, other: &Self) {
        let mut combined = Vec::with_capacity(MAX_ANCESTRY_HASHES);
        for &h in &self.recent_hashes {
            if (self.deleted || self.hash != Some(h)) && !combined.contains(&h) {
                combined.push(h);
            }
        }
        for &h in &other.recent_hashes {
            if combined.len() >= MAX_ANCESTRY_HASHES {
                break;
            }
            if (self.deleted || self.hash != Some(h)) && !combined.contains(&h) {
                combined.push(h);
            }
        }
        combined.truncate(MAX_ANCESTRY_HASHES);
        self.recent_hashes = combined;
    }

    /// Computes a compact digest of this record's identity for same-seq clone detection.
    pub fn head_fingerprint(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&self.seq.to_le_bytes());
        hasher.update(&[u8::from(self.deleted)]);
        if let Some(h) = self.hash {
            hasher.update(h.as_bytes());
        }
        if let Some(ref t) = self.link_target {
            hasher.update(t.as_bytes());
        }
        hasher.update(self.modified_by.as_bytes());
        Hash::from_bytes(*hasher.finalize().as_bytes())
    }
}

/// Page of records returned by [`crate::Peer::records_since`] from one consistent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordsPage {
    /// Active incarnation ID of the serving peer at this snapshot.
    pub incarnation: u64,
    /// Records with `seq > since_seq`, ordered by ascending `seq`.
    pub records: Vec<IndexRecord>,
    /// Peer's `local_seq` at the consistent snapshot where this page was read.
    pub snapshot_seq: u64,
    /// `true` if more records exist beyond this page in the snapshot.
    pub has_more: bool,
    /// Direct acknowledgement ONLY: the `(incarnation, seq)` of the *caller* that this peer itself
    /// has directly pulled and applied. Never carries third-party claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_acked: Option<(u64, u64)>,
    /// Fingerprint observed by this peer at the caller's acknowledged `(incarnation, seq)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_acked_fingerprint: Option<Hash>,
    /// Fingerprint of the serving peer's record at `snapshot_seq` (for same-seq clone detection).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
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
    /// Stale pre-existing file moved to bin during join/rollback reconciliation.
    StaleOnJoin,
    /// Operating-system litter file moved to bin when removing a deleted directory.
    OsLitterInDeletedDir,
}

/// Self-describing metadata for one item stored in `.legion-sync/bin`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinEntry {
    /// Unique random identifier for this bin entry.
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
    /// Move the existing indexed live entry into `.legion-sync/bin` first, then restore.
    /// Still refuses if the file on disk has changed since the last scan.
    Replace,
    /// Restore alongside the existing file under `name (restored <date>).ext`.
    Rename,
}

/// Classification of a sync problem reported in [`FolderStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// Two live paths differ only in letter case (or Unicode normalization) on this filesystem.
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
    /// An entry or directory could not be read or written because of permissions or I/O errors.
    FilesystemError,
    /// Folder identity marker is missing or does not match the index (e.g. unmounted drive).
    FolderMarkerMissing,
    /// A scan saw a large fraction of the folder disappear and tripped the mass-deletion brake.
    DeletionBrakeTripped,
    /// Two live machines presented alternating incarnations under the same member ID.
    DuplicateMemberId,
    /// A peer sent a malformed or unauthorized record that was skipped.
    InvalidPeerRecord,
    /// A directory deleted elsewhere was kept locally because it still holds ignored or held-back
    /// files.
    DirectoryKeptLocalFiles,
    /// The held-back table reached [`MAX_HELD_BACK_ENTRIES`].
    HeldBackLimitReached,
    /// A record exceeded [`MAX_DEFERRAL_ATTEMPTS`] consecutive deferrals.
    DeferredLimitExceeded,
}

/// Human-readable problem reported in [`FolderStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncProblem {
    /// Relative path affected by the problem (empty string for folder-wide problems).
    pub path: String,
    /// Machine-readable problem classification.
    pub kind: ProblemKind,
    /// Peer involved, if applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<MemberId>,
    /// Plain-language sentence explaining the problem.
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

/// An item temporarily deferred during sync with an explicit reason and retry count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredItem {
    /// Peer that sent the deferred record.
    pub peer: MemberId,
    /// Relative path of the deferred record.
    pub path: String,
    /// Plain-language reason why it was deferred.
    pub reason: String,
    /// Number of consecutive pull attempts that deferred this item.
    pub attempts: u32,
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
    /// Keep both files as ordinary files and clear the conflict flag locally.
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
    pub members: Vec<MemberStatus>,
    /// Files that currently have two versions awaiting resolution.
    pub conflicts: Vec<ConflictInfo>,
    /// Contents of `.legion-sync/bin`.
    pub bin: Vec<BinEntry>,
    /// Active mass-deletion brake awaiting a person's confirmation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_brake: Option<DeletionBrake>,
    /// Temporarily deferred items with their reasons and retry counts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<DeferredItem>,
}

/// Snapshot of what was expected on disk at the target path immediately before an apply began,
/// used by crash recovery to distinguish a pre-mutation crash from a mid-mutation crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExpectedLocalSnapshot {
    /// Path did not exist on disk before apply.
    Missing,
    /// Path held a regular file with these verified properties.
    File {
        /// Expected byte length.
        size: u64,
        /// Expected disk modification time.
        mtime: Timestamp,
        /// Expected BLAKE3 digest.
        hash: Hash,
    },
    /// Path held a directory.
    Dir,
    /// Path held a symbolic link with this target.
    Symlink {
        /// Expected link target string.
        link_target: String,
    },
}

/// Durable write-ahead intent persisted in the index store immediately before mutating the live
/// tree, so restarting or recovering after a crash or error at any step converges without loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum PendingOp {
    /// Replacing or creating a single live path (moving any existing entry to `.legion-sync/bin`).
    Place {
        /// Record to commit once the live path is materialized.
        target_record: IndexRecord,
        /// Staged filename inside `.legion-sync/tmp/` for regular files.
        staged_tmp: Option<String>,
        /// Optional temporary claim filename inside `.legion-sync/tmp/` used during swap/claim.
        claim_tmp: Option<String>,
        /// Bin metadata for the displaced prior live entry, if any.
        bin_entry: Option<BinEntry>,
        /// Expected disk state at `target_record.path` prior to mutation.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Deleting a live path into `.legion-sync/bin` and recording a tombstone.
    Delete {
        /// Tombstone record to commit once the live entry is moved to the bin.
        target_record: IndexRecord,
        /// Optional temporary claim filename inside `.legion-sync/tmp/`.
        claim_tmp: Option<String>,
        /// Bin metadata for the deleted entry, if it was a file or symlink.
        bin_entry: Option<BinEntry>,
        /// Expected disk state at `target_record.path` prior to deletion.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Local rename optimization: move an existing live file at `from_record.path` to
    /// `to_record.path` (same hash, no download).
    LocalRename {
        /// Tombstone for the old path.
        from_record: IndexRecord,
        /// Live record for the new path.
        to_record: IndexRecord,
        /// Expected disk state at `from_record.path` prior to rename.
        expected_from: ExpectedLocalSnapshot,
    },
    /// Concurrent conflict where the local version loses and moves to `loser_record.path` while
    /// the remote version wins `winner_record.path`.
    ConflictLocalLoses {
        /// Record for the original (winning) path.
        winner_record: IndexRecord,
        /// Independent record for the conflict copy (losing) path.
        loser_record: IndexRecord,
        /// Staged filename inside `.legion-sync/tmp/` for the winning remote file, if a file.
        winner_tmp: Option<String>,
        /// `true` if `loser_record.path` already held identical bytes before this op.
        conflict_already_present: bool,
        /// Expected disk state at `winner_record.path` prior to mutation.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Concurrent conflict where the local version wins `winner_record.path` and the remote
    /// version is placed at `loser_record.path`.
    ConflictRemoteLoses {
        /// Updated record for the original (winning) path.
        winner_record: IndexRecord,
        /// Independent record for the conflict copy (losing) path.
        loser_record: IndexRecord,
        /// Staged filename inside `.legion-sync/tmp/` for the losing remote file, if a file.
        loser_tmp: Option<String>,
        /// `true` if `loser_record.path` already held identical bytes before this op.
        conflict_already_present: bool,
        /// Expected disk state at `winner_record.path` prior to mutation.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Crash-safe restore of a bin entry into the live tree.
    RestoreFromBin {
        /// Restored live record to commit.
        target_record: IndexRecord,
        /// Bin entry ID being restored (to remove from bin on completion).
        restored_bin_id: String,
        /// Storage filename of the restored bin entry (to remove on completion).
        restored_storage_name: String,
        /// Staged filename inside `.legion-sync/tmp/` copied from bin.
        staged_tmp: Option<String>,
        /// Optional temporary claim filename for a displaced live file under `Replace`.
        claim_tmp: Option<String>,
        /// Bin metadata for a displaced live file under `Replace`, if any.
        replacement_bin: Option<BinEntry>,
        /// Expected disk state at `target_record.path` prior to restore.
        expected_local: ExpectedLocalSnapshot,
    },
    /// Crash-safe conflict resolution (`KeepConflict`: promote conflict copy to original path).
    ResolveKeepConflict {
        /// Promoted live record at original path.
        promoted_record: IndexRecord,
        /// Tombstone record for the old conflict copy path.
        deleted_conflict_record: IndexRecord,
        /// Optional temporary claim filename for the displaced original file.
        claim_tmp: Option<String>,
        /// Bin metadata for the displaced original file, if any.
        bin_entry: Option<BinEntry>,
        /// Expected disk state at `promoted_record.path` prior to mutation.
        expected_orig: ExpectedLocalSnapshot,
        /// Expected disk state at `deleted_conflict_record.path` prior to mutation.
        expected_conflict: ExpectedLocalSnapshot,
    },
}

/// Active sync progress tracked for a peer during `pull_from_peer`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncProgress {
    /// Files remaining to pull/apply.
    pub files_left: u64,
    /// Bytes remaining to transfer.
    pub bytes_left: u64,
}
