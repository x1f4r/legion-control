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
use toml_edit::{DocumentMut, InlineTable, Item, Table, value};

use super::authkeys::{
    AuthKeysFile, AuthKeysTarget, ForcedCommandShell, LineKind, PinnedHome,
    edit_auth_keys_with_retry, format_device_line, line_matches_account, line_matches_home,
    read_auth_keys, resolve_safe_write_path, resolve_target,
    verify_exe_not_group_or_world_writable, verify_owner_matches_euid,
};
use super::keys::{fingerprint_key_b64, validate_ed25519_public_key, validate_pairing_id};
use super::sessions::{SessionTarget, validate_device_label};
use super::store::{DevicesState, load_state, lock_auth_keys};

/// Current schema version of `devices.toml`.
pub const DEVICES_VERSION: u32 = 1;

/// Prefix of per-device principals automatically managed in `policy.toml`.
pub const DEVICE_PRINCIPAL_PREFIX: &str = "device:";

/// Maximum length of a role name recorded in `devices.toml`.
const MAX_ROLE_NAME_LEN: usize = 128;

/// Longest time an operation waits for `state/sync/config.lock`.
const CONFIG_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum future clock skew accepted on incoming `devices.toml` timestamps (`paired_at`,
/// `updated_at`, `removed_at`). Timestamps further ahead than `now + MAX_FUTURE_SKEW` are
/// refused so a peer cannot pin an entry with a far-future timestamp that defeats later edits.
pub const MAX_FUTURE_SKEW: SignedDuration = SignedDuration::from_mins(10);

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

/// Who created a paired device (captured from the invite creator at pairing time).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedBy {
    /// Principal that created the pairing invite.
    pub principal: String,
    /// Pairing identifier of the device that created the invite, when created by a paired device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
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
    /// Who created the invite that paired this device (principal and optional creator device id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paired_by: Option<PairedBy>,
    /// Owner principal that explicitly confirmed this device after a quarantine, clearing
    /// quarantine for it and its unconfirmed descendants when `confirmed_at` is later than the
    /// removing tombstone's `removed_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_by: Option<String>,
    /// RFC 3339 UTC timestamp when `confirmed_by` confirmed this device via `device.set`.
    /// Must be present whenever `confirmed_by` is set and absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<String>,
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
            if let Some(paired_by) = &entry.paired_by {
                Principal::parse(&paired_by.principal)
                    .map_err(|e| format!("devices.{id}.paired_by.principal: {}", e.message))?;
                if let Some(dev_id) = &paired_by.device {
                    validate_pairing_id(dev_id)
                        .map_err(|e| format!("devices.{id}.paired_by.device: {}", e.message))?;
                }
            }
            if let Some(confirmed_by) = &entry.confirmed_by {
                Principal::parse(confirmed_by)
                    .map_err(|e| format!("devices.{id}.confirmed_by: {}", e.message))?;
            }
            if let Some(confirmed_at) = &entry.confirmed_at {
                confirmed_at
                    .parse::<Timestamp>()
                    .map_err(|e| format!("devices.{id}.confirmed_at is not RFC 3339 UTC: {e}"))?;
            }
            if entry.confirmed_by.is_some() != entry.confirmed_at.is_some() {
                return Err(format!(
                    "devices.{id}: confirmed_by and confirmed_at must either both be set or both be omitted"
                ));
            }
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
                if let Some(paired_by) = &dev.paired_by {
                    let mut inline = InlineTable::new();
                    inline.insert("principal", paired_by.principal.as_str().into());
                    if let Some(dev_id) = &paired_by.device {
                        inline.insert("device", dev_id.as_str().into());
                    }
                    t["paired_by"] = value(inline);
                }
                if let Some(confirmed_by) = &dev.confirmed_by {
                    t["confirmed_by"] = value(confirmed_by.as_str());
                }
                if let Some(confirmed_at) = &dev.confirmed_at {
                    t["confirmed_at"] = value(confirmed_at.as_str());
                }
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

/// Returns a current RFC 3339 UTC timestamp strictly greater than `previous` (if present),
/// preventing same-second ties during rapid local updates.
///
/// Returns an error rather than going backwards if `previous` cannot be parsed or is already at
/// the maximum representable timestamp (`Timestamp::MAX`).
pub fn next_updated_timestamp(previous: Option<&str>) -> Result<String, ErrorBody> {
    next_updated_timestamp_at(previous, Timestamp::now())
}

/// Deterministic variant of [`next_updated_timestamp`] using an explicit `now`.
pub fn next_updated_timestamp_at(
    previous: Option<&str>,
    now: Timestamp,
) -> Result<String, ErrorBody> {
    let chosen = match previous {
        None => now,
        Some(raw) => {
            let prev = raw.parse::<Timestamp>().map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Existing device timestamp {raw:?} is not valid RFC 3339 UTC: {error}."
                    ),
                )
            })?;
            if now <= prev {
                prev.checked_add(SignedDuration::from_nanos(1)).map_err(|_| {
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Cannot advance device timestamp past maximum representable time ({raw})."
                        ),
                    )
                    .with_hint(
                        "Inspect devices.toml for a corrupt or maximum-value timestamp before updating.",
                    )
                })?
            } else {
                now
            }
        }
    };
    Ok(format_precise_rfc3339(chosen))
}

/// Rejects any incoming `devices.toml` entry whose `paired_at`, `updated_at`, or `removed_at`
/// lies further in the future than `now + MAX_FUTURE_SKEW`.
pub fn validate_incoming_timestamps(
    devices: &DevicesFile,
    now: Timestamp,
) -> Result<(), ErrorBody> {
    let limit = now.checked_add(MAX_FUTURE_SKEW).unwrap_or(Timestamp::MAX);
    let check = |section: &str, id: &str, field: &str, raw: &str| -> Result<(), ErrorBody> {
        let parsed = raw.parse::<Timestamp>().map_err(|error| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("devices.toml {section}.{id}.{field} ({raw:?}) is not valid RFC 3339 UTC: {error}."),
            )
        })?;
        if parsed > limit {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "devices.toml {section}.{id}.{field} ({raw}) is further in the future than the allowed {} minute clock skew.",
                    MAX_FUTURE_SKEW.as_mins()
                ),
            )
            .with_hint("Check the clock on the sending machine and try syncing again."));
        }
        Ok(())
    };

    for (id, entry) in &devices.devices {
        check("devices", id, "paired_at", &entry.paired_at)?;
        check("devices", id, "updated_at", &entry.updated_at)?;
        if let Some(confirmed_at) = &entry.confirmed_at {
            check("devices", id, "confirmed_at", confirmed_at)?;
        }
    }
    for (id, tomb) in &devices.removed {
        if let Some(paired_at) = &tomb.paired_at {
            check("removed", id, "paired_at", paired_at)?;
        }
        check("removed", id, "removed_at", &tomb.removed_at)?;
    }
    Ok(())
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
        d.paired_by
            .as_ref()
            .map(|p| p.principal.as_str())
            .unwrap_or(""),
        d.paired_by
            .as_ref()
            .and_then(|p| p.device.as_deref())
            .unwrap_or(""),
        d.confirmed_by.as_deref().unwrap_or(""),
        d.confirmed_at.as_deref().unwrap_or(""),
    )
}

fn confirmed_after(entry: &FleetDevice, removed_ts: Timestamp) -> bool {
    entry.confirmed_by.is_some()
        && entry
            .confirmed_at
            .as_deref()
            .and_then(|raw| raw.parse::<Timestamp>().ok())
            .is_some_and(|confirmed_ts| confirmed_ts > removed_ts)
}

/// Returns the live pairing ids in `devices` that are quarantined, mapped to
/// `(causing_removed_at, human_reason)`.
///
/// A child counts as created after its creator's removal when `paired_at >= removed_at - 10
/// minutes` (matching [`MAX_FUTURE_SKEW`]). A confirmation (`confirmed_by` + `confirmed_at`)
/// clears quarantine for a device and its unconfirmed descendants ONLY when `confirmed_at` is
/// strictly later than the removing tombstone's `removed_at`.
pub fn quarantined_devices_with_timestamps(
    devices: &DevicesFile,
) -> BTreeMap<String, (Timestamp, String)> {
    let mut quarantined: BTreeMap<String, (Timestamp, String)> = BTreeMap::new();

    for (id, entry) in &devices.devices {
        let Some(creator_id) = entry.paired_by.as_ref().and_then(|p| p.device.as_deref()) else {
            continue;
        };
        let Some(tomb) = devices.removed.get(creator_id) else {
            continue;
        };
        let (Ok(paired_ts), Ok(removed_ts)) = (
            entry.paired_at.parse::<Timestamp>(),
            tomb.removed_at.parse::<Timestamp>(),
        ) else {
            continue;
        };
        let cutoff = removed_ts
            .checked_sub(MAX_FUTURE_SKEW)
            .unwrap_or(Timestamp::MIN);
        if paired_ts >= cutoff && !confirmed_after(entry, removed_ts) {
            quarantined.insert(
                id.clone(),
                (
                    removed_ts,
                    format!(
                        "created by removed device {} ({creator_id}) at {} (creator was removed at {})",
                        tomb.name, entry.paired_at, tomb.removed_at
                    ),
                ),
            );
        }
    }

    loop {
        let mut added = Vec::new();
        for (id, entry) in &devices.devices {
            if quarantined.contains_key(id) {
                continue;
            }
            let Some(creator_id) = entry.paired_by.as_ref().and_then(|p| p.device.as_deref())
            else {
                continue;
            };
            if let Some((parent_removed_ts, _)) = quarantined.get(creator_id)
                && !confirmed_after(entry, *parent_removed_ts)
            {
                let creator_name = devices
                    .devices
                    .get(creator_id)
                    .map(|d| d.name.as_str())
                    .unwrap_or(creator_id);
                added.push((
                    id.clone(),
                    (
                        *parent_removed_ts,
                        format!("created by quarantined device {creator_name} ({creator_id})"),
                    ),
                ));
            }
        }
        if added.is_empty() {
            break;
        }
        for (id, entry) in added {
            quarantined.insert(id, entry);
        }
    }

    quarantined
}

