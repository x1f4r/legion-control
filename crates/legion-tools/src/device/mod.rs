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
pub mod registry;
pub mod sessions;
pub mod store;

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use legion_core::config::{Document, NodeFile, NodeName};
use legion_core::policy::{NOBODY, OWNER, Policy, Principal};
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
    resolve_host_keys, validate_invite_id, validate_pairing_id,
};
use self::registry::{
    DEVICE_PRINCIPAL_PREFIX, DeviceTombstone, DevicesFile, FleetPushOutcome,
    adopt_legacy_devices_from_auth_keys, load_devices_raw, lock_config, next_updated_timestamp,
    push_devices_to_fleet, reconcile_authorized_keys_in_memory, reconcile_device_principals_locked,
    save_devices_locked, upsert_device_principal_locked, validate_role_name,
};
use self::store::{
    InviteRecord, format_rfc3339, load_state, lock_auth_keys, sanitise_device_name, save_state,
    sweep,
};

/// Default invite validity in minutes.
pub const DEFAULT_VALID_MINUTES: u32 = 15;

/// Minimum invite validity in minutes.
pub const MIN_VALID_MINUTES: u32 = 1;

/// Maximum invite validity in minutes (24 hours).
pub const MAX_VALID_MINUTES: u32 = 1440;

/// Default SSH port for pairing invites.
pub const DEFAULT_SSH_PORT: u16 = 22;

/// Adds the `device` tools (`device.invite`, `device.list`, `device.remove`, `device.set`,
/// `device.endpoint`) and registers `devices.toml`.
pub fn register(registry: &mut Registry) {
    registry.add_document::<registry::DevicesFile>();
    registry.add_fn(
        "device.invite",
        Effect::Admin,
        "Create a single-use pairing invite for a remote device to connect over SSH. Either give `role` and `name` (owner role only: reserves `device:<name>` and creates it in policy.toml when the device enrols, leaving nothing in policy.toml if unused or expired) or `principal` (an existing non-device-owned principal that policy.toml names and the caller could act as itself). Appends a temporary key with sshd's expiry-time and a forced enrolment command to authorized_keys, and returns the invite text once (never logged or stored). Inviting for the owner role requires an owner caller and allow_owner=true.",
        invite,
    );
    registry.add_fn(
        "device.list",
        Effect::Admin,
        "List paired devices (from devices.toml, adopting pre-existing --pairing lines on first run) and pending pairing invites for principals the caller could act as, reconciling authorized_keys with devices.toml and sweeping any expired or used invites. For owner callers, also reports Legion lines in authorized_keys belonging to another or missing Legion home (marked with `foreign`, `missing_home`, and `home`).",
        list,
    );
    registry.add_fn(
        "device.set",
        Effect::Admin,
        "Change a paired device's role (only for a dedicated `device:*` principal marked with `device = \"<pairing_id>\"` in policy.toml; refuses shared or unmarked principals) or rename it in devices.toml, reconciling authorized_keys and pushing the change to reachable fleet peers. Changing a role requires the owner role (plus allow_owner=true for the owner role) and is refused through an expiring credential.",
        set_device,
    );
    registry.add_fn(
        "device.remove",
        Effect::Admin,
        "Remove a paired device by name or pairing id, or revoke a pending pairing invite by id, for a principal the caller could act as. Records a permanent tombstone in devices.toml, reconciles authorized_keys, and immediately ends live sessions and same-account SSH connections on this node. A removed device is still accepted by any other machine in the fleet that has not yet synced the removal; unreachable machines are named in `still_trusted_by`. Detached background jobs the device started keep running and are listed in `jobs_left_running` (stop them separately with `job.cancel` if needed). Pass `foreign=true` (owner role only) to remove a Legion line belonging to another or missing Legion home; for foreign devices this removes the key without ending sessions tracked under that other home and reports `sessions_not_ended: true`.",
        remove,
    );
    registry.add_fn(
        "device.endpoint",
        Effect::Read,
        "Return how a paired device reaches this node over SSH: node name, candidate host names or Tailscale addresses, SSH port, OS account name, and pinned sshd public host keys with their source. Uses the same detection as device.invite (honouring any [devices] settings in node.toml). Call with on=\"all\" through any node to learn every machine's direct SSH endpoint.",
        endpoint,
    );
}

/// Arguments of `device.invite`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InviteArgs {
    /// Existing principal in policy.toml the paired device will act as. Give either `principal`,
    /// or both `role` and `name`.
    #[serde(default)]
    principal: Option<String>,
    /// Role from policy.toml (or built-in `owner` with allow_owner=true, or `nobody`) for a new
    /// `device:<name>` principal created atomically in policy.toml. Requires `name` and an owner
    /// caller.
    #[serde(default)]
    role: Option<String>,
    /// Device name used to create principal `device:<name>` when pairing with `role`.
    #[serde(default)]
    name: Option<String>,
    /// Minutes until the invite expires, from 1 to 1440 (default 15).
    #[serde(default = "default_valid_minutes")]
    #[schemars(range(min = 1, max = 1440))]
    valid_minutes: u32,
    /// Host name or IP address (a single string or a list of strings) to put in the invite,
    /// overriding `[devices] hosts` in `node.toml` and automatic Tailscale/hostname detection.
    #[serde(default)]
    #[schemars(with = "Option<Vec<String>>")]
    host: Option<HostArg>,
    /// SSH port on this machine (defaults to `[devices] port` in `node.toml`, or 22).
    #[serde(default)]
    #[schemars(range(min = 1, max = 65535))]
    port: Option<u16>,
    /// OS account name on this machine (defaults to `[devices] user` in `node.toml` or the account
    /// legion runs as).
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

/// What `device.endpoint` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointResult {
    /// Node name as configured in `node.toml` or derived from the hostname.
    pub node: String,
    /// Candidate host names or Tailscale addresses to reach this node's sshd.
    pub hosts: Vec<String>,
    /// SSH port on this node.
    pub port: u16,
    /// OS account name on this node.
    pub user: String,
    /// Pinned SSH public host keys (`"<type> <base64>"`, without comments).
    pub host_keys: Vec<String>,
    /// Where the pinned host keys came from (`configured`, `ssh-keyscan+files`, `ssh-keyscan only`,
    /// or `files`).
    pub host_keys_source: String,
}

async fn endpoint(ctx: &Ctx, _args: NoArgs) -> Result<EndpointResult, ToolError> {
    let node_file = ctx.config().get::<NodeFile>()?;
    detect_endpoint(ctx, &node_file, None, None, None).await
}

