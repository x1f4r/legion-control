//! The `fleet` tool family: listing, pinging, editing, and discovering machines in `fleet.toml`.
//!
//! These tools hold a clone of [`Fleet`] so `fleet.ping` reuses the session pool and
//! `fleet.add`/`fleet.remove` evict stale connections after editing `fleet.toml`.

use std::collections::{BTreeMap, HashSet};
use std::marker::PhantomData;
use std::time::Duration;

use legion_core::config::{ConfigStore, NodeName};
use legion_core::{BoxFuture, Ctx, NoArgs, Registry, Tool, ToolError, input_schema, parse_args};
use legion_os::OsError;
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use legion_proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinSet;

use crate::config::{
    FLEET_LOCK_FILE, FleetFile, MachineEntry, RemoteOs, SystemEntry, Transport,
    add_missing_machines, remove_machine, upsert_machine, validate_ssh_target,
};
use crate::pool::{Fleet, NameMismatch};

/// Default `fleet.ping` timeout in seconds.
const DEFAULT_PING_TIMEOUT_S: f64 = 8.0;
/// Maximum `fleet.ping` timeout in seconds.
const MAX_PING_TIMEOUT_S: f64 = 60.0;
/// Timeout for running `tailscale status --json`.
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(10);

/// Registers `fleet.toml` with `meta.doctor` and adds every `fleet.*` tool, `fs.transfer`, and the
/// power tools that reach other machines, to `registry`.
pub fn register(registry: &mut Registry, fleet: Fleet) {
    registry.add_document::<FleetFile>();
    crate::transfer::register(registry, fleet.clone());
    crate::install::register(registry, fleet.clone());
    crate::power::register(registry, fleet.clone());
    crate::sync::register(registry, fleet.clone());
    crate::updates::register(registry, fleet.clone());

    registry.add(FleetTool::<NoArgs, ListResult, _>::new(
        "fleet.list",
        Effect::Read,
        "List the machines configured in fleet.toml, their systems and transport targets, without making network connections. Includes `name_mismatch` for any machine whose last observed node name differed from its fleet name.",
        fleet.clone(),
        |fleet, ctx, args| Box::pin(fleet_list(fleet, ctx, args)),
    ));
    registry.add(FleetTool::<PingArgs, PingResult, _>::new(
        "fleet.ping",
        Effect::Read,
        "Check whether machines in fleet.toml answer hello, concurrently, reporting their node name, system, version, platform, latency, and any `name_mismatch` when a machine answers under a different name than its fleet name.",
        fleet.clone(),
        |fleet, ctx, args| Box::pin(fleet_ping(fleet, ctx, args)),
    ));
    registry.add(FleetTool::<AddArgs, AddResult, _>::new(
        "fleet.add",
        Effect::Admin,
        "Add or replace a machine entry in fleet.toml atomically while preserving comments and unrelated entries.",
        fleet.clone(),
        |fleet, ctx, args| Box::pin(fleet_add(fleet, ctx, args)),
    ));
    registry.add(FleetTool::<RemoveArgs, RemoveResult, _>::new(
        "fleet.remove",
        Effect::Admin,
        "Remove a machine entry from fleet.toml atomically while preserving comments and unrelated entries.",
        fleet.clone(),
        |fleet, ctx, args| Box::pin(fleet_remove(fleet, ctx, args)),
    ));
    registry.add(FleetTool::<DiscoverArgs, DiscoverResult, _>::new(
        "fleet.discover",
        Effect::Read,
        "Discover peer machines from `tailscale status --json` using their MagicDNS names. Only peers belonging to this node's own Tailscale user (`Self.UserID`) are listed, and shared-in nodes (`ShareeNode`) are skipped so calls and `--on all` never reach machines owned by someone else. Tagged nodes are marked `addable: false` and skipped by `add: true` (add them explicitly with `fleet.add`). Pass `add: true` (requires admin permission) to add unlisted addable peers to fleet.toml.",
        fleet,
        |fleet, ctx, args| Box::pin(fleet_discover(fleet, ctx, args)),
    ));
}

