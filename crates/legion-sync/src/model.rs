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
/// cleanly with [`SyncError::SchemaTooNew`]; opening an index with an older version is refused as
/// damaged/incompatible ([`SyncError::IndexDamaged`]) so it is rebuilt rather than misread.
pub const INDEX_SCHEMA_VERSION: u32 = 4;

/// Sync wire/trait protocol version advertised in [`PeerHandshake`] and [`HandshakeHello`].
pub const SYNC_PROTOCOL_VERSION: u32 = 3;

/// Maximum version vector counter value accepted for any member key (`2^62`).
pub const MAX_VERSION_COUNTER: u64 = 1u64 << 62;

/// Minimum interval (in seconds) between peer-forced incarnation rotations per peer (1 day).
pub const FORCED_ROTATION_COOLDOWN_SECS: u64 = 86_400;

/// Rolling window (in seconds) over which total peer-forced incarnation rotations across all peers
/// are bounded (7 days).
pub const FORCED_ROTATION_WEEK_WINDOW_SECS: u64 = 7 * 86_400;

/// Maximum number of peer-forced incarnation rotations accepted in total across all peers within
/// [`FORCED_ROTATION_WEEK_WINDOW_SECS`].
pub const MAX_FORCED_ROTATIONS_PER_WEEK: usize = 3;

/// Default retention duration for files moved to `.legion-sync/bin` (30 days).
pub const DEFAULT_BIN_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Default idle fetch timeout (no bytes received) when pulling a file from a peer.
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum number of recent content hashes retained in [`IndexRecord::recent_hashes`].
pub const MAX_ANCESTRY_HASHES: usize = 16;

/// Maximum number of records accepted in one [`RecordsPage`].
pub const MAX_PAGE_RECORDS: usize = 4_096;

/// Maximum number of distinct incarnations per member accepted in one incoming [`VersionVector`].
pub const MAX_VV_INCARNATIONS_PER_MEMBER: usize = 64;

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

/// Default minimum count of deleted files over the rolling brake window required to trip the
/// mass-deletion brake.
pub const DEFAULT_BRAKE_MIN_FILES: usize = 50;

/// Default minimum percentage (1..=100) of live files deleted over the rolling brake window
/// required to trip the mass-deletion brake (more than a quarter).
pub const DEFAULT_BRAKE_MIN_PERCENT: u8 = 25;

/// Default settle period (in seconds) that unmatched local deletions wait before peers see them
/// so cross-scan moves can be recognized and mass-deletion storms cannot leak before the brake
/// trips (30 seconds).
pub const DEFAULT_BRAKE_SETTLE_SECS: u64 = 30;

/// Default rolling window (in seconds) over which pending and recently emitted deletions are
/// counted against the live file count at the window's start (10 minutes).
pub const DEFAULT_BRAKE_WINDOW_SECS: u64 = 600;

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

fn default_brake_settle_secs() -> u64 {
    DEFAULT_BRAKE_SETTLE_SECS
}

fn default_brake_window_secs() -> u64 {
    DEFAULT_BRAKE_WINDOW_SECS
}

/// Threshold configuration for the move-aware, windowed outbound and inbound mass-deletion brakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionBrakeConfig {
    /// Minimum number of live files that must vanish in the rolling window to trip the brake
    /// (default 50). Setting this to `usize::MAX` disables both outbound and inbound brakes.
    pub min_deleted_files: usize,
    /// Minimum percentage (`1..=100`) of live files at the window's start that must vanish
    /// together with `min_deleted_files` to trip the brake (default 25, i.e. more than a quarter).
    pub min_deleted_percent: u8,
    /// Settle delay in seconds before unmatched local deletions become visible to peers so
    /// cross-scan moves are recognized and split deletion storms are caught before leaking
    /// (default 30 seconds).
    #[serde(default = "default_brake_settle_secs")]
    pub settle_secs: u64,
    /// Rolling window duration in seconds over which pending and recent deletions are counted
    /// against the window-start live file count (default 600 seconds / 10 minutes).
    #[serde(default = "default_brake_window_secs")]
    pub window_secs: u64,
}

