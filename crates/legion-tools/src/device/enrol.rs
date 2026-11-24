//! Device enrolment handler for `legion gate --enrol <id>`.
//!
//! Runs as the forced command of a single-use temporary invite key in `authorized_keys`. Reads
//! exactly one JSON line from stdin with a bounded timeout and size cap, verifies the invite under
//! `state/devices.lock`, validates the permanent Ed25519 public key strictly, swaps the temporary
//! `authorized_keys` line for a permanent `legion gate --as <principal>` line atomically, records
//! the device, audits the attempt under door `ssh` (`device.enrol`), and writes one JSON reply line.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use legion_core::config::{Document, NodeFile, NodeName, parse};
use legion_core::policy::{NOBODY, OWNER, Policy};
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{Door, Node};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

use super::authkeys::{
    ForcedCommandShell, LineKind, PinnedHome, edit_auth_keys_with_retry, format_device_line,
    read_auth_keys, resolve_safe_write_path, resolve_target,
    verify_exe_not_group_or_world_writable, verify_owner_matches_euid,
};
use super::keys::{generate_pairing_id, validate_ed25519_public_key, validate_invite_id};
use super::registry::{
    DeviceRegistryPushAudit, DevicesFile, FleetDevice, FleetPushOutcome, MAX_DEVICES_TOML_BYTES,
    NON_OWNER_DEVICES_TOML_LIMIT_BYTES, PairedBy, adopt_legacy_devices_from_auth_keys,
    attach_rollback_failures, devices_reserved_bytes, devices_size_limit_error,
    format_precise_rfc3339, load_devices_raw, lock_config, non_owner_devices_half_limit_error,
    push_devices_to_fleet, reconcile_authorized_keys_in_memory, restore_file_snapshot,
    save_devices_locked, upsert_device_principal_locked,
};
use super::store::{
    DeviceRecord, format_rfc3339, load_state, lock_auth_keys, parse_rfc3339, save_state, sweep,
    unique_device_name,
};

/// Maximum time `legion gate --enrol` waits for the client's one-line JSON request on stdin.
pub const ENROL_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum accepted byte length of the enrolment line on stdin.
pub const ENROL_MAX_LINE_BYTES: usize = 2048;

/// Protocol version spoken by `legion gate --enrol`.
const ENROL_PROTOCOL: u32 = 1;

/// Incoming enrolment line sent over stdin.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrolRequest {
    enrol: u32,
    id: String,
    public_key: String,
    name: String,
}

/// Reply written as one JSON line to stdout by `legion gate --enrol`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum EnrolReply {
    /// Enrolment succeeded and the permanent key is active.
    Ok {
        /// Always `true`.
        ok: bool,
        /// Name of this node.
        node: String,
        /// Principal the device is pinned to.
        principal: String,
        /// Role assigned to that principal in `policy.toml`.
        role: String,
        /// Unique sanitised name recorded for the device.
        device: String,
        /// Per-machine sync outcome when pushing the new device to peers in `fleet.toml`.
        /// Omitted when no other machines are configured.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        fleet: BTreeMap<String, FleetPushOutcome>,
        /// Fleet machines that did not receive this device yet during enrolment push.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        not_synced_to: Vec<String>,
        /// Exact command to bring unsynced machines up to date, when `not_synced_to` is non-empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sync_command: Option<String>,
    },
    /// Enrolment failed; nothing on disk was changed (beyond sweeping already-expired invites).
    Err {
        /// Always `false`.
        ok: bool,
        /// Structured error body.
        error: EnrolError,
    },
}

/// Error object returned inside `{"ok":false,"error":{...}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnrolError {
    /// Stable error code.
    pub code: ErrorCode,
    /// One-sentence explanation.
    pub message: String,
    /// Actionable guidance when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl EnrolReply {
    /// Builds a failure reply from an [`ErrorBody`].
    pub fn from_error(error: ErrorBody) -> EnrolReply {
        EnrolReply::Err {
            ok: false,
            error: EnrolError {
                code: error.code,
                message: error.message,
                hint: error.hint,
            },
        }
    }

    /// Returns `true` when enrolment succeeded.
    pub fn is_ok(&self) -> bool {
        matches!(self, EnrolReply::Ok { .. })
    }
}

