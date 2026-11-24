//! Persistent device and pending-invite metadata under `state/`, locking, sweeping, and name
//! sanitisation.
//!
//! `authorized_keys` is always the source of truth for which keys exist; `state/devices.json`
//! holds non-secret metadata (principals, timestamps, fingerprints) and never stores private keys
//! or invite strings.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::atomic::{WriteOptions, write_atomic};
use legion_os::lock::{FileLock, LockMode};
use serde::{Deserialize, Serialize};

use super::authkeys::{AuthKeysFile, LineKind};

/// Longest time a device operation waits for another holding `state/devices.lock`.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum length of the sanitised base slug of a device name before any numeric suffix.
const MAX_DEVICE_SLUG_LEN: usize = 32;

/// Persistent non-secret metadata stored at `state/devices.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DevicesState {
    /// Schema version (`1`).
    #[serde(default = "default_version")]
    pub version: u32,
    /// Pending single-use invites, keyed by invite id. Never stores keys or invite text.
    #[serde(default)]
    pub invites: BTreeMap<String, InviteRecord>,
    /// Paired devices, keyed by unique device name.
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceRecord>,
}

fn default_version() -> u32 {
    1
}

/// Non-secret metadata for one pending invite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteRecord {
    /// Invite identifier.
    pub id: String,
    /// Principal the enrolled device will act as.
    pub principal: String,
    /// RFC 3339 UTC creation timestamp.
    pub created: String,
    /// RFC 3339 UTC expiry timestamp.
    pub expires: String,
    /// `SHA256:...` fingerprint of the temporary public key.
    pub fingerprint: String,
}

/// Non-secret metadata for one paired device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// Unique sanitised device name (matches `legion-device:<name>`).
    pub name: String,
    /// Principal the device's forced command pins.
    pub principal: String,
    /// RFC 3339 UTC timestamp when the device enrolled.
    pub added: String,
    /// `SHA256:...` fingerprint of the device's public key.
    pub fingerprint: String,
}

/// Acquires the exclusive kernel lock guarding both `authorized_keys` and `state/devices.json`.
pub fn lock_devices(paths: &Paths) -> Result<FileLock, ErrorBody> {
    let lock = lock_path(paths);
    if let Some(dir) = lock.parent() {
        std::fs::create_dir_all(dir).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", dir.display()),
            )
        })?;
    }
    FileLock::acquire_timeout(&lock, LockMode::Exclusive, LOCK_TIMEOUT).map_err(|error| match error
    {
        OsError::TimedOut { .. } => ErrorBody::new(
            ErrorCode::Busy,
            "Another call has been changing paired devices for too long.",
        )
        .with_hint("Try again in a moment."),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be locked: {other}.", lock.display()),
        ),
    })
}

/// Loads `state/devices.json`, returning an empty state when the file does not exist.
pub fn load_state(paths: &Paths) -> Result<DevicesState, ErrorBody> {
    let path = state_path(paths);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be parsed: {error}.", path.display()),
            )
            .with_hint("Fix or remove the damaged devices state file and try again.")
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DevicesState {
            version: 1,
            invites: BTreeMap::new(),
            devices: BTreeMap::new(),
        }),
        Err(error) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be read: {error}.", path.display()),
        )),
    }
}

/// Writes `state/devices.json` atomically with restrictive permissions (`0o600` on Unix).
pub fn save_state(paths: &Paths, state: &DevicesState) -> Result<(), ErrorBody> {
    let path = state_path(paths);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", dir.display()),
            )
        })?;
    }
    let mut bytes = serde_json::to_vec_pretty(state).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Device state cannot be serialized: {error}."),
        )
    })?;
    bytes.push(b'\n');
    write_atomic(
        &path,
        &bytes,
        WriteOptions::with_mode(0o600).force_mode(true),
    )
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", path.display()),
        )
    })
}

