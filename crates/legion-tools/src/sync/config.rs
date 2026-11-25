//! Shared sync folder configuration (`sync.toml`) and CRDT merge.
//!
//! `sync.toml` lives in Legion's configuration directory beside `fleet.toml` and `policy.toml`
//! and is synchronized across fleet machines by `fleet.sync` (and periodically by `legion sync run`).
//!
//! # Exact merge rule (join-semilattice)
//!
//! Every mutable item carries a stamp `(at, on)` where `at` is a canonical nanosecond-precision
//! RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`) and `on` is the authoring node's
//! name. Stamps are totally ordered by `(timestamp_nanos, on)`.
//!
//! Merging two valid `SyncFile` documents `A` and `B` proceeds entry by entry:
//!
//! 1. **Folder presence (`folders.<id>` vs `removed.<id>`)**:
//!    - Folder deletions (`sync.delete`) write a permanent folder tombstone `removed.<id>` with
//!      stamp `(removed_at, removed_on)`.
//!    - If `id` is present in `removed` on either side, the merged document keeps `removed.<id>`
//!      with the greater tombstone stamp and drops any live `folders.<id>` entry. (`sync.create`
//!      never reuses a tombstoned folder id.)
//! 2. **Live folder fields (when `id` is live and not tombstoned)**:
//!    - `created_at` / `created_on`: the earlier creation stamp (symmetric `min`).
//!    - Per-field metadata (`name`, `ignore`, `paused`, `bin_days`), each stamped with its own
//!      `(at, on)` (defaulting to the folder's `(updated_at, updated_on)` when omitted in hand-edited
//!      TOML): the entry with the greater stamp wins; on an identical stamp, deterministic
//!      lexicographical order breaks ties.
//!    - Folder `updated_at` / `updated_on`: the maximum of the two sides' folder stamps and winning
//!      field stamps.
//! 3. **Per-member state (`members.<machine>` vs `removed_members.<machine>`)**:
//!    - Each machine `m` is merged independently by taking the maximum over the total order
//!      `(stamp, is_tombstone, value_tiebreak)` where `is_tombstone = true > false`:
//!      - Concurrent `sync.add` of different machines on two nodes keeps **both**.
//!      - A member removal (`removed_members.<m>`) beats any older or equal-stamp member add/edit.
//!      - A later re-add (`members.<m>`) with a strictly newer stamp beats an older member removal.
//!      - On equal stamps between two live entries, lexicographical `(path, added_at, added_on)`
//!        breaks ties deterministically.
//!
//! Because every component reduces via `max` (or `min`) on a total order with an absorbing folder
//! tombstone, `merge_sync_files` is strictly **commutative**, **associative**, and **idempotent**
//! across all valid inputs, including arbitrary combinations of additions, edits, member removals,
//! re-adds, and folder deletions.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::Duration;

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::config::{Document, NodeName, parse, write_atomically};
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use legion_sync::{IgnoreMatcher, validate_folder_id, validate_member_id};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Current schema version of `sync.toml`.
pub const SYNC_VERSION: u32 = 1;

/// Default retention period in days for `.legion-sync/bin`.
pub const DEFAULT_BIN_DAYS: u32 = 30;

/// Minimum allowed bin retention in days.
pub const MIN_BIN_DAYS: u32 = 1;

/// Maximum allowed bin retention in days (10 years).
pub const MAX_BIN_DAYS: u32 = 3_650;

/// Maximum serialized size of `sync.toml` (1 MiB, matching `config.write`).
pub const MAX_SYNC_TOML_BYTES: usize = 1024 * 1024;

/// Maximum length in bytes of a folder's human-readable `name`.
pub const MAX_FOLDER_NAME_LEN: usize = 128;

/// Maximum length in bytes of a member's configured path string.
pub const MAX_MEMBER_PATH_LEN: usize = 4096;

/// Maximum number of per-folder custom ignore patterns.
pub const MAX_IGNORE_PATTERNS: usize = 512;

/// Maximum length of a single ignore pattern.
pub const MAX_IGNORE_PATTERN_LEN: usize = 512;

/// Longest wait for `state/sync/config.lock`.
const CONFIG_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Formats `ts` as canonical RFC 3339 UTC with nanosecond precision (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`).
pub fn format_stamp_time(ts: Timestamp) -> String {
    let base = ts.strftime("%Y-%m-%dT%H:%M:%S");
    let nanos = ts.subsec_nanosecond().unsigned_abs();
    format!("{base}.{nanos:09}Z")
}

/// Parses an RFC 3339 UTC timestamp string (accepting both whole-second and fractional forms).
pub fn parse_stamp_time(raw: &str) -> Result<Timestamp, String> {
    if raw.len() > 35 || !raw.ends_with('Z') {
        return Err(format!(
            "timestamp {raw:?} must be an RFC 3339 UTC string ending in 'Z'"
        ));
    }
    raw.parse::<Timestamp>()
        .map_err(|err| format!("invalid RFC 3339 timestamp {raw:?}: {err}"))
}

/// Returns a fresh stamp strictly newer than `min_after` (if provided) authored on `node_name`.
pub fn next_stamp(node_name: &str, min_after: Option<&Stamp>) -> Stamp {
    let mut now = Timestamp::now();
    if let Some(prev) = min_after
        && let Ok(prev_ts) = parse_stamp_time(&prev.at)
        && now <= prev_ts
    {
        let bumped = prev_ts
            .as_nanosecond()
            .saturating_add(1);
        if let Ok(ts) = Timestamp::from_nanosecond(bumped) {
            now = ts;
        }
    }
    Stamp {
        at: format_stamp_time(now),
        on: node_name.to_owned(),
    }
}

/// Logical entry stamp `(at, on)` establishing a deterministic total order across nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    /// Canonical RFC 3339 UTC timestamp (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`).
    pub at: String,
    /// Name of the node that authored this change.
    pub on: String,
}