/// Returns the live pairing ids in `devices` that are quarantined because they (or an unconfirmed
/// ancestor in their `paired_by.device` chain) were paired at or after their creator device's
/// removal (within the [`MAX_FUTURE_SKEW`] window), mapped to a human-readable reason.
pub fn quarantined_devices(devices: &DevicesFile) -> BTreeMap<String, String> {
    quarantined_devices_with_timestamps(devices)
        .into_iter()
        .map(|(id, (_, reason))| (id, reason))
        .collect()
}

/// Read-only list of `(name, pairing_id, reason)` for quarantined live devices in `devices.toml`,
/// used by `meta.doctor`.
pub fn inspect_quarantined_devices(paths: &Paths) -> Vec<(String, String, String)> {
    let Ok(Some(devices)) = load_devices_raw(paths) else {
        return Vec::new();
    };
    quarantined_devices(&devices)
        .into_iter()
        .map(|(id, reason)| {
            let name = devices
                .devices
                .get(&id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| id.clone());
            (name, id, reason)
        })
        .collect()
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

/// Inserts or updates a registry-owned `[principals."device:..."]` entry carrying
/// `role = "<role>"` and `device = "<pairing_id>"` in `policy.toml`, preserving existing comments
/// and layout, validating with `parse::<Policy>` before writing, backing up the previous file, and
/// replacing it atomically.
///
/// Refuses to create or modify any principal that is not a `device:*` principal or that already
/// exists in `policy.toml` without a matching `device = "<pairing_id>"` ownership marker.
/// Caller must hold [`lock_config`].
pub fn upsert_device_principal_locked(
    paths: &Paths,
    principal: &str,
    role: &str,
    pairing_id: &str,
) -> Result<PolicyEditOutcome, ErrorBody> {
    Principal::parse(principal)?;
    validate_role_name(role)?;
    validate_pairing_id(pairing_id)?;
    if principal == OWNER || !principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Principal {principal} is not a registry-owned `device:*` principal and cannot be modified by device tools."
            ),
        )
        .with_hint(format!(
            "Edit [principals.\"{principal}\"] in policy.toml directly if you want to change its role."
        )));
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

    let existing_entry = previous_policy
        .principals
        .iter()
        .find(|(p, _)| p.as_str() == principal)
        .map(|(_, entry)| entry);

    if let Some(entry) = existing_entry {
        match entry.device.as_deref() {
            Some(owner_id) if owner_id == pairing_id => {}
            Some(other_id) => {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Principal {principal} in policy.toml is owned by pairing {other_id}, not {pairing_id}."
                    ),
                )
                .with_hint(format!(
                    "Edit [principals.\"{principal}\"] in policy.toml directly or remove the conflicting pairing."
                )));
            }
            None => {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Principal {principal} in policy.toml has no `device = \"{pairing_id}\"` ownership marker and will not be modified automatically."
                    ),
                )
                .with_hint(format!(
                    "Edit [principals.\"{principal}\"] in policy.toml directly to change its role, or pair this device with `legion pair --name ... --role ...`."
                )));
            }
        }
    }

    let previous_role = existing_entry.map(|entry| entry.role.clone());

    if previous_role.as_deref() == Some(role)
        && existing_entry.and_then(|e| e.device.as_deref()) == Some(pairing_id)
        && existing_bytes.is_some()
    {
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
    entry["device"] = value(pairing_id);
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

/// Removes `[principals."<principal>"]` from `policy.toml` ONLY when it carries a matching
/// `device = "<pairing_id>"` ownership marker and is not `policy.default`, keeping all other
/// entries and comments intact. Caller must hold [`lock_config`].
pub fn remove_device_principal_locked(
    paths: &Paths,
    principal: &str,
    pairing_id: &str,
) -> Result<bool, ErrorBody> {
    if principal == OWNER || !principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
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
    if parsed.device_owner(principal) != Some(pairing_id) {
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

/// Reconciles registry-owned `device:*` principals in `policy.toml` to match `devices`.
///
/// Security invariants:
/// 1. Only principals starting with `device:` that either do not yet exist in `policy.toml` or
///    carry a matching `device = "<pairing_id>"` ownership marker are ever touched. Non-device
///    principals and human-authored (unmarked) `device:*` principals are never created, elevated,
///    modified, or removed; unmarked `device:*` principals emit a warning.
/// 2. Tombstoned registry-owned `device:*` principals (matching `device = "<tombstone_id>"` and
///    not referenced by any token) are removed first, and a `device:*` principal that belonged to
///    a tombstoned pairing id is never re-created for a different pairing id.
/// 3. Every single live `device:*` entry that carries a recorded `role` sets its owned
///    `[principals."device:..."]` entry to that exact role with `device = "<pairing_id>"`.
/// 4. If a recorded role is not defined in this node's `policy.toml` (and is not `owner` or
///    `nobody`), the owned principal fails closed to `nobody` with a warning.
/// 5. If two live devices collide on the same `device:*` principal, any matching registry-owned
///    entry fails closed to `nobody` (an absent or unmarked entry is left untouched) with a warning.
/// 6. An entry with `role: None` (such as an adopted legacy line without a recorded role) never
///    creates or elevates a principal.
///
/// Caller must hold [`lock_config`]. Returns any warnings.
pub fn reconcile_device_principals_locked(
    paths: &Paths,
    devices: &DevicesFile,
) -> Result<Vec<String>, ErrorBody> {
    let policy_path = paths.config_file(Policy::FILE);
    let read_policy = || -> Result<Policy, ErrorBody> {
        match std::fs::read(&policy_path) {
            Ok(bytes) => parse::<Policy>(&policy_path, &bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(error) => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", policy_path.display()),
            )),
        }
    };

    let tokens_path = paths.config_file(Tokens::FILE);
    let token_principals: HashSet<String> = match std::fs::read(&tokens_path) {
        Ok(bytes) => parse::<Tokens>(&tokens_path, &bytes)
            .map(|t| t.tokens.values().map(|e| e.principal.to_string()).collect())
            .unwrap_or_default(),
        Err(_) => HashSet::new(),
    };

    // 1. Clean up tombstoned registry-owned `device:*` principals first (only when `policy.toml`
    //    marks `device = "<tombstone_id>"` and no HTTP token uses the principal).
    let mut policy = read_policy()?;
    let mut removed_any = false;
    for (tomb_id, tomb) in &devices.removed {
        if tomb.principal.starts_with(DEVICE_PRINCIPAL_PREFIX)
            && policy.device_owner(&tomb.principal) == Some(tomb_id.as_str())
            && !token_principals.contains(&tomb.principal)
            && remove_device_principal_locked(paths, &tomb.principal, tomb_id)?
        {
            removed_any = true;
        }
    }
    if removed_any {
        policy = read_policy()?;
    }

    // 2. Group live `device:*` entries by principal to detect collisions and check ownership.
    let mut live_by_principal: BTreeMap<&str, Vec<(&str, Option<&str>)>> = BTreeMap::new();
    for (id, entry) in &devices.devices {
        if entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
            live_by_principal
                .entry(entry.principal.as_str())
                .or_default()
                .push((id.as_str(), entry.role.as_deref()));
        }
    }

    let quarantined = quarantined_devices(devices);
    let mut warnings = Vec::new();
    for (principal, entries) in &live_by_principal {
        let existing_entry = policy
            .principals
            .iter()
            .find(|(p, _)| p.as_str() == *principal)
            .map(|(_, entry)| entry);

        // An existing `device:*` principal without a `device = "<pairing_id>"` marker is human-
        // authored (possibly shared with a token or agent): never touch it, and warn loudly.
        if let Some(existing) = existing_entry
            && existing.device.is_none()
        {
            warnings.push(format!(
                "Principal {principal} in policy.toml has no `device = \"<pairing_id>\"` ownership marker and was left untouched (role: {}).",
                existing.role
            ));
            continue;
        }

        if entries.len() > 1 {
            let ids: Vec<&str> = entries.iter().map(|(id, _)| *id).collect();
            warnings.push(format!(
                "Principal {principal} is claimed by multiple live devices ({}); leaving it restricted to `nobody` until disambiguated.",
                ids.join(", ")
            ));
            if let Some(existing) = existing_entry
                && let Some(owner_id) = existing.device.as_deref()
                && ids.contains(&owner_id)
            {
                upsert_device_principal_locked(paths, principal, NOBODY, owner_id)?;
            }
            continue;
        }

        let (pairing_id, recorded_role) = entries[0];
        if let Some(existing) = existing_entry
            && existing.device.as_deref() != Some(pairing_id)
        {
            let other = existing.device.as_deref().unwrap_or("unknown");
            warnings.push(format!(
                "Principal {principal} in policy.toml is owned by pairing {other}, not {pairing_id}; leaving it untouched."
            ));
            continue;
        }

        if let Some((tomb_id, _)) = devices
            .removed
            .iter()
            .find(|(tid, t)| tid.as_str() != pairing_id && t.principal == *principal)
        {
            warnings.push(format!(
                "Principal {principal} previously belonged to removed pairing {tomb_id} and will not be re-created for pairing {pairing_id}."
            ));
            if let Some(existing) = existing_entry
                && existing.device.as_deref() == Some(pairing_id)
            {
                upsert_device_principal_locked(paths, principal, NOBODY, pairing_id)?;
            }
            continue;
        }

        if let Some(reason) = quarantined.get(pairing_id) {
            warnings.push(format!(
                "Principal {principal} belongs to quarantined device {pairing_id} ({reason}); leaving it restricted to `nobody` until confirmed by the owner."
            ));
            if let Some(existing) = existing_entry
                && existing.device.as_deref() == Some(pairing_id)
            {
                upsert_device_principal_locked(paths, principal, NOBODY, pairing_id)?;
            }
            continue;
        }

        let Some(recorded) = recorded_role else {
            // Adopted legacy entry without a recorded role: never invent or elevate a principal.
            continue;
        };

        let known = recorded == OWNER || recorded == NOBODY || policy.roles.contains_key(recorded);
        let target_role = if known {
            recorded
        } else {
            warnings.push(format!(
                "Role {recorded:?} for {principal} is not defined in this node's policy.toml; setting {principal} to `nobody`."
            ));
            NOBODY
        };

        upsert_device_principal_locked(paths, principal, target_role, pairing_id)?;
    }

    Ok(warnings)
}

/// Checks whether `auth_keys` contains `wire_blob` on any foreign or invite line (or another home's
/// device line), so reconciling a synced device never collides with an existing non-managed key.
fn contains_non_owned_key_blob(
    auth_keys: &AuthKeysFile,
    devices: &DevicesFile,
    wire_blob: &[u8],
    paths: &Paths,
    caller_account: Option<&str>,
    target: &AuthKeysTarget,
) -> bool {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

    for line in &auth_keys.lines {
        let is_registry_managed_line = matches!(
            &line.kind,
            LineKind::Device { pairing_id, home, account, .. }
                if (devices.devices.contains_key(pairing_id.as_str())
                    || devices.removed.contains_key(pairing_id.as_str()))
                    && line_matches_home(home.as_ref(), paths)
                    && line_matches_account(
                        account.as_deref(),
                        caller_account,
                        target.windows_admin_acl,
                        cfg!(windows),
                    )
        );
        if is_registry_managed_line {
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
///
/// Only lines matching a record in this node's own `state/devices.json` (with equal `principal`,
/// key `fingerprint`, and `pairing_id`) are adopted. Any other Legion-shaped line is left
/// untouched as local-only and never becomes a fleet entry on its own.
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
        let Some(matched_state) = state.devices.values().find(|meta| {
            meta.principal == *principal
                && meta.fingerprint == validated_key.fingerprint
                && meta.pairing_id.as_deref() == Some(pairing_id.as_str())
        }) else {
            // Unmatched Legion-shaped line stays local-only and is never adopted into devices.toml.
            continue;
        };
        let paired_at = matched_state
            .added
            .parse::<Timestamp>()
            .ok()
            .map(|_| matched_state.added.clone())
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
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: paired_at,
                updated_on: valid_node.clone(),
            });
    }

    file
}