/// Executes one enrolment session for `forced_id`, reading one JSON line from `input`, auditing
/// the outcome under door `ssh` (`device.enrol`), and returning the single-line [`EnrolReply`].
pub async fn run_enrol(node: &Node, forced_id: &str, input: impl AsyncRead + Unpin) -> EnrolReply {
    let started = Instant::now();
    let target_node = node.name();
    let forced_id_owned = forced_id.to_owned();

    let (principal, push_audit, audit_args, mut outcome) = match read_request_line(input).await {
        Err(error) => (None, None, json!({ "id": forced_id }), Err(error)),
        Ok(request) => {
            let node_clone = node.clone();
            let id_for_task = forced_id_owned.clone();
            match tokio::task::spawn_blocking(move || {
                process_enrol_blocking(&node_clone, &id_for_task, request)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => (
                    None,
                    None,
                    json!({ "id": forced_id }),
                    Err(ErrorBody::new(
                        ErrorCode::Internal,
                        format!("Device enrolment stopped unexpectedly: {error}."),
                    )),
                ),
            }
        }
    };

    if let Ok(EnrolReply::Ok {
        ref mut fleet,
        ref mut not_synced_to,
        ref mut sync_command,
        ..
    }) = outcome
    {
        let trigger = push_audit.unwrap_or_else(|| DeviceRegistryPushAudit {
            trigger: "device.enrol".to_owned(),
            principal: principal.clone(),
            device: None,
            pairing_id: None,
            source_node: Some(target_node.clone()),
        });
        *fleet = push_devices_to_fleet(node, Door::Ssh, trigger, None).await;
        let pending: Vec<String> = fleet
            .iter()
            .filter(|(_, outcome)| outcome.status != "synced")
            .map(|(machine, _)| machine.clone())
            .collect();
        if !pending.is_empty() {
            *not_synced_to = pending;
            *sync_command =
                Some("legion fleet.sync direction=push documents='[\"devices.toml\"]'".to_owned());
        }
    }

    let enrolled_device = outcome.as_ref().ok().and_then(|reply| match reply {
        EnrolReply::Ok { device, .. } => Some(device.clone()),
        EnrolReply::Err { .. } => None,
    });
    let audit_result = outcome
        .as_ref()
        .map(|reply| serde_json::to_value(reply).unwrap_or_else(|_| json!({ "ok": true })))
        .map_err(Clone::clone);
    node.record_audit_with_device(
        target_node,
        principal,
        Door::Ssh,
        enrolled_device,
        "device.enrol".to_owned(),
        &audit_args,
        started,
        &audit_result,
    )
    .await;

    match outcome {
        Ok(reply) => reply,
        Err(error) => EnrolReply::from_error(error),
    }
}

async fn read_request_line(input: impl AsyncRead + Unpin) -> Result<EnrolRequest, ErrorBody> {
    let mut reader = BufReader::new(input.take((ENROL_MAX_LINE_BYTES + 1) as u64));
    let mut buf = Vec::new();
    let read = tokio::time::timeout(ENROL_TIMEOUT, reader.read_until(b'\n', &mut buf))
        .await
        .map_err(|_| {
            ErrorBody::new(
                ErrorCode::Timeout,
                "Timed out waiting for the enrolment request on stdin.",
            )
            .with_hint("Send one JSON enrolment line immediately after connecting.")
        })?
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not read the enrolment request: {error}."),
            )
        })?;

    if read == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "No enrolment request was received.",
        )
        .with_hint("Send one JSON line: {\"enrol\":1,\"id\":\"...\",\"public_key\":\"ssh-ed25519 ...\",\"name\":\"...\"}."));
    }
    if buf.len() > ENROL_MAX_LINE_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Enrolment request exceeds the {ENROL_MAX_LINE_BYTES}-byte limit."),
        )
        .with_hint("Send a single compact JSON enrolment line with a short device name."));
    }

    let line_bytes = buf
        .strip_suffix(b"\n")
        .unwrap_or(&buf)
        .strip_suffix(b"\r")
        .unwrap_or_else(|| buf.strip_suffix(b"\n").unwrap_or(&buf));
    let text = std::str::from_utf8(line_bytes).map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Enrolment request is not valid UTF-8.",
        )
    })?;

    serde_json::from_str::<EnrolRequest>(text).map_err(|error| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Enrolment request is not valid JSON: {error}."),
        )
        .with_hint("Send {\"enrol\":1,\"id\":\"<id>\",\"public_key\":\"ssh-ed25519 ...\",\"name\":\"<device>\"}.")
    })
}