impl Stamp {
    /// Creates a new stamp from an explicit timestamp and node name.
    pub fn new(at: impl Into<String>, on: impl Into<String>) -> Self {
        Self {
            at: at.into(),
            on: on.into(),
        }
    }

    /// Compares two stamps by `(parsed_timestamp_nanos, on)`.
    pub fn cmp_total(&self, other: &Self) -> Ordering {
        let lhs = parse_stamp_time(&self.at)
            .map(Timestamp::as_nanosecond)
            .unwrap_or(i128::MIN);
        let rhs = parse_stamp_time(&other.at)
            .map(Timestamp::as_nanosecond)
            .unwrap_or(i128::MIN);
        lhs.cmp(&rhs)
            .then_with(|| self.on.cmp(&other.on))
            .then_with(|| self.at.cmp(&other.at))
    }

    /// Validates timestamp syntax and node name.
    pub fn validate(&self, field: &str) -> Result<(), String> {
        parse_stamp_time(&self.at).map_err(|err| format!("{field}: {err}"))?;
        validate_node_label(&self.on).map_err(|err| format!("{field}: {err}"))?;
        Ok(())
    }
}

impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_total(other)
    }
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn validate_node_label(name: &str) -> Result<(), String> {
    NodeName::try_from(name.to_owned())?;
    validate_member_id(name).map_err(|err| err.to_string())?;
    Ok(())
}

/// Fleet-wide shared sync folder configuration (`sync.toml`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncFile {
    /// Schema version (always `1`).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Active sync folders keyed by folder id.
    #[serde(default)]
    pub folders: BTreeMap<String, SyncFolderEntry>,
    /// Deleted sync folders (tombstones) keyed by folder id.
    #[serde(default)]
    pub removed: BTreeMap<String, FolderTombstone>,
}

fn default_version() -> u32 {
    SYNC_VERSION
}

fn default_bin_days() -> u32 {
    DEFAULT_BIN_DAYS
}

impl Default for SyncFile {
    fn default() -> Self {
        Self {
            version: SYNC_VERSION,
            folders: BTreeMap::new(),
            removed: BTreeMap::new(),
        }
    }
}

/// One active sync folder in `sync.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncFolderEntry {
    /// Human-readable display name.
    pub name: String,
    /// Per-folder custom ignore globs.
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Whether syncing is paused for this folder.
    #[serde(default)]
    pub paused: bool,
    /// Days to retain deleted/replaced copies in `.legion-sync/bin`.
    #[serde(default = "default_bin_days")]
    pub bin_days: u32,
    /// Creation timestamp (RFC 3339 UTC).
    pub created_at: String,
    /// Node where the folder was created.
    pub created_on: String,
    /// Latest folder metadata update timestamp (RFC 3339 UTC).
    pub updated_at: String,
    /// Node that authored the latest folder metadata update.
    pub updated_on: String,
    /// Optional per-field stamp for `name` (defaults to `updated_at`/`updated_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_updated_at: Option<String>,
    /// Optional authoring node for `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_updated_on: Option<String>,
    /// Optional per-field stamp for `ignore` (defaults to `updated_at`/`updated_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_updated_at: Option<String>,
    /// Optional authoring node for `ignore`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_updated_on: Option<String>,
    /// Optional per-field stamp for `paused` (defaults to `updated_at`/`updated_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_updated_at: Option<String>,
    /// Optional authoring node for `paused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_updated_on: Option<String>,
    /// Optional per-field stamp for `bin_days` (defaults to `updated_at`/`updated_on`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_days_updated_at: Option<String>,
    /// Optional authoring node for `bin_days`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_days_updated_on: Option<String>,
    /// Active members keyed by machine name.
    #[serde(default)]
    pub members: BTreeMap<String, SyncMemberEntry>,
    /// Removed members (tombstones) keyed by machine name.
    #[serde(default)]
    pub removed_members: BTreeMap<String, MemberTombstone>,
}

impl SyncFolderEntry {
    /// Returns the effective creation stamp.
    pub fn created_stamp(&self) -> Stamp {
        Stamp::new(&self.created_at, &self.created_on)
    }

    /// Returns the folder's top-level updated stamp.
    pub fn updated_stamp(&self) -> Stamp {
        Stamp::new(&self.updated_at, &self.updated_on)
    }

    /// Returns the effective stamp for `name`.
    pub fn name_stamp(&self) -> Stamp {
        match (&self.name_updated_at, &self.name_updated_on) {
            (Some(at), Some(on)) => Stamp::new(at, on),
            _ => self.updated_stamp(),
        }
    }

    /// Returns the effective stamp for `ignore`.
    pub fn ignore_stamp(&self) -> Stamp {
        match (&self.ignore_updated_at, &self.ignore_updated_on) {
            (Some(at), Some(on)) => Stamp::new(at, on),
            _ => self.updated_stamp(),
        }
    }

    /// Returns the effective stamp for `paused`.
    pub fn paused_stamp(&self) -> Stamp {
        match (&self.paused_updated_at, &self.paused_updated_on) {
            (Some(at), Some(on)) => Stamp::new(at, on),
            _ => self.updated_stamp(),
        }
    }

    /// Returns the effective stamp for `bin_days`.
    pub fn bin_days_stamp(&self) -> Stamp {
        match (&self.bin_days_updated_at, &self.bin_days_updated_on) {
            (Some(at), Some(on)) => Stamp::new(at, on),
            _ => self.updated_stamp(),
        }
    }