async fn detect_endpoint(
    ctx: &Ctx,
    node_file: &NodeFile,
    host_override: Option<HostArg>,
    port_override: Option<u16>,
    user_override: Option<String>,
) -> Result<EndpointResult, ErrorBody> {
    let port = port_override
        .or(node_file.devices.port)
        .unwrap_or(DEFAULT_SSH_PORT);
    if port == 0 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "port must be between 1 and 65535.",
        ));
    }

    let user = match user_override.or_else(|| node_file.devices.user.clone()) {
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
                        "Detected SSH user name {detected:?} is not safe to place in an invite or endpoint."
                    ),
                )
                .with_hint("Set `[devices] user` in node.toml or pass a valid `user` explicitly."));
            }
            detected
        }
    };

    let host_arg = host_override.or_else(|| {
        (!node_file.devices.hosts.is_empty())
            .then(|| HostArg::Many(node_file.devices.hosts.clone()))
    });
    let hosts = resolve_hosts(host_arg, ctx.cancel_token().child_token()).await?;
    let resolved_host_keys = resolve_host_keys(
        &node_file.devices.host_keys,
        ctx.paths().home(),
        port,
        ctx.cancel_token().child_token(),
    )
    .await?;

    Ok(EndpointResult {
        node: ctx.node_name().to_owned(),
        hosts,
        port,
        user,
        host_keys: resolved_host_keys.keys,
        host_keys_source: resolved_host_keys.source,
    })
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

    let caller = Caller::of(ctx)?;
    let (principal, role, invite_role, invite_device_name, create_device_principal) = match (
        args.principal,
        args.role,
        args.name,
    ) {
        (Some(raw_principal), None, None) => {
            let principal = Principal::parse(&raw_principal)?;
            let principal = principal.as_str().to_owned();
            let role = caller.check_issue(
                &principal,
                args.allow_owner,
                "a device invite",
                format!(
                    "{principal} has the owner role: a paired device acting as it may do everything the account can over SSH, so it is not made unless asked for in so many words."
                ),
                "Give the device a principal with a narrower role in policy.toml, or pass allow_owner=true if this is really what you want.",
            )?;
            let recorded_role = None;
            (principal, role, recorded_role, None, false)
        }
        (None, Some(raw_role), Some(raw_name)) => {
            if !caller.is_owner() {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "Only a caller with the owner role may create a new device principal when pairing.",
                )
                .with_hint(
                    "Run `device.invite` as the owner, or pass `principal` for your own existing principal.",
                ));
            }
            validate_role_name(&raw_role)?;
            if raw_role == OWNER && !args.allow_owner {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "The owner role may do everything the account can over SSH, so a device is not paired with it unless asked for in so many words.",
                )
                .with_hint(
                    "Choose a narrower role from policy.toml, or pass allow_owner=true if this is really what you want.",
                ));
            }
            let trimmed_name = raw_name.trim();
            if trimmed_name.is_empty() || trimmed_name.chars().any(char::is_control) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{raw_name:?} is not a valid device name."),
                )
                .with_hint("Give a short name such as `pixel` or `tablet`."));
            }
            let policy = ctx.config().get::<Policy>()?;
            let known_role =
                raw_role == OWNER || raw_role == NOBODY || policy.roles.contains_key(&raw_role);
            if !known_role {
                let mut available: Vec<&str> = policy.roles.keys().map(String::as_str).collect();
                available.push(OWNER);
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("policy.toml does not define role {raw_role:?}."),
                )
                .with_hint(format!(
                    "Available roles: {}. Define custom roles in policy.toml or run `legion init`.",
                    available.join(", ")
                )));
            }
            let slug = sanitise_device_name(trimmed_name);
            let principal = format!("{DEVICE_PRINCIPAL_PREFIX}{slug}");
            Principal::parse(&principal)?;
            (
                principal,
                raw_role.clone(),
                Some(raw_role),
                Some(slug),
                true,
            )
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Pass either `principal`, or `role` and `name`, not both.",
            )
            .with_hint("Use `role` + `name` to create `device:<name>`, or `principal` to reuse an existing principal."));
        }
        (None, Some(_), None) | (None, None, Some(_)) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "`role` and `name` must be given together when pairing without `principal`.",
            )
            .with_hint("Pass both `name` and `role`, or pass an existing `principal`."));
        }
        (None, None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "device.invite requires either `principal`, or both `role` and `name`.",
            )
            .with_hint("No role or principal is built in; choose a role from policy.toml and a device name, or name an existing principal."));
        }
    };

    let node_file = ctx.config().get::<NodeFile>()?;
    let endpoint = detect_endpoint(ctx, &node_file, args.host, args.port, args.user).await?;
    let hosts = endpoint.hosts;
    let port = endpoint.port;
    let user = endpoint.user;
    let host_keys = endpoint.host_keys;
    let host_keys_source = endpoint.host_keys_source;
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
        port,
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
        role: invite_role,
        create_principal: create_device_principal,
        device_name: invite_device_name.clone(),
        created_by: Some(ctx.principal().to_owned()),
        created,
        expires: expires.clone(),
        fingerprint: ephemeral.fingerprint,
    };

    let paths = ctx.paths().clone();
    let id_for_write = id.clone();
    let principal_for_write = principal.clone();
    let node_name_for_write = ctx.node_name().to_owned();
    blocking(move || {
        let _cfg_guard = lock_config(&paths)?;
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

        let precheck = |auth_keys: &self::authkeys::AuthKeysFile,
                        state: &store::DevicesState|
         -> Result<(), ErrorBody> {
            if !create_device_principal {
                return Ok(());
            }
            let policy_path = paths.config_file(Policy::FILE);
            let live_policy = match std::fs::read(&policy_path) {
                Ok(bytes) => legion_core::config::parse::<Policy>(&policy_path, &bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Policy::default(),
                Err(error) => {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{} cannot be read: {error}.", policy_path.display()),
                    ));
                }
            };
            if live_policy
                .principals
                .keys()
                .any(|known| known.as_str() == principal_for_write)
            {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!("policy.toml already defines principal {principal_for_write}."),
                )
                .with_hint(
                    "Choose a different device name, or remove the existing device/principal first.",
                ));
            }
            if let Some(slug) = invite_device_name.as_deref()
                && let Some(existing_inv) = state.invites.values().find(|inv| {
                    inv.principal == principal_for_write
                        || inv.device_name.as_deref() == Some(slug)
                })
            {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!(
                        "A pending pairing invite ({}) already exists for device name {slug}.",
                        existing_inv.id
                    ),
                )
                .with_hint(format!(
                    "Use or revoke the existing invite with `legion device.remove id={}`, or choose a different device name.",
                    existing_inv.id
                )));
            }
            let devices_file = match load_devices_raw(&paths)? {
                Some(file) => file,
                None => adopt_legacy_devices_from_auth_keys(
                    &paths,
                    &node_name_for_write,
                    auth_keys,
                    &target,
                    caller_account.as_deref(),
                    state,
                    &live_policy,
                ),
            };
            if let Some(slug) = invite_device_name.as_deref()
                && devices_file
                    .devices
                    .values()
                    .any(|dev| dev.name == slug || dev.principal == principal_for_write)
            {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!("A paired device called {slug} already exists."),
                )
                .with_hint(
                    "Choose a different device name, or remove the existing device first with `legion device.remove`.",
                ));
            }
            Ok(())
        };

        commit_invite_and_state_checked(
            &paths,
            &target,
            &write_path,
            &id_for_write,
            &invite_line,
            record,
            now,
            precheck,
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
        port,
        user,
        host_keys,
        host_keys_source,
    })
}