/// Sweeps expired or used invites from both `auth_keys` and `state`, and prunes device records
/// whose `authorized_keys` line was removed externally.
///
/// Returns `(auth_keys_changed, state_changed)`.
pub fn sweep(
    auth_keys: &mut AuthKeysFile,
    state: &mut DevicesState,
    now: Timestamp,
) -> (bool, bool) {
    let auth_invite_ids: HashSet<String> = auth_keys
        .lines
        .iter()
        .filter_map(|line| match &line.kind {
            LineKind::Invite { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let auth_device_names: HashSet<String> = auth_keys
        .lines
        .iter()
        .filter_map(|line| match &line.kind {
            LineKind::Device { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();

    // An invite is alive only when it exists in BOTH authorized_keys and state and has not expired.
    let mut active_invite_ids = HashSet::new();
    for (id, record) in &state.invites {
        if !auth_invite_ids.contains(id) {
            continue;
        }
        if parse_rfc3339(&record.expires).is_some_and(|exp| now < exp) {
            active_invite_ids.insert(id.clone());
        }
    }

    let auth_changed = auth_keys.retain(|line| match &line.kind {
        LineKind::Invite {
            id, expiry_time, ..
        } => {
            if !active_invite_ids.contains(id) {
                return false;
            }
            if let Some(exp) = expiry_time
                && now >= *exp
            {
                return false;
            }
            true
        }
        LineKind::Device { .. } | LineKind::Foreign => true,
    });

    // Recompute surviving invite ids in case the line's own expiry-time had already elapsed.
    let surviving_invites: HashSet<String> = auth_keys
        .lines
        .iter()
        .filter_map(|line| match &line.kind {
            LineKind::Invite { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();

    let invites_before = state.invites.len();
    state.invites.retain(|id, _| surviving_invites.contains(id));
    let devices_before = state.devices.len();
    state
        .devices
        .retain(|name, _| auth_device_names.contains(name));
    let state_changed =
        state.invites.len() != invites_before || state.devices.len() != devices_before;

    (auth_changed, state_changed)
}

/// Formats `time` as RFC 3339 UTC to the second (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_rfc3339(time: Timestamp) -> String {
    time.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Parses an RFC 3339 UTC timestamp string.
pub fn parse_rfc3339(text: &str) -> Option<Timestamp> {
    text.parse::<Timestamp>().ok()
}

/// Sanitises an arbitrary device name into a short, safe lowercase ASCII slug (`[a-z0-9-]`).
pub fn sanitise_device_name(raw: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
        if slug.len() >= MAX_DEVICE_SLUG_LEN {
            break;
        }
    }
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "device".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Sanitises `raw` and appends `-2`, `-3`, ... when needed so the result is not in `existing`.
pub fn unique_device_name(raw: &str, existing: &HashSet<String>) -> String {
    let base = sanitise_device_name(raw);
    if !existing.contains(&base) {
        return base;
    }
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !existing.contains(&candidate) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

fn lock_path(paths: &Paths) -> PathBuf {
    paths.state_dir().join("devices.lock")
}

fn state_path(paths: &Paths) -> PathBuf {
    paths.state_dir().join("devices.json")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::Path;

    use super::*;
    use crate::device::authkeys::format_invite_line;
    use crate::device::keys::generate_ephemeral_ed25519;

    #[test]
    fn sanitises_and_deduplicates_device_names() {
        assert_eq!(sanitise_device_name("My Pixel 10 Pro!"), "my-pixel-10-pro");
        assert_eq!(sanitise_device_name("   ---   "), "device");
        assert_eq!(sanitise_device_name("phone\ninjected"), "phone-injected");
        assert_eq!(sanitise_device_name("a".repeat(60).as_str()).len(), 32);

        let mut existing = HashSet::new();
        let first = unique_device_name("Pixel", &existing);
        assert_eq!(first, "pixel");
        existing.insert(first);
        let second = unique_device_name("Pixel", &existing);
        assert_eq!(second, "pixel-2");
        existing.insert(second);
        assert_eq!(unique_device_name("PIXEL!!", &existing), "pixel-3");
    }

    #[test]
    fn sweep_removes_expired_and_orphaned_invites_while_preserving_foreign_and_valid() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let past: Timestamp = "2026-11-24T11:59:00Z".parse().unwrap();
        let future: Timestamp = "2026-11-24T12:15:00Z".parse().unwrap();

        let key1 = generate_ephemeral_ed25519().unwrap();
        let key2 = generate_ephemeral_ed25519().unwrap();
        let key3 = generate_ephemeral_ed25519().unwrap();

        let expired_line = format_invite_line(
            Path::new("/home/me/legion"),
            "invexpired",
            past,
            &key1.public_key,
            false,
        )
        .unwrap();
        let orphan_line = format_invite_line(
            Path::new("/home/me/legion"),
            "invorphan",
            future,
            &key2.public_key,
            false,
        )
        .unwrap();
        let valid_line = format_invite_line(
            Path::new("/home/me/legion"),
            "invvalid",
            future,
            &key3.public_key,
            false,
        )
        .unwrap();

        let initial = format!("# keep me\n{expired_line}\n{orphan_line}\n{valid_line}\n");
        let mut auth = AuthKeysFile::parse(initial.as_bytes());
        let mut state = DevicesState {
            version: 1,
            invites: BTreeMap::from([
                (
                    "invexpired".to_owned(),
                    InviteRecord {
                        id: "invexpired".to_owned(),
                        principal: "phone".to_owned(),
                        created: "2026-11-24T11:44:00Z".to_owned(),
                        expires: format_rfc3339(past),
                        fingerprint: key1.fingerprint,
                    },
                ),
                (
                    "invvalid".to_owned(),
                    InviteRecord {
                        id: "invvalid".to_owned(),
                        principal: "phone".to_owned(),
                        created: "2026-11-24T11:55:00Z".to_owned(),
                        expires: format_rfc3339(future),
                        fingerprint: key3.fingerprint,
                    },
                ),
                (
                    "invmissingline".to_owned(),
                    InviteRecord {
                        id: "invmissingline".to_owned(),
                        principal: "phone".to_owned(),
                        created: "2026-11-24T11:55:00Z".to_owned(),
                        expires: format_rfc3339(future),
                        fingerprint: "SHA256:abc".to_owned(),
                    },
                ),
            ]),
            devices: BTreeMap::new(),
        };

        let (auth_changed, state_changed) = sweep(&mut auth, &mut state, now);
        assert!(auth_changed);
        assert!(state_changed);
        assert_eq!(state.invites.len(), 1);
        assert!(state.invites.contains_key("invvalid"));
        let out = String::from_utf8(auth.to_bytes()).unwrap();
        assert!(out.starts_with("# keep me\n"));
        assert!(out.contains("legion-invite:invvalid"));
        assert!(!out.contains("invexpired"));
        assert!(!out.contains("invorphan"));

        // Second sweep is a no-op.
        assert_eq!(sweep(&mut auth, &mut state, now), (false, false));
    }
}