    /// Returns the maximum stamp across all fields and members of this folder.
    pub fn max_stamp(&self) -> Stamp {
        let mut best = self.updated_stamp();
        for s in [
            self.created_stamp(),
            self.name_stamp(),
            self.ignore_stamp(),
            self.paused_stamp(),
            self.bin_days_stamp(),
        ] {
            if s > best {
                best = s;
            }
        }
        for m in self.members.values() {
            let ms = m.updated_stamp();
            if ms > best {
                best = ms;
            }
        }
        for rm in self.removed_members.values() {
            let rs = rm.removed_stamp();
            if rs > best {
                best = rs;
            }
        }
        best
    }

    /// Sets `paused` with a fresh stamp strictly after existing stamps on this folder.
    pub fn set_paused(&mut self, paused: bool, node_name: &str) {
        let stamp = next_stamp(node_name, Some(&self.max_stamp()));
        self.paused = paused;
        self.paused_updated_at = Some(stamp.at.clone());
        self.paused_updated_on = Some(stamp.on.clone());
        self.updated_at = stamp.at;
        self.updated_on = stamp.on;
        self.normalize_field_stamps();
    }

    /// Normalizes `updated_at`/`updated_on` to at least the maximum of all per-field stamps and
    /// clears optional per-field stamps that match `updated_at`/`updated_on`.
    pub fn normalize_field_stamps(&mut self) {
        let name_s = self.name_stamp();
        let ignore_s = self.ignore_stamp();
        let paused_s = self.paused_stamp();
        let bin_s = self.bin_days_stamp();
        let mut base = self.updated_stamp();
        for s in [&name_s, &ignore_s, &paused_s, &bin_s] {
            if *s > base {
                base = s.clone();
            }
        }
        self.updated_at = base.at.clone();
        self.updated_on = base.on.clone();
        if name_s == base {
            self.name_updated_at = None;
            self.name_updated_on = None;
        } else {
            self.name_updated_at = Some(name_s.at);
            self.name_updated_on = Some(name_s.on);
        }
        if ignore_s == base {
            self.ignore_updated_at = None;
            self.ignore_updated_on = None;
        } else {
            self.ignore_updated_at = Some(ignore_s.at);
            self.ignore_updated_on = Some(ignore_s.on);
        }
        if paused_s == base {
            self.paused_updated_at = None;
            self.paused_updated_on = None;
        } else {
            self.paused_updated_at = Some(paused_s.at);
            self.paused_updated_on = Some(paused_s.on);
        }
        if bin_s == base {
            self.bin_days_updated_at = None;
            self.bin_days_updated_on = None;
        } else {
            self.bin_days_updated_at = Some(bin_s.at);
            self.bin_days_updated_on = Some(bin_s.on);
        }
    }
}

/// One active member inside a sync folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMemberEntry {
    /// Absolute (or `~`-prefixed) directory path on this member machine.
    pub path: String,
    /// When this member was added (RFC 3339 UTC).
    pub added_at: String,
    /// Node that added this member.
    pub added_on: String,
    /// Latest update timestamp for this member entry (RFC 3339 UTC).
    pub updated_at: String,
    /// Node that authored the latest update for this member entry.
    pub updated_on: String,
}

impl SyncMemberEntry {
    /// Returns this member's update stamp.
    pub fn updated_stamp(&self) -> Stamp {
        Stamp::new(&self.updated_at, &self.updated_on)
    }

    /// Returns this member's added stamp.
    pub fn added_stamp(&self) -> Stamp {
        Stamp::new(&self.added_at, &self.added_on)
    }
}

/// Tombstone recording that a member was removed from a sync folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberTombstone {
    /// When the member was removed (RFC 3339 UTC).
    pub removed_at: String,
    /// Node that recorded the removal.
    pub removed_on: String,
}

impl MemberTombstone {
    /// Returns this tombstone's removal stamp.
    pub fn removed_stamp(&self) -> Stamp {
        Stamp::new(&self.removed_at, &self.removed_on)
    }
}

/// Tombstone recording that an entire sync folder was deleted (`sync.delete`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderTombstone {
    /// When the folder was deleted (RFC 3339 UTC).
    pub removed_at: String,
    /// Node that recorded the deletion.
    pub removed_on: String,
}

impl FolderTombstone {
    /// Returns this tombstone's removal stamp.
    pub fn removed_stamp(&self) -> Stamp {
        Stamp::new(&self.removed_at, &self.removed_on)
    }
}

impl Document for SyncFile {
    const FILE: &'static str = "sync.toml";

    fn validate(&self) -> Result<(), String> {
        self.validate_strict()
    }
}