struct FleetTool<A, O, F> {
    spec: ToolSpec,
    fleet: Fleet,
    handler: F,
    types: PhantomData<fn(A) -> O>,
}

impl<A, O, F> FleetTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a Fleet, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn new(name: &str, effect: Effect, description: &str, fleet: Fleet, handler: F) -> Self {
        Self {
            spec: ToolSpec {
                name: name.to_owned(),
                description: description.to_owned(),
                effect,
                input_schema: input_schema::<A>(),
            },
            fleet,
            handler,
            types: PhantomData,
        }
    }
}

impl<A, O, F> Tool for FleetTool<A, O, F>
where
    A: DeserializeOwned + JsonSchema + Send + 'static,
    O: Serialize + 'static,
    F: for<'a> Fn(&'a Fleet, &'a Ctx, A) -> BoxFuture<'a, Result<O, ToolError>>
        + Send
        + Sync
        + 'static,
{
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let parsed = parse_args::<A>(&self.spec.name, args)?;
            let out = (self.handler)(&self.fleet, ctx, parsed).await?;
            match serde_json::to_value(&out) {
                Ok(value @ Value::Object(_)) => Ok(value),
                Ok(_) => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("{} did not return a JSON object.", self.spec.name),
                )),
                Err(error) => Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{} returned a result that cannot be written as JSON: {error}.",
                        self.spec.name
                    ),
                )),
            }
        })
    }
}

/// What `fleet.list` returns.
#[derive(Debug, Serialize)]
struct ListResult {
    node: String,
    machines: Vec<ListedMachine>,
}