impl Default for DeletionBrakeConfig {
    fn default() -> Self {
        Self {
            min_deleted_files: DEFAULT_BRAKE_MIN_FILES,
            min_deleted_percent: DEFAULT_BRAKE_MIN_PERCENT,
            settle_secs: DEFAULT_BRAKE_SETTLE_SECS,
            window_secs: DEFAULT_BRAKE_WINDOW_SECS,
        }
    }
}

impl DeletionBrakeConfig {
    /// Returns a configuration with both outbound and inbound mass-deletion brakes disabled.
    pub fn disabled() -> Self {
        Self {
            min_deleted_files: usize::MAX,
            min_deleted_percent: 100,
            settle_secs: 0,
            window_secs: 0,
        }
    }

    /// Returns `true` if the mass-deletion brake is enabled.
    pub fn is_enabled(&self) -> bool {
        self.min_deleted_files != usize::MAX
    }

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

/// Persisted state describing a tripped outbound mass-deletion brake awaiting a person's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionBrake {
    /// Number of live files that disappeared in the rolling window that tripped the brake.
    pub deleted_count: usize,
    /// Number of live files at the start of the rolling window.
    pub total_live_files: usize,
    /// Up to 10 sample missing paths shown to the person.
    pub sample_paths: Vec<String>,
}

/// A person's response to a tripped outbound mass-deletion brake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionBrakeDecision {
    /// "They are really deleted": unpause and proceed to record and propagate the deletions.
    ConfirmDeleted,
    /// "They are not": unpause and rescan with the brake still armed.
    Rescan,
}

/// A local deletion held pending during the move-awareness settle period before peers see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLocalDeletion {
    /// Prior indexed live record that vanished from disk.
    pub old_record: IndexRecord,
    /// Timestamp when the disappearance was first observed.
    pub detected_at: Timestamp,
}

/// Rolling deletion-window accounting persisted in [`FolderMeta`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletionWindowState {
    /// Start timestamp of the current rolling brake window.
    pub window_start: Timestamp,
    /// Number of live files indexed when the rolling window began.
    pub window_start_live_files: usize,
    /// Distinct file deletions already emitted during the current rolling window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emitted_deletions: Vec<(String, Timestamp)>,
    /// Recent `(path, hash, timestamp)` of regular files newly created or modified within the
    /// settle period (used to recognize cross-scan moves even when the destination is scanned
    /// before the source; a deletion only matches a *different* path with the same hash).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_created_hashes: Vec<(String, Hash, Timestamp)>,
}

/// Persisted state describing a tripped inbound mass-deletion brake for one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundDeletionBrake {
    /// Peer whose changes would delete more than the threshold of live files.
    pub peer: MemberId,
    /// Number of local live files that would be deleted in the rolling inbound window.
    pub deleted_count: usize,
    /// Number of live files at the start of the rolling inbound window.
    pub total_live_files: usize,
    /// Highest peer sequence number among the blocked deletion records.
    pub max_blocked_seq: u64,
    /// Up to 10 sample paths that would be deleted.
    pub sample_paths: Vec<String>,
    /// All candidate live file paths blocked by this inbound brake.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_paths: Vec<String>,
    /// Peer deletion tombstones held back so choosing `KeepLocalFiles` can re-assert local files
    /// strictly newer than every blocked tombstone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_tombstones: Vec<IndexRecord>,
}