impl SyncFile {
    /// Validates every field, identifier, timestamp, ignore pattern, and path in this `SyncFile`.
    pub fn validate_strict(&self) -> Result<(), String> {
        if self.version != SYNC_VERSION {
            return Err(format!(
                "unsupported sync.toml version {} (this node supports version {SYNC_VERSION})",
                self.version
            ));
        }
        for (id, tomb) in &self.removed {
            validate_folder_id(id).map_err(|e| format!("removed.{id}: {e}"))?;
            tomb.removed_stamp()
                .validate(&format!("removed.{id}"))?;
            if self.folders.contains_key(id) {
                return Err(format!(
                    "folder {id:?} appears in both [folders] and [removed]"
                ));
            }
        }
        for (id, folder) in &self.folders {
            validate_folder_id(id).map_err(|e| format!("folders.{id}: {e}"))?;
            validate_folder_name(&folder.name)?;
            if folder.bin_days < MIN_BIN_DAYS || folder.bin_days > MAX_BIN_DAYS {
                return Err(format!(
                    "folders.{id}.bin_days must be between {MIN_BIN_DAYS} and {MAX_BIN_DAYS} (got {})",
                    folder.bin_days
                ));
            }
            if folder.ignore.len() > MAX_IGNORE_PATTERNS {
                return Err(format!(
                    "folders.{id}.ignore exceeds maximum of {MAX_IGNORE_PATTERNS} patterns"
                ));
            }
            for pat in &folder.ignore {
                if pat.is_empty() || pat.len() > MAX_IGNORE_PATTERN_LEN {
                    return Err(format!(
                        "folders.{id}.ignore contains an empty or over-long pattern"
                    ));
                }
            }
            IgnoreMatcher::compile(&folder.ignore)
                .map_err(|err| format!("folders.{id}.ignore: {err}"))?;
            folder
                .created_stamp()
                .validate(&format!("folders.{id}.created"))?;
            folder
                .updated_stamp()
                .validate(&format!("folders.{id}.updated"))?;
            validate_optional_stamp_pair(
                &folder.name_updated_at,
                &folder.name_updated_on,
                &format!("folders.{id}.name_updated"),
            )?;
            validate_optional_stamp_pair(
                &folder.ignore_updated_at,
                &folder.ignore_updated_on,
                &format!("folders.{id}.ignore_updated"),
            )?;
            validate_optional_stamp_pair(
                &folder.paused_updated_at,
                &folder.paused_updated_on,
                &format!("folders.{id}.paused_updated"),
            )?;
            validate_optional_stamp_pair(
                &folder.bin_days_updated_at,
                &folder.bin_days_updated_on,
                &format!("folders.{id}.bin_days_updated"),
            )?;
            for (member, entry) in &folder.members {
                validate_node_label(member)
                    .map_err(|e| format!("folders.{id}.members.{member}: {e}"))?;
                validate_member_path(&entry.path)
                    .map_err(|e| format!("folders.{id}.members.{member}.path: {e}"))?;
                entry
                    .added_stamp()
                    .validate(&format!("folders.{id}.members.{member}.added"))?;
                entry
                    .updated_stamp()
                    .validate(&format!("folders.{id}.members.{member}.updated"))?;
                if folder.removed_members.contains_key(member) {
                    return Err(format!(
                        "folders.{id}: member {member:?} appears in both members and removed_members"
                    ));
                }
            }
            for (member, tomb) in &folder.removed_members {
                validate_node_label(member)
                    .map_err(|e| format!("folders.{id}.removed_members.{member}: {e}"))?;
                tomb.removed_stamp()
                    .validate(&format!("folders.{id}.removed_members.{member}"))?;
            }
        }
        Ok(())
    }

    /// Serializes this `SyncFile` into deterministic canonical TOML, verifying the size bound.
    pub fn to_canonical_toml(&self) -> Result<String, ErrorBody> {
        let mut normalized = self.clone();
        for folder in normalized.folders.values_mut() {
            folder.normalize_field_stamps();
        }
        let text = toml::to_string_pretty(&normalized).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("sync.toml could not be serialised: {error}."),
            )
        })?;
        if text.len() > MAX_SYNC_TOML_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "sync.toml would be {} bytes, exceeding the {MAX_SYNC_TOML_BYTES}-byte limit.",
                    text.len()
                ),
            )
            .with_hint("Reduce the number of sync folders or ignore patterns in sync.toml."));
        }
        Ok(text)
    }

    /// Returns all active folders that list `machine` as a current member.
    pub fn folders_for_machine(&self, machine: &str) -> Vec<(&str, &SyncFolderEntry, &SyncMemberEntry)> {
        self.folders
            .iter()
            .filter_map(|(id, folder)| {
                folder
                    .members
                    .get(machine)
                    .map(|member| (id.as_str(), folder, member))
            })
            .collect()
    }
}

fn validate_optional_stamp_pair(
    at: &Option<String>,
    on: &Option<String>,
    field: &str,
) -> Result<(), String> {
    match (at, on) {
        (Some(at), Some(on)) => Stamp::new(at, on).validate(field),
        (None, None) => Ok(()),
        _ => Err(format!("{field}_at and {field}_on must either both be set or both be omitted")),
    }
}

/// Validates a folder's display name.
pub fn validate_folder_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || name.len() > MAX_FOLDER_NAME_LEN {
        return Err(format!(
            "folder name must be between 1 and {MAX_FOLDER_NAME_LEN} non-whitespace bytes"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("folder name cannot contain control characters".to_owned());
    }
    Ok(())
}

/// Validates a member path string stored in `sync.toml` (absolute or `~`-prefixed, no NUL/control).
pub fn validate_member_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_MEMBER_PATH_LEN {
        return Err(format!(
            "member path must be between 1 and {MAX_MEMBER_PATH_LEN} bytes"
        ));
    }
    if path.chars().any(char::is_control) {
        return Err("member path cannot contain control characters".to_owned());
    }
    let is_abs_unix = path.starts_with('/');
    let is_tilde = path == "~" || path.starts_with("~/") || path.starts_with("~\\");
    let bytes = path.as_bytes();
    let is_win_drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !(is_abs_unix || is_tilde || is_win_drive) {
        return Err(format!(
            "{path:?} must be an absolute path (starting with /, ~, or a Windows drive letter)"
        ));
    }
    Ok(())
}

/// Derives a safe folder identifier slug from a display name, avoiding collisions with existing
/// live or tombstoned folder ids.
pub fn derive_folder_id(name: &str, existing: &SyncFile) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !slug.is_empty() {
            slug.push('-');
            prev_dash = true;
        }
        if slug.len() >= 48 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("folder");
    }
    let taken = |candidate: &str| {
        existing.folders.contains_key(candidate) || existing.removed.contains_key(candidate)
    };
    if !taken(&slug) {
        return slug;
    }
    for n in 2u32..10_000 {
        let candidate = format!("{slug}-{n}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{slug}-{}", legion_sync::model::random_hex_token())
}