#[derive(Debug, Serialize)]
struct ListedMachine {
    name: String,
    this_node: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name_mismatch: Option<NameMismatch>,
    systems: Vec<ListedSystem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
    wake_via: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ListedSystem {
    name: String,
    via: &'static str,
    target: SystemTarget,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemTarget {
    Ssh(String),
    Command(Vec<String>),
}

async fn fleet_list(fleet: &Fleet, ctx: &Ctx, _args: NoArgs) -> Result<ListResult, ToolError> {
    let file = ctx.config().get::<FleetFile>()?;
    let here = ctx.node_name();
    let machines = file
        .machines
        .iter()
        .map(|(name, entry)| {
            let systems = entry
                .resolved_systems()
                .into_iter()
                .map(|sys| match sys.transport {
                    Transport::Ssh { target, .. } => ListedSystem {
                        name: sys.name,
                        via: "ssh",
                        target: SystemTarget::Ssh(target),
                    },
                    Transport::Command { argv } => ListedSystem {
                        name: sys.name,
                        via: "command",
                        target: SystemTarget::Command(argv),
                    },
                })
                .collect();
            let this_node = name == here;
            let name_mismatch = if this_node {
                None
            } else {
                fleet.last_reported_node(name).and_then(|reported| {
                    (reported.node != *name).then_some(NameMismatch {
                        reported: reported.node,
                        explicit: reported.explicit,
                    })
                })
            };
            ListedMachine {
                name: name.clone(),
                this_node,
                name_mismatch,
                systems,
                via: entry.via.clone(),
                mac: entry.mac.clone(),
                wake_via: entry.wake_via.clone(),
            }
        })
        .collect();
    Ok(ListResult {
        node: here.to_owned(),
        machines,
    })
}

/// Arguments of `fleet.ping`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PingArgs {
    /// Machine names to probe; every machine in fleet.toml when absent.
    #[serde(default)]
    machines: Option<Vec<String>>,
    /// Seconds to wait per machine, up to 60; 8 when absent.
    #[serde(default)]
    timeout_s: Option<f64>,
}

#[derive(Debug, Serialize)]
struct PingResult {
    results: Vec<PingEntry>,
}

#[derive(Debug, Serialize)]
struct PingEntry {
    machine: String,
    reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    node: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name_mismatch: Option<NameMismatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    os: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody>,
}

async fn fleet_ping(fleet: &Fleet, ctx: &Ctx, args: PingArgs) -> Result<PingResult, ToolError> {
    let timeout_s = args.timeout_s.unwrap_or(DEFAULT_PING_TIMEOUT_S);
    if !timeout_s.is_finite() || timeout_s <= 0.0 || timeout_s > MAX_PING_TIMEOUT_S {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fleet.ping timeout_s must be greater than 0 and at most {MAX_PING_TIMEOUT_S}, and {timeout_s} was given."
            ),
        )
        .with_hint("Give timeout_s as a positive number of seconds up to 60."));
    }
    let timeout = Duration::from_secs_f64(timeout_s);
    let file = ctx.config().get::<FleetFile>()?;
    let targets: Vec<String> = match args.machines {
        Some(names) => names,
        None => file.machines.keys().cloned().collect(),
    };

    let here = ctx.node_name().to_owned();
    let config_dir = ctx.config().dir().to_path_buf();
    let mut set = JoinSet::new();
    for (index, machine) in targets.into_iter().enumerate() {
        let fleet = fleet.clone();
        let here = here.clone();
        let config_dir = config_dir.clone();
        let cancel = ctx.cancel_token().child_token();
        set.spawn(async move {
            let store = legion_core::ConfigStore::new(config_dir);
            let outcome = fleet.ping(&store, &here, &machine, timeout, cancel).await;
            (index, outcome)
        });
    }

    let mut ordered = BTreeMap::new();
    while let Some(joined) = set.join_next().await {
        if let Ok((index, outcome)) = joined {
            ordered.insert(
                index,
                PingEntry {
                    machine: outcome.machine,
                    reachable: outcome.reachable,
                    node: outcome.node,
                    name_mismatch: outcome.name_mismatch,
                    system: outcome.system,
                    version: outcome.version,
                    os: outcome.os,
                    arch: outcome.arch,
                    latency_ms: outcome.latency_ms,
                    error: outcome.error,
                },
            );
        }
    }

    if ctx.is_cancelled() {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            "The call to fleet.ping was cancelled.",
        ));
    }

    Ok(PingResult {
        results: ordered.into_values().collect(),
    })
}

/// One bootable system of a machine, for `fleet.add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddSystemArgs {
    /// Name of this bootable system, such as `"linux"` or `"windows"`.
    name: String,
    /// SSH target accepted by the system ssh client. Give this or `command`, not both.
    #[serde(default)]
    ssh: Option<String>,
    /// Exact argv to start `legion rpc`. Give this or `ssh`, not both.
    #[serde(default)]
    command: Option<Vec<String>>,
    /// Remote `legion` binary name or path when it is not found the default way.
    #[serde(default)]
    program: Option<String>,
    /// Operating system hint (`"unix"` or `"windows"`).
    #[serde(default)]
    os: Option<RemoteOs>,
}

/// Arguments of `fleet.add`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// Machine name (lower-case letters, digits, `-` and `_`, starting with a letter or digit).
    name: String,
    /// SSH target accepted by the system ssh client. Give one of `ssh`, `command`, `systems` or `via`.
    #[serde(default)]
    ssh: Option<String>,
    /// Exact argv to start `legion rpc`. Give one of `ssh`, `command`, `systems` or `via`.
    #[serde(default)]
    command: Option<Vec<String>>,
    /// Multiple bootable systems for a dual-boot machine. Give one of `ssh`, `command`, `systems` or `via`.
    #[serde(default)]
    systems: Option<Vec<AddSystemArgs>>,
    /// Relay machine in `fleet.toml` through which calls to this machine are forwarded. Give one of `ssh`, `command`, `systems` or `via`.
    #[serde(default)]
    via: Option<String>,
    /// Remote `legion` binary name or path when it is not found the default way.
    #[serde(default)]
    program: Option<String>,
    /// Operating system hint (`"unix"` or `"windows"`).
    #[serde(default)]
    os: Option<RemoteOs>,
    /// Hardware MAC address for waking the machine, such as `"02:00:00:00:00:01"`.
    #[serde(default)]
    mac: Option<String>,
    /// Helper machines that can wake this machine.
    #[serde(default)]
    wake_via: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct AddResult {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    added: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replaced: Option<bool>,
}

