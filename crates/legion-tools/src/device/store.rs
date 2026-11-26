//! Persistent device and pending-invite metadata under `state/`, locking, sweeping, and name
//! sanitisation.
//!
//! `authorized_keys` is always the source of truth for which keys exist; `state/devices.json`
//! holds non-secret metadata (principals, timestamps, fingerprints) and never stores private keys
//! or invite strings.

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::atomic::{WriteOptions, write_atomic};
use legion_os::lock::{FileLock, LockMode};
use serde::{Deserialize, Serialize};

use super::authkeys::{AuthKeysFile, AuthKeysTarget, LineKind, ensure_parent_dir};

/// Longest time a device operation waits for another holding the `authorized_keys` lock.
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
    /// Local-only records for devices enrolled on THIS machine that have not yet opened an
    /// authenticated `legion gate` session with their permanent key, keyed by `pairing_id`.
    ///
    /// Written to `state/devices.json` before `devices.toml`, `policy.toml`, and `authorized_keys`
    /// are mutated during enrolment so any crash mid-enrolment can be repaired on the next run,
    /// and never written into the fleet-synced `devices.toml`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unconfirmed: BTreeMap<String, UnconfirmedRecord>,
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
    /// Role recorded when the invite was created for a `device:*` principal (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Whether enrolment should create the dedicated `device:<name>` principal in `policy.toml`
    /// from `role` under the config lock (rather than reusing a pre-existing principal).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub create_principal: bool,
    /// Preferred device label when chosen at invite creation time (`--name`, optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    /// Principal that created this invite, recorded in `paired_by` and device-registry push audit logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// Pairing identifier of the device that created this invite, when created by a paired device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_device: Option<String>,
    /// Name of the paired device that created this invite, when created by a paired device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by_device_name: Option<String>,
    /// RFC 3339 UTC creation timestamp.
    pub created: String,
    /// RFC 3339 UTC expiry timestamp.
    pub expires: String,
    /// `SHA256:...` fingerprint of the temporary public key.
    pub fingerprint: String,
    /// Sorted, deduplicated `SHA256:...` fingerprints of the host keys pinned into this invite,
    /// used to verify proof-of-possession signatures bound to this machine identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_key_fingerprints: Vec<String>,
}

/// Local-only record of an enrolment on THIS machine awaiting its first authenticated session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnconfirmedRecord {
    /// Pairing identifier allocated for this enrolment.
    pub pairing_id: String,
    /// Single-use invite identifier that created this enrolment, used to remove a leftover invite
    /// if the process was killed after `devices.toml` committed.
    pub invite_id: String,
    /// Unique sanitised device name chosen for this enrolment.
    pub name: String,
    /// Principal pinned for this enrolment.
    pub principal: String,
    /// `SHA256:...` fingerprint of the enrolled device public key.
    pub fingerprint: String,
    /// RFC 3339 UTC timestamp when enrolment began.
    pub created: String,
    /// Previous unconfirmed leftover's `pairing_id` that this enrolment is replacing, if any.
    ///
    /// Step 1 records this without removing the old `state.unconfirmed` entry; if the process is
    /// killed before step 2 commits `devices.toml`, repair rolls back by dropping only this new
    /// record so the old leftover stays unconfirmed and the invite stays usable. Once step 2
    /// commits, repair (and step 4) rolls forward by removing the invite, dropping the replaced
    /// record, and restoring the new permanent `authorized_keys` line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_pairing_id: Option<String>,
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
    /// Random per-pairing identifier pinned via `--pairing <id>` in `authorized_keys`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing_id: Option<String>,
}

/// Returns the owner-only lock file path placed beside `write_path` so distinct Legion homes
/// sharing one `authorized_keys` file serialize edits through the same lock.
pub fn auth_keys_lock_path(write_path: &Path) -> PathBuf {
    let base = write_path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or("authorized_keys");
    let file = format!(".{base}.legion.lock");
    match write_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(file),
        _ => PathBuf::from(file),
    }
}

