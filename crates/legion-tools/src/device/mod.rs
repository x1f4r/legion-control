//! `device`: pair remote controllers (phones, tablets, other clients) over SSH, confined by
//! `legion gate --as <principal>`.
//!
//! `device.invite` creates a single-use temporary Ed25519 key forced to `legion gate --enrol <id>`
//! with sshd's own `expiry-time`, returning an invite string (`legion-invite-1.<base64url>`) that
//! carries the node's hosts, account, pinned sshd host keys, and the temporary key. The temporary
//! key is never stored on disk or logged. `device.list` reads `authorized_keys` as the source of
//! truth joined with non-secret state metadata, and `device.remove` revokes a paired device or
//! pending invite at once.

pub mod authkeys;
pub mod enrol;
pub mod hosts;
pub mod keys;
pub mod store;

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use legion_core::config::NodeFile;
use legion_core::policy::Principal;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, NoArgs, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::caller::Caller;

use self::authkeys::{
    LineKind, format_invite_line, read_auth_keys, resolve_safe_write_path, resolve_target,
    write_auth_keys,
};
use self::hosts::{HostArg, resolve_hosts};
use self::keys::{
    InvitePayload, fingerprint_key_b64, generate_ephemeral_ed25519, generate_invite_id,
    read_host_keys, validate_invite_id,
};
use self::store::{InviteRecord, format_rfc3339, load_state, lock_devices, save_state, sweep};

/// Default invite validity in minutes.
pub const DEFAULT_VALID_MINUTES: u32 = 15;

/// Minimum invite validity in minutes.
pub const MIN_VALID_MINUTES: u32 = 1;

/// Maximum invite validity in minutes (24 hours).
pub const MAX_VALID_MINUTES: u32 = 1440;

/// Default SSH port for pairing invites.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// Adds the `device` tools (`device.invite`, `device.list`, `device.remove`).
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "device.invite",
        Effect::Admin,
        "Create a single-use pairing invite for a remote device to connect over SSH, pinned to one principal that policy.toml names and the caller could act as itself. Appends a temporary key with sshd's expiry-time and a forced enrolment command to authorized_keys, and returns the invite text once (never logged or stored). Inviting for a principal with the owner role requires an owner caller and allow_owner=true.",
        invite,
    );
    registry.add_fn(
        "device.list",
        Effect::Admin,
        "List paired devices and pending pairing invites for principals the caller could act as, reading authorized_keys as the source of truth joined with recorded metadata, and sweeping any expired or used invites.",
        list,
    );
    registry.add_fn(
        "device.remove",
        Effect::Admin,
        "Remove a paired device by name or revoke a pending pairing invite by id for a principal the caller could act as, deleting its authorized_keys line(s) and recorded metadata.",
        remove,
    );
}

/// Arguments of `device.invite`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InviteArgs {
    /// The principal the paired device will act as. Must be named in policy.toml (or have the
    /// owner role with allow_owner=true) and be one the caller could act as.
    principal: String,
    /// Minutes until the invite expires, from 1 to 1440 (default 15).
    #[serde(default = "default_valid_minutes")]
    #[schemars(range(min = 1, max = 1440))]
    valid_minutes: u32,
    /// Host name or IP address (a single string or a list of strings) to put in the invite,
    /// overriding automatic Tailscale/hostname detection.
    #[serde(default)]
    #[schemars(with = "Option<Vec<String>>")]
    host: Option<HostArg>,
    /// SSH port on this machine (default 22).
    #[serde(default = "default_ssh_port")]
    #[schemars(range(min = 1, max = 65535))]
    port: u16,
    /// OS account name on this machine (defaults to the account legion runs as).
    #[serde(default)]
    user: Option<String>,
    /// Allow inviting a device as a principal with the owner role, which may do everything the
    /// account can. Off unless given as true.
    #[serde(default)]
    allow_owner: bool,
}

fn default_valid_minutes() -> u32 {
    DEFAULT_VALID_MINUTES
}

fn default_ssh_port() -> u16 {
    DEFAULT_SSH_PORT
}

