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
    host_key_fingerprints, resolve_host_keys, resolve_host_keys_cached, validate_invite_id,
    validate_pairing_id,
};
use self::registry::{
    DEVICE_PRINCIPAL_PREFIX, DeviceTombstone, DevicesFile, FleetPushOutcome,
    adopt_legacy_devices_from_auth_keys, load_devices_raw, lock_config, next_updated_timestamp,
    push_devices_to_fleet, reconcile_authorized_keys_in_memory, reconcile_device_principals_locked,
    repair_incomplete_enrolments_locked, repair_unconfirmed_state_in_memory, save_devices_locked,
    upsert_device_principal_locked, validate_role_name,
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
/// `device.endpoint`, `device.roles`) and registers `devices.toml`.
pub fn register(registry: &mut Registry) {
    registry.add_document::<registry::DevicesFile>();
    registry.add_fn(
        "device.invite",
        Effect::Admin,
        "Create a single-use pairing invite for a remote device to connect over SSH. Either give `role` and `name` (owner role only: reserves a unique `device:<name>` principal and creates it in policy.toml when the device enrols, leaving nothing in policy.toml if unused or expired) or `principal` (an existing non-device-owned principal that policy.toml names and the caller could act as itself). Appends a temporary key with sshd's expiry-time and a forced enrolment command to authorized_keys, and returns the invite text once (never logged or stored). Inviting for the owner role or any admin (owner-strength) role requires an owner caller and allow_owner=true. A paired device may create invites only on the node it is connected to directly (not forwarded via `on=`).",
        invite,
    );
    registry.add_fn(
        "device.list",
        Effect::Admin,
        "List paired devices (from devices.toml, adopting state-matched --pairing lines on first run and listing any unmatched local-only lines) and pending pairing invites for principals the caller could act as, reconciling authorized_keys with devices.toml and sweeping any expired or used invites. Devices enrolled on this machine that have not yet opened their first authenticated session are marked locally with optional `unconfirmed: true`. For owner callers, also reports Legion lines in authorized_keys belonging to another or missing Legion home (marked with `foreign`, `missing_home`, and `home`).",
        list,
    );
    registry.add_fn(
        "device.set",
        Effect::Admin,
        "Change a paired device's role (only for a dedicated `device:*` principal marked with `device = \"<pairing_id>\"` in policy.toml; refuses shared or unmarked principals), rename it, or confirm a currently quarantined descendant (`confirm=true`, owner role only; confirmation records `confirmed_at` locally on this node and is refused if the device is not quarantined) in devices.toml, reconciling authorized_keys and pushing role or name changes to reachable fleet peers. Changing a role or confirming requires the owner role (plus allow_owner=true for `owner` or any admin owner-strength role) and every `device.set` call is refused through an expiring credential.",
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
        "Return how a paired device reaches this node over SSH: node name, candidate host names or Tailscale addresses, SSH port, OS account name, and pinned sshd public host keys. Uses the same detection as device.invite (honouring any [devices] settings in node.toml, caching auto-discovered host-key scans briefly per node). Call with on=\"all\" through any node to learn every machine's direct SSH endpoint.",
        endpoint,
    );
    registry.add_fn(
        "device.roles",
        Effect::Read,
        "Return the roles this caller may assign to a newly paired device on this node: each role's name, one-line plain description (generated with the same analysis as `legion pair`), strongest allowed effect, and whether it is owner-strength (`owner` or any role allowing an `admin` tool). Lists only roles the caller is allowed to hand out (empty for callers without the `owner` role) and exposes nothing else from policy.toml.",
        roles,
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
    /// Role from policy.toml (or built-in `owner`, or `nobody`) for a new `device:<name>`
    /// principal created atomically in policy.toml. Requires `name` and an owner caller (plus
    /// `allow_owner=true` for `owner` or any admin owner-strength role).
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
    /// Allow inviting a device with the `owner` role or any admin (owner-strength) role, which can
    /// change who may do what. Off unless given as true.
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
}

struct DetectedEndpoint {
    endpoint: EndpointResult,
    host_keys_source: String,
}

async fn endpoint(ctx: &Ctx, _args: NoArgs) -> Result<EndpointResult, ToolError> {
    let node_file = ctx.config().get::<NodeFile>()?;
    let detected = detect_endpoint(ctx, &node_file, None, None, None, true).await?;
    Ok(detected.endpoint)
}

/// What `device.roles` returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRolesResult {
    /// Node name as configured in `node.toml` or derived from the hostname.
    pub node: String,
    /// Roles this caller is allowed to hand out to a newly paired device, ordered from narrowest
    /// to broadest. Empty when the caller may not hand out roles for new devices.
    pub roles: Vec<crate::roles::RoleSummary>,
}

async fn roles(ctx: &Ctx, _args: NoArgs) -> Result<DeviceRolesResult, ToolError> {
    let caller = Caller::of(ctx)?;
    let roles = if caller.is_owner() {
        let policy = ctx.config().get::<Policy>()?;
        let specs = ctx.node().specs();
        crate::roles::role_summaries(&policy, &specs)
    } else {
        Vec::new()
    };
    Ok(DeviceRolesResult {
        node: ctx.node_name().to_owned(),
        roles,
    })
}

async fn detect_endpoint(
    ctx: &Ctx,
    node_file: &NodeFile,
    host_override: Option<HostArg>,
    port_override: Option<u16>,
    user_override: Option<String>,
    use_host_key_cache: bool,
) -> Result<DetectedEndpoint, ErrorBody> {
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
    let resolved_host_keys = if use_host_key_cache {
        resolve_host_keys_cached(
            ctx.paths().config_dir(),
            &node_file.devices.host_keys,
            ctx.paths().home(),
            port,
            ctx.cancel_token().child_token(),
        )
        .await?
    } else {
        resolve_host_keys(
            &node_file.devices.host_keys,
            ctx.paths().home(),
            port,
            ctx.cancel_token().child_token(),
        )
        .await?
    };

    Ok(DetectedEndpoint {
        endpoint: EndpointResult {
            node: ctx.node_name().to_owned(),
            hosts,
            port,
            user,
            host_keys: resolved_host_keys.keys,
        },
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
    /// Other machines in `fleet.toml` that this node will push the device to after enrolment.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fleet_peers: Vec<String>,
}

async fn invite(ctx: &Ctx, args: InviteArgs) -> Result<InviteResult, ToolError> {
    // A paired device may create invites only on the node it is connected to directly, so
    // `paired_by.device` is always known from the session context.
    if ctx.hops() > 0 {
        let policy = ctx.config().get::<Policy>()?;
        let is_paired_device = ctx.pairing_id().is_some()
            || ctx.device().is_some()
            || policy.device_owner(ctx.principal()).is_some()
            || ctx.principal().starts_with(DEVICE_PRINCIPAL_PREFIX);
        if is_paired_device {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "A paired device may create pairing invites only on the machine it is connected to directly, not forwarded to {}.",
                    ctx.node_name()
                ),
            )
            .with_hint(format!(
                "Connect directly to {} over SSH and run `device.invite` there.",
                ctx.node_name()
            )));
        }
    }

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
                    "{principal} has the owner role ({}): a paired device acting as it may do everything the account can over SSH, so it is not made unless asked for in so many words.",
                    crate::roles::OWNER_STRENGTH_REASON
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
            if caller.is_owner_strength_role(&raw_role) && !args.allow_owner {
                let reason = crate::roles::OWNER_STRENGTH_REASON;
                let message = if raw_role == OWNER {
                    format!(
                        "The owner role may do everything the account can over SSH ({reason}), so a device is not paired with it unless asked for in so many words."
                    )
                } else {
                    format!(
                        "Role {raw_role} cannot be granted without allow_owner=true: {reason}, so a device is not paired with it unless asked for in so many words."
                    )
                };
                return Err(ErrorBody::new(ErrorCode::Denied, message).with_hint(
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
    let detected = detect_endpoint(ctx, &node_file, args.host, args.port, args.user, false).await?;
    let hosts = detected.endpoint.hosts;
    let port = detected.endpoint.port;
    let user = detected.endpoint.user;
    let host_keys = detected.endpoint.host_keys;
    let host_keys_source = detected.host_keys_source;
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

    let creator_policy = ctx.config().get::<Policy>()?;
    let created_by_device = ctx.pairing_id().map(str::to_owned).or_else(|| {
        creator_policy
            .device_owner(ctx.principal())
            .map(str::to_owned)
    });
    let base_record = InviteRecord {
        id: id.clone(),
        principal: principal.clone(),
        role: invite_role,
        create_principal: create_device_principal,
        device_name: invite_device_name.clone(),
        created_by: Some(ctx.principal().to_owned()),
        created_by_device,
        created_by_device_name: ctx.device().map(str::to_owned),
        created,
        expires: expires.clone(),
        fingerprint: ephemeral.fingerprint,
        host_key_fingerprints: host_key_fingerprints(&host_keys),
    };

    let paths = ctx.paths().clone();
    let id_for_write = id.clone();
    let principal_for_write = principal.clone();
    let node_name_for_write = ctx.node_name().to_owned();
    let caller_is_owner = caller.is_owner();
    let caller_principal_for_cap = ctx.principal().to_owned();
    let final_principal = blocking(move || {
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
        repair_incomplete_enrolments_locked(
            &paths,
            &target,
            &write_path,
            caller_account.as_deref(),
        )?;

        let resolve_record = |auth_keys: &self::authkeys::AuthKeysFile,
                              state: &store::DevicesState|
         -> Result<InviteRecord, ErrorBody> {
            if !caller_is_owner {
                let pending = state
                    .invites
                    .values()
                    .filter(|inv| {
                        inv.created_by.as_deref() == Some(caller_principal_for_cap.as_str())
                            || inv.principal == caller_principal_for_cap
                    })
                    .count();
                if pending >= registry::MAX_NON_OWNER_PENDING_INVITES {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Principal {caller_principal_for_cap} already has {pending} pending pairing invites (the limit for non-owner callers is {}).",
                            registry::MAX_NON_OWNER_PENDING_INVITES
                        ),
                    )
                    .with_hint(
                        "Wait for existing invites to expire or remove unused ones with `legion device.remove id=<invite_id>` before creating another.",
                    ));
                }
            }
            let mut record = base_record.clone();
            if !create_device_principal {
                return Ok(record);
            }
            let policy_path = paths.config_file(Policy::FILE);
            let live_policy = match legion_core::config::read_regular_file(&policy_path)? {
                Some(bytes) => legion_core::config::parse::<Policy>(&policy_path, &bytes)?,
                None => Policy::default(),
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
            // A registry-owned `device:*` principal belongs to at most one pairing id across the
            // lifetime of the fleet. If a removed tombstone already used `device:<slug>`, allocate
            // the first unused numbered principal (`device:<slug>-2`, ...) so the replacement device
            // keeps its name without reusing a tombstoned principal.
            if let Some(slug) = invite_device_name.as_deref()
                && devices_file
                    .removed
                    .values()
                    .any(|tomb| tomb.principal == principal_for_write)
            {
                let mut suffix: u32 = 2;
                let chosen = loop {
                    let candidate = format!("{DEVICE_PRINCIPAL_PREFIX}{slug}-{suffix}");
                    let taken = live_policy
                        .principals
                        .keys()
                        .any(|known| known.as_str() == candidate)
                        || state.invites.values().any(|inv| inv.principal == candidate)
                        || devices_file
                            .devices
                            .values()
                            .any(|dev| dev.principal == candidate)
                        || devices_file
                            .removed
                            .values()
                            .any(|tomb| tomb.principal == candidate);
                    if !taken && Principal::parse(&candidate).is_ok() {
                        break candidate;
                    }
                    suffix = suffix.checked_add(1).ok_or_else(|| {
                        ErrorBody::new(
                            ErrorCode::Conflict,
                            format!(
                                "No unused `device:*` principal is available for device name {slug}."
                            ),
                        )
                    })?;
                };
                record.principal = chosen;
            }
            Ok(record)
        };

        let saved_record = commit_invite_and_state_checked(
            &paths,
            &target,
            &write_path,
            &id_for_write,
            &invite_line,
            now,
            resolve_record,
            save_state,
        )?;
        Ok(saved_record.principal)
    })
    .await?;

    let payload = InvitePayload {
        v: keys::INVITE_VERSION,
        id: id.clone(),
        node: ctx.node_name().to_owned(),
        hosts: hosts.clone(),
        port,
        user: user.clone(),
        host_keys: host_keys.clone(),
        key: ephemeral.seed_b64,
        principal: final_principal.clone(),
        expires: expires.clone(),
    };
    let invite_text = payload.encode()?;

    let fleet_peers = registry::fleet_peer_names(ctx.paths(), ctx.node_name());
    Ok(InviteResult {
        invite: invite_text,
        id,
        node: ctx.node_name().to_owned(),
        principal: final_principal,
        role,
        expires,
        hosts,
        port,
        user,
        host_keys,
        host_keys_source,
        fleet_peers,
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
    /// New role from `policy.toml` (or built-in `owner`, or `nobody`). Updates the device's
    /// principal in `policy.toml` and `devices.toml`. Setting `owner` or any admin
    /// (owner-strength) role requires `allow_owner=true`.
    #[serde(default)]
    role: Option<String>,
    /// New device name (sanitised to lowercase ASCII letters, digits, and `-`).
    #[serde(default)]
    rename: Option<String>,
    /// Allow setting the device's role to `owner` or any admin (owner-strength) role, which can
    /// change who may do what. Off unless given as true.
    #[serde(default)]
    allow_owner: bool,
    /// Explicitly confirm a quarantined paired device so it becomes active again (owner role
    /// only). Off unless given as true.
    #[serde(default)]
    confirm: bool,
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
    #[serde(skip_serializing_if = "is_false")]
    confirmed: bool,
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

    if args.role.is_none() && args.rename.is_none() && !args.confirm {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "device.set requires at least one of `role`, `rename`, or `confirm=true`.",
        )
        .with_hint("Pass `role=<role>` to change permissions, `rename=<new-name>` to rename, or `confirm=true` to confirm a quarantined device."));
    }

    let caller = Caller::of(ctx)?;
    if let Some(limit) = ctx.credential_expires() {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "The credential this call came with expires at {}, so it may not modify live paired devices.",
                format_rfc3339(limit)
            ),
        )
        .with_hint("Run `device.set` from a local shell or a non-expiring credential."));
    }
    if args.confirm && !caller.is_owner() {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "Only a caller with the owner role may confirm a quarantined paired device.",
        )
        .with_hint("Run `device.set confirm=true` as the owner of this machine."));
    }
    if let Some(ref new_role) = args.role {
        validate_role_name(new_role)?;
        if !caller.is_owner() {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                "Only a caller with the owner role may change a paired device's role.",
            )
            .with_hint("Run `device.set` as the owner of this machine."));
        }
        if caller.is_owner_strength_role(new_role) && !args.allow_owner {
            let reason = crate::roles::OWNER_STRENGTH_REASON;
            let message = if new_role == OWNER {
                format!(
                    "The owner role may do everything the account can over SSH ({reason}), so a device's role is not raised to owner unless asked for in so many words."
                )
            } else {
                format!(
                    "Role {new_role} cannot be granted without allow_owner=true: {reason}, so a device's role is not set to {new_role} unless asked for in so many words."
                )
            };
            return Err(ErrorBody::new(ErrorCode::Denied, message)
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
    let caller_device_id = ctx
        .pairing_id()
        .map(str::to_owned)
        .or_else(|| policy.device_owner(ctx.principal()).map(str::to_owned));
    let node_name = ctx.node_name().to_owned();
    let valid_node_name = NodeName::try_from(node_name)
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "node".to_owned());
    let paths = ctx.paths().clone();
    let paths_for_cancel = paths.clone();
    let role_changed = args.role.is_some();
    let should_push_fleet = args.role.is_some() || new_name.is_some();
    let new_role = args.role;
    let confirm = args.confirm;
    let caller_principal = ctx.principal().to_owned();

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
        repair_incomplete_enrolments_locked(
            &paths,
            &target,
            &write_path,
            caller_account.as_deref(),
        )?;

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
                    let auth_keys = authkeys::read_auth_keys(&write_path)?;
                    let is_local_only = auth_keys.lines.iter().any(|line| {
                        matches!(
                            &line.kind,
                            LineKind::Device { pairing_id: pid, principal, home, account, .. }
                                if pid == id
                                    && caller.may_manage(principal)
                                    && line_matches_home(home.as_ref(), &paths)
                                    && line_matches_account(
                                        account.as_deref(),
                                        caller_account.as_deref(),
                                        target.windows_admin_acl,
                                        cfg!(windows),
                                    )
                        )
                    });
                    if is_local_only {
                        return Err(ErrorBody::new(
                            ErrorCode::InvalidArgs,
                            format!(
                                "Device with id {id} is local-only (present in authorized_keys but not in devices.toml) and cannot be modified with device.set."
                            ),
                        )
                        .with_hint(
                            "Pair this device again with `legion pair` to manage it across the fleet, or remove it with `legion device.remove`.",
                        ));
                    }
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
                        let auth_keys = authkeys::read_auth_keys(&write_path)?;
                        let is_local_only = auth_keys.lines.iter().any(|line| {
                            matches!(
                                &line.kind,
                                LineKind::Device { name: line_name, principal, home, account, .. }
                                    if line_name == name
                                        && caller.may_manage(principal)
                                        && line_matches_home(home.as_ref(), &paths)
                                        && line_matches_account(
                                            account.as_deref(),
                                            caller_account.as_deref(),
                                            target.windows_admin_acl,
                                            cfg!(windows),
                                        )
                            )
                        });
                        if is_local_only {
                            return Err(ErrorBody::new(
                                ErrorCode::InvalidArgs,
                                format!(
                                    "Device {name} is local-only (present in authorized_keys but not in devices.toml) and cannot be modified with device.set."
                                ),
                            )
                            .with_hint(
                                "Pair this device again with `legion pair` to manage it across the fleet, or remove it with `legion device.remove`.",
                            ));
                        }
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

        if current_entry.conflict {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Device {} ({pairing_id}) has conflicting immutable identity or pairing provenance across peers and cannot be modified with device.set.",
                    current_entry.name
                ),
            )
            .with_hint(format!(
                "Remove the conflicted entry with `legion device.remove id={pairing_id}` and re-pair if needed."
            )));
        }

        if current_entry.role_conflict && new_role.is_none() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Device {} ({pairing_id}) has conflicting role updates with equal stamp and origin across peers; resolve the role conflict by passing `role=<role>` as owner.",
                    current_entry.name
                ),
            )
            .with_hint(format!(
                "Resolve as owner with `legion device.set id={pairing_id} role=<role>`, or remove with `legion device.remove id={pairing_id}`."
            )));
        }

        let quarantine_removed_ts = if confirm {
            let quarantined_map = registry::quarantined_devices_with_timestamps(&devices_file);
            match quarantined_map.get(&pairing_id) {
                Some((removed_ts, _)) => Some(*removed_ts),
                None => {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!(
                            "Device {} ({pairing_id}) is not quarantined, so confirm=true cannot be applied.",
                            current_entry.name
                        ),
                    )
                    .with_hint(
                        "Only a device that is currently quarantined may be confirmed with `device.set confirm=true`.",
                    ));
                }
            }
        } else {
            None
        };

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

        let policy_path = paths.config_file(Policy::FILE);
        let policy_before = if new_role.is_some() {
            legion_core::config::read_regular_file(&policy_path)?
        } else {
            None
        };

        let previous_role_effective = policy.role_of(&current_entry.principal).0;
        let mut policy_updated = false;
        let final_role = if let Some(ref target_role) = new_role {
            if current_entry.principal == OWNER {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "This device is pinned to the built-in `owner` principal, whose role cannot be changed.",
                )
                .with_hint("Pair the device with `legion pair --name ... --role ...` so it gets its own registry-owned `device:<name>` principal."));
            }
            let policy_has_principal = policy
                .principals
                .keys()
                .any(|p| p.as_str() == current_entry.principal);
            let owned_by_this_pairing =
                policy.device_owner(&current_entry.principal) == Some(pairing_id.as_str());
            let can_provision_on_role_conflict = current_entry.role_conflict
                && current_entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX)
                && !policy_has_principal
                && !devices_file.devices.iter().any(|(other_id, other)| {
                    other_id != &pairing_id && other.principal == current_entry.principal
                })
                && !devices_file.removed.iter().any(|(tomb_id, tomb)| {
                    tomb_id != &pairing_id && tomb.principal == current_entry.principal
                });
            if !owned_by_this_pairing && !can_provision_on_role_conflict {
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
            policy_updated = true;
            target_role.clone()
        } else {
            previous_role_effective.clone()
        };

        let rollback_policy = |error: ErrorBody| -> ErrorBody {
            if !policy_updated {
                return error;
            }
            let mut failures = Vec::new();
            if let Err(msg) = registry::restore_file_snapshot(
                &paths,
                "device.set",
                &policy_path,
                policy_before.as_deref(),
            ) {
                failures.push(msg);
            }
            registry::attach_rollback_failures(error, &failures)
        };

        let mut updated_entry = current_entry.clone();
        let now = Timestamp::now();
        let confirm_ts = if let Some(removed_ts) = quarantine_removed_ts {
            let removed_str = registry::format_precise_rfc3339(removed_ts);
            let ts = registry::next_updated_timestamp_at(Some(&removed_str), now)
                .map_err(&rollback_policy)?;
            updated_entry.confirmed_by = Some(caller_principal);
            updated_entry.confirmed_at = Some(ts.clone());
            Some(ts)
        } else {
            None
        };
        let (previous_name, name_ts) = if let Some(ref renamed) = new_name
            && renamed != &current_entry.name
        {
            let ts = registry::next_updated_timestamp_at(Some(&current_entry.name_updated_at), now)
                .map_err(&rollback_policy)?;
            updated_entry.name = renamed.clone();
            updated_entry.name_updated_at = ts.clone();
            updated_entry.name_updated_on = valid_node_name.clone();
            (Some(current_entry.name.clone()), Some(ts))
        } else {
            (None, None)
        };
        let (previous_role, role_ts) = if let Some(ref target_role) = new_role {
            let role_ts = if updated_entry.principal.starts_with(DEVICE_PRINCIPAL_PREFIX) {
                let ts =
                    registry::next_updated_timestamp_at(Some(&current_entry.role_updated_at), now)
                        .map_err(&rollback_policy)?;
                updated_entry.role = Some(target_role.clone());
                updated_entry.role_updated_at = ts.clone();
                updated_entry.role_updated_on = valid_node_name.clone();
                updated_entry.role_conflict = false;
                Some(ts)
            } else {
                None
            };
            (
                (target_role != &previous_role_effective).then_some(previous_role_effective),
                role_ts,
            )
        } else {
            (None, None)
        };

        let mut next_updated =
            registry::next_updated_timestamp_at(Some(&current_entry.updated_at), now)
                .map_err(&rollback_policy)?;
        for stamp in [&confirm_ts, &name_ts, &role_ts].into_iter().flatten() {
            if let (Ok(s_ts), Ok(u_ts)) = (
                stamp.parse::<Timestamp>(),
                next_updated.parse::<Timestamp>(),
            ) && u_ts < s_ts
            {
                next_updated = stamp.clone();
            }
        }
        updated_entry.updated_at = next_updated;
        updated_entry.updated_on = valid_node_name;
        devices_file
            .devices
            .insert(pairing_id.clone(), updated_entry.clone());

        save_devices_locked(&paths, &devices_file).map_err(&rollback_policy)?;

        if confirm {
            let _ = reconcile_device_principals_locked(&paths, &devices_file)?;
        }
        let live_policy = match legion_core::config::read_regular_file(&policy_path)? {
            Some(bytes) => legion_core::config::parse::<Policy>(&policy_path, &bytes)?,
            None => Policy::default(),
        };
        let effective_role_after = if confirm && new_role.is_none() {
            live_policy.role_of(&updated_entry.principal).0
        } else {
            final_role
        };

        let exe = legion_core::current_exe()?;
        let shell = ForcedCommandShell::host()?;
        edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
            let report = reconcile_authorized_keys_in_memory(
                &mut auth_keys,
                &devices_file,
                &live_policy,
                &paths,
                &target,
                caller_account.as_deref(),
                &exe,
                shell,
            )?;
            Ok((report.changed.then_some(auth_keys), ()))
        })?;

        // Keep local state/devices.json metadata in step on rename.
        if let Some(ref old_name) = previous_name
            && let Ok(mut state) = load_state(&paths)
        {
            let mut changed = false;
            if let Some(mut rec) = state.devices.remove(old_name) {
                rec.name = updated_entry.name.clone();
                state.devices.insert(updated_entry.name.clone(), rec);
                changed = true;
            }
            if let Some(unconf) = state.unconfirmed.get_mut(&pairing_id) {
                unconf.name = updated_entry.name.clone();
                changed = true;
            }
            if changed {
                let _ = save_state(&paths, &state);
            }
        }

        Ok(SetResult {
            id: pairing_id,
            name: updated_entry.name,
            previous_name,
            principal: updated_entry.principal,
            role: effective_role_after,
            previous_role,
            confirmed: confirm,
            fleet: BTreeMap::new(),
        })
    })
    .await?;

    if role_changed {
        let _ = crate::job::cancel_revoked_transfer_jobs(
            &paths_for_cancel,
            Some(&result.principal),
            Some(&result.id),
        )
        .await;
    }

    if should_push_fleet {
        let trigger = registry::DeviceRegistryPushAudit {
            trigger: "device.set".to_owned(),
            principal: Some(ctx.principal().to_owned()),
            device: ctx.device().map(str::to_owned),
            pairing_id: caller_device_id,
            source_node: Some(ctx.node_name().to_owned()),
        };
        result.fleet =
            push_devices_to_fleet(ctx.node(), ctx.door(), trigger, ctx.credential_expires()).await;
    }
    Ok(result)
}