async fn fleet_add(fleet: &Fleet, ctx: &Ctx, args: AddArgs) -> Result<AddResult, ToolError> {
    let entry = MachineEntry {
        ssh: args.ssh,
        command: args.command,
        program: args.program,
        os: args.os,
        systems: args.systems.map(|list| {
            list.into_iter()
                .map(|sys| SystemEntry {
                    name: sys.name,
                    ssh: sys.ssh,
                    command: sys.command,
                    program: sys.program,
                    os: sys.os,
                })
                .collect()
        }),
        via: args.via,
        mac: args.mac,
        wake_via: args.wake_via.unwrap_or_default(),
    };
    let config_dir = ctx.config().dir().to_path_buf();
    let lock_path = ctx.paths().state_dir().join(FLEET_LOCK_FILE);
    let name = args.name.clone();
    let replaced = tokio::task::spawn_blocking(move || {
        let store = ConfigStore::new(config_dir);
        upsert_machine(&store, &lock_path, &name, &entry)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Updating fleet.toml stopped unexpectedly: {err}."),
        )
    })??;
    fleet.clear_cached_node(&args.name);
    fleet.evict(&args.name).await;
    Ok(AddResult {
        name: args.name,
        added: (!replaced).then_some(true),
        replaced: replaced.then_some(true),
    })
}

/// Arguments of `fleet.remove`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArgs {
    /// Name of the machine to remove from fleet.toml.
    name: String,
}

#[derive(Debug, Serialize)]
struct RemoveResult {
    name: String,
    removed: bool,
}

async fn fleet_remove(
    fleet: &Fleet,
    ctx: &Ctx,
    args: RemoveArgs,
) -> Result<RemoveResult, ToolError> {
    let config_dir = ctx.config().dir().to_path_buf();
    let lock_path = ctx.paths().state_dir().join(FLEET_LOCK_FILE);
    let name = args.name.clone();
    tokio::task::spawn_blocking(move || {
        let store = ConfigStore::new(config_dir);
        remove_machine(&store, &lock_path, &name)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Updating fleet.toml stopped unexpectedly: {err}."),
        )
    })??;
    fleet.clear_cached_node(&args.name);
    fleet.evict(&args.name).await;
    Ok(RemoveResult {
        name: args.name,
        removed: true,
    })
}

/// Arguments of `fleet.discover`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DiscoverArgs {
    /// Add every addable discovered machine (same Tailscale user, not shared in, not tagged) that
    /// is not already listed in fleet.toml. Requires admin permission.
    #[serde(default)]
    add: bool,
}

/// One candidate machine parsed from `tailscale status --json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DiscoveredMachine {
    /// Normalized Legion machine name derived from the peer's MagicDNS label or host name.
    pub name: String,
    /// MagicDNS name used as the SSH target.
    pub ssh: String,
    /// Remote operating system reported by Tailscale (lower-cased).
    pub os: String,
    /// Whether Tailscale currently reports the peer as online.
    pub online: bool,
    /// Whether this machine name or SSH target is already listed in `fleet.toml`.
    pub already_listed: bool,
    /// Whether the peer carries Tailscale ACL tags.
    pub tagged: bool,
    /// Whether `fleet.discover` with `add = true` will add this peer by default (`false` for
    /// tagged nodes, which should be added explicitly with `fleet.add`).
    pub addable: bool,
}