/// Arguments of `device.set`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SetArgs {
    /// Paired device name to update, as `device.list` shows it. Give `name` or `id`, not both.
    #[serde(default)]
    name: Option<String>,
    /// Pairing id (32 lowercase hex characters) of the device to update. Give `name` or `id`, not
    /// both.
    #[serde(default)]
    id: Option<String>,
    /// New role from `policy.toml` (or built-in `owner` with `allow_owner=true`, or `nobody`).
    /// Updates the device's principal in `policy.toml` and `devices.toml`.
    #[serde(default)]
    role: Option<String>,
    /// New device name (sanitised to lowercase ASCII letters, digits, and `-`).
    #[serde(default)]
    rename: Option<String>,
    /// Allow setting the device's role to `owner`, which may do everything the account can. Off
    /// unless given as true.
    #[serde(default)]
    allow_owner: bool,
}

/// What `device.set` returns.
#[derive(Debug, Serialize)]
struct SetResult {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_name: Option<String>,
    principal: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_role: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    fleet: BTreeMap<String, FleetPushOutcome>,
}

async fn set_device(ctx: &Ctx, args: SetArgs) -> Result<SetResult, ToolError> {
    enum SelectBy {
        Name(String),
        Id(String),
    }

    let selector = match (args.name, args.id) {
        (Some(name), None) => {
            if name.trim().is_empty() || name.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{name:?} is not a valid device name."),
                )
                .with_hint("Use a device name from `device.list`."));
            }
            SelectBy::Name(name)
        }
        (None, Some(id)) => {
            validate_pairing_id(&id)?;
            SelectBy::Id(id)
        }
        (Some(_), Some(_)) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Give either `name` or `id`, not both.",
            ));
        }
        (None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Give `name` or `id` to choose which paired device to update.",
            )
            .with_hint("Run `device.list` to see paired devices."));
        }
    };

    if args.role.is_none() && args.rename.is_none() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "device.set requires at least one of `role` or `rename`.",
        )
        .with_hint("Pass `role=<role>` to change permissions, or `rename=<new-name>` to rename."));
    }

    let caller = Caller::of(ctx)?;
    if let Some(ref new_role) = args.role {
        validate_role_name(new_role)?;
        if !caller.is_owner() {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "Only a caller with the owner role may change a paired device's role.",
            )
            .with_hint("Run `device.set` as the owner of this machine."));
        }
        if let Some(limit) = ctx.credential_expires() {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "The credential this call came with expires at {}, so it may not change a paired device's role.",
                    format_rfc3339(limit)
                ),
            )
            .with_hint("Run `device.set` from a local shell or a non-expiring owner credential."));
        }
        if new_role == OWNER && !args.allow_owner {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "The owner role may do everything the account can over SSH, so a device's role is not raised to owner unless asked for in so many words.",
            )
            .with_hint("Pass allow_owner=true if this is really what you want."));
        }
    }

    let new_name = match args.rename {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("{raw:?} is not a valid device name."),
                )
                .with_hint("Use a short name such as `pixel` or `tablet`."));
            }
            Some(sanitise_device_name(trimmed))
        }
        None => None,
    };

    let node_file = ctx.config().get::<NodeFile>()?;
    let policy = ctx.config().get::<Policy>()?;
    let node_name = ctx.node_name().to_owned();
    let valid_node_name = NodeName::try_from(node_name)
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "node".to_owned());
    let paths = ctx.paths().clone();
    let new_role = args.role;

    let mut result = blocking(move || {
        let _cfg_guard = lock_config(&paths)?;
        let target = resolve_target(&paths, &node_file)?;
        let caller_account = caller_account_for_target(&target)?;
        let write_path = resolve_safe_write_path(&target, paths.home())?;
        verify_owner_matches_euid(
            &write_path,
            paths.home(),
            target.is_default_home_ssh,
            legion_os::user::effective_uid(),
        )?;
        let _auth_guard = lock_auth_keys(&target, &write_path)?;

        let mut devices_file = match load_devices_raw(&paths)? {
            Some(file) => file,
            None => {
                let auth_keys = self::authkeys::read_auth_keys(&write_path)?;
                let state = load_state(&paths).unwrap_or_default();
                adopt_legacy_devices_from_auth_keys(
                    &paths,
                    &valid_node_name,
                    &auth_keys,
                    &target,
                    caller_account.as_deref(),
                    &state,
                    &policy,
                )
            }
        };

        let pairing_id = match &selector {
            SelectBy::Id(id) => {
                let Some(entry) = devices_file.devices.get(id) else {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("There is no paired device with id {id}."),
                    )
                    .with_hint("Run `device.list` to see paired devices."));
                };
                if !caller.may_manage(&entry.principal) {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("There is no paired device with id {id}."),
                    ));
                }
                id.clone()
            }
            SelectBy::Name(name) => {
                let matches: Vec<String> = devices_file
                    .devices
                    .iter()
                    .filter(|(_, entry)| entry.name == *name && caller.may_manage(&entry.principal))
                    .map(|(id, _)| id.clone())
                    .collect();
                match matches.len() {
                    0 => {
                        return Err(ErrorBody::new(
                            ErrorCode::NotFound,
                            format!("There is no paired device called {name}."),
                        )
                        .with_hint("Run `device.list` to see paired devices."));
                    }
                    1 => matches[0].clone(),
                    _ => {
                        return Err(ErrorBody::new(
                            ErrorCode::Conflict,
                            format!(
                                "Multiple paired devices are named {name} (pairing ids: {}).",
                                matches.join(", ")
                            ),
                        )
                        .with_hint("Pass `id=<pairing_id>` from `device.list` to choose which one to update."));
                    }
                }
            }
        };

        let current_entry = devices_file
            .devices
            .get(&pairing_id)
            .cloned()
            .ok_or_else(|| ErrorBody::new(ErrorCode::Internal, "Matched device disappeared."))?;

        if let Some(ref renamed) = new_name
            && renamed != &current_entry.name
            && devices_file
                .devices
                .iter()
                .any(|(other_id, other)| other_id != &pairing_id && other.name == *renamed)
        {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!("Another paired device is already called {renamed}."),
            )
            .with_hint("Choose a different name."));
        }

        let previous_role_effective = policy.role_of(&current_entry.principal).0;
        let final_role = if let Some(ref target_role) = new_role {
            if current_entry.principal == OWNER {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "This device is pinned to the built-in `owner` principal, whose role cannot be changed.",
                )
                .with_hint("Pair the device with `legion pair --name ... --role ...` so it gets its own registry-owned `device:<name>` principal."));
            }
            if policy.device_owner(&current_entry.principal) != Some(pairing_id.as_str()) {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Principal {} is not owned exclusively by device {} ({pairing_id}), so `device.set role=` refuses to modify it.",
                        current_entry.principal, current_entry.name
                    ),
                )
                .with_hint(format!(
                    "Edit [principals.\"{}\"] in policy.toml directly to change the role for every credential using it, or pair this device with `legion pair --name ... --role ...` so it gets its own principal.",
                    current_entry.principal
                )));
            }
            let known_role = target_role == OWNER
                || target_role == NOBODY
                || policy.roles.contains_key(target_role);
            if !known_role {
                let mut available: Vec<&str> = policy.roles.keys().map(String::as_str).collect();
                available.push(OWNER);
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("policy.toml does not define role {target_role:?}."),
                )
                .with_hint(format!(
                    "Available roles: {}. Define custom roles in policy.toml first.",
                    available.join(", ")
                )));
            }
            upsert_device_principal_locked(&paths, &current_entry.principal, target_role, &pairing_id)?;
            target_role.clone()
        } else {
            previous_role_effective.clone()
        };

        let mut updated_entry = current_entry.clone();
        let previous_name = if let Some(ref renamed) = new_name
            && renamed != &current_entry.name
        {
            updated_entry.name = renamed.clone();
            Some(current_entry.name.clone())
        } else {
            None
        };
        let previous_role = if let Some(ref target_role) = new_role {
            if updated_entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
                updated_entry.role = Some(target_role.clone());
            }
            (target_role != &previous_role_effective).then_some(previous_role_effective)
        } else {
            None
        };

        updated_entry.updated_at = next_updated_timestamp(Some(&current_entry.updated_at))?;
        updated_entry.updated_on = valid_node_name;
        devices_file
            .devices
            .insert(pairing_id.clone(), updated_entry.clone());

        save_devices_locked(&paths, &devices_file)?;

        let exe = legion_core::current_exe()?;
        let shell = ForcedCommandShell::host()?;
        edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
            let (changed, _) = reconcile_authorized_keys_in_memory(
                &mut auth_keys,
                &devices_file,
                &paths,
                &target,
                caller_account.as_deref(),
                &exe,
                shell,
            )?;
            Ok((changed.then_some(auth_keys), ()))
        })?;

        // Keep local state/devices.json metadata in step on rename.
        if let Some(ref old_name) = previous_name
            && let Ok(mut state) = load_state(&paths)
            && let Some(mut rec) = state.devices.remove(old_name)
        {
            rec.name = updated_entry.name.clone();
            state.devices.insert(updated_entry.name.clone(), rec);
            let _ = save_state(&paths, &state);
        }

        Ok(SetResult {
            id: pairing_id,
            name: updated_entry.name,
            previous_name,
            principal: updated_entry.principal,
            role: final_role,
            previous_role,
            fleet: BTreeMap::new(),
        })
    })
    .await?;

    result.fleet = push_devices_to_fleet(ctx.node(), Some(ctx), None).await;
    Ok(result)
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
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    name: String,
    principal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    paired_on: Option<String>,
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
    let policy = ctx.config().get::<Policy>()?;
    let node_name = ctx.node_name().to_owned();
    let paths = ctx.paths().clone();
    let paths_for_sessions = paths.clone();

    let (raw_devices, raw_invites, removed_stale) = blocking(move || {
        let _cfg_guard = lock_config(&paths)?;
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

        let existing_devices = load_devices_raw(&paths)?;
        let exe = legion_core::current_exe()?;
        let shell = ForcedCommandShell::host()?;

        let (auth_keys, state, state_changed, devices_file, adopted_new, removed_stale) =
            edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                let mut state = load_state(&paths)?;
                let now = Timestamp::now();
                let (swept_auth, state_changed) = sweep(&mut auth_keys, &mut state, now);

                let (devices_file, adopted_new) = match &existing_devices {
                    Some(file) => (file.clone(), false),
                    None => {
                        let adopted = adopt_legacy_devices_from_auth_keys(
                            &paths,
                            &node_name,
                            &auth_keys,
                            &target,
                            caller_account.as_deref(),
                            &state,
                            &policy,
                        );
                        let has_any = !adopted.devices.is_empty();
                        (adopted, has_any)
                    }
                };

                let (reconciled_auth, removed_stale) = reconcile_authorized_keys_in_memory(
                    &mut auth_keys,
                    &devices_file,
                    &paths,
                    &target,
                    caller_account.as_deref(),
                    &exe,
                    shell,
                )?;
                let write_auth = (swept_auth || reconciled_auth).then(|| auth_keys.clone());
                Ok((
                    write_auth,
                    (
                        auth_keys,
                        state,
                        state_changed,
                        devices_file,
                        adopted_new,
                        removed_stale,
                    ),
                ))
            })?;
        if adopted_new {
            save_devices_locked(&paths, &devices_file)?;
        }
        if state_changed {
            save_state(&paths, &state)?;
        }
        sessions::sweep_stale_sessions(&paths);

        let mut own_devices: Vec<ListedDevice> = devices_file
            .devices
            .iter()
            .map(|(id, entry)| {
                let effective_role = policy.role_of(&entry.principal).0;
                ListedDevice {
                    id: Some(id.clone()),
                    name: entry.name.clone(),
                    principal: entry.principal.clone(),
                    role: Some(effective_role),
                    fingerprint: DevicesFile::fingerprint_of(entry),
                    added: Some(entry.paired_at.clone()),
                    paired_on: Some(entry.paired_on.clone()),
                    home: None,
                    foreign: false,
                    missing_home: false,
                }
            })
            .collect();
        own_devices.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));

        let mut devices = own_devices;
        let mut invites = Vec::new();
        for line in &auth_keys.lines {
            match &line.kind {
                LineKind::Device {
                    name,
                    key_b64,
                    principal,
                    pairing_id,
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
                    let is_this_home = line_matches_home(home.as_ref(), &paths)
                        || (home.is_none() && state.devices.contains_key(name));
                    if is_this_home {
                        continue;
                    }
                    if !caller_is_owner {
                        continue;
                    }
                    let missing_home = home.as_ref().is_some_and(|h| !h.config_exists());
                    let fingerprint =
                        fingerprint_key_b64(key_b64).unwrap_or_else(|| "unknown".to_owned());
                    devices.push(ListedDevice {
                        id: Some(pairing_id.clone()),
                        name: name.clone(),
                        principal: principal.clone(),
                        role: None,
                        fingerprint,
                        added: None,
                        paired_on: None,
                        home: home.as_ref().map(PinnedHome::display_label),
                        foreign: true,
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
        Ok((devices, invites, removed_stale))
    })
    .await?;

    for stale_name in removed_stale {
        let _ = sessions::terminate_device_sessions(
            &paths_for_sessions,
            &stale_name,
            sessions::DEFAULT_TERMINATE_GRACE,
        )
        .await;
    }

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
    /// Paired device pairing id or pending invite id to remove, as `device.list` shows it. Give
    /// `name` or `id`, not both.
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    fleet: BTreeMap<String, FleetPushOutcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    still_trusted_by: Vec<String>,
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
            RemoveTarget::DeviceName(name)
        }
        (None, Some(id)) => {
            validate_invite_id(&id)?;
            RemoveTarget::Id(id)
        }
        (Some(_), Some(_)) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Give either `name` (a paired device) or `id` (a pairing id or pending invite), not both.",
            ));
        }
        (None, None) => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Give `name` (a paired device) or `id` (a pairing id or pending invite) to remove.",
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
    let policy = ctx.config().get::<Policy>()?;
    let node_name = ctx.node_name().to_owned();
    let valid_node_name = NodeName::try_from(node_name.clone())
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "node".to_owned());
    let paths = ctx.paths().clone();
    let paths_for_sessions = paths.clone();

    let (mut removed, terminate_device, extra_stale) = blocking(move || {
        let _cfg_guard = lock_config(&paths)?;
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

        let existing_devices = load_devices_raw(&paths)?;
        let exe = legion_core::current_exe()?;
        let shell = ForcedCommandShell::host()?;

        let (outcome, state_to_save, devices_to_save, extra_stale) =
            edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                let mut state = load_state(&paths)?;
                let now = Timestamp::now();
                let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

                let (mut devices_file, adopted_new) = match &existing_devices {
                    Some(file) => (file.clone(), false),
                    None => {
                        let adopted = adopt_legacy_devices_from_auth_keys(
                            &paths,
                            &valid_node_name,
                            &auth_keys,
                            &target,
                            caller_account.as_deref(),
                            &state,
                            &policy,
                        );
                        let has_any = !adopted.devices.is_empty();
                        (adopted, has_any)
                    }
                };

                // Local paired-device removal by pairing id (when `id` matches a live device).
                let local_device_match_by_id = if !remove_foreign
                    && let RemoveTarget::Id(id) = &target_item
                    && let Some(entry) = devices_file.devices.get(id)
                    && caller.may_manage(&entry.principal)
                {
                    Some(id.clone())
                } else {
                    None
                };

                if let Some(pairing_id) = local_device_match_by_id {
                    let entry = devices_file.devices.remove(&pairing_id).ok_or_else(|| {
                        ErrorBody::new(ErrorCode::Internal, "Matched device disappeared.")
                    })?;
                    let removed_at = next_updated_timestamp(Some(&entry.updated_at))?;
                    devices_file.removed.insert(
                        pairing_id.clone(),
                        DeviceTombstone {
                            name: entry.name.clone(),
                            principal: entry.principal.clone(),
                            paired_at: Some(entry.paired_at),
                            paired_on: Some(entry.paired_on),
                            removed_at,
                            removed_on: valid_node_name.clone(),
                        },
                    );
                    let lines_before = auth_keys
                        .lines
                        .iter()
                        .filter(|line| {
                            matches!(
                                &line.kind,
                                LineKind::Device { pairing_id: pid, home, account, .. }
                                    if pid == &pairing_id
                                        && line_matches_home(home.as_ref(), &paths)
                                        && line_matches_account(
                                            account.as_deref(),
                                            caller_account.as_deref(),
                                            target.windows_admin_acl,
                                            cfg!(windows),
                                        )
                            )
                        })
                        .count();
                    let (reconciled_auth, stale_names) = reconcile_authorized_keys_in_memory(
                        &mut auth_keys,
                        &devices_file,
                        &paths,
                        &target,
                        caller_account.as_deref(),
                        &exe,
                        shell,
                    )?;
                    state.devices.remove(&entry.name);
                    let write_auth = (swept_auth || reconciled_auth).then_some(auth_keys);
                    return Ok((
                        write_auth,
                        (
                            Ok((
                                Removed {
                                    kind: "device",
                                    name: Some(entry.name.clone()),
                                    id: Some(pairing_id),
                                    principal: entry.principal,
                                    removed: true,
                                    lines_removed: lines_before,
                                    sessions_ended: 0,
                                    sessions_not_ended: false,
                                    jobs_left_running: Vec::new(),
                                    fleet: BTreeMap::new(),
                                    still_trusted_by: Vec::new(),
                                    hint: None,
                                },
                                Some(entry.name),
                            )),
                            Some(state),
                            Some(devices_file),
                            stale_names,
                        ),
                    ));
                }

                match &target_item {
                    RemoveTarget::DeviceName(name) if !remove_foreign => {
                        let visible_entries: Vec<(&String, &registry::FleetDevice)> = devices_file
                            .devices
                            .iter()
                            .filter(|(_, entry)| caller.may_manage(&entry.principal))
                            .collect();
                        let matched: Vec<String> = visible_entries
                            .iter()
                            .filter(|(_, entry)| entry.name == *name)
                            .map(|(id, _)| (*id).clone())
                            .collect();

                        if matched.len() > 1 {
                            let err = ErrorBody::new(
                                ErrorCode::Conflict,
                                format!(
                                    "Multiple paired devices are named {name} (pairing ids: {}).",
                                    matched.join(", ")
                                ),
                            )
                            .with_hint(
                                "Pass `id=<pairing_id>` from `device.list` to choose which one to remove.",
                            );
                            return Ok((
                                swept_auth.then_some(auth_keys),
                                (
                                    Err(err),
                                    swept_state.then_some(state),
                                    adopted_new.then_some(devices_file),
                                    Vec::new(),
                                ),
                            ));
                        }

                        if let Some(pairing_id) = matched.into_iter().next() {
                            let entry =
                                devices_file.devices.remove(&pairing_id).ok_or_else(|| {
                                    ErrorBody::new(
                                        ErrorCode::Internal,
                                        "Matched device disappeared.",
                                    )
                                })?;
                            let removed_at = next_updated_timestamp(Some(&entry.updated_at))?;
                            devices_file.removed.insert(
                                pairing_id.clone(),
                                DeviceTombstone {
                                    name: entry.name.clone(),
                                    principal: entry.principal.clone(),
                                    paired_at: Some(entry.paired_at),
                                    paired_on: Some(entry.paired_on),
                                    removed_at,
                                    removed_on: valid_node_name.clone(),
                                },
                            );
                            let lines_before = auth_keys
                                .lines
                                .iter()
                                .filter(|line| {
                                    matches!(
                                        &line.kind,
                                        LineKind::Device { pairing_id: pid, home, account, .. }
                                            if pid == &pairing_id
                                                && line_matches_home(home.as_ref(), &paths)
                                                && line_matches_account(
                                                    account.as_deref(),
                                                    caller_account.as_deref(),
                                                    target.windows_admin_acl,
                                                    cfg!(windows),
                                                )
                                    )
                                })
                                .count();
                            let (reconciled_auth, stale_names) =
                                reconcile_authorized_keys_in_memory(
                                    &mut auth_keys,
                                    &devices_file,
                                    &paths,
                                    &target,
                                    caller_account.as_deref(),
                                    &exe,
                                    shell,
                                )?;
                            state.devices.remove(&entry.name);
                            let write_auth = (swept_auth || reconciled_auth).then_some(auth_keys);
                            return Ok((
                                write_auth,
                                (
                                    Ok((
                                        Removed {
                                            kind: "device",
                                            name: Some(entry.name.clone()),
                                            id: Some(pairing_id),
                                            principal: entry.principal,
                                            removed: true,
                                            lines_removed: lines_before,
                                            sessions_ended: 0,
                                            sessions_not_ended: false,
                                            jobs_left_running: Vec::new(),
                                            fleet: BTreeMap::new(),
                                            still_trusted_by: Vec::new(),
                                            hint: None,
                                        },
                                        Some(entry.name),
                                    )),
                                    Some(state),
                                    Some(devices_file),
                                    stale_names,
                                ),
                            ));
                        }

                        // Not found locally: check whether a foreign home owns a device with this name.
                        let foreign_match_home = if caller.is_owner() {
                            auth_keys.lines.iter().find_map(|line| match &line.kind {
                                LineKind::Device {
                                    name: line_name,
                                    home,
                                    account,
                                    ..
                                } if line_name == name
                                    && line_matches_account(
                                        account.as_deref(),
                                        caller_account.as_deref(),
                                        target.windows_admin_acl,
                                        cfg!(windows),
                                    )
                                    && !line_matches_home(home.as_ref(), &paths) =>
                                {
                                    Some(
                                        home.as_ref()
                                            .map(PinnedHome::display_label)
                                            .unwrap_or_else(|| "another home".to_owned()),
                                    )
                                }
                                _ => None,
                            })
                        } else {
                            None
                        };
                        let hint = if let Some(other_home) = foreign_match_home {
                            format!(
                                "Device {name} belongs to another Legion home ({other_home}); pass `foreign=true` to remove it explicitly."
                            )
                        } else if visible_entries.is_empty() {
                            "There are no paired devices.".to_owned()
                        } else {
                            let mut known: Vec<&str> = visible_entries
                                .iter()
                                .map(|(_, e)| e.name.as_str())
                                .collect();
                            known.sort_unstable();
                            known.dedup();
                            format!("The paired devices are {}.", known.join(", "))
                        };
                        let err = ErrorBody::new(
                            ErrorCode::NotFound,
                            format!("There is no paired device called {name}."),
                        )
                        .with_hint(hint);
                        Ok((
                            swept_auth.then_some(auth_keys),
                            (
                                Err(err),
                                swept_state.then_some(state),
                                adopted_new.then_some(devices_file),
                                Vec::new(),
                            ),
                        ))
                    }
                    RemoveTarget::DeviceName(name) => {
                        // `foreign = true` (owner role only): operates directly on foreign lines in
                        // authorized_keys without touching this home's devices.toml.
                        let mut visible_names: BTreeMap<String, String> = BTreeMap::new();
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
                            }
                        }

                        let Some(principal) = matched_principal else {
                            let hint = if visible_names.is_empty() {
                                "There are no paired devices belonging to another or missing Legion home.".to_owned()
                            } else {
                                let known: Vec<&str> =
                                    visible_names.keys().map(String::as_str).collect();
                                format!("The foreign paired devices are {}.", known.join(", "))
                            };
                            let err = ErrorBody::new(
                                ErrorCode::NotFound,
                                format!(
                                    "There is no paired device called {name} belonging to another or missing Legion home."
                                ),
                            )
                            .with_hint(hint);
                            return Ok((
                                swept_auth.then_some(auth_keys),
                                (
                                    Err(err),
                                    swept_state.then_some(state),
                                    adopted_new.then_some(devices_file),
                                    Vec::new(),
                                ),
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
                                        && !(line_matches_home(home.as_ref(), &paths)
                                            || (home.is_none()
                                                && state.devices.contains_key(line_name)))
                            )
                        });
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
                                        sessions_not_ended: true,
                                        jobs_left_running: Vec::new(),
                                        fleet: BTreeMap::new(),
                                        still_trusted_by: Vec::new(),
                                        hint: Some(
                                            "Active sessions tracked under another Legion home were not ended; new SSH connections and channels are refused immediately, and if that other home still exists you can run `device.remove` from it or end any open sshd sessions manually.".to_owned(),
                                        ),
                                    },
                                    None,
                                )),
                                swept_state.then_some(state),
                                adopted_new.then_some(devices_file),
                                Vec::new(),
                            ),
                        ))
                    }
                    RemoveTarget::Id(id) => {
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
                                    format!("There is no pending invite or paired device with id {id}.")
                                },
                            )
                            .with_hint(hint);
                            return Ok((
                                swept_auth.then_some(auth_keys),
                                (
                                    Err(err),
                                    swept_state.then_some(state),
                                    adopted_new.then_some(devices_file),
                                    Vec::new(),
                                ),
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
                                        fleet: BTreeMap::new(),
                                        still_trusted_by: Vec::new(),
                                        hint: None,
                                    },
                                    None,
                                )),
                                if remove_foreign {
                                    swept_state.then_some(state)
                                } else {
                                    Some(state)
                                },
                                adopted_new.then_some(devices_file),
                                Vec::new(),
                            ),
                        ))
                    }
                }
            })?;
        match outcome {
            Ok((result, terminate)) => {
                if let Some(devices) = devices_to_save {
                    save_devices_locked(&paths, &devices)?;
                    let _ = reconcile_device_principals_locked(&paths, &devices)?;
                }
                if let Some(state) = state_to_save {
                    save_state(&paths, &state)?;
                }
                Ok((result, terminate, extra_stale))
            }
            Err(err) => {
                if let Some(devices) = devices_to_save {
                    let _ = save_devices_locked(&paths, &devices);
                }
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
        for stale in extra_stale {
            if stale != device_name {
                let _ = sessions::terminate_device_sessions(
                    &paths_for_sessions,
                    &stale,
                    sessions::DEFAULT_TERMINATE_GRACE,
                )
                .await;
            }
        }
        let paths_for_jobs = paths_for_sessions.clone();
        let principal_for_jobs = removed.principal.clone();
        let device_for_jobs = device_name.clone();
        removed.jobs_left_running = blocking(move || {
            Ok(crate::job::running_job_ids_for_device(
                &paths_for_jobs,
                &device_for_jobs,
                &principal_for_jobs,
            ))
        })
        .await
        .unwrap_or_default();

        let fleet_outcomes = push_devices_to_fleet(ctx.node(), Some(ctx), None).await;
        let still_trusted: Vec<String> = fleet_outcomes
            .iter()
            .filter(|(_, outcome)| outcome.status != "synced")
            .map(|(machine, _)| machine.clone())
            .collect();
        if !still_trusted.is_empty() {
            removed.hint = Some(format!(
                "These machines could not be reached and still trust {device_name} until their next sync: {}. Run `legion fleet.sync direction=push documents='[\"devices.toml\"]'` when they are back online.",
                still_trusted.join(", ")
            ));
        }
        removed.fleet = fleet_outcomes;
        removed.still_trusted_by = still_trusted;
    }

    Ok(removed)
}

