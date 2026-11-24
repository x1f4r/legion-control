//! Fleet-wide paired-device registry (`devices.toml`), CRDT merge, `device:*` principal
//! reconciliation, first-run adoption of legacy `--pairing` lines, and derived `authorized_keys`
//! reconciliation.
//!
//! `devices.toml` lives beside `policy.toml` in Legion's configuration directory (so non-admin
//! roles cannot read or modify it through file tools). Each node derives its own `authorized_keys`
//! device lines from `devices.toml` while preserving foreign lines, pending invites, and entries
//! belonging to other Legion homes or OS accounts byte for byte.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use legion_core::config::{Document, NodeFile, NodeName, parse, write_atomically};
use legion_core::policy::{NOBODY, OWNER, Policy, Principal};
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::tokens::Tokens;
use legion_core::{Call, Ctx, Door, Node, Paths};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

use super::authkeys::{
    AuthKeysFile, AuthKeysTarget, ForcedCommandShell, LineKind, PinnedHome,
    edit_auth_keys_with_retry, format_device_line, line_matches_account, line_matches_home,
    read_auth_keys, resolve_safe_write_path, resolve_target,
    verify_exe_not_group_or_world_writable, verify_owner_matches_euid,
};
use super::keys::{fingerprint_key_b64, validate_ed25519_public_key, validate_pairing_id};
use super::sessions::validate_device_label;
use super::store::{DevicesState, load_state, lock_auth_keys};

/// Current schema version of `devices.toml`.
pub const DEVICES_VERSION: u32 = 1;

/// Prefix of per-device principals automatically managed in `policy.toml`.
pub const DEVICE_PRINCIPAL_PREFIX: &str = "device:";

/// Maximum length of a role name recorded in `devices.toml`.
const MAX_ROLE_NAME_LEN: usize = 128;

/// Longest time an operation waits for `state/sync/config.lock`.
const CONFIG_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Fleet-wide paired device registry (`devices.toml`), stored beside `policy.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicesFile {
    /// Schema version (always `1`).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Live paired devices keyed by their 32-hex pairing identifier.
    #[serde(default)]
    pub devices: BTreeMap<String, FleetDevice>,
    /// Removed devices (tombstones) keyed by pairing identifier. Tombstones always win in merges
    /// and can never be dropped by syncing.
    #[serde(default)]
    pub removed: BTreeMap<String, DeviceTombstone>,
}

fn default_version() -> u32 {
    DEVICES_VERSION
}

impl Default for DevicesFile {
    fn default() -> Self {
        Self {
            version: DEVICES_VERSION,
            devices: BTreeMap::new(),
            removed: BTreeMap::new(),
        }
    }
}

/// One live paired device entry in `devices.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetDevice {
    /// Sanitised device label (matches `--device <name>` and `legion-device:<name>`).
    pub name: String,
    /// Canonical OpenSSH Ed25519 public key (`"ssh-ed25519 <base64>"`).
    pub public_key: String,
    /// Principal pinned by `legion gate --as <principal>`.
    pub principal: String,
    /// Role recorded by the device's origin node (or by `device.set`) for `device:*` principal
    /// provisioning across the fleet. Sync reconciles the receiving node's `device:*` principal
    /// to this exact role and never raises a device's role above what was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// RFC 3339 UTC timestamp when this device was paired.
    pub paired_at: String,
    /// Node name where this device was paired.
    pub paired_on: String,
    /// RFC 3339 UTC timestamp of the most recent update (initially `paired_at`).
    pub updated_at: String,
    /// Node name that wrote the most recent update (initially `paired_on`).
    pub updated_on: String,
}

/// One removed-device tombstone in `devices.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceTombstone {
    /// Device label at the time of removal.
    pub name: String,
    /// Principal pinned at the time of removal.
    pub principal: String,
    /// RFC 3339 UTC timestamp when the device was originally paired, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_at: Option<String>,
    /// Node name where the device was originally paired, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_on: Option<String>,
    /// RFC 3339 UTC timestamp when the device was removed.
    pub removed_at: String,
    /// Node name where the device was removed.
    pub removed_on: String,
}

/// Per-machine outcome reported when pushing device changes to peers in `fleet.toml`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPushOutcome {
    /// `"synced"` when the peer merged the change, or `"not_reached"` when it could not be reached
    /// or returned an error.
    pub status: String,
    /// Human-readable reason when `status` is `"not_reached"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl FleetPushOutcome {
    /// Builds a `"synced"` peer push outcome.
    pub fn synced() -> Self {
        Self {
            status: "synced".to_owned(),
            reason: None,
        }
    }

    /// Builds a `"not_reached"` peer push outcome with `reason`.
    pub fn not_reached(reason: impl Into<String>) -> Self {
        Self {
            status: "not_reached".to_owned(),
            reason: Some(reason.into()),
        }
    }
}

/// Validates a role name recorded in `devices.toml` or passed to `device.invite` / `device.set`.
pub fn validate_role_name(role: &str) -> Result<(), ErrorBody> {
    if role.is_empty()
        || role.len() > MAX_ROLE_NAME_LEN
        || role.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{role:?} is not a valid role name."),
        )
        .with_hint("Use a role defined in policy.toml, or built-in `owner` or `nobody`."));
    }
    Ok(())
}

impl Document for DevicesFile {
    const FILE: &'static str = "devices.toml";
    const EMPTY_IS_DEFAULT: bool = true;

    fn validate(&self) -> Result<(), String> {
        if self.version != DEVICES_VERSION {
            return Err(format!(
                "unsupported devices.toml version {} (expected {DEVICES_VERSION})",
                self.version
            ));
        }
        for (id, entry) in &self.devices {
            validate_pairing_id(id).map_err(|e| format!("devices.{id}: {}", e.message))?;
            if self.removed.contains_key(id) {
                return Err(format!(
                    "pairing {id} appears in both [devices] and [removed]; removed entries must stay tombstoned"
                ));
            }
            validate_device_label(&entry.name)
                .map_err(|e| format!("devices.{id}.name: {}", e.message))?;
            let validated_key = validate_ed25519_public_key(&entry.public_key)
                .map_err(|e| format!("devices.{id}.public_key: {}", e.message))?;
            if validated_key.canonical != entry.public_key {
                return Err(format!(
                    "devices.{id}.public_key must be canonical `ssh-ed25519 <base64>` without extra comments or whitespace"
                ));
            }
            Principal::parse(&entry.principal)
                .map_err(|e| format!("devices.{id}.principal: {}", e.message))?;
            if let Some(role) = &entry.role {
                validate_role_name(role)
                    .map_err(|e| format!("devices.{id}.role: {}", e.message))?;
            }
            entry
                .paired_at
                .parse::<Timestamp>()
                .map_err(|e| format!("devices.{id}.paired_at is not RFC 3339 UTC: {e}"))?;
            NodeName::try_from(entry.paired_on.clone())
                .map_err(|e| format!("devices.{id}.paired_on: {e}"))?;
            entry
                .updated_at
                .parse::<Timestamp>()
                .map_err(|e| format!("devices.{id}.updated_at is not RFC 3339 UTC: {e}"))?;
            NodeName::try_from(entry.updated_on.clone())
                .map_err(|e| format!("devices.{id}.updated_on: {e}"))?;
        }
        for (id, tomb) in &self.removed {
            validate_pairing_id(id).map_err(|e| format!("removed.{id}: {}", e.message))?;
            validate_device_label(&tomb.name)
                .map_err(|e| format!("removed.{id}.name: {}", e.message))?;
            Principal::parse(&tomb.principal)
                .map_err(|e| format!("removed.{id}.principal: {}", e.message))?;
            if let Some(paired_at) = &tomb.paired_at {
                paired_at
                    .parse::<Timestamp>()
                    .map_err(|e| format!("removed.{id}.paired_at is not RFC 3339 UTC: {e}"))?;
            }
            if let Some(paired_on) = &tomb.paired_on {
                NodeName::try_from(paired_on.clone())
                    .map_err(|e| format!("removed.{id}.paired_on: {e}"))?;
            }
            tomb.removed_at
                .parse::<Timestamp>()
                .map_err(|e| format!("removed.{id}.removed_at is not RFC 3339 UTC: {e}"))?;
            NodeName::try_from(tomb.removed_on.clone())
                .map_err(|e| format!("removed.{id}.removed_on: {e}"))?;
        }
        Ok(())
    }
}