/// Status message shown when a live device's public key already sits on a foreign line of
/// `authorized_keys`.
pub const FOREIGN_KEY_COLLISION_STATUS: &str =
    "not active here: this key is already on another line of authorized_keys";

/// Outcome of reconciling this home's device lines in `authorized_keys` against `devices.toml`.
#[derive(Clone, Debug, Default)]
pub struct AuthKeysReconcileReport {
    /// Whether `authorized_keys` was modified in memory and needs to be saved.
    pub changed: bool,
    /// Devices (disambiguated by pairing id) whose `authorized_keys` lines were removed during
    /// reconciliation.
    pub removed_targets: Vec<SessionTarget>,
    /// Live pairing ids skipped because two or more live devices share the same public key, mapped
    /// to the other colliding pairing ids.
    pub duplicate_key_ids: BTreeMap<String, Vec<String>>,
    /// Live pairing ids skipped because their public key already sits on a foreign line of
    /// `authorized_keys`.
    pub foreign_key_ids: BTreeSet<String>,
    /// Live pairing ids quarantined because an unconfirmed ancestor was removed at or before their
    /// pairing time, mapped to a human-readable reason.
    pub quarantined_ids: BTreeMap<String, String>,
    /// Live pairing ids skipped because their `device:*` principal is unmarked in `policy.toml`,
    /// marked for another pairing id, shared/tombstoned, or assigned a different role in
    /// `policy.toml` than recorded in `devices.toml`, mapped to a human-readable reason.
    pub principal_conflict_ids: BTreeMap<String, String>,
    /// Warnings generated for colliding, principal-conflicting, quarantined, or inactive entries.
    pub warnings: Vec<String>,
}

/// Returns live pairing ids in `devices` whose `device:*` principal cannot safely be activated
/// under `policy`, mapped to a human-readable reason:
/// - the `device:*` principal exists in `policy.toml` without a `device = "<pairing_id>"` marker
///   (unmarked / human-authored),
/// - the `device:*` principal is marked in `policy.toml` for a different pairing id,
/// - two or more live devices claim the same `device:*` principal,
/// - a tombstoned pairing of a different id previously owned that `device:*` principal, or
/// - `policy.toml` assigns a role other than the device's recorded `role` (or `nobody`), so a
///   recorded role can never be exceeded by what `policy.toml` happens to say.
pub fn device_principal_conflicts(
    devices: &DevicesFile,
    policy: &Policy,
) -> BTreeMap<String, String> {
    let mut conflicts: BTreeMap<String, String> = BTreeMap::new();
    let mut live_by_principal: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, entry) in &devices.devices {
        if entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
            live_by_principal
                .entry(entry.principal.as_str())
                .or_default()
                .push(id.as_str());
        }
    }

    for (id, entry) in &devices.devices {
        if !entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
            continue;
        }
        let principal = entry.principal.as_str();
        if let Some(ids) = live_by_principal.get(principal)
            && ids.len() > 1
        {
            let others: Vec<&str> = ids.iter().copied().filter(|other| *other != id).collect();
            conflicts.insert(
                id.clone(),
                format!(
                    "principal {principal} is claimed by multiple live devices ({})",
                    others.join(", ")
                ),
            );
            continue;
        }
        if let Some((tomb_id, _)) = devices
            .removed
            .iter()
            .find(|(tid, tomb)| tid.as_str() != id && tomb.principal == principal)
        {
            conflicts.insert(
                id.clone(),
                format!("principal {principal} previously belonged to removed pairing {tomb_id}"),
            );
            continue;
        }
        if let Some((_, existing)) = policy
            .principals
            .iter()
            .find(|(p, _)| p.as_str() == principal)
        {
            match existing.device.as_deref() {
                None => {
                    conflicts.insert(
                        id.clone(),
                        format!(
                            "principal {principal} in policy.toml has no `device = \"{id}\"` ownership marker (role: {})",
                            existing.role
                        ),
                    );
                    continue;
                }
                Some(owner_id) if owner_id != id => {
                    conflicts.insert(
                        id.clone(),
                        format!(
                            "principal {principal} in policy.toml is owned by pairing {owner_id}, not {id}"
                        ),
                    );
                    continue;
                }
                Some(_) => {
                    if let Some(recorded) = entry.role.as_deref()
                        && existing.role != recorded
                        && existing.role != NOBODY
                    {
                        conflicts.insert(
                            id.clone(),
                            format!(
                                "policy.toml gives {principal} role {:?}, which does not match recorded role {recorded:?}",
                                existing.role
                            ),
                        );
                    }
                }
            }
        }
    }

    conflicts
}

/// Read-only list of `(name, pairing_id, reason)` for live devices in `devices.toml` blocked by a
/// `device:*` principal ownership or role conflict against `policy`, used by `meta.doctor`.
pub fn inspect_device_principal_conflicts(
    paths: &Paths,
    policy: &Policy,
) -> Vec<(String, String, String)> {
    let Ok(Some(devices)) = load_devices_raw(paths) else {
        return Vec::new();
    };
    device_principal_conflicts(&devices, policy)
        .into_iter()
        .map(|(id, reason)| {
            let name = devices
                .devices
                .get(&id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| id.clone());
            (name, id, reason)
        })
        .collect()
}

/// Groups live pairing ids in `devices` by their decoded Ed25519 public-key wire blob, returning
/// only wire blobs shared by two or more live entries.
pub fn duplicate_live_public_keys(
    devices: &DevicesFile,
) -> Result<BTreeMap<Vec<u8>, Vec<String>>, ErrorBody> {
    let mut by_blob: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    for (id, entry) in &devices.devices {
        let validated = validate_ed25519_public_key(&entry.public_key)?;
        by_blob
            .entry(validated.wire_blob.to_vec())
            .or_default()
            .push(id.clone());
    }
    by_blob.retain(|_, ids| ids.len() > 1);
    Ok(by_blob)
}