/// Arguments of `device.list`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {}

/// What `device.list` returns.
#[derive(Debug, Serialize)]
struct ListResult {
    devices: Vec<ListedDevice>,
    invites: Vec<ListedInvite>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
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
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    home: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    foreign: bool,
    #[serde(skip_serializing_if = "is_false")]
    quarantined: bool,
    #[serde(skip_serializing_if = "is_false")]
    local_only: bool,
    #[serde(skip_serializing_if = "is_false")]
    missing_home: bool,
    #[serde(skip_serializing_if = "is_false")]
    unconfirmed: bool,
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

async fn list(ctx: &Ctx, _args: ListArgs) -> Result<ListResult, ToolError> {
    let caller = Caller::of(ctx)?;
    let caller_is_owner = caller.is_owner();
    let node_file = ctx.config().get::<NodeFile>()?;
    let policy = ctx.config().get::<Policy>()?;
    let node_name = ctx.node_name().to_owned();
    let paths = ctx.paths().clone();
    let paths_for_sessions = paths.clone();

    let (raw_devices, raw_invites, removed_stale, warnings) = blocking(move || {
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

        let (auth_keys, state, state_changed, devices_file, adopted_new, report) =
            edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                let mut state = load_state(&paths)?;
                let now = Timestamp::now();
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
                let (repaired_auth, repaired_state) = repair_unconfirmed_state_in_memory(
                    &mut auth_keys,
                    &mut state,
                    &devices_file,
                    &paths,
                );
                let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

                let report = reconcile_authorized_keys_in_memory(
                    &mut auth_keys,
                    &devices_file,
                    &policy,
                    &paths,
                    &target,
                    caller_account.as_deref(),
                    &exe,
                    shell,
                )?;
                let write_auth =
                    (repaired_auth || swept_auth || report.changed).then(|| auth_keys.clone());
                Ok((
                    write_auth,
                    (
                        auth_keys,
                        state,
                        repaired_state || swept_state,
                        devices_file,
                        adopted_new,
                        report,
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
                let is_unconfirmed = state.unconfirmed.contains_key(id);
                let (quarantined, status) = if report.immutable_conflict_ids.contains(id)
                    || entry.conflict
                {
                    (
                        false,
                        Some(format!(
                            "not active here: conflicting immutable identity or pairing provenance across peers; remove with `legion device.remove id={id}`"
                        )),
                    )
                } else if report.role_conflict_ids.contains(id) || entry.role_conflict {
                    (
                        false,
                        Some(format!(
                            "not active here: conflicting role updates with equal stamp and origin across peers; resolve as owner with `legion device.set id={id} role=<role>` or remove with `legion device.remove id={id}`"
                        )),
                    )
                } else if let Some(reason) = report.quarantined_ids.get(id) {
                    (
                        true,
                        Some(format!(
                            "quarantined: {reason}; confirm with `legion device.set id={id} confirm=true` or remove with `legion device.remove id={id}`"
                        )),
                    )
                } else if let Some(reason) = report.principal_conflict_ids.get(id) {
                    (false, Some(format!("not active here: {reason}")))
                } else if let Some(others) = report.duplicate_key_ids.get(id) {
                    (
                        false,
                        Some(format!(
                            "not active here: public key collides with another live device ({})",
                            others.join(", ")
                        )),
                    )
                } else if report.foreign_key_ids.contains(id) {
                    (false, Some(registry::FOREIGN_KEY_COLLISION_STATUS.to_owned()))
                } else if is_unconfirmed {
                    (false, Some("Pairing not finished".to_owned()))
                } else {
                    (false, None)
                };
                ListedDevice {
                    id: Some(id.clone()),
                    name: entry.name.clone(),
                    principal: entry.principal.clone(),
                    role: Some(effective_role),
                    fingerprint: DevicesFile::fingerprint_of(entry),
                    added: Some(entry.paired_at.clone()),
                    paired_on: Some(entry.paired_on.clone()),
                    status,
                    home: None,
                    foreign: false,
                    quarantined,
                    local_only: false,
                    missing_home: false,
                    unconfirmed: is_unconfirmed,
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
                        if devices_file.devices.contains_key(pairing_id) {
                            continue;
                        }
                        let fingerprint =
                            fingerprint_key_b64(key_b64).unwrap_or_else(|| "unknown".to_owned());
                        let effective_role = policy.role_of(principal).0;
                        devices.push(ListedDevice {
                            id: Some(pairing_id.clone()),
                            name: name.clone(),
                            principal: principal.clone(),
                            role: Some(effective_role),
                            fingerprint,
                            added: None,
                            paired_on: None,
                            status: Some(
                                "local-only: present in authorized_keys but not in devices.toml and never synced to peers; pair again with `legion pair` or remove with `legion device.remove`"
                                    .to_owned(),
                            ),
                            home: None,
                            foreign: false,
                            quarantined: false,
                            local_only: true,
                            missing_home: false,
                            unconfirmed: false,
                        });
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
                        status: None,
                        home: home.as_ref().map(PinnedHome::display_label),
                        foreign: true,
                        quarantined: false,
                        local_only: false,
                        missing_home,
                        unconfirmed: false,
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
        Ok((
            devices,
            invites,
            report.removed_targets,
            report.warnings,
        ))
    })
    .await?;

    for stale_target in removed_stale {
        let _ = sessions::terminate_device_sessions_for_target(
            &paths_for_sessions,
            &stale_target,
            sessions::DEFAULT_TERMINATE_GRACE,
        )
        .await;
    }

    let devices: Vec<ListedDevice> = raw_devices
        .into_iter()
        .filter(|d| d.foreign || caller.may_manage(&d.principal))
        .collect();

    let invites = raw_invites
        .into_iter()
        .filter(|i| i.foreign || caller.may_manage(&i.principal))
        .collect();

    let warnings = if caller_is_owner {
        warnings
    } else {
        let visible_ids: std::collections::HashSet<&str> =
            devices.iter().filter_map(|d| d.id.as_deref()).collect();
        warnings
            .into_iter()
            .filter(|w| visible_ids.iter().any(|id| w.contains(id)))
            .collect()
    };

    Ok(ListResult {
        devices,
        invites,
        warnings,
    })
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
    let caller_principal_for_tomb = ctx.principal().to_owned();
    let caller_device_id_for_tomb = ctx
        .pairing_id()
        .map(str::to_owned)
        .or_else(|| policy.device_owner(ctx.principal()).map(str::to_owned));
    let caller_device_id_for_push = caller_device_id_for_tomb.clone();
    let node_name = ctx.node_name().to_owned();
    let valid_node_name = NodeName::try_from(node_name.clone())
        .map(|n| n.as_str().to_owned())
        .unwrap_or_else(|_| "node".to_owned());
    let paths = ctx.paths().clone();
    let paths_for_sessions = paths.clone();

    enum RemoveBlockingError {
        BeforeSave(ErrorBody),
        AfterSave {
            error: ErrorBody,
            tombstoned: Vec<sessions::SessionTarget>,
        },
    }

    let blocking_result = tokio::task::spawn_blocking(move || -> Result<
        (
            Removed,
            Option<sessions::SessionTarget>,
            Vec<sessions::SessionTarget>,
            bool,
        ),
        RemoveBlockingError,
    > {
        let _cfg_guard = lock_config(&paths).map_err(RemoveBlockingError::BeforeSave)?;
        let existing_devices = load_devices_raw(&paths).map_err(RemoveBlockingError::BeforeSave)?;

        // For local paired-device removals, write the tombstone to `devices.toml` FIRST before
        // touching `policy.toml` or `authorized_keys`. If anything fails after `devices.toml` is
        // saved, return `AfterSave` so the caller still ends every live session for that device.
        if !remove_foreign {
            let mut devices_file = match &existing_devices {
                Some(file) => file.clone(),
                None => {
                    let target = resolve_target(&paths, &node_file)
                        .map_err(RemoveBlockingError::BeforeSave)?;
                    let caller_account = caller_account_for_target(&target)
                        .map_err(RemoveBlockingError::BeforeSave)?;
                    let write_path = resolve_safe_write_path(&target, paths.home())
                        .map_err(RemoveBlockingError::BeforeSave)?;
                    let auth_keys = if write_path.is_file() {
                        authkeys::read_auth_keys(&write_path)
                            .map_err(RemoveBlockingError::BeforeSave)?
                    } else {
                        authkeys::AuthKeysFile::parse(b"")
                    };
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

            let matched_id = match &target_item {
                RemoveTarget::Id(id) => devices_file
                    .devices
                    .get(id)
                    .filter(|entry| caller.may_manage(&entry.principal))
                    .map(|_| id.clone()),
                RemoveTarget::DeviceName(name) => {
                    let matched: Vec<String> = devices_file
                        .devices
                        .iter()
                        .filter(|(_, entry)| {
                            entry.name == *name && caller.may_manage(&entry.principal)
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                    if matched.len() > 1 {
                        return Err(RemoveBlockingError::BeforeSave(
                            ErrorBody::new(
                                ErrorCode::Conflict,
                                format!(
                                    "Multiple paired devices are named {name} (pairing ids: {}).",
                                    matched.join(", ")
                                ),
                            )
                            .with_hint(
                                "Pass `id=<pairing_id>` from `device.list` to choose which one to remove.",
                            ),
                        ));
                    }
                    matched.into_iter().next()
                }
            };

            if let Some(pairing_id) = matched_id {
                let quarantined_before = registry::quarantined_devices(&devices_file);
                let entry = devices_file.devices.remove(&pairing_id).ok_or_else(|| {
                    RemoveBlockingError::BeforeSave(ErrorBody::new(
                        ErrorCode::Internal,
                        "Matched device disappeared.",
                    ))
                })?;
                let removed_at = next_updated_timestamp(Some(&entry.updated_at))
                    .map_err(RemoveBlockingError::BeforeSave)?;
                devices_file.removed.insert(
                    pairing_id.clone(),
                    DeviceTombstone {
                        name: entry.name.clone(),
                        principal: entry.principal.clone(),
                        fingerprint: Some(DevicesFile::fingerprint_of(&entry)),
                        paired_at: Some(entry.paired_at),
                        paired_on: Some(entry.paired_on),
                        paired_by: entry.paired_by,
                        removed_by: Some(registry::PairedBy {
                            principal: caller_principal_for_tomb,
                            device: caller_device_id_for_tomb,
                        }),
                        removed_at,
                        removed_on: valid_node_name.clone(),
                    },
                );
                let quarantined_after = registry::quarantined_devices(&devices_file);

                let reserved_bytes = registry::devices_reserved_bytes(&devices_file);
                if !caller.is_owner()
                    && reserved_bytes > registry::NON_OWNER_DEVICES_TOML_LIMIT_BYTES
                {
                    return Err(RemoveBlockingError::BeforeSave(
                        registry::non_owner_devices_half_limit_error(
                            "remove a device",
                            reserved_bytes,
                        ),
                    ));
                }

                // 1. Write the tombstone to `devices.toml` FIRST. Turning a live entry into a
                //    tombstone is NEVER blocked by the 1 MiB size limit.
                registry::save_devices_locked_with_policy(
                    &paths,
                    &devices_file,
                    registry::SaveSizePolicy::AllowTombstoneOnly,
                )
                .map_err(RemoveBlockingError::BeforeSave)?;

                let primary_target =
                    sessions::SessionTarget::new(entry.name.clone(), Some(pairing_id.clone()));
                let mut tombstoned = vec![primary_target.clone()];
                for (qid, qdev) in &devices_file.devices {
                    if quarantined_after.contains_key(qid) && !quarantined_before.contains_key(qid)
                    {
                        tombstoned.push(sessions::SessionTarget::new(
                            qdev.name.clone(),
                            Some(qid.clone()),
                        ));
                    }
                }
                let after_save = |error: ErrorBody| RemoveBlockingError::AfterSave {
                    error,
                    tombstoned: tombstoned.clone(),
                };

                // 2. Reconcile `policy.toml` (clean up registry-owned `device:*` principal).
                let _ = reconcile_device_principals_locked(&paths, &devices_file)
                    .map_err(after_save)?;
                let policy_path = paths.config_file(Policy::FILE);
                let live_policy = match legion_core::config::read_regular_file(&policy_path)
                    .map_err(after_save)?
                {
                    Some(bytes) => legion_core::config::parse::<Policy>(&policy_path, &bytes)
                        .map_err(after_save)?,
                    None => Policy::default(),
                };

                // 3. Reconcile `authorized_keys` and sweep expired invites.
                let target = resolve_target(&paths, &node_file).map_err(after_save)?;
                let caller_account =
                    caller_account_for_target(&target).map_err(after_save)?;
                let write_path =
                    resolve_safe_write_path(&target, paths.home()).map_err(after_save)?;
                verify_owner_matches_euid(
                    &write_path,
                    paths.home(),
                    target.is_default_home_ssh,
                    legion_os::user::effective_uid(),
                )
                .map_err(after_save)?;
                let _guard = lock_auth_keys(&target, &write_path).map_err(after_save)?;

                let exe = legion_core::current_exe().map_err(after_save)?;
                let shell = ForcedCommandShell::host().map_err(after_save)?;
                let (lines_before, state_to_save, extra_stale) =
                    edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                        let mut state = load_state(&paths)?;
                        let now = Timestamp::now();
                        let (repaired_auth, _) = repair_unconfirmed_state_in_memory(
                            &mut auth_keys,
                            &mut state,
                            &devices_file,
                            &paths,
                        );
                        let (swept_auth, _) = sweep(&mut auth_keys, &mut state, now);
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
                        let report = reconcile_authorized_keys_in_memory(
                            &mut auth_keys,
                            &devices_file,
                            &live_policy,
                            &paths,
                            &target,
                            caller_account.as_deref(),
                            &exe,
                            shell,
                        )?;
                        let still_uses_name = devices_file
                            .devices
                            .values()
                            .any(|other| other.name == entry.name);
                        if !still_uses_name {
                            state.devices.remove(&entry.name);
                        }
                        state.unconfirmed.remove(&pairing_id);
                        let write_auth =
                            (repaired_auth || swept_auth || report.changed).then_some(auth_keys);
                        Ok((write_auth, (lines_before, state, report.removed_targets)))
                    })
                    .map_err(after_save)?;
                save_state(&paths, &state_to_save).map_err(after_save)?;

                return Ok((
                    Removed {
                        kind: "device",
                        name: Some(entry.name),
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
                    Some(primary_target),
                    extra_stale,
                    true,
                ));
            }
        }

        // Remaining paths: pending invites, foreign lines (`foreign=true`), or not-found errors.
        let target = resolve_target(&paths, &node_file).map_err(RemoveBlockingError::BeforeSave)?;
        let caller_account =
            caller_account_for_target(&target).map_err(RemoveBlockingError::BeforeSave)?;
        let write_path = resolve_safe_write_path(&target, paths.home())
            .map_err(RemoveBlockingError::BeforeSave)?;
        verify_owner_matches_euid(
            &write_path,
            paths.home(),
            target.is_default_home_ssh,
            legion_os::user::effective_uid(),
        )
        .map_err(RemoveBlockingError::BeforeSave)?;
        let _guard = lock_auth_keys(&target, &write_path).map_err(RemoveBlockingError::BeforeSave)?;
        repair_incomplete_enrolments_locked(
            &paths,
            &target,
            &write_path,
            caller_account.as_deref(),
        )
        .map_err(RemoveBlockingError::BeforeSave)?;

        let (outcome, state_to_save, devices_to_save, extra_stale) =
            edit_auth_keys_with_retry(&target, &write_path, |mut auth_keys| {
                let mut state = load_state(&paths)?;
                let now = Timestamp::now();
                let (swept_auth, swept_state) = sweep(&mut auth_keys, &mut state, now);

                let (devices_file, adopted_new) = match &existing_devices {
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

                match &target_item {
                    RemoveTarget::DeviceName(name) if !remove_foreign => {
                        // First check local-only device lines belonging to THIS home/account (not in devices.toml).
                        let mut local_only_matches: Vec<(String, String)> = Vec::new();
                        for line in &auth_keys.lines {
                            if let LineKind::Device {
                                name: line_name,
                                principal,
                                pairing_id: pid,
                                home,
                                account,
                                ..
                            } = &line.kind
                                && line_name == name
                                && !devices_file.devices.contains_key(pid)
                                && caller.may_manage(principal)
                                && line_matches_home(home.as_ref(), &paths)
                                && line_matches_account(
                                    account.as_deref(),
                                    caller_account.as_deref(),
                                    target.windows_admin_acl,
                                    cfg!(windows),
                                )
                                && !local_only_matches.iter().any(|(id, _)| id == pid)
                            {
                                local_only_matches.push((pid.clone(), principal.clone()));
                            }
                        }
                        if local_only_matches.len() > 1 {
                            let ids: Vec<&str> = local_only_matches
                                .iter()
                                .map(|(id, _)| id.as_str())
                                .collect();
                            let err = ErrorBody::new(
                                ErrorCode::Conflict,
                                format!(
                                    "Multiple local-only devices are named {name} (pairing ids: {}).",
                                    ids.join(", ")
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
                        if let Some((pid, principal)) = local_only_matches.into_iter().next() {
                            let mut removed_count = 0usize;
                            auth_keys.retain(|line| {
                                let drop_it = matches!(
                                    &line.kind,
                                    LineKind::Device {
                                        name: line_name,
                                        pairing_id: line_pid,
                                        home,
                                        account,
                                        ..
                                    } if line_name == name
                                        && line_pid == &pid
                                        && line_matches_home(home.as_ref(), &paths)
                                        && line_matches_account(
                                            account.as_deref(),
                                            caller_account.as_deref(),
                                            target.windows_admin_acl,
                                            cfg!(windows),
                                        )
                                );
                                if drop_it {
                                    removed_count += 1;
                                }
                                !drop_it
                            });
                            return Ok((
                                Some(auth_keys),
                                (
                                    Ok((
                                        Removed {
                                            kind: "device",
                                            name: Some(name.clone()),
                                            id: Some(pid.clone()),
                                            principal,
                                            removed: true,
                                            lines_removed: removed_count,
                                            sessions_ended: 0,
                                            sessions_not_ended: false,
                                            jobs_left_running: Vec::new(),
                                            fleet: BTreeMap::new(),
                                            still_trusted_by: Vec::new(),
                                            hint: None,
                                        },
                                        Some(sessions::SessionTarget::new(
                                            name.clone(),
                                            Some(pid),
                                        )),
                                    )),
                                    swept_state.then_some(state),
                                    adopted_new.then_some(devices_file),
                                    Vec::new(),
                                ),
                            ));
                        }

                        let visible_entries: Vec<(&String, &registry::FleetDevice)> = devices_file
                            .devices
                            .iter()
                            .filter(|(_, entry)| caller.may_manage(&entry.principal))
                            .collect();

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
                        // First check local-only device lines belonging to THIS home/account by pairing id.
                        if !remove_foreign {
                            let local_only_match = auth_keys.lines.iter().find_map(|line| {
                                match &line.kind {
                                    LineKind::Device {
                                        name: line_name,
                                        principal,
                                        pairing_id: line_pid,
                                        home,
                                        account,
                                        ..
                                    } if line_pid == id
                                        && !devices_file.devices.contains_key(line_pid)
                                        && caller.may_manage(principal)
                                        && line_matches_home(home.as_ref(), &paths)
                                        && line_matches_account(
                                            account.as_deref(),
                                            caller_account.as_deref(),
                                            target.windows_admin_acl,
                                            cfg!(windows),
                                        ) =>
                                    {
                                        Some((line_name.clone(), principal.clone()))
                                    }
                                    _ => None,
                                }
                            });
                            if let Some((dev_name, principal)) = local_only_match {
                                let mut removed_count = 0usize;
                                auth_keys.retain(|line| {
                                    let drop_it = matches!(
                                        &line.kind,
                                        LineKind::Device {
                                            pairing_id: line_pid,
                                            home,
                                            account,
                                            ..
                                        } if line_pid == id
                                            && line_matches_home(home.as_ref(), &paths)
                                            && line_matches_account(
                                                account.as_deref(),
                                                caller_account.as_deref(),
                                                target.windows_admin_acl,
                                                cfg!(windows),
                                            )
                                    );
                                    if drop_it {
                                        removed_count += 1;
                                    }
                                    !drop_it
                                });
                                return Ok((
                                    Some(auth_keys),
                                    (
                                        Ok((
                                            Removed {
                                                kind: "device",
                                                name: Some(dev_name.clone()),
                                                id: Some(id.clone()),
                                                principal,
                                                removed: true,
                                                lines_removed: removed_count,
                                                sessions_ended: 0,
                                                sessions_not_ended: false,
                                                jobs_left_running: Vec::new(),
                                                fleet: BTreeMap::new(),
                                                still_trusted_by: Vec::new(),
                                                hint: None,
                                            },
                                            Some(sessions::SessionTarget::new(
                                                dev_name,
                                                Some(id.clone()),
                                            )),
                                        )),
                                        swept_state.then_some(state),
                                        adopted_new.then_some(devices_file),
                                        Vec::new(),
                                    ),
                                ));
                            }
                        }

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
            })
            .map_err(RemoveBlockingError::BeforeSave)?;
        match outcome {
            Ok((result, terminate)) => {
                if let Some(devices) = devices_to_save {
                    save_devices_locked(&paths, &devices).map_err(RemoveBlockingError::BeforeSave)?;
                    let _ = reconcile_device_principals_locked(&paths, &devices)
                        .map_err(RemoveBlockingError::BeforeSave)?;
                }
                if let Some(state) = state_to_save {
                    save_state(&paths, &state).map_err(RemoveBlockingError::BeforeSave)?;
                }
                Ok((result, terminate, extra_stale, false))
            }
            Err(err) => {
                if let Some(devices) = devices_to_save {
                    let _ = save_devices_locked(&paths, &devices);
                }
                if let Some(state) = state_to_save {
                    let _ = save_state(&paths, &state);
                }
                Err(RemoveBlockingError::BeforeSave(err))
            }
        }
    })
    .await
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("device.remove stopped unexpectedly: {error}."),
        )
    })?;

    let (mut removed, terminate_target, extra_stale, push_to_fleet) = match blocking_result {
        Ok(ok) => ok,
        Err(RemoveBlockingError::BeforeSave(err)) => return Err(err),
        Err(RemoveBlockingError::AfterSave { error, tombstoned }) => {
            for target in &tombstoned {
                let _ = sessions::terminate_device_sessions_for_target(
                    &paths_for_sessions,
                    target,
                    sessions::DEFAULT_TERMINATE_GRACE,
                )
                .await;
                let _ = crate::job::cancel_transfer_jobs_for_device(
                    &paths_for_sessions,
                    &target.device,
                    target.pairing_id.as_deref(),
                    None,
                )
                .await;
            }
            return Err(error);
        }
    };

    if let Some(target) = terminate_target {
        removed.sessions_ended = sessions::terminate_device_sessions_for_target(
            &paths_for_sessions,
            &target,
            sessions::DEFAULT_TERMINATE_GRACE,
        )
        .await;
        let _ = crate::job::cancel_transfer_jobs_for_device(
            &paths_for_sessions,
            &target.device,
            target.pairing_id.as_deref(),
            Some(&removed.principal),
        )
        .await;
        for stale in extra_stale {
            if stale != target {
                let _ = sessions::terminate_device_sessions_for_target(
                    &paths_for_sessions,
                    &stale,
                    sessions::DEFAULT_TERMINATE_GRACE,
                )
                .await;
                let _ = crate::job::cancel_transfer_jobs_for_device(
                    &paths_for_sessions,
                    &stale.device,
                    stale.pairing_id.as_deref(),
                    None,
                )
                .await;
            }
        }
        let paths_for_jobs = paths_for_sessions.clone();
        let principal_for_jobs = removed.principal.clone();
        let device_for_jobs = target.device.clone();
        let pairing_for_jobs = target.pairing_id.clone();
        removed.jobs_left_running = blocking(move || {
            Ok(crate::job::running_job_ids_for_device(
                &paths_for_jobs,
                &device_for_jobs,
                pairing_for_jobs.as_deref(),
                &principal_for_jobs,
            ))
        })
        .await
        .unwrap_or_default();

        if push_to_fleet {
            let trigger = registry::DeviceRegistryPushAudit {
                trigger: "device.remove".to_owned(),
                principal: Some(ctx.principal().to_owned()),
                device: ctx.device().map(str::to_owned),
                pairing_id: caller_device_id_for_push,
                source_node: Some(ctx.node_name().to_owned()),
            };
            let fleet_outcomes =
                push_devices_to_fleet(ctx.node(), ctx.door(), trigger, ctx.credential_expires())
                    .await;
            let still_trusted: Vec<String> = fleet_outcomes
                .iter()
                .filter(|(_, outcome)| outcome.status != "synced" && outcome.status != "partial")
                .map(|(machine, _)| machine.clone())
                .collect();
            let partial_peers: Vec<String> = fleet_outcomes
                .iter()
                .filter(|(_, outcome)| outcome.status == "partial")
                .map(|(machine, _)| machine.clone())
                .collect();
            if !still_trusted.is_empty() {
                let label = match target.pairing_id.as_deref() {
                    Some(id) => format!("{} ({id})", target.device),
                    None => target.device.clone(),
                };
                let mut hint = format!(
                    "These machines could not be reached and still trust {label} until their next sync: {}. Run `legion fleet.sync direction=push documents='[\"devices.toml\"]'` when they are back online.",
                    still_trusted.join(", ")
                );
                if !partial_peers.is_empty() {
                    hint.push_str(&format!(
                        " Removals were delivered to {}, though live device additions were refused there (see `fleet` for reasons).",
                        partial_peers.join(", ")
                    ));
                }
                removed.hint = Some(hint);
            } else if !partial_peers.is_empty() {
                removed.hint = Some(format!(
                    "Removal was delivered to {}, though live device additions were refused there (see `fleet` for reasons).",
                    partial_peers.join(", ")
                ));
            }
            removed.fleet = fleet_outcomes;
            removed.still_trusted_by = still_trusted;
        }
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
        now,
        |_, _| Ok(record.clone()),
        save_fn,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn commit_invite_and_state_checked(
    paths: &legion_core::Paths,
    target: &AuthKeysTarget,
    write_path: &std::path::Path,
    invite_id: &str,
    invite_line: &str,
    now: Timestamp,
    build_record: impl Fn(
        &self::authkeys::AuthKeysFile,
        &store::DevicesState,
    ) -> Result<InviteRecord, ErrorBody>,
    save_fn: impl FnOnce(&legion_core::Paths, &store::DevicesState) -> Result<(), ErrorBody>,
) -> Result<InviteRecord, ErrorBody> {
    let (mut state, record) = edit_auth_keys_with_retry(target, write_path, |mut auth_keys| {
        let mut state = load_state(paths)?;
        sweep(&mut auth_keys, &mut state, now);
        let record = build_record(&auth_keys, &state)?;
        auth_keys.append_line(invite_line);
        Ok((Some(auth_keys), (state, record)))
    })?;

    state.invites.insert(invite_id.to_owned(), record.clone());
    if let Err(state_err) = save_fn(paths, &state) {
        // Roll back through the guarded concurrent-edit path by removing only this invite's line,
        // so any out-of-band edit that landed after the append is preserved intact.
        let _ = rollback_invite_line(target, write_path, invite_id);
        return Err(state_err);
    }
    Ok(record)
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
                not_synced_to: Vec::new(),
                sync_command: None,
            }
        );

        // Second use of the same invite must fail!
        let replay = run_enrol(&h.node, id, req1.as_bytes()).await;
        assert!(!replay.is_ok());

        // Confirm device_key1 via its first authenticated gate session before testing duplicate-key
        // conflict against an already-confirmed device.
        let reg_after_first = registry::load_devices_raw(h.node.paths()).unwrap().unwrap();
        let pid1 = reg_after_first.devices.keys().next().unwrap().clone();
        let node_file = h.node.config().get::<NodeFile>().unwrap();
        registry::verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "my-pixel",
            &pid1,
            "phone",
            None,
        )
        .unwrap();

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
                not_synced_to: Vec::new(),
                sync_command: None,
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

        // device.remove removes the named device and keeps the foreign line + second device,
        // recording key fingerprint, pairing provenance, and remover in the tombstone.
        let removed = h
            .call("device.remove", json!({"name": "my-pixel"}))
            .await
            .unwrap();
        assert_eq!(removed["kind"], json!("device"));
        assert_eq!(removed["removed"], json!(true));
        let removed_id = removed["id"].as_str().unwrap();
        let saved_after_rm: registry::DevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        let tomb = saved_after_rm.removed.get(removed_id).unwrap();
        assert_eq!(tomb.name, "my-pixel");
        assert_eq!(tomb.principal, "phone");
        assert_eq!(
            tomb.fingerprint.as_deref(),
            Some(device_key1.fingerprint.as_str())
        );
        assert_eq!(
            tomb.paired_by.as_ref().map(|p| p.principal.as_str()),
            Some("owner")
        );
        assert_eq!(
            tomb.removed_by.as_ref().map(|p| p.principal.as_str()),
            Some("owner")
        );
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

        // Non-owner admin (`tablet`, role `phone_admin` with `effect:admin`) has an owner-strength
        // role, so it may NOT invite for itself (nor for `phone` or `owner`), even with
        // `allow_owner=true`: only an `owner` caller with `allow_owner=true` may issue invites for
        // an owner-strength principal.
        for allow_owner in [false, true] {
            let self_err = h
                .call_as(
                    "device.invite",
                    "tablet",
                    json!({
                        "principal": "tablet",
                        "allow_owner": allow_owner,
                        "host": "tower.example",
                        "user": "me"
                    }),
                )
                .await
                .unwrap_err();
            assert_eq!(self_err.code, ErrorCode::Denied, "{self_err:?}");
            assert!(
                self_err.message.contains("phone_admin")
                    && self_err
                        .message
                        .contains(crate::roles::OWNER_STRENGTH_REASON),
                "{self_err:?}"
            );
        }

        // Owner without `allow_owner=true` is also refused for `tablet` (owner-strength).
        let tablet_no_flag = h
            .call(
                "device.invite",
                json!({"principal": "tablet", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
        assert_eq!(tablet_no_flag.code, ErrorCode::Denied, "{tablet_no_flag:?}");
        assert!(
            tablet_no_flag.message.contains("phone_admin")
                && tablet_no_flag
                    .message
                    .contains(crate::roles::OWNER_STRENGTH_REASON),
            "{tablet_no_flag:?}"
        );

        // Owner WITH `allow_owner=true` creates `tablet`'s invite.
        let tablet_inv = h
            .call(
                "device.invite",
                json!({
                    "principal": "tablet",
                    "allow_owner": true,
                    "host": "tower.example",
                    "user": "me"
                }),
            )
            .await
            .unwrap();
        let tablet_inv_id = tablet_inv["id"].as_str().unwrap().to_owned();

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
    async fn enrol_step1_state_failure_leaves_disk_clean_and_step4_state_failure_replies_ok() {
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

        // 1. Make state_dir read-only (0500) BEFORE enrolment: step 1 writes `state.unconfirmed`
        //    first, so failing there refuses cleanly without touching `devices.toml` or
        //    `authorized_keys`.
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
        let err_reply = run_enrol(&h.node, &id, line.as_bytes()).await;
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            !err_reply.is_ok(),
            "step 1 state write failure must fail before mutating devices.toml/authorized_keys: {err_reply:?}"
        );
        let auth_after_step1_fail = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(!auth_after_step1_fail.contains("legion-device:resilient-phone"));
        assert!(auth_after_step1_fail.contains(&format!("legion-invite:{id}")));
        assert!(
            registry::load_devices_raw(h.node.paths())
                .unwrap()
                .is_none(),
            "devices.toml must not be written when step 1 fails"
        );

        // 2. Step 4 state write failure AFTER step 3 already wrote the permanent device line to
        //    authorized_keys: still replies ok: true (the device is active) and leaves the
        //    unconfirmed record so the next run repairs the leftover invite cleanly.
        let reply = enrol::run_enrol_with_kill_point(
            &h.node,
            &id,
            line.as_bytes(),
            enrol::EnrolKillPoint::FailStep4StateSave,
        )
        .await;
        assert_eq!(
            reply,
            EnrolReply::Ok {
                ok: true,
                node: "tower".to_owned(),
                principal: "phone".to_owned(),
                role: "observer".to_owned(),
                device: "resilient-phone".to_owned(),
                fleet: BTreeMap::new(),
                not_synced_to: Vec::new(),
                sync_command: None,
            }
        );

        // authorized_keys has the permanent device line and no longer has the invite line.
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(auth_text.contains("legion-device:resilient-phone"));
        assert!(!auth_text.contains(&format!("legion-invite:{id}")));

        // Even though step 4 failed to save state, device.list repairs state.invites and reports
        // the paired device cleanly (still marked unconfirmed locally until first session).
        let listed = h.call("device.list", json!({})).await.unwrap();
        assert_eq!(listed["invites"].as_array().unwrap().len(), 0);
        let devices = listed["devices"].as_array().unwrap();
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0]["name"], json!("resilient-phone"));
        assert_eq!(devices[0]["principal"], json!("phone"));
        assert_eq!(devices[0]["fingerprint"], json!(device_key.fingerprint));
        assert_eq!(devices[0]["unconfirmed"], json!(true));
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
                created_by_device: None,
                created_by_device_name: None,
                created: format_rfc3339(now),
                expires: format_rfc3339(exp),
                fingerprint: eph.fingerprint.clone(),
                host_key_fingerprints: Vec::new(),
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
        // Only the minimal fields a client needs are returned (no `host_keys_source`).
        let ep = h
            .call_as("device.endpoint", "phone", json!({}))
            .await
            .unwrap();
        assert_eq!(ep["node"], json!("tower"));
        assert_eq!(ep["hosts"], json!(["tower.tailnet.example", "100.64.0.10"]));
        assert_eq!(ep["port"], json!(2222));
        assert_eq!(ep["user"], json!("me"));
        assert_eq!(ep["host_keys"], json!([h.host_key]));
        assert!(
            ep.get("host_keys_source").is_none(),
            "device.endpoint must not expose host_keys_source: {ep}"
        );
        let mut keys: Vec<&str> = ep.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["host_keys", "hosts", "node", "port", "user"]);

        // device.invite without explicit host/port/user uses the exact same endpoint settings
        // and still reports host_keys_source for admin callers / `legion pair` warnings.
        let inv = h
            .call("device.invite", json!({"principal": "phone"}))
            .await
            .unwrap();
        assert_eq!(inv["hosts"], ep["hosts"]);
        assert_eq!(inv["port"], ep["port"]);
        assert_eq!(inv["user"], ep["user"]);
        assert_eq!(inv["host_keys"], ep["host_keys"]);
        assert_eq!(inv["host_keys_source"], json!("configured"));
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
                not_synced_to: Vec::new(),
                sync_command: None,
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

        // Raising role to `owner` or any owner-strength (`effect:admin`) role via `device.set`
        // requires `allow_owner=true`.
        for role_name in ["owner", "phone_admin"] {
            let err = h
                .call("device.set", json!({"name": "my-phone", "role": role_name}))
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{role_name}: {err:?}");
            assert!(
                err.message.contains(role_name)
                    && err.message.contains(crate::roles::OWNER_STRENGTH_REASON),
                "{role_name}: {err:?}"
            );
        }

        // Owner changes role to `phone_admin` (with `allow_owner=true`) and renames to
        // `pocket-phone`.
        let updated = h
            .call(
                "device.set",
                json!({
                    "name": "my-phone",
                    "role": "phone_admin",
                    "allow_owner": true,
                    "rename": "Pocket Phone"
                }),
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
                not_synced_to: Vec::new(),
                sync_command: None,
            }
        );
        assert_eq!(
            h.node
                .config()
                .get::<Policy>()
                .unwrap()
                .role_of("device:pixel")
                .0,
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
            h.node
                .config()
                .get::<Policy>()
                .unwrap()
                .role_of("device:raced")
                .0,
            "owner"
        );
    }

    #[tokio::test]
    async fn device_set_role_refuses_shared_principal_and_credentials_refuse_registry_owned_principal()
     {
        let h = Harness::new();

        // 1. Pair a device under shared principal `tablet` via `--as` (`principal`), passing
        //    `allow_owner=true` because `tablet`'s role (`phone_admin`) grants `effect:admin`.
        let shared_inv = h
            .call(
                "device.invite",
                json!({
                    "principal": "tablet",
                    "allow_owner": true,
                    "host": "tower.example",
                    "user": "me"
                }),
            )
            .await
            .unwrap();
        let shared_id = shared_inv["id"].as_str().unwrap();
        let shared_key = generate_ephemeral_ed25519().unwrap();
        let shared_req = format!(
            "{{\"enrol\":1,\"id\":\"{shared_id}\",\"public_key\":\"{}\",\"name\":\"shared-tab\"}}\n",
            shared_key.public_key
        );
        assert!(
            run_enrol(&h.node, shared_id, shared_req.as_bytes())
                .await
                .is_ok()
        );

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
        assert!(
            run_enrol(&h.node, owned_inv_id, owned_req.as_bytes())
                .await
                .is_ok()
        );

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

    #[tokio::test]
    async fn expiring_credential_is_refused_by_device_set_for_role_and_rename() {
        let h = Harness::new();
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            r#"
default = "owner"

[roles.observer]
allow = ["effect:read"]

[roles.operator]
allow = ["effect:read", "effect:write"]
"#,
        )
        .unwrap();
        let inv = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "tablet", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let id = inv["id"].as_str().unwrap();
        let key = generate_ephemeral_ed25519().unwrap();
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{id}\",\"public_key\":\"{}\",\"name\":\"tablet\"}}\n",
            key.public_key
        );
        assert!(run_enrol(&h.node, id, req.as_bytes()).await.is_ok());

        let expires = Some(Timestamp::now() + SignedDuration::from_hours(1));
        for args in [
            json!({"name": "tablet", "role": "operator"}),
            json!({"name": "tablet", "rename": "tablet-2"}),
        ] {
            let mut call = Call::new(Door::Http, "device.set", args.clone());
            call.credential_expires = expires;
            let err = h.node.call(call).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{args}: {err:?}");
        }
    }

    #[tokio::test]
    async fn duplicate_live_keys_and_foreign_key_collision_write_no_line_and_warn_in_list_and_doctor()
     {
        use crate::device::keys::ed25519_public_key_from_seed;
        use crate::device::registry::{DevicesFile, FleetDevice};

        let h = Harness::new();
        let shared_key = ed25519_public_key_from_seed(&[77u8; 32]);
        let foreign_key = ed25519_public_key_from_seed(&[88u8; 32]);
        let good_key = ed25519_public_key_from_seed(&[99u8; 32]);

        // Put `foreign_key` on an unrelated hand-written authorized_keys line.
        std::fs::write(&h.auth_keys, format!("{foreign_key} hand-written@laptop\n")).unwrap();

        let id1 = "11".repeat(16);
        let id2 = "22".repeat(16);
        let id_foreign = "33".repeat(16);
        let id_good = "44".repeat(16);

        let mut file = DevicesFile::default();
        for (id, name, key) in [
            (&id1, "dup-a", &shared_key),
            (&id2, "dup-b", &shared_key),
            (&id_foreign, "foreign-dev", &foreign_key),
            (&id_good, "healthy-dev", &good_key),
        ] {
            file.devices.insert(
                id.clone(),
                FleetDevice {
                    name: name.to_owned(),
                    public_key: key.clone(),
                    principal: "owner".to_owned(),
                    role: None,
                    paired_at: "2026-11-24T10:00:00Z".to_owned(),
                    paired_on: "tower".to_owned(),
                    paired_by: None,
                    confirmed_by: None,
                    confirmed_at: None,
                    updated_at: "2026-11-24T10:00:00Z".to_owned(),
                    updated_on: "tower".to_owned(),
                    name_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                    name_updated_on: "tower".to_owned(),
                    role_updated_at: "2026-11-24T10:00:00Z".to_owned(),
                    role_updated_on: "tower".to_owned(),
                    conflict: false,
                    role_conflict: false,
                },
            );
        }
        std::fs::write(
            h.node.paths().config_file("devices.toml"),
            file.to_toml_string(),
        )
        .unwrap();

        let listed = h.call("device.list", json!({})).await.unwrap();
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        // Neither dup-a nor dup-b may have a line written; foreign-dev gets no Legion line (its
        // hand-written line stays intact); healthy-dev gets its line.
        assert!(!auth_text.contains("legion-device:dup-a"), "{auth_text}");
        assert!(!auth_text.contains("legion-device:dup-b"), "{auth_text}");
        assert!(
            !auth_text.contains("legion-device:foreign-dev"),
            "{auth_text}"
        );
        assert!(auth_text.contains("hand-written@laptop"), "{auth_text}");
        assert!(
            auth_text.contains("legion-device:healthy-dev"),
            "{auth_text}"
        );

        let devices = listed["devices"].as_array().unwrap();
        let foreign_entry = devices
            .iter()
            .find(|d| d["name"] == json!("foreign-dev"))
            .unwrap();
        assert_eq!(
            foreign_entry["status"],
            json!("not active here: this key is already on another line of authorized_keys")
        );
        let dup_a = devices
            .iter()
            .find(|d| d["name"] == json!("dup-a"))
            .unwrap();
        let dup_b = devices
            .iter()
            .find(|d| d["name"] == json!("dup-b"))
            .unwrap();
        assert!(
            dup_a["status"]
                .as_str()
                .is_some_and(|s| s.contains("not active here")),
            "{dup_a}"
        );
        assert!(
            dup_b["status"]
                .as_str()
                .is_some_and(|s| s.contains("not active here")),
            "{dup_b}"
        );
        let warnings = listed["warnings"].as_array().unwrap();
        assert!(
            !warnings.is_empty(),
            "expected warnings in device.list: {listed}"
        );

        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        let checks = doctor["checks"].as_array().unwrap();
        assert!(
            checks.iter().any(|c| c["warning"] == json!(true)
                && c["detail"]
                    .as_str()
                    .is_some_and(|d| d.contains("dup-a") && d.contains("dup-b"))),
            "expected duplicate-key warning in meta.doctor: {doctor}"
        );
        assert!(
            checks.iter().any(|c| c["warning"] == json!(true)
                && c["detail"]
                    .as_str()
                    .is_some_and(|d| d.contains("foreign-dev")
                        && d.contains("already on another line of authorized_keys"))),
            "expected foreign-key warning in meta.doctor: {doctor}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remove_writes_tombstone_first_and_runs_session_termination_after_save_failure() {
        use std::os::unix::fs::PermissionsExt;

        let h = Harness::new();
        let inv = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "pixel", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv_id = inv["id"].as_str().unwrap();
        let key = generate_ephemeral_ed25519().unwrap();
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{inv_id}\",\"public_key\":\"{}\",\"name\":\"pixel\"}}\n",
            key.public_key
        );
        assert!(run_enrol(&h.node, inv_id, req.as_bytes()).await.is_ok());

        // 1. Unwritable config/ directory: `device.remove` must fail BEFORE touching authorized_keys.
        let cfg_dir = h.node.paths().config_dir().to_path_buf();
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let unwritable_err = h.call("device.remove", json!({"name": "pixel"})).await;
        std::fs::set_permissions(&cfg_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            unwritable_err.is_err(),
            "device.remove must fail when config dir is unwritable"
        );
        let auth_after_unwritable = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(
            auth_after_unwritable.contains("legion-device:pixel"),
            "authorized_keys must remain untouched when saving devices.toml fails:\n{auth_after_unwritable}"
        );

        // 2. Failure AFTER saving devices.toml (authorized_keys replaced with a directory):
        //    devices.toml MUST contain the tombstone AND session termination MUST still run.
        let sess_dir = h.node.paths().state_dir().join("device-sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let marker_session = sess_dir.join("pixel-999999-1.json");
        std::fs::write(&marker_session, b"{\"stale\":true}\n").unwrap();

        std::fs::remove_file(&h.auth_keys).unwrap();
        std::fs::create_dir(&h.auth_keys).unwrap();
        let after_save_err = h.call("device.remove", json!({"name": "pixel"})).await;
        let _ = std::fs::remove_dir(&h.auth_keys);
        assert!(
            after_save_err.is_err(),
            "device.remove must report error when authorized_keys cannot be updated"
        );
        let saved: DevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        assert!(
            saved.devices.is_empty(),
            "device should be removed from live map"
        );
        assert_eq!(saved.removed.len(), 1, "tombstone must be saved durably");
        assert!(
            !marker_session.exists(),
            "session termination/sweep must run whenever devices.toml was saved, even if authorized_keys update fails"
        );
    }

    #[tokio::test]
    async fn strict_adoption_requires_matching_state_and_keeps_unmatched_lines_local_only() {
        use crate::device::authkeys::{ForcedCommandShell, PinnedHome, format_device_line};
        use crate::device::keys::{ed25519_public_key_from_seed, validate_ed25519_public_key};
        use crate::device::registry::verify_gate_device;
        use crate::device::store::{DeviceRecord, DevicesState, save_state};

        let h = Harness::new();
        let exe = legion_core::current_exe().unwrap();
        let shell = ForcedCommandShell::host().unwrap();
        let pinned_home = PinnedHome::from_paths(h.node.paths());

        let adopted_id = "11".repeat(16);
        let adopted_key = ed25519_public_key_from_seed(&[11u8; 32]);
        let adopted_fp = validate_ed25519_public_key(&adopted_key)
            .unwrap()
            .fingerprint;

        let local_only_id = "22".repeat(16);
        let local_only_key = ed25519_public_key_from_seed(&[22u8; 32]);

        let mismatch_id = "33".repeat(16);
        let mismatch_key = ed25519_public_key_from_seed(&[33u8; 32]);
        let wrong_fp = validate_ed25519_public_key(&ed25519_public_key_from_seed(&[44u8; 32]))
            .unwrap()
            .fingerprint;

        let line_adopted = format_device_line(
            &exe,
            "phone",
            &adopted_key,
            "matched-phone",
            &adopted_id,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        let line_local = format_device_line(
            &exe,
            "phone",
            &local_only_key,
            "untracked-phone",
            &local_only_id,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        let line_mismatch = format_device_line(
            &exe,
            "phone",
            &mismatch_key,
            "mismatch-phone",
            &mismatch_id,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        std::fs::write(
            &h.auth_keys,
            format!("{line_adopted}\n{line_local}\n{line_mismatch}\n"),
        )
        .unwrap();

        // Populate state/devices.json with an exact match for `matched-phone` and a mismatched
        // fingerprint for `mismatch-phone`, and nothing for `untracked-phone`.
        let mut state = DevicesState::default();
        state.devices.insert(
            "matched-phone".to_owned(),
            DeviceRecord {
                name: "matched-phone".to_owned(),
                principal: "phone".to_owned(),
                added: "2026-11-24T10:00:00Z".to_owned(),
                fingerprint: adopted_fp,
                pairing_id: Some(adopted_id.clone()),
            },
        );
        state.devices.insert(
            "mismatch-phone".to_owned(),
            DeviceRecord {
                name: "mismatch-phone".to_owned(),
                principal: "phone".to_owned(),
                added: "2026-11-24T10:00:00Z".to_owned(),
                fingerprint: wrong_fp,
                pairing_id: Some(mismatch_id.clone()),
            },
        );
        save_state(h.node.paths(), &state).unwrap();

        let listed = h.call("device.list", json!({})).await.unwrap();

        // Only `matched-phone` may be adopted into devices.toml.
        let saved: DevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        assert!(saved.devices.contains_key(&adopted_id));
        assert!(
            !saved.devices.contains_key(&local_only_id),
            "untracked line must NOT be adopted into devices.toml"
        );
        assert!(
            !saved.devices.contains_key(&mismatch_id),
            "mismatched-fingerprint line must NOT be adopted into devices.toml"
        );

        // Both unmatched lines stay intact in authorized_keys and keep working locally.
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(
            auth_text.contains("legion-device:untracked-phone"),
            "{auth_text}"
        );
        assert!(
            auth_text.contains("legion-device:mismatch-phone"),
            "{auth_text}"
        );

        let node_file = h.node.config().get::<NodeFile>().unwrap();
        verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "untracked-phone",
            &local_only_id,
            "phone",
            None,
        )
        .expect("local-only line must continue working locally via legion gate");

        // `device.list` shows unmatched lines as local_only with a hint to pair again or remove.
        let devices = listed["devices"].as_array().unwrap();
        let untracked = devices
            .iter()
            .find(|d| d["name"] == json!("untracked-phone"))
            .expect("untracked-phone must appear in device.list");
        assert_eq!(untracked["local_only"], json!(true), "{untracked}");
        assert!(
            untracked["status"]
                .as_str()
                .is_some_and(|s| s.contains("local-only") && s.contains("remove")),
            "{untracked}"
        );

        // `device.set` refuses local-only lines cleanly.
        let set_err = h
            .call(
                "device.set",
                json!({"name": "untracked-phone", "rename": "renamed"}),
            )
            .await
            .unwrap_err();
        assert!(
            set_err.message.contains("local-only")
                || set_err
                    .hint
                    .as_deref()
                    .is_some_and(|hint| hint.contains("legion pair")),
            "{set_err:?}"
        );

        // `device.remove` removes a local-only line cleanly and gate immediately denies it.
        let rem = h
            .call("device.remove", json!({"name": "untracked-phone"}))
            .await
            .unwrap();
        assert_eq!(rem["removed"], json!(true));
        let auth_after = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(!auth_after.contains("legion-device:untracked-phone"));
        let gate_err = verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "untracked-phone",
            &local_only_id,
            "phone",
            None,
        )
        .unwrap_err();
        assert_eq!(gate_err.code, ErrorCode::Denied);
    }

    #[tokio::test]
    async fn paired_by_and_tombstone_quarantine_block_post_removal_descendants_until_owner_confirms()
     {
        let h = Harness::new();

        // 1. Enrol creator device A (`device:dev-a`, owner role) from a local shell.
        let inv_a = h
            .call(
                "device.invite",
                json!({ "name": "dev-a", "role": "owner", "allow_owner": true }),
            )
            .await
            .unwrap();
        let id_inv_a = inv_a["id"].as_str().unwrap().to_owned();
        let key_a = keys::ed25519_public_key_from_seed(&[0x61; 32]);
        let req_a = format!(
            "{{\"enrol\":1,\"id\":\"{id_inv_a}\",\"public_key\":\"{key_a}\",\"name\":\"dev-a\"}}\n"
        );
        let reply_a = enrol::run_enrol(&h.node, &id_inv_a, req_a.as_bytes()).await;
        assert!(reply_a.is_ok(), "{reply_a:?}");

        let devices_after_a: DevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        let id_a = devices_after_a.devices.keys().next().unwrap().clone();
        assert_eq!(
            devices_after_a.devices[&id_a].paired_by,
            Some(registry::PairedBy {
                principal: "owner".to_owned(),
                device: None,
            })
        );

        // 2. Device A (pinned to `device:dev-a` with `pairing_id = Some(id_a)`) invites and enrols
        //    child device B (`device:dev-b`, observer role).
        let mut call_b = Call::new(
            Door::Ssh,
            "device.invite",
            json!({ "name": "dev-b", "role": "observer" }),
        );
        call_b.pinned = Some("device:dev-a".to_owned());
        call_b.device = Some("dev-a".to_owned());
        call_b.pairing_id = Some(id_a.clone());
        let inv_b = h.node.call(call_b).await.unwrap();
        let id_inv_b = inv_b["id"].as_str().unwrap().to_owned();
        let key_b = keys::ed25519_public_key_from_seed(&[0x62; 32]);
        let req_b = format!(
            "{{\"enrol\":1,\"id\":\"{id_inv_b}\",\"public_key\":\"{key_b}\",\"name\":\"dev-b\"}}\n"
        );
        let reply_b = enrol::run_enrol(&h.node, &id_inv_b, req_b.as_bytes()).await;
        assert!(reply_b.is_ok(), "{reply_b:?}");

        let mut devices_file: DevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        let id_b = devices_file
            .devices
            .iter()
            .find(|(_, d)| d.name == "dev-b")
            .unwrap()
            .0
            .clone();
        assert_eq!(
            devices_file.devices[&id_b].paired_by,
            Some(registry::PairedBy {
                principal: "device:dev-a".to_owned(),
                device: Some(id_a.clone()),
            })
        );
        let b_paired_at = devices_file.devices[&id_b].paired_at.clone();

        // 3. Add a transitive grandchild device C created by B, and simulate a tombstone for A
        //    arriving with `removed_at <= B.paired_at` (A was revoked on another node before it
        //    paired B here).
        let id_c = "63".repeat(16);
        devices_file.devices.insert(
            id_c.clone(),
            registry::FleetDevice {
                name: "dev-c".to_owned(),
                public_key: keys::ed25519_public_key_from_seed(&[0x63; 32]),
                principal: "device:dev-c".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: b_paired_at.clone(),
                paired_on: "tower".to_owned(),
                paired_by: Some(registry::PairedBy {
                    principal: "device:dev-b".to_owned(),
                    device: Some(id_b.clone()),
                }),
                confirmed_by: None,
                confirmed_at: None,
                updated_at: b_paired_at.clone(),
                updated_on: "tower".to_owned(),
                name_updated_at: b_paired_at.clone(),
                name_updated_on: "tower".to_owned(),
                role_updated_at: b_paired_at.clone(),
                role_updated_on: "tower".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let dev_a = devices_file.devices.remove(&id_a).unwrap();
        let dev_a_fp = registry::DevicesFile::fingerprint_of(&dev_a);
        devices_file.removed.insert(
            id_a.clone(),
            registry::DeviceTombstone {
                name: dev_a.name,
                principal: dev_a.principal,
                fingerprint: Some(dev_a_fp),
                paired_at: Some(dev_a.paired_at),
                paired_on: Some(dev_a.paired_on),
                paired_by: dev_a.paired_by,
                removed_by: None,
                removed_at: b_paired_at.clone(),
                removed_on: "other".to_owned(),
            },
        );
        {
            let _cfg = registry::lock_config(h.node.paths()).unwrap();
            registry::save_devices_locked(h.node.paths(), &devices_file).unwrap();
            let _ = registry::reconcile_device_principals_locked(h.node.paths(), &devices_file)
                .unwrap();
        }

        // Reconciling via `device.list` removes BOTH B and C from authorized_keys and flags both.
        let listed = h.call("device.list", json!({})).await.unwrap();
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(!auth_text.contains("legion-device:dev-b"), "{auth_text}");
        assert!(!auth_text.contains("legion-device:dev-c"), "{auth_text}");

        let pol_quarantined: Policy = legion_core::config::parse(
            &h.node.paths().config_file("policy.toml"),
            &std::fs::read(h.node.paths().config_file("policy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(pol_quarantined.role_of("device:dev-b").0, "nobody");

        for name in ["dev-b", "dev-c"] {
            let entry = listed["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["name"] == json!(name))
                .expect("device must be listed");
            assert_eq!(entry["quarantined"], json!(true), "{entry}");
            assert!(
                entry["status"]
                    .as_str()
                    .is_some_and(|s| s.contains("quarantined") && s.contains("confirm=true")),
                "{entry}"
            );
        }

        // `legion gate` denies both B and C with `ErrorCode::Denied`.
        let node_file = h.node.config().get::<NodeFile>().unwrap();
        for (name, pid, principal) in [
            ("dev-b", id_b.as_str(), "device:dev-b"),
            ("dev-c", id_c.as_str(), "device:dev-c"),
        ] {
            let err = registry::verify_gate_device(
                h.node.paths(),
                "tower",
                &node_file,
                name,
                pid,
                principal,
                None,
            )
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{name}: {err:?}");
            assert!(err.message.contains("quarantined"), "{name}: {err:?}");
        }

        // `meta.doctor` flags the quarantined devices loudly.
        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        let q_check = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == json!("quarantined devices"))
            .expect("meta.doctor must report quarantined devices");
        assert_eq!(q_check["warning"], json!(true), "{q_check}");
        assert!(q_check["detail"].as_str().unwrap().contains("dev-b"));
        assert!(q_check["detail"].as_str().unwrap().contains("dev-c"));

        // Non-owner cannot confirm a quarantined device.
        let mut non_owner_confirm = Call::new(
            Door::Cli,
            "device.set",
            json!({"id": id_b, "confirm": true}),
        );
        non_owner_confirm.pinned = Some("phone".to_owned());
        let deny = h.node.call(non_owner_confirm).await.unwrap_err();
        assert_eq!(deny.code, ErrorCode::Denied, "{deny:?}");

        // Owner explicitly confirms B -> restores B and unconfirmed child C in authorized_keys and
        // restores B's `observer` role in policy.toml.
        let confirmed = h
            .call("device.set", json!({"id": id_b, "confirm": true}))
            .await
            .unwrap();
        assert_eq!(confirmed["confirmed"], json!(true), "{confirmed}");
        assert_eq!(confirmed["role"], json!("observer"), "{confirmed}");

        let auth_after = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(auth_after.contains("legion-device:dev-b"), "{auth_after}");
        assert!(auth_after.contains("legion-device:dev-c"), "{auth_after}");
        registry::verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "dev-b",
            &id_b,
            "device:dev-b",
            None,
        )
        .expect("confirmed device B must pass gate verification");
        registry::verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "dev-c",
            &id_c,
            "device:dev-c",
            None,
        )
        .expect("descendant C of confirmed B must pass gate verification");
    }

    #[tokio::test]
    async fn m1_quarantine_skew_confirm_guards_and_forwarded_invite_refusal() {
        let h = Harness::new();
        let now = Timestamp::now();
        let id_parent = "71".repeat(16);
        let id_within_skew = "72".repeat(16);
        let id_before_skew = "73".repeat(16);
        let id_preconfirmed = "74".repeat(16);

        let removed_at = now - SignedDuration::from_mins(20);
        let within_skew = removed_at - SignedDuration::from_mins(5);
        let before_skew = removed_at - SignedDuration::from_mins(15);
        let preconfirm_at = removed_at - SignedDuration::from_mins(1);

        let key_within = keys::ed25519_public_key_from_seed(&[0x72; 32]);
        let key_before = keys::ed25519_public_key_from_seed(&[0x73; 32]);
        let key_pre = keys::ed25519_public_key_from_seed(&[0x74; 32]);

        let devices_toml = format!(
            "\
version = 1

[devices.{id_within_skew}]
name = \"within-skew\"
public_key = \"{key_within}\"
principal = \"device:within-skew\"
role = \"observer\"
paired_at = \"{within_skew}\"
paired_on = \"tower\"
paired_by = {{ principal = \"device:parent\", device = \"{id_parent}\" }}
updated_at = \"{within_skew}\"
updated_on = \"tower\"

[devices.{id_before_skew}]
name = \"before-skew\"
public_key = \"{key_before}\"
principal = \"device:before-skew\"
role = \"observer\"
paired_at = \"{before_skew}\"
paired_on = \"tower\"
paired_by = {{ principal = \"device:parent\", device = \"{id_parent}\" }}
updated_at = \"{before_skew}\"
updated_on = \"tower\"

[devices.{id_preconfirmed}]
name = \"preconfirmed\"
public_key = \"{key_pre}\"
principal = \"device:preconfirmed\"
role = \"observer\"
paired_at = \"{within_skew}\"
paired_on = \"tower\"
paired_by = {{ principal = \"device:parent\", device = \"{id_parent}\" }}
confirmed_by = \"owner\"
confirmed_at = \"{preconfirm_at}\"
updated_at = \"{preconfirm_at}\"
updated_on = \"tower\"

[removed.{id_parent}]
name = \"parent\"
principal = \"device:parent\"
paired_at = \"{before_skew}\"
paired_on = \"tower\"
removed_at = \"{removed_at}\"
removed_on = \"tower\"
"
        );
        std::fs::write(h.node.paths().config_file("devices.toml"), devices_toml).unwrap();

        let listed = h.call("device.list", json!({})).await.unwrap();
        let find_dev = |name: &str| {
            listed["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["name"] == json!(name))
                .cloned()
                .unwrap_or_else(|| panic!("missing {name} in {listed}"))
        };

        // 1. Child paired 5 min before parent's removed_at (within 10 min skew) IS quarantined.
        assert_eq!(find_dev("within-skew")["quarantined"], json!(true));
        // 2. Child paired 15 min before parent's removed_at (outside 10 min skew) is NOT quarantined.
        assert_ne!(find_dev("before-skew")["quarantined"], json!(true));
        // 3. Synthetic confirmation with confirmed_at <= removed_at does NOT clear quarantine.
        assert_eq!(find_dev("preconfirmed")["quarantined"], json!(true));

        // 4. `device.set confirm=true` on a non-quarantined device is refused.
        let not_q_err = h
            .call("device.set", json!({"id": id_before_skew, "confirm": true}))
            .await
            .unwrap_err();
        assert_eq!(not_q_err.code, ErrorCode::InvalidArgs, "{not_q_err:?}");

        // 5. `device.set confirm=true` on a quarantined device records `confirmed_at > removed_at`
        //    and lifts quarantine.
        let confirmed = h
            .call("device.set", json!({"id": id_within_skew, "confirm": true}))
            .await
            .unwrap();
        assert_eq!(confirmed["confirmed"], json!(true));
        let saved_toml =
            std::fs::read_to_string(h.node.paths().config_file("devices.toml")).unwrap();
        assert!(
            saved_toml.contains("confirmed_at ="),
            "confirm=true must record confirmed_at in devices.toml:\n{saved_toml}"
        );
        let listed_after = h.call("device.list", json!({})).await.unwrap();
        let within_after = listed_after["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == json!("within-skew"))
            .unwrap();
        assert_ne!(within_after["quarantined"], json!(true));

        // 6. Forwarded `device.invite` (`hops > 0`) from an owner-role paired device is refused
        //    with a hint to connect directly.
        {
            let _cfg = registry::lock_config(h.node.paths()).unwrap();
            registry::upsert_device_principal_locked(
                h.node.paths(),
                "device:before-skew",
                "owner",
                &id_before_skew,
            )
            .unwrap();
        }
        let mut fwd_inv = Call::new(
            Door::Rpc,
            "device.invite",
            json!({"name": "via-hop", "role": "observer"}),
        );
        fwd_inv.pinned = Some("device:before-skew".to_owned());
        fwd_inv.hops = 1;
        let fwd_err = h.node.call(fwd_inv).await.unwrap_err();
        assert_eq!(fwd_err.code, ErrorCode::Denied, "{fwd_err:?}");
        assert!(
            fwd_err
                .hint
                .as_deref()
                .is_some_and(|hint| hint.to_lowercase().contains("connect")),
            "expected direct-connection hint: {fwd_err:?}"
        );
    }

    #[tokio::test]
    async fn s1_unmarked_wrong_owner_and_exceeded_device_principal_blocked_in_authkeys_gate_list_and_doctor()
     {
        let h = Harness::new();
        let id_unmarked = "91".repeat(16);
        let id_other = "92".repeat(16);
        let id_wrong_owner = "93".repeat(16);
        let id_exceeded = "94".repeat(16);
        let id_healthy = "95".repeat(16);

        let policy_path = h.node.paths().config_file("policy.toml");
        let mut policy_text = std::fs::read_to_string(&policy_path).unwrap();
        policy_text.push_str(&format!(
            "\n\
[principals.\"device:unmarked\"]
role = \"owner\"

[principals.\"device:wrong-owner\"]
role = \"owner\"
device = \"{id_other}\"

[principals.\"device:exceeded\"]
role = \"owner\"
device = \"{id_exceeded}\"

[principals.\"device:healthy\"]
role = \"observer\"
device = \"{id_healthy}\"
"
        ));
        std::fs::write(&policy_path, policy_text).unwrap();

        let now = "2026-11-24T10:00:00Z";
        let mut file = DevicesFile::default();
        for (id, seed, name, principal) in [
            (&id_unmarked, 0x91u8, "unmarked", "device:unmarked"),
            (&id_wrong_owner, 0x93u8, "wrong-owner", "device:wrong-owner"),
            (&id_exceeded, 0x94u8, "exceeded", "device:exceeded"),
            (&id_healthy, 0x95u8, "healthy", "device:healthy"),
        ] {
            file.devices.insert(
                id.clone(),
                registry::FleetDevice {
                    name: name.to_owned(),
                    public_key: keys::ed25519_public_key_from_seed(&[seed; 32]),
                    principal: principal.to_owned(),
                    role: Some("observer".to_owned()),
                    paired_at: now.to_owned(),
                    paired_on: "tower".to_owned(),
                    paired_by: None,
                    confirmed_by: None,
                    confirmed_at: None,
                    updated_at: now.to_owned(),
                    updated_on: "tower".to_owned(),
                    name_updated_at: now.to_owned(),
                    name_updated_on: "tower".to_owned(),
                    role_updated_at: now.to_owned(),
                    role_updated_on: "tower".to_owned(),
                    conflict: false,
                    role_conflict: false,
                },
            );
        }
        std::fs::write(
            h.node.paths().config_file("devices.toml"),
            file.to_toml_string(),
        )
        .unwrap();

        // Pre-seed an authorized_keys line for `unmarked` so we verify reconcile strips it too.
        let exe = legion_core::current_exe().unwrap();
        let shell = authkeys::ForcedCommandShell::host().unwrap();
        let pinned_home = authkeys::PinnedHome::from_paths(h.node.paths());
        let pre_line = authkeys::format_device_line(
            &exe,
            "device:unmarked",
            &file.devices[&id_unmarked].public_key,
            "unmarked",
            &id_unmarked,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        std::fs::write(&h.auth_keys, format!("{pre_line}\n")).unwrap();

        // 1. `device.list` writes NO authorized_keys line for unmarked/wrong-owner/exceeded,
        //    writes the line for healthy, sets status, and returns warnings.
        let listed = h.call("device.list", json!({})).await.unwrap();
        let auth_text = std::fs::read_to_string(&h.auth_keys).unwrap();
        assert!(!auth_text.contains("legion-device:unmarked"), "{auth_text}");
        assert!(
            !auth_text.contains("legion-device:wrong-owner"),
            "{auth_text}"
        );
        assert!(!auth_text.contains("legion-device:exceeded"), "{auth_text}");
        assert!(auth_text.contains("legion-device:healthy"), "{auth_text}");

        let warnings = listed["warnings"].as_array().unwrap();
        for name in ["unmarked", "wrong-owner", "exceeded"] {
            let entry = listed["devices"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["name"] == json!(name))
                .unwrap();
            assert!(
                entry["status"]
                    .as_str()
                    .is_some_and(|s| s.contains("not active here")),
                "{name}: expected not-active status, got {entry}"
            );
            assert!(
                warnings
                    .iter()
                    .any(|w| w.as_str().is_some_and(|s| s.contains(name))),
                "{name}: expected warning in {warnings:?}"
            );
        }
        let healthy_entry = listed["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == json!("healthy"))
            .unwrap();
        assert!(healthy_entry.get("status").is_none(), "{healthy_entry}");

        // 2. `legion gate` denies unmarked, wrong-owner, and exceeded (even if a line exists on
        //    disk), and allows healthy.
        let node_file = h.node.config().get::<NodeFile>().unwrap();
        for (name, pid, principal) in [
            ("unmarked", id_unmarked.as_str(), "device:unmarked"),
            ("wrong-owner", id_wrong_owner.as_str(), "device:wrong-owner"),
            ("exceeded", id_exceeded.as_str(), "device:exceeded"),
        ] {
            let err = registry::verify_gate_device(
                h.node.paths(),
                "tower",
                &node_file,
                name,
                pid,
                principal,
                None,
            )
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{name}: {err:?}");
        }
        registry::verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "healthy",
            &id_healthy,
            "device:healthy",
            None,
        )
        .expect("healthy device must pass gate");

        // 3. `meta.doctor` flags all three blocked devices.
        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        let conflict_check = doctor["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == json!("device principal conflicts"))
            .expect("meta.doctor must report device principal conflicts");
        assert_eq!(conflict_check["warning"], json!(true), "{conflict_check}");
        let detail = conflict_check["detail"].as_str().unwrap();
        assert!(detail.contains("unmarked"), "{detail}");
        assert!(detail.contains("wrong-owner"), "{detail}");
        assert!(detail.contains("exceeded"), "{detail}");
    }

    #[tokio::test]
    async fn s2_enrol_saves_config_first_leaves_authkeys_untouched_on_save_failure_and_rolls_back_on_swap_failure()
     {
        let h = Harness::new();

        // 1. Failing `devices.toml` save (devices.toml exists and reads cleanly, policy.toml
        //    updates cleanly, but writing `state/sync/backups/devices.toml.bak` fails because a
        //    directory sits at the backup path) must fail BEFORE touching authorized_keys and must
        //    roll back the newly created policy.toml principal.
        let devices_path = h.node.paths().config_file("devices.toml");
        std::fs::write(&devices_path, "version = 1\n").unwrap();
        let inv1 = h
            .call(
                "device.invite",
                json!({"name": "fail-save", "role": "observer", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv1_id = inv1["id"].as_str().unwrap().to_owned();
        let auth_before_1 = std::fs::read(&h.auth_keys).unwrap();
        let policy_before_1 = std::fs::read(h.node.paths().config_file("policy.toml")).unwrap();

        let devices_bak = h
            .node
            .paths()
            .state_dir()
            .join("sync")
            .join("backups")
            .join("devices.toml.bak");
        std::fs::create_dir_all(&devices_bak).unwrap();

        let key1 = keys::ed25519_public_key_from_seed(&[0xa5; 32]);
        let req1 = format!(
            "{{\"enrol\":1,\"id\":\"{inv1_id}\",\"public_key\":\"{key1}\",\"name\":\"fail-save\"}}\n"
        );
        let reply1 = enrol::run_enrol(&h.node, &inv1_id, req1.as_bytes()).await;
        std::fs::remove_dir(&devices_bak).unwrap();

        assert!(
            !reply1.is_ok(),
            "enrol must fail when devices.toml save fails: {reply1:?}"
        );
        assert_eq!(
            std::fs::read(&h.auth_keys).unwrap(),
            auth_before_1,
            "authorized_keys must be untouched when devices.toml save fails"
        );
        assert_eq!(
            std::fs::read(h.node.paths().config_file("policy.toml")).unwrap(),
            policy_before_1,
            "policy.toml must be rolled back when devices.toml save fails"
        );
        assert_eq!(
            std::fs::read_to_string(&devices_path).unwrap(),
            "version = 1\n",
            "devices.toml must stay unchanged when its save fails"
        );

        // 2. Failing `authorized_keys` swap (on Unix, unwritable parent dir after lock file exists)
        //    must roll back BOTH devices.toml AND policy.toml to their exact pre-enrol state.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::remove_file(&devices_path).unwrap();
            let inv2 = h
                .call(
                    "device.invite",
                    json!({"name": "fail-swap", "role": "observer", "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap();
            let inv2_id = inv2["id"].as_str().unwrap().to_owned();
            let auth_before_2 = std::fs::read(&h.auth_keys).unwrap();
            let policy_before_2 = std::fs::read(h.node.paths().config_file("policy.toml")).unwrap();
            assert!(!devices_path.exists());

            let auth_dir = h.auth_keys.parent().unwrap().to_path_buf();
            std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

            let key2 = keys::ed25519_public_key_from_seed(&[0xa6; 32]);
            let req2 = format!(
                "{{\"enrol\":1,\"id\":\"{inv2_id}\",\"public_key\":\"{key2}\",\"name\":\"fail-swap\"}}\n"
            );
            let reply2 = enrol::run_enrol(&h.node, &inv2_id, req2.as_bytes()).await;
            std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

            assert!(
                !reply2.is_ok(),
                "enrol must fail when authorized_keys swap fails: {reply2:?}"
            );
            assert!(
                !devices_path.exists(),
                "devices.toml must be rolled back to absent when authorized_keys swap fails"
            );
            assert_eq!(
                std::fs::read(h.node.paths().config_file("policy.toml")).unwrap(),
                policy_before_2,
                "policy.toml must be rolled back when authorized_keys swap fails"
            );
            assert_eq!(
                std::fs::read(&h.auth_keys).unwrap(),
                auth_before_2,
                "authorized_keys must stay unchanged when swap fails"
            );
        }
    }

    #[tokio::test]
    async fn nit_removing_local_only_device_does_not_push_to_fleet_or_report_still_trusted_by() {
        use crate::device::authkeys::{ForcedCommandShell, PinnedHome, format_device_line};
        use crate::device::keys::ed25519_public_key_from_seed;

        let h = Harness::new();
        // Configure another fleet machine (`pi`) so any fleet push attempt produces a `fleet` entry
        // and `still_trusted_by = ["pi"]`.
        std::fs::write(
            h.node.paths().config_file("fleet.toml"),
            r#"
[machines.tower]
default = "linux"
[machines.tower.systems.linux]
host = "tower.example"

[machines.pi]
default = "linux"
[machines.pi.systems.linux]
host = "pi.example"
"#,
        )
        .unwrap();
        // Empty devices.toml ensures lines present only in authorized_keys are local-only.
        std::fs::write(h.node.paths().config_file("devices.toml"), "version = 1\n").unwrap();

        let exe = legion_core::current_exe().unwrap();
        let shell = ForcedCommandShell::host().unwrap();
        let pinned_home = PinnedHome::from_paths(h.node.paths());
        let id_by_name = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6";
        let id_by_id = "f6e5d4c3b2a1f6e5d4c3b2a1f6e5d4c3";
        let line_by_name = format_device_line(
            &exe,
            "phone",
            &ed25519_public_key_from_seed(&[0x71; 32]),
            "local-name",
            id_by_name,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        let line_by_id = format_device_line(
            &exe,
            "phone",
            &ed25519_public_key_from_seed(&[0x72; 32]),
            "local-id",
            id_by_id,
            &pinned_home,
            None,
            shell,
        )
        .unwrap();
        std::fs::write(&h.auth_keys, format!("{line_by_name}\n{line_by_id}\n")).unwrap();

        // 1. Remove local-only device by name: must not push to fleet or report still_trusted_by.
        let removed_name = h
            .call("device.remove", json!({"name": "local-name"}))
            .await
            .unwrap();
        assert_eq!(removed_name["removed"], json!(true));
        assert_eq!(removed_name["lines_removed"], json!(1));
        assert!(
            removed_name.get("fleet").is_none(),
            "local-only removal by name must not push to fleet: {removed_name}"
        );
        assert!(
            removed_name.get("still_trusted_by").is_none(),
            "local-only removal by name must not list fleet machines as still trusting it: {removed_name}"
        );
        assert!(
            removed_name.get("hint").is_none(),
            "local-only removal by name must not emit a fleet sync hint: {removed_name}"
        );

        // 2. Remove local-only device by pairing id: same guarantee.
        let removed_id = h
            .call("device.remove", json!({"id": id_by_id}))
            .await
            .unwrap();
        assert_eq!(removed_id["removed"], json!(true));
        assert_eq!(removed_id["lines_removed"], json!(1));
        assert!(
            removed_id.get("fleet").is_none(),
            "local-only removal by id must not push to fleet: {removed_id}"
        );
        assert!(
            removed_id.get("still_trusted_by").is_none(),
            "local-only removal by id must not list fleet machines as still trusting it: {removed_id}"
        );
        assert!(
            removed_id.get("hint").is_none(),
            "local-only removal by id must not emit a fleet sync hint: {removed_id}"
        );
    }

    #[tokio::test]
    async fn size_limits_cap_non_owner_pending_invites_and_stop_create_remove_growth_at_half_limit()
    {
        use crate::device::enrol::run_enrol;
        use crate::device::keys::ed25519_public_key_from_seed;
        use crate::device::registry::{DeviceTombstone, DevicesFile, PairedBy};

        let h = Harness::new();
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            "\
default = \"owner\"

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"ops\"
",
        )
        .unwrap();

        // 1. Non-owner (`tablet`, whose `ops` role grants `effect:admin`) cannot issue invites at
        //    all under the owner-strength guard, even with `allow_owner=true`.
        for allow_owner in [false, true] {
            let mut call = Call::new(
                Door::Cli,
                "device.invite",
                json!({
                    "principal": "tablet",
                    "allow_owner": allow_owner,
                    "host": "127.0.0.1",
                    "port": 22,
                    "user": "tester"
                }),
            );
            call.pinned = Some("tablet".to_owned());
            let err = h
                .node
                .call(call)
                .await
                .expect_err("non-owner owner-strength invite must be refused");
            assert_eq!(err.code, ErrorCode::Denied, "{err:?}");
            assert!(
                err.message.contains("ops")
                    && err.message.contains(crate::roles::OWNER_STRENGTH_REASON),
                "{err:?}"
            );
        }

        // 2. Non-owner (`tablet`) device removal is still capped at the 512 KiB half-limit once
        //    `devices.toml` grows past 512 KiB, while owner can still enrol (with
        //    `allow_owner=true`) and remove cleanly!
        let half_limit: usize = 512 * 1024;
        let now_str = registry::format_precise_rfc3339(Timestamp::now());
        let make_tomb = |idx: u32| DeviceTombstone {
            name: format!("past-device-{idx:04}"),
            principal: "tablet".to_owned(),
            fingerprint: Some("SHA256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned()),
            paired_at: Some(now_str.clone()),
            paired_on: Some("tower".to_owned()),
            paired_by: Some(PairedBy {
                principal: "owner".to_owned(),
                device: None,
            }),
            removed_by: Some(PairedBy {
                principal: "tablet".to_owned(),
                device: None,
            }),
            removed_at: now_str.clone(),
            removed_on: "tower".to_owned(),
        };
        let mut sample = DevicesFile::default();
        sample
            .removed
            .insert(format!("f{:031x}", 0u32), make_tomb(0));
        let per_entry = sample
            .to_toml_string()
            .len()
            .saturating_sub("version = 1\n".len())
            .max(1);
        let initial_count = ((half_limit.saturating_sub(1000)) / per_entry) as u32;
        let mut near_half = DevicesFile::default();
        for seed_idx in 0..initial_count {
            near_half
                .removed
                .insert(format!("f{seed_idx:031x}"), make_tomb(seed_idx));
        }
        let mut seed_idx = initial_count;
        while near_half.to_toml_string().len() + 600 < half_limit {
            near_half
                .removed
                .insert(format!("f{seed_idx:031x}"), make_tomb(seed_idx));
            seed_idx += 1;
        }
        std::fs::write(
            h.node.paths().config_file(DevicesFile::FILE),
            near_half.to_toml_string(),
        )
        .unwrap();

        // Owner enrols two devices for `tablet` (with `allow_owner=true`) so the file is strictly
        // past 512 KiB even after turning one into a tombstone; then non-owner `tablet` removal
        // must be refused with the honest owner size-limit hint while owner removal succeeds.
        for (seed, name) in [
            (0xee_u8, "owner-seeded-tablet-1"),
            (0xef_u8, "owner-seeded-tablet-2"),
        ] {
            let inv = h
                .call(
                    "device.invite",
                    json!({
                        "principal": "tablet",
                        "allow_owner": true,
                        "host": "127.0.0.1",
                        "port": 22,
                        "user": "tester"
                    }),
                )
                .await
                .unwrap();
            let inv_id = inv["id"].as_str().unwrap().to_owned();
            let req = format!(
                "{}\n",
                json!({"enrol": 1, "id": inv_id, "public_key": ed25519_public_key_from_seed(&[seed; 32]), "name": name})
            );
            let reply = run_enrol(&h.node, &inv_id, req.as_bytes()).await;
            assert!(
                reply.is_ok(),
                "owner enrol past 512 KiB must still succeed: {reply:?}"
            );
        }

        let mut rm_call = Call::new(
            Door::Cli,
            "device.remove",
            json!({"name": "owner-seeded-tablet-2"}),
        );
        rm_call.pinned = Some("tablet".to_owned());
        let rm_err = h
            .node
            .call(rm_call)
            .await
            .expect_err("non-owner remove past 512 KiB must be refused");
        assert_eq!(rm_err.code, ErrorCode::Denied, "{rm_err:?}");
        assert!(
            rm_err
                .hint
                .as_deref()
                .is_some_and(|hint| hint.to_ascii_lowercase().contains("owner")
                    && hint.contains("device.remove")
                    && !hint.to_ascii_lowercase().contains("tidy")),
            "expected honest owner size-limit hint: {rm_err:?}"
        );

        let owner_rm = h
            .call("device.remove", json!({"name": "owner-seeded-tablet-2"}))
            .await
            .unwrap();
        assert_eq!(owner_rm["removed"], json!(true));
    }

    #[tokio::test]
    async fn nit3_device_set_role_restores_policy_snapshot_when_devices_save_fails() {
        use crate::device::enrol::run_enrol;
        use crate::device::keys::ed25519_public_key_from_seed;

        let h = Harness::new();
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[roles.editor]
allow = [\"effect:read\", \"effect:write\"]
",
        )
        .unwrap();

        let inv = h
            .call(
                "device.invite",
                json!({
                    "name": "pixel",
                    "role": "observer",
                    "host": "127.0.0.1",
                    "port": 22,
                    "user": "tester"
                }),
            )
            .await
            .unwrap();
        let inv_id = inv["id"].as_str().unwrap().to_owned();
        let req = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": inv_id,
                "public_key": ed25519_public_key_from_seed(&[0x33; 32]),
                "name": "pixel"
            })
        );
        assert!(run_enrol(&h.node, &inv_id, req.as_bytes()).await.is_ok());

        let policy_path = h.node.paths().config_file("policy.toml");
        let policy_before = std::fs::read(&policy_path).unwrap();
        let devices_before = std::fs::read(h.node.paths().config_file("devices.toml")).unwrap();

        // Force `save_devices_locked` to fail AFTER `upsert_device_principal_locked` has already
        // written the new `editor` role into `policy.toml` by placing a directory at the backup
        // path `state/sync/backups/devices.toml.bak`.
        let devices_bak = h
            .node
            .paths()
            .state_dir()
            .join("sync")
            .join("backups")
            .join("devices.toml.bak");
        let _ = std::fs::remove_file(&devices_bak);
        std::fs::create_dir_all(&devices_bak).unwrap();

        let err = h
            .call("device.set", json!({"name": "pixel", "role": "editor"}))
            .await
            .expect_err("device.set must fail when saving devices.toml fails");
        let _ = std::fs::remove_dir_all(&devices_bak);
        assert_eq!(err.code, ErrorCode::Failed, "{err:?}");

        assert_eq!(
            std::fs::read(&policy_path).unwrap(),
            policy_before,
            "policy.toml must be rolled back to its snapshot when devices.toml save fails"
        );
        assert_eq!(
            std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
            devices_before
        );
    }

    #[tokio::test]
    async fn nit4_failed_rollback_reports_loudly_in_error_and_meta_doctor() {
        let h = Harness::new();
        // Create a regular file where a parent directory is expected so restoring a snapshot
        // underneath it fails deterministically across Linux, macOS, Windows, and root.
        let blocker = h.dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"blocker").unwrap();
        let unwritable_target = blocker.join("policy.toml");

        let rollback_err = registry::restore_file_snapshot(
            h.node.paths(),
            "device.set",
            &unwritable_target,
            Some(b"default = \"owner\"\n"),
        )
        .expect_err("restore_file_snapshot must return Err when snapshot restoration fails");
        assert!(
            rollback_err.contains("ROLLBACK FAILED")
                && rollback_err.contains("device.set")
                && rollback_err.contains("policy.toml"),
            "expected loud rollback error: {rollback_err}"
        );

        let attached = registry::attach_rollback_failures(
            ErrorBody::new(ErrorCode::Failed, "devices.toml cannot be written."),
            &[rollback_err],
        );
        assert!(
            attached.message.contains("ROLLBACK FAILED")
                && attached.message.contains("policy.toml"),
            "attached error must loudly surface rollback failure: {attached:?}"
        );
        assert!(
            attached
                .hint
                .as_deref()
                .is_some_and(|hint| hint.contains("meta.doctor")),
            "attached error hint should point to meta.doctor: {attached:?}"
        );

        // `meta.doctor` MUST report a failing check (`ok: false`) for the recorded rollback failure.
        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        let checks = doctor["checks"].as_array().unwrap();
        let rollback_check = checks
            .iter()
            .find(|c| c["name"] == json!("config rollback failures"))
            .unwrap_or_else(|| panic!("expected config rollback failures check in {doctor}"));
        assert_eq!(doctor["ok"], json!(false), "{doctor}");
        assert_eq!(rollback_check["ok"], json!(false), "{rollback_check}");
        assert!(rollback_check.get("warning").is_none(), "{rollback_check}");
        assert!(
            rollback_check["detail"]
                .as_str()
                .is_some_and(|d| d.contains("device.set") && d.contains("policy.toml")),
            "{rollback_check}"
        );
    }

    #[tokio::test]
    async fn only_device_set_role_writes_role_stamp_and_conflicted_device_is_blocked() {
        let h = Harness::new();
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[roles.editor]
allow = [\"effect:read\", \"effect:write\"]
",
        )
        .unwrap();

        // 1. Enrol `phone` with role `observer`. Both `name_updated_*` and `role_updated_*`
        //    start equal to `(paired_at, paired_on)`.
        let inv = h
            .call(
                "device.invite",
                json!({"name": "phone", "role": "observer"}),
            )
            .await
            .unwrap();
        let inv_id = inv["id"].as_str().unwrap().to_owned();
        let key = keys::ed25519_public_key_from_seed(&[0xe1; 32]);
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{inv_id}\",\"public_key\":\"{key}\",\"name\":\"phone\"}}\n"
        );
        let reply = enrol::run_enrol(&h.node, &inv_id, req.as_bytes()).await;
        assert!(reply.is_ok(), "{reply:?}");

        let load_devices = || -> DevicesFile {
            legion_core::config::parse(
                &h.node.paths().config_file("devices.toml"),
                &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
            )
            .unwrap()
        };
        let initial = load_devices();
        let id = initial.devices.keys().next().unwrap().clone();
        let dev0 = &initial.devices[&id];
        assert_eq!(dev0.name_updated_at, dev0.paired_at);
        assert_eq!(dev0.name_updated_on, dev0.paired_on);
        assert_eq!(dev0.role_updated_at, dev0.paired_at);
        assert_eq!(dev0.role_updated_on, dev0.paired_on);

        // 2. Rename ONLY bumps `name_updated_*` and MUST NOT touch `role_updated_*`.
        h.call("device.set", json!({"id": id, "rename": "phone-2"}))
            .await
            .unwrap();
        let after_rename = load_devices();
        let dev1 = &after_rename.devices[&id];
        assert_eq!(dev1.name, "phone-2");
        assert_ne!(dev1.name_updated_at, dev0.name_updated_at);
        assert_eq!(
            dev1.role_updated_at, dev0.role_updated_at,
            "rename must not touch role_updated_at"
        );
        assert_eq!(
            dev1.role_updated_on, dev0.role_updated_on,
            "rename must not touch role_updated_on"
        );

        // 3. `device.list` MUST NOT touch `name_updated_*` or `role_updated_*`.
        h.call("device.list", json!({})).await.unwrap();
        let after_list = load_devices();
        assert_eq!(after_list.devices[&id], *dev1);

        // 4. `device.set confirm=true` ONLY bumps `confirmed_at` / `updated_at` and MUST NOT
        //    touch `name_updated_*` or `role_updated_*`.
        let parent_id = "aa".repeat(16);
        let mut quarantined_file = after_list.clone();
        quarantined_file.devices.get_mut(&id).unwrap().paired_by = Some(registry::PairedBy {
            principal: "owner".to_owned(),
            device: Some(parent_id.clone()),
        });
        quarantined_file.removed.insert(
            parent_id,
            registry::DeviceTombstone {
                name: "old-parent".to_owned(),
                principal: "owner".to_owned(),
                fingerprint: None,
                paired_at: None,
                paired_on: None,
                paired_by: None,
                removed_by: None,
                removed_at: dev0.paired_at.clone(),
                removed_on: "tower".to_owned(),
            },
        );
        std::fs::write(
            h.node.paths().config_file("devices.toml"),
            quarantined_file.to_toml_string(),
        )
        .unwrap();
        h.call("device.set", json!({"id": id, "confirm": true}))
            .await
            .unwrap();
        let after_confirm = load_devices();
        let dev2 = &after_confirm.devices[&id];
        assert!(dev2.confirmed_at.is_some());
        assert_eq!(
            dev2.name_updated_at, dev1.name_updated_at,
            "confirm=true must not touch name_updated_at"
        );
        assert_eq!(
            dev2.role_updated_at, dev0.role_updated_at,
            "confirm=true must not touch role_updated_at"
        );

        // 5. `device.set role=editor` ONLY bumps `role_updated_*` and MUST NOT touch `name_updated_*`.
        h.call("device.set", json!({"id": id, "role": "editor"}))
            .await
            .unwrap();
        let after_role = load_devices();
        let dev3 = &after_role.devices[&id];
        assert_eq!(dev3.role.as_deref(), Some("editor"));
        assert_ne!(dev3.role_updated_at, dev0.role_updated_at);
        assert_eq!(
            dev3.name_updated_at, dev1.name_updated_at,
            "role change must not touch name_updated_at"
        );

        // 6. Conflicted entry: `device.list` flags it & warns, `authorized_keys` has no line,
        //    `device.set` refuses with Conflict, `meta.doctor` flags it, and `device.remove` clears it.
        let mut conflicted = after_role.clone();
        let c = conflicted.devices.get_mut(&id).unwrap();
        c.conflict = true;
        c.confirmed_by = None;
        c.confirmed_at = None;
        std::fs::write(
            h.node.paths().config_file("devices.toml"),
            conflicted.to_toml_string(),
        )
        .unwrap();

        let listed = h.call("device.list", json!({})).await.unwrap();
        let item = listed["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap();
        assert!(
            item["status"]
                .as_str()
                .is_some_and(|s| s.contains("conflicting immutable")),
            "{item}"
        );
        assert!(
            listed["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w.as_str().is_some_and(|s| s.contains(&id))),
            "{listed}"
        );
        assert!(
            !std::fs::read_to_string(&h.auth_keys).unwrap().contains(&id),
            "conflicted device must have no authorized_keys line"
        );

        let set_err = h
            .call("device.set", json!({"id": id, "rename": "phone-3"}))
            .await
            .unwrap_err();
        assert_eq!(set_err.code, ErrorCode::Conflict, "{set_err:?}");

        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        assert!(
            doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == json!("conflicting device entries")),
            "{doctor}"
        );

        let rem = h.call("device.remove", json!({"id": id})).await.unwrap();
        assert_eq!(rem["removed"], json!(true), "{rem}");
    }

    #[tokio::test]
    async fn role_tie_conflict_is_blocked_until_owner_runs_device_set_role_while_immutable_conflict_stays_refused()
     {
        let h = Harness::new();
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[roles.editor]
allow = [\"effect:read\", \"effect:write\"]

[roles.helper]
allow = [\"device.*\"]

[principals.helper_user]
role = \"helper\"
",
        )
        .unwrap();

        let invite = h
            .call(
                "device.invite",
                json!({"name": "tablet", "role": "observer"}),
            )
            .await
            .unwrap();
        let invite_id = invite["id"].as_str().unwrap().to_owned();
        let device_key = keys::ed25519_public_key_from_seed(&[0x59; 32]);
        let req = format!(
            "{{\"enrol\":1,\"id\":\"{invite_id}\",\"public_key\":\"{device_key}\",\"name\":\"tablet\"}}\n"
        );
        let enrol_reply = enrol::run_enrol(&h.node, &invite_id, req.as_bytes()).await;
        assert!(enrol_reply.is_ok(), "{enrol_reply:?}");

        // Produce a genuine equal-stamp, equal-origin role tie via merge_devices.
        let base_bytes = std::fs::read(h.node.paths().config_file("devices.toml")).unwrap();
        let side_a: DevicesFile =
            legion_core::config::parse(&h.node.paths().config_file("devices.toml"), &base_bytes)
                .unwrap();
        let id = side_a.devices.keys().next().unwrap().clone();
        let node_file = h.node.config().get::<NodeFile>().unwrap();
        registry::verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "tablet",
            &id,
            "device:tablet",
            None,
        )
        .unwrap();
        let mut side_b = side_a.clone();
        side_b.devices.get_mut(&id).unwrap().role = Some("editor".to_owned());
        let tied = registry::merge_devices(&side_a, &side_b);
        std::fs::write(
            h.node.paths().config_file("devices.toml"),
            tied.to_toml_string(),
        )
        .unwrap();
        {
            let _lock = registry::lock_config(h.node.paths()).unwrap();
            let _ = registry::reconcile_device_principals_locked(h.node.paths(), &tied).unwrap();
        }

        // 1. `device.list` flags the role conflict, warns, and strips authorized_keys;
        //    `meta.doctor` flags it too.
        let listed = h.call("device.list", json!({})).await.unwrap();
        let item = listed["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap();
        assert!(
            item["status"]
                .as_str()
                .is_some_and(|s| s.contains("device.set") && s.contains("role=")),
            "expected role-conflict status with device.set role= hint: {item}"
        );
        assert!(
            !std::fs::read_to_string(&h.auth_keys).unwrap().contains(&id),
            "role-conflicted device must have no authorized_keys line"
        );
        let doctor = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        assert!(
            doctor["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == json!("role-conflicting device entries")),
            "expected meta.doctor role-conflict check: {doctor}"
        );

        // 2. Rename-only `device.set` without `role=` is REFUSED with Conflict.
        let rename_err = h
            .call("device.set", json!({"id": id, "rename": "tablet-2"}))
            .await
            .unwrap_err();
        assert_eq!(rename_err.code, ErrorCode::Conflict, "{rename_err:?}");

        // 3. Non-owner `device.set role=` is REFUSED with Denied.
        let non_owner_err = h
            .call_as(
                "device.set",
                "helper_user",
                json!({"id": id, "role": "observer"}),
            )
            .await
            .unwrap_err();
        assert_eq!(non_owner_err.code, ErrorCode::Denied, "{non_owner_err:?}");

        // 4. Owner `device.set id=<id> role=editor` resolves the role conflict, restores policy
        //    and authorized_keys, and clears device.list + meta.doctor warnings!
        let resolved = h
            .call("device.set", json!({"id": id, "role": "editor"}))
            .await
            .unwrap();
        assert_eq!(resolved["role"], json!("editor"), "{resolved}");
        let pol: Policy = legion_core::config::parse(
            &h.node.paths().config_file("policy.toml"),
            &std::fs::read(h.node.paths().config_file("policy.toml")).unwrap(),
        )
        .unwrap();
        assert_eq!(pol.role_of("device:tablet").0, "editor");
        assert!(
            std::fs::read_to_string(&h.auth_keys).unwrap().contains(&id),
            "resolving role conflict via owner device.set role= must restore authorized_keys line"
        );
        let listed_after = h.call("device.list", json!({})).await.unwrap();
        let item_after = listed_after["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap();
        assert!(
            item_after.get("status").is_none(),
            "resolved device should have no conflict status: {item_after}"
        );
        let doctor_after = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        assert!(
            !doctor_after["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == json!("role-conflicting device entries")),
            "meta.doctor must clear role-conflict warning after resolution: {doctor_after}"
        );

        // 5. Immutable conflict MUST still refuse even owner `device.set role=`.
        let mut imm_conflicted: DevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            &std::fs::read(h.node.paths().config_file("devices.toml")).unwrap(),
        )
        .unwrap();
        imm_conflicted.devices.get_mut(&id).unwrap().conflict = true;
        std::fs::write(
            h.node.paths().config_file("devices.toml"),
            imm_conflicted.to_toml_string(),
        )
        .unwrap();
        let imm_err = h
            .call("device.set", json!({"id": id, "role": "observer"}))
            .await
            .unwrap_err();
        assert_eq!(imm_err.code, ErrorCode::Conflict, "{imm_err:?}");
    }

    #[tokio::test]
    async fn f2_owner_remove_succeeds_at_one_mib_and_live_addition_respects_tombstone_reserve() {
        use crate::device::enrol::run_enrol;
        use crate::device::keys::ed25519_public_key_from_seed;
        use crate::device::registry::{DeviceTombstone, FleetDevice, MAX_DEVICES_TOML_BYTES};

        let h = Harness::new();
        let long_node = "n".repeat(63);
        let orig_node_toml =
            std::fs::read_to_string(h.node.paths().config_file("node.toml")).unwrap();
        std::fs::write(
            h.node.paths().config_file("node.toml"),
            orig_node_toml.replace("name = \"tower\"", &format!("name = \"{long_node}\"")),
        )
        .unwrap();
        let long_owner = format!("owner-{}", "o".repeat(120));
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            format!(
                "\
default = \"owner\"

[roles.observer]
allow = [\"effect:read\"]

[principals.\"{long_owner}\"]
role = \"owner\"
"
            ),
        )
        .unwrap();

        let ts = "2026-11-24T10:00:00Z";
        let live_id = "aa".repeat(16);
        let live_dev = FleetDevice {
            name: "x".to_owned(),
            public_key: ed25519_public_key_from_seed(&[0xaa; 32]),
            principal: "owner".to_owned(),
            role: None,
            paired_at: ts.to_owned(),
            paired_on: "a".to_owned(),
            paired_by: None,
            confirmed_by: None,
            confirmed_at: None,
            updated_at: ts.to_owned(),
            updated_on: "a".to_owned(),
            name_updated_at: ts.to_owned(),
            name_updated_on: "a".to_owned(),
            role_updated_at: ts.to_owned(),
            role_updated_on: "a".to_owned(),
            conflict: false,
            role_conflict: false,
        };
        let make_tomb = |idx: u32, pad_len: usize| DeviceTombstone {
            name: format!("d{idx:04}-{}", "p".repeat(pad_len)),
            principal: "owner".to_owned(),
            fingerprint: Some("SHA256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
            paired_at: Some(ts.to_owned()),
            paired_on: Some("tower".to_owned()),
            paired_by: None,
            removed_by: None,
            removed_at: ts.to_owned(),
            removed_on: "tower".to_owned(),
        };

        // 1. Seed devices.toml right under 1 MiB (within ~64 bytes of MAX_DEVICES_TOML_BYTES)
        //    containing live device `x`. Removing `x` as owner (producing a larger tombstone with
        //    fingerprint + long removed_by + 63-char removed_on) pushes raw bytes > 1 MiB, and
        //    MUST STILL SUCCEED!
        let per_tomb = {
            let mut one = DevicesFile::default();
            one.removed
                .insert(format!("f{:031x}", 0u32), make_tomb(0, 100));
            one.to_toml_string().len() - "version = 1\n".len()
        };
        let mut near_one_mib = DevicesFile::default();
        near_one_mib.devices.insert(live_id.clone(), live_dev);
        let init_count = ((MAX_DEVICES_TOML_BYTES - 2500) / per_tomb) as u32;
        for i in 0..init_count {
            near_one_mib
                .removed
                .insert(format!("f{i:031x}"), make_tomb(i, 100));
        }
        let mut idx = init_count;
        loop {
            let mut probe = near_one_mib.clone();
            probe
                .removed
                .insert(format!("f{idx:031x}"), make_tomb(idx, 100));
            if probe.to_toml_string().len() > MAX_DEVICES_TOML_BYTES - 600 {
                break;
            }
            near_one_mib = probe;
            idx += 1;
        }
        // Top off with a minimal-overhead tombstone (no fingerprint/paired_at/paired_on) padded
        // right up to `MAX_DEVICES_TOML_BYTES - 10`.
        let make_bare_tomb = |i: u32, pad: usize| DeviceTombstone {
            name: format!("b{i:04}-{}", "p".repeat(pad)),
            principal: "owner".to_owned(),
            fingerprint: None,
            paired_at: None,
            paired_on: None,
            paired_by: None,
            removed_by: None,
            removed_at: ts.to_owned(),
            removed_on: "a".to_owned(),
        };
        let bare_len = {
            let mut probe = near_one_mib.clone();
            probe
                .removed
                .insert(format!("f{idx:031x}"), make_bare_tomb(idx, 0));
            probe.to_toml_string().len() - near_one_mib.to_toml_string().len()
        };
        while MAX_DEVICES_TOML_BYTES - near_one_mib.to_toml_string().len() > bare_len + 115 {
            near_one_mib
                .removed
                .insert(format!("f{idx:031x}"), make_bare_tomb(idx, 10));
            idx += 1;
        }
        let remaining = MAX_DEVICES_TOML_BYTES - near_one_mib.to_toml_string().len();
        if remaining > bare_len + 10 {
            let pad = (remaining - bare_len - 10).min(115);
            near_one_mib
                .removed
                .insert(format!("f{idx:031x}"), make_bare_tomb(idx, pad));
        }
        let seeded_text = near_one_mib.to_toml_string();
        assert!(
            seeded_text.len() <= MAX_DEVICES_TOML_BYTES
                && seeded_text.len() > MAX_DEVICES_TOML_BYTES - 64,
            "seeded size = {}",
            seeded_text.len()
        );
        std::fs::write(h.node.paths().config_file(DevicesFile::FILE), &seeded_text).unwrap();

        let mut rm_call = Call::new(Door::Cli, "device.remove", json!({"id": live_id}));
        rm_call.pinned = Some(long_owner.clone());
        rm_call.pairing_id = Some("bb".repeat(16));
        let rm_res = h
            .node
            .call(rm_call)
            .await
            .expect("owner device.remove MUST succeed even when file sits right at 1 MiB");
        assert_eq!(rm_res["removed"], json!(true), "{rm_res}");
        let after_rm_bytes = std::fs::read(h.node.paths().config_file(DevicesFile::FILE)).unwrap();
        assert!(
            after_rm_bytes.len() > MAX_DEVICES_TOML_BYTES,
            "test precondition: resulting tombstone file should have crossed 1 MiB (got {})",
            after_rm_bytes.len()
        );

        // 2. Seed devices.toml with ~700 bytes of raw headroom (< WORST_CASE_TOMBSTONE_BYTES,
        //    > actual live entry size of ~450 bytes). Adding a new live device fits raw < 1 MiB
        //    but breaks the tombstone reserve and MUST be refused with an honest hint!
        let mut reserve_full = DevicesFile::default();
        let reserve_init = ((MAX_DEVICES_TOML_BYTES - 2500) / per_tomb) as u32;
        for i in 0..reserve_init {
            reserve_full
                .removed
                .insert(format!("e{i:031x}"), make_tomb(i, 100));
        }
        let mut r_idx = reserve_init;
        loop {
            let mut probe = reserve_full.clone();
            probe
                .removed
                .insert(format!("e{r_idx:031x}"), make_tomb(r_idx, 100));
            if probe.to_toml_string().len() > MAX_DEVICES_TOML_BYTES - 650 {
                break;
            }
            reserve_full = probe;
            r_idx += 1;
        }
        let reserve_text = reserve_full.to_toml_string();
        assert!(
            reserve_text.len() <= MAX_DEVICES_TOML_BYTES - 600
                && reserve_text.len() >= MAX_DEVICES_TOML_BYTES - 1050,
            "reserve test seeded size = {}",
            reserve_text.len()
        );
        std::fs::write(h.node.paths().config_file(DevicesFile::FILE), &reserve_text).unwrap();

        let inv = h
            .call(
                "device.invite",
                json!({"principal": "owner", "allow_owner": true}),
            )
            .await
            .unwrap();
        let inv_id = inv["id"].as_str().unwrap().to_owned();
        let req = format!(
            "{}\n",
            json!({"enrol": 1, "id": inv_id, "public_key": ed25519_public_key_from_seed(&[0xcc; 32]), "name": "z"})
        );
        let reply = run_enrol(&h.node, &inv_id, req.as_bytes()).await;
        let err = match reply {
            crate::device::enrol::EnrolReply::Err { error, .. } => error,
            other => panic!("enrol breaking tombstone reserve must be refused, got {other:?}"),
        };
        assert_eq!(err.code, ErrorCode::InvalidArgs, "{err:?}");
        let hint = err.hint.as_deref().unwrap_or("");
        assert!(
            hint.contains("device.remove")
                && hint.contains("ever had")
                && !hint.to_ascii_lowercase().contains("tidy")
                && !hint.to_ascii_lowercase().contains("prune"),
            "size error hint must be honest (no tidy/prune, mention device.remove and lifetime fleet bound): {err:?}"
        );
    }

    #[tokio::test]
    async fn f4_meta_doctor_acknowledge_devices_rollback_is_owner_only_and_audited() {
        let h = Harness::new();
        std::fs::write(
            h.node.paths().config_file("policy.toml"),
            "\
default = \"owner\"

[roles.ops]
allow = [\"effect:read\", \"effect:admin\"]

[principals.tablet]
role = \"ops\"
",
        )
        .unwrap();

        // Trigger a recorded rollback failure so state/devices-rollback-error.json exists.
        let blocker = h.dir.path().join("not-a-dir-f4");
        std::fs::write(&blocker, b"blocker").unwrap();
        let _ = registry::restore_file_snapshot(
            h.node.paths(),
            "device.set",
            &blocker.join("policy.toml"),
            Some(b"default = \"owner\"\n"),
        )
        .unwrap_err();
        let record_path = registry::rollback_failure_record_path(h.node.paths());
        assert!(record_path.is_file());

        // `meta.doctor` fails and its hint points to `meta.doctor acknowledge=devices-rollback`.
        let before = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        assert_eq!(before["ok"], json!(false), "{before}");
        let rollback_check = before["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == json!("config rollback failures"))
            .unwrap();
        assert!(
            rollback_check["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("meta.doctor acknowledge=devices-rollback")),
            "meta.doctor hint must mention `meta.doctor acknowledge=devices-rollback`: {rollback_check}"
        );

        // Non-owner (`tablet` with effect:read + effect:admin) MUST be denied and leave the marker intact.
        let mut non_owner_call = Call::new(
            Door::Ssh,
            "meta.doctor",
            json!({"quick": true, "acknowledge": "devices-rollback"}),
        );
        non_owner_call.pinned = Some("tablet".to_owned());
        non_owner_call.device = Some("my-tablet".to_owned());
        let denied = h.node.call(non_owner_call).await.unwrap_err();
        assert_eq!(denied.code, ErrorCode::Denied, "{denied:?}");
        assert!(
            record_path.is_file(),
            "denied acknowledge must not delete rollback error marker"
        );

        // Unknown acknowledge target is rejected with InvalidArgs.
        let bad_target = h
            .call(
                "meta.doctor",
                json!({"quick": true, "acknowledge": "something-else"}),
            )
            .await
            .unwrap_err();
        assert_eq!(bad_target.code, ErrorCode::InvalidArgs, "{bad_target:?}");
        assert!(record_path.is_file());

        // Owner calling `meta.doctor acknowledge="devices-rollback"` removes the marker, reports
        // ok=true, and records an audit entry naming who acknowledged it.
        let mut owner_ack = Call::new(
            Door::Ssh,
            "meta.doctor",
            json!({"quick": true, "acknowledge": "devices-rollback"}),
        );
        owner_ack.device = Some("owner-laptop".to_owned());
        let ack_res = h.node.call(owner_ack).await.unwrap();
        assert_eq!(ack_res["ok"], json!(true), "{ack_res}");
        assert_eq!(
            ack_res["acknowledged"],
            json!("devices-rollback"),
            "{ack_res}"
        );
        assert!(
            !record_path.exists(),
            "owner acknowledge must remove state/devices-rollback-error.json"
        );

        // Subsequent `meta.doctor` is clean with no rollback-failure check.
        let after = h.call("meta.doctor", json!({"quick": true})).await.unwrap();
        assert_eq!(after["ok"], json!(true), "{after}");
        assert!(
            !after["checks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["name"] == json!("config rollback failures")),
            "{after}"
        );

        // Audit log records who acknowledged the rollback failure.
        let audit =
            std::fs::read_to_string(h.node.paths().state_dir().join("audit.jsonl")).unwrap();
        let records: Vec<Value> = audit
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let ack_audit = records
            .iter()
            .find(|r| {
                r["tool"] == json!("meta.doctor.acknowledge")
                    && r["principal"] == json!("owner")
                    && r["device"] == json!("owner-laptop")
                    && r["args"]["acknowledge"] == json!("devices-rollback")
            })
            .unwrap_or_else(|| {
                panic!("expected audited meta.doctor.acknowledge record: {records:?}")
            });
        assert_eq!(ack_audit["args"]["cleared_count"], json!(1), "{ack_audit}");
    }

    #[tokio::test]
    async fn owner_strength_guard_covers_all_admin_roles_on_invite_and_set() {
        use legion_core::NoArgs;

        let h = Harness::new();
        // Register a `config.write` Admin stub just like `legion-net` does on live nodes so we
        // verify roles reaching an admin tool by exact name (including `config.write` and
        // `device.invite`) are resolved identically to `legion pair`.
        let mut reg = crate::registry();
        reg.add_fn(
            "config.write",
            Effect::Admin,
            "Write a config file (test stub).",
            |_ctx: &Ctx, _args: NoArgs| async { Ok(json!({})) },
        );
        let node = Node::new(h.node.paths().clone(), reg);

        std::fs::write(
            node.paths().config_file("policy.toml"),
            r#"
default = "owner"

[roles.observer]
allow = ["effect:read"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy"]
deny = ["effect:power", "effect:admin"]

[roles.phone_admin]
allow = ["effect:read", "effect:admin"]

[roles.wildcard_admin]
allow = ["*"]

[roles.policy_writer]
allow = ["effect:read", "config.write"]

[roles.inviter]
allow = ["effect:read", "device.invite"]

[principals.phone]
role = "observer"

[principals.ops_box]
role = "operator"

[principals.tablet]
role = "phone_admin"

[principals.star]
role = "wildcard_admin"

[principals.writer]
role = "policy_writer"

[principals.inv_user]
role = "inviter"
"#,
        )
        .unwrap();

        let reason = "this role can change who may do what, which makes it as strong as owner";

        let call_as = |principal: Option<&str>, tool: &str, args: Value| {
            let mut call = Call::new(Door::Cli, tool, args);
            if let Some(p) = principal {
                call.pinned = Some(p.to_owned());
            }
            node.call(call)
        };

        // Pair a regular observer device to test `device.set role=`.
        let seed_inv = call_as(
            None,
            "device.invite",
            json!({"name": "target-dev", "role": "observer", "host": "tower.example", "user": "me"}),
        )
        .await
        .unwrap();
        let seed_id = seed_inv["id"].as_str().unwrap();
        let seed_key = generate_ephemeral_ed25519().unwrap();
        let seed_req = format!(
            "{{\"enrol\":1,\"id\":\"{seed_id}\",\"public_key\":\"{}\",\"name\":\"target-dev\"}}\n",
            seed_key.public_key
        );
        assert!(run_enrol(&node, seed_id, seed_req.as_bytes()).await.is_ok());

        // Non-admin roles (`observer`, `operator`) do NOT require allow_owner=true.
        for (idx, (role, principal)) in [("observer", "phone"), ("operator", "ops_box")]
            .into_iter()
            .enumerate()
        {
            call_as(
                None,
                "device.invite",
                json!({"principal": principal, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_or_else(|e| {
                panic!("non-admin principal {principal} should not need allow_owner: {e:?}")
            });
            call_as(
                None,
                "device.invite",
                json!({"name": format!("non-admin-{idx}"), "role": role, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_or_else(|e| panic!("non-admin role {role} should not need allow_owner: {e:?}"));
            call_as(
                None,
                "device.set",
                json!({"name": "target-dev", "role": role}),
            )
            .await
            .unwrap_or_else(|e| {
                panic!("device.set to non-admin role {role} should not need allow_owner: {e:?}")
            });
        }

        // Every owner-strength role (effect:admin, *, config.write, device.invite) must enforce
        // BOTH the explicit allow_owner=true flag AND the owner-only caller rule.
        for (idx, (role, principal)) in [
            ("phone_admin", "tablet"),
            ("wildcard_admin", "star"),
            ("policy_writer", "writer"),
            ("inviter", "inv_user"),
        ]
        .into_iter()
        .enumerate()
        {
            // 1. Owner without allow_owner=true is refused on device.invite (principal=).
            let err = call_as(
                None,
                "device.invite",
                json!({"principal": principal, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{principal}/{role}: {err:?}");
            assert!(
                err.message.contains(role) && err.message.contains(reason),
                "principal={principal} missing role or plain reason: {}",
                err.message
            );

            // 2. Owner without allow_owner=true is refused on device.invite (role= + name=).
            let err = call_as(
                None,
                "device.invite",
                json!({"name": format!("os-dev-{idx}"), "role": role, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "role={role}: {err:?}");
            assert!(
                err.message.contains(role) && err.message.contains(reason),
                "role={role} invite missing role or plain reason: {}",
                err.message
            );

            // 3. Owner without allow_owner=true is refused on device.set role=.
            let err = call_as(
                None,
                "device.set",
                json!({"name": "target-dev", "role": role}),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "device.set {role}: {err:?}");
            assert!(
                err.message.contains(role) && err.message.contains(reason),
                "device.set role={role} missing role or plain reason: {}",
                err.message
            );

            // 4. Non-owner caller (even the principal itself, and even with allow_owner=true) is
            //    refused on device.invite principal= (same caller rule as owner).
            for flag in [false, true] {
                let err = call_as(
                    Some(principal),
                    "device.invite",
                    json!({
                        "principal": principal,
                        "allow_owner": flag,
                        "host": "tower.example",
                        "user": "me"
                    }),
                )
                .await
                .unwrap_err();
                assert_eq!(
                    err.code,
                    ErrorCode::Denied,
                    "self-invite for {principal} (allow_owner={flag}): {err:?}"
                );
                // Roles that can reach `device.invite` hit check_issue and must explain why;
                // roles that cannot call `device.invite` (like `policy_writer`) are already
                // denied by policy.
                if role != "policy_writer" {
                    assert!(
                        err.message.contains(role) && err.message.contains(reason),
                        "non-owner caller {principal} (allow_owner={flag}) missing role or plain reason: {}",
                        err.message
                    );
                }
            }

            // 5. Non-owner admin caller (`tablet`) trying to invite for `writer` is also refused
            //    with the owner-strength caller message.
            let err = call_as(
                Some("tablet"),
                "device.invite",
                json!({
                    "principal": principal,
                    "allow_owner": true,
                    "host": "tower.example",
                    "user": "me"
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(
                err.code,
                ErrorCode::Denied,
                "tablet -> {principal}: {err:?}"
            );
            assert!(
                err.message.contains(role) && err.message.contains(reason),
                "tablet -> {principal} missing role or plain reason: {}",
                err.message
            );

            // 6. Owner WITH allow_owner=true succeeds for principal=, role=+name=, and device.set.
            call_as(
                None,
                "device.invite",
                json!({"principal": principal, "allow_owner": true, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_or_else(|e| panic!("owner + allow_owner principal={principal} failed: {e:?}"));
            call_as(
                None,
                "device.invite",
                json!({"name": format!("os-ok-{idx}"), "role": role, "allow_owner": true, "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap_or_else(|e| panic!("owner + allow_owner role={role} failed: {e:?}"));
            call_as(
                None,
                "device.set",
                json!({"name": "target-dev", "role": role, "allow_owner": true}),
            )
            .await
            .unwrap_or_else(|e| panic!("owner + allow_owner device.set role={role} failed: {e:?}"));
        }
    }

    #[tokio::test]
    async fn device_roles_returns_caller_allowed_roles_with_pair_descriptions_and_hides_policy_from_non_owners()
     {
        use legion_core::NoArgs;

        let h = Harness::new();
        let mut reg = crate::registry();
        reg.add_fn(
            "config.write",
            Effect::Admin,
            "Write a config file (test stub).",
            |_ctx: &Ctx, _args: NoArgs| async { Ok(json!({})) },
        );
        let node = Node::new(h.node.paths().clone(), reg);

        std::fs::write(
            node.paths().config_file("policy.toml"),
            r#"
default = "owner"
secrets = ["~/top-secret-vault"]

[roles.empty_role]
allow = []

[roles.observer]
allow = ["effect:read"]
deny = ["audit.*"]
paths = ["~/private-project/**"]

[roles.editor]
allow = ["effect:read", "effect:write"]
deny = ["audit.*"]
paths = ["~/private-project/**"]

[roles.cleaner]
allow = ["effect:read", "fs.remove"]

[roles.unknown_ext]
allow = ["effect:read", "custom.mystery"]

[roles.power_user]
allow = ["effect:read", "power.shutdown"]

[roles.operator]
allow = ["effect:read", "effect:write", "effect:exec", "effect:destroy"]
deny = ["effect:power", "effect:admin"]

[roles.policy_writer]
allow = ["effect:read", "config.write"]

[roles.phone_admin]
allow = ["effect:read", "effect:admin"]

[principals.phone]
role = "observer"

[principals.dev]
role = "editor"

[principals.ops]
role = "operator"

[principals.tablet]
role = "phone_admin"

[principals.secret_user]
role = "observer"
"#,
        )
        .unwrap();

        // 1. `device.roles` is registered with Effect::Read.
        let specs = node.specs();
        let role_spec = specs
            .iter()
            .find(|s| s.name == "device.roles")
            .expect("device.roles tool must be registered");
        assert_eq!(role_spec.effect, Effect::Read);

        // 2. Owner sees every role from policy.toml + built-in `owner`, in `legion pair`'s exact
        //    order and with `legion pair`'s exact descriptions, plus strongest_effect and
        //    owner_strength — and nothing else from policy.toml.
        let owner_res = node
            .call(Call::new(Door::Cli, "device.roles", json!({})))
            .await
            .unwrap();
        assert_eq!(owner_res["node"], json!("tower"), "{owner_res}");
        let owner_text = serde_json::to_string(&owner_res).unwrap();
        for leaked in [
            "secret_user",
            "top-secret-vault",
            "private-project",
            "audit.*",
        ] {
            assert!(
                !owner_text.contains(leaked),
                "device.roles must not expose {leaked:?} from policy.toml: {owner_text}"
            );
        }

        let policy = node.config().get::<Policy>().unwrap();
        let expected_choices = crate::roles::role_choices(&policy, &specs);
        let returned = owner_res["roles"].as_array().unwrap();
        let returned_pairs: Vec<(String, String)> = returned
            .iter()
            .map(|r| {
                let obj = r.as_object().unwrap();
                assert_eq!(
                    obj.len(),
                    4,
                    "unexpected extra fields in role object: {obj:?}"
                );
                (
                    obj["name"].as_str().unwrap().to_owned(),
                    obj["description"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        assert_eq!(returned_pairs, expected_choices);

        let by_name = |name: &str| {
            returned
                .iter()
                .find(|r| r["name"] == json!(name))
                .unwrap_or_else(|| panic!("missing role {name} in {returned:?}"))
        };
        for (name, expected_effect, expected_owner_strength) in [
            ("empty_role", Value::Null, false),
            ("observer", json!("read"), false),
            ("editor", json!("write"), false),
            ("cleaner", json!("destroy"), false),
            ("unknown_ext", Value::Null, false),
            ("power_user", json!("power"), false),
            ("operator", json!("exec"), false),
            ("phone_admin", json!("admin"), true),
            ("policy_writer", json!("admin"), true),
            ("owner", json!("admin"), true),
        ] {
            let entry = by_name(name);
            assert_eq!(
                entry["strongest_effect"], expected_effect,
                "role {name} strongest_effect mismatch: {entry}"
            );
            assert_eq!(
                entry["owner_strength"],
                json!(expected_owner_strength),
                "role {name} owner_strength mismatch: {entry}"
            );
        }

        // 3. Every non-owner caller (observer, editor, operator, and even non-owner admin
        //    `tablet`) can call `device.roles` (it is Effect::Read) and receives an empty list,
        //    leaking nothing from policy.toml.
        for caller in ["phone", "dev", "ops", "tablet"] {
            let mut call = Call::new(Door::Cli, "device.roles", json!({}));
            call.pinned = Some(caller.to_owned());
            let res = node
                .call(call)
                .await
                .unwrap_or_else(|e| panic!("device.roles as {caller} should succeed: {e:?}"));
            assert_eq!(res["node"], json!("tower"), "{caller}: {res}");
            assert_eq!(
                res["roles"],
                json!([]),
                "non-owner {caller} may not hand out roles and must get []: {res}"
            );
            let text = serde_json::to_string(&res).unwrap();
            for hidden in [
                "observer",
                "policy_writer",
                "phone_admin",
                "secret_user",
                "top-secret-vault",
            ] {
                assert!(
                    !text.contains(hidden),
                    "non-owner {caller} response leaked {hidden:?}: {text}"
                );
            }
        }
    }

    #[tokio::test]
    async fn enrol_unconfirmed_on_broken_pipe_replaced_by_fresh_invite_confirmed_on_session_and_conflict_detail()
     {
        use std::pin::Pin;
        use std::task::{Context, Poll};

        struct BrokenPipeWriter;

        impl tokio::io::AsyncWrite for BrokenPipeWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "client disconnected before reply",
                )))
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "client disconnected before flush",
                )))
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let h = Harness::new();

        // 1. Client sends enrolment request, machine commits, then writing the reply hits
        //    BrokenPipe because the client has gone -> recorded as `unconfirmed = true` in
        //    devices.toml, `device.list`, and `audit.jsonl`.
        let inv1 = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "vanish-phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv1_id = inv1["id"].as_str().unwrap().to_owned();
        let key1 = generate_ephemeral_ed25519().unwrap();
        let req1 = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": inv1_id,
                "public_key": key1.public_key,
                "name": "vanish-phone",
            })
        );
        let io1 = enrol::run_enrol_io(
            &h.node,
            &inv1_id,
            req1.as_bytes(),
            BrokenPipeWriter,
            std::future::pending(),
        )
        .await;
        assert!(
            io1.reply.is_ok(),
            "commit itself succeeded: {:?}",
            io1.reply
        );
        assert!(!io1.delivered, "BrokenPipe must report delivered = false");
        assert!(io1.unconfirmed, "BrokenPipe must mark device unconfirmed");

        let reg1 = registry::load_devices_raw(h.node.paths()).unwrap().unwrap();
        assert_eq!(reg1.devices.len(), 1);
        let (pid1, _) = reg1.devices.iter().next().unwrap();
        let pid1 = pid1.clone();
        // `unconfirmed` MUST live strictly in local `state/devices.json`, NEVER in `devices.toml`!
        let devices_toml_1 =
            std::fs::read_to_string(h.node.paths().config_file("devices.toml")).unwrap();
        assert!(
            !devices_toml_1.contains("unconfirmed"),
            "devices.toml must never carry an unconfirmed field:\n{devices_toml_1}"
        );
        let state1 = store::load_state(h.node.paths()).unwrap();
        assert!(
            state1.unconfirmed.contains_key(&pid1),
            "local state/devices.json must record unconfirmed pairing"
        );

        let listed1 = h.call("device.list", json!({})).await.unwrap();
        let list_devs1 = listed1["devices"].as_array().unwrap();
        assert_eq!(list_devs1.len(), 1);
        assert_eq!(list_devs1[0]["unconfirmed"], json!(true));
        assert_eq!(list_devs1[0]["status"], json!("Pairing not finished"));

        let audit_text = std::fs::read_to_string(h.node.paths().audit_log()).unwrap();
        assert!(
            audit_text.contains("\"reply_delivered\":false")
                && audit_text.contains("\"unconfirmed\":true"),
            "audit.jsonl must record undelivered/unconfirmed enrolment: {audit_text}"
        );

        // 2. A fresh invite presented with the SAME device key replaces that unconfirmed leftover
        //    under the same locks, tombstones the old entry, removes the old `device:*` principal,
        //    and succeeds cleanly (the replacement itself is born locally unconfirmed until its
        //    first real authenticated `legion gate` session).
        let inv2 = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "vanish-retry", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv2_id = inv2["id"].as_str().unwrap().to_owned();
        let req2 = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": inv2_id,
                "public_key": key1.public_key,
                "name": "vanish-retry",
            })
        );
        let mut out2 = Vec::new();
        let io2 = enrol::run_enrol_io(
            &h.node,
            &inv2_id,
            req2.as_bytes(),
            &mut out2,
            std::future::pending(),
        )
        .await;
        assert!(
            io2.reply.is_ok(),
            "fresh invite should replace unconfirmed entry: {:?}",
            io2.reply
        );
        assert!(io2.delivered);
        assert!(
            io2.unconfirmed,
            "every newly committed enrolment starts locally unconfirmed until first gate session"
        );
        assert!(
            String::from_utf8(out2).unwrap().contains("\"ok\":true"),
            "reply line must be written on stdout"
        );

        let reg2 = registry::load_devices_raw(h.node.paths()).unwrap().unwrap();
        assert_eq!(reg2.devices.len(), 1);
        assert!(!reg2.devices.contains_key(&pid1));
        assert!(
            reg2.removed.contains_key(&pid1),
            "replaced unconfirmed entry must be tombstoned"
        );
        let (pid2, dev2) = reg2.devices.iter().next().unwrap();
        let pid2 = pid2.clone();
        assert_eq!(dev2.principal, "device:vanish-retry");
        let state2 = store::load_state(h.node.paths()).unwrap();
        assert!(!state2.unconfirmed.contains_key(&pid1));
        assert!(state2.unconfirmed.contains_key(&pid2));

        let policy_text =
            std::fs::read_to_string(h.node.paths().config_file(legion_core::Policy::FILE)).unwrap();
        assert!(
            !policy_text.contains("device:vanish-phone"),
            "replaced unconfirmed device principal must be removed from policy.toml: {policy_text}"
        );
        assert!(policy_text.contains("device:vanish-retry"));

        let listed2 = h.call("device.list", json!({})).await.unwrap();
        let list_devs2 = listed2["devices"].as_array().unwrap();
        assert_eq!(list_devs2.len(), 1);
        assert_eq!(list_devs2[0]["unconfirmed"], json!(true));
        assert_eq!(list_devs2[0]["status"], json!("Pairing not finished"));

        // 3. Another unconfirmed device gets confirmed automatically on its first session open
        //    (`verify_gate_device`).
        let inv3 = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "confirm-on-open", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv3_id = inv3["id"].as_str().unwrap().to_owned();
        let key2 = generate_ephemeral_ed25519().unwrap();
        let req3 = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": inv3_id,
                "public_key": key2.public_key,
                "name": "confirm-on-open",
            })
        );
        let io3 = enrol::run_enrol_io(
            &h.node,
            &inv3_id,
            req3.as_bytes(),
            BrokenPipeWriter,
            std::future::pending(),
        )
        .await;
        assert!(io3.unconfirmed);
        let reg3 = registry::load_devices_raw(h.node.paths()).unwrap().unwrap();
        let (pid3, dev3) = reg3
            .devices
            .iter()
            .find(|(_, d)| d.principal == "device:confirm-on-open")
            .unwrap();
        let (pid3, dev3_name, dev3_principal) =
            (pid3.clone(), dev3.name.clone(), dev3.principal.clone());
        assert!(
            store::load_state(h.node.paths())
                .unwrap()
                .unconfirmed
                .contains_key(&pid3)
        );

        let node_file = h
            .node
            .config()
            .get::<legion_core::config::NodeFile>()
            .unwrap();
        registry::verify_gate_device(
            h.node.paths(),
            &h.node.name(),
            &node_file,
            &dev3_name,
            &pid3,
            &dev3_principal,
            None,
        )
        .unwrap();

        assert!(
            !store::load_state(h.node.paths())
                .unwrap()
                .unconfirmed
                .contains_key(&pid3),
            "first session open must clear local unconfirmed mark"
        );
        let listed3 = h.call("device.list", json!({})).await.unwrap();
        let entry3 = listed3["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(pid3))
            .unwrap();
        assert!(entry3.get("unconfirmed").is_none());
        assert!(entry3.get("status").is_none());

        // 4. Confirmed device presenting a fresh invite is still refused with `ErrorCode::Conflict`
        //    and now includes optional `details.existing_principal`, `existing_device`, and `node`.
        let inv4 = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "confirm-dup", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv4_id = inv4["id"].as_str().unwrap().to_owned();
        let req4 = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": inv4_id,
                "public_key": key2.public_key,
                "name": "confirm-dup",
            })
        );
        let mut out4 = Vec::new();
        let io4 = enrol::run_enrol_io(
            &h.node,
            &inv4_id,
            req4.as_bytes(),
            &mut out4,
            std::future::pending(),
        )
        .await;
        match io4.reply {
            EnrolReply::Err { ok: false, error } => {
                assert_eq!(error.code, ErrorCode::Conflict);
                assert!(
                    error
                        .message
                        .contains("already paired with tower as device:confirm-on-open"),
                    "unexpected conflict message: {}",
                    error.message
                );
                let details = error
                    .details
                    .expect("confirmed conflict must include details");
                assert_eq!(
                    details["existing_principal"],
                    json!("device:confirm-on-open")
                );
                assert_eq!(details["existing_device"], json!(dev3_name));
                assert_eq!(details["node"], json!("tower"));
            }
            other => panic!("expected Conflict for confirmed duplicate key, got {other:?}"),
        }

        // 5. Wire backward compatibility: an older client struct without `details` or `unconfirmed`
        //    deserializes the new replies cleanly.
        #[derive(serde::Deserialize)]
        struct LegacyEnrolErr {
            ok: bool,
            error: LegacyErrBody,
        }
        #[derive(serde::Deserialize)]
        struct LegacyErrBody {
            code: ErrorCode,
            message: String,
        }
        let legacy_err: LegacyEnrolErr = serde_json::from_slice(&out4).unwrap();
        assert!(!legacy_err.ok);
        assert_eq!(legacy_err.error.code, ErrorCode::Conflict);
        assert!(legacy_err.error.message.contains("device:confirm-on-open"));
    }

    #[tokio::test]
    async fn unconfirmed_stays_local_only_survives_fleet_merge_and_main_parses_devices_toml() {
        // Proves reviewer findings p5 and p6:
        // 1. `devices.toml` after an unconfirmed enrolment contains NO unknown fields and parses
        //    under `origin/main`'s exact `#[serde(deny_unknown_fields)]` schema.
        // 2. A fleet merge (incoming peer `devices.toml` without `unconfirmed`) NEVER clears the
        //    enrolling machine's local `unconfirmed` state without a real authenticated gate
        //    session.
        #[derive(Default, serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct MainDevicesFile {
            #[allow(dead_code)]
            version: u32,
            #[serde(default)]
            devices: BTreeMap<String, MainRawFleetDevice>,
            #[serde(default)]
            #[allow(dead_code)]
            removed: BTreeMap<String, registry::DeviceTombstone>,
        }

        impl legion_core::config::Document for MainDevicesFile {
            const FILE: &'static str = "devices.toml";
        }

        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct MainRawFleetDevice {
            name: String,
            #[allow(dead_code)]
            public_key: String,
            principal: String,
            #[serde(default)]
            #[allow(dead_code)]
            role: Option<String>,
            #[allow(dead_code)]
            paired_at: String,
            #[allow(dead_code)]
            paired_on: String,
            #[serde(default)]
            #[allow(dead_code)]
            paired_by: Option<registry::PairedBy>,
            #[serde(default)]
            #[allow(dead_code)]
            confirmed_by: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            confirmed_at: Option<String>,
            #[allow(dead_code)]
            updated_at: String,
            #[allow(dead_code)]
            updated_on: String,
            #[serde(default)]
            #[allow(dead_code)]
            name_updated_at: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            name_updated_on: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            role_updated_at: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            role_updated_on: Option<String>,
            #[serde(default)]
            #[allow(dead_code)]
            conflict: bool,
            #[serde(default)]
            #[allow(dead_code)]
            role_conflict: bool,
        }

        let h = Harness::new();
        let inv = h
            .call(
                "device.invite",
                json!({"role": "observer", "name": "fleet-phone", "host": "tower.example", "user": "me"}),
            )
            .await
            .unwrap();
        let inv_id = inv["id"].as_str().unwrap().to_owned();
        let key = generate_ephemeral_ed25519().unwrap();
        let req = format!(
            "{}\n",
            json!({
                "enrol": 1,
                "id": inv_id,
                "public_key": key.public_key,
                "name": "fleet-phone",
            })
        );
        let reply = run_enrol(&h.node, &inv_id, req.as_bytes()).await;
        assert!(reply.is_ok(), "{reply:?}");

        // p6: raw `devices.toml` on disk must have zero `unconfirmed` lines and deserialize under
        // `origin/main`'s strict `deny_unknown_fields` parser.
        let raw_toml = std::fs::read_to_string(h.node.paths().config_file("devices.toml")).unwrap();
        assert!(
            !raw_toml.contains("unconfirmed"),
            "devices.toml must never contain `unconfirmed`:\n{raw_toml}"
        );
        let parsed_by_main: MainDevicesFile = legion_core::config::parse(
            &h.node.paths().config_file("devices.toml"),
            raw_toml.as_bytes(),
        )
        .unwrap_or_else(|e| panic!("origin/main parser rejected devices.toml: {e:?}\n{raw_toml}"));
        assert_eq!(parsed_by_main.devices.len(), 1);
        let (pid, main_dev) = parsed_by_main.devices.iter().next().unwrap();
        assert_eq!(main_dev.name, "fleet-phone");
        assert_eq!(main_dev.principal, "device:fleet-phone");
        let pid = pid.clone();

        // p5: simulate a peer syncing back a `devices.toml` containing the same device (with a
        // newer rename from another machine). Merging it via `apply_devices_merge_blocking` must
        // NEVER clear the local `state.unconfirmed` mark on the enrolling machine.
        let mut incoming = registry::load_devices_raw(h.node.paths()).unwrap().unwrap();
        let entry = incoming.devices.get_mut(&pid).unwrap();
        let next_ts = registry::next_updated_timestamp(Some(&entry.updated_at)).unwrap();
        entry.name = "fleet-phone-renamed".to_owned();
        entry.name_updated_at = next_ts.clone();
        entry.name_updated_on = "pi".to_owned();
        entry.updated_at = next_ts;
        entry.updated_on = "pi".to_owned();

        let node_file = h.node.config().get::<NodeFile>().unwrap();
        registry::apply_devices_merge_blocking(
            h.node.paths(),
            "tower",
            &node_file,
            &incoming,
            |_| Ok(()),
            |_, _, _| Ok(()),
            |_| Ok(()),
        )
        .expect("fleet merge of peer rename must succeed");

        let state_after_merge = store::load_state(h.node.paths()).unwrap();
        assert!(
            state_after_merge.unconfirmed.contains_key(&pid),
            "fleet merge must NEVER clear local unconfirmed state without a real gate session"
        );
        let listed = h.call("device.list", json!({})).await.unwrap();
        let devs = listed["devices"].as_array().unwrap();
        assert_eq!(devs.len(), 1);
        assert_eq!(devs[0]["name"], json!("fleet-phone-renamed"));
        assert_eq!(devs[0]["unconfirmed"], json!(true));
        assert_eq!(devs[0]["status"], json!("Pairing not finished"));

        // Only a real authenticated `legion gate` session (`verify_gate_device`) clears it.
        registry::verify_gate_device(
            h.node.paths(),
            "tower",
            &node_file,
            "fleet-phone-renamed",
            &pid,
            "device:fleet-phone",
            None,
        )
        .unwrap();
        assert!(
            !store::load_state(h.node.paths())
                .unwrap()
                .unconfirmed
                .contains_key(&pid),
            "authenticated gate session must clear local unconfirmed mark"
        );
        let listed_after = h.call("device.list", json!({})).await.unwrap();
        assert!(listed_after["devices"][0].get("unconfirmed").is_none());
        assert!(listed_after["devices"][0].get("status").is_none());
    }

    #[tokio::test]
    async fn enrol_crash_kill_points_repaired_on_start_and_next_mutation() {
        // Test 1: Kill after step 1 (`state.unconfirmed` written, before `devices.toml` /
        // `policy.toml` / `authorized_keys`). Next run removes the leftover invite and prunes
        // the orphan unconfirmed record without creating a device.
        {
            let h = Harness::new();
            let inv = h
                .call(
                    "device.invite",
                    json!({"role": "observer", "name": "kill-step1", "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap();
            let inv_id = inv["id"].as_str().unwrap().to_owned();
            let key = generate_ephemeral_ed25519().unwrap();
            let req = format!(
                "{}\n",
                json!({"enrol": 1, "id": inv_id, "public_key": key.public_key, "name": "kill-step1"})
            );
            let reply = enrol::run_enrol_with_kill_point(
                &h.node,
                &inv_id,
                req.as_bytes(),
                enrol::EnrolKillPoint::AfterUnconfirmedState,
            )
            .await;
            assert!(!reply.is_ok());
            assert!(
                registry::load_devices_raw(h.node.paths())
                    .unwrap()
                    .is_none(),
                "step 1 kill must not have written devices.toml"
            );
            assert!(
                !store::load_state(h.node.paths())
                    .unwrap()
                    .unconfirmed
                    .is_empty(),
                "step 1 kill leaves local unconfirmed marker"
            );
            assert!(
                std::fs::read_to_string(&h.auth_keys)
                    .unwrap()
                    .contains(&format!("legion-invite:{inv_id}"))
            );

            // Startup repair removes leftover invite and prunes orphan unconfirmed marker.
            let node_file = h.node.config().get::<NodeFile>().unwrap();
            registry::repair_incomplete_enrolments_at_start(h.node.paths(), "tower", &node_file);
            let state = store::load_state(h.node.paths()).unwrap();
            assert!(
                state.unconfirmed.is_empty(),
                "orphan unconfirmed must be pruned"
            );
            assert!(
                !state.invites.contains_key(&inv_id),
                "leftover invite must be removed"
            );
            let auth = std::fs::read_to_string(&h.auth_keys).unwrap();
            assert!(!auth.contains(&format!("legion-invite:{inv_id}")), "{auth}");
            assert!(!auth.contains("legion-device:kill-step1"), "{auth}");
        }

        // Test 2 (Kill Point A): Kill after step 2 (`devices.toml` + `policy.toml` written,
        // BEFORE `authorized_keys` gets the permanent device key line).
        // Verify startup repair restores the missing permanent key line AND removes the leftover
        // invite, while keeping the device locally marked `unconfirmed`.
        {
            let h = Harness::new();
            let inv = h
                .call(
                    "device.invite",
                    json!({"role": "observer", "name": "kill-step2-start", "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap();
            let inv_id = inv["id"].as_str().unwrap().to_owned();
            let key = generate_ephemeral_ed25519().unwrap();
            let req = format!(
                "{}\n",
                json!({"enrol": 1, "id": inv_id, "public_key": key.public_key, "name": "kill-step2-start"})
            );
            let reply = enrol::run_enrol_with_kill_point(
                &h.node,
                &inv_id,
                req.as_bytes(),
                enrol::EnrolKillPoint::AfterDevicesAndPolicy,
            )
            .await;
            assert!(!reply.is_ok());

            // Pre-repair state: devices.toml has the device, authorized_keys is MISSING the
            // permanent key line and STILL has the invite line.
            let reg = registry::load_devices_raw(h.node.paths()).unwrap().unwrap();
            assert_eq!(reg.devices.len(), 1);
            let pid = reg.devices.keys().next().unwrap().clone();
            let auth_before = std::fs::read_to_string(&h.auth_keys).unwrap();
            assert!(!auth_before.contains("legion-device:kill-step2-start"));
            assert!(auth_before.contains(&format!("legion-invite:{inv_id}")));

            let node_file = h.node.config().get::<NodeFile>().unwrap();
            registry::repair_incomplete_enrolments_at_start(h.node.paths(), "tower", &node_file);

            let auth_after = std::fs::read_to_string(&h.auth_keys).unwrap();
            assert!(
                auth_after.contains("legion-device:kill-step2-start"),
                "startup repair must restore missing permanent key line:\n{auth_after}"
            );
            assert!(
                !auth_after.contains(&format!("legion-invite:{inv_id}")),
                "startup repair must remove leftover invite:\n{auth_after}"
            );
            let state = store::load_state(h.node.paths()).unwrap();
            assert!(!state.invites.contains_key(&inv_id));
            assert!(
                state.unconfirmed.contains_key(&pid),
                "device stays locally unconfirmed until first gate session"
            );
        }

        // Test 3 (Kill Point A repaired by next mutation — `device.invite` AND `device.enrol`):
        {
            let h = Harness::new();
            let inv = h
                .call(
                    "device.invite",
                    json!({"role": "observer", "name": "kill-step2-mut", "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap();
            let inv_id = inv["id"].as_str().unwrap().to_owned();
            let key = generate_ephemeral_ed25519().unwrap();
            let req = format!(
                "{}\n",
                json!({"enrol": 1, "id": inv_id, "public_key": key.public_key, "name": "kill-step2-mut"})
            );
            let _ = enrol::run_enrol_with_kill_point(
                &h.node,
                &inv_id,
                req.as_bytes(),
                enrol::EnrolKillPoint::AfterDevicesAndPolicy,
            )
            .await;

            // Retrying the leftover invite via `device.enrol` runs repair BEFORE checking invites:
            // removes the leftover invite, restores the missing permanent key line, and rejects
            // the replay with NotFound!
            let replay = run_enrol(&h.node, &inv_id, req.as_bytes()).await;
            match replay {
                EnrolReply::Err { ok: false, error } => {
                    assert_eq!(error.code, ErrorCode::NotFound, "{error:?}");
                }
                other => panic!("expected NotFound after crash repair, got {other:?}"),
            }
            let auth_after = std::fs::read_to_string(&h.auth_keys).unwrap();
            assert!(
                auth_after.contains("legion-device:kill-step2-mut"),
                "device.enrol repair must restore missing permanent key line:\n{auth_after}"
            );
            assert!(
                !auth_after.contains(&format!("legion-invite:{inv_id}")),
                "device.enrol repair must remove leftover invite:\n{auth_after}"
            );
        }

        // Test 4 (Kill Point B): Kill after step 3 (`authorized_keys` has BOTH permanent device
        // line and leftover invite line, before step 4 removes the invite).
        // Verify next mutation (`device.invite`) removes the leftover invite while preserving the
        // permanent device line and local `unconfirmed` state.
        {
            let h = Harness::new();
            let inv = h
                .call(
                    "device.invite",
                    json!({"role": "observer", "name": "kill-step3", "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap();
            let inv_id = inv["id"].as_str().unwrap().to_owned();
            let key = generate_ephemeral_ed25519().unwrap();
            let req = format!(
                "{}\n",
                json!({"enrol": 1, "id": inv_id, "public_key": key.public_key, "name": "kill-step3"})
            );
            let reply = enrol::run_enrol_with_kill_point(
                &h.node,
                &inv_id,
                req.as_bytes(),
                enrol::EnrolKillPoint::AfterAuthKeysBeforeInviteRemoval,
            )
            .await;
            assert!(!reply.is_ok());

            let auth_before = std::fs::read_to_string(&h.auth_keys).unwrap();
            assert!(auth_before.contains("legion-device:kill-step3"));
            assert!(auth_before.contains(&format!("legion-invite:{inv_id}")));

            // Next mutation (`device.invite`) cleans up the leftover invite.
            let _inv2 = h
                .call(
                    "device.invite",
                    json!({"role": "observer", "name": "other-phone", "host": "tower.example", "user": "me"}),
                )
                .await
                .unwrap();

            let auth_after = std::fs::read_to_string(&h.auth_keys).unwrap();
            assert!(
                auth_after.contains("legion-device:kill-step3"),
                "permanent key line must stay intact:\n{auth_after}"
            );
            assert!(
                !auth_after.contains(&format!("legion-invite:{inv_id}")),
                "leftover invite must be removed by next mutation:\n{auth_after}"
            );
            let state = store::load_state(h.node.paths()).unwrap();
            assert!(!state.invites.contains_key(&inv_id));
            assert_eq!(state.unconfirmed.len(), 1);
        }
    }
}