/// A person's response to a tripped inbound peer mass-deletion brake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundDeletionBrakeDecision {
    /// Allow and apply the peer's blocked deletions on the next pull.
    ApplyDeletions,
    /// Keep this machine's local files and re-assert them as newer than the peer's deletions.
    KeepLocalFiles,
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
    /// Chained fingerprint at `local_seq` (commits to the full sequence history `1..=local_seq`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Last timestamp at which each peer forced an incarnation rotation (rate-limited to at most
    /// once per day per peer).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_forced_rotation_by_peer: BTreeMap<MemberId, Timestamp>,
    /// Bounded history of `(peer, timestamp)` peer-forced incarnation rotations within the last
    /// week (enforcing at most once per day per peer and three times per week in total).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forced_rotation_history: Vec<(MemberId, Timestamp)>,
    /// Directories deleted remotely that were intentionally kept on disk only because they still
    /// hold ignored or held-back local files, persisted across tombstone GC and restarts so scans
    /// never revive them unless synced files are later added inside.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub kept_deleted_dirs: BTreeSet<String>,
    /// Live paths that raced with a person's save or move during placement or crash recovery and
    /// must be hashed on the next scan regardless of cached stat metadata.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub needs_rescan: BTreeSet<String>,
    /// Local deletions waiting through the settle period before peers see them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pending_deletions: BTreeMap<String, PendingLocalDeletion>,
    /// Rolling window state for the outbound move-aware deletion brake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_window: Option<DeletionWindowState>,
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
            last_forced_rotation_by_peer: BTreeMap::new(),
            forced_rotation_history: Vec::new(),
            kept_deleted_dirs: BTreeSet::new(),
            needs_rescan: BTreeSet::new(),
            pending_deletions: BTreeMap::new(),
            deletion_window: None,
        }
    }

    /// Computes and advances the chained digest for sequence number `seq` with `record_fp`,
    /// updating [`Self::head_fingerprint`] and returning the new chained digest to persist in the
    /// index's sequence fingerprint table.
    pub fn advance_chain_fingerprint(&mut self, seq: u64, record_fp: Hash) -> Hash {
        let chain = chain_fingerprint(self.head_fingerprint, seq, record_fp);
        self.head_fingerprint = Some(chain);
        chain
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

    /// Advances `version` for this member's active `(local_member, incarnation)` key to strictly
    /// exceed both its previous value and `self.local_clock`, updating `self.local_clock` to match
    /// the issued counter so `self.local_clock` is always `>=` every counter issued locally.
    pub fn bump_version(&mut self, version: &mut VersionVector) -> u64 {
        let vkey = self.version_key();
        let issued = version.bump_above(vkey, self.local_clock);
        if issued > self.local_clock {
            self.local_clock = issued;
        }
        issued
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

/// Computes the chained sequence fingerprint for `seq`, binding `prev_chain`, `seq`, and
/// `record_fp` so any divergence at any earlier sequence number changes every subsequent digest.
pub fn chain_fingerprint(prev_chain: Option<Hash>, seq: u64, record_fp: Hash) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"legion-sync-chain-v1");
    if let Some(prev) = prev_chain {
        hasher.update(prev.as_bytes());
    } else {
        hasher.update(&[0u8; 32]);
    }
    hasher.update(&seq.to_le_bytes());
    hasher.update(record_fp.as_bytes());
    Hash::from_bytes(*hasher.finalize().as_bytes())
}

/// Direct view of what one member has pulled from or applied/acknowledged from another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAckView {
    /// Observed peer incarnation identifier.
    pub incarnation: u64,
    /// Sequence number within `incarnation`.
    pub seq: u64,
    /// Chained fingerprint observed at `(incarnation, seq)` (`None` when `seq == 0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<Hash>,
}