/// Result of read-only key conflict inspection for `meta.doctor`.
#[derive(Clone, Debug, Default)]
pub struct DeviceKeyConflicts {
    /// Groups of live devices sharing the same SSH public key (`(name, pairing_id)`).
    pub duplicate_groups: Vec<Vec<(String, String)>>,
    /// Live devices `(name, pairing_id)` whose key already sits on a foreign line of
    /// `authorized_keys`.
    pub foreign_conflicts: Vec<(String, String)>,
}

/// Read-only inspection of `devices.toml` and `authorized_keys` for `meta.doctor`.
pub fn inspect_device_key_conflicts(paths: &Paths, node_file: &NodeFile) -> DeviceKeyConflicts {
    let Ok(Some(devices)) = load_devices_raw(paths) else {
        return DeviceKeyConflicts::default();
    };
    let mut by_blob: BTreeMap<Vec<u8>, Vec<(String, String)>> = BTreeMap::new();
    let mut validated_by_id: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (id, entry) in &devices.devices {
        if let Ok(validated) = validate_ed25519_public_key(&entry.public_key) {
            let blob = validated.wire_blob.to_vec();
            by_blob
                .entry(blob.clone())
                .or_default()
                .push((entry.name.clone(), id.clone()));
            validated_by_id.insert(id.clone(), blob);
        }
    }
    let mut duplicate_groups: Vec<Vec<(String, String)>> = Vec::new();
    let mut dup_ids: BTreeSet<String> = BTreeSet::new();
    for group in by_blob.into_values() {
        if group.len() > 1 {
            for (_, id) in &group {
                dup_ids.insert(id.clone());
            }
            duplicate_groups.push(group);
        }
    }

    let mut foreign_conflicts: Vec<(String, String)> = Vec::new();
    if let Ok(target) = resolve_target(paths, node_file)
        && let Ok(write_path) = resolve_safe_write_path(&target, paths.home())
        && write_path.is_file()
        && let Ok(auth_keys) = read_auth_keys(&write_path)
    {
        let caller_account = caller_account_for_target(&target).ok().flatten();
        for (id, entry) in &devices.devices {
            if dup_ids.contains(id) {
                continue;
            }
            if let Some(blob) = validated_by_id.get(id)
                && contains_non_owned_key_blob(
                    &auth_keys,
                    &devices,
                    blob,
                    paths,
                    caller_account.as_deref(),
                    &target,
                )
            {
                foreign_conflicts.push((entry.name.clone(), id.clone()));
            }
        }
    }

    DeviceKeyConflicts {
        duplicate_groups,
        foreign_conflicts,
    }
}

/// Reconciles this home's device lines in `auth_keys` to match `devices`.
///
/// Foreign lines, pending invites, and device lines belonging to another Legion home or another OS
/// account are preserved byte for byte. If two or more live entries in `devices` share the same
/// public key, NO `authorized_keys` line is written for ANY of them (and any existing lines for
/// them are removed) with a loud warning. Live entries whose public key already sits on a foreign
/// line of `authorized_keys` are similarly skipped with a warning.
#[allow(clippy::too_many_arguments)]
pub fn reconcile_authorized_keys_in_memory(
    auth_keys: &mut AuthKeysFile,
    devices: &DevicesFile,
    policy: &Policy,
    paths: &Paths,
    target: &AuthKeysTarget,
    caller_account: Option<&str>,
    exe: &Path,
    shell: ForcedCommandShell,
) -> Result<AuthKeysReconcileReport, ErrorBody> {
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

    // Pass 1: validate every live key and detect duplicate public keys across live entries so NO
    // line is written for ANY entry in a colliding group.
    let mut validated_keys: BTreeMap<&str, (String, Vec<u8>)> = BTreeMap::new();
    let mut ids_by_blob: BTreeMap<Vec<u8>, Vec<&str>> = BTreeMap::new();
    for (id, entry) in &devices.devices {
        let validated = validate_ed25519_public_key(&entry.public_key)?;
        let blob = validated.wire_blob.to_vec();
        ids_by_blob
            .entry(blob.clone())
            .or_default()
            .push(id.as_str());
        validated_keys.insert(id.as_str(), (validated.canonical, blob));
    }

    let mut duplicate_key_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    for ids in ids_by_blob.values() {
        if ids.len() > 1 {
            let labels: Vec<String> = ids
                .iter()
                .filter_map(|id| {
                    devices
                        .devices
                        .get(*id)
                        .map(|d| format!("{} ({id})", d.name))
                })
                .collect();
            warnings.push(format!(
                "Public key collision in devices.toml: live devices {} share the same SSH public key; no authorized_keys line was written for any of them.",
                labels.join(", ")
            ));
            for id in ids {
                let others: Vec<String> = ids
                    .iter()
                    .filter(|other| *other != id)
                    .map(|other| (*other).to_owned())
                    .collect();
                duplicate_key_ids.insert((*id).to_owned(), others);
            }
        }
    }

    // Pass 2: build expected canonical line per non-colliding, non-quarantined, non-conflicting
    // live pairing id.
    let quarantined_ids = quarantined_devices(devices);
    let principal_conflict_ids = device_principal_conflicts(devices, policy);
    let mut expected_lines: BTreeMap<&str, (String, Vec<u8>)> = BTreeMap::new();
    let mut foreign_key_ids: BTreeSet<String> = BTreeSet::new();
    for (id, entry) in &devices.devices {
        if let Some(reason) = quarantined_ids.get(id) {
            warnings.push(format!(
                "Device {} ({id}) is quarantined ({reason}) and not active here; confirm with `legion device.set id={id} confirm=true` or remove with `legion device.remove id={id}`.",
                entry.name
            ));
            continue;
        }
        if let Some(reason) = principal_conflict_ids.get(id) {
            warnings.push(format!(
                "Device {} ({id}) is not active here: {reason}.",
                entry.name
            ));
            continue;
        }
        if duplicate_key_ids.contains_key(id) {
            continue;
        }
        let Some((canonical, wire_blob)) = validated_keys.get(id.as_str()) else {
            continue;
        };
        if contains_non_owned_key_blob(auth_keys, devices, wire_blob, paths, caller_account, target)
        {
            foreign_key_ids.insert(id.clone());
            warnings.push(format!(
                "Device {} ({id}) is {FOREIGN_KEY_COLLISION_STATUS}.",
                entry.name
            ));
            continue;
        }
        let line = format_device_line(
            exe,
            &entry.principal,
            canonical,
            &entry.name,
            id,
            &pinned_home,
            pinned_account,
            shell,
        )?;
        expected_lines.insert(id.as_str(), (line, wire_blob.clone()));
    }

    let mut kept_ids: HashSet<String> = HashSet::new();
    let mut removed_targets: BTreeSet<SessionTarget> = BTreeSet::new();

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
        if !devices.devices.contains_key(pairing_id.as_str())
            && !devices.removed.contains_key(pairing_id.as_str())
        {
            // Local-only Legion-shaped line (never adopted into devices.toml and not tombstoned):
            // keep it working locally byte-for-byte without promoting it to a fleet entry.
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

        removed_targets.insert(SessionTarget::new(name.clone(), Some(pairing_id.clone())));
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

    Ok(AuthKeysReconcileReport {
        changed,
        removed_targets: removed_targets.into_iter().collect(),
        duplicate_key_ids,
        foreign_key_ids,
        quarantined_ids,
        principal_conflict_ids,
        warnings,
    })
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
    /// Devices (disambiguated by pairing id) whose `authorized_keys` lines were removed during
    /// reconciliation (callers should terminate active sessions for these targets).
    pub removed_targets: Vec<SessionTarget>,
    /// Warnings generated during `authorized_keys` reconciliation (duplicate keys or foreign-line
    /// collisions).
    pub warnings: Vec<String>,
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

    let policy_path = paths.config_file(Policy::FILE);
    let live_policy = match std::fs::read(&policy_path) {
        Ok(bytes) => parse::<Policy>(&policy_path, &bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Policy::default(),
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", policy_path.display()),
            ));
        }
    };

    let existing_devices = load_devices_raw(paths)?;
    let (devices, devices_adopted) = match existing_devices {
        Some(file) => (file, 0),
        None => {
            let auth_keys = read_auth_keys(&write_path)?;
            let state = load_state(paths).unwrap_or_default();
            let adopted = adopt_legacy_devices_from_auth_keys(
                paths,
                node_name,
                &auth_keys,
                &target,
                caller_account.as_deref(),
                &state,
                &live_policy,
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
    let report = edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
        let report = reconcile_authorized_keys_in_memory(
            &mut auth_keys,
            &devices,
            &live_policy,
            paths,
            &target,
            caller_account.as_deref(),
            &exe,
            shell,
        )?;
        Ok((report.changed.then_some(auth_keys), report))
    })?;

    Ok(ReconcileOutcome {
        devices,
        devices_adopted,
        auth_keys_changed: report.changed,
        removed_targets: report.removed_targets,
        warnings: report.warnings,
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
) -> Result<Vec<SessionTarget>, ErrorBody> {
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

    let quarantined = quarantined_devices(&devices);
    if let Some(reason) = quarantined.get(pairing_id) {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Device {device_name} ({pairing_id}) is quarantined ({reason}) and cannot connect until confirmed by the owner."
            ),
        )
        .with_hint(format!(
            "Confirm this device as owner with `legion device.set id={pairing_id} confirm=true`, or remove it with `legion device.remove id={pairing_id}`."
        )));
    }

    let policy_path = paths.config_file(Policy::FILE);
    let live_policy = match std::fs::read(&policy_path) {
        Ok(bytes) => parse::<Policy>(&policy_path, &bytes)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Policy::default(),
        Err(error) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", policy_path.display()),
            ));
        }
    };

    let principal_conflicts = device_principal_conflicts(&devices, &live_policy);
    if let Some(reason) = principal_conflicts.get(pairing_id) {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("Device {device_name} ({pairing_id}) is not active here: {reason}."),
        ));
    }

    let Some(entry) = devices.devices.get(pairing_id) else {
        // If this pairing id is neither live nor tombstoned in `devices.toml`, check whether a
        // matching local-only Legion-shaped line still exists on disk in `authorized_keys`.
        let caller_account = caller_account_for_target(&target)?;
        let auth_keys = read_auth_keys(&write_path)?;
        let local_only_present = auth_keys.lines.iter().any(|line| {
            matches!(
                &line.kind,
                LineKind::Device {
                    name: line_name,
                    principal: line_principal,
                    pairing_id: line_pid,
                    home,
                    account: line_account,
                    ..
                } if line_pid == pairing_id
                    && line_name == device_name
                    && line_principal == principal
                    && line_matches_home(home.as_ref(), paths)
                    && line_matches_account(
                        line_account.as_deref(),
                        caller_account.as_deref(),
                        target.windows_admin_acl,
                        cfg!(windows),
                    )
            )
        });
        if local_only_present {
            return Ok(Vec::new());
        }
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

    // Hot path: check read-only whether authorized_keys on disk actually matches the registry's
    // key for this pairing id BEFORE reconciling (and also deny if reconciliation marks our own
    // device stale or inactive due to key collision).
    let caller_account = caller_account_for_target(&target)?;
    let auth_keys = read_auth_keys(&write_path)?;
    let expected_key = validate_ed25519_public_key(&entry.public_key)?;
    let mut saw_matching_id_line = false;
    let mut disk_key_mismatch = false;
    for line in &auth_keys.lines {
        if let LineKind::Device {
            key_b64,
            pairing_id: line_pid,
            home,
            account: line_account,
            ..
        } = &line.kind
            && line_pid == pairing_id
            && line_matches_home(home.as_ref(), paths)
            && line_matches_account(
                line_account.as_deref(),
                caller_account.as_deref(),
                target.windows_admin_acl,
                cfg!(windows),
            )
        {
            saw_matching_id_line = true;
            let raw_key = format!("ssh-ed25519 {key_b64}");
            let matches_key = validate_ed25519_public_key(&raw_key)
                .is_ok_and(|disk_key| disk_key.wire_blob == expected_key.wire_blob);
            if !matches_key {
                disk_key_mismatch = true;
            }
        }
    }
    if !saw_matching_id_line || disk_key_mismatch {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Device {device_name} ({pairing_id}) on disk does not match the public key recorded in devices.toml."
            ),
        ));
    }

    let exe = legion_core::current_exe()?;
    let shell = ForcedCommandShell::host()?;
    let mut probe = auth_keys;
    let probe_report = reconcile_authorized_keys_in_memory(
        &mut probe,
        &devices,
        &live_policy,
        paths,
        &target,
        caller_account.as_deref(),
        &exe,
        shell,
    )?;
    if probe_report.duplicate_key_ids.contains_key(pairing_id)
        || probe_report.foreign_key_ids.contains(pairing_id)
    {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Device {device_name} ({pairing_id}) is not active here due to a public key conflict."
            ),
        ));
    }
    if let Some(reason) = probe_report.principal_conflict_ids.get(pairing_id) {
        let _ = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false);
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("Device {device_name} ({pairing_id}) is not active here: {reason}."),
        ));
    }
    if probe_report.changed {
        let outcome = reconcile_devices_and_auth_keys_blocking(paths, node_name, node_file, false)?;
        if outcome
            .removed_targets
            .iter()
            .any(|t| t.pairing_id.as_deref() == Some(pairing_id))
        {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Device {device_name} ({pairing_id}) had a stale authorized_keys entry that was replaced; reconnect with the current key."
                ),
            ));
        }
        return Ok(outcome.removed_targets);
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
    /// Devices (disambiguated by pairing id) whose `authorized_keys` lines were removed or that
    /// just became tombstoned (callers should terminate live sessions for these targets).
    pub removed_targets: Vec<SessionTarget>,
    /// Warnings generated during `device:*` principal or `authorized_keys` reconciliation.
    pub warnings: Vec<String>,
}