/// Merges two `SyncFile` documents according to the join-semilattice rule documented at module
/// level. Strictly commutative, associative, and idempotent.
pub fn merge_sync_files(a: &SyncFile, b: &SyncFile) -> SyncFile {
    let mut out = SyncFile {
        version: SYNC_VERSION,
        folders: BTreeMap::new(),
        removed: BTreeMap::new(),
    };

    let mut all_ids = BTreeSet::new();
    all_ids.extend(a.folders.keys().cloned());
    all_ids.extend(a.removed.keys().cloned());
    all_ids.extend(b.folders.keys().cloned());
    all_ids.extend(b.removed.keys().cloned());

    for id in all_ids {
        // Folder-level tombstone always absorbs live folder state; keep greater removal stamp.
        let tomb = match (a.removed.get(&id), b.removed.get(&id)) {
            (Some(ta), Some(tb)) => Some(max_folder_tombstone(ta, tb)),
            (Some(ta), None) => Some(ta.clone()),
            (None, Some(tb)) => Some(tb.clone()),
            (None, None) => None,
        };
        if let Some(t) = tomb {
            out.removed.insert(id, t);
            continue;
        }

        match (a.folders.get(&id), b.folders.get(&id)) {
            (Some(fa), Some(fb)) => {
                out.folders.insert(id, merge_folder_entries(fa, fb));
            }
            (Some(fa), None) => {
                let mut f = fa.clone();
                f.normalize_field_stamps();
                out.folders.insert(id, f);
            }
            (None, Some(fb)) => {
                let mut f = fb.clone();
                f.normalize_field_stamps();
                out.folders.insert(id, f);
            }
            (None, None) => {}
        }
    }

    out
}

fn max_folder_tombstone(a: &FolderTombstone, b: &FolderTombstone) -> FolderTombstone {
    if b.removed_stamp() > a.removed_stamp() {
        b.clone()
    } else {
        a.clone()
    }
}

fn merge_stamped_field<T: Ord + Clone>(
    sa: Stamp,
    va: &T,
    sb: Stamp,
    vb: &T,
) -> (Stamp, T) {
    match sa.cmp(&sb) {
        Ordering::Greater => (sa, va.clone()),
        Ordering::Less => (sb, vb.clone()),
        Ordering::Equal => {
            if vb > va {
                (sb, vb.clone())
            } else {
                (sa, va.clone())
            }
        }
    }
}

fn merge_folder_entries(a: &SyncFolderEntry, b: &SyncFolderEntry) -> SyncFolderEntry {
    // Earliest creation stamp (symmetric min).
    let created = if b.created_stamp() < a.created_stamp() {
        b.created_stamp()
    } else {
        a.created_stamp()
    };

    let (name_s, name) = merge_stamped_field(a.name_stamp(), &a.name, b.name_stamp(), &b.name);
    let (ignore_s, ignore) =
        merge_stamped_field(a.ignore_stamp(), &a.ignore, b.ignore_stamp(), &b.ignore);
    let (paused_s, paused) =
        merge_stamped_field(a.paused_stamp(), &a.paused, b.paused_stamp(), &b.paused);
    let (bin_s, bin_days) = merge_stamped_field(
        a.bin_days_stamp(),
        &a.bin_days,
        b.bin_days_stamp(),
        &b.bin_days,
    );

    let mut updated = a.updated_stamp();
    for s in [
        b.updated_stamp(),
        name_s.clone(),
        ignore_s.clone(),
        paused_s.clone(),
        bin_s.clone(),
    ] {
        if s > updated {
            updated = s;
        }
    }

    // Merge members and member tombstones point-wise by total order `(stamp, is_tomb, tiebreak)`.
    let mut all_members = BTreeSet::new();
    all_members.extend(a.members.keys().cloned());
    all_members.extend(a.removed_members.keys().cloned());
    all_members.extend(b.members.keys().cloned());
    all_members.extend(b.removed_members.keys().cloned());

    let mut members = BTreeMap::new();
    let mut removed_members = BTreeMap::new();

    for m in all_members {
        let state_a = member_state_of(a, &m);
        let state_b = member_state_of(b, &m);
        match merge_member_state(state_a, state_b) {
            Some(MemberSlot::Live(entry)) => {
                members.insert(m, entry);
            }
            Some(MemberSlot::Removed(tomb)) => {
                removed_members.insert(m, tomb);
            }
            None => {}
        }
    }

    let mut merged = SyncFolderEntry {
        name,
        ignore,
        paused,
        bin_days,
        created_at: created.at,
        created_on: created.on,
        updated_at: updated.at,
        updated_on: updated.on,
        name_updated_at: Some(name_s.at),
        name_updated_on: Some(name_s.on),
        ignore_updated_at: Some(ignore_s.at),
        ignore_updated_on: Some(ignore_s.on),
        paused_updated_at: Some(paused_s.at),
        paused_updated_on: Some(paused_s.on),
        bin_days_updated_at: Some(bin_s.at),
        bin_days_updated_on: Some(bin_s.on),
        members,
        removed_members,
    };
    merged.normalize_field_stamps();
    merged
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum MemberSlot {
    Live(SyncMemberEntry),
    Removed(MemberTombstone),
}

impl MemberSlot {
    fn cmp_total(&self, other: &Self) -> Ordering {
        let (sa, ta) = match self {
            Self::Live(e) => (e.updated_stamp(), false),
            Self::Removed(t) => (t.removed_stamp(), true),
        };
        let (sb, tb) = match other {
            Self::Live(e) => (e.updated_stamp(), false),
            Self::Removed(t) => (t.removed_stamp(), true),
        };
        sa.cmp(&sb).then_with(|| ta.cmp(&tb)).then_with(|| match (self, other) {
            (Self::Live(ea), Self::Live(eb)) => ea
                .path
                .cmp(&eb.path)
                .then_with(|| ea.added_stamp().cmp(&eb.added_stamp())),
            _ => Ordering::Equal,
        })
    }
}

fn member_state_of(folder: &SyncFolderEntry, member: &str) -> Option<MemberSlot> {
    match (folder.members.get(member), folder.removed_members.get(member)) {
        (Some(live), Some(tomb)) => {
            let l = MemberSlot::Live(live.clone());
            let r = MemberSlot::Removed(tomb.clone());
            if r.cmp_total(&l) != Ordering::Less {
                Some(r)
            } else {
                Some(l)
            }
        }
        (Some(live), None) => Some(MemberSlot::Live(live.clone())),
        (None, Some(tomb)) => Some(MemberSlot::Removed(tomb.clone())),
        (None, None) => None,
    }
}

fn merge_member_state(a: Option<MemberSlot>, b: Option<MemberSlot>) -> Option<MemberSlot> {
    match (a, b) {
        (Some(sa), Some(sb)) => {
            if sb.cmp_total(&sa) == Ordering::Greater {
                Some(sb)
            } else {
                Some(sa)
            }
        }
        (Some(sa), None) => Some(sa),
        (None, Some(sb)) => Some(sb),
        (None, None) => None,
    }
}

/// Acquires `state/sync/config.lock` before reading or writing `sync.toml`.
pub fn lock_sync_config(paths: &Paths) -> Result<FileLock, ErrorBody> {
    let lock_path = paths.state_dir().join("sync").join("config.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    FileLock::acquire_timeout(&lock_path, LockMode::Exclusive, CONFIG_LOCK_TIMEOUT).map_err(
        |error| match error {
            OsError::TimedOut { .. } => ErrorBody::new(
                ErrorCode::Busy,
                "Another call has been updating sync.toml for too long.",
            )
            .with_hint("Try again in a moment."),
            other => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be locked: {other}.", lock_path.display()),
            ),
        },
    )
}