/// Acquires the exclusive owner-only kernel lock beside `write_path` guarding `authorized_keys`
/// edits across all Legion homes sharing that file.
pub fn lock_auth_keys(target: &AuthKeysTarget, write_path: &Path) -> Result<FileLock, ErrorBody> {
    let lock = auth_keys_lock_path(write_path);
    if let Some(dir) = lock.parent()
        && !dir.as_os_str().is_empty()
    {
        ensure_parent_dir(
            dir,
            target.is_default_home_ssh && target.path.parent() == Some(dir),
        )?;
    }
    let guard = FileLock::acquire_private_timeout(&lock, LockMode::Exclusive, LOCK_TIMEOUT)
        .map_err(|error| match error {
            OsError::TimedOut { .. } => ErrorBody::new(
                ErrorCode::Busy,
                "Another call has been changing paired devices for too long.",
            )
            .with_hint("Try again in a moment."),
            other => ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be locked: {other}.", lock.display()),
            ),
        })?;
    if target.windows_admin_acl {
        legion_os::user::apply_windows_admin_ssh_acl(&lock).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Cannot secure lock file {} with Administrators/SYSTEM ACL: {error}.",
                    lock.display()
                ),
            )
            .with_hint(
                "Run from an elevated Administrator terminal so Legion can update administrators_authorized_keys.",
            )
        })?;
    }
    Ok(guard)
}

/// Creates a private directory (`0o700` on Unix) for state files.
pub fn ensure_private_dir(dir: &Path) -> Result<(), ErrorBody> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.exists() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(dir).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be created: {error}.", dir.display()),
                )
            })?;
        }
        if let Ok(meta) = std::fs::metadata(dir)
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", dir.display()),
            )
        })
    }
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
            unconfirmed: BTreeMap::new(),
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
        ensure_private_dir(dir)?;
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

/// Sweeps expired or used invites recorded by **this** Legion home from `auth_keys` and `state`,
/// and prunes device records whose `authorized_keys` line was removed externally.
///
/// `legion-invite:` lines whose id was not recorded in `state.invites` are left untouched so
/// another Legion home sharing the same `authorized_keys` file never loses its pending invites.
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

    // Classify only invite ids this home knows about: expired ones are removed from authorized_keys
    // and state; missing ones (used or deleted externally) are pruned from state; unknown invite
    // lines in authorized_keys belong to another home and are never deleted.
    let mut expired_owned_ids = HashSet::new();
    let mut active_owned_ids = HashSet::new();
    for (id, record) in &state.invites {
        if !auth_invite_ids.contains(id) {
            continue;
        }
        if parse_rfc3339(&record.expires).is_some_and(|exp| now < exp) {
            active_owned_ids.insert(id.clone());
        } else {
            expired_owned_ids.insert(id.clone());
        }
    }

    let auth_changed = auth_keys.retain(|line| match &line.kind {
        LineKind::Invite { id, .. } => !expired_owned_ids.contains(id),
        LineKind::Device { .. } | LineKind::Foreign => true,
    });

    let invites_before = state.invites.len();
    state.invites.retain(|id, _| active_owned_ids.contains(id));
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