impl DevicesFile {
    /// Returns `true` when there are neither live devices nor tombstones.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty() && self.removed.is_empty()
    }

    /// Serialises this registry into canonical, deterministic TOML so equal [`DevicesFile`] values
    /// always produce identical bytes and identical BLAKE3 hashes.
    pub fn to_toml_string(&self) -> String {
        let mut doc = DocumentMut::new();
        doc["version"] = value(i64::from(self.version));

        if !self.devices.is_empty() {
            let mut devices_table = Table::new();
            devices_table.set_implicit(true);
            for (id, dev) in &self.devices {
                let mut t = Table::new();
                t["name"] = value(dev.name.as_str());
                t["public_key"] = value(dev.public_key.as_str());
                t["principal"] = value(dev.principal.as_str());
                if let Some(role) = &dev.role {
                    t["role"] = value(role.as_str());
                }
                t["paired_at"] = value(dev.paired_at.as_str());
                t["paired_on"] = value(dev.paired_on.as_str());
                t["updated_at"] = value(dev.updated_at.as_str());
                t["updated_on"] = value(dev.updated_on.as_str());
                devices_table.insert(id, Item::Table(t));
            }
            doc.insert("devices", Item::Table(devices_table));
        }

        if !self.removed.is_empty() {
            let mut removed_table = Table::new();
            removed_table.set_implicit(true);
            for (id, tomb) in &self.removed {
                let mut t = Table::new();
                t["name"] = value(tomb.name.as_str());
                t["principal"] = value(tomb.principal.as_str());
                if let Some(paired_at) = &tomb.paired_at {
                    t["paired_at"] = value(paired_at.as_str());
                }
                if let Some(paired_on) = &tomb.paired_on {
                    t["paired_on"] = value(paired_on.as_str());
                }
                t["removed_at"] = value(tomb.removed_at.as_str());
                t["removed_on"] = value(tomb.removed_on.as_str());
                removed_table.insert(id, Item::Table(t));
            }
            doc.insert("removed", Item::Table(removed_table));
        }

        let mut text = doc.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text
    }

    /// Computes the `SHA256:...` fingerprint of `entry.public_key`, if valid.
    pub fn fingerprint_of(entry: &FleetDevice) -> String {
        entry
            .public_key
            .split_whitespace()
            .nth(1)
            .and_then(fingerprint_key_b64)
            .unwrap_or_else(|| "unknown".to_owned())
    }
}

/// Formats `time` as RFC 3339 UTC, keeping sub-second precision when non-zero so rapid edits
/// remain strictly ordered.
pub fn format_precise_rfc3339(time: Timestamp) -> String {
    time.to_string()
}

/// Returns a current RFC 3339 UTC timestamp strictly greater than `previous` (if present and
/// parseable), preventing same-second ties during rapid local updates.
pub fn next_updated_timestamp(previous: Option<&str>) -> String {
    let now = Timestamp::now();
    let chosen = match previous.and_then(|s| s.parse::<Timestamp>().ok()) {
        Some(prev) if now <= prev => prev
            .checked_add(SignedDuration::from_nanos(1))
            .unwrap_or(now),
        _ => now,
    };
    format_precise_rfc3339(chosen)
}

fn timestamp_sort_key(raw: &str) -> (bool, i128, &str) {
    match raw.parse::<Timestamp>() {
        Ok(ts) => (true, ts.as_nanosecond(), raw),
        Err(_) => (false, 0, raw),
    }
}

fn tombstone_sort_key(t: &DeviceTombstone) -> impl Ord + '_ {
    (
        timestamp_sort_key(&t.removed_at),
        t.removed_on.as_str(),
        t.name.as_str(),
        t.principal.as_str(),
        t.paired_at.as_deref().unwrap_or(""),
        t.paired_on.as_deref().unwrap_or(""),
    )
}

fn device_sort_key(d: &FleetDevice) -> impl Ord + '_ {
    (
        timestamp_sort_key(&d.updated_at),
        d.updated_on.as_str(),
        d.name.as_str(),
        d.role.as_deref().unwrap_or(""),
        d.principal.as_str(),
        d.public_key.as_str(),
        d.paired_at.as_str(),
        d.paired_on.as_str(),
    )
}

/// Pure CRDT merge of two [`DevicesFile`] states.
///
/// Semilattice join laws guaranteed for all valid inputs:
/// - **Commutative**: `merge_devices(a, b) == merge_devices(b, a)`
/// - **Associative**: `merge_devices(merge_devices(a, b), c) == merge_devices(a, merge_devices(b, c))`
/// - **Idempotent**: `merge_devices(a, a) == a`
/// - **Tombstone dominance**: an `id` present in `removed` on either side is always in `removed`
///   and never in `devices` (tombstones can never be resurrected or dropped).
/// - **Live edit ordering**: when both sides hold a live entry for the same `id`, the entry with
///   the newer `updated_at` wins (with a deterministic total-order tie-break).
pub fn merge_devices(a: &DevicesFile, b: &DevicesFile) -> DevicesFile {
    let mut all_ids: BTreeSet<&str> = BTreeSet::new();
    for id in a
        .devices
        .keys()
        .chain(a.removed.keys())
        .chain(b.devices.keys())
        .chain(b.removed.keys())
    {
        all_ids.insert(id.as_str());
    }

    let mut out = DevicesFile {
        version: DEVICES_VERSION,
        devices: BTreeMap::new(),
        removed: BTreeMap::new(),
    };

    for id in all_ids {
        match (a.removed.get(id), b.removed.get(id)) {
            (Some(ta), Some(tb)) => {
                let winner = if tombstone_sort_key(ta) <= tombstone_sort_key(tb) {
                    ta.clone()
                } else {
                    tb.clone()
                };
                out.removed.insert(id.to_owned(), winner);
            }
            (Some(ta), None) => {
                out.removed.insert(id.to_owned(), ta.clone());
            }
            (None, Some(tb)) => {
                out.removed.insert(id.to_owned(), tb.clone());
            }
            (None, None) => match (a.devices.get(id), b.devices.get(id)) {
                (Some(da), Some(db)) => {
                    let winner = if device_sort_key(da) >= device_sort_key(db) {
                        da.clone()
                    } else {
                        db.clone()
                    };
                    out.devices.insert(id.to_owned(), winner);
                }
                (Some(da), None) => {
                    out.devices.insert(id.to_owned(), da.clone());
                }
                (None, Some(db)) => {
                    out.devices.insert(id.to_owned(), db.clone());
                }
                (None, None) => {}
            },
        }
    }

    out
}

/// Lock path guarding shared configuration writes (`policy.toml` and `devices.toml`), matching
/// `crates/legion-net/src/sync.rs`.
pub fn config_lock_path(paths: &Paths) -> PathBuf {
    paths.state_dir().join("sync").join("config.lock")
}

/// Acquires the exclusive shared-configuration lock (`state/sync/config.lock`).
///
/// Global lock ordering: always acquire `lock_config` BEFORE `lock_auth_keys` when both are held.
pub fn lock_config(paths: &Paths) -> Result<FileLock, ErrorBody> {
    let lock = config_lock_path(paths);
    if let Some(parent) = lock.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    FileLock::acquire_timeout(&lock, LockMode::Exclusive, CONFIG_LOCK_TIMEOUT).map_err(|error| {
        match error {
            OsError::TimedOut { .. } => ErrorBody::new(
                ErrorCode::Busy,
                "Another call has been updating shared configuration for too long.",
            )
            .with_hint("Try again in a moment."),
            other => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be locked: {other}.", lock.display()),
            ),
        }
    })
}

