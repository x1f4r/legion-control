//! Device enrolment handler for `legion gate --enrol <id>`.
//!
//! Runs as the forced command of a single-use temporary invite key in `authorized_keys`. Reads
//! exactly one JSON line from stdin with a bounded timeout and size cap, verifies the invite under
//! `state/devices.lock`, validates the permanent Ed25519 public key strictly, swaps the temporary
//! `authorized_keys` line for a permanent `legion gate --as <principal>` line atomically, records
//! the device, audits the attempt under door `ssh` (`device.enrol`), and writes one JSON reply line.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use legion_core::config::NodeFile;
use legion_core::policy::{OWNER, Policy};
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{Door, Node};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

use super::authkeys::{
    ForcedCommandShell, LineKind, PinnedHome, edit_auth_keys_with_retry, format_device_line,
    resolve_safe_write_path, resolve_target, verify_exe_not_group_or_world_writable,
    verify_owner_matches_euid,
};
use super::keys::{validate_ed25519_public_key, validate_invite_id};
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

    let (principal, audit_args, outcome) = match read_request_line(input).await {
        Err(error) => (None, json!({ "id": forced_id }), Err(error)),
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
                    json!({ "id": forced_id }),
                    Err(ErrorBody::new(
                        ErrorCode::Internal,
                        format!("Device enrolment stopped unexpectedly: {error}."),
                    )),
                ),
            }
        }
    };

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
    serde_json::Value,
    Result<EnrolReply, ErrorBody>,
) {
    let base_args = json!({ "id": forced_id });

    if let Err(error) = validate_invite_id(forced_id) {
        return (None, base_args, Err(error));
    }
    if request.enrol != ENROL_PROTOCOL {
        return (
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
        Err(error) => return (None, base_args, Err(error)),
    };
    let target = match resolve_target(paths, &node_file) {
        Ok(target) => target,
        Err(error) => return (None, base_args, Err(error)),
    };
    let write_path = match resolve_safe_write_path(&target, paths.home()) {
        Ok(path) => path,
        Err(error) => return (None, base_args, Err(error)),
    };
    if let Err(error) = verify_owner_matches_euid(
        &write_path,
        paths.home(),
        target.is_default_home_ssh,
        legion_os::user::effective_uid(),
    ) {
        return (None, base_args, Err(error));
    }

    let _guard = match lock_auth_keys(&target, &write_path) {
        Ok(guard) => guard,
        Err(error) => return (None, base_args, Err(error)),
    };

    enum EnrolAttempt {
        Succeeded {
            principal: String,
            role: String,
            device_name: String,
            fingerprint: String,
            state: super::store::DevicesState,
        },
        Failed {
            principal: Option<String>,
            error: ErrorBody,
            swept_state: Option<super::store::DevicesState>,
        },
    }

    let attempt = match edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
        let mut state = load_state(paths)?;
        let now = Timestamp::now();
        // Inspect the requested invite before sweeping so we can distinguish "expired" from "unknown/used".
        let invite_record = state.invites.get(forced_id).cloned();
        let principal_hint = invite_record.as_ref().map(|r| r.principal.clone());
        let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

        let fail_with_sweep = |principal: Option<String>,
                               error: ErrorBody,
                               auth_keys: super::authkeys::AuthKeysFile,
                               state: super::store::DevicesState| {
            Ok((
                swept_auth.then_some(auth_keys),
                EnrolAttempt::Failed {
                    principal,
                    error,
                    swept_state: swept_state.then_some(state),
                },
            ))
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
            return fail_with_sweep(principal_hint, err, auth_keys, state);
        };

        let principal = invite.principal.clone();

        // Validate the incoming public key strictly before making any changes.
        let validated_key = match validate_ed25519_public_key(&request.public_key) {
            Ok(key) => key,
            Err(error) => return fail_with_sweep(Some(principal), error, auth_keys, state),
        };

        // Refuse enrolling the invite's own temporary key (its private seed was carried in the QR and
        // transcripts) or any key already present on any line of authorized_keys (managed or foreign).
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
                auth_keys,
                state,
            );
        }
        if auth_keys.contains_key_blob(&validated_key.wire_blob) {
            return fail_with_sweep(
                Some(principal),
                ErrorBody::new(
                    ErrorCode::Conflict,
                    "That public key is already present in authorized_keys.",
                )
                .with_hint(
                    "Generate a fresh, dedicated Ed25519 key pair for this device and try again.",
                ),
                auth_keys,
                state,
            );
        }

        // Resolve the principal's current role from policy.toml.
        let policy = match node.config().get::<Policy>() {
            Ok(policy) => policy,
            Err(error) => return fail_with_sweep(Some(principal), error, auth_keys, state),
        };
        let (role, _) = policy.role_of(&principal);
        if role != OWNER
            && !policy
                .principals
                .keys()
                .any(|known| known.as_str() == principal)
        {
            return fail_with_sweep(
                Some(principal.clone()),
                ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "policy.toml no longer names {principal}, so this device cannot be enrolled."
                    ),
                )
                .with_hint("Restore the principal in policy.toml and create a new invite."),
                auth_keys,
                state,
            );
        }

        let exe = match legion_core::current_exe() {
            Ok(exe) => exe,
            Err(error) => return fail_with_sweep(Some(principal), error, auth_keys, state),
        };
        if let Err(error) = verify_exe_not_group_or_world_writable(&exe) {
            return fail_with_sweep(Some(principal), error, auth_keys, state);
        }

        let existing_names: HashSet<String> = auth_keys
            .lines
            .iter()
            .filter_map(|line| match &line.kind {
                LineKind::Device { name, .. } => Some(name.clone()),
                _ => None,
            })
            .chain(state.devices.keys().cloned())
            .collect();
        let device_name = unique_device_name(&request.name, &existing_names);

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
                auth_keys,
                state,
            );
        }

        let shell = match ForcedCommandShell::host() {
            Ok(shell) => shell,
            Err(error) => return fail_with_sweep(Some(principal), error, auth_keys, state),
        };
        let pinned_home = PinnedHome::from_paths(paths);
        let permanent_line = match format_device_line(
            &exe,
            &principal,
            &validated_key.canonical,
            &device_name,
            &pinned_home,
            pinned_account.as_deref(),
            shell,
        ) {
            Ok(line) => line,
            Err(error) => return fail_with_sweep(Some(principal), error, auth_keys, state),
        };

        // Swap temporary invite line for the permanent device line in authorized_keys atomically.
        auth_keys
            .retain(|line| !matches!(&line.kind, LineKind::Invite { id, .. } if id == forced_id));
        auth_keys.append_line(&permanent_line);

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
            },
        );

        Ok((
            Some(auth_keys),
            EnrolAttempt::Succeeded {
                principal,
                role,
                device_name,
                fingerprint: validated_key.fingerprint,
                state,
            },
        ))
    }) {
        Ok(attempt) => attempt,
        Err(error) => return (None, base_args, Err(error)),
    };

    let (principal, role, device_name, fingerprint, state) = match attempt {
        EnrolAttempt::Succeeded {
            principal,
            role,
            device_name,
            fingerprint,
            state,
        } => (principal, role, device_name, fingerprint, state),
        EnrolAttempt::Failed {
            principal,
            error,
            swept_state,
        } => {
            if let Some(state) = swept_state {
                let _ = save_state(paths, &state);
            }
            return (principal, base_args, Err(error));
        }
    };

    // authorized_keys is the source of truth and already holds the permanent device line (with the
    // temporary invite line removed), and device.list tolerates missing metadata. If persisting
    // state/devices.json fails after the swap, record the warning in the audit args and still reply
    // ok: true so the controller does not mistakenly believe enrolment failed.
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
        audit_args,
        Ok(EnrolReply::Ok {
            ok: true,
            node: node.name(),
            principal,
            role,
            device: device_name,
        }),
    )
}