/// Direct synchronization cursor and acknowledgement state for one peer member.
///
/// Keeps two separate positions:
/// - [`Self::seq`] is the **pull position** (may advance past records stored durably as unapplied).
/// - [`Self::applied_ack_seq`] is the **acknowledgement** reported back to the peer, which never
///   advances past the oldest unapplied record from `(peer, incarnation)`.
///
/// Only direct knowledge received straight from `peer` is stored here; third-party claims about
/// other members are never accepted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerCursor {
    /// Incarnation ID last observed for this peer (`0` before first contact).
    pub incarnation: u64,
    /// Highest contiguous sequence number pulled from `(peer, incarnation)`.
    pub seq: u64,
    /// Highest sequence number in `(peer, incarnation)` that has been fully applied (never past
    /// the oldest unapplied held-back or deferred record from this peer/incarnation).
    #[serde(default)]
    pub applied_ack_seq: u64,
    /// Chained fingerprint observed at `(incarnation, applied_ack_seq)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_fingerprint: Option<Hash>,
    /// Our own incarnation that `peer` directly reported having applied up to `acked_local_seq`.
    pub acked_local_incarnation: u64,
    /// Highest `local_seq` of our own `acked_local_incarnation` that `peer` directly confirmed
    /// applying.
    pub acked_local_seq: u64,
    /// Distinct earlier incarnations previously seen for this peer (bounded); seeing an earlier
    /// incarnation return after switching away flags a duplicate member ID (cloned machine).
    #[serde(default)]
    pub prior_incarnations: Vec<u64>,
    /// Chained fingerprint of the peer's history at `(incarnation, seq)` (the pull position).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Set when a duplicate member ID or rollback/divergence was detected for this peer.
    #[serde(default)]
    pub duplicate_detected: bool,
    /// Set permanently when two distinct live incarnations alternated under this member ID.
    #[serde(default)]
    pub alternation_detected: bool,
    /// Start timestamp of this peer's rolling inbound deletion brake window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound_window_start: Option<Timestamp>,
    /// Number of local live files at the start of this peer's rolling inbound deletion window.
    #[serde(default)]
    pub inbound_window_start_live_files: usize,
    /// Local regular files binned due to this peer's deletions inside the rolling inbound window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbound_binned_in_window: Vec<(String, Timestamp)>,
    /// Active inbound mass-deletion brake pausing pulls from this peer until resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbound_brake: Option<InboundDeletionBrake>,
    /// Peer sequence number up to which inbound mass deletions were explicitly confirmed.
    #[serde(default)]
    pub allow_inbound_mass_deletion_until_seq: u64,
    /// Peer sequence number up to which the user chose `KeepLocalFiles` on an inbound mass
    /// deletion brake, re-asserting local live files across all pending pages of that batch.
    #[serde(default)]
    pub keep_local_files_until_seq: u64,
}

/// Compact summary carried in handshakes informing the remote peer that this member could not
/// accept one or more of its records (so both sides show the problem and neither shows "up to
/// date" while tombstones remain pinned).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRefusalSummary {
    /// Number of unapplied held-back records currently refused from that peer.
    pub count: usize,
    /// Sample path of one refused record.
    pub sample_path: String,
    /// Plain-language reason why the sample record was refused.
    pub reason: String,
}

/// Caller-side handshake message sent to a peer before pulling any page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandshakeHello {
    /// Sync folder identifier.
    pub folder_id: FolderId,
    /// Caller's member identifier.
    pub member_id: MemberId,
    /// Caller's active incarnation identifier.
    pub incarnation: u64,
    /// Caller's current `local_seq` in `incarnation`.
    pub snapshot_seq: u64,
    /// Caller's chained fingerprint at `(incarnation, snapshot_seq)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Sync protocol version supported by the caller.
    pub protocol_version: u32,
    /// Highest contiguous position the caller has pulled from the callee (`(inc, seq, chain_fp)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_pulled: Option<PeerAckView>,
    /// Highest fully-applied position the caller acknowledges to the callee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_acked: Option<PeerAckView>,
    /// `true` when the caller intentionally reset its cursor for the callee (because the caller
    /// removed and re-added the callee), so `peer_pulled == None` is expected rather than a
    /// rollback.
    #[serde(default, skip_serializing_if = "is_false")]
    pub peer_rejoining: bool,
    /// Short summary of records the caller could not accept from the callee, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_from_peer: Option<PeerRefusalSummary>,
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
    /// Peer's chained fingerprint at `(incarnation, snapshot_seq)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Sync protocol version supported by the peer.
    pub protocol_version: u32,
    /// Highest contiguous position the peer has pulled from the caller (`(inc, seq, chain_fp)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pulled: Option<PeerAckView>,
    /// Highest fully-applied position the peer acknowledges to the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_acked: Option<PeerAckView>,
    /// Result of verifying `HandshakeHello::peer_pulled` against the peer's own sequence
    /// fingerprint history (`Some(false)` means the peer's index went back in time or diverged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_pull_matches: Option<bool>,
    /// `true` when the caller claimed a pull/ack position for this peer's current incarnation that
    /// is strictly behind what the caller previously directly acknowledged (meaning the caller
    /// rolled back in time).
    #[serde(default)]
    pub caller_regressed: bool,
    /// `true` when the callee has `caller` in its `rejoining_peers` list (removed and re-added),
    /// so `caller_pulled == None` and `caller_acked == None` reflect the fresh re-add rather than
    /// a rollback.
    #[serde(default, skip_serializing_if = "is_false")]
    pub caller_rejoining: bool,
    /// Short summary of records the callee could not accept from the caller, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused_from_caller: Option<PeerRefusalSummary>,
}