/// Error returned by [`apply_devices_merge_blocking`], distinguishing failures before `devices.toml`
/// was saved from failures after `devices.toml` was durably updated (in which case callers MUST
/// still terminate active sessions for every newly tombstoned device).
#[derive(Clone, Debug)]
pub enum MergeApplyError {
    /// Nothing was written to `devices.toml`.
    BeforeSave(ErrorBody),
    /// `devices.toml` was saved with the tombstones, but a subsequent reconciliation step failed.
    AfterSave {
        /// Underlying failure from `policy.toml` or `authorized_keys` reconciliation.
        error: ErrorBody,
        /// Every previously live device that was just tombstoned in `devices.toml`.
        tombstoned: Vec<SessionTarget>,
    },
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
/// this node's current `devices.toml` (after adopting any legacy lines when `devices.toml` is
/// absent), verifying preconditions, preparing/validating incoming entries against the live
/// `policy.toml` and `devices.toml`, enforcing registry-owned principal uniqueness for every
/// caller, checking caller authorization, saving the merged `devices.toml` FIRST, then updating
/// registry-owned `device:*` principals in `policy.toml` and reconciling `authorized_keys`.
#[allow(clippy::too_many_arguments)]
pub fn apply_devices_merge_blocking(
    paths: &Paths,
    node_name: &str,
    node_file: &NodeFile,
    incoming: &DevicesFile,
    check_precondition: impl FnOnce(Option<&[u8]>) -> Result<(), ErrorBody>,
    prepare_incoming: impl FnOnce(&DevicesFile, &Policy, &mut DevicesFile) -> Result<(), ErrorBody>,
    authorize_change: impl Fn(MergeChange<'_>) -> Result<(), ErrorBody>,
) -> Result<MergeApplyOutcome, MergeApplyError> {
    let _cfg_guard = lock_config(paths).map_err(MergeApplyError::BeforeSave)?;

    let devices_path = paths.config_file(DevicesFile::FILE);
    let bytes_before = match std::fs::read(&devices_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(MergeApplyError::BeforeSave(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", devices_path.display()),
            )));
        }
    };
    check_precondition(bytes_before.as_deref()).map_err(MergeApplyError::BeforeSave)?;
    validate_incoming_timestamps(incoming, Timestamp::now())
        .map_err(MergeApplyError::BeforeSave)?;

