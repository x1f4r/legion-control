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
pub mod sessions;
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
    AuthKeysTarget, ForcedCommandShell, LineKind, PinnedHome, edit_auth_keys_with_retry,
    format_invite_line, line_matches_account, line_matches_home, resolve_safe_write_path,
    resolve_target, verify_exe_not_group_or_world_writable, verify_owner_matches_euid,
};
use self::hosts::{HostArg, resolve_hosts};
use self::keys::{
    InvitePayload, fingerprint_key_b64, generate_ephemeral_ed25519, generate_invite_id,
    resolve_host_keys, validate_invite_id,
};
use self::store::{InviteRecord, format_rfc3339, load_state, lock_auth_keys, save_state, sweep};

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
        "List paired devices and pending pairing invites for principals the caller could act as, reading authorized_keys as the source of truth joined with recorded metadata, and sweeping any expired or used invites. For owner callers, also reports Legion lines in authorized_keys belonging to another or missing Legion home (marked with `foreign`, `missing_home`, and `home`).",
        list,
    );
    registry.add_fn(
        "device.remove",
        Effect::Admin,
        "Remove a paired device by name or revoke a pending pairing invite by id for a principal the caller could act as, deleting its authorized_keys line(s) and recorded metadata and immediately ending any live sessions and same-account SSH connections for a removed device. Detached background jobs the device started keep running and are listed in `jobs_left_running` (stop them separately with `job.cancel` if needed). Pass `foreign=true` (owner role only) to remove a Legion line belonging to another or missing Legion home; for foreign devices this removes the key without ending sessions tracked under that other home and reports `sessions_not_ended: true`.",
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
    /// Where the pinned host keys came from (`configured`, `ssh-keyscan+files`, `ssh-keyscan only`,
    /// or `files`).
    pub host_keys_source: String,
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
            if raw.is_empty()
                || raw.len() > 128
                || raw.starts_with('-')
                || raw.chars().any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{raw:?} is not a valid SSH user name."),
                )
                .with_hint(
                    "Pass an OS account name that does not start with `-` and contains no spaces or control characters.",
                ));
            }
            raw
        }
        None => {
            let detected =
                resolve_default_ssh_user(ctx.paths().home(), legion_os::user::effective_uid())?;
            if detected.starts_with('-')
                || detected
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace())
            {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Detected SSH user name {detected:?} is not safe to place in an invite."
                    ),
                )
                .with_hint("Pass a valid `user` explicitly to device.invite."));
            }
            detected
        }
    };

    let hosts = resolve_hosts(args.host, ctx.cancel_token().child_token()).await?;
    let node_file = ctx.config().get::<NodeFile>()?;
    let target = resolve_target(ctx.paths(), &node_file)?;
    let pinned_account = if target.windows_admin_acl {
        let acct = legion_os::user::current_username().map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot pin OS account on shared administrators_authorized_keys: {error}."),
            )
        })?;
        Some(acct)
    } else {
        None
    };
    let resolved_host_keys = resolve_host_keys(
        &node_file.devices.host_keys,
        ctx.paths().home(),
        args.port,
        ctx.cancel_token().child_token(),
    )
    .await?;
    let host_keys = resolved_host_keys.keys;
    let host_keys_source = resolved_host_keys.source;
    let exe = legion_core::current_exe()?;
    verify_exe_not_group_or_world_writable(&exe)?;

    let now = Timestamp::from_second(Timestamp::now().as_second()).map_err(internal_time)?;
    let expires_ts = now
        .checked_add(SignedDuration::from_mins(i64::from(args.valid_minutes)))
        .map_err(internal_time)?;
    let created = format_rfc3339(now);
    let expires = format_rfc3339(expires_ts);

    let id = generate_invite_id()?;
    let ephemeral = generate_ephemeral_ed25519()?;
    let pinned_home = PinnedHome::from_paths(ctx.paths());
    let invite_line = format_invite_line(
        &exe,
        &id,
        expires_ts,
        &ephemeral.public_key,
        &pinned_home,
        pinned_account.as_deref(),
        ForcedCommandShell::host()?,
    )?;

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
        verify_owner_matches_euid(
            &write_path,
            paths.home(),
            target.is_default_home_ssh,
            legion_os::user::effective_uid(),
        )?;
        let _guard = lock_auth_keys(&target, &write_path)?;

        commit_invite_and_state(
            &paths,
            &target,
            &write_path,
            &id_for_write,
            &invite_line,
            record,
            now,
            save_state,
        )
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
        host_keys_source,
    })
}