/// Monotonic elapsed-time clock for [`FetchOptions`] idle-timeout enforcement.
///
/// Production callers leave [`FetchOptions::clock`] unset (`None`), which uses real wall time
/// (`std::time::Instant`). Deterministic tests and the simulator can inject a virtual clock so
/// idle-timeout rules are verified without sleeping.
#[derive(Clone)]
pub struct FetchClock(Arc<dyn Fn() -> Duration + Send + Sync>);

impl FetchClock {
    /// Creates a monotonic clock backed by a closure returning elapsed time since an arbitrary
    /// epoch.
    pub fn new(now: impl Fn() -> Duration + Send + Sync + 'static) -> Self {
        Self(Arc::new(now))
    }

    /// Reads the current monotonic timestamp from this clock.
    pub fn now(&self) -> Duration {
        (self.0)()
    }
}

impl std::fmt::Debug for FetchClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FetchClock(..)")
    }
}

/// Per-fetch transfer options supporting ranged resume, idle timeouts, and cooperative cancellation.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Byte offset to start or resume reading from (`0` for the start of the file).
    pub offset: u64,
    /// Maximum idle duration allowed with no bytes received during this transfer.
    pub timeout: Duration,
    /// Optional cancellation flag checked during streaming reads.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Optional monotonic clock override for deterministic idle-timeout checks.
    pub clock: Option<FetchClock>,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            offset: 0,
            timeout: DEFAULT_FETCH_TIMEOUT,
            cancel: None,
            clock: None,
        }
    }
}

impl FetchOptions {
    /// Creates options starting at `offset` with `timeout` and optional cancellation flag.
    pub fn at_offset(offset: u64, timeout: Duration, cancel: Option<Arc<AtomicBool>>) -> Self {
        Self {
            offset,
            timeout,
            cancel,
            clock: None,
        }
    }

    /// Attaches a deterministic monotonic clock for idle-timeout checks.
    #[must_use]
    pub fn with_clock(mut self, clock: FetchClock) -> Self {
        self.clock = Some(clock);
        self
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

    /// Computes a compact digest of this record's identity for chained sequence fingerprinting.
    pub fn head_fingerprint(&self) -> Hash {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&self.seq.to_le_bytes());
        hasher.update(&[u8::from(self.deleted)]);
        let kind_byte = match self.kind {
            EntryKind::File => 1u8,
            EntryKind::Dir => 2u8,
            EntryKind::Symlink => 3u8,
        };
        hasher.update(&[kind_byte]);
        hasher.update(&self.size.to_le_bytes());
        hasher.update(&self.mtime.secs.to_le_bytes());
        hasher.update(&self.mtime.nanos.to_le_bytes());
        if let Some(h) = self.hash {
            hasher.update(h.as_bytes());
        }
        if let Some(ref t) = self.link_target {
            hasher.update(t.as_bytes());
        }
        hasher.update(self.modified_by.as_bytes());
        hasher.update(&[0]);
        for (key, counter) in self.version.iter() {
            hasher.update(key.member.as_bytes());
            hasher.update(&[0]);
            hasher.update(&key.incarnation.to_le_bytes());
            hasher.update(&counter.to_le_bytes());
        }
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
    /// Direct acknowledgement ONLY: the `(incarnation, applied_ack_seq)` of the *caller* that this
    /// peer itself has directly pulled and applied. Never advances past unapplied records and never
    /// carries third-party claims.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_acked: Option<(u64, u64)>,
    /// Chained fingerprint observed by this peer at the caller's acknowledged `(incarnation, seq)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_acked_fingerprint: Option<Hash>,
    /// Chained fingerprint of the serving peer's history at `snapshot_seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_fingerprint: Option<Hash>,
    /// Chained fingerprints for sequence numbers referenced by this page (`r.seq`, `r.seq - 1`, and
    /// `snapshot_seq`) so receivers can record exact pull and applied-ack fingerprints.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        with = "u64_key_map"
    )]
    pub chain_fingerprints: BTreeMap<u64, Hash>,
}