/// What `device.invite` returns.
#[derive(Debug, Serialize)]
pub struct InviteResult {
    /// The one-line invite text (`legion-invite-1.<base64url>`). Shown once; never logged.
    pub invite: String,
    /// Single-use invite identifier.
    pub id: String,
    /// Node name that issued this invite.
    pub node: String,
    /// Principal the paired device will act as.
    pub principal: String,
    /// Role assigned to `principal` in `policy.toml`.
    pub role: String,
    /// RFC 3339 UTC timestamp when this invite expires.
    pub expires: String,
    /// Candidate host names / Tailscale addresses in the invite.
    pub hosts: Vec<String>,
    /// SSH port in the invite.
    pub port: u16,
    /// OS account name in the invite.
    pub user: String,
    /// Pinned SSH public host keys in the invite.
    pub host_keys: Vec<String>,
}

async fn invite(ctx: &Ctx, args: InviteArgs) -> Result<InviteResult, ToolError> {
    let principal = Principal::parse(&args.principal)?;
    let principal = principal.as_str().to_owned();
    let caller = Caller::of(ctx)?;
    let role = caller.check_issue(
        &principal,
        args.allow_owner,
        "a device invite",
        format!(
            "{principal} has the owner role: a paired device acting as it may do everything the account can over SSH, so it is not made unless asked for in so many words."
        ),
        "Give the device a principal with a narrower role in policy.toml, or pass allow_owner=true if this is really what you want.",
    )?;

    // An expiring credential (such as an expiring HTTP token) must not mint a permanent SSH key.
    if let Some(limit) = ctx.credential_expires() {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "The credential this call came with expires at {}, so it may not issue a device pairing invite that grants a permanent SSH key.",
                format_rfc3339(limit)
            ),
        )
        .with_hint("Create the device invite from a local shell or a non-expiring owner credential."));
    }

    if !(MIN_VALID_MINUTES..=MAX_VALID_MINUTES).contains(&args.valid_minutes) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("valid_minutes must be between {MIN_VALID_MINUTES} and {MAX_VALID_MINUTES}."),
        )
        .with_hint("Use a duration between 1 minute and 1440 minutes (24 hours)."));
    }
    if args.port == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "port must be between 1 and 65535.",
        ));
    }

    let user = match args.user {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty()
                || trimmed.len() > 128
                || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{raw:?} is not a valid SSH user name."),
                )
                .with_hint("Pass the OS account name with no spaces or control characters."));
            }
            trimmed.to_owned()
        }
        None => legion_os::user::current_username().map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot determine the account legion runs as: {error}."),
            )
            .with_hint("Pass `user` explicitly to device.invite.")
        })?,
    };

    let hosts = resolve_hosts(args.host, ctx.cancel_token().child_token()).await?;
    let node_file = ctx.config().get::<NodeFile>()?;
    let host_keys = read_host_keys(&node_file.devices.host_keys, ctx.paths().home())?;
    let exe = legion_core::current_exe()?;

    let now = Timestamp::from_second(Timestamp::now().as_second()).map_err(internal_time)?;
    let expires_ts = now
        .checked_add(SignedDuration::from_mins(i64::from(args.valid_minutes)))
        .map_err(internal_time)?;
    let created = format_rfc3339(now);
    let expires = format_rfc3339(expires_ts);

    let id = generate_invite_id()?;
    let ephemeral = generate_ephemeral_ed25519()?;
    let invite_line =
        format_invite_line(&exe, &id, expires_ts, &ephemeral.public_key, cfg!(windows))?;

    let payload = InvitePayload {
        v: keys::INVITE_VERSION,
        id: id.clone(),
        node: ctx.node_name().to_owned(),
        hosts: hosts.clone(),
        port: args.port,
        user: user.clone(),
        host_keys: host_keys.clone(),
        key: ephemeral.seed_b64,
        principal: principal.clone(),
        expires: expires.clone(),
    };
    let invite_text = payload.encode()?;

    let record = InviteRecord {
        id: id.clone(),
        principal: principal.clone(),
        created,
        expires: expires.clone(),
        fingerprint: ephemeral.fingerprint,
    };

    let paths = ctx.paths().clone();
    let id_for_write = id.clone();
    blocking(move || {
        let target = resolve_target(&paths, &node_file)?;
        let write_path = resolve_safe_write_path(&target, paths.home())?;
        let _guard = lock_devices(&paths)?;

        let mut auth_keys = read_auth_keys(&write_path)?;
        let original_bytes = auth_keys.to_bytes();
        let existed_before = write_path.exists();
        let mut state = load_state(&paths)?;

        sweep(&mut auth_keys, &mut state, now);
        auth_keys.append_line(&invite_line);
        write_auth_keys(&target, &write_path, &auth_keys.to_bytes())?;

        state.invites.insert(id_for_write, record);
        if let Err(state_err) = save_state(&paths, &state) {
            // Roll back authorized_keys so a failed state write never leaves an orphaned invite key.
            if existed_before {
                let _ = write_auth_keys(&target, &write_path, &original_bytes);
            } else {
                let _ = std::fs::remove_file(&write_path);
            }
            return Err(state_err);
        }
        Ok(())
    })
    .await?;

    Ok(InviteResult {
        invite: invite_text,
        id,
        node: ctx.node_name().to_owned(),
        principal,
        role,
        expires,
        hosts,
        port: args.port,
        user,
        host_keys,
    })
}