    let policy_path = paths.config_file(Policy::FILE);
    let live_policy = match std::fs::read(&policy_path) {
        Ok(bytes) => parse::<Policy>(&policy_path, &bytes).map_err(MergeApplyError::BeforeSave)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Policy::default(),
        Err(error) => {
            return Err(MergeApplyError::BeforeSave(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", policy_path.display()),
            )));
        }
    };

    let had_existing_file = bytes_before.is_some();
    let local = match bytes_before.as_deref() {
        Some(bytes) => {
            parse::<DevicesFile>(&devices_path, bytes).map_err(MergeApplyError::BeforeSave)?
        }
        None => {
            let target = resolve_target(paths, node_file).map_err(MergeApplyError::BeforeSave)?;
            let caller_account =
                caller_account_for_target(&target).map_err(MergeApplyError::BeforeSave)?;
            let write_path = resolve_safe_write_path(&target, paths.home())
                .map_err(MergeApplyError::BeforeSave)?;
            let auth_keys = if write_path.is_file() {
                read_auth_keys(&write_path).map_err(MergeApplyError::BeforeSave)?
            } else {
                super::authkeys::AuthKeysFile::parse(b"")
            };
            let state = load_state(paths).unwrap_or_default();
            adopt_legacy_devices_from_auth_keys(
                paths,
                node_name,
                &auth_keys,
                &target,
                caller_account.as_deref(),
                &state,
                &live_policy,
            )
        }
    };

    let mut prepared_incoming = incoming.clone();
    prepare_incoming(&local, &live_policy, &mut prepared_incoming)
        .map_err(MergeApplyError::BeforeSave)?;

    let merged = merge_devices(&local, &prepared_incoming);

    // Every registry-owned (marked) principal belongs to exactly one pairing id: refuse, for
    // EVERY caller and at EVERY merge, any incoming or newly added/changed live entry whose
    // principal is marked for a different id in policy.toml, claimed by another live pairing in
    // devices.toml, or owned by a tombstoned pairing of a different id.
    let check_principal_ownership = |id: &str, entry: &FleetDevice| -> Result<(), ErrorBody> {
        if let Some(owner_id) = live_policy.device_owner(&entry.principal)
            && owner_id != id
        {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Principal {} in policy.toml is owned by pairing {owner_id} and cannot be assigned to pairing {id}.",
                    entry.principal
                ),
            )
            .with_hint(
                "Each registry-owned `device:*` principal belongs to exactly one pairing id.",
            ));
        }
        if entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
            if let Some((other_id, _)) = merged.devices.iter().find(|(other_id, other)| {
                other_id.as_str() != id && other.principal == entry.principal
            }) {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Principal {} is already claimed by live pairing {other_id} and cannot also be assigned to pairing {id}.",
                        entry.principal
                    ),
                )
                .with_hint(
                    "Each `device:*` principal belongs to exactly one pairing id.",
                ));
            }
            if let Some((tomb_id, _)) = merged
                .removed
                .iter()
                .find(|(tomb_id, tomb)| tomb_id.as_str() != id && tomb.principal == entry.principal)
            {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Principal {} previously belonged to removed pairing {tomb_id} and cannot be assigned to pairing {id}.",
                        entry.principal
                    ),
                )
                .with_hint(
                    "Each `device:*` principal belongs to exactly one pairing id and cannot be reused after removal.",
                ));
            }
        }
        Ok(())
    };

    for (id, inc) in &prepared_incoming.devices {
        if merged.removed.contains_key(id) {
            continue;
        }
        if local.devices.get(id) != Some(inc) {
            check_principal_ownership(id, inc).map_err(MergeApplyError::BeforeSave)?;
        }
    }
    for (id, entry) in &merged.devices {
        if local.devices.get(id) != Some(entry) {
            check_principal_ownership(id, entry).map_err(MergeApplyError::BeforeSave)?;
        }
    }

    // Refuse any merge that adds or changes a live entry whose public key collides with another
    // live entry in the merged view.
    let duplicate_keys =
        duplicate_live_public_keys(&merged).map_err(MergeApplyError::BeforeSave)?;
    for ids in duplicate_keys.values() {
        if let Some(changed_id) = ids
            .iter()
            .find(|id| local.devices.get(*id) != merged.devices.get(*id))
        {
            let name = merged
                .devices
                .get(changed_id)
                .map(|d| d.name.as_str())
                .unwrap_or("device");
            let others: Vec<String> = ids
                .iter()
                .filter(|id| *id != changed_id)
                .map(|id| {
                    merged
                        .devices
                        .get(id)
                        .map(|d| format!("{} ({id})", d.name))
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            return Err(MergeApplyError::BeforeSave(
                ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "Paired device {name} ({changed_id}) uses a public key already assigned to another live paired device: {}.",
                        others.join(", ")
                    ),
                )
                .with_hint(
                    "Remove the conflicting device or pair with a dedicated Ed25519 key pair.",
                ),
            ));
        }
    }

    // Authorize every live entry and tombstone that the merge adds or modifies before writing.
    for (id, entry) in &merged.devices {
        let previous = local.devices.get(id);
        if previous != Some(entry) {
            authorize_change(MergeChange::Live { entry, previous })
                .map_err(MergeApplyError::BeforeSave)?;
        }
    }
    for (id, tombstone) in &merged.removed {
        let previous_tomb = local.removed.get(id);
        let previous_live = local.devices.get(id);
        if previous_tomb != Some(tombstone) || previous_live.is_some() {
            authorize_change(MergeChange::Removed {
                tombstone,
                previous_live,
            })
            .map_err(MergeApplyError::BeforeSave)?;
        }
    }

    // Record every previously live device that this merge tombstones or newly quarantines so live
    // sessions are ALWAYS terminated once `devices.toml` has been saved, even if a later step fails.
    let quarantined_before = quarantined_devices(&local);
    let quarantined_after = quarantined_devices(&merged);
    let newly_tombstoned: Vec<SessionTarget> = local
        .devices
        .iter()
        .filter(|(id, _)| {
            merged.removed.contains_key(*id)
                || (quarantined_after.contains_key(*id) && !quarantined_before.contains_key(*id))
        })
        .map(|(id, prev)| SessionTarget::new(prev.name.clone(), Some(id.clone())))
        .collect();

    // Write `devices.toml` FIRST (tombstone first, then policy and authorized_keys).
    let canonical = save_devices_locked(paths, &merged).map_err(MergeApplyError::BeforeSave)?;
    let backup = had_existing_file.then(|| {
        paths
            .state_dir()
            .join("sync")
            .join("backups")
            .join("devices.toml.bak")
            .display()
            .to_string()
    });

    let after_save = |error: ErrorBody| MergeApplyError::AfterSave {
        error,
        tombstoned: newly_tombstoned.clone(),
    };

    let mut warnings = reconcile_device_principals_locked(paths, &merged).map_err(after_save)?;
    let reconciled_policy = match std::fs::read(&policy_path) {
        Ok(bytes) => parse::<Policy>(&policy_path, &bytes).map_err(after_save)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Policy::default(),
        Err(error) => {
            return Err(after_save(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", policy_path.display()),
            )));
        }
    };

    let target = resolve_target(paths, node_file).map_err(after_save)?;
    let caller_account = caller_account_for_target(&target).map_err(after_save)?;
    let write_path = resolve_safe_write_path(&target, paths.home()).map_err(after_save)?;
    verify_owner_matches_euid(
        &write_path,
        paths.home(),
        target.is_default_home_ssh,
        legion_os::user::effective_uid(),
    )
    .map_err(after_save)?;
    let _auth_guard = lock_auth_keys(&target, &write_path).map_err(after_save)?;

    let exe = legion_core::current_exe().map_err(after_save)?;
    let shell = ForcedCommandShell::host().map_err(after_save)?;
    let auth_report = edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
        let report = reconcile_authorized_keys_in_memory(
            &mut auth_keys,
            &merged,
            &reconciled_policy,
            paths,
            &target,
            caller_account.as_deref(),
            &exe,
            shell,
        )?;
        Ok((report.changed.then_some(auth_keys), report))
    })
    .map_err(after_save)?;
    warnings.extend(auth_report.warnings);

    let mut removed_set: BTreeSet<SessionTarget> =
        auth_report.removed_targets.into_iter().collect();
    for target_dev in newly_tombstoned {
        removed_set.insert(target_dev);
    }

    Ok(MergeApplyOutcome {
        devices: merged,
        canonical_toml: canonical,
        bytes_before,
        backup,
        removed_targets: removed_set.into_iter().collect(),
        warnings,
    })
}

/// Lists sorted peer machine names configured under `[machines]` in `fleet.toml`, excluding `here`.
pub fn fleet_peer_names(paths: &Paths, here: &str) -> Vec<String> {
    let fleet_path = paths.config_file("fleet.toml");
    let Ok(bytes) = std::fs::read(&fleet_path) else {
        return Vec::new();
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Vec::new();
    };
    let Ok(doc) = text.parse::<DocumentMut>() else {
        return Vec::new();
    };
    let Some(machines) = doc.get("machines").and_then(Item::as_table) else {
        return Vec::new();
    };
    let mut names: Vec<String> = machines
        .iter()
        .map(|(k, _)| k.to_owned())
        .filter(|name| name.as_str() != here)
        .collect();
    names.sort();
    names
}