/// Reads and strictly validates `config/devices.toml`, returning `Ok(None)` when absent.
pub fn load_devices_raw(paths: &Paths) -> Result<Option<DevicesFile>, ErrorBody> {
    let path = paths.config_file(DevicesFile::FILE);
    match std::fs::read(&path) {
        Ok(bytes) => parse::<DevicesFile>(&path, &bytes).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be read: {error}.", path.display()),
        )
        .with_hint("Make devices.toml readable by the account legion runs as.")),
    }
}

/// Validates, backs up, and atomically writes `config/devices.toml`. Caller must hold
/// [`lock_config`]. Returns the canonical TOML text written.
pub fn save_devices_locked(paths: &Paths, devices: &DevicesFile) -> Result<String, ErrorBody> {
    let target = paths.config_file(DevicesFile::FILE);
    let text = devices.to_toml_string();
    parse::<DevicesFile>(&target, text.as_bytes())?;

    if let Ok(existing) = std::fs::read(&target) {
        let backup = paths
            .state_dir()
            .join("sync")
            .join("backups")
            .join("devices.toml.bak");
        if let Some(parent) = backup.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be created: {error}.", parent.display()),
                )
            })?;
        }
        write_atomically(&backup, &existing).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Backup {} cannot be written: {error}.", backup.display()),
            )
        })?;
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    write_atomically(&target, text.as_bytes()).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", target.display()),
        )
    })?;
    Ok(text)
}

/// Result of an atomic `policy.toml` principal update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyEditOutcome {
    /// Whether `policy.toml` on disk actually changed.
    pub changed: bool,
    /// Previous role assigned to `principal`, if it existed before.
    pub previous_role: Option<String>,
}

/// Inserts or updates `[principals."<principal>"] role = "<role>"` in `policy.toml`, preserving
/// existing comments and layout, validating with `parse::<Policy>` before writing, backing up the
/// previous file, and replacing it atomically. Caller must hold [`lock_config`].
pub fn upsert_policy_principal_locked(
    paths: &Paths,
    principal: &str,
    role: &str,
) -> Result<PolicyEditOutcome, ErrorBody> {
    Principal::parse(principal)?;
    validate_role_name(role)?;
    if principal == OWNER {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Built-in principal `owner` cannot be edited in policy.toml.",
        )
        .with_hint("Give the device its own principal (for example `device:<name>`)."));
    }

    let path = paths.config_file(Policy::FILE);
    let existing_bytes = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            ));
        }
    };

    let (previous_policy, mut doc) = match &existing_bytes {
        Some(bytes) => {
            let parsed = parse::<Policy>(&path, bytes)?;
            let text = std::str::from_utf8(bytes).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} is not valid UTF-8 text: {error}.", path.display()),
                )
            })?;
            let doc = text.parse::<DocumentMut>().map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be edited as TOML: {error}.", path.display()),
                )
            })?;
            (parsed, doc)
        }
        None => {
            let mut doc = DocumentMut::new();
            doc["default"] = value(OWNER);
            (Policy::default(), doc)
        }
    };

    let previous_role = previous_policy
        .principals
        .iter()
        .find(|(p, _)| p.as_str() == principal)
        .map(|(_, entry)| entry.role.clone());

    if previous_role.as_deref() == Some(role) && existing_bytes.is_some() {
        return Ok(PolicyEditOutcome {
            changed: false,
            previous_role,
        });
    }

    let known_role = role == OWNER || role == NOBODY || previous_policy.roles.contains_key(role);
    if !known_role {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("policy.toml does not define role {role:?}."),
        )
        .with_hint(
            "Define [roles.<name>] in policy.toml (or run `legion init` for starter roles), or pass `owner` with allow_owner=true.",
        ));
    }

    if !doc.contains_key("principals") {
        let mut principals_table = Table::new();
        principals_table.set_implicit(true);
        doc.insert("principals", Item::Table(principals_table));
    }
    let Some(principals_table) = doc.get_mut("principals").and_then(Item::as_table_like_mut) else {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} has a non-table `principals` entry.", path.display()),
        ));
    };

    let mut entry = Table::new();
    entry["role"] = value(role);
    principals_table.insert(principal, Item::Table(entry));

    let mut updated_text = doc.to_string();
    if !updated_text.ends_with('\n') {
        updated_text.push('\n');
    }

    // Strictly validate with Policy's own parser before touching backup or policy.toml.
    parse::<Policy>(&path, updated_text.as_bytes())?;
    write_policy_with_backup(paths, &path, existing_bytes.as_deref(), &updated_text)?;

    Ok(PolicyEditOutcome {
        changed: true,
        previous_role,
    })
}

/// Removes `[principals."<principal>"]` from `policy.toml` if present, keeping all other entries
/// and comments intact. Caller must hold [`lock_config`].
pub fn remove_policy_principal_locked(paths: &Paths, principal: &str) -> Result<bool, ErrorBody> {
    if principal == OWNER {
        return Ok(false);
    }
    let path = paths.config_file(Policy::FILE);
    let existing_bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            ));
        }
    };
    let parsed = parse::<Policy>(&path, &existing_bytes)?;
    if parsed
        .default
        .as_ref()
        .is_some_and(|d| d.as_str() == principal)
    {
        return Ok(false);
    }
    if !parsed
        .principals
        .keys()
        .any(|known| known.as_str() == principal)
    {
        return Ok(false);
    }
    let text = std::str::from_utf8(&existing_bytes).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} is not valid UTF-8 text: {error}.", path.display()),
        )
    })?;
    let mut doc = text.parse::<DocumentMut>().map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be edited as TOML: {error}.", path.display()),
        )
    })?;
    if let Some(principals) = doc.get_mut("principals").and_then(Item::as_table_like_mut) {
        principals.remove(principal);
    }
    let mut updated_text = doc.to_string();
    if !updated_text.ends_with('\n') {
        updated_text.push('\n');
    }
    parse::<Policy>(&path, updated_text.as_bytes())?;
    write_policy_with_backup(paths, &path, Some(&existing_bytes), &updated_text)?;
    Ok(true)
}

fn write_policy_with_backup(
    paths: &Paths,
    policy_path: &Path,
    existing_bytes: Option<&[u8]>,
    updated_text: &str,
) -> Result<(), ErrorBody> {
    if let Some(old_bytes) = existing_bytes {
        let backup = paths
            .state_dir()
            .join("sync")
            .join("backups")
            .join("policy.toml.bak");
        if let Some(parent) = backup.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be created: {error}.", parent.display()),
                )
            })?;
        }
        write_atomically(&backup, old_bytes).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Backup {} cannot be written: {error}.", backup.display()),
            )
        })?;
    }
    if let Some(parent) = policy_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", parent.display()),
            )
        })?;
    }
    write_atomically(policy_path, updated_text.as_bytes()).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", policy_path.display()),
        )
    })
}