/// What `device.list` returns.
#[derive(Debug, Serialize)]
struct ListResult {
    devices: Vec<ListedDevice>,
    invites: Vec<ListedInvite>,
}

/// One paired device listed by `device.list`.
#[derive(Debug, Serialize)]
struct ListedDevice {
    name: String,
    principal: String,
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<String>,
}

/// One pending pairing invite listed by `device.list`.
#[derive(Debug, Serialize)]
struct ListedInvite {
    id: String,
    principal: String,
    fingerprint: String,
    expires: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created: Option<String>,
}

async fn list(ctx: &Ctx, _args: NoArgs) -> Result<ListResult, ToolError> {
    let caller = Caller::of(ctx)?;
    let node_file = ctx.config().get::<NodeFile>()?;
    let paths = ctx.paths().clone();

    let (raw_devices, raw_invites) = blocking(move || {
        let target = resolve_target(&paths, &node_file)?;
        let write_path = resolve_safe_write_path(&target, paths.home())?;
        let _guard = lock_devices(&paths)?;

        let mut auth_keys = read_auth_keys(&write_path)?;
        let mut state = load_state(&paths)?;
        let now = Timestamp::now();
        let (auth_changed, state_changed) = sweep(&mut auth_keys, &mut state, now);
        if auth_changed {
            write_auth_keys(&target, &write_path, &auth_keys.to_bytes())?;
        }
        if state_changed {
            save_state(&paths, &state)?;
        }

        let mut devices = Vec::new();
        let mut invites = Vec::new();
        for line in &auth_keys.lines {
            match &line.kind {
                LineKind::Device {
                    name,
                    key_b64,
                    principal,
                } => {
                    let meta = state.devices.get(name);
                    let resolved_principal = meta
                        .map(|m| m.principal.clone())
                        .or_else(|| principal.clone());
                    let fingerprint = fingerprint_key_b64(key_b64)
                        .or_else(|| meta.map(|m| m.fingerprint.clone()))
                        .unwrap_or_else(|| "unknown".to_owned());
                    devices.push((
                        name.clone(),
                        resolved_principal,
                        fingerprint,
                        meta.map(|m| m.added.clone()),
                    ));
                }
                LineKind::Invite {
                    id,
                    key_b64,
                    expiry_time,
                } => {
                    let Some(meta) = state.invites.get(id) else {
                        continue;
                    };
                    let fingerprint =
                        fingerprint_key_b64(key_b64).unwrap_or_else(|| meta.fingerprint.clone());
                    let expires = if !meta.expires.is_empty() {
                        meta.expires.clone()
                    } else if let Some(exp) = expiry_time {
                        format_rfc3339(*exp)
                    } else {
                        String::new()
                    };
                    invites.push((
                        id.clone(),
                        meta.principal.clone(),
                        fingerprint,
                        expires,
                        Some(meta.created.clone()),
                    ));
                }
                LineKind::Foreign => {}
            }
        }
        Ok((devices, invites))
    })
    .await?;

    let devices = raw_devices
        .into_iter()
        .filter_map(|(name, principal, fingerprint, added)| {
            let principal = match principal {
                Some(p) if caller.may_manage(&p) => p,
                None if caller.is_owner() => "unknown".to_owned(),
                _ => return None,
            };
            Some(ListedDevice {
                name,
                principal,
                fingerprint,
                added,
            })
        })
        .collect();

    let invites = raw_invites
        .into_iter()
        .filter(|(_, principal, _, _, _)| caller.may_manage(principal))
        .map(
            |(id, principal, fingerprint, expires, created)| ListedInvite {
                id,
                principal,
                fingerprint,
                expires,
                created,
            },
        )
        .collect();

    Ok(ListResult { devices, invites })
}