/// What `device.list` returns.
#[derive(Debug, Serialize)]
struct ListResult {
    devices: Vec<ListedDevice>,
    invites: Vec<ListedInvite>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// One paired device listed by `device.list`.
#[derive(Debug, Serialize)]
struct ListedDevice {
    name: String,
    principal: String,
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    home: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    foreign: bool,
    #[serde(skip_serializing_if = "is_false")]
    missing_home: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    home: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    foreign: bool,
    #[serde(skip_serializing_if = "is_false")]
    missing_home: bool,
}

async fn list(ctx: &Ctx, _args: NoArgs) -> Result<ListResult, ToolError> {
    let caller = Caller::of(ctx)?;
    let caller_is_owner = caller.is_owner();
    let node_file = ctx.config().get::<NodeFile>()?;
    let paths = ctx.paths().clone();

    let (raw_devices, raw_invites) = blocking(move || {
        let target = resolve_target(&paths, &node_file)?;
        let caller_account = caller_account_for_target(&target)?;
        let write_path = resolve_safe_write_path(&target, paths.home())?;
        verify_owner_matches_euid(
            &write_path,
            paths.home(),
            target.is_default_home_ssh,
            legion_os::user::effective_uid(),
        )?;
        let _guard = lock_auth_keys(&target, &write_path)?;

        let (auth_keys, state, state_changed) =
            edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                let mut state = load_state(&paths)?;
                let now = Timestamp::now();
                let (auth_changed, state_changed) = sweep(&mut auth_keys, &mut state, now);
                let to_write = auth_changed.then(|| auth_keys.clone());
                Ok((to_write, (auth_keys, state, state_changed)))
            })?;
        if state_changed {
            save_state(&paths, &state)?;
        }
        sessions::sweep_stale_sessions(&paths);

        let mut devices = Vec::new();
        let mut invites = Vec::new();
        for line in &auth_keys.lines {
            match &line.kind {
                LineKind::Device {
                    name,
                    key_b64,
                    principal,
                    home,
                    account,
                    ..
                } => {
                    if !line_matches_account(
                        account.as_deref(),
                        caller_account.as_deref(),
                        target.windows_admin_acl,
                        cfg!(windows),
                    ) {
                        continue;
                    }
                    let is_this_home = line_matches_home(home.as_ref(), &paths)
                        || (home.is_none() && state.devices.contains_key(name));
                    let missing_home = home.as_ref().is_some_and(|h| !h.config_exists());
                    if !is_this_home {
                        if !caller_is_owner {
                            continue;
                        }
                        let fingerprint =
                            fingerprint_key_b64(key_b64).unwrap_or_else(|| "unknown".to_owned());
                        devices.push(ListedDevice {
                            name: name.clone(),
                            principal: principal.clone(),
                            fingerprint,
                            added: None,
                            home: home.as_ref().map(PinnedHome::display_label),
                            foreign: true,
                            missing_home,
                        });
                        continue;
                    }
                    let meta = state.devices.get(name);
                    let resolved_principal = meta
                        .map(|m| m.principal.clone())
                        .unwrap_or_else(|| principal.clone());
                    let fingerprint = fingerprint_key_b64(key_b64)
                        .or_else(|| meta.map(|m| m.fingerprint.clone()))
                        .unwrap_or_else(|| "unknown".to_owned());
                    devices.push(ListedDevice {
                        name: name.clone(),
                        principal: resolved_principal,
                        fingerprint,
                        added: meta.map(|m| m.added.clone()),
                        home: missing_home
                            .then(|| home.as_ref().map(PinnedHome::display_label))
                            .flatten(),
                        foreign: false,
                        missing_home,
                    });
                }
                LineKind::Invite {
                    id,
                    key_b64,
                    expiry_time,
                    home,
                    account,
                } => {
                    if !line_matches_account(
                        account.as_deref(),
                        caller_account.as_deref(),
                        target.windows_admin_acl,
                        cfg!(windows),
                    ) {
                        continue;
                    }
                    let is_this_home = line_matches_home(home.as_ref(), &paths);
                    let missing_home = home.as_ref().is_some_and(|h| !h.config_exists());
                    if !is_this_home {
                        if !caller_is_owner {
                            continue;
                        }
                        let fingerprint =
                            fingerprint_key_b64(key_b64).unwrap_or_else(|| "unknown".to_owned());
                        invites.push(ListedInvite {
                            id: id.clone(),
                            principal: "unknown".to_owned(),
                            fingerprint,
                            expires: expiry_time.clone().unwrap_or_else(|| "unknown".to_owned()),
                            created: None,
                            home: home.as_ref().map(PinnedHome::display_label),
                            foreign: true,
                            missing_home,
                        });
                        continue;
                    }
                    let Some(meta) = state.invites.get(id) else {
                        continue;
                    };
                    let fingerprint =
                        fingerprint_key_b64(key_b64).unwrap_or_else(|| meta.fingerprint.clone());
                    let expires = meta.expires.clone();
                    invites.push(ListedInvite {
                        id: id.clone(),
                        principal: meta.principal.clone(),
                        fingerprint,
                        expires,
                        created: Some(meta.created.clone()),
                        home: missing_home
                            .then(|| home.as_ref().map(PinnedHome::display_label))
                            .flatten(),
                        foreign: false,
                        missing_home,
                    });
                }
                LineKind::Foreign => {}
            }
        }
        Ok((devices, invites))
    })
    .await?;

    let devices = raw_devices
        .into_iter()
        .filter(|d| d.foreign || caller.may_manage(&d.principal))
        .collect();

    let invites = raw_invites
        .into_iter()
        .filter(|i| i.foreign || caller.may_manage(&i.principal))
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
    /// Remove a Legion device or invite line belonging to another or missing Legion home (owner
    /// role only). Off unless given as true.
    #[serde(default)]
    foreign: bool,
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
    sessions_ended: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    sessions_not_ended: bool,
    jobs_left_running: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
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
    let remove_foreign = args.foreign;
    if remove_foreign && !caller.is_owner() {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "Only a caller with the owner role may remove device or invite lines belonging to another or missing Legion home.",
        )
        .with_hint("Run `device.remove` as the owner of this machine."));
    }
    let node_file = ctx.config().get::<NodeFile>()?;
    let paths = ctx.paths().clone();
    let paths_for_sessions = paths.clone();

    let (mut removed, terminate_device) = blocking(move || {
        let target = resolve_target(&paths, &node_file)?;
        let caller_account = caller_account_for_target(&target)?;
        let write_path = resolve_safe_write_path(&target, paths.home())?;
        verify_owner_matches_euid(
            &write_path,
            paths.home(),
            target.is_default_home_ssh,
            legion_os::user::effective_uid(),
        )?;
        let _guard = lock_auth_keys(&target, &write_path)?;

        let (outcome, state_to_save) =
            edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                let mut state = load_state(&paths)?;
                let now = Timestamp::now();
                let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

                match &target_item {
                    RemoveTarget::Device(name) => {
                        let mut visible_names: BTreeMap<String, String> = BTreeMap::new();
                        let mut foreign_match_home: Option<String> = None;
                        let mut matched_principal: Option<String> = None;
                        let mut matched_lines = 0usize;

                        for line in &auth_keys.lines {
                            if let LineKind::Device {
                                name: line_name,
                                principal,
                                home,
                                account,
                                ..
                            } = &line.kind
                            {
                                if !line_matches_account(
                                    account.as_deref(),
                                    caller_account.as_deref(),
                                    target.windows_admin_acl,
                                    cfg!(windows),
                                ) {
                                    continue;
                                }
                                let is_this_home = line_matches_home(home.as_ref(), &paths)
                                    || (home.is_none() && state.devices.contains_key(line_name));
                                if remove_foreign {
                                    if is_this_home {
                                        continue;
                                    }
                                    visible_names
                                        .entry(line_name.clone())
                                        .or_insert_with(|| principal.clone());
                                    if line_name == name {
                                        matched_principal.get_or_insert_with(|| principal.clone());
                                        matched_lines += 1;
                                    }
                                    continue;
                                }
                                if !is_this_home {
                                    if caller.is_owner() && line_name == name {
                                        foreign_match_home.get_or_insert_with(|| {
                                            home.as_ref()
                                                .map(PinnedHome::display_label)
                                                .unwrap_or_else(|| "another home".to_owned())
                                        });
                                    }
                                    continue;
                                }
                                let resolved = state
                                    .devices
                                    .get(line_name)
                                    .map(|m| m.principal.clone())
                                    .unwrap_or_else(|| principal.clone());
                                if caller.may_manage(&resolved) {
                                    visible_names
                                        .entry(line_name.clone())
                                        .or_insert_with(|| resolved.clone());
                                    if line_name == name {
                                        matched_principal.get_or_insert(resolved);
                                        matched_lines += 1;
                                    }
                                }
                            }
                        }

                        let Some(principal) = matched_principal else {
                            let hint = if let Some(other_home) = foreign_match_home {
                                format!(
                                    "Device {name} belongs to another Legion home ({other_home}); pass `foreign=true` to remove it explicitly."
                                )
                            } else if visible_names.is_empty() {
                                if remove_foreign {
                                    "There are no paired devices belonging to another or missing Legion home.".to_owned()
                                } else {
                                    "There are no paired devices.".to_owned()
                                }
                            } else {
                                let known: Vec<&str> =
                                    visible_names.keys().map(String::as_str).collect();
                                if remove_foreign {
                                    format!("The foreign paired devices are {}.", known.join(", "))
                                } else {
                                    format!("The paired devices are {}.", known.join(", "))
                                }
                            };
                            let err = ErrorBody::new(
                                ErrorCode::NotFound,
                                if remove_foreign {
                                    format!(
                                        "There is no paired device called {name} belonging to another or missing Legion home."
                                    )
                                } else {
                                    format!("There is no paired device called {name}.")
                                },
                            )
                            .with_hint(hint);
                            return Ok((
                                swept_auth.then_some(auth_keys),
                                (Err(err), swept_state.then_some(state)),
                            ));
                        };

                        auth_keys.retain(|line| {
                            !matches!(
                                &line.kind,
                                LineKind::Device { name: line_name, home, account, .. }
                                    if line_name == name
                                        && line_matches_account(
                                            account.as_deref(),
                                            caller_account.as_deref(),
                                            target.windows_admin_acl,
                                            cfg!(windows),
                                        )
                                        && {
                                            let is_this_home = line_matches_home(home.as_ref(), &paths)
                                                || (home.is_none()
                                                    && state.devices.contains_key(line_name));
                                            if remove_foreign { !is_this_home } else { is_this_home }
                                        }
                            )
                        });
                        let terminate = if remove_foreign {
                            None
                        } else {
                            state.devices.remove(name);
                            Some(name.clone())
                        };
                        Ok((
                            Some(auth_keys),
                            (
                                Ok((
                                    Removed {
                                        kind: "device",
                                        name: Some(name.clone()),
                                        id: None,
                                        principal,
                                        removed: true,
                                        lines_removed: matched_lines,
                                        sessions_ended: 0,
                                        sessions_not_ended: remove_foreign,
                                        jobs_left_running: Vec::new(),
                                        hint: remove_foreign.then(|| {
                                            "Active sessions tracked under another Legion home were not ended; new SSH connections and channels are refused immediately, and if that other home still exists you can run `device.remove` from it or end any open sshd sessions manually.".to_owned()
                                        }),
                                    },
                                    terminate,
                                )),
                                if remove_foreign {
                                    swept_state.then_some(state)
                                } else {
                                    Some(state)
                                },
                            ),
                        ))
                    }
                    RemoveTarget::Invite(id) => {
                        let mut visible_ids: Vec<String> = Vec::new();
                        let mut foreign_match_home: Option<String> = None;
                        let mut matched_principal: Option<String> = None;
                        let mut matched_lines = 0usize;

                        for line in &auth_keys.lines {
                            if let LineKind::Invite {
                                id: line_id,
                                home,
                                account,
                                ..
                            } = &line.kind
                            {
                                if !line_matches_account(
                                    account.as_deref(),
                                    caller_account.as_deref(),
                                    target.windows_admin_acl,
                                    cfg!(windows),
                                ) {
                                    continue;
                                }
                                let is_this_home = line_matches_home(home.as_ref(), &paths);
                                if remove_foreign {
                                    if is_this_home {
                                        continue;
                                    }
                                    if !visible_ids.contains(line_id) {
                                        visible_ids.push(line_id.clone());
                                    }
                                    if line_id == id {
                                        matched_principal
                                            .get_or_insert_with(|| "unknown".to_owned());
                                        matched_lines += 1;
                                    }
                                    continue;
                                }
                                if !is_this_home {
                                    if caller.is_owner() && line_id == id {
                                        foreign_match_home.get_or_insert_with(|| {
                                            home.as_ref()
                                                .map(PinnedHome::display_label)
                                                .unwrap_or_else(|| "another home".to_owned())
                                        });
                                    }
                                    continue;
                                }
                                if let Some(meta) = state.invites.get(line_id)
                                    && caller.may_manage(&meta.principal)
                                {
                                    if !visible_ids.contains(line_id) {
                                        visible_ids.push(line_id.clone());
                                    }
                                    if line_id == id {
                                        matched_principal
                                            .get_or_insert_with(|| meta.principal.clone());
                                        matched_lines += 1;
                                    }
                                }
                            }
                        }

                        let Some(principal) = matched_principal else {
                            let hint = if let Some(other_home) = foreign_match_home {
                                format!(
                                    "Invite {id} belongs to another Legion home ({other_home}); pass `foreign=true` to remove it explicitly."
                                )
                            } else if visible_ids.is_empty() {
                                if remove_foreign {
                                    "There are no pending invites belonging to another or missing Legion home.".to_owned()
                                } else {
                                    "There are no pending invites.".to_owned()
                                }
                            } else if remove_foreign {
                                format!("The foreign pending invites are {}.", visible_ids.join(", "))
                            } else {
                                format!("The pending invites are {}.", visible_ids.join(", "))
                            };
                            let err = ErrorBody::new(
                                ErrorCode::NotFound,
                                if remove_foreign {
                                    format!(
                                        "There is no pending invite with id {id} belonging to another or missing Legion home."
                                    )
                                } else {
                                    format!("There is no pending invite with id {id}.")
                                },
                            )
                            .with_hint(hint);
                            return Ok((
                                swept_auth.then_some(auth_keys),
                                (Err(err), swept_state.then_some(state)),
                            ));
                        };

                        auth_keys.retain(|line| {
                            !matches!(
                                &line.kind,
                                LineKind::Invite { id: line_id, home, account, .. }
                                    if line_id == id
                                        && line_matches_account(
                                            account.as_deref(),
                                            caller_account.as_deref(),
                                            target.windows_admin_acl,
                                            cfg!(windows),
                                        )
                                        && {
                                            let is_this_home = line_matches_home(home.as_ref(), &paths);
                                            if remove_foreign { !is_this_home } else { is_this_home }
                                        }
                            )
                        });
                        if !remove_foreign {
                            state.invites.remove(id);
                        }
                        Ok((
                            Some(auth_keys),
                            (
                                Ok((
                                    Removed {
                                        kind: "invite",
                                        name: None,
                                        id: Some(id.clone()),
                                        principal,
                                        removed: true,
                                        lines_removed: matched_lines,
                                        sessions_ended: 0,
                                        sessions_not_ended: false,
                                        jobs_left_running: Vec::new(),
                                        hint: None,
                                    },
                                    None,
                                )),
                                if remove_foreign {
                                    swept_state.then_some(state)
                                } else {
                                    Some(state)
                                },
                            ),
                        ))
                    }
                }
            })?;
        match outcome {
            Ok(result) => {
                if let Some(state) = state_to_save {
                    save_state(&paths, &state)?;
                }
                Ok(result)
            }
            Err(err) => {
                if let Some(state) = state_to_save {
                    let _ = save_state(&paths, &state);
                }
                Err(err)
            }
        }
    })
    .await?;

    if let Some(device_name) = terminate_device {
        removed.sessions_ended = sessions::terminate_device_sessions(
            &paths_for_sessions,
            &device_name,
            sessions::DEFAULT_TERMINATE_GRACE,
        )
        .await;
        let paths_for_jobs = paths_for_sessions.clone();
        let principal_for_jobs = removed.principal.clone();
        removed.jobs_left_running = blocking(move || {
            Ok(crate::job::running_job_ids_for_device(
                &paths_for_jobs,
                &device_name,
                &principal_for_jobs,
            ))
        })
        .await
        .unwrap_or_default();
    }

    Ok(removed)
}