/// Reconciles `device:*` principals in `policy.toml` to match the live entries in `devices`.
///
/// Security invariants:
/// 1. Only principals starting with `device:` are ever touched; non-device principals are never
///    created, changed, or removed here.
/// 2. Every live `device:*` entry that carries a recorded `role` sets `[principals."device:..."]`
///    to that exact role (never leaving a stale wider role on the receiver, and never elevating).
/// 3. If a recorded role is not defined in this node's `policy.toml` (and is not `owner` or
///    `nobody`), or if two live devices collide on the same `device:*` principal with different
///    recorded roles, the principal fails closed to `nobody`.
/// 4. An entry with `role: None` (such as an adopted legacy line without a recorded role) never
///    creates or elevates a principal.
/// 5. A tombstoned `device:*` principal not referenced by any live device or active token is
///    removed from `policy.toml`.
///
/// Caller must hold [`lock_config`]. Returns any warnings (for example unknown roles set to
/// `nobody`).
pub fn reconcile_device_principals_locked(
    paths: &Paths,
    devices: &DevicesFile,
) -> Result<Vec<String>, ErrorBody> {
    let policy_path = paths.config_file(Policy::FILE);
    let policy = match std::fs::read(&policy_path) {
        Ok(bytes) => parse::<Policy>(&policy_path, &bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Policy::default(),
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", policy_path.display()),
            ));
        }
    };

    let tokens_path = paths.config_file(Tokens::FILE);
    let token_principals: HashSet<String> = match std::fs::read(&tokens_path) {
        Ok(bytes) => parse::<Tokens>(&tokens_path, &bytes)
            .map(|t| t.tokens.values().map(|e| e.principal.to_string()).collect())
            .unwrap_or_default(),
        Err(_) => HashSet::new(),
    };

    // Group live `device:*` entries by principal to detect conflicting offline role assignments.
    let mut live_by_principal: BTreeMap<&str, Vec<Option<&str>>> = BTreeMap::new();
    for entry in devices.devices.values() {
        if entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
            live_by_principal
                .entry(entry.principal.as_str())
                .or_default()
                .push(entry.role.as_deref());
        }
    }

    let mut warnings = Vec::new();
    for (principal, roles) in &live_by_principal {
        let non_none: BTreeSet<&str> = roles.iter().copied().flatten().collect();
        if non_none.is_empty() {
            // Adopted legacy entry without a recorded role: never invent or elevate a principal.
            continue;
        }
        let target_role = if non_none.len() > 1 || roles.iter().any(Option::is_none) {
            warnings.push(format!(
                "Principal {principal} is shared by live devices with conflicting roles; setting it to `nobody` until disambiguated."
            ));
            NOBODY
        } else if let Some(&recorded) = non_none.iter().next() {
            let known =
                recorded == OWNER || recorded == NOBODY || policy.roles.contains_key(recorded);
            if known {
                recorded
            } else {
                warnings.push(format!(
                    "Role {recorded:?} for {principal} is not defined in this node's policy.toml; setting {principal} to `nobody`."
                ));
                NOBODY
            }
        } else {
            continue;
        };

        upsert_policy_principal_locked(paths, principal, target_role)?;
    }

    // Clean up tombstoned `device:*` principals not referenced by any live device or HTTP token.
    for tomb in devices.removed.values() {
        if tomb.principal.starts_with(DEVICE_PRINCIPAL_PREFIX)
            && !live_by_principal.contains_key(tomb.principal.as_str())
            && !token_principals.contains(&tomb.principal)
        {
            let _ = remove_policy_principal_locked(paths, &tomb.principal)?;
        }
    }

    Ok(warnings)
}

/// Checks whether `auth_keys` contains `wire_blob` on any foreign or invite line (or another home's
/// device line), so reconciling a synced device never collides with an existing non-managed key.
fn contains_non_owned_key_blob(
    auth_keys: &AuthKeysFile,
    wire_blob: &[u8],
    paths: &Paths,
    caller_account: Option<&str>,
    target: &AuthKeysTarget,
) -> bool {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    for line in &auth_keys.lines {
        let is_this_homes_device = matches!(
            &line.kind,
            LineKind::Device { home, account, .. }
                if line_matches_home(home.as_ref(), paths)
                    && line_matches_account(
                        account.as_deref(),
                        caller_account,
                        target.windows_admin_acl,
                        cfg!(windows),
                    )
        );
        if is_this_homes_device {
            continue;
        }
        let key_matches = match &line.kind {
            LineKind::Device { key_b64, .. } | LineKind::Invite { key_b64, .. } => {
                STANDARD.decode(key_b64).ok().as_deref() == Some(wire_blob)
            }
            LineKind::Foreign => {
                // Delegate foreign-line blob extraction to a one-line AuthKeysFile check.
                AuthKeysFile::parse(&line.raw).contains_key_blob(wire_blob)
            }
        };
        if key_matches {
            return true;
        }
    }
    false
}

/// Adopts legacy `--pairing <id>` device lines belonging to THIS home from `authorized_keys` into
/// a [`DevicesFile`] during first-run migration (when `devices.toml` does not exist yet).
pub fn adopt_legacy_devices_from_auth_keys(
    paths: &Paths,
    node_name: &str,
    auth_keys: &AuthKeysFile,
    target: &AuthKeysTarget,
    caller_account: Option<&str>,
    state: &DevicesState,
    policy: &Policy,
) -> DevicesFile {
    let mut file = DevicesFile::default();
    let now_str = format_precise_rfc3339(Timestamp::now());
    let valid_node = NodeName::try_from(node_name.to_owned())
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "node".to_owned());

    for line in &auth_keys.lines {
        let LineKind::Device {
            name,
            key_b64,
            principal,
            pairing_id,
            home,
            account,
        } = &line.kind
        else {
            continue;
        };
        if !line_matches_account(
            account.as_deref(),
            caller_account,
            target.windows_admin_acl,
            cfg!(windows),
        ) {
            continue;
        }
        let is_this_home = line_matches_home(home.as_ref(), paths)
            || (home.is_none() && state.devices.contains_key(name));
        if !is_this_home {
            continue;
        }
        if validate_pairing_id(pairing_id).is_err()
            || validate_device_label(name).is_err()
            || Principal::parse(principal).is_err()
        {
            continue;
        }
        let raw_key = format!("ssh-ed25519 {key_b64}");
        let Ok(validated_key) = validate_ed25519_public_key(&raw_key) else {
            continue;
        };
        let paired_at = state
            .devices
            .get(name)
            .and_then(|meta| {
                meta.added
                    .parse::<Timestamp>()
                    .ok()
                    .map(|_| meta.added.clone())
            })
            .unwrap_or_else(|| now_str.clone());
        let role = if principal.starts_with(DEVICE_PRINCIPAL_PREFIX)
            && policy.principals.keys().any(|p| p.as_str() == principal)
        {
            Some(policy.role_of(principal).0)
        } else {
            None
        };
        file.devices
            .entry(pairing_id.clone())
            .or_insert_with(|| FleetDevice {
                name: name.clone(),
                public_key: validated_key.canonical,
                principal: principal.clone(),
                role,
                paired_at: paired_at.clone(),
                paired_on: valid_node.clone(),
                updated_at: paired_at,
                updated_on: valid_node.clone(),
            });
    }

    file
}