#[derive(Debug, Serialize)]
struct DiscoverResult {
    candidates: Vec<DiscoveredMachine>,
}

async fn fleet_discover(
    fleet: &Fleet,
    ctx: &Ctx,
    args: DiscoverArgs,
) -> Result<DiscoverResult, ToolError> {
    if args.add && !ctx.grant().allows_effect(Effect::Admin) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "{} may not use fleet.discover with add = true: its role {} does not allow effect:admin.",
                ctx.grant().principal(),
                ctx.grant().role(),
            ),
        )
        .with_hint(
            "Call fleet.discover without add, or ask the owner to allow effect:admin in policy.toml.",
        ));
    }

    let existing = ctx.config().get::<FleetFile>()?;
    let options = RunOptions::new(CommandSpec::argv("tailscale", ["status", "--json"]))
        .c_locale()
        .timeout(TAILSCALE_TIMEOUT)
        .cancel(ctx.cancel_token().child_token());

    let output = run(options).await.map_err(|err| match err {
        OsError::ProgramNotFound { .. } => ErrorBody::new(
            ErrorCode::Unsupported,
            "Cannot discover machines: `tailscale` is not installed or not on PATH.",
        )
        .with_hint("Install Tailscale, or add machines directly with fleet.add."),
        other => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("Cannot run `tailscale status --json`: {other}."),
        )
        .with_hint("Ensure Tailscale is installed and reachable, or add machines with fleet.add."),
    })?;

    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            "The call to fleet.discover was cancelled.",
        ));
    }
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "Running `tailscale status --json` timed out.",
        )
        .with_hint("Check that the Tailscale daemon is running, or add machines with fleet.add."));
    }
    if !output.success() {
        let detail = output.stderr.text.trim();
        let message = if detail.is_empty() {
            format!(
                "`tailscale status --json` failed (exit {:?}).",
                output.exit_code
            )
        } else {
            format!("`tailscale status --json` failed: {detail}")
        };
        return Err(ErrorBody::new(ErrorCode::Unsupported, message).with_hint(
            "Check that Tailscale is running and logged in (`tailscale status`), or add machines with fleet.add.",
        ));
    }

    let candidates = parse_tailscale_status(&output.stdout.text, &existing, ctx.node_name())?;

    if args.add {
        let to_add = addable_entries(&candidates);
        let config_dir = ctx.config().dir().to_path_buf();
        let lock_path = ctx.paths().state_dir().join(FLEET_LOCK_FILE);
        let added = tokio::task::spawn_blocking(move || {
            let store = ConfigStore::new(config_dir);
            add_missing_machines(&store, &lock_path, &to_add)
        })
        .await
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Updating fleet.toml stopped unexpectedly: {err}."),
            )
        })??;
        for name in &added {
            fleet.evict(name).await;
        }
    }

    Ok(DiscoverResult { candidates })
}

fn addable_entries(candidates: &[DiscoveredMachine]) -> Vec<(String, MachineEntry)> {
    candidates
        .iter()
        .filter(|c| c.addable && !c.already_listed)
        .map(|c| {
            let entry = MachineEntry {
                ssh: Some(c.ssh.clone()),
                os: infer_remote_os(&c.os),
                ..Default::default()
            };
            (c.name.clone(), entry)
        })
        .collect()
}