/// Pushes `devices.toml` to every peer listed in `fleet.toml` in parallel with a short timeout,
/// returning a per-machine [`FleetPushOutcome`] map.
///
/// Returns an empty map ONLY when `fleet.toml` lists no other machines. Whenever `fleet.toml`
/// names peers and the push cannot run (for example `fleet.sync` is absent, the invite did not
/// record a creator principal, the caller lacks permission, or `fleet.sync` fails), every peer is
/// explicitly reported as `"not_reached"` with the reason — never silently omitted as `{}` — and
/// an invite without a recorded creator never falls back to `owner`.
pub async fn push_devices_to_fleet(
    node: &Node,
    ctx_opt: Option<&Ctx>,
    acting_principal: Option<&str>,
) -> BTreeMap<String, FleetPushOutcome> {
    let peers = fleet_peer_names(node.paths(), &node.name());
    if peers.is_empty() {
        return BTreeMap::new();
    }
    let all_not_reached = |reason: &str| -> BTreeMap<String, FleetPushOutcome> {
        peers
            .iter()
            .map(|peer| (peer.clone(), FleetPushOutcome::not_reached(reason)))
            .collect()
    };
    let Some(sync_tool) = node.registry().get("fleet.sync") else {
        return all_not_reached(
            "fleet.sync is not available on this node; run `legion fleet.sync direction=push documents='[\"devices.toml\"]'` manually",
        );
    };
    let args = json!({
        "direction": "push",
        "documents": [DevicesFile::FILE],
        "timeout_s": DEVICE_PUSH_TIMEOUT_S,
    });
    let reply = match ctx_opt {
        Some(ctx) => {
            if !ctx.grant().allows(sync_tool.spec()) {
                return all_not_reached(
                    "caller is not permitted to run fleet.sync; run `legion fleet.sync direction=push documents='[\"devices.toml\"]'` manually",
                );
            }
            sync_tool.call(ctx, args).await
        }
        None => {
            let Some(creator) = acting_principal else {
                return all_not_reached(
                    "cannot push automatically because this invite did not record who created it; run `legion fleet.sync direction=push documents='[\"devices.toml\"]'` manually",
                );
            };
            let mut call = Call::new(Door::Ssh, "fleet.sync", args);
            call.pinned = Some(creator.to_owned());
            node.call(call).await
        }
    };
    match reply {
        Ok(value) => {
            let mut outcomes = parse_fleet_push_outcomes(&value);
            for peer in &peers {
                outcomes.entry(peer.clone()).or_insert_with(|| {
                    FleetPushOutcome::not_reached("machine did not finish syncing devices.toml")
                });
            }
            outcomes
        }
        Err(error) => all_not_reached(&error.message),
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
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
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
        let principals = ["device:pixel", "device:ipad", "owner", "phone"];
        let roles: [Option<&str>; 4] = [Some("observer"), Some("editor"), Some("operator"), None];
        let nodes = ["tower", "pi", "mac"];
        // Small timestamp pools so tie-breaks trigger frequently across all remaining fields.
        let paired_times = ["2026-11-24T09:00:00Z", "2026-11-24T09:30:00Z"];
        let times = ["2026-11-24T10:00:00Z", "2026-11-24T10:00:00.500000000Z"];
        let creator_principals = ["owner", "device:pixel"];
        let confirmed_opts: [Option<&str>; 2] = [None, Some("owner")];

        let mut file = DevicesFile::default();
        for (idx, id) in ids.iter().enumerate() {
            match rng.next() % 4 {
                0 => {} // absent
                1 | 2 => {
                    let name = *rng.pick(&names);
                    let principal = *rng.pick(&principals);
                    let role = *rng.pick(&roles);
                    let paired_at = *rng.pick(&paired_times);
                    let paired_on = *rng.pick(&nodes);
                    let updated_at = *rng.pick(&times);
                    let updated_on = *rng.pick(&nodes);
                    let key_seed = ((idx + 1) as u8) * 10 + (rng.next() % 3) as u8;
                    let paired_by = match rng.next() % 3 {
                        0 => None,
                        1 => Some(PairedBy {
                            principal: (*rng.pick(&creator_principals)).to_owned(),
                            device: None,
                        }),
                        _ => Some(PairedBy {
                            principal: (*rng.pick(&creator_principals)).to_owned(),
                            device: Some(rng.pick(&ids).clone()),
                        }),
                    };
                    let confirmed_by = (*rng.pick(&confirmed_opts)).map(str::to_owned);
                    let confirmed_at = confirmed_by
                        .as_ref()
                        .map(|_| (*rng.pick(&times)).to_owned());
                    file.devices.insert(
                        id.clone(),
                        FleetDevice {
                            name: name.to_owned(),
                            public_key: sample_key(key_seed),
                            principal: principal.to_owned(),
                            role: role.map(str::to_owned),
                            paired_at: paired_at.to_owned(),
                            paired_on: paired_on.to_owned(),
                            paired_by,
                            confirmed_by,
                            confirmed_at,
                            updated_at: updated_at.to_owned(),
                            updated_on: updated_on.to_owned(),
                        },
                    );
                }
                _ => {
                    let name = *rng.pick(&names);
                    let principal = *rng.pick(&principals);
                    let paired_at = rng
                        .next()
                        .is_multiple_of(2)
                        .then(|| (*rng.pick(&paired_times)).to_owned());
                    let paired_on = rng
                        .next()
                        .is_multiple_of(2)
                        .then(|| (*rng.pick(&nodes)).to_owned());
                    let removed_at = *rng.pick(&times);
                    let removed_on = *rng.pick(&nodes);
                    file.removed.insert(
                        id.clone(),
                        DeviceTombstone {
                            name: name.to_owned(),
                            principal: principal.to_owned(),
                            paired_at,
                            paired_on,
                            removed_at: removed_at.to_owned(),
                            removed_on: removed_on.to_owned(),
                        },
                    );
                }
            }
        }
        file
    }

    #[test]
    fn merge_tie_breaks_cover_every_field_and_stay_commutative() {
        let id = sample_id(9);
        let base = FleetDevice {
            name: "alpha".to_owned(),
            public_key: sample_key(10),
            principal: "device:alpha".to_owned(),
            role: Some("observer".to_owned()),
            paired_at: "2026-11-24T09:00:00Z".to_owned(),
            paired_on: "mac".to_owned(),
            paired_by: Some(PairedBy {
                principal: "owner".to_owned(),
                device: None,
            }),
            confirmed_by: None,
            confirmed_at: None,
            updated_at: "2026-11-24T10:00:00Z".to_owned(),
            updated_on: "pi".to_owned(),
        };

        let mut variants: Vec<FleetDevice> = Vec::new();
        let mut push_variant = |mutate: fn(&mut FleetDevice)| {
            let mut v = base.clone();
            mutate(&mut v);
            variants.push(v);
        };
        push_variant(|d| d.updated_on = "tower".to_owned());
        push_variant(|d| d.name = "beta".to_owned());
        push_variant(|d| d.role = Some("editor".to_owned()));
        push_variant(|d| d.principal = "device:beta".to_owned());
        push_variant(|d| d.public_key = sample_key(11));
        push_variant(|d| d.paired_at = "2026-11-24T09:30:00Z".to_owned());
        push_variant(|d| d.paired_on = "tower".to_owned());
        push_variant(|d| {
            d.paired_by = Some(PairedBy {
                principal: "owner".to_owned(),
                device: Some(sample_id(1)),
            });
        });
        push_variant(|d| {
            d.confirmed_by = Some("owner".to_owned());
            d.confirmed_at = Some("2026-11-24T10:00:00Z".to_owned());
        });
        push_variant(|d| {
            d.confirmed_by = Some("owner".to_owned());
            d.confirmed_at = Some("2026-11-24T10:00:01Z".to_owned());
        });

        let mut base_file = DevicesFile::default();
        base_file.devices.insert(id.clone(), base.clone());
        for variant in &variants {
            let mut var_file = DevicesFile::default();
            var_file.devices.insert(id.clone(), variant.clone());
            let ab = merge_devices(&base_file, &var_file);
            let ba = merge_devices(&var_file, &base_file);
            assert_eq!(ab, ba, "tie-break must be commutative for {variant:?}");
            assert_eq!(ab.to_toml_string(), ba.to_toml_string());
        }

        // Same check for every tombstone tie-break field.
        let tomb_base = DeviceTombstone {
            name: "alpha".to_owned(),
            principal: "device:alpha".to_owned(),
            paired_at: Some("2026-11-24T09:00:00Z".to_owned()),
            paired_on: Some("mac".to_owned()),
            removed_at: "2026-11-24T10:00:00Z".to_owned(),
            removed_on: "pi".to_owned(),
        };
        let mut tomb_variants: Vec<DeviceTombstone> = Vec::new();
        let mut push_tomb = |mutate: fn(&mut DeviceTombstone)| {
            let mut t = tomb_base.clone();
            mutate(&mut t);
            tomb_variants.push(t);
        };
        push_tomb(|t| t.removed_on = "tower".to_owned());
        push_tomb(|t| t.name = "beta".to_owned());
        push_tomb(|t| t.principal = "device:beta".to_owned());
        push_tomb(|t| t.paired_at = Some("2026-11-24T09:30:00Z".to_owned()));
        push_tomb(|t| t.paired_on = Some("tower".to_owned()));

        let mut tomb_base_file = DevicesFile::default();
        tomb_base_file.removed.insert(id.clone(), tomb_base);
        for variant in &tomb_variants {
            let mut var_file = DevicesFile::default();
            var_file.removed.insert(id.clone(), variant.clone());
            let ab = merge_devices(&tomb_base_file, &var_file);
            let ba = merge_devices(&var_file, &tomb_base_file);
            assert_eq!(
                ab, ba,
                "tombstone tie-break must be commutative for {variant:?}"
            );
        }
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
        let pixel_id = sample_id(1);
        std::fs::write(
            paths.config_file("policy.toml"),
            format!(
                "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[roles.editor]
allow = [\"effect:read\", \"effect:write\"]

# Stale leftover owner entry owned by device:pixel's pairing id must be downgraded when syncing an observer device!
[principals.\"device:pixel\"]
role = \"owner\"
device = \"{pixel_id}\"

# Custom non-device principal must never be touched by device reconciliation.
[principals.phone]
role = \"editor\"
"
            ),
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
        let legacy_key = sample_key(7);
        let legacy_fp = validate_ed25519_public_key(&legacy_key)
            .unwrap()
            .fingerprint;
        let legacy_line = format_device_line(
            &exe,
            "phone",
            &legacy_key,
            "legacy-phone",
            &legacy_id,
            &PinnedHome::from_paths(&paths),
            None,
            shell,
        )
        .unwrap();
        std::fs::write(&auth_path, format!("{foreign_line}\n{legacy_line}\n")).unwrap();
        let mut legacy_state = DevicesState::default();
        legacy_state.devices.insert(
            "legacy-phone".to_owned(),
            crate::device::store::DeviceRecord {
                name: "legacy-phone".to_owned(),
                principal: "phone".to_owned(),
                added: "2026-11-24T10:00:00Z".to_owned(),
                fingerprint: legacy_fp,
                pairing_id: Some(legacy_id.clone()),
            },
        );
        crate::device::store::save_state(&paths, &legacy_state).unwrap();

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
        assert_eq!(
            after.removed_targets,
            vec![SessionTarget::new("legacy-phone", Some(legacy_id.clone()))]
        );
        let auth_after = std::fs::read_to_string(&auth_path).unwrap();
        assert!(auth_after.contains(&foreign_line));
        assert!(!auth_after.contains("legion-device:legacy-phone"));
    }

    #[test]
    fn future_timestamps_beyond_skew_and_max_time_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_path = dir.path().join("authorized_keys");
        let mut node_file = NodeFile::default();
        node_file.devices.authorized_keys = Some(auth_path.display().to_string());

        let now = Timestamp::now();
        let far_future = (now + SignedDuration::from_mins(20)).to_string();
        let max_time = Timestamp::MAX.to_string();

        for (label, incoming) in [
            ("live updated_at far future", {
                let mut f = DevicesFile::default();
                let mut d = sample_device(1, "pixel", Some("observer"), &far_future, "pi");
                d.paired_at = now.to_string();
                f.devices.insert(sample_id(1), d);
                f
            }),
            ("live updated_at Timestamp::MAX", {
                let mut f = DevicesFile::default();
                let mut d = sample_device(2, "pixel", Some("observer"), &max_time, "pi");
                d.paired_at = now.to_string();
                f.devices.insert(sample_id(2), d);
                f
            }),
            ("live paired_at far future", {
                let mut f = DevicesFile::default();
                let mut d = sample_device(3, "pixel", Some("observer"), &now.to_string(), "pi");
                d.paired_at = far_future.clone();
                f.devices.insert(sample_id(3), d);
                f
            }),
            ("tombstone removed_at far future", {
                let mut f = DevicesFile::default();
                f.removed
                    .insert(sample_id(4), sample_tombstone("pixel", &far_future, "pi"));
                f
            }),
            ("tombstone removed_at Timestamp::MAX", {
                let mut f = DevicesFile::default();
                f.removed
                    .insert(sample_id(5), sample_tombstone("pixel", &max_time, "pi"));
                f
            }),
            ("tombstone paired_at far future", {
                let mut f = DevicesFile::default();
                let mut t = sample_tombstone("pixel", &now.to_string(), "pi");
                t.paired_at = Some(far_future.clone());
                f.removed.insert(sample_id(6), t);
                f
            }),
        ] {
            let err = match apply_devices_merge_blocking(
                &paths,
                "tower",
                &node_file,
                &incoming,
                |_| Ok(()),
                |_, _, _| Ok(()),
                |_| Ok(()),
            ) {
                Err(MergeApplyError::BeforeSave(err)) => err,
                other => panic!("{label}: expected BeforeSave error, got {other:?}"),
            };
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{label}: {err:?}");
        }

        // Timestamps within the 10-minute skew allowance are accepted.
        let within_skew = (now + SignedDuration::from_mins(5)).to_string();
        let mut ok_file = DevicesFile::default();
        let mut d = sample_device(7, "pixel", Some("observer"), &within_skew, "pi");
        d.paired_at = within_skew.clone();
        ok_file.devices.insert(sample_id(7), d);
        validate_incoming_timestamps(&ok_file, now).unwrap();

        // next_updated_timestamp returns an error at Timestamp::MAX rather than going backwards.
        let err = next_updated_timestamp_at(Some(&max_time), now).unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed, "{err:?}");
        assert!(
            err.message.contains("maximum representable time"),
            "{}",
            err.message
        );
        // Normal advancement moves strictly forward by 1ns when now <= previous.
        let prev = "2026-11-24T18:00:00Z";
        let prev_ts: Timestamp = prev.parse().unwrap();
        let next = next_updated_timestamp_at(Some(prev), prev_ts).unwrap();
        let next_ts: Timestamp = next.parse().unwrap();
        assert!(next_ts > prev_ts);
    }

    #[test]
    fn unmarked_device_principal_is_left_untouched_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[principals.\"device:legacy\"]
role = \"observer\"
",
        )
        .unwrap();

        let mut devices = DevicesFile::default();
        let mut dev = sample_device(1, "legacy", Some("owner"), "2026-11-24T10:00:00Z", "pi");
        dev.principal = "device:legacy".to_owned();
        devices.devices.insert(sample_id(1), dev);

        let _lock = lock_config(&paths).unwrap();
        let warnings = reconcile_device_principals_locked(&paths, &devices).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("device:legacy")),
            "expected warning for unmarked device:legacy, got {warnings:?}"
        );
        let policy = parse::<Policy>(
            &paths.config_file("policy.toml"),
            &std::fs::read(paths.config_file("policy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            policy.role_of("device:legacy").0,
            "observer",
            "unmarked pre-existing device:legacy principal must not be raised to owner"
        );
    }

    #[test]
    fn verify_gate_device_refuses_mismatched_key_on_disk_and_reconciles() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let auth_keys = dir.path().join("authorized_keys");
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
        let node_toml =
            format!("name = \"tower\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\n");
        std::fs::write(paths.config_file("node.toml"), &node_toml).unwrap();
        let node_file: NodeFile =
            parse(&paths.config_file("node.toml"), node_toml.as_bytes()).unwrap();

        let id_x = sample_id(10);
        let k1 = ed25519_public_key_from_seed(&[1u8; 32]);
        let k2 = ed25519_public_key_from_seed(&[2u8; 32]);

        // Put a stale K1 line for pairing id X on disk in authorized_keys.
        let exe = legion_core::current_exe().unwrap();
        let shell = ForcedCommandShell::host().unwrap();
        let pinned_home = PinnedHome::from_paths(&paths);
        let stale_line = format_device_line(
            &exe,
            "device:pixel",
            &k1,
            "pixel",
            &id_x,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        std::fs::write(&auth_keys, format!("{stale_line}\n")).unwrap();

        // Registry says K2 for the exact same pairing id X, name, and principal.
        let mut devices = DevicesFile::default();
        let mut dev = sample_device(10, "pixel", Some("observer"), "2026-11-24T10:00:00Z", "pi");
        dev.public_key = k2.clone();
        devices.devices.insert(id_x.clone(), dev);
        std::fs::write(paths.config_file("devices.toml"), devices.to_toml_string()).unwrap();

        // Gate check for a session authenticated via the stale K1 line MUST return Denied
        // AND reconcile authorized_keys to K2!
        let err = verify_gate_device(
            &paths,
            "tower",
            &node_file,
            "pixel",
            &id_x,
            "device:pixel",
            None,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{err:?}");

        let updated = std::fs::read_to_string(&auth_keys).unwrap();
        assert!(updated.contains(&k2), "expected K2 on disk:\n{updated}");
        assert!(
            !updated.contains(&k1),
            "expected K1 removed from disk:\n{updated}"
        );

        // Once disk matches K2, a subsequent gate check succeeds.
        verify_gate_device(
            &paths,
            "tower",
            &node_file,
            "pixel",
            &id_x,
            "device:pixel",
            None,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn failed_push_and_missing_creator_report_every_fleet_machine_not_reached() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path().join("legion"));
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            paths.config_file("fleet.toml"),
            "[machines.pi]\ncommand = [\"echo\"]\n\n[machines.nas]\ncommand = [\"echo\"]\n",
        )
        .unwrap();

        let node = Node::new(paths.clone(), crate::registry());
        // No `fleet.sync` registered AND no recorded creator -> must report EVERY fleet peer as
        // `not_reached` with a reason, never an empty map `{}`.
        let outcomes = push_devices_to_fleet(&node, None, None).await;
        assert_eq!(
            outcomes.len(),
            2,
            "expected both peers reported: {outcomes:?}"
        );
        for peer in ["pi", "nas"] {
            let entry = outcomes.get(peer).expect("peer must be present");
            assert_eq!(entry.status, "not_reached", "{peer}: {entry:?}");
            assert!(
                entry.reason.as_deref().is_some_and(|r| !r.is_empty()),
                "{peer}: {entry:?}"
            );
        }

        // Even when `fleet.sync` IS registered, a missing creator principal (`acting_principal = None`)
        // MUST refuse to push (never assuming `owner`) and report every peer as `not_reached`.
        struct StubFailingSync {
            spec: legion_core::proto::ToolSpec,
            invoked: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl legion_core::Tool for StubFailingSync {
            fn spec(&self) -> &legion_core::proto::ToolSpec {
                &self.spec
            }
            fn call<'a>(
                &'a self,
                _ctx: &'a Ctx,
                _args: Value,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<Value, ErrorBody>> + Send + 'a>,
            > {
                let flag = std::sync::Arc::clone(&self.invoked);
                Box::pin(async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Err(ErrorBody::new(
                        ErrorCode::Failed,
                        "simulated fleet.sync failure",
                    ))
                })
            }
        }
        let mut reg_with_sync = crate::registry();
        let invoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        reg_with_sync.add(StubFailingSync {
            spec: legion_core::proto::ToolSpec {
                name: "fleet.sync".to_owned(),
                description: "stub sync".to_owned(),
                effect: legion_core::proto::Effect::Admin,
                input_schema: json!({"type": "object"}),
            },
            invoked: std::sync::Arc::clone(&invoked),
        });
        let node_with_sync = Node::new(paths.clone(), reg_with_sync);
        let no_creator = push_devices_to_fleet(&node_with_sync, None, None).await;
        assert!(
            !invoked.load(std::sync::atomic::Ordering::SeqCst),
            "push_devices_to_fleet must NOT invoke fleet.sync when acting_principal is None"
        );
        assert_eq!(no_creator.len(), 2, "{no_creator:?}");
        for peer in ["pi", "nas"] {
            let entry = no_creator.get(peer).expect("peer must be present");
            assert_eq!(entry.status, "not_reached", "{peer}: {entry:?}");
            assert!(
                entry
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("did not record who created it")),
                "{peer}: {entry:?}"
            );
        }

        // When `acting_principal` IS present and `fleet.sync` returns an error, every peer is still
        // reported as `not_reached` carrying the error message (never silently omitted as `{}`).
        let failed = push_devices_to_fleet(&node_with_sync, None, Some("owner")).await;
        assert!(invoked.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(failed.len(), 2, "{failed:?}");
        for peer in ["pi", "nas"] {
            let entry = failed.get(peer).expect("peer must be present");
            assert_eq!(entry.status, "not_reached", "{peer}: {entry:?}");
            assert!(
                entry
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("simulated fleet.sync failure")),
                "{peer}: {entry:?}"
            );
        }
    }
}