/// Reconciles this home's device lines in `auth_keys` to match `devices`.
///
/// Foreign lines, pending invites, and device lines belonging to another Legion home or another OS
/// account are preserved byte for byte. Returns `(auth_keys_changed, removed_device_names)`.
#[allow(clippy::too_many_arguments)]
pub fn reconcile_authorized_keys_in_memory(
    auth_keys: &mut AuthKeysFile,
    devices: &DevicesFile,
    paths: &Paths,
    target: &AuthKeysTarget,
    caller_account: Option<&str>,
    exe: &Path,
    shell: ForcedCommandShell,
) -> Result<(bool, Vec<String>), ErrorBody> {
    let pinned_home = PinnedHome::from_paths(paths);
    let pinned_account = if target.windows_admin_acl {
        let Some(acct) = caller_account else {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                "Cannot pin OS account on shared administrators_authorized_keys because the current account name is unavailable.",
            ));
        };
        Some(acct)
    } else {
        None
    };

    // Build expected canonical line per live pairing id so we can detect exact matches without
    // unnecessary rewrites.
    let mut expected_lines: BTreeMap<&str, (String, Vec<u8>)> = BTreeMap::new();
    let mut seen_wire_blobs: HashSet<Vec<u8>> = HashSet::new();
    for (id, entry) in &devices.devices {
        let validated = validate_ed25519_public_key(&entry.public_key)?;
        if contains_non_owned_key_blob(
            auth_keys,
            &validated.wire_blob,
            paths,
            caller_account,
            target,
        ) {
            continue;
        }
        if !seen_wire_blobs.insert(validated.wire_blob.to_vec()) {
            continue;
        }
        let line = format_device_line(
            exe,
            &entry.principal,
            &validated.canonical,
            &entry.name,
            id,
            &pinned_home,
            pinned_account,
            shell,
        )?;
        expected_lines.insert(id.as_str(), (line, validated.wire_blob.to_vec()));
    }

    let mut kept_ids: HashSet<String> = HashSet::new();
    let mut removed_device_names: BTreeSet<String> = BTreeSet::new();

    let mut changed = auth_keys.retain(|line| {
        let LineKind::Device {
            name,
            key_b64,
            principal,
            pairing_id,
            home,
            account,
        } = &line.kind
        else {
            return true;
        };
        if !line_matches_account(
            account.as_deref(),
            caller_account,
            target.windows_admin_acl,
            cfg!(windows),
        ) {
            return true;
        }
        if !line_matches_home(home.as_ref(), paths) {
            return true;
        }

        if let Some(entry) = devices.devices.get(pairing_id.as_str())
            && let Some((expected_raw, _)) = expected_lines.get(pairing_id.as_str())
        {
            let expected_key_b64 = entry.public_key.split_whitespace().nth(1).unwrap_or("");
            let matches_entry = entry.name == *name
                && entry.principal == *principal
                && expected_key_b64 == key_b64
                && line.raw_trimmed_str() == Some(expected_raw.as_str());
            if matches_entry && kept_ids.insert(pairing_id.clone()) {
                return true;
            }
        }

        removed_device_names.insert(name.clone());
        false
    });

    let mut needs_append: Vec<&str> = Vec::new();
    for (id, (expected_line, _)) in &expected_lines {
        if !kept_ids.contains(*id) {
            needs_append.push(expected_line.as_str());
        }
    }

    if !needs_append.is_empty() {
        verify_exe_not_group_or_world_writable(exe)?;
        for line in needs_append {
            auth_keys.append_line(line);
        }
        changed = true;
    }

    Ok((changed, removed_device_names.into_iter().collect()))
}

/// Outcome of loading (with first-run adoption) and reconciling `devices.toml` and
/// `authorized_keys`.
#[derive(Clone, Debug)]
pub struct ReconcileOutcome {
    /// Authoritative [`DevicesFile`] after any first-run adoption.
    pub devices: DevicesFile,
    /// Number of legacy `--pairing` lines adopted from `authorized_keys`.
    pub devices_adopted: usize,
    /// Whether `authorized_keys` was rewritten.
    pub auth_keys_changed: bool,
    /// Device names whose `authorized_keys` lines were removed during reconciliation (callers
    /// should terminate active sessions for these names).
    pub removed_device_names: Vec<String>,
}

/// Loads `devices.toml` (adopting legacy `--pairing` lines from `authorized_keys` if `devices.toml`
/// does not exist yet) and reconciles `authorized_keys` to match.
///
/// When `devices.toml` is already up to date and `authorized_keys` matches, this writes neither
/// file. Caller must NOT already hold `lock_config` or `lock_auth_keys`.
pub fn reconcile_devices_and_auth_keys_blocking(
    paths: &Paths,
    node_name: &str,
    node_file: &NodeFile,
    save_if_adopted: bool,
) -> Result<ReconcileOutcome, ErrorBody> {
    let _cfg_guard = lock_config(paths)?;
    reconcile_devices_and_auth_keys_with_config_lock(paths, node_name, node_file, save_if_adopted)
}

/// Same as [`reconcile_devices_and_auth_keys_blocking`], for callers already holding
/// [`lock_config`].
pub fn reconcile_devices_and_auth_keys_with_config_lock(
    paths: &Paths,
    node_name: &str,
    node_file: &NodeFile,
    save_if_adopted: bool,
) -> Result<ReconcileOutcome, ErrorBody> {
    let target = resolve_target(paths, node_file)?;
    let caller_account = caller_account_for_target(&target)?;
    let write_path = resolve_safe_write_path(&target, paths.home())?;
    verify_owner_matches_euid(
        &write_path,
        paths.home(),
        target.is_default_home_ssh,
        legion_os::user::effective_uid(),
    )?;
    let _auth_guard = lock_auth_keys(&target, &write_path)?;

    let existing_devices = load_devices_raw(paths)?;
    let (devices, devices_adopted) = match existing_devices {
        Some(file) => (file, 0),
        None => {
            let auth_keys = read_auth_keys(&write_path)?;
            let state = load_state(paths).unwrap_or_default();
            let policy_path = paths.config_file(Policy::FILE);
            let policy = std::fs::read(&policy_path)
                .ok()
                .and_then(|b| parse::<Policy>(&policy_path, &b).ok())
                .unwrap_or_default();
            let adopted = adopt_legacy_devices_from_auth_keys(
                paths,
                node_name,
                &auth_keys,
                &target,
                caller_account.as_deref(),
                &state,
                &policy,
            );
            let count = adopted.devices.len();
            if count > 0 || save_if_adopted {
                save_devices_locked(paths, &adopted)?;
            }
            (adopted, count)
        }
    };

    let exe = legion_core::current_exe()?;
    let shell = ForcedCommandShell::host()?;
    let (auth_keys_changed, removed_device_names) =
        edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
            let (changed, removed) = reconcile_authorized_keys_in_memory(
                &mut auth_keys,
                &devices,
                paths,
                &target,
                caller_account.as_deref(),
                &exe,
                shell,
            )?;
            Ok((changed.then_some(auth_keys), (changed, removed)))
        })?;

    Ok(ReconcileOutcome {
        devices,
        devices_adopted,
        auth_keys_changed,
        removed_device_names,
    })
}

/// Verifies a `legion gate --as <principal> --device <name> --pairing <id>` session against
/// `devices.toml` (adopting legacy devices on first run if `devices.toml` is absent) and keeps
/// `authorized_keys` reconciled with `devices.toml`.
///
/// Returns `Err(ErrorCode::Denied)` ONLY when the device/pairing is definitively tombstoned,
/// missing from `devices.toml`, or mismatched in name/principal/account (so `legion gate` ends the
/// parent `sshd` connection). Transient I/O or config parse failures return non-`Denied` errors so
/// the parent connection is left intact.
pub fn verify_gate_device(
    paths: &Paths,
    node_name: &str,
    node_file: &NodeFile,
    device_name: &str,
    pairing_id: &str,
    principal: &str,
    account: Option<&str>,
) -> Result<Vec<String>, ErrorBody> {
    let target = resolve_target(paths, node_file)?;
    let write_path = resolve_safe_write_path(&target, paths.home())?;
    if target.windows_admin_acl {
        let current = legion_os::user::current_username().map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Cannot determine current OS account for gate authorization check: {error}."
                ),
            )
        })?;
        if !line_matches_account(account, Some(&current), true, cfg!(windows)) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Device {device_name} is no longer authorized as {principal} for this account."
                ),
            ));
        }
    }

    let loaded = load_devices_raw(paths)?;
    let devices = match loaded {
        Some(file) => file,
        None => {
            // First-run migration: adopt pre-existing `--pairing` lines if any exist.
            let outcome =
                reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false)?;
            outcome.devices
        }
    };

    if devices.removed.contains_key(pairing_id) {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Device {device_name} is no longer authorized as {principal} (it was removed)."
            ),
        ));
    }

    let Some(entry) = devices.devices.get(pairing_id) else {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("Device {device_name} is no longer authorized as {principal}."),
        ));
    };

    if entry.name != device_name || entry.principal != principal {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("Device {device_name} is no longer authorized as {principal}."),
        ));
    }

    // Hot path: check read-only whether authorized_keys already matches devices.toml.
    let caller_account = caller_account_for_target(&target)?;
    let auth_keys = read_auth_keys(&write_path)?;
    let exe = legion_core::current_exe()?;
    let shell = ForcedCommandShell::host()?;
    let mut probe = auth_keys;
    let (needs_write, _) = reconcile_authorized_keys_in_memory(
        &mut probe,
        &devices,
        paths,
        &target,
        caller_account.as_deref(),
        &exe,
        shell,
    )?;
    if needs_write {
        let outcome = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false)?;
        return Ok(outcome.removed_device_names);
    }

    Ok(Vec::new())
}