/// Arguments of `device.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// Paired device name to remove, as `device.list` shows it. Give `name` or `id`, not both.
    #[serde(default)]
    name: Option<String>,
    /// Pending invite id to revoke, as `device.list` shows it. Give `name` or `id`, not both.
    #[serde(default)]
    id: Option<String>,
}

/// What `device.remove` returns.
#[derive(Debug, Serialize)]
struct Removed {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    principal: String,
    removed: bool,
    lines_removed: usize,
}

async fn remove(ctx: &Ctx, args: RemoveArgs) -> Result<Removed, ToolError> {
    let target_item = match (args.name, args.id) {
        (Some(name), None) => {
            if name.trim().is_empty() || name.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{name:?} is not a valid device name."),
                )
                .with_hint("Use a device name from `device.list`."));
            }
            RemoveTarget::Device(name)
        }
        (None, Some(id)) => {
            validate_invite_id(&id)?;
            RemoveTarget::Invite(id)
        }
        (Some(_), Some(_)) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Give either `name` (a paired device) or `id` (a pending invite), not both.",
            ));
        }
        (None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Give `name` (a paired device) or `id` (a pending invite) to remove.",
            )
            .with_hint("Run `device.list` to see paired devices and pending invites."));
        }
    };

    let caller = Caller::of(ctx)?;
    let node_file = ctx.config().get::<NodeFile>()?;
    let paths = ctx.paths().clone();

    blocking(move || {
        let target = resolve_target(&paths, &node_file)?;
        let write_path = resolve_safe_write_path(&target, paths.home())?;
        let _guard = lock_devices(&paths)?;

        let mut auth_keys = read_auth_keys(&write_path)?;
        let mut state = load_state(&paths)?;
        let now = Timestamp::now();
        let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

        match target_item {
            RemoveTarget::Device(name) => {
                let mut visible_names: BTreeMap<String, String> = BTreeMap::new();
                let mut matched_principal: Option<String> = None;
                let mut matched_lines = 0usize;

                for line in &auth_keys.lines {
                    if let LineKind::Device {
                        name: line_name,
                        principal,
                        ..
                    } = &line.kind
                    {
                        let resolved = state
                            .devices
                            .get(line_name)
                            .map(|m| m.principal.clone())
                            .or_else(|| principal.clone());
                        let can_manage = match resolved.as_deref() {
                            Some(p) => caller.may_manage(p),
                            None => caller.is_owner(),
                        };
                        if can_manage {
                            let p = resolved.unwrap_or_else(|| "unknown".to_owned());
                            visible_names.entry(line_name.clone()).or_insert_with(|| p.clone());
                            if line_name == &name {
                                matched_principal.get_or_insert(p);
                                matched_lines += 1;
                            }
                        }
                    }
                }

                let Some(principal) = matched_principal else {
                    if swept_auth {
                        let _ = write_auth_keys(&target, &write_path, &auth_keys.to_bytes());
                    }
                    if swept_state {
                        let _ = save_state(&paths, &state);
                    }
                    let hint = if visible_names.is_empty() {
                        "There are no paired devices.".to_owned()
                    } else {
                        let known: Vec<&str> = visible_names.keys().map(String::as_str).collect();
                        format!("The paired devices are {}.", known.join(", "))
                    };
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("There is no paired device called {name}."),
                    )
                    .with_hint(hint));
                };

                auth_keys.retain(|line| {
                    !matches!(&line.kind, LineKind::Device { name: line_name, .. } if line_name == &name)
                });
                write_auth_keys(&target, &write_path, &auth_keys.to_bytes())?;
                state.devices.remove(&name);
                save_state(&paths, &state)?;

                Ok(Removed {
                    kind: "device",
                    name: Some(name),
                    id: None,
                    principal,
                    removed: true,
                    lines_removed: matched_lines,
                })
            }
            RemoveTarget::Invite(id) => {
                let mut visible_ids: Vec<String> = Vec::new();
                let mut matched_principal: Option<String> = None;
                let mut matched_lines = 0usize;

                for line in &auth_keys.lines {
                    if let LineKind::Invite { id: line_id, .. } = &line.kind
                        && let Some(meta) = state.invites.get(line_id)
                        && caller.may_manage(&meta.principal)
                    {
                        if !visible_ids.contains(line_id) {
                            visible_ids.push(line_id.clone());
                        }
                        if line_id == &id {
                            matched_principal.get_or_insert_with(|| meta.principal.clone());
                            matched_lines += 1;
                        }
                    }
                }

                let Some(principal) = matched_principal else {
                    if swept_auth {
                        let _ = write_auth_keys(&target, &write_path, &auth_keys.to_bytes());
                    }
                    if swept_state {
                        let _ = save_state(&paths, &state);
                    }
                    let hint = if visible_ids.is_empty() {
                        "There are no pending invites.".to_owned()
                    } else {
                        format!("The pending invites are {}.", visible_ids.join(", "))
                    };
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("There is no pending invite with id {id}."),
                    )
                    .with_hint(hint));
                };

                auth_keys.retain(|line| {
                    !matches!(&line.kind, LineKind::Invite { id: line_id, .. } if line_id == &id)
                });
                write_auth_keys(&target, &write_path, &auth_keys.to_bytes())?;
                state.invites.remove(&id);
                save_state(&paths, &state)?;

                Ok(Removed {
                    kind: "invite",
                    name: None,
                    id: Some(id),
                    principal,
                    removed: true,
                    lines_removed: matched_lines,
                })
            }
        }
    })
    .await
}