/// Serializes and deserializes `BTreeMap<u64, V>` cleanly across both direct JSON and buffered
/// `serde` internally-tagged / untagged enums (where buffered JSON hands map keys back as strings).
mod u64_key_map {
    use std::collections::BTreeMap;
    use std::fmt;

    use serde::de::{Deserialize, Deserializer, Error as DeError, MapAccess, Visitor};
    use serde::ser::{Serialize, SerializeMap, Serializer};

    struct U64Key(u64);

    impl<'de> Deserialize<'de> for U64Key {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KeyVisitor;
            impl Visitor<'_> for KeyVisitor {
                type Value = U64Key;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a u64 integer or decimal u64 string")
                }

                fn visit_u64<E: DeError>(self, v: u64) -> Result<Self::Value, E> {
                    Ok(U64Key(v))
                }

                fn visit_i64<E: DeError>(self, v: i64) -> Result<Self::Value, E> {
                    u64::try_from(v)
                        .map(U64Key)
                        .map_err(|_| E::custom(format!("negative key {v}")))
                }

                fn visit_str<E: DeError>(self, v: &str) -> Result<Self::Value, E> {
                    v.parse::<u64>()
                        .map(U64Key)
                        .map_err(|err| E::custom(format!("invalid u64 key {v:?}: {err}")))
                }
            }
            deserializer.deserialize_any(KeyVisitor)
        }
    }

    pub fn serialize<S: Serializer, V: Serialize>(
        map: &BTreeMap<u64, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let human_readable = serializer.is_human_readable();
        let mut s = serializer.serialize_map(Some(map.len()))?;
        if human_readable {
            for (k, v) in map {
                s.serialize_entry(&k.to_string(), v)?;
            }
        } else {
            for (k, v) in map {
                s.serialize_entry(k, v)?;
            }
        }
        s.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>, V: Deserialize<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<u64, V>, D::Error> {
        struct MapVisitor<V>(std::marker::PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = BTreeMap<u64, V>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map keyed by u64 sequence numbers")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut out = BTreeMap::new();
                while let Some((U64Key(k), v)) = access.next_entry::<U64Key, V>()? {
                    out.insert(k, v);
                }
                Ok(out)
            }
        }

        deserializer.deserialize_map(MapVisitor(std::marker::PhantomData))
    }
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
    /// Disk modification time observed on `.legion-sync/bin/<storage_name>` immediately after
    /// binning (used to cheaply detect late writes through an already-open handle on scan).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binned_mtime: Option<Timestamp>,
    /// Local change fingerprint observed on `.legion-sync/bin/<storage_name>` immediately after
    /// binning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binned_change_id: Option<u64>,
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
    /// Pulling from a peer would delete a large fraction of live files and tripped the inbound
    /// peer deletion brake.
    InboundDeletionBrakeTripped,
    /// Two live machines presented alternating incarnations under the same member ID.
    DuplicateMemberId,
    /// A peer sent a malformed or unauthorized record that was skipped.
    InvalidPeerRecord,
    /// A directory deleted elsewhere was kept locally because it still holds ignored or held-back
    /// files.
    DirectoryKeptLocalFiles,
    /// The held-back table reached [`MAX_HELD_BACK_ENTRIES`] and pulling from the peer stopped.
    HeldBackLimitReached,
    /// A record exceeded [`MAX_DEFERRAL_ATTEMPTS`] consecutive deferrals and is held unapplied.
    DeferredLimitExceeded,
    /// A record mentions a syntactically valid member this machine does not know yet ("not yet").
    UnknownMember,
    /// This machine rotated its incarnation because a peer reported a newer or divergent history.
    PeerForcedRotation,
    /// A peer repeatedly tried to force incarnation rotation beyond the rate limit (once a day per
    /// peer or three times a week in total).
    PeerRollbackRateLimited,
    /// Local sync index file is missing, empty, or damaged; folder is stopped until rebuilt.
    IndexDamaged,
    /// Local volume does not have enough free space to receive a file.
    NoSpace,
    /// A peer reported in its handshake that it could not accept one or more records from this
    /// machine.
    PeerRefusedRecords,
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