/// Per-machine timeout when automatically pushing `devices.toml` changes to the fleet.
pub const DEVICE_PUSH_TIMEOUT_S: f64 = 5.0;

/// One change introduced by merging an incoming [`DevicesFile`] into this node's local registry,
/// passed to the caller's authorization check before anything is written.
#[derive(Clone, Copy, Debug)]
pub enum MergeChange<'a> {
    /// A live device entry was added or modified.
    Live {
        /// Resulting live device entry after the merge.
        entry: &'a FleetDevice,
        /// Previous local entry for this pairing id, if any.
        previous: Option<&'a FleetDevice>,
    },
    /// A tombstone was added or updated (possibly revoking a previously live device).
    Removed {
        /// Resulting tombstone after the merge.
        tombstone: &'a DeviceTombstone,
        /// Previously live local entry revoked by this tombstone, if any.
        previous_live: Option<&'a FleetDevice>,
    },
}

/// Result of merging an incoming [`DevicesFile`] into this node's registry and reconciling
/// `policy.toml` and `authorized_keys`.
#[derive(Clone, Debug)]
pub struct MergeApplyOutcome {
    /// Merged [`DevicesFile`] written to `devices.toml`.
    pub devices: DevicesFile,
    /// Canonical TOML text written to `devices.toml`.
    pub canonical_toml: String,
    /// Raw bytes of `devices.toml` before this merge, or `None` if it did not exist yet.
    pub bytes_before: Option<Vec<u8>>,
    /// Backup path created when `devices.toml` previously existed.
    pub backup: Option<String>,
    /// Device names whose `authorized_keys` lines were removed or that just became tombstoned
    /// (callers should terminate live sessions for these names).
    pub removed_device_names: Vec<String>,
    /// Warnings generated during `device:*` principal reconciliation (for example unknown roles
    /// set to `nobody`).
    pub warnings: Vec<String>,
}

/// Read-only view of `devices.toml` for `config.read` and `fleet.sync`.
///
/// If `devices.toml` already exists on disk, returns its exact UTF-8 text. Otherwise, inspects
/// `authorized_keys` read-only (never creating `.ssh/` or writing files) and returns canonical
/// TOML for any legacy `--pairing` lines belonging to this Legion home, or `Ok(None)` when none
/// exist.
pub fn read_devices_for_sync_blocking(
    paths: &Paths,
    node_name: &str,
    node_file: Option<&NodeFile>,
) -> Result<Option<String>, ErrorBody> {
    let devices_path = paths.config_file(DevicesFile::FILE);
    match std::fs::read(&devices_path) {
        Ok(bytes) => {
            let text = String::from_utf8(bytes).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{} is not valid UTF-8 text: {error}.",
                        devices_path.display()
                    ),
                )
            })?;
            return Ok(Some(text));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", devices_path.display()),
            ));
        }
    }

    let Some(node_file) = node_file else {
        return Ok(None);
    };
    let Ok(target) = resolve_target(paths, node_file) else {
        return Ok(None);
    };
    let Ok(write_path) = resolve_safe_write_path(&target, paths.home()) else {
        return Ok(None);
    };
    if !write_path.is_file() {
        return Ok(None);
    }
    let Ok(caller_account) = caller_account_for_target(&target) else {
        return Ok(None);
    };
    let Ok(auth_keys) = read_auth_keys(&write_path) else {
        return Ok(None);
    };
    let state = load_state(paths).unwrap_or_default();
    let policy_path = paths.config_file(Policy::FILE);
    let policy = std::fs::read(&policy_path)
        .ok()
        .and_then(|b| parse::<Policy>(&policy_path, &b).ok())
        .unwrap_or_default();
    let adopted = adopt_legacy_devices_from_auth_keys(
        paths,
        node_name,
        &auth_keys,
        &target,
        caller_account.as_deref(),
        &state,
        &policy,
    );
    if adopted.devices.is_empty() {
        Ok(None)
    } else {
        Ok(Some(adopted.to_toml_string()))
    }
}

/// Applies an incoming `DevicesFile` (e.g. from `fleet.sync` or `config.write`) by merging it with
/// this node's current `devices.toml` (after adopting any legacy lines), verifying preconditions
/// and caller authorization, updating `device:*` principals in `policy.toml` without escalation,
/// saving the merged `devices.toml`, and reconciling `authorized_keys`.
pub fn apply_devices_merge_blocking(
    paths: &Paths,
    node_name: &str,
    node_file: &NodeFile,
    incoming: &DevicesFile,
    check_precondition: impl FnOnce(Option<&[u8]>) -> Result<(), ErrorBody>,
    authorize_change: impl Fn(MergeChange<'_>) -> Result<(), ErrorBody>,
) -> Result<MergeApplyOutcome, ErrorBody> {
    let _cfg_guard = lock_config(paths)?;

    let target = resolve_target(paths, node_file)?;
    let caller_account = caller_account_for_target(&target)?;
    let write_path = resolve_safe_write_path(&target, paths.home())?;
    verify_owner_matches_euid(
        &write_path,
        paths.home(),
        target.is_default_home_ssh,
        legion_os::user::effective_uid(),
    )?;
    let _auth_guard = lock_auth_keys(&target, &write_path)?;

    let devices_path = paths.config_file(DevicesFile::FILE);
    let bytes_before = match std::fs::read(&devices_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", devices_path.display()),
            ));
        }
    };
    check_precondition(bytes_before.as_deref())?;

    let had_existing_file = bytes_before.is_some();
    let local = match bytes_before.as_deref() {
        Some(bytes) => parse::<DevicesFile>(&devices_path, bytes)?,
        None => {
            let auth_keys = read_auth_keys(&write_path)?;
            let state = load_state(paths).unwrap_or_default();
            let policy_path = paths.config_file(Policy::FILE);
            let policy = std::fs::read(&policy_path)
                .ok()
                .and_then(|b| parse::<Policy>(&policy_path, &b).ok())
                .unwrap_or_default();
            adopt_legacy_devices_from_auth_keys(
                paths,
                node_name,
                &auth_keys,
                &target,
                caller_account.as_deref(),
                &state,
                &policy,
            )
        }
    };

    let merged = merge_devices(&local, incoming);

    // Authorize every live entry and tombstone that the merge adds or modifies before writing.
    for (id, entry) in &merged.devices {
        let previous = local.devices.get(id);
        if previous != Some(entry) {
            authorize_change(MergeChange::Live { entry, previous })?;
        }
    }
    for (id, tombstone) in &merged.removed {
        let previous_tomb = local.removed.get(id);
        let previous_live = local.devices.get(id);
        if previous_tomb != Some(tombstone) || previous_live.is_some() {
            authorize_change(MergeChange::Removed {
                tombstone,
                previous_live,
            })?;
        }
    }

    let warnings = reconcile_device_principals_locked(paths, &merged)?;
    let canonical = save_devices_locked(paths, &merged)?;
    let backup = had_existing_file.then(|| {
        paths
            .state_dir()
            .join("sync")
            .join("backups")
            .join("devices.toml.bak")
            .display()
            .to_string()
    });

    let exe = legion_core::current_exe()?;
    let shell = ForcedCommandShell::host()?;
    let mut removed_device_names =
        edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
            let (changed, removed) = reconcile_authorized_keys_in_memory(
                &mut auth_keys,
                &merged,
                paths,
                &target,
                caller_account.as_deref(),
                &exe,
                shell,
            )?;
            Ok((changed.then_some(auth_keys), removed))
        })?;

    // Also include names of any previously live devices that just became tombstoned so live
    // sessions are terminated even if their authorized_keys line was already gone.
    for (id, prev) in &local.devices {
        if merged.removed.contains_key(id) && !removed_device_names.contains(&prev.name) {
            removed_device_names.push(prev.name.clone());
        }
    }

    Ok(MergeApplyOutcome {
        devices: merged,
        canonical_toml: canonical,
        bytes_before,
        backup,
        removed_device_names,
        warnings,
    })
}