enum RemoveTarget {
    Device(String),
    Invite(String),
}

fn internal_time(error: impl std::fmt::Display) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Internal,
        format!("The invite's times cannot be worked out: {error}."),
    )
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("A device tool stopped unexpectedly: {error}."),
        )
    })?
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use legion_core::{Call, Door, Node, Paths};
    use serde_json::{Value, json};

    use super::*;
    use crate::device::enrol::{EnrolReply, run_enrol};
    use crate::device::keys::generate_ephemeral_ed25519;

    struct Harness {
        dir: tempfile::TempDir,
        node: Node,
        auth_keys: std::path::PathBuf,
        host_key: String,
    }

    impl Harness {
        fn new() -> Harness {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            let auth_keys = dir.path().join("authorized_keys");
            let host_key_path = dir.path().join("ssh_host_ed25519_key.pub");
            let host_key = generate_ephemeral_ed25519().unwrap().public_key;
            std::fs::write(&host_key_path, format!("{host_key} root@tower\n")).unwrap();

            let paths = Paths::under(dir.path().join("legion")).with_home(&home);
            std::fs::create_dir_all(paths.config_dir()).unwrap();
            let auth_escaped = auth_keys.display().to_string().replace('\\', "\\\\");
            let hk_escaped = host_key_path.display().to_string().replace('\\', "\\\\");
            std::fs::write(
                paths.config_file("node.toml"),
                format!(
                    "name = \"tower\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\nhost_keys = [\"{hk_escaped}\"]\n"
                ),
            )
            .unwrap();
            std::fs::write(
                paths.config_file("policy.toml"),
                r#"
default = "owner"

[roles.observer]
allow = ["effect:read"]

[roles.phone_admin]
allow = ["effect:read", "effect:admin"]

[principals.phone]
role = "observer"

[principals.tablet]
role = "phone_admin"

[principals.co_owner]
role = "owner"
"#,
            )
            .unwrap();

            let node = Node::new(paths, crate::registry());
            Harness {
                dir,
                node,
                auth_keys,
                host_key,
            }
        }

        async fn call(&self, tool: &str, args: Value) -> Result<Value, ErrorBody> {
            self.node.call(Call::new(Door::Cli, tool, args)).await
        }

        async fn call_as(
            &self,
            tool: &str,
            principal: &str,
            args: Value,
        ) -> Result<Value, ErrorBody> {
            let mut call = Call::new(Door::Cli, tool, args);
            call.pinned = Some(principal.to_owned());
            self.node.call(call).await
        }
    }

    #[tokio::test]
    async fn full_invite_enrol_duplicate_name_list_and_remove_flow() {
        let h = Harness::new();
        let foreign = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000 me@laptop";
        std::fs::write(&h.auth_keys, foreign).unwrap();

        let inv = h
            .call(
                "device.invite",
                json!({
                    "principal": "phone",
                    "host": "tower.example",
                    "port": 2222,
                    "user": "me",
                    "valid_minutes": 10
                }),
            )
            .await
            .unwrap();
        let invite_text = inv["invite"].as_str().unwrap();
        let id = inv["id"].as_str().unwrap();
        let decoded = InvitePayload::decode(invite_text).unwrap();
        assert_eq!(decoded.v, 1);
        assert_eq!(decoded.id, id);
        assert_eq!(decoded.node, "tower");
        assert_eq!(decoded.hosts, vec!["tower.example"]);
        assert_eq!(decoded.port, 2222);
        assert_eq!(decoded.user, "me");
        assert_eq!(decoded.principal, "phone");
        assert_eq!(decoded.host_keys, vec![h.host_key.clone()]);

        // Invite text and key never appear in audit.jsonl or devices.json.
        let audit = std::fs::read_to_string(h.node.paths().audit_log()).unwrap();
        assert!(!audit.contains(invite_text));
        assert!(!audit.contains(&decoded.key));
        let state_raw =
            std::fs::read_to_string(h.node.paths().state_dir().join("devices.json")).unwrap();
        assert!(!state_raw.contains(invite_text));
        assert!(!state_raw.contains(&decoded.key));

        // List shows the pending invite and no devices yet.
        let listed = h.call("device.list", json!({})).await.unwrap();
        assert_eq!(listed["devices"].as_array().unwrap().len(), 0);
        assert_eq!(listed["invites"].as_array().unwrap().len(), 1);
        assert_eq!(listed["invites"][0]["id"], json!(id));

        // Wrong invite id is rejected and changes nothing.
        let device_key1 = generate_ephemeral_ed25519().unwrap();
        let wrong_req = format!(
            "{{\"enrol\":1,\"id\":\"wrongid\",\"public_key\":\"{}\",\"name\":\"Pixel 10\"}}\n",
            device_key1.public_key
        );
        let wrong_reply = run_enrol(&h.node, id, wrong_req.as_bytes()).await;
        assert!(!wrong_reply.is_ok());

        // Hostile public key is rejected and leaves the invite usable.
        let bad_key_req = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"restrict {}\",\"name\":\"Pixel 10\"}}\n",
            device_key1.public_key
        );
        let bad_key_reply = run_enrol(&h.node, id, bad_key_req.as_bytes()).await;
        assert!(!bad_key_reply.is_ok());

        // Valid enrolment succeeds.
        let req1 = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"{} pixel@phone\",\"name\":\"My Pixel!\"}}\n",
            device_key1.public_key
        );
        let reply1 = run_enrol(&h.node, id, req1.as_bytes()).await;
        assert_eq!(
            reply1,
            EnrolReply::Ok {
                ok: true,
                node: "tower".to_owned(),
                principal: "phone".to_owned(),
                role: "observer".to_owned(),
                device: "my-pixel".to_owned(),
            }
        );

        // Second use of the same invite must fail!
        let replay = run_enrol(&h.node, id, req1.as_bytes()).await;
        assert!(!replay.is_ok());

        // Second device with the same requested name gets a unique slug (`my-pixel-2`).
        let inv2 = h
            .call(
                "device.invite",
                json!({"principal": "phone", "host": ["tower.example", "100.64.0.1"], "user": "me"}),
            )
            .await
            .unwrap();
        let id2 = inv2["id"].as_str().unwrap();
        let device_key2 = generate_ephemeral_ed25519().unwrap();
        let req2 = format!(
            "{{\"enrol\":1,\"id\":\"{id2}\",\"public_key\":\"{}\",\"name\":\"My Pixel!\"}}\n",
            device_key2.public_key
        );
        let reply2 = run_enrol(&h.node, id2, req2.as_bytes()).await;
        assert_eq!(
            reply2,
            EnrolReply::Ok {
                ok: true,
                node: "tower".to_owned(),
                principal: "phone".to_owned(),
                role: "observer".to_owned(),
                device: "my-pixel-2".to_owned(),
            }
        );

        // Audit log records door="ssh" and tool="device.enrol" with no public key bytes.
        let audit_after = std::fs::read_to_string(h.node.paths().audit_log()).unwrap();
        assert!(audit_after.contains("\"door\":\"ssh\""));
        assert!(audit_after.contains("\"tool\":\"device.enrol\""));
        assert!(!audit_after.contains(&device_key1.public_key));

        // Foreign line is still intact in authorized_keys.
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(auth_text.starts_with(&format!("{foreign}\n")));
        assert!(auth_text.contains("legion-device:my-pixel\n"));
        assert!(auth_text.contains("legion-device:my-pixel-2\n"));
        assert!(!auth_text.contains("legion-invite:"));

        // device.list shows both devices with fingerprints matching SHA256.
        let listed = h.call("device.list", json!({})).await.unwrap();
        let devices = listed["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0]["name"], json!("my-pixel"));
        assert_eq!(devices[0]["fingerprint"], json!(device_key1.fingerprint));
        assert_eq!(devices[1]["name"], json!("my-pixel-2"));

        // device.remove removes the named device and keeps the foreign line + second device.
        let removed = h
            .call("device.remove", json!({"name": "my-pixel"}))
            .await
            .unwrap();
        assert_eq!(removed["kind"], json!("device"));
        assert_eq!(removed["removed"], json!(true));
        let auth_after = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(auth_after.contains(foreign));
        assert!(!auth_after.contains("legion-device:my-pixel\n"));
        assert!(auth_after.contains("legion-device:my-pixel-2\n"));
    }

    #[tokio::test]
    async fn principal_scoping_and_owner_guard_match_token_rules() {
        let h = Harness::new();

        // Unknown principal is refused with NotFound.
        let err = h
            .call(
                "device.invite",
                json!({"principal": "ghost", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);

        // Owner-role principal requires allow_owner=true.
        for owner_principal in ["owner", "co_owner"] {
            let err = h
                .call(
                    "device.invite",
                    json!({"principal": owner_principal, "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied);
        }
        // With allow_owner=true, owner caller succeeds.
        let owner_inv = h
            .call(
                "device.invite",
                json!({"principal": "owner", "allow_owner": true, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let owner_inv_id = owner_inv["id"].as_str().unwrap().to_owned();

        // Non-owner admin (`tablet`) may invite for itself, but NOT for `phone` or `owner`.
        let self_inv = h
            .call_as(
                "device.invite",
                "tablet",
                json!({"principal": "tablet", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let tablet_inv_id = self_inv["id"].as_str().unwrap().to_owned();

        let other_err = h
            .call_as(
                "device.invite",
                "tablet",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(other_err.code, ErrorCode::Denied);

        let owner_err = h
            .call_as(
                "device.invite",
                "tablet",
                json!({"principal": "owner", "allow_owner": true, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(owner_err.code, ErrorCode::Denied);

        // `tablet` only sees its own invite in device.list and cannot remove `owner`'s invite.
        let tablet_list = h.call_as("device.list", "tablet", json!({})).await.unwrap();
        let ids: Vec<&str> = tablet_list["invites"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![tablet_inv_id.as_str()]);

        let rm_err = h
            .call_as("device.remove", "tablet", json!({"id": owner_inv_id}))
            .await
            .unwrap_err();
        assert_eq!(rm_err.code, ErrorCode::NotFound);

        // An expiring HTTP credential may not issue a permanent device pairing invite.
        let mut expiring_call = Call::new(
            Door::Http,
            "device.invite",
            json!({"principal": "phone", "host": "tower.example", "user": "me"}),
        );
        expiring_call.credential_expires = Some(Timestamp::now() + SignedDuration::from_hours(1));
        let exp_err = h.node.call(expiring_call).await.unwrap_err();
        assert_eq!(exp_err.code, ErrorCode::Denied);
    }

    #[tokio::test]
    async fn expired_invite_is_refused_on_enrol_and_swept() {
        let h = Harness::new();
        let inv = h
            .call(
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let id = inv["id"].as_str().unwrap().to_owned();

        // Backdate the invite's expiry in state/devices.json to simulate expiration.
        let mut state = load_state(h.node.paths()).unwrap();
        state.invites.get_mut(&id).unwrap().expires = "2020-01-01T00:00:00Z".to_owned();
        save_state(h.node.paths(), &state).unwrap();

        let key = generate_ephemeral_ed25519().unwrap();
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"{}\",\"name\":\"Late Phone\"}}\n",
            key.public_key
        );
        let reply = run_enrol(&h.node, &id, req.as_bytes()).await;
        match reply {
            EnrolReply::Err { ok: false, error } => {
                assert_eq!(error.code, ErrorCode::Denied);
                assert!(error.message.contains("expired"), "{}", error.message);
            }
            other => panic!("expected expired error, got {other:?}"),
        }

        // The expired invite line and record were swept.
        let auth = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(!auth.contains(&id));
        assert!(load_state(h.node.paths()).unwrap().invites.is_empty());
    }

    #[tokio::test]
    async fn missing_host_keys_fails_with_clear_error_and_hint() {
        let h = Harness::new();
        let missing = h.dir.path().join("no_such_host_key.pub");
        let auth_escaped = h.auth_keys.display().to_string().replace('\\', "\\\\");
        let miss_escaped = missing.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            h.node.paths().config_file("node.toml"),
            format!(
                "name = \"tower\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\nhost_keys = [\"{miss_escaped}\"]\n"
            ),
        )
        .unwrap();

        let err = h
            .call(
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|hint| hint.contains("pinned host key"))
        );
    }

    #[tokio::test]
    async fn enrol_rejects_empty_oversized_and_malformed_requests_without_consuming_invite() {
        let h = Harness::new();
        let inv = h
            .call(
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let id = inv["id"].as_str().unwrap().to_owned();

        // Empty stdin.
        let empty = run_enrol(&h.node, &id, &b""[..]).await;
        assert!(!empty.is_ok());

        // Oversized line (> ENROL_MAX_LINE_BYTES).
        let huge = format!("{}\n", "x".repeat(enrol::ENROL_MAX_LINE_BYTES + 100));
        let over = run_enrol(&h.node, &id, huge.as_bytes()).await;
        assert!(!over.is_ok());

        // Unknown fields / wrong enrol version.
        let bad_ver = format!(
            "{{\"enrol\":2,\"id\":\"{id}\",\"public_key\":\"ssh-ed25519 AAAA\",\"name\":\"x\"}}\n"
        );
        assert!(!run_enrol(&h.node, &id, bad_ver.as_bytes()).await.is_ok());

        // Invite is still intact and usable after all rejected attempts.
        let listed = h.call("device.list", json!({})).await.unwrap();
        assert_eq!(listed["invites"].as_array().unwrap().len(), 1);
    }
}