/// A peer record that could not be applied yet and is stored durably until the local, membership,
/// or remote condition clears. While present, this member's acknowledgement to `from_peer` never
/// advances past `peer_seq - 1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldBackRecord {
    /// The incoming peer record that was not applied.
    pub record: IndexRecord,
    /// Peer from which the record was received.
    pub from_peer: MemberId,
    /// Specific problem describing why the record was not applied.
    pub problem: SyncProblem,
    /// Peer incarnation in which `peer_seq` was observed.
    #[serde(default)]
    pub peer_incarnation: u64,
    /// Sequence number of this record on `from_peer`.
    #[serde(default)]
    pub peer_seq: u64,
    /// Peer's chained fingerprint at `peer_seq - 1` (if `peer_seq > 1` and available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_chain_fingerprint: Option<Hash>,
}

/// An item temporarily deferred during sync with an explicit reason and retry count, persisted
/// durably until applied or promoted to held-back.
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
    /// Peer incarnation in which `peer_seq` was observed.
    #[serde(default)]
    pub peer_incarnation: u64,
    /// Sequence number of this record on `peer`.
    #[serde(default)]
    pub peer_seq: u64,
    /// Peer's chained fingerprint at `peer_seq - 1` (if `peer_seq > 1` and available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_chain_fingerprint: Option<Hash>,
}

/// One line in the append-only, compacted `.legion-sync/deletions.log` kept inside the folder so
/// wiping the index while keeping the folder never resurrects deleted files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FolderDeletionLogEntry {
    /// A path was deleted and its tombstone (or held deletion inside the settle window) is active.
    Deleted {
        /// Normalized relative path inside the sync folder.
        path: String,
        /// Original entry kind.
        kind: EntryKind,
        /// Recent content hashes of the deleted entry.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        hashes: Vec<Hash>,
        /// Version vector of the deletion tombstone (or prior record when held).
        version: VersionVector,
        /// Deletion timestamp.
        mtime: Timestamp,
        /// Member that authored the deletion.
        modified_by: MemberId,
        /// `true` when this deletion was recorded while still held inside the settle period.
        #[serde(default, skip_serializing_if = "is_false")]
        held: bool,
    },
    /// A previously deleted path became live again.
    Revived {
        /// Normalized relative path inside the sync folder.
        path: String,
    },
}

/// Active deletion entry recovered by replaying `.legion-sync/deletions.log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderDeletionRecord {
    /// Normalized relative path inside the sync folder.
    pub path: String,
    /// Original entry kind.
    pub kind: EntryKind,
    /// Recent content hashes of the deleted entry.
    pub hashes: Vec<Hash>,
    /// Version vector of the deletion tombstone.
    pub version: VersionVector,
    /// Deletion timestamp.
    pub mtime: Timestamp,
    /// Member that authored the deletion.
    pub modified_by: MemberId,
    /// `true` when this entry came from a deletion held inside the settle period.
    pub held: bool,
}

impl FolderDeletionLogEntry {
    /// Builds a `Deleted` log entry from a tombstone [`IndexRecord`].
    pub fn from_tombstone(rec: &IndexRecord) -> Self {
        Self::Deleted {
            path: rec.path.clone(),
            kind: rec.kind,
            hashes: rec.recent_hashes.clone(),
            version: rec.version.clone(),
            mtime: rec.mtime,
            modified_by: rec.modified_by.clone(),
            held: false,
        }
    }