/// Pushes `devices.toml` to every peer listed in `fleet.toml` in parallel with a short timeout,
/// returning a per-machine [`FleetPushOutcome`] map.
///
/// Returns an empty map when `fleet.sync` is not registered or `fleet.toml` lists no other
/// machines. Never fails the already-committed local change when a peer is unreachable.
pub async fn push_devices_to_fleet(
    node: &Node,
    ctx_opt: Option<&Ctx>,
    acting_principal: Option<&str>,
) -> BTreeMap<String, FleetPushOutcome> {
    let Some(sync_tool) = node.registry().get("fleet.sync") else {
        return BTreeMap::new();
    };
    let args = json!({
        "direction": "push",
        "documents": [DevicesFile::FILE],
        "timeout_s": DEVICE_PUSH_TIMEOUT_S,
    });
    let reply = match ctx_opt {
        Some(ctx) => sync_tool.call(ctx, args).await,
        None => {
            let mut call = Call::new(Door::Ssh, "fleet.sync", args);
            call.pinned = Some(acting_principal.unwrap_or(OWNER).to_owned());
            node.call(call).await
        }
    };
    match reply {
        Ok(value) => parse_fleet_push_outcomes(&value),
        Err(_) => BTreeMap::new(),
    }
}

/// Translates a `fleet.sync` result JSON object for `devices.toml` into per-machine
/// [`FleetPushOutcome`] entries.
pub fn parse_fleet_push_outcomes(sync_result: &Value) -> BTreeMap<String, FleetPushOutcome> {
    let mut out = BTreeMap::new();
    let Some(results) = sync_result.get("results").and_then(Value::as_object) else {
        return out;
    };
    for (machine, docs) in results {
        let doc_outcome = docs.get(DevicesFile::FILE);
        let status = doc_outcome
            .and_then(|d| d.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("error");
        let outcome = if matches!(status, "same" | "pushed" | "pulled") {
            FleetPushOutcome::synced()
        } else {
            let reason = doc_outcome
                .and_then(|d| {
                    d.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .or_else(|| d.get("detail").and_then(Value::as_str))
                })
                .unwrap_or("machine did not finish syncing devices.toml");
            FleetPushOutcome::not_reached(reason)
        };
        out.insert(machine.clone(), outcome);
    }
    out
}

fn caller_account_for_target(target: &AuthKeysTarget) -> Result<Option<String>, ErrorBody> {
    match legion_os::user::current_username() {
        Ok(name) => Ok(Some(name)),
        Err(error) if target.windows_admin_acl => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot determine current OS account to scope shared administrators_authorized_keys: {error}."
            ),
        )),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use legion_core::{Access, Paths};

    use super::*;
    use crate::device::keys::ed25519_public_key_from_seed;

    fn sample_id(n: u8) -> String {
        format!("{n:02x}").repeat(16)
    }

    fn sample_key(seed: u8) -> String {
        ed25519_public_key_from_seed(&[seed; 32])
    }

    fn sample_device(
        seed: u8,
        name: &str,
        role: Option<&str>,
        updated_at: &str,
        node: &str,
    ) -> FleetDevice {
        FleetDevice {
            name: name.to_owned(),
            public_key: sample_key(seed),
            principal: format!("device:{name}"),
            role: role.map(str::to_owned),
            paired_at: "2026-11-24T10:00:00Z".to_owned(),
            paired_on: "tower".to_owned(),
            updated_at: updated_at.to_owned(),
            updated_on: node.to_owned(),
        }
    }

    fn sample_tombstone(name: &str, removed_at: &str, node: &str) -> DeviceTombstone {
        DeviceTombstone {
            name: name.to_owned(),
            principal: format!("device:{name}"),
            paired_at: Some("2026-11-24T10:00:00Z".to_owned()),
            paired_on: Some("tower".to_owned()),
            removed_at: removed_at.to_owned(),
            removed_on: node.to_owned(),
        }
    }

    /// Deterministic xorshift64 PRNG for property tests without external dependencies.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
            let idx = (self.next() as usize) % items.len();
            &items[idx]
        }
    }

    fn random_devices_file(rng: &mut Rng) -> DevicesFile {
        let ids: Vec<String> = (1_u8..=6).map(sample_id).collect();
        let names = ["pixel", "ipad", "laptop", "phone", "watch"];
        let roles: [Option<&str>; 4] = [Some("observer"), Some("editor"), Some("operator"), None];
        let nodes = ["tower", "pi", "mac"];
        let times = [
            "2026-11-24T10:00:00Z",
            "2026-11-24T10:00:00.500000000Z",
            "2026-11-24T11:00:00Z",
            "2026-11-24T12:30:00Z",
        ];

        let mut file = DevicesFile::default();
        for (idx, id) in ids.iter().enumerate() {
            match rng.next() % 4 {
                0 => {} // absent
                1 | 2 => {
                    let name = *rng.pick(&names);
                    let role = *rng.pick(&roles);
                    let updated_at = *rng.pick(&times);
                    let updated_on = *rng.pick(&nodes);
                    file.devices.insert(
                        id.clone(),
                        sample_device((idx + 1) as u8, name, role, updated_at, updated_on),
                    );
                }
                _ => {
                    let name = *rng.pick(&names);
                    let removed_at = *rng.pick(&times);
                    let removed_on = *rng.pick(&nodes);
                    file.removed
                        .insert(id.clone(), sample_tombstone(name, removed_at, removed_on));
                }
            }
        }
        file
    }

    #[test]
    fn merge_is_commutative_associative_idempotent_and_tombstone_dominant() {
        let mut rng = Rng(0x5eed_cafe_1234_5678);
        for _ in 0..250 {
            let a = random_devices_file(&mut rng);
            let b = random_devices_file(&mut rng);
            let c = random_devices_file(&mut rng);

            // Round-trip canonical TOML serialization.
            let text_a = a.to_toml_string();
            let parsed_a =
                parse::<DevicesFile>(Path::new("devices.toml"), text_a.as_bytes()).unwrap();
            assert_eq!(parsed_a, a);

            // Commutative.
            let ab = merge_devices(&a, &b);
            let ba = merge_devices(&b, &a);
            assert_eq!(ab, ba, "merge must be commutative");
            assert_eq!(ab.to_toml_string(), ba.to_toml_string());

            // Associative.
            let ab_c = merge_devices(&ab, &c);
            let a_bc = merge_devices(&a, &merge_devices(&b, &c));
            assert_eq!(ab_c, a_bc, "merge must be associative");

            // Idempotent.
            assert_eq!(merge_devices(&a, &a), a, "merge must be idempotent");
            assert_eq!(merge_devices(&ab_c, &ab_c), ab_c);

            // Tombstone dominance and permanence: no tombstoned id ever returns to devices.
            for tomb_id in a
                .removed
                .keys()
                .chain(b.removed.keys())
                .chain(c.removed.keys())
            {
                assert!(
                    ab_c.removed.contains_key(tomb_id),
                    "tombstone {tomb_id} disappeared after merge"
                );
                assert!(
                    !ab_c.devices.contains_key(tomb_id),
                    "tombstoned id {tomb_id} returned to live devices"
                );
            }

            // Validated merged document parses cleanly.
            let merged_text = ab_c.to_toml_string();
            let parsed_merged =
                parse::<DevicesFile>(Path::new("devices.toml"), merged_text.as_bytes()).unwrap();
            assert_eq!(parsed_merged, ab_c);
        }
    }

    #[test]
    fn merge_prefers_newer_edit_and_tombstone_beats_newer_live_edit() {
        let id = sample_id(1);
        let mut a = DevicesFile::default();
        a.devices.insert(
            id.clone(),
            sample_device(
                1,
                "pixel",
                Some("observer"),
                "2026-11-24T10:00:00Z",
                "tower",
            ),
        );
        let mut b = DevicesFile::default();
        b.devices.insert(
            id.clone(),
            sample_device(
                1,
                "pixel-renamed",
                Some("editor"),
                "2026-11-24T11:00:00Z",
                "pi",
            ),
        );

        let merged = merge_devices(&a, &b);
        assert_eq!(merged.devices[&id].name, "pixel-renamed");
        assert_eq!(merged.devices[&id].role.as_deref(), Some("editor"));

        // Even an older tombstone beats a newer live edit.
        let mut c = DevicesFile::default();
        c.removed.insert(
            id.clone(),
            sample_tombstone("pixel", "2026-11-24T10:05:00Z", "mac"),
        );
        let merged_tomb = merge_devices(&b, &c);
        assert!(merged_tomb.devices.is_empty());
        assert!(merged_tomb.removed.contains_key(&id));
    }

    #[test]
    fn devices_toml_validation_rejects_overlap_bad_keys_and_bad_ids() {
        let id = sample_id(1);
        let mut overlap = DevicesFile::default();
        overlap.devices.insert(
            id.clone(),
            sample_device(
                1,
                "pixel",
                Some("observer"),
                "2026-11-24T10:00:00Z",
                "tower",
            ),
        );
        overlap
            .removed
            .insert(id, sample_tombstone("pixel", "2026-11-24T11:00:00Z", "pi"));
        assert!(overlap.validate().is_err());

        let bad_key = format!(
            "version = 1\n[devices.{}]\nname = \"pixel\"\npublic_key = \"restrict {}\"\nprincipal = \"device:pixel\"\npaired_at = \"2026-11-24T10:00:00Z\"\npaired_on = \"tower\"\nupdated_at = \"2026-11-24T10:00:00Z\"\nupdated_on = \"tower\"\n",
            sample_id(2),
            sample_key(2)
        );
        assert!(parse::<DevicesFile>(Path::new("devices.toml"), bad_key.as_bytes()).is_err());
    }

    #[test]
    fn reconcile_device_principals_never_escalates_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[roles.editor]