/// Parses a `tailscale status --json` document into sorted fleet discovery candidates.
///
/// Only peers owned by the same Tailscale user as `Self.UserID` are returned, shared-in peers
/// (`ShareeNode: true`) are skipped, and tagged peers (`Tags` non-empty) are marked
/// `addable = false` so `fleet.discover add=true` does not add them by default.
///
/// Pure function so it can be unit-tested exhaustively against canned Tailscale snapshots without
/// running `tailscale` or touching the network.
pub fn parse_tailscale_status(
    json: &str,
    existing: &FleetFile,
    this_node: &str,
) -> Result<Vec<DiscoveredMachine>, ErrorBody> {
    #[derive(Deserialize)]
    struct TailscaleStatus {
        #[serde(rename = "Self", default)]
        self_node: Option<TailscaleSelf>,
        #[serde(rename = "Peer", default)]
        peers: Option<BTreeMap<String, TailscalePeer>>,
    }

    #[derive(Deserialize)]
    struct TailscaleSelf {
        #[serde(rename = "UserID", default)]
        user_id: Option<i64>,
    }

    #[derive(Deserialize)]
    struct TailscalePeer {
        #[serde(rename = "UserID", default)]
        user_id: Option<i64>,
        #[serde(rename = "ShareeNode", default)]
        sharee_node: bool,
        #[serde(rename = "Tags", default)]
        tags: Option<Vec<String>>,
        #[serde(rename = "HostName", default)]
        host_name: String,
        #[serde(rename = "DNSName", default)]
        dns_name: String,
        #[serde(rename = "OS", default)]
        os: String,
        #[serde(rename = "Online", default)]
        online: bool,
    }

    let parsed: TailscaleStatus = serde_json::from_str(json).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Unsupported,
            format!("Could not parse `tailscale status --json` output: {err}."),
        )
        .with_hint("Ensure a supported version of the Tailscale CLI is installed.")
    })?;

    let self_user_id = parsed
        .self_node
        .and_then(|s| s.user_id)
        .filter(|&id| id > 0)
        .ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Unsupported,
                "Cannot filter Tailscale peers safely: `tailscale status --json` did not include a valid `Self.UserID`.",
            )
            .with_hint("Ensure Tailscale is logged in as a user account, or add machines explicitly with fleet.add.")
        })?;

    let mut seen_names = HashSet::new();
    let mut candidates = Vec::new();
    for peer in parsed.peers.unwrap_or_default().into_values() {
        if peer.sharee_node || peer.user_id != Some(self_user_id) {
            continue;
        }
        let ssh = peer.dns_name.trim().trim_end_matches('.').to_owned();
        if ssh.is_empty() || validate_ssh_target(&ssh).is_err() {
            continue;
        }
        let raw_label = ssh
            .split('.')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&peer.host_name);
        let name = NodeName::from_host_name(raw_label).to_string();
        if name == this_node || !seen_names.insert(name.clone()) {
            continue;
        }
        let already_listed = existing.machines.contains_key(&name) || existing.has_ssh_target(&ssh);
        let tagged = peer.tags.as_deref().is_some_and(|tags| !tags.is_empty());
        candidates.push(DiscoveredMachine {
            name,
            ssh,
            os: peer.os.to_lowercase(),
            online: peer.online,
            already_listed,
            tagged,
            addable: !tagged,
        });
    }
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(candidates)
}