#[allow(clippy::too_many_arguments)]
fn commit_invite_and_state(
    paths: &legion_core::Paths,
    target: &AuthKeysTarget,
    write_path: &std::path::Path,
    invite_id: &str,
    invite_line: &str,
    record: InviteRecord,
    now: Timestamp,
    save_fn: impl FnOnce(&legion_core::Paths, &store::DevicesState) -> Result<(), ErrorBody>,
) -> Result<(), ErrorBody> {
    let mut state = edit_auth_keys_with_retry(target, write_path, |mut auth_keys| {
        let mut state = load_state(paths)?;
        sweep(&mut auth_keys, &mut state, now);
        auth_keys.append_line(invite_line);
        Ok((Some(auth_keys), state))
    })?;

    state.invites.insert(invite_id.to_owned(), record);
    if let Err(state_err) = save_fn(paths, &state) {
        // Roll back through the guarded concurrent-edit path by removing only this invite's line,
        // so any out-of-band edit that landed after the append is preserved intact.
        let _ = rollback_invite_line(target, write_path, invite_id);
        return Err(state_err);
    }
    Ok(())
}

fn rollback_invite_line(
    target: &AuthKeysTarget,
    write_path: &std::path::Path,
    invite_id: &str,
) -> Result<(), ErrorBody> {
    edit_auth_keys_with_retry(target, write_path, |mut auth_keys| {
        let removed = auth_keys
            .retain(|line| !matches!(&line.kind, LineKind::Invite { id, .. } if id == invite_id));
        Ok((removed.then_some(auth_keys), ()))
    })
}