    /// Builds a held `Deleted` log entry from a [`PendingLocalDeletion`] inside the settle window.
    pub fn from_pending_deletion(pd: &PendingLocalDeletion, local_member: &str) -> Self {
        let mut hashes = pd.old_record.recent_hashes.clone();
        if let Some(h) = pd.old_record.hash {
            hashes.retain(|x| *x != h);
            hashes.insert(0, h);
            hashes.truncate(MAX_ANCESTRY_HASHES);
        }
        Self::Deleted {
            path: pd.old_record.path.clone(),
            kind: pd.old_record.kind,
            hashes,
            version: pd.old_record.version.clone(),
            mtime: pd.detected_at,
            modified_by: local_member.to_string(),
            held: true,
        }
    }
}

/// Serializes one or more [`FolderDeletionLogEntry`] items as NDJSON bytes.
pub fn encode_deletion_log_entries(entries: &[FolderDeletionLogEntry]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut out, entry).map_err(|err| SyncError::Storage {
            context: "serialize deletion log entry".to_string(),
            detail: err.to_string(),
        })?;
        out.push(b'\n');
    }
    Ok(out)
}

/// Replays NDJSON bytes from `.legion-sync/deletions.log`, tolerating an incomplete trailing line
/// from a crash, and returns the map of currently active deletions.
pub fn parse_deletion_log(bytes: &[u8]) -> BTreeMap<String, FolderDeletionRecord> {
    let mut active = BTreeMap::new();
    for line in bytes.split(|&b| b == b'\n') {
        let trimmed = trim_ascii_whitespace(line);
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_slice::<FolderDeletionLogEntry>(trimmed) else {
            // Tolerate a truncated trailing line after an abrupt crash.
            continue;
        };
        match entry {
            FolderDeletionLogEntry::Deleted {
                path,
                kind,
                hashes,
                version,
                mtime,
                modified_by,
                held,
            } => {
                if crate::path::validate_sync_path(&path).is_ok() {
                    active.insert(
                        path.clone(),
                        FolderDeletionRecord {
                            path,
                            kind,
                            hashes,
                            version,
                            mtime,
                            modified_by,
                            held,
                        },
                    );
                }
            }
            FolderDeletionLogEntry::Revived { path } => {
                active.remove(&path);
            }
        }
    }
    active
}

fn trim_ascii_whitespace(mut slice: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = slice.split_first() {
        if first.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = slice.split_last() {
        if last.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    slice
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
    /// Active outbound mass-deletion brake awaiting a person's confirmation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_brake: Option<DeletionBrake>,
    /// Active inbound peer mass-deletion brakes pausing individual peers until resolved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inbound_brakes: Vec<InboundDeletionBrake>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_page_chain_fingerprints_roundtrip_inside_tagged_and_untagged_json() {
        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum TaggedEnvelope {
            Records { page: RecordsPage },
        }

        #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(untagged)]
        enum UntaggedEnvelope {
            Page { page: RecordsPage },
        }

        let mut fps = BTreeMap::new();
        fps.insert(1, legion_xfer::hash::hash_bytes(b"seq1"));
        fps.insert(42, legion_xfer::hash::hash_bytes(b"seq42"));
        let page = RecordsPage {
            incarnation: 7,
            records: Vec::new(),
            snapshot_seq: 42,
            has_more: false,
            caller_acked: Some((3, 10)),
            caller_acked_fingerprint: Some(legion_xfer::hash::hash_bytes(b"ack")),
            head_fingerprint: Some(legion_xfer::hash::hash_bytes(b"seq42")),
            chain_fingerprints: fps,
        };

        let tagged_json = serde_json::to_string(&TaggedEnvelope::Records { page: page.clone() })
            .expect("serialize tagged");
        let tagged_back: TaggedEnvelope =
            serde_json::from_str(&tagged_json).expect("deserialize tagged");
        assert_eq!(tagged_back, TaggedEnvelope::Records { page: page.clone() });

        let untagged_json = serde_json::to_string(&UntaggedEnvelope::Page { page: page.clone() })
            .expect("serialize untagged");
        let untagged_back: UntaggedEnvelope =
            serde_json::from_str(&untagged_json).expect("deserialize untagged");
        assert_eq!(untagged_back, UntaggedEnvelope::Page { page });
    }
}