fn infer_remote_os(tailscale_os: &str) -> Option<RemoteOs> {
    match tailscale_os.to_lowercase().as_str() {
        "windows" => Some(RemoteOs::Windows),
        "linux" | "macos" | "freebsd" | "openbsd" | "netbsd" => Some(RemoteOs::Unix),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use legion_core::config::parse;

    use super::*;

    const CANNED_TAILSCALE: &str = r#"{
        "BackendState": "Running",
        "Self": {
            "UserID": 1001,
            "HostName": "laptop",
            "DNSName": "laptop.tailnet.example.",
            "OS": "macOS",
            "Online": true
        },
        "Peer": {
            "nodekey:aaa": {
                "UserID": 1001,
                "HostName": "Tower",
                "DNSName": "tower.tailnet.example.",
                "OS": "linux",
                "Online": true
            },
            "nodekey:bbb": {
                "UserID": 1001,
                "HostName": "tower-win",
                "DNSName": "tower-win.tailnet.example.",
                "OS": "windows",
                "Online": false
            },
            "nodekey:ccc": {
                "UserID": 1001,
                "HostName": "pi",
                "DNSName": "pi.tailnet.example.",
                "OS": "linux",
                "Online": true
            },
            "nodekey:ddd": {
                "UserID": 1001,
                "HostName": "no-dns",
                "DNSName": "",
                "OS": "linux",
                "Online": true
            },
            "nodekey:eee": {
                "UserID": 2002,
                "HostName": "other-user",
                "DNSName": "other-user.tailnet.example.",
                "OS": "linux",
                "Online": true
            },
            "nodekey:fff": {
                "UserID": 1001,
                "ShareeNode": true,
                "HostName": "shared-box",
                "DNSName": "shared-box.other.example.",
                "OS": "linux",
                "Online": true
            },
            "nodekey:ggg": {
                "UserID": 1001,
                "Tags": ["tag:server"],
                "HostName": "tagged-box",
                "DNSName": "tagged-box.tailnet.example.",
                "OS": "linux",
                "Online": true
            }
        }
    }"#;

    #[test]
    fn parses_canned_tailscale_status_json_and_filters_unowned_shared_and_tagged_peers() {
        let existing = parse::<FleetFile>(
            Path::new("fleet.toml"),
            b"[machines.pi]\nssh = \"pi.tailnet.example\"\n",
        )
        .unwrap();

        let candidates = parse_tailscale_status(CANNED_TAILSCALE, &existing, "laptop").unwrap();
        assert_eq!(
            candidates,
            vec![
                DiscoveredMachine {
                    name: "pi".into(),
                    ssh: "pi.tailnet.example".into(),
                    os: "linux".into(),
                    online: true,
                    already_listed: true,
                    tagged: false,
                    addable: true,
                },
                DiscoveredMachine {
                    name: "tagged-box".into(),
                    ssh: "tagged-box.tailnet.example".into(),
                    os: "linux".into(),
                    online: true,
                    already_listed: false,
                    tagged: true,
                    addable: false,
                },
                DiscoveredMachine {
                    name: "tower".into(),
                    ssh: "tower.tailnet.example".into(),
                    os: "linux".into(),
                    online: true,
                    already_listed: false,
                    tagged: false,
                    addable: true,
                },
                DiscoveredMachine {
                    name: "tower-win".into(),
                    ssh: "tower-win.tailnet.example".into(),
                    os: "windows".into(),
                    online: false,
                    already_listed: false,
                    tagged: false,
                    addable: true,
                },
            ]
        );

        // `add = true` only adds untagged, unlisted same-user peers (never tagged-box or pi).
        let to_add: Vec<String> = addable_entries(&candidates)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(to_add, vec!["tower", "tower-win"]);

        // Missing Self.UserID fails closed so peers are never listed without verifying ownership.
        let no_self = r#"{"Peer": {"k": {"UserID": 1001, "DNSName": "x.example."}}}"#;
        let err = parse_tailscale_status(no_self, &existing, "laptop").unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
    }

    #[test]
    fn invalid_tailscale_json_returns_unsupported() {
        let err = parse_tailscale_status("not json", &FleetFile::default(), "laptop").unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
    }

    #[test]
    fn every_fleet_tool_schema_is_one_a_strict_client_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = legion_core::Paths::under(dir.path());
        let mut registry = Registry::new();
        register(&mut registry, Fleet::new(&paths));

        let mut found = Vec::new();
        for spec in registry.specs() {
            if spec.input_schema["type"] != serde_json::json!("object") {
                found.push(format!("{}: the schema is not an object schema", spec.name));
            }
            found.extend(
                legion_core::schema::problems(&spec.input_schema)
                    .into_iter()
                    .map(|problem| format!("{}: {problem}", spec.name)),
            );
        }
        assert!(found.is_empty(), "{}", found.join("\n"));
    }
}