#[cfg(test)]
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
    commit_invite_and_state_checked(
        paths,
        target,
        write_path,
        invite_id,
        invite_line,
        record,
        now,
        |_, _| Ok(()),
        save_fn,
    )
}

#[allow(clippy::too_many_arguments)]
fn commit_invite_and_state_checked(
    paths: &legion_core::Paths,
    target: &AuthKeysTarget,
    write_path: &std::path::Path,
    invite_id: &str,
    invite_line: &str,
    record: InviteRecord,
    now: Timestamp,
    precheck: impl Fn(&self::authkeys::AuthKeysFile, &store::DevicesState) -> Result<(), ErrorBody>,
    save_fn: impl FnOnce(&legion_core::Paths, &store::DevicesState) -> Result<(), ErrorBody>,
) -> Result<(), ErrorBody> {
    let mut state = edit_auth_keys_with_retry(target, write_path, |mut auth_keys| {
        let mut state = load_state(paths)?;
        sweep(&mut auth_keys, &mut state, now);
        precheck(&auth_keys, &state)?;
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
    DeviceName(String),
    Id(String),
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
                fleet: BTreeMap::new(),
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
                fleet: BTreeMap::new(),
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
                fleet: BTreeMap::new(),
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
            let shell = ForcedCommandShell::host().unwrap();
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
                role: None,
                create_principal: false,
                device_name: None,
                created_by: None,
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

    #[tokio::test]
    async fn device_endpoint_is_readable_by_observer_and_matches_invite_detection() {
        let h = Harness::new();
        let auth_escaped = h.auth_keys.display().to_string().replace('\\', "\\\\");
        let hk_path = h.dir.path().join("ssh_host_ed25519_key.pub");
        let hk_escaped = hk_path.display().to_string().replace('\\', "\\\\");
        std::fs::write(
            h.node.paths().config_file("node.toml"),
            format!(
                "name = \"tower\"\n\
                 [devices]\n\
                 authorized_keys = \"{auth_escaped}\"\n\
                 host_keys = [\"{hk_escaped}\"]\n\
                 hosts = [\"tower.tailnet.example\", \"100.64.0.10\"]\n\
                 port = 2222\n\
                 user = \"me\"\n"
            ),
        )
        .unwrap();

        // Observer (`phone`) has only effect:read and can call device.endpoint.
        let ep = h
            .call_as("device.endpoint", "phone", json!({}))
            .await
            .unwrap();
        assert_eq!(ep["node"], json!("tower"));
        assert_eq!(ep["hosts"], json!(["tower.tailnet.example", "100.64.0.10"]));
        assert_eq!(ep["port"], json!(2222));
        assert_eq!(ep["user"], json!("me"));
        assert_eq!(ep["host_keys"], json!([h.host_key]));
        assert_eq!(ep["host_keys_source"], json!("configured"));

        // device.invite without explicit host/port/user uses the exact same endpoint settings.
        let inv = h
            .call("device.invite", json!({"principal": "phone"}))
            .await
            .unwrap();
        assert_eq!(inv["hosts"], ep["hosts"]);
        assert_eq!(inv["port"], ep["port"]);
        assert_eq!(inv["user"], ep["user"]);
        assert_eq!(inv["host_keys"], ep["host_keys"]);
        assert_eq!(inv["host_keys_source"], ep["host_keys_source"]);
    }

    #[tokio::test]
    async fn role_pairing_creates_device_principal_and_device_set_updates_role_and_name() {
        let h = Harness::new();

        // Missing principal and missing role/name fails clearly (nothing preset).
        let err = h.call("device.invite", json!({})).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        // Non-owner admin cannot create a new `device:*` principal via role+name.
        let err = h
            .call_as(
                "device.invite",
                "tablet",
                json!({"role": "observer", "name": "mini", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // Role `owner` requires `allow_owner=true`.
        let err = h
            .call(
                "device.invite",
                json!({"role": "owner", "name": "boss", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // Role + name reserves `device:my-phone` without touching policy.toml until enrolment.
        let inv = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "My Phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        assert_eq!(inv["principal"], json!("device:my-phone"));
        assert_eq!(inv["role"], json!("observer"));
        let policy_before_enrol = h.node.config().get::<Policy>().unwrap();
        assert!(
            !policy_before_enrol
                .principals
                .keys()
                .any(|p| p.as_str() == "device:my-phone")
        );

        let id = inv["id"].as_str().unwrap();
        let dev_key = generate_ephemeral_ed25519().unwrap();
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"{}\",\"name\":\"ignored\"}}\n",
            dev_key.public_key
        );
        let reply = run_enrol(&h.node, id, req.as_bytes()).await;
        assert_eq!(
            reply,
            EnrolReply::Ok {
                ok: true,
                node: "tower".to_owned(),
                principal: "device:my-phone".to_owned(),
                role: "observer".to_owned(),
                device: "my-phone".to_owned(),
                fleet: BTreeMap::new(),
            }
        );
        let policy_after = h.node.config().get::<Policy>().unwrap();
        assert_eq!(policy_after.role_of("device:my-phone").0, "observer");

        // Non-owner admin cannot change a device's role via `device.set`.
        let err = h
            .call_as(
                "device.set",
                "tablet",
                json!({"name": "my-phone", "role": "phone_admin"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // Raising role to `owner` via `device.set` requires `allow_owner=true`.
        let err = h
            .call("device.set", json!({"name": "my-phone", "role": "owner"}))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // Owner changes role to `phone_admin` and renames to `pocket-phone`.
        let updated = h
            .call(
                "device.set",
                json!({"name": "my-phone", "role": "phone_admin", "rename": "Pocket Phone"}),
            )
            .await
            .unwrap();
        assert_eq!(updated["name"], json!("pocket-phone"));
        assert_eq!(updated["previous_name"], json!("my-phone"));
        assert_eq!(updated["principal"], json!("device:my-phone"));
        assert_eq!(updated["role"], json!("phone_admin"));
        assert_eq!(updated["previous_role"], json!("observer"));

        let policy_updated = h.node.config().get::<Policy>().unwrap();
        assert_eq!(policy_updated.role_of("device:my-phone").0, "phone_admin");
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(auth_text.contains("--device pocket-phone"), "{auth_text}");
        assert!(
            auth_text.contains("legion-device:pocket-phone"),
            "{auth_text}"
        );
        assert!(!auth_text.contains("legion-device:my-phone"), "{auth_text}");

        // Removing the device cleans up its unused `device:my-phone` principal from policy.toml.
        let removed = h
            .call("device.remove", json!({"name": "pocket-phone"}))
            .await
            .unwrap();
        assert_eq!(removed["removed"], json!(true));
        let policy_cleaned = h.node.config().get::<Policy>().unwrap();
        assert!(
            !policy_cleaned
                .principals
                .keys()
                .any(|p| p.as_str() == "device:my-phone")
        );
    }

    #[tokio::test]
    async fn role_invite_does_not_write_policy_refuses_duplicate_pending_and_checks_policy_at_enrol()
    {
        let h = Harness::new();

        // 1. Invite `pixel` as `observer` — policy.toml must NOT be touched at invite time.
        let inv1 = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "pixel", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv1_id = inv1["id"].as_str().unwrap().to_owned();
        let policy_before = h.node.config().get::<Policy>().unwrap();
        assert!(
            !policy_before
                .principals
                .keys()
                .any(|p| p.as_str() == "device:pixel"),
            "invite must not create device:pixel in policy.toml before enrolment"
        );

        // 2. Second invite for the same name (`pixel`) as `owner` must be refused.
        let dup_err = h
            .call(
                "device.invite",
                json!({
                    "role": "owner",
                    "name": "pixel",
                    "allow_owner": true,
                    "host": "tower.example",
                    "user": "me"
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(dup_err.code, ErrorCode::Conflict, "{dup_err:?}");

        // 3. Enrolling the first invite creates `device:pixel` with `observer` (never `owner`).
        let key1 = generate_ephemeral_ed25519().unwrap();
        let req1 = format!(
            "{{\"enrol\":1,\"id\":\"{inv1_id}\",\"public_key\":\"{}\",\"name\":\"ignored\"}}\n",
            key1.public_key
        );
        let reply1 = run_enrol(&h.node, &inv1_id, req1.as_bytes()).await;
        assert_eq!(
            reply1,
            EnrolReply::Ok {
                ok: true,
                node: "tower".to_owned(),
                principal: "device:pixel".to_owned(),
                role: "observer".to_owned(),
                device: "pixel".to_owned(),
                fleet: BTreeMap::new(),
            }
        );
        assert_eq!(
            h.node.config().get::<Policy>().unwrap().role_of("device:pixel").0,
            "observer"
        );

        // 4. An unused/revoked invite leaves nothing behind in policy.toml.
        let inv_unused = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "unused", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let unused_id = inv_unused["id"].as_str().unwrap();
        h.call("device.remove", json!({"id": unused_id}))
            .await
            .unwrap();
        assert!(
            !h.node
                .config()
                .get::<Policy>()
                .unwrap()
                .principals
                .keys()
                .any(|p| p.as_str() == "device:unused"),
            "unused invite must leave nothing behind in policy.toml"
        );

        // 5. Invite for a name whose `device:*` principal already exists in policy.toml is refused.
        let policy_path = h.node.paths().config_file("policy.toml");
        let mut policy_text = std::fs::read_to_string(&policy_path).unwrap();
        policy_text.push_str("\n[principals.\"device:taken\"]\nrole = \"observer\"\n");
        std::fs::write(&policy_path, &policy_text).unwrap();
        let taken_err = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "taken", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(taken_err.code, ErrorCode::Conflict, "{taken_err:?}");

        // 6. If policy.toml disagrees at enrolment time, enrolment refuses and leaves policy.toml untouched.
        let inv_race = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "raced", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let race_id = inv_race["id"].as_str().unwrap();
        let mut policy_text = std::fs::read_to_string(&policy_path).unwrap();
        policy_text.push_str("\n[principals.\"device:raced\"]\nrole = \"owner\"\n");
        std::fs::write(&policy_path, &policy_text).unwrap();

        let key_race = generate_ephemeral_ed25519().unwrap();
        let req_race = format!(
            "{{\"enrol\":1,\"id\":\"{race_id}\",\"public_key\":\"{}\",\"name\":\"ignored\"}}\n",
            key_race.public_key
        );
        let reply_race = run_enrol(&h.node, race_id, req_race.as_bytes()).await;
        assert!(
            !reply_race.is_ok(),
            "enrol must fail when policy.toml already disagrees: {reply_race:?}"
        );
        assert_eq!(
            h.node.config().get::<Policy>().unwrap().role_of("device:raced").0,
            "owner"
        );
    }

    #[tokio::test]
    async fn device_set_role_refuses_shared_principal_and_credentials_refuse_registry_owned_principal()
    {
        let h = Harness::new();

        // 1. Pair a device under shared principal `tablet` via `--as` (`principal`).
        let shared_inv = h
            .call(
                "device.invite",
                json!({"principal": "tablet", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let shared_id = shared_inv["id"].as_str().unwrap();
        let shared_key = generate_ephemeral_ed25519().unwrap();
        let shared_req = format!(
            "{{\"enrol\":1,\"id\":\"{shared_id}\",\"public_key\":\"{}\",\"name\":\"shared-tab\"}}\n",
            shared_key.public_key
        );
        assert!(run_enrol(&h.node, shared_id, shared_req.as_bytes()).await.is_ok());

        // `device.set role=` on a shared principal must be refused with a policy.toml hint.
        let shared_err = h
            .call(
                "device.set",
                json!({"name": "shared-tab", "role": "observer"}),
            )
            .await
            .unwrap_err();
        assert_eq!(shared_err.code, ErrorCode::Denied, "{shared_err:?}");
        assert!(
            shared_err
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("policy.toml")),
            "{shared_err:?}"
        );
        assert_eq!(
            h.node.config().get::<Policy>().unwrap().role_of("tablet").0,
            "phone_admin"
        );

        // 2. Pair a device with `role` + `name`, creating a registry-owned `device:owned-pad` principal.
        let owned_inv = h
            .call(
                "device.invite",
                json!({"name": "owned-pad", "role": "observer", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let owned_inv_id = owned_inv["id"].as_str().unwrap();
        let owned_key = generate_ephemeral_ed25519().unwrap();
        let owned_req = format!(
            "{{\"enrol\":1,\"id\":\"{owned_inv_id}\",\"public_key\":\"{}\",\"name\":\"ignored\"}}\n",
            owned_key.public_key
        );
        assert!(run_enrol(&h.node, owned_inv_id, owned_req.as_bytes()).await.is_ok());

        let policy_text =
            std::fs::read_to_string(h.node.paths().config_file("policy.toml")).unwrap();
        assert!(
            policy_text.contains("device = "),
            "registry-owned principal must carry a device ownership marker in policy.toml:\n{policy_text}"
        );

        // 3. Tokens, agents, and `--as` pairings on a registry-owned principal must be refused.
        let tok_err = h
            .call(
                "token.add",
                json!({"name": "badtok", "principal": "device:owned-pad"}),
            )
            .await
            .unwrap_err();
        assert_eq!(tok_err.code, ErrorCode::Denied, "{tok_err:?}");

        let agent_err = h
            .call(
                "agent.add",
                json!({"agent": "claude", "as": "device:owned-pad", "dry_run": true}),
            )
            .await
            .unwrap_err();
        assert_eq!(agent_err.code, ErrorCode::Denied, "{agent_err:?}");

        let pair_as_err = h
            .call(
                "device.invite",
                json!({"principal": "device:owned-pad", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(pair_as_err.code, ErrorCode::Denied, "{pair_as_err:?}");

        // 4. Unmarked `device:legacy` in policy.toml triggers a warning in `meta.doctor`.
        let mut text = std::fs::read_to_string(h.node.paths().config_file("policy.toml")).unwrap();
        text.push_str("\n[principals.\"device:legacy\"]\nrole = \"observer\"\n");
        std::fs::write(h.node.paths().config_file("policy.toml"), &text).unwrap();
        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        let checks = doctor["checks"].as_array().unwrap();
        assert!(
            checks.iter().any(|c| c["warning"] == json!(true)
                && c["detail"]
                    .as_str()
                    .is_some_and(|d| d.contains("device:legacy"))),
            "expected meta.doctor warning for unmarked device:legacy: {doctor}"
        );
    }
}