allow = [\"effect:read\", \"effect:write\"]

# Stale leftover owner entry for device:pixel must be downgraded when syncing an observer device!
[principals.\"device:pixel\"]
role = \"owner\"

# Custom non-device principal must never be touched by device reconciliation.
[principals.phone]
role = \"editor\"
",
        )
        .unwrap();

        let mut devices = DevicesFile::default();
        // 1. device:pixel recorded as observer -> must downgrade stale owner to observer.
        devices.devices.insert(
            sample_id(1),
            sample_device(
                1,
                "pixel",
                Some("observer"),
                "2026-11-24T10:00:00Z",
                "tower",
            ),
        );
        // 2. device:missingrole has role not defined on this node -> must fail closed to nobody.
        devices.devices.insert(
            sample_id(2),
            sample_device(
                2,
                "missingrole",
                Some("super_custom"),
                "2026-11-24T10:00:00Z",
                "tower",
            ),
        );
        // 3. Two live devices colliding on device:collide with different roles -> must fail closed to nobody.
        let mut c1 = sample_device(
            3,
            "collide",
            Some("observer"),
            "2026-11-24T10:00:00Z",
            "tower",
        );
        c1.principal = "device:collide".to_owned();
        let mut c2 = sample_device(4, "collide-2", Some("owner"), "2026-11-24T10:00:00Z", "pi");
        c2.principal = "device:collide".to_owned();
        devices.devices.insert(sample_id(3), c1);
        devices.devices.insert(sample_id(4), c2);
        // 4. Non-device principal entry must leave [principals.phone] untouched.
        let mut custom = sample_device(
            5,
            "shared",
            Some("observer"),
            "2026-11-24T10:00:00Z",
            "tower",
        );
        custom.principal = "phone".to_owned();
        devices.devices.insert(sample_id(5), custom);

        let _lock = lock_config(&paths).unwrap();
        let warnings = reconcile_device_principals_locked(&paths, &devices).unwrap();
        assert_eq!(warnings.len(), 2, "{warnings:?}");

        let policy = parse::<Policy>(
            &paths.config_file("policy.toml"),
            &std::fs::read(paths.config_file("policy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(policy.role_of("device:pixel").0, "observer");
        assert_eq!(policy.role_of("device:missingrole").0, NOBODY);
        assert_eq!(policy.role_of("device:collide").0, NOBODY);
        assert_eq!(policy.role_of("phone").0, "editor");
    }

    #[test]
    fn non_admin_roles_cannot_read_or_write_devices_toml_via_file_tools() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let devices_path = paths.config_file(DevicesFile::FILE);
        std::fs::write(&devices_path, "version = 1\n").unwrap();

        let policy: Policy = parse(
            Path::new("policy.toml"),
            br#"
default = "owner"
[roles.editor]
allow = ["effect:read", "effect:write"]
[principals.agent]
role = "editor"
"#,
        )
        .unwrap();
        let grant = policy.grant("agent", &paths);
        assert_eq!(
            grant
                .check_path(&devices_path, Access::Read)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            grant
                .check_path(&devices_path, Access::Write)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
    }

    #[test]
    fn adopts_legacy_pairing_lines_and_reconciles_authorized_keys_without_touching_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_path = dir.path().join("authorized_keys");
        let exe = legion_core::current_exe().unwrap();
        let shell = ForcedCommandShell::host().unwrap();

        let foreign_line = format!("{} me@laptop", sample_key(99));
        let legacy_id = sample_id(7);
        let legacy_line = format_device_line(
            &exe,
            "phone",
            &sample_key(7),
            "legacy-phone",
            &legacy_id,
            &PinnedHome::from_paths(&paths),
            None,
            shell,
        )
        .unwrap();
        std::fs::write(&auth_path, format!("{foreign_line}\n{legacy_line}\n")).unwrap();

        let mut node_file = NodeFile::default();
        node_file.devices.authorized_keys = Some(auth_path.display().to_string());

        // First run adopts legacy-phone into devices.toml rather than dropping it.
        let adopted =
            reconcile_devices_and_auth_keys_blocking(&paths, "tower", &node_file, false).unwrap();
        assert_eq!(adopted.devices_adopted, 1);
        assert!(adopted.devices.devices.contains_key(&legacy_id));
        assert!(paths.config_file(DevicesFile::FILE).is_file());
        let auth_text = std::fs::read_to_string(&auth_path).unwrap();
        assert!(auth_text.contains(&foreign_line));
        assert!(auth_text.contains("legion-device:legacy-phone"));

        // Tombstoning legacy-phone in devices.toml removes its line from authorized_keys while
        // keeping the foreign line untouched.
        let mut tombstoned = adopted.devices.clone();
        tombstoned.devices.remove(&legacy_id);
        tombstoned.removed.insert(
            legacy_id.clone(),
            sample_tombstone("legacy-phone", "2026-11-24T12:00:00Z", "pi"),
        );
        {
            let _lock = lock_config(&paths).unwrap();
            save_devices_locked(&paths, &tombstoned).unwrap();
        }
        let after =
            reconcile_devices_and_auth_keys_blocking(&paths, "tower", &node_file, false).unwrap();
        assert!(after.auth_keys_changed);
        assert_eq!(after.removed_device_names, vec!["legacy-phone".to_owned()]);
        let auth_after = std::fs::read_to_string(&auth_path).unwrap();
        assert!(auth_after.contains(&foreign_line));
        assert!(!auth_after.contains("legion-device:legacy-phone"));
    }
}