fn caller_account_for_target(
    target: &self::authkeys::AuthKeysTarget,
) -> Result<Option<String>, ErrorBody> {
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

fn resolve_default_ssh_user(
    home: Option<&std::path::Path>,
    euid: Option<u32>,
) -> Result<String, ErrorBody> {
    if let (Some(home), Some(euid)) = (home, euid)
        && home.exists()
        && let Some(owner_uid) = legion_os::user::path_owner_uid(home).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Home directory {} cannot be inspected: {error}.",
                    home.display()
                ),
            )
        })?
    {
        if owner_uid != euid {
            let owner_label = legion_os::user::username_for_uid(owner_uid)
                .map(|name| format!("{name} (uid {owner_uid})"))
                .unwrap_or_else(|| format!("uid {owner_uid}"));
            let runner_label = legion_os::user::username_for_uid(euid)
                .map(|name| format!("{name} (uid {euid})"))
                .unwrap_or_else(|| format!("uid {euid}"));
            let hint_user = legion_os::user::username_for_uid(owner_uid)
                .unwrap_or_else(|| format!("#{owner_uid}"));
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Home directory {} is owned by {owner_label}, which does not match the running account {runner_label}.",
                    home.display()
                ),
            )
            .with_hint(format!(
                "Run legion as {hint_user} (for example `sudo -u {hint_user} legion pair ...`) instead of running with another user's HOME."
            )));
        }
        if let Some(name) = legion_os::user::username_for_uid(owner_uid) {
            return Ok(name);
        }
    }
    legion_os::user::current_username().map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot determine the account legion runs as: {error}."),
        )
        .with_hint("Pass `user` explicitly to device.invite.")
    })
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
        let foreign_key = generate_ephemeral_ed25519().unwrap();
        let foreign = format!("{} me@laptop", foreign_key.public_key);
        std::fs::write(&h.auth_keys, &foreign).unwrap();

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
        let temp_pub = keys::ed25519_public_key_from_seed(&decoded.decode_seed().unwrap());
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

        // Enrolling the invite's own temporary key is refused and leaves the invite untouched.
        let self_key_req = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"{temp_pub}\",\"name\":\"Reused Temp\"}}\n",
        );
        let self_key_reply = run_enrol(&h.node, id, self_key_req.as_bytes()).await;
        assert!(matches!(
            self_key_reply,
            EnrolReply::Err { ref error, .. } if error.code == ErrorCode::Denied
        ));

        // Enrolling a key already present on a foreign line in authorized_keys is refused.
        let foreign_key_req = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"{}\",\"name\":\"Foreign Key\"}}\n",
            foreign_key.public_key
        );
        let foreign_key_reply = run_enrol(&h.node, id, foreign_key_req.as_bytes()).await;
        assert!(matches!(
            foreign_key_reply,
            EnrolReply::Err { ref error, .. } if error.code == ErrorCode::Conflict
        ));

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
        // Enrolling an already-paired device's key is also refused.
        let dup_device_req = format!(
            "{{\"enrol\":1,\"id\":\"{id2}\",\"public_key\":\"{}\",\"name\":\"Duplicate Key\"}}\n",
            device_key1.public_key
        );
        let dup_device_reply = run_enrol(&h.node, id2, dup_device_req.as_bytes()).await;
        assert!(matches!(
            dup_device_reply,
            EnrolReply::Err { ref error, .. } if error.code == ErrorCode::Conflict
        ));
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
        assert!(auth_after.contains(&foreign));
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
        // Reject leading `-`, whitespace, and control chars in `user` and `host`.
        for (bad_args, label) in [
            (
                json!({"principal": "phone", "host": "tower.example", "user": "-oProxyCommand=id"}),
                "user leading dash",
            ),
            (
                json!({"principal": "phone", "host": "tower.example", "user": "bad user"}),
                "user whitespace",
            ),
            (
                json!({"principal": "phone", "host": "tower.example", "user": "bad\nuser"}),
                "user control",
            ),
            (
                json!({"principal": "phone", "host": "-oProxyCommand=id", "user": "me"}),
                "host leading dash",
            ),
        ] {
            let err = h.call("device.invite", bad_args).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{label}: {err:?}");
        }
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

    #[cfg(unix)]
    #[test]
    fn default_ssh_user_uses_home_owner_and_refuses_euid_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let actual_uid = legion_os::user::effective_uid().unwrap();
        let wrong_uid = if actual_uid == 0 { 65534 } else { 0 };

        let resolved = resolve_default_ssh_user(Some(dir.path()), Some(actual_uid)).unwrap();
        let expected = legion_os::user::username_for_uid(actual_uid)
            .unwrap_or_else(|| legion_os::user::current_username().unwrap());
        assert_eq!(resolved, expected);

        let err = resolve_default_ssh_user(Some(dir.path()), Some(wrong_uid)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(err.message.contains("owned by"), "{}", err.message);
    }

    #[tokio::test]
    async fn two_legion_homes_sharing_one_authorized_keys_do_not_clobber_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let shared_auth = dir.path().join("shared_authorized_keys");
        let host_key_path = dir.path().join("ssh_host_ed25519_key.pub");
        let host_key = generate_ephemeral_ed25519().unwrap().public_key;
        std::fs::write(&host_key_path, format!("{host_key} root@tower\n")).unwrap();

        let make_node = |label: &str| {
            let paths = Paths::under(dir.path().join(label)).with_home(&home);
            std::fs::create_dir_all(paths.config_dir()).unwrap();
            let auth_escaped = shared_auth.display().to_string().replace('\\', "\\\\");
            let hk_escaped = host_key_path.display().to_string().replace('\\', "\\\\");
            std::fs::write(
                paths.config_file("node.toml"),
                format!(
                    "name = \"{label}\"\n[devices]\nauthorized_keys = \"{auth_escaped}\"\nhost_keys = [\"{hk_escaped}\"]\n"
                ),
            )
            .unwrap();
            std::fs::write(
                paths.config_file("policy.toml"),
                "default = \"owner\"\n\
                 [roles.observer]\nallow = [\"effect:read\"]\n\
                 [roles.admin_self]\nallow = [\"effect:read\", \"effect:admin\"]\n\
                 [principals.phone]\nrole = \"observer\"\n\
                 [principals.helper]\nrole = \"admin_self\"\n",
            )
            .unwrap();
            Node::new(paths, crate::registry())
        };

        let node_a = make_node("home-a");
        let node_b = make_node("home-b");

        // Both homes create invites into the same authorized_keys file.
        let inv_a_exp = node_a
            .call(Call::new(
                Door::Cli,
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            ))
            .await
            .unwrap();
        let inv_a = node_a
            .call(Call::new(
                Door::Cli,
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            ))
            .await
            .unwrap();
        let inv_b = node_b
            .call(Call::new(
                Door::Cli,
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            ))
            .await
            .unwrap();
        let id_a_exp = inv_a_exp["id"].as_str().unwrap().to_owned();
        let id_a = inv_a["id"].as_str().unwrap().to_owned();
        let id_b = inv_b["id"].as_str().unwrap().to_owned();

        // Lock file sits beside shared_authorized_keys (owner-only on Unix), not in state/.
        let lock_file = store::auth_keys_lock_path(&shared_auth);
        assert!(lock_file.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&lock_file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        // Expire only home-a's first invite; sweeping on home-a must leave home-b's invite intact.
        let mut state_a = load_state(node_a.paths()).unwrap();
        state_a.invites.get_mut(&id_a_exp).unwrap().expires = "2020-01-01T00:00:00Z".to_owned();
        save_state(node_a.paths(), &state_a).unwrap();

        let list_a = node_a
            .call(Call::new(Door::Cli, "device.list", json!({})))
            .await
            .unwrap();
        let invites_a = list_a["invites"].as_array().unwrap();
        assert_eq!(invites_a.len(), 2, "{list_a}");
        let own_inv_a = invites_a.iter().find(|i| i["id"] == json!(id_a)).unwrap();
        assert!(own_inv_a.get("foreign").is_none());
        let foreign_inv_b = invites_a.iter().find(|i| i["id"] == json!(id_b)).unwrap();
        assert_eq!(foreign_inv_b["foreign"], json!(true));
        assert!(foreign_inv_b.get("missing_home").is_none());
        assert!(
            foreign_inv_b["home"]
                .as_str()
                .is_some_and(|h| h.contains("home-b")),
            "{foreign_inv_b}"
        );

        // home-b sees its own invite plus home-a's as foreign, and the file holds both active invites.
        let list_b = node_b
            .call(Call::new(Door::Cli, "device.list", json!({})))
            .await
            .unwrap();
        let invites_b = list_b["invites"].as_array().unwrap();
        assert_eq!(invites_b.len(), 2, "{list_b}");
        assert!(
            invites_b
                .iter()
                .any(|i| i["id"] == json!(id_b) && i.get("foreign").is_none())
        );
        let auth_text = std::fs::read_to_string(&shared_auth).unwrap();
        assert!(!auth_text.contains(&id_a_exp));
        assert!(auth_text.contains(&id_a));
        assert!(auth_text.contains(&id_b));
        assert!(auth_text.contains("--home"));

        // Enrol one device on each home with distinct keys; owner sees own + foreign marked clearly.
        let key_a = generate_ephemeral_ed25519().unwrap();
        let key_b = generate_ephemeral_ed25519().unwrap();
        let req_a = format!(
            "{{\"enrol\":1,\"id\":\"{id_a}\",\"public_key\":\"{}\",\"name\":\"Phone\"}}\n",
            key_a.public_key
        );
        let req_b = format!(
            "{{\"enrol\":1,\"id\":\"{id_b}\",\"public_key\":\"{}\",\"name\":\"Tablet\"}}\n",
            key_b.public_key
        );
        assert!(run_enrol(&node_a, &id_a, req_a.as_bytes()).await.is_ok());
        assert!(run_enrol(&node_b, &id_b, req_b.as_bytes()).await.is_ok());

        let list_a = node_a
            .call(Call::new(Door::Cli, "device.list", json!({})))
            .await
            .unwrap();
        let devices_a = list_a["devices"].as_array().unwrap();
        assert_eq!(devices_a.len(), 2, "{list_a}");
        assert!(
            devices_a
                .iter()
                .any(|d| d["name"] == json!("phone") && d.get("foreign").is_none())
        );
        let foreign_tablet = devices_a
            .iter()
            .find(|d| d["name"] == json!("tablet"))
            .unwrap();
        assert_eq!(foreign_tablet["foreign"], json!(true));
        assert!(foreign_tablet.get("missing_home").is_none());
        assert!(
            foreign_tablet["home"]
                .as_str()
                .is_some_and(|h| h.contains("home-b"))
        );

        // A non-owner admin caller neither sees foreign lines nor may pass `foreign=true`.
        let mut helper_list = Call::new(Door::Cli, "device.list", json!({}));
        helper_list.pinned = Some("helper".to_owned());
        let helper_listed = node_a.call(helper_list).await.unwrap();
        assert_eq!(helper_listed["devices"].as_array().unwrap().len(), 0);

        let mut helper_rm = Call::new(
            Door::Cli,
            "device.remove",
            json!({"name": "tablet", "foreign": true}),
        );
        helper_rm.pinned = Some("helper".to_owned());
        let helper_err = node_a.call(helper_rm).await.unwrap_err();
        assert_eq!(helper_err.code, ErrorCode::Denied);

        // Without `foreign=true`, home-a refuses to remove home-b's device and hints `foreign=true`.
        let cross_rm = node_a
            .call(Call::new(
                Door::Cli,
                "device.remove",
                json!({"name": "tablet"}),
            ))
            .await
            .unwrap_err();
        assert_eq!(cross_rm.code, ErrorCode::NotFound);
        assert!(
            cross_rm
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("foreign=true")),
            "{:?}",
            cross_rm.hint
        );

        // Removing home-a's device leaves home-b's device line untouched and does not set
        // `sessions_not_ended` or `hint`.
        let removed_local = node_a
            .call(Call::new(
                Door::Cli,
                "device.remove",
                json!({"name": "phone"}),
            ))
            .await
            .unwrap();
        assert!(removed_local.get("sessions_not_ended").is_none());
        assert!(removed_local.get("hint").is_none());
        let list_b = node_b
            .call(Call::new(Door::Cli, "device.list", json!({})))
            .await
            .unwrap();
        assert_eq!(list_b["devices"].as_array().unwrap().len(), 1);
        assert_eq!(list_b["devices"][0]["name"], json!("tablet"));

        // Delete home-b completely: tablet becomes an orphaned device with `missing_home=true`,
        // `meta.doctor` on home-a warns about it, and owner can remove it with `foreign=true`.
        std::fs::remove_dir_all(dir.path().join("home-b")).unwrap();
        let list_after_delete = node_a
            .call(Call::new(Door::Cli, "device.list", json!({})))
            .await
            .unwrap();
        let orphaned = &list_after_delete["devices"][0];
        assert_eq!(orphaned["name"], json!("tablet"));
        assert_eq!(orphaned["foreign"], json!(true));
        assert_eq!(orphaned["missing_home"], json!(true));

        let doctor = node_a
            .call(Call::new(Door::Cli, "meta.doctor", json!({"quick": true})))
            .await
            .unwrap();
        let checks = doctor["checks"].as_array().unwrap();
        let orphan_check = checks
            .iter()
            .find(|c| c["name"] == json!("orphaned device keys"))
            .expect("meta.doctor should warn about orphaned device keys");
        assert_eq!(orphan_check["ok"], json!(true));
        assert_eq!(orphan_check["warning"], json!(true));
        assert!(
            orphan_check["detail"]
                .as_str()
                .is_some_and(|d| d.contains("tablet") && d.contains("home-b")),
            "{orphan_check}"
        );

        let removed_foreign = node_a
            .call(Call::new(
                Door::Cli,
                "device.remove",
                json!({"name": "tablet", "foreign": true}),
            ))
            .await
            .unwrap();
        assert_eq!(removed_foreign["removed"], json!(true));
        assert_eq!(removed_foreign["lines_removed"], json!(1));
        assert_eq!(removed_foreign["sessions_not_ended"], json!(true));
        assert!(
            removed_foreign["hint"]
                .as_str()
                .is_some_and(|h| h.contains("another") && h.contains("session")),
            "expected clear sessions_not_ended hint, got: {removed_foreign}"
        );
        assert!(
            !std::fs::read_to_string(&shared_auth)
                .unwrap()
                .contains("legion-device:tablet")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn enrol_replies_ok_when_authorized_keys_swap_succeeds_even_if_state_write_fails() {
        use std::os::unix::fs::PermissionsExt;

        if legion_os::user::effective_uid() == Some(0) {
            return;
        }
        let h = Harness::new();
        let inv = h
            .call(
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let id = inv["id"].as_str().unwrap().to_owned();
        let device_key = generate_ephemeral_ed25519().unwrap();

        // Make state_dir read-only (0500) so load_state succeeds, authorized_keys swap succeeds,
        // and save_state fails with PermissionDenied.
        let state_dir = h.node.paths().state_dir().to_path_buf();
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let line = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": id,
                "public_key": device_key.public_key,
                "name": "Resilient Phone",
            })
        );
        let reply = run_enrol(&h.node, &id, line.as_bytes()).await;

        // Restore writable permissions so cleanup and device.list succeed.
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert_eq!(
            reply,
            EnrolReply::Ok {
                ok: true,
                node: "tower".to_owned(),
                principal: "phone".to_owned(),
                role: "observer".to_owned(),
                device: "resilient-phone".to_owned(),
            }
        );

        // authorized_keys has the permanent device line and no longer has the invite line.
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(auth_text.contains("legion-device:resilient-phone"));
        assert!(!auth_text.contains(&format!("legion-invite:{id}")));

        // Even with stale/missing state metadata, device.list reports the paired device cleanly.
        let listed = h.call("device.list", json!({})).await.unwrap();
        assert_eq!(listed["invites"].as_array().unwrap().len(), 0);
        let devices = listed["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["name"], json!("resilient-phone"));
        assert_eq!(devices[0]["principal"], json!("phone"));
        assert_eq!(devices[0]["fingerprint"], json!(device_key.fingerprint));
    }

    #[tokio::test]
    async fn enrol_rejects_duplicate_key_behind_non_utf8_foreign_comment() {
        let h = Harness::new();
        let existing_key = generate_ephemeral_ed25519().unwrap();
        let mut foreign_raw = Vec::new();
        foreign_raw.extend_from_slice(existing_key.public_key.as_bytes());
        foreign_raw.extend_from_slice(b" legacy-\xff\xfe-comment\n");
        std::fs::write(&h.auth_keys, &foreign_raw).unwrap();

        let inv = h
            .call(
                "device.invite",
                json!({"principal": "phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let id = inv["id"].as_str().unwrap().to_owned();

        let req = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": id,
                "public_key": existing_key.public_key,
                "name": "Duplicate Phone",
            })
        );
        let reply = run_enrol(&h.node, &id, req.as_bytes()).await;
        match reply {
            EnrolReply::Err { ok: false, error } => {
                assert_eq!(error.code, ErrorCode::Conflict);
            }
            other => {
                panic!("expected Conflict refusal for non-UTF-8 foreign duplicate, got {other:?}")
            }
        }

        // The original non-UTF-8 foreign line must remain untouched byte-for-byte.
        let after = std::fs::read(&h.auth_keys).unwrap();
        assert!(after.starts_with(&foreign_raw));
    }

    #[tokio::test]
    async fn hand_written_and_legacy_no_device_lines_are_untouchable_foreign() {
        let h = Harness::new();
        let key1 = generate_ephemeral_ed25519().unwrap();
        let key2 = generate_ephemeral_ed25519().unwrap();
        let hand_written = format!("{} legion-device:handwritten\n", key1.public_key);
        let legacy_no_device = format!(
            "restrict,command=\"/home/me/legion gate --as phone\" {} legion-device:legacy\n",
            key2.public_key
        );
        let initial = format!("{hand_written}{legacy_no_device}");
        std::fs::write(&h.auth_keys, &initial).unwrap();

        let listed = h.call("device.list", json!({})).await.unwrap();
        assert_eq!(listed["devices"].as_array().unwrap().len(), 0);
        assert_eq!(listed["invites"].as_array().unwrap().len(), 0);

        for name in ["handwritten", "legacy"] {
            let err = h
                .call("device.remove", json!({"name": name}))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::NotFound, "{name}");
        }
        assert_eq!(std::fs::read_to_string(&h.auth_keys).unwrap(), initial);
    }

    #[test]
    fn invite_rollback_removes_only_invite_line_and_preserves_concurrent_edits() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = Paths::under(dir.path().join("legion")).with_home(&home);
        std::fs::create_dir_all(paths.state_dir()).unwrap();

        let now: Timestamp = "2026-11-24T12:00:00Z".parse().unwrap();
        let exp: Timestamp = "2026-11-24T12:15:00Z".parse().unwrap();
        let eph = generate_ephemeral_ed25519().unwrap();
        let concurrent_key = generate_ephemeral_ed25519().unwrap();
        let concurrent_line = format!("{} added-concurrently@host\n", concurrent_key.public_key);

        for (label, initial) in [
            ("existing_file", Some("# preexisting\n")),
            ("newly_created_file", None),
        ] {
            let auth_path = dir.path().join(format!("authorized_keys_{label}"));
            if let Some(seed_content) = initial {
                std::fs::write(&auth_path, seed_content).unwrap();
            }
            let target = AuthKeysTarget {
                path: auth_path.clone(),
                is_default_home_ssh: false,
                windows_admin_acl: false,
            };
            let id = format!("rollback{label}");
            let fake_exe = if cfg!(windows) {
                dir.path().join("legion.exe")
            } else {
                dir.path().join("legion")
            };
            let shell = if cfg!(windows) {
                ForcedCommandShell::WindowsCmd
            } else {
                ForcedCommandShell::Posix
            };
            let invite_line = format_invite_line(
                &fake_exe,
                &id,
                exp,
                &eph.public_key,
                &PinnedHome::from_paths(&paths),
                None,
                shell,
            )
            .unwrap();
            let record = InviteRecord {
                id: id.clone(),
                principal: "phone".to_owned(),
                created: format_rfc3339(now),
                expires: format_rfc3339(exp),
                fingerprint: eph.fingerprint.clone(),
            };

            let err = commit_invite_and_state(
                &paths,
                &target,
                &auth_path,
                &id,
                &invite_line,
                record,
                now,
                |_, _| {
                    // Simulate an out-of-band edit landing between the invite append and rollback.
                    let mut current = std::fs::read_to_string(&auth_path).unwrap();
                    current.push_str(&concurrent_line);
                    std::fs::write(&auth_path, current).unwrap();
                    Err(ErrorBody::new(
                        ErrorCode::Failed,
                        "simulated devices.json save failure",
                    ))
                },
            )
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Failed, "{label}");

            let final_text = std::fs::read_to_string(&auth_path).unwrap();
            assert!(
                !final_text.contains(&format!("legion-invite:{id}")),
                "{label}: invite line was not rolled back: {final_text}"
            );
            assert!(
                final_text.contains(&concurrent_line),
                "{label}: concurrent edit was lost during rollback: {final_text}"
            );
            if let Some(seed_content) = initial {
                assert!(final_text.contains(seed_content), "{label}: {final_text}");
            }
        }
    }
}