pub(crate) fn state_path(paths: &Paths) -> PathBuf {
    paths.state_dir().join("devices.json")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::path::{Path, PathBuf};

    use super::*;
    use crate::device::authkeys::{ForcedCommandShell, PinnedHome, format_invite_line};
    use crate::device::keys::generate_ephemeral_ed25519;

    #[test]
    fn sanitises_and_deduplicates_device_names() {
        assert_eq!(sanitise_device_name("My Phone 12 Max!"), "my-phone-12-max");
        assert_eq!(sanitise_device_name("   ---   "), "device");
        assert_eq!(sanitise_device_name("phone\ninjected"), "phone-injected");
        assert_eq!(sanitise_device_name("a".repeat(60).as_str()).len(), 32);

        let mut existing = HashSet::new();
        let first = unique_device_name("Handset", &existing);
        assert_eq!(first, "handset");
        existing.insert(first);
        let second = unique_device_name("Handset", &existing);
        assert_eq!(second, "handset-2");
        existing.insert(second);
        assert_eq!(unique_device_name("HANDSET!!", &existing), "handset-3");
    }

    #[test]
    fn sweep_removes_only_this_homes_expired_invites_and_preserves_other_homes() {
        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let past: Timestamp = "2026-11-24T11:59:00Z".parse().unwrap();
        let future: Timestamp = "2026-11-24T12:15:00Z".parse().unwrap();

        let key1 = generate_ephemeral_ed25519().unwrap();
        let key2 = generate_ephemeral_ed25519().unwrap();
        let key3 = generate_ephemeral_ed25519().unwrap();

        let default_home = PinnedHome::Split {
            config_dir: PathBuf::from("/home/me/.config/legion"),
            state_dir: PathBuf::from("/home/me/.local/state/legion"),
        };
        let expired_line = format_invite_line(
            Path::new("/home/me/legion"),
            "invexpired",
            past,
            &key1.public_key,
            &default_home,
            None,
            ForcedCommandShell::Posix,
        )
        .unwrap();
        let other_home_line = format_invite_line(
            Path::new("/home/me/legion"),
            "invotherhome",
            future,
            &key2.public_key,
            &PinnedHome::Root(PathBuf::from("/home/me/other-legion")),
            None,
            ForcedCommandShell::Posix,
        )
        .unwrap();
        let valid_line = format_invite_line(
            Path::new("/home/me/legion"),
            "invvalid",
            future,
            &key3.public_key,
            &default_home,
            None,
            ForcedCommandShell::Posix,
        )
        .unwrap();

        let initial = format!("# keep me\n{expired_line}\n{other_home_line}\n{valid_line}\n");
        let mut auth =
            AuthKeysFile::parse_for_host(initial.as_bytes(), ForcedCommandShell::Posix, false);
        let mut state = DevicesState {
            version: 1,
            invites: BTreeMap::from([
                (
                    "invexpired".to_owned(),
                    InviteRecord {
                        id: "invexpired".to_owned(),
                        principal: "phone".to_owned(),
                        role: None,
                        create_principal: false,
                        device_name: None,
                        created_by: None,
                        created_by_device: None,
                        created_by_device_name: None,
                        created: "2026-11-24T11:44:00Z".to_owned(),
                        expires: format_rfc3339(past),
                        fingerprint: key1.fingerprint,
                        host_key_fingerprints: Vec::new(),
                    },
                ),
                (
                    "invvalid".to_owned(),
                    InviteRecord {
                        id: "invvalid".to_owned(),
                        principal: "phone".to_owned(),
                        role: None,
                        create_principal: false,
                        device_name: None,
                        created_by: None,
                        created_by_device: None,
                        created_by_device_name: None,
                        created: "2026-11-24T11:55:00Z".to_owned(),
                        expires: format_rfc3339(future),
                        fingerprint: key3.fingerprint,
                        host_key_fingerprints: Vec::new(),
                    },
                ),
                (
                    "invmissingline".to_owned(),
                    InviteRecord {
                        id: "invmissingline".to_owned(),
                        principal: "phone".to_owned(),
                        role: None,
                        create_principal: false,
                        device_name: None,
                        created_by: None,
                        created_by_device: None,
                        created_by_device_name: None,
                        created: "2026-11-24T11:55:00Z".to_owned(),
                        expires: format_rfc3339(future),
                        fingerprint: "SHA256:abc".to_owned(),
                        host_key_fingerprints: Vec::new(),
                    },
                ),
            ]),
            devices: BTreeMap::new(),
            unconfirmed: BTreeMap::new(),
        };

        let (auth_changed, state_changed) = sweep(&mut auth, &mut state, now);
        assert!(auth_changed);
        assert!(state_changed);
        assert_eq!(state.invites.len(), 1);
        assert!(state.invites.contains_key("invvalid"));
        let out = String::from_utf8(auth.to_bytes()).unwrap();
        assert!(out.starts_with("# keep me\n"));
        assert!(out.contains("legion-invite:invvalid"));
        // Another Legion home's invite line was NOT recorded here and MUST be preserved.
        assert!(out.contains("legion-invite:invotherhome"));
        assert!(!out.contains("invexpired"));

        // Second sweep is a no-op.
        assert_eq!(sweep(&mut auth, &mut state, now), (false, false));
    }
}