fn process_enrol_blocking(
    node: &Node,
    forced_id: &str,
    request: EnrolRequest,
) -> (
    Option<String>,
    Option<DeviceRegistryPushAudit>,
    serde_json::Value,
    Result<EnrolReply, ErrorBody>,
) {
    let base_args = json!({ "id": forced_id });

    if let Err(error) = validate_invite_id(forced_id) {
        return (None, None, base_args, Err(error));
    }
    if request.enrol != ENROL_PROTOCOL {
        return (
            None,
            None,
            base_args,
            Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "Unsupported enrolment version {}; this node speaks version {ENROL_PROTOCOL}.",
                    request.enrol
                ),
            )
            .with_hint("Update the client or node so both speak enrolment version 1.")),
        );
    }
    if request.id != forced_id {
        return (
            None,
            None,
            base_args,
            Err(ErrorBody::new(
                ErrorCode::Denied,
                "The invite id in the request does not match this key's invite.",
            )
            .with_hint("Use the invite id that belongs to the temporary key you connected with.")),
        );
    }

    let paths = node.paths();
    let node_file = match node.config().get::<NodeFile>() {
        Ok(file) => file,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    let target = match resolve_target(paths, &node_file) {
        Ok(target) => target,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    let write_path = match resolve_safe_write_path(&target, paths.home()) {
        Ok(path) => path,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    if let Err(error) = verify_owner_matches_euid(
        &write_path,
        paths.home(),
        target.is_default_home_ssh,
        legion_os::user::effective_uid(),
    ) {
        return (None, None, base_args, Err(error));
    }

    let _cfg_guard = match lock_config(paths) {
        Ok(guard) => guard,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    let _guard = match lock_auth_keys(&target, &write_path) {
        Ok(guard) => guard,
        Err(error) => return (None, None, base_args, Err(error)),
    };

    let node_name = node.name();
    let valid_node_name = NodeName::try_from(node_name.clone())
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "node".to_owned());

    let policy_path = paths.config_file(Policy::FILE);
    let devices_path = paths.config_file(DevicesFile::FILE);
    let read_optional_bytes = |path: &std::path::Path| -> Result<Option<Vec<u8>>, ErrorBody> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            )),
        }
    };
    let policy_before = match read_optional_bytes(&policy_path) {
        Ok(bytes) => bytes,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    let devices_before = match read_optional_bytes(&devices_path) {
        Ok(bytes) => bytes,
        Err(error) => return (None, None, base_args, Err(error)),
    };

    let mut auth_keys = match read_auth_keys(&write_path) {
        Ok(file) => file,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    let mut state = match load_state(paths) {
        Ok(state) => state,
        Err(error) => return (None, None, base_args, Err(error)),
    };
    let now = Timestamp::now();
    // Inspect the requested invite before sweeping so we can distinguish "expired" from "unknown/used".
    let invite_record = state.invites.get(forced_id).cloned();
    let principal_hint = invite_record.as_ref().map(|r| r.principal.clone());
    let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

    let fail_with_sweep = |principal: Option<String>, error: ErrorBody| {
        if swept_auth {
            let _ = edit_auth_keys_with_retry(&target, &write_path, |mut disk_auth| {
                let mut disk_state = load_state(paths)?;
                let (changed, _) = sweep(&mut disk_auth, &mut disk_state, now);
                Ok((changed.then_some(disk_auth), ()))
            });
        }
        if swept_state {
            let _ = save_state(paths, &state);
        }
        (principal, None, base_args.clone(), Err(error))
    };

    let Some(invite) = state.invites.get(forced_id).cloned() else {
        let expired = invite_record
            .as_ref()
            .and_then(|r| parse_rfc3339(&r.expires))
            .is_some_and(|exp| now >= exp);
        let err = if expired {
            ErrorBody::new(ErrorCode::Denied, "This pairing invite has expired.")
                .with_hint("Create a new invite on the node with `legion pair`.")
        } else {
            ErrorBody::new(
                ErrorCode::NotFound,
                "This pairing invite is no longer valid or has already been used.",
            )
            .with_hint("Create a new invite on the node with `legion pair`.")
        };
        return fail_with_sweep(principal_hint, err);
    };

    let principal = invite.principal.clone();

    // Validate the incoming public key strictly before making any changes.
    let validated_key = match validate_ed25519_public_key(&request.public_key) {
        Ok(key) => key,
        Err(error) => return fail_with_sweep(Some(principal), error),
    };

    // Resolve and verify the principal's role against policy.toml under the config lock.
    let policy = match policy_before.as_deref() {
        Some(bytes) => match parse::<Policy>(&policy_path, bytes) {
            Ok(policy) => policy,
            Err(error) => return fail_with_sweep(Some(principal), error),
        },
        None => Policy::default(),
    };
    let principal_exists_in_policy = policy
        .principals
        .keys()
        .any(|known| known.as_str() == principal);
    let role = if invite.create_principal {
        let Some(invite_role) = invite.role.as_deref() else {
            return fail_with_sweep(
                Some(principal),
                ErrorBody::new(
                    ErrorCode::Failed,
                    "This pairing invite is missing its recorded role and cannot be enrolled.",
                )
                .with_hint("Create a new invite on the node with `legion pair`."),
            );
        };
        let known_role =
            invite_role == OWNER || invite_role == NOBODY || policy.roles.contains_key(invite_role);
        if !known_role {
            return fail_with_sweep(
                Some(principal),
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "policy.toml no longer defines role {invite_role:?}, so this device cannot be enrolled."
                    ),
                )
                .with_hint("Restore the role in policy.toml and create a new invite."),
            );
        }
        if principal_exists_in_policy {
            return fail_with_sweep(
                Some(principal.clone()),
                ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "policy.toml already defines {principal}, so this invite cannot create a dedicated device principal for it."
                    ),
                )
                .with_hint("Create a new invite with a different device name, or remove the conflicting principal from policy.toml."),
            );
        }
        invite_role.to_owned()
    } else {
        if let Some(owner_id) = policy.device_owner(&principal) {
            return fail_with_sweep(
                Some(principal.clone()),
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "{principal} is owned by paired device {owner_id}, so another device cannot enrol under it."
                    ),
                )
                .with_hint("Pair the new device with `legion pair --name ... --role ...` or under a shared principal."),
            );
        }
        let (resolved_role, _) = policy.role_of(&principal);
        if resolved_role != OWNER && !principal_exists_in_policy {
            return fail_with_sweep(
                Some(principal.clone()),
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "policy.toml no longer names {principal}, so this device cannot be enrolled."
                    ),
                )
                .with_hint("Restore the principal in policy.toml and create a new invite."),
            );
        }
        resolved_role
    };

    let pinned_account: Option<String> = auth_keys
        .lines
        .iter()
        .find_map(|line| match &line.kind {
            LineKind::Invite { id, account, .. } if id == forced_id => account.clone(),
            _ => None,
        })
        .or_else(|| {
            if target.windows_admin_acl {
                legion_os::user::current_username().ok()
            } else {
                None
            }
        });
    if target.windows_admin_acl && pinned_account.is_none() {
        return fail_with_sweep(
            Some(principal),
            ErrorBody::new(
                ErrorCode::Failed,
                "Cannot pin OS account on shared administrators_authorized_keys because the current account name is unavailable.",
            ),
        );
    }

    // Load devices.toml, adopting any pre-existing `--pairing` lines on first run.
    let mut devices = match load_devices_raw(paths) {
        Ok(Some(file)) => file,
        Ok(None) => adopt_legacy_devices_from_auth_keys(
            paths,
            &valid_node_name,
            &auth_keys,
            &target,
            pinned_account.as_deref(),
            &state,
            &policy,
        ),
        Err(error) => return fail_with_sweep(Some(principal), error),
    };

    // Refuse enrolling the invite's own temporary key (its private seed was carried in the QR and
    // transcripts) or any key already present on any line of authorized_keys or devices.toml.
    let matches_invite_key = validated_key.fingerprint == invite.fingerprint
        || auth_keys.lines.iter().any(|line| {
            matches!(&line.kind, LineKind::Invite { id, key_b64, .. } if id == forced_id && key_b64 == &validated_key.key_b64)
        });
    if matches_invite_key {
        return fail_with_sweep(
            Some(principal),
            ErrorBody::new(
                ErrorCode::Denied,
                "The device public key cannot be the temporary key from the pairing invite.",
            )
            .with_hint(
                "Generate a fresh Ed25519 key pair on the device and send its public key instead.",
            ),
        );
    }
    let key_in_registry = devices.devices.values().any(|dev| {
        dev.public_key == validated_key.canonical
            || validate_ed25519_public_key(&dev.public_key)
                .is_ok_and(|existing| existing.wire_blob == validated_key.wire_blob)
    });
    if key_in_registry || auth_keys.contains_key_blob(&validated_key.wire_blob) {
        return fail_with_sweep(
            Some(principal),
            ErrorBody::new(
                ErrorCode::Conflict,
                "That public key is already present in authorized_keys.",
            )
            .with_hint(
                "Generate a fresh, dedicated Ed25519 key pair for this device and try again.",
            ),
        );
    }
    if invite.create_principal
        && (devices.devices.values().any(|dev| {
            dev.principal == principal
                || invite
                    .device_name
                    .as_deref()
                    .is_some_and(|slug| dev.name == slug)
        }) || devices
            .removed
            .values()
            .any(|tomb| tomb.principal == principal))
    {
        return fail_with_sweep(
            Some(principal.clone()),
            ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "A paired device for {principal} already exists or was previously removed in devices.toml."
                ),
            )
            .with_hint("Create a new invite so a fresh `device:*` principal is allocated."),
        );
    }

    let exe = match legion_core::current_exe() {
        Ok(exe) => exe,
        Err(error) => return fail_with_sweep(Some(principal), error),
    };
    if let Err(error) = verify_exe_not_group_or_world_writable(&exe) {
        return fail_with_sweep(Some(principal), error);
    }

    let existing_names: HashSet<String> = devices
        .devices
        .values()
        .map(|d| d.name.clone())
        .chain(auth_keys.lines.iter().filter_map(|line| match &line.kind {
            LineKind::Device { name, .. } => Some(name.clone()),
            _ => None,
        }))
        .chain(state.devices.keys().cloned())
        .collect();
    let requested_name = invite
        .device_name
        .as_deref()
        .unwrap_or(request.name.as_str());
    let device_name = unique_device_name(requested_name, &existing_names);

    let shell = match ForcedCommandShell::host() {
        Ok(shell) => shell,
        Err(error) => return fail_with_sweep(Some(principal), error),
    };
    let pairing_id = match generate_pairing_id() {
        Ok(id) => id,
        Err(error) => return fail_with_sweep(Some(principal), error),
    };
    let pinned_home = PinnedHome::from_paths(paths);
    if let Err(error) = format_device_line(
        &exe,
        &principal,
        &validated_key.canonical,
        &device_name,
        &pairing_id,
        &pinned_home,
        pinned_account.as_deref(),
        shell,
    ) {
        return fail_with_sweep(Some(principal), error);
    }

    let precise_now = format_precise_rfc3339(now);
    let recorded_role = if invite.create_principal {
        Some(role.clone())
    } else {
        None
    };
    let paired_by = invite.created_by.as_ref().map(|creator| PairedBy {
        principal: creator.clone(),
        device: invite.created_by_device.clone(),
    });
    devices.devices.insert(
        pairing_id.clone(),
        FleetDevice {
            name: device_name.clone(),
            public_key: validated_key.canonical.clone(),
            principal: principal.clone(),
            role: recorded_role,
            paired_at: precise_now.clone(),
            paired_on: valid_node_name.clone(),
            paired_by,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: precise_now.clone(),
            updated_on: valid_node_name.clone(),
            name_updated_at: precise_now.clone(),
            name_updated_on: valid_node_name.clone(),
            role_updated_at: precise_now,
            role_updated_on: valid_node_name.clone(),
            conflict: false,
            role_conflict: false,
        },
    );

    let reserved_bytes = devices_reserved_bytes(&devices);
    if reserved_bytes > MAX_DEVICES_TOML_BYTES {
        return fail_with_sweep(Some(principal), devices_size_limit_error(reserved_bytes));
    }
    let creator_is_owner = match invite.created_by.as_deref() {
        Some(creator) => policy.role_of(creator).0 == OWNER,
        None => true,
    };
    if !creator_is_owner && reserved_bytes > NON_OWNER_DEVICES_TOML_LIMIT_BYTES {
        return fail_with_sweep(
            Some(principal),
            non_owner_devices_half_limit_error("enrol a new device", reserved_bytes),
        );
    }

    let added = Timestamp::from_second(now.as_second())
        .map(format_rfc3339)
        .unwrap_or_else(|_| format_rfc3339(now));
    state.invites.remove(forced_id);
    state.devices.insert(
        device_name.clone(),
        DeviceRecord {
            name: device_name.clone(),
            principal: principal.clone(),
            added,
            fingerprint: validated_key.fingerprint.clone(),
            pairing_id: Some(pairing_id.clone()),
        },
    );

    // 1. Save the new principal in `policy.toml` (when creating a dedicated `device:*` principal)
    //    and `devices.toml` FIRST, before touching `authorized_keys`. If anything fails here, roll
    //    back so nothing on disk has changed (and report any rollback failure loudly).
    if invite.create_principal
        && let Err(error) = upsert_device_principal_locked(paths, &principal, &role, &pairing_id)
    {
        return (Some(principal), None, base_args, Err(error));
    }
    if let Err(error) = save_devices_locked(paths, &devices) {
        let mut rollback_failures = Vec::new();
        if invite.create_principal
            && let Err(msg) = restore_file_snapshot(
                paths,
                "device.enrol",
                &policy_path,
                policy_before.as_deref(),
            )
        {
            rollback_failures.push(msg);
        }
        return (
            Some(principal),
            None,
            base_args,
            Err(attach_rollback_failures(error, &rollback_failures)),
        );
    }

    let rollback_both = |error: ErrorBody| -> ErrorBody {
        let mut rollback_failures = Vec::new();
        if let Err(msg) = restore_file_snapshot(
            paths,
            "device.enrol",
            &devices_path,
            devices_before.as_deref(),
        ) {
            rollback_failures.push(msg);
        }
        if invite.create_principal
            && let Err(msg) = restore_file_snapshot(
                paths,
                "device.enrol",
                &policy_path,
                policy_before.as_deref(),
            )
        {
            rollback_failures.push(msg);
        }
        attach_rollback_failures(error, &rollback_failures)
    };

    let live_policy = match std::fs::read(&policy_path) {
        Ok(bytes) => match parse::<Policy>(&policy_path, &bytes) {
            Ok(policy) => policy,
            Err(error) => {
                return (Some(principal), None, base_args, Err(rollback_both(error)));
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Policy::default(),
        Err(error) => {
            return (
                Some(principal),
                None,
                base_args,
                Err(rollback_both(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be read: {error}.", policy_path.display()),
                ))),
            );
        }
    };

    // 2. Swap the temporary invite line for the permanent device line in `authorized_keys` LAST.
    //    If the swap fails, roll back BOTH `devices.toml` and `policy.toml`.
    let swap_result = edit_auth_keys_with_retry(&target, &write_path, |mut disk_auth| {
        let mut disk_state = load_state(paths)?;
        let _ = sweep(&mut disk_auth, &mut disk_state, now);
        disk_auth
            .retain(|line| !matches!(&line.kind, LineKind::Invite { id, .. } if id == forced_id));
        let report = reconcile_authorized_keys_in_memory(
            &mut disk_auth,
            &devices,
            &live_policy,
            paths,
            &target,
            pinned_account.as_deref(),
            &exe,
            shell,
        )?;
        if report.foreign_key_ids.contains(&pairing_id)
            || report.duplicate_key_ids.contains_key(&pairing_id)
        {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "That public key is already present in authorized_keys.",
            )
            .with_hint(
                "Generate a fresh, dedicated Ed25519 key pair for this device and try again.",
            ));
        }
        if let Some(reason) = report.principal_conflict_ids.get(&pairing_id) {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!("Device {device_name} ({pairing_id}) cannot be activated: {reason}."),
            ));
        }
        Ok((Some(disk_auth), ()))
    });
    if let Err(error) = swap_result {
        return (Some(principal), None, base_args, Err(rollback_both(error)));
    }

    let push_audit = Some(DeviceRegistryPushAudit {
        trigger: "device.enrol".to_owned(),
        principal: invite.created_by,
        device: invite.created_by_device_name,
        pairing_id: invite.created_by_device,
        source_node: Some(node.name()),
    });
    let fingerprint = validated_key.fingerprint;

    // authorized_keys, devices.toml, and policy.toml already hold the permanent device (with the
    // temporary invite line removed), and device.list tolerates missing state/devices.json
    // metadata. If persisting state/devices.json fails after the swap, record the warning in the
    // audit args and still reply ok: true so the controller does not mistakenly believe enrolment
    // failed.
    let state_warning = save_state(paths, &state).err().map(|error| error.message);

    let mut audit_args = json!({
        "id": forced_id,
        "device": device_name,
        "fingerprint": fingerprint,
    });
    if let Some(warning) = state_warning
        && let Some(obj) = audit_args.as_object_mut()
    {
        obj.insert("state_warning".to_owned(), json!(warning));
    }
    (
        Some(principal.clone()),
        push_audit,
        audit_args,
        Ok(EnrolReply::Ok {
            ok: true,
            node: node.name(),
            principal,
            role,
            device: device_name,
            fleet: BTreeMap::new(),
            not_synced_to: Vec::new(),
            sync_command: None,
        }),
    )
}