/// Reads and parses `sync.toml` from `paths`, returning `Ok((SyncFile::default(), None))` when
/// absent.
pub fn load_sync_file_raw(paths: &Paths) -> Result<(SyncFile, Option<Vec<u8>>), ErrorBody> {
    let path = paths.config_file(SyncFile::FILE);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((SyncFile::default(), None));
        }
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            ));
        }
    };
    let parsed = parse::<SyncFile>(&path, &bytes)?;
    Ok((parsed, Some(bytes)))
}

/// Reads and parses `sync.toml` from `paths`, returning an empty `SyncFile` if the file does not
/// exist yet.
pub fn load_sync_file(paths: &Paths) -> Result<SyncFile, ErrorBody> {
    load_sync_file_raw(paths).map(|(file, _)| file)
}

/// Outcome of atomically saving (or merging) `sync.toml`.
#[derive(Clone, Debug)]
pub struct SyncSaveOutcome {
    /// Merged / saved `SyncFile`.
    pub file: SyncFile,
    /// Canonical TOML written to disk.
    pub canonical_toml: String,
    /// Raw bytes on disk before the write, if the file existed.
    pub bytes_before: Option<Vec<u8>>,
    /// Backup path under `state/sync/backups/sync.toml.bak`, if a previous file was backed up.
    pub backup: Option<String>,
}

/// Validates, backs up (when previously present), and atomically writes `file` to `sync.toml`.
/// Caller must hold [`lock_sync_config`].
pub fn save_sync_file_locked(
    paths: &Paths,
    file: &SyncFile,
    bytes_before: Option<Vec<u8>>,
) -> Result<SyncSaveOutcome, ErrorBody> {
    let target = paths.config_file(SyncFile::FILE);
    file.validate_strict().map_err(|reason| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} is not valid: {reason}.", target.display()),
        )
        .with_hint("Fix sync.toml or manage sync folders with `legion sync.create`, `sync.add`, `sync.remove`, and `sync.delete`.")
        .with_details(json!({"file": target}))
    })?;
    let canonical_toml = file.to_canonical_toml()?;

    let backup = if let Some(old_bytes) = bytes_before.as_deref() {
        let backup_path = paths
            .state_dir()
            .join("sync")
            .join("backups")
            .join("sync.toml.bak");
        if let Some(parent) = backup_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be created: {error}.", parent.display()),
                )
            })?;
        }
        write_atomically(&backup_path, old_bytes).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Backup {} cannot be written: {error}.",
                    backup_path.display()
                ),
            )
        })?;
        Some(backup_path.display().to_string())
    } else {
        None
    };

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    write_atomically(&target, canonical_toml.as_bytes()).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", target.display()),
        )
    })?;

    Ok(SyncSaveOutcome {
        file: file.clone(),
        canonical_toml,
        bytes_before,
        backup,
    })
}

/// Merges `incoming` into this node's `sync.toml` under `state/sync/config.lock`, checking an
/// optional precondition on the pre-merge bytes before saving atomically.
pub fn apply_sync_merge_blocking<F>(
    paths: &Paths,
    incoming: &SyncFile,
    check_precondition: F,
) -> Result<SyncSaveOutcome, ErrorBody>
where
    F: FnOnce(Option<&[u8]>) -> Result<(), ErrorBody>,
{
    let _lock = lock_sync_config(paths)?;
    let (local, bytes_before) = load_sync_file_raw(paths)?;
    check_precondition(bytes_before.as_deref())?;
    let merged = merge_sync_files(&local, incoming);
    save_sync_file_locked(paths, &merged, bytes_before)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use proptest::prelude::*;

    fn ts(secs: i64) -> String {
        let t = Timestamp::from_second(1_770_000_000 + secs).expect("valid ts");
        format_stamp_time(t)
    }

    fn mk_member(path: &str, secs: i64, on: &str) -> SyncMemberEntry {
        let t = ts(secs);
        SyncMemberEntry {
            path: path.to_owned(),
            added_at: t.clone(),
            added_on: on.to_owned(),
            updated_at: t,
            updated_on: on.to_owned(),
        }
    }

    fn mk_folder(name: &str, secs: i64, on: &str) -> SyncFolderEntry {
        let t = ts(secs);
        SyncFolderEntry {
            name: name.to_owned(),
            ignore: Vec::new(),
            paused: false,
            bin_days: DEFAULT_BIN_DAYS,
            created_at: t.clone(),
            created_on: on.to_owned(),
            updated_at: t,
            updated_on: on.to_owned(),
            name_updated_at: None,
            name_updated_on: None,
            ignore_updated_at: None,
            ignore_updated_on: None,
            paused_updated_at: None,
            paused_updated_on: None,
            bin_days_updated_at: None,
            bin_days_updated_on: None,
            members: BTreeMap::new(),
            removed_members: BTreeMap::new(),
        }
    }

    #[test]
    fn concurrent_add_member_on_two_machines_keeps_both() {
        let mut base = SyncFile::default();
        let mut f = mk_folder("Docs", 10, "tower");
        f.members
            .insert("tower".to_owned(), mk_member("/home/me/docs", 10, "tower"));
        base.folders.insert("docs".to_owned(), f);

        let mut on_a = base.clone();
        on_a.folders
            .get_mut("docs")
            .unwrap()
            .members
            .insert("laptop".to_owned(), mk_member("/Users/me/docs", 20, "tower"));

        let mut on_b = base.clone();
        on_b.folders
            .get_mut("docs")
            .unwrap()
            .members
            .insert("pi".to_owned(), mk_member("/srv/docs", 20, "pi"));

        let ab = merge_sync_files(&on_a, &on_b);
        let ba = merge_sync_files(&on_b, &on_a);
        assert_eq!(ab, ba);
        let members = &ab.folders["docs"].members;
        assert_eq!(members.len(), 3);
        assert_eq!(members["tower"].path, "/home/me/docs");
        assert_eq!(members["laptop"].path, "/Users/me/docs");
        assert_eq!(members["pi"].path, "/srv/docs");
    }

    #[test]
    fn member_removal_wins_over_older_and_equal_edit_but_newer_readd_wins() {
        let mut a = SyncFile::default();
        let mut fa = mk_folder("Docs", 10, "tower");
        fa.members
            .insert("laptop".to_owned(), mk_member("/Users/me/docs", 20, "tower"));
        a.folders.insert("docs".to_owned(), fa);

        // Removal at t=25 beats edit at t=20.
        let mut b = SyncFile::default();
        let mut fb = mk_folder("Docs", 10, "tower");
        fb.removed_members.insert(
            "laptop".to_owned(),
            MemberTombstone {
                removed_at: ts(25),
                removed_on: "pi".to_owned(),
            },
        );
        b.folders.insert("docs".to_owned(), fb);

        let ab = merge_sync_files(&a, &b);
        assert!(!ab.folders["docs"].members.contains_key("laptop"));
        assert!(ab.folders["docs"].removed_members.contains_key("laptop"));
        assert_eq!(ab, merge_sync_files(&b, &a));

        // Removal at equal stamp (t=20, same node) still beats live entry.
        let mut b_eq = SyncFile::default();
        let mut fb_eq = mk_folder("Docs", 10, "tower");
        fb_eq.removed_members.insert(
            "laptop".to_owned(),
            MemberTombstone {
                removed_at: ts(20),
                removed_on: "tower".to_owned(),
            },
        );
        b_eq.folders.insert("docs".to_owned(), fb_eq);
        let ab_eq = merge_sync_files(&a, &b_eq);
        assert!(!ab_eq.folders["docs"].members.contains_key("laptop"));
        assert!(ab_eq.folders["docs"].removed_members.contains_key("laptop"));

        // Strictly newer re-add (t=30) beats older removal (t=25).
        let mut c = SyncFile::default();
        let mut fc = mk_folder("Docs", 10, "tower");
        fc.members.insert(
            "laptop".to_owned(),
            mk_member("/Users/me/docs-new", 30, "laptop"),
        );
        c.folders.insert("docs".to_owned(), fc);

        let abc = merge_sync_files(&ab, &c);
        assert_eq!(abc.folders["docs"].members["laptop"].path, "/Users/me/docs-new");
        assert!(!abc.folders["docs"].removed_members.contains_key("laptop"));
        assert_eq!(abc, merge_sync_files(&a, &merge_sync_files(&b, &c)));
    }

    #[test]
    fn folder_removal_beats_older_and_concurrent_edits_and_roundtrips_toml() {
        let mut a = SyncFile::default();
        let mut fa = mk_folder("Docs", 10, "tower");
        fa.members
            .insert("tower".to_owned(), mk_member("/home/me/docs", 15, "tower"));
        fa.set_paused(true, "tower");
        a.folders.insert("docs".to_owned(), fa);

        let mut b = SyncFile::default();
        b.removed.insert(
            "docs".to_owned(),
            FolderTombstone {
                removed_at: ts(20),
                removed_on: "pi".to_owned(),
            },
        );

        let merged = merge_sync_files(&a, &b);
        assert!(!merged.folders.contains_key("docs"));
        assert!(merged.removed.contains_key("docs"));
        assert_eq!(merged, merge_sync_files(&b, &a));

        let toml_text = merged.to_canonical_toml().unwrap();
        let reparsed: SyncFile = parse(Path::new("sync.toml"), toml_text.as_bytes()).unwrap();
        assert_eq!(reparsed, merged);
    }

    fn arb_node() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("tower".to_owned()),
            Just("pi".to_owned()),
            Just("laptop".to_owned()),
            Just("studio".to_owned()),
        ]
    }

    fn arb_stamp() -> impl Strategy<Value = Stamp> {
        (0i64..8, arb_node()).prop_map(|(sec, on)| Stamp { at: ts(sec), on })
    }

    fn arb_member_or_tomb() -> impl Strategy<Value = Option<MemberSlot>> {
        prop_oneof![
            2 => Just(None),
            3 => (
                prop_oneof![
                    Just("/data/a".to_owned()),
                    Just("/data/b".to_owned()),
                    Just("~/docs".to_owned()),
                ],
                arb_stamp(),
                arb_stamp(),
            )
                .prop_map(|(path, added, updated)| {
                    Some(MemberSlot::Live(SyncMemberEntry {
                        path,
                        added_at: added.at,
                        added_on: added.on,
                        updated_at: updated.at,
                        updated_on: updated.on,
                    }))
                }),
            3 => arb_stamp().prop_map(|s| {
                Some(MemberSlot::Removed(MemberTombstone {
                    removed_at: s.at,
                    removed_on: s.on,
                }))
            }),
        ]
    }

    fn arb_folder_entry() -> BoxedStrategy<SyncFolderEntry> {
        (
            prop_oneof![
                Just("Docs".to_owned()),
                Just("Notes".to_owned()),
                Just("Photos".to_owned())
            ],
            prop_oneof![
                Just(vec![]),
                Just(vec!["*.tmp".to_owned()]),
                Just(vec![".git/".to_owned(), "*.bak".to_owned()]),
            ],
            any::<bool>(),
            prop_oneof![Just(7u32), Just(30u32), Just(90u32)],
            (arb_stamp(), arb_stamp(), arb_stamp()),
            (arb_stamp(), arb_stamp(), arb_stamp()),
            (
                arb_member_or_tomb(),
                arb_member_or_tomb(),
                arb_member_or_tomb(),
            ),
        )
            .prop_map(
                |(
                    name,
                    ignore,
                    paused,
                    bin_days,
                    (created, updated, name_s),
                    (ignore_s, paused_s, bin_s),
                    (m_tower, m_pi, m_laptop),
                )| {
                    let mut members = BTreeMap::new();
                    let mut removed_members = BTreeMap::new();
                    for (m_name, slot) in
                        [("tower", m_tower), ("pi", m_pi), ("laptop", m_laptop)]
                    {
                        match slot {
                            Some(MemberSlot::Live(e)) => {
                                members.insert(m_name.to_owned(), e);
                            }
                            Some(MemberSlot::Removed(t)) => {
                                removed_members.insert(m_name.to_owned(), t);
                            }
                            None => {}
                        }
                    }
                    let mut entry = SyncFolderEntry {
                        name,
                        ignore,
                        paused,
                        bin_days,
                        created_at: created.at,
                        created_on: created.on,
                        updated_at: updated.at,
                        updated_on: updated.on,
                        name_updated_at: Some(name_s.at),
                        name_updated_on: Some(name_s.on),
                        ignore_updated_at: Some(ignore_s.at),
                        ignore_updated_on: Some(ignore_s.on),
                        paused_updated_at: Some(paused_s.at),
                        paused_updated_on: Some(paused_s.on),
                        bin_days_updated_at: Some(bin_s.at),
                        bin_days_updated_on: Some(bin_s.on),
                        members,
                        removed_members,
                    };
                    entry.normalize_field_stamps();
                    entry
                },
            )
            .boxed()
    }

    fn arb_folder_state() -> BoxedStrategy<Option<Result<SyncFolderEntry, FolderTombstone>>> {
        prop_oneof![
            2 => Just(None),
            4 => arb_folder_entry().prop_map(|f| Some(Ok(f))),
            2 => arb_stamp().prop_map(|s| {
                Some(Err(FolderTombstone {
                    removed_at: s.at,
                    removed_on: s.on,
                }))
            }),
        ]
        .boxed()
    }

    fn arb_sync_file() -> BoxedStrategy<SyncFile> {
        (arb_folder_state(), arb_folder_state(), arb_folder_state())
            .prop_map(|(f1, f2, f3)| {
                let mut file = SyncFile::default();
                for (id, st) in [("alpha", f1), ("beta", f2), ("gamma", f3)] {
                    match st {
                        Some(Ok(folder)) => {
                            file.folders.insert(id.to_owned(), folder);
                        }
                        Some(Err(tomb)) => {
                            file.removed.insert(id.to_owned(), tomb);
                        }
                        None => {}
                    }
                }
                file
            })
            .boxed()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn merge_obeys_semilattice_laws_and_roundtrips(
            a in arb_sync_file(),
            b in arb_sync_file(),
            c in arb_sync_file(),
        ) {
            prop_assert!(a.validate_strict().is_ok());
            prop_assert!(b.validate_strict().is_ok());
            prop_assert!(c.validate_strict().is_ok());

            // Idempotence: a ⊔ a == a
            let aa = merge_sync_files(&a, &a);
            prop_assert_eq!(&aa, &a);

            // Commutativity: a ⊔ b == b ⊔ a
            let ab = merge_sync_files(&a, &b);
            let ba = merge_sync_files(&b, &a);
            prop_assert_eq!(&ab, &ba);

            // Associativity: (a ⊔ b) ⊔ c == a ⊔ (b ⊔ c)
            let ab_c = merge_sync_files(&ab, &c);
            let bc = merge_sync_files(&b, &c);
            let a_bc = merge_sync_files(&a, &bc);
            prop_assert_eq!(&ab_c, &a_bc);

            // Closure + canonical TOML round-trip
            prop_assert!(ab_c.validate_strict().is_ok());
            let toml_text = ab_c.to_canonical_toml().unwrap();
            let reparsed: SyncFile = parse(Path::new("sync.toml"), toml_text.as_bytes()).unwrap();
            prop_assert_eq!(&reparsed, &ab_c);
        }
    }
}
