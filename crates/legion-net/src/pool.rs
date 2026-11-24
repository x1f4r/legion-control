//! Session pool and [`legion_core::Router`] implementation for reaching other Legion nodes.
//!
//! [`Fleet`] keeps one multiplexed [`Session`] per remote machine, ensures concurrent first calls
//! open only one session, bounds concurrent new connections during fan-outs, remembers the last
//! responding system and SSH OS shape in the state cache, enforces the multi-hop limit, tags
//! routed events with their source machine, and closes every session cleanly on shutdown or drop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_core::config::ConfigStore;
use legion_core::policy::Policy;
use legion_core::{
    BoxFuture, Call, CancellationToken, Ctx, EventSink, MachineEffects, Node, Paths, RouteOutcome,
    Router, VERSION,
};
use legion_proto::{CallParams, Effect, ErrorBody, ErrorCode, Target};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::{FleetFile, MachineEntry, RemoteOs, ResolvedSystem, Transport};
use crate::connector::{Pipe, Subcommand};
use crate::session::{CallError, OpenError, Session};
use crate::state::StateStore;

/// Maximum number of node-to-node forwards allowed for one call so routing loops cannot spin.
pub const MAX_HOPS: u32 = 3;

/// Maximum number of new remote connections opened concurrently during a fan-out or ping.
const MAX_CONCURRENT_CONNECTS: usize = 8;

/// Default connect timeout for a single-system machine.
const SINGLE_SYSTEM_TIMEOUT: Duration = Duration::from_secs(10);

/// Shorter per-system connect timeout when probing a multi-system machine.
const MULTI_SYSTEM_TIMEOUT: Duration = Duration::from_secs(4);

/// Outcome of probing one machine with [`Fleet::ping`].
#[derive(Clone, Debug)]
pub struct PingOutcome {
    /// Fleet machine name that was probed.
    pub machine: String,
    /// Whether the machine answered `hello`.
    pub reachable: bool,
    /// Node name reported by `hello`, when reachable.
    pub node: Option<String>,
    /// System name that answered, when the machine defines named `[[systems]]`.
    pub system: Option<String>,
    /// Legion version reported by `hello`, when reachable.
    pub version: Option<String>,
    /// Remote operating system, when reachable.
    pub os: Option<String>,
    /// Remote CPU architecture, when reachable.
    pub arch: Option<String>,
    /// Round-trip latency in milliseconds, when reachable.
    pub latency_ms: Option<u64>,
    /// Why the machine could not be reached, when `reachable` is `false`.
    pub error: Option<ErrorBody>,
}

/// Pooled handle to the fleet's remote sessions, implementing [`legion_core::Router`].
///
/// Cloning is cheap and shares the same session pool. Doors call [`Router::shutdown`] (which calls
/// [`Fleet::close`]) before exiting; dropping the last clone also closes sessions synchronously.
#[derive(Clone)]
pub struct Fleet {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    slots: Mutex<HashMap<String, Arc<MachineSlot>>>,
    connect_permits: Arc<Semaphore>,
    state: StateStore,
    /// Bumped on [`Fleet::close`] so an in-flight connect started before shutdown never installs a
    /// session afterwards.
    generation: AtomicU64,
}

struct MachineSlot {
    /// Serializes opening a new session for this machine so concurrent first calls share one.
    open_lock: tokio::sync::Mutex<()>,
    current: Mutex<Option<LiveEntry>>,
}

#[derive(Clone)]
struct LiveEntry {
    session: Arc<Session>,
    /// Config snapshot that opened this session; if `fleet.toml` changes, the session is replaced.
    systems_fingerprint: Vec<ResolvedSystem>,
    /// The concrete system and OS that succeeded when this session opened (reused by `open_pipe`).
    active_system: ResolvedSystem,
    active_os: Option<RemoteOs>,
}

impl Fleet {
    /// Creates a new session pool using `paths.state_dir()` for its system/OS hint cache.
    pub fn new(paths: &Paths) -> Fleet {
        Fleet {
            inner: Arc::new(PoolInner {
                slots: Mutex::new(HashMap::new()),
                connect_permits: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTS)),
                state: StateStore::new(paths.state_dir()),
                generation: AtomicU64::new(0),
            }),
        }
    }

    /// Gets the live pooled JSON-RPC session for `machine` from inside a tool, opening one if none
    /// is open yet. Concurrent first calls for the same machine share one connect attempt.
    pub async fn session(&self, ctx: &Ctx, machine: &str) -> Result<Arc<Session>, ErrorBody> {
        self.session_for(
            ctx.config(),
            ctx.node_name(),
            machine,
            ctx.cancel_token().clone(),
        )
        .await
    }

    /// Gets the live pooled JSON-RPC session for `machine` using a [`ConfigStore`] directly.
    pub async fn session_for(
        &self,
        config: &ConfigStore,
        here: &str,
        machine: &str,
        cancel: CancellationToken,
    ) -> Result<Arc<Session>, ErrorBody> {
        let fleet_file = config.get::<FleetFile>()?;
        let entry = lookup_machine(&fleet_file, machine, here)?;
        if let Some(relay) = &entry.via {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!("Machine {machine} is reached via relay {relay}, not through a direct session."),
            )
            .with_hint(format!(
                "Use Fleet::call_remote for relayed calls to {machine}, or open a direct session to {relay}."
            ))
            .with_machine(here));
        }
        self.get_or_open(machine, entry, None, cancel)
            .await
            .map(|live| live.session)
    }

    /// Runs a tool call on a remote `machine` from inside a tool, authorising `ctx.principal()`
    /// against this node's live policy, writing one audit line on this node, and forwarding
    /// `ctx.principal()`, `ctx.hops() + 1`, cancellation, and events (tagged with `machine`),
    /// retrying once only if the session died before any byte was written.
    pub async fn call_remote(
        &self,
        ctx: &Ctx,
        machine: &str,
        tool: &str,
        args: Value,
        events: EventSink,
    ) -> Result<Value, ErrorBody> {
        let started = Instant::now();
        let node = ctx.node();
        let here = ctx.node_name();
        let principal = ctx.principal().to_owned();
        let result = self
            .call_remote_checked(ctx, node, here, &principal, machine, tool, &args, events)
            .await;
        node.record_audit(
            machine.to_owned(),
            Some(principal),
            ctx.door(),
            tool.to_owned(),
            &args,
            started,
            &result,
        )
        .await;
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn call_remote_checked(
        &self,
        ctx: &Ctx,
        node: &Node,
        here: &str,
        principal: &str,
        machine: &str,
        tool: &str,
        args: &Value,
        events: EventSink,
    ) -> Result<Value, ErrorBody> {
        let auth = authorize_principal_for_tool(node, here, principal, tool)?;
        let next_hops = ctx.hops().saturating_add(1);
        if next_hops > MAX_HOPS {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Forwarding {tool} to {machine} exceeded the maximum of {MAX_HOPS} hops (possible routing loop)."
                ),
            )
            .with_hint(
                "Check fleet.toml and node.toml on the machines along the path so they do not forward to each other in a cycle.",
            )
            .with_machine(machine));
        }
        let fleet_file = ctx
            .config()
            .get::<FleetFile>()
            .map_err(|err| ensure_machine(err, here))?;
        let (direct_machine, direct_entry, relayed) = resolve_route(&fleet_file, machine, here)?;
        if relayed && direct_machine == here {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Machine {machine} relays through {here}, which is this node."),
            )
            .with_machine(here));
        }
        if let ForwardAuth::NeedsRemoteEffect { policy, .. } = auth {
            if relayed {
                policy
                    .authorize_name(principal, tool, None, node.paths())
                    .map_err(|err| err.with_machine(here))?;
            } else {
                let effect = self
                    .query_remote_effect(
                        direct_machine,
                        direct_entry,
                        tool,
                        ctx.cancel_token().clone(),
                    )
                    .await?;
                policy
                    .authorize_name(principal, tool, effect, node.paths())
                    .map_err(|err| err.with_machine(here))?;
            }
        }
        let params = CallParams {
            tool: tool.to_owned(),
            args: args.clone(),
            as_principal: Some(principal.to_owned()),
            on: relayed.then(|| Target::One(machine.to_owned())),
            hops: next_hops,
        };
        self.execute_on_machine(
            direct_machine,
            direct_entry,
            machine,
            relayed,
            params,
            ctx.cancel_token().clone(),
            events.for_machine(machine),
        )
        .await
    }

    /// Opens a fresh dedicated pipe to `machine` running a fixed remote [`Subcommand`] (such as
    /// [`Subcommand::Stream`] for `legion stream` bulk file transfer) from inside a tool.
    ///
    /// Authorises `ctx.principal()` against this node's live policy before spawning any pipe and
    /// writes one audit line on this node for the attempt.
    ///
    /// When the machine has multiple systems or an SSH transport with no cached OS shape, the pool
    /// first verifies the active system and OS through [`Fleet::session`] so the pipe targets the
    /// running system directly.
    pub async fn open_pipe(
        &self,
        ctx: &Ctx,
        machine: &str,
        subcommand: Subcommand,
    ) -> Result<Pipe, ErrorBody> {
        let (audit_tool, required_tool, required_effect, sub_name) = match subcommand {
            Subcommand::Stream => ("stream.open", "fs.transfer", Some(Effect::Write), "stream"),
            Subcommand::Rpc => ("rpc.open", "fleet.rpc", None, "rpc"),
        };
        let args = json!({ "subcommand": sub_name });
        self.open_pipe_checked(
            ctx,
            machine,
            subcommand,
            audit_tool,
            &[(required_tool, required_effect)],
            &args,
        )
        .await
    }

    /// Opens a dedicated `legion stream` pipe for `fs.transfer`, checking this node's live policy
    /// for both `fs.transfer` and the stream operation's underlying file tool (`fs.read`,
    /// `fs.write`, or `fs.remove`), and writing one audit line on this node.
    pub(crate) async fn open_stream_pipe(
        &self,
        ctx: &Ctx,
        machine: &str,
        op: &str,
        underlying_tool: &str,
        underlying_effect: Effect,
        recorded_args: &Value,
    ) -> Result<Pipe, ErrorBody> {
        let audit_tool = format!("stream.{op}");
        self.open_pipe_checked(
            ctx,
            machine,
            Subcommand::Stream,
            &audit_tool,
            &[
                ("fs.transfer", Some(Effect::Write)),
                (underlying_tool, Some(underlying_effect)),
            ],
            recorded_args,
        )
        .await
    }

    async fn open_pipe_checked(
        &self,
        ctx: &Ctx,
        machine: &str,
        subcommand: Subcommand,
        audit_tool: &str,
        required: &[(&str, Option<Effect>)],
        recorded_args: &Value,
    ) -> Result<Pipe, ErrorBody> {
        let started = Instant::now();
        let node = ctx.node();
        let here = ctx.node_name();
        let principal = ctx.principal().to_owned();
        let result = match authorize_pipe_open(node, here, &principal, required) {
            Ok(()) => {
                self.open_pipe_for(
                    ctx.config(),
                    here,
                    machine,
                    subcommand,
                    ctx.cancel_token().clone(),
                )
                .await
            }
            Err(err) => Err(err),
        };
        let audit_outcome = match &result {
            Ok(_) => Ok(Value::Null),
            Err(err) => Err(err.clone()),
        };
        node.record_audit(
            machine.to_owned(),
            Some(principal),
            ctx.door(),
            audit_tool.to_owned(),
            recorded_args,
            started,
            &audit_outcome,
        )
        .await;
        result
    }

    /// Opens a fresh dedicated pipe to `machine` running `subcommand`, using a [`ConfigStore`]
    /// directly.
    pub async fn open_pipe_for(
        &self,
        config: &ConfigStore,
        here: &str,
        machine: &str,
        subcommand: Subcommand,
        cancel: CancellationToken,
    ) -> Result<Pipe, ErrorBody> {
        if machine == here {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{machine} is this node; a remote pipe is only needed for another machine."
                ),
            )
            .with_machine(machine));
        }
        let fleet_file = config
            .get::<FleetFile>()
            .map_err(|err| ensure_machine(err, here))?;
        let entry = lookup_machine(&fleet_file, machine, here)?;
        if let Some(relay) = &entry.via {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!("Machine {machine} is reached via relay {relay}, so a direct pipe cannot be opened to it."),
            )
            .with_hint(format!(
                "Run transfers involving {machine} on {relay} instead (for example `legion fs.transfer --on {relay} from=... to=...`), or configure a direct ssh or command transport for {machine} in fleet.toml."
            ))
            .with_machine(here));
        }
        let systems = entry.resolved_systems();

        // Reuse the verified active system and OS from an already-open session when available.
        let slot = self.slot(machine);
        let known = {
            let guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.as_ref().and_then(|live| {
                (live.session.is_alive() && live.systems_fingerprint == systems)
                    .then(|| (live.active_system.clone(), live.active_os))
            })
        };

        let (system, resolved_os) = if let Some(pair) = known {
            pair
        } else if systems.len() == 1 {
            let only = systems[0].clone();
            match only.transport {
                Transport::Command { .. } => (only, None),
                Transport::Ssh { os: Some(os), .. } => (only, Some(os)),
                Transport::Ssh { os: None, .. } => {
                    if let Some(cached) = self.inner.state.last_os(machine, &only.name) {
                        (only, Some(cached))
                    } else {
                        let live = self
                            .get_or_open(machine, entry, None, cancel.clone())
                            .await?;
                        (live.active_system, live.active_os)
                    }
                }
            }
        } else {
            let live = self
                .get_or_open(machine, entry, None, cancel.clone())
                .await?;
            (live.active_system, live.active_os)
        };

        let _permit = acquire_connect_permit(&self.inner.connect_permits, machine, &cancel).await?;
        let pipe = match &system.transport {
            Transport::Command { argv } => Pipe::spawn_command(argv, subcommand),
            Transport::Ssh {
                target, program, ..
            } => {
                let os = resolved_os.unwrap_or(RemoteOs::Unix);
                Pipe::spawn_ssh(
                    target,
                    os,
                    subcommand,
                    program.as_deref(),
                    SINGLE_SYSTEM_TIMEOUT,
                )
            }
        }
        .map_err(|err| err.with_machine(machine))?;

        Ok(pipe)
    }

    /// Runs `call` on a single named machine (locally if `machine` is this node, otherwise over
    /// the pooled session, or through an explicit `via` relay).
    ///
    /// Before forwarding, this node authorises the call against its own policy for the caller's
    /// principal and appends one audit line for the forward with the target machine and outcome.
    /// A node never runs a call locally when `machine` is not this node and is missing from
    /// `fleet.toml`: unknown names fail with `not_found` carrying this node's name.
    pub async fn call_machine(
        &self,
        node: &Node,
        machine: &str,
        call: Call,
    ) -> Result<Value, ErrorBody> {
        let here = node.name();
        if machine == here {
            return node.call(call).await;
        }
        let started = Instant::now();
        let auth = match authorize_for_forward(node, &call) {
            Ok(auth) => auth,
            Err((principal, err)) => {
                node.record_audit(
                    machine.to_owned(),
                    principal,
                    call.door,
                    call.tool,
                    &call.args,
                    started,
                    &Err(err.clone()),
                )
                .await;
                return Err(err);
            }
        };
        self.call_remote_authorized(node, machine, call, auth, started)
            .await
    }

    async fn call_remote_authorized(
        &self,
        node: &Node,
        machine: &str,
        call: Call,
        auth: ForwardAuth,
        started: Instant,
    ) -> Result<Value, ErrorBody> {
        let here = node.name();
        let door = call.door;
        let tool = call.tool.clone();
        let args = call.args.clone();
        let principal = auth.principal().to_owned();
        let result = match node
            .config()
            .get::<FleetFile>()
            .map_err(|err| ensure_machine(err, &here))
            .and_then(|fleet_file| {
                let (direct_machine, direct_entry, relayed) =
                    resolve_route(&fleet_file, machine, &here)?;
                Ok((direct_machine.to_owned(), direct_entry.clone(), relayed))
            }) {
            Ok((direct_machine, direct_entry, relayed)) => {
                self.forward_authorized(
                    node,
                    machine,
                    &direct_machine,
                    &direct_entry,
                    relayed,
                    call,
                    auth,
                )
                .await
            }
            Err(err) => Err(err),
        };
        node.record_audit(
            machine.to_owned(),
            Some(principal),
            door,
            tool,
            &args,
            started,
            &result,
        )
        .await;
        result
    }

    /// Probes one machine with `hello` (or `meta.version` through its relay when configured with
    /// `via`), returning structured reachability and latency data.
    pub async fn ping(
        &self,
        config: &ConfigStore,
        here: &str,
        machine: &str,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> PingOutcome {
        if machine == here {
            return PingOutcome {
                machine: machine.to_owned(),
                reachable: true,
                node: Some(here.to_owned()),
                system: None,
                version: Some(VERSION.to_owned()),
                os: Some(std::env::consts::OS.to_owned()),
                arch: Some(std::env::consts::ARCH.to_owned()),
                latency_ms: Some(0),
                error: None,
            };
        }

        let fleet_file = match config.get::<FleetFile>() {
            Ok(file) => file,
            Err(err) => {
                return ping_failed(machine, ensure_machine(err, here));
            }
        };
        let (direct_machine, direct_entry, relayed) =
            match resolve_route(&fleet_file, machine, here) {
                Ok(route) => route,
                Err(err) => return ping_failed(machine, err),
            };

        let started = Instant::now();
        if relayed {
            if direct_machine == here {
                return ping_failed(
                    machine,
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!("Machine {machine} relays through {here}, which is this node."),
                    )
                    .with_machine(here),
                );
            }
            let params = CallParams {
                tool: "meta.version".to_owned(),
                args: serde_json::json!({}),
                as_principal: None,
                on: Some(Target::One(machine.to_owned())),
                hops: 1,
            };
            let fut = self.execute_on_machine(
                direct_machine,
                direct_entry,
                machine,
                true,
                params,
                cancel,
                EventSink::discard(),
            );
            return match tokio::time::timeout(timeout, fut).await {
                Ok(Ok(value)) => PingOutcome {
                    machine: machine.to_owned(),
                    reachable: true,
                    node: value.get("node").and_then(Value::as_str).map(str::to_owned),
                    system: None,
                    version: value
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    os: value.get("os").and_then(Value::as_str).map(str::to_owned),
                    arch: value.get("arch").and_then(Value::as_str).map(str::to_owned),
                    latency_ms: Some(
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    ),
                    error: None,
                },
                Ok(Err(err)) => ping_failed(machine, ensure_machine(err, machine)),
                Err(_) => ping_failed(
                    machine,
                    ErrorBody::new(
                        ErrorCode::Unreachable,
                        format!(
                            "Timed out waiting for {machine} after {}s.",
                            timeout.as_secs_f64()
                        ),
                    )
                    .with_machine(machine),
                ),
            };
        }

        let entry = direct_entry;
        let systems = entry.resolved_systems();
        let slot = self.slot(machine);
        let existing = {
            let guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.as_ref().and_then(|live| {
                (live.session.is_alive() && live.systems_fingerprint == systems)
                    .then(|| live.clone())
            })
        };

        // If a live session is already pooled, send a live hello on it so ping checks real
        // responsiveness rather than returning stale handshake metadata.
        if let Some(live) = existing {
            match live.session.hello(timeout, cancel.clone()).await {
                Ok(hello) => {
                    return ping_ok(machine, hello, live.session.info().system.clone(), started);
                }
                Err(CallError::Remote(err)) => {
                    return ping_failed(machine, ensure_machine(err, machine));
                }
                Err(CallError::NotWritten(_) | CallError::LostInFlight(_)) => {
                    self.invalidate_if_same(&slot, &live.session);
                }
            }
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return ping_failed(
                machine,
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    format!(
                        "Timed out waiting for {machine} after {}s.",
                        timeout.as_secs_f64()
                    ),
                )
                .with_machine(machine),
            );
        }

        match self
            .get_or_open(machine, entry, Some(remaining), cancel)
            .await
        {
            Ok(live) => {
                let info = live.session.info();
                ping_ok(machine, info.hello.clone(), info.system.clone(), started)
            }
            Err(err) => ping_failed(machine, ensure_machine(err, machine)),
        }
    }

    /// Closes and removes any cached session for `machine`, e.g. after editing `fleet.toml`.
    pub async fn evict(&self, machine: &str) {
        let removed = {
            let mut slots = self
                .inner
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slots.remove(machine)
        };
        if let Some(slot) = removed {
            let session = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(live) = session {
                live.session.close().await;
            }
        }
    }

    /// Closes every open session cleanly (EOF on stdin, short wait, then kill).
    pub async fn close(&self) {
        self.inner.generation.fetch_add(1, Ordering::AcqRel);
        let slots: Vec<Arc<MachineSlot>> = {
            let mut map = self
                .inner
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.drain().map(|(_, slot)| slot).collect()
        };
        let mut closing = JoinSet::new();
        for slot in slots {
            let live = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(live) = live {
                closing.spawn(async move {
                    live.session.close().await;
                });
            }
        }
        while closing.join_next().await.is_some() {}
    }

    #[allow(clippy::too_many_arguments)]
    async fn forward_authorized(
        &self,
        node: &Node,
        machine: &str,
        direct_machine: &str,
        direct_entry: &MachineEntry,
        relayed: bool,
        call: Call,
        auth: ForwardAuth,
    ) -> Result<Value, ErrorBody> {
        let here = node.name();
        if relayed && direct_machine == here {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Machine {machine} relays through {here}, which is this node."),
            )
            .with_machine(&here));
        }

        let next_hops = call.hops.saturating_add(1);
        if next_hops > MAX_HOPS {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Forwarding {} to {machine} exceeded the maximum of {MAX_HOPS} hops (possible routing loop).",
                    call.tool
                ),
            )
            .with_hint(
                "Check fleet.toml and node.toml on the machines along the path so they do not forward to each other in a cycle.",
            )
            .with_machine(machine));
        }

        let principal = match auth {
            ForwardAuth::Authorized { principal } => principal,
            ForwardAuth::NeedsRemoteEffect { principal, policy } => {
                if relayed {
                    // Cannot inspect the final target's tool list through a relay before forwarding,
                    // so its effect is unknown here and only a `*` role may forward it.
                    policy
                        .authorize_name(&principal, &call.tool, None, node.paths())
                        .map_err(|err| err.with_machine(&here))?;
                    principal
                } else {
                    let effect = self
                        .query_remote_effect(
                            direct_machine,
                            direct_entry,
                            &call.tool,
                            call.cancel.clone(),
                        )
                        .await?;
                    policy
                        .authorize_name(&principal, &call.tool, effect, node.paths())
                        .map_err(|err| err.with_machine(&here))?;
                    principal
                }
            }
        };

        let params = CallParams {
            tool: call.tool,
            args: call.args,
            as_principal: Some(principal),
            // Direct forwards send no `on` so the receiver runs the call locally regardless of
            // what it calls itself. Explicit `via` relays send `on` naming the final target.
            on: relayed.then(|| Target::One(machine.to_owned())),
            hops: next_hops,
        };

        self.execute_on_machine(
            direct_machine,
            direct_entry,
            machine,
            relayed,
            params,
            call.cancel,
            call.events.for_machine(machine),
        )
        .await
    }

    async fn query_remote_effect(
        &self,
        machine: &str,
        entry: &MachineEntry,
        tool: &str,
        cancel: CancellationToken,
    ) -> Result<Option<legion_proto::Effect>, ErrorBody> {
        let slot = self.slot(machine);
        let live = self
            .get_or_open(machine, entry, None, cancel.clone())
            .await
            .map_err(|err| err.with_machine(machine))?;
        match live.session.tool_effect(tool, cancel.clone()).await {
            Ok(effect) => Ok(effect),
            Err(CallError::NotWritten(_)) => {
                self.invalidate_if_same(&slot, &live.session);
                let fresh = self
                    .get_or_open(machine, entry, None, cancel.clone())
                    .await
                    .map_err(|err| err.with_machine(machine))?;
                match fresh.session.tool_effect(tool, cancel).await {
                    Ok(effect) => Ok(effect),
                    Err(CallError::LostInFlight(err) | CallError::NotWritten(err)) => {
                        self.invalidate_if_same(&slot, &fresh.session);
                        Err(err.with_machine(machine))
                    }
                    Err(CallError::Remote(err)) => Err(err.with_machine(machine)),
                }
            }
            Err(CallError::LostInFlight(err)) => {
                self.invalidate_if_same(&slot, &live.session);
                Err(err.with_machine(machine))
            }
            Err(CallError::Remote(err)) => Err(err.with_machine(machine)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_on_machine(
        &self,
        direct_machine: &str,
        direct_entry: &MachineEntry,
        target_machine: &str,
        relayed: bool,
        params: CallParams,
        cancel: CancellationToken,
        events: EventSink,
    ) -> Result<Value, ErrorBody> {
        let label_error = |err: ErrorBody| {
            if relayed
                && err.code == ErrorCode::NotFound
                && err.machine.as_deref() == Some(direct_machine)
            {
                err.with_machine(direct_machine)
            } else {
                err.with_machine(target_machine)
            }
        };
        let slot = self.slot(direct_machine);
        let live = self
            .get_or_open(direct_machine, direct_entry, None, cancel.clone())
            .await
            .map_err(&label_error)?;

        match live
            .session
            .call(params.clone(), cancel.clone(), events.clone())
            .await
        {
            Ok(value) => Ok(value),
            Err(CallError::NotWritten(_)) => {
                // Request was never written to the dead session: reopen once and retry.
                self.invalidate_if_same(&slot, &live.session);
                let fresh = self
                    .get_or_open(direct_machine, direct_entry, None, cancel.clone())
                    .await
                    .map_err(&label_error)?;
                match fresh.session.call(params, cancel, events).await {
                    Ok(value) => Ok(value),
                    Err(CallError::LostInFlight(err) | CallError::NotWritten(err)) => {
                        self.invalidate_if_same(&slot, &fresh.session);
                        Err(label_error(err))
                    }
                    Err(CallError::Remote(err)) => Err(label_error(err)),
                }
            }
            Err(CallError::LostInFlight(err)) => {
                self.invalidate_if_same(&slot, &live.session);
                Err(label_error(err))
            }
            Err(CallError::Remote(err)) => Err(label_error(err)),
        }
    }

    async fn get_or_open(
        &self,
        machine: &str,
        entry: &MachineEntry,
        timeout_override: Option<Duration>,
        cancel: CancellationToken,
    ) -> Result<LiveEntry, ErrorBody> {
        let systems = entry.resolved_systems();
        if systems.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Machine {machine} in fleet.toml has no configured transport."),
            )
            .with_machine(machine));
        }

        let start_gen = self.inner.generation.load(Ordering::Acquire);
        let slot = self.slot(machine);
        // Fast path: reuse an open, healthy session whose config still matches.
        {
            let guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = guard.as_ref()
                && live.session.is_alive()
                && live.systems_fingerprint == systems
            {
                return Ok(live.clone());
            }
        }

        // Slow path: serialize session opening per machine so concurrent first calls spawn once.
        let _open_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("Connecting to {machine} was cancelled."),
                )
                .with_machine(machine));
            }
            guard = slot.open_lock.lock() => guard,
        };

        // Re-check now that we hold the per-machine open lock.
        let stale = {
            let mut guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = guard.as_ref()
                && live.session.is_alive()
                && live.systems_fingerprint == systems
            {
                return Ok(live.clone());
            }
            guard.take()
        };
        if let Some(old) = stale {
            old.session.close().await;
        }

        let _permit = acquire_connect_permit(&self.inner.connect_permits, machine, &cancel).await?;

        let (session, active_system, active_os) = self
            .open_machine_systems(machine, &systems, timeout_override, cancel)
            .await?;

        let live = LiveEntry {
            session: Arc::new(session),
            systems_fingerprint: systems,
            active_system,
            active_os,
        };
        if self.inner.generation.load(Ordering::Acquire) == start_gen {
            *slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(live.clone());
            Ok(live)
        } else {
            live.session.close().await;
            Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("The fleet session pool shut down while connecting to {machine}."),
            )
            .with_machine(machine))
        }
    }

    async fn open_machine_systems(
        &self,
        machine: &str,
        systems: &[ResolvedSystem],
        timeout_override: Option<Duration>,
        cancel: CancellationToken,
    ) -> Result<(Session, ResolvedSystem, Option<RemoteOs>), ErrorBody> {
        let multi = systems.len() > 1;
        let ordered = self.order_systems(machine, systems);
        let per_system_timeout = match timeout_override {
            Some(total) if multi => {
                let count = u32::try_from(ordered.len()).unwrap_or(1).max(1);
                (total / count)
                    .min(MULTI_SYSTEM_TIMEOUT)
                    .max(Duration::from_millis(200).min(total))
            }
            Some(total) => total,
            None if multi => MULTI_SYSTEM_TIMEOUT,
            None => SINGLE_SYSTEM_TIMEOUT,
        };

        let mut failures: Vec<(String, ErrorBody)> = Vec::new();
        for sys in ordered {
            match self
                .open_one_system(machine, &sys, per_system_timeout, cancel.clone())
                .await
            {
                Ok((session, worked_os)) => {
                    if sys.explicit {
                        self.inner.state.record_system(machine, &sys.name);
                    }
                    if let Some(os) = worked_os {
                        self.inner.state.record_os(machine, &sys.name, os);
                    }
                    return Ok((session, sys, worked_os));
                }
                Err(OpenError::Unsupported(err)) | Err(OpenError::Cancelled(err)) => {
                    return Err(ensure_machine(err, machine));
                }
                Err(OpenError::Unreachable { error, .. }) => {
                    failures.push((sys.name.clone(), error));
                }
            }
        }

        if failures.len() == 1
            && let Some((_, err)) = failures.pop()
        {
            return Err(ensure_machine(err, machine));
        }

        let summary = failures
            .iter()
            .map(|(name, err)| format!("{name}: {}", err.message))
            .collect::<Vec<_>>()
            .join("; ");
        let hints = failures
            .iter()
            .filter_map(|(name, err)| err.hint.as_deref().map(|h| format!("{name}: {h}")))
            .collect::<Vec<_>>()
            .join("; ");
        let mut combined = ErrorBody::new(
            ErrorCode::Unreachable,
            format!("None of {machine}'s systems answered ({summary})."),
        )
        .with_machine(machine);
        if !hints.is_empty() {
            combined = combined.with_hint(hints);
        }
        Err(combined)
    }

    async fn open_one_system(
        &self,
        machine: &str,
        sys: &ResolvedSystem,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<(Session, Option<RemoteOs>), OpenError> {
        let reported_system = sys.explicit.then(|| sys.name.clone());
        match &sys.transport {
            Transport::Command { argv } => {
                let pipe = Pipe::spawn_command(argv, Subcommand::Rpc).map_err(|err| {
                    OpenError::Unreachable {
                        error: err.with_machine(machine),
                        retry_other_os: false,
                    }
                })?;
                let session =
                    Session::open_pipe(pipe, machine, reported_system, timeout, cancel).await?;
                Ok((session, None))
            }
            Transport::Ssh {
                target,
                program,
                os,
            } => {
                let attempts: Vec<RemoteOs> = match *os {
                    Some(hinted) => vec![hinted],
                    None => {
                        let first = self
                            .inner
                            .state
                            .last_os(machine, &sys.name)
                            .unwrap_or_else(|| default_first_os(program.as_deref()));
                        vec![first, first.other()]
                    }
                };
                let mut last_err = None;
                for (index, candidate_os) in attempts.iter().copied().enumerate() {
                    let pipe = Pipe::spawn_ssh(
                        target,
                        candidate_os,
                        Subcommand::Rpc,
                        program.as_deref(),
                        timeout,
                    )
                    .map_err(|err| OpenError::Unreachable {
                        error: err.with_machine(machine),
                        retry_other_os: false,
                    })?;
                    match Session::open_pipe(
                        pipe,
                        machine,
                        reported_system.clone(),
                        timeout,
                        cancel.clone(),
                    )
                    .await
                    {
                        Ok(session) => {
                            let learned_os = if session.info().hello.os == "windows" {
                                RemoteOs::Windows
                            } else {
                                candidate_os
                            };
                            return Ok((session, Some(learned_os)));
                        }
                        Err(err @ OpenError::Unsupported(_))
                        | Err(err @ OpenError::Cancelled(_)) => return Err(err),
                        Err(OpenError::Unreachable {
                            error,
                            retry_other_os,
                        }) => {
                            let has_more = index + 1 < attempts.len();
                            if !retry_other_os || !has_more {
                                return Err(OpenError::Unreachable {
                                    error,
                                    retry_other_os,
                                });
                            }
                            last_err = Some(error);
                        }
                    }
                }
                Err(OpenError::Unreachable {
                    error: last_err.unwrap_or_else(|| {
                        ErrorBody::new(
                            ErrorCode::Unreachable,
                            format!("Could not reach {machine} over ssh."),
                        )
                        .with_machine(machine)
                    }),
                    retry_other_os: false,
                })
            }
        }
    }

    fn order_systems(&self, machine: &str, systems: &[ResolvedSystem]) -> Vec<ResolvedSystem> {
        let Some(last) = self.inner.state.last_system(machine) else {
            return systems.to_vec();
        };
        let mut ordered = Vec::with_capacity(systems.len());
        if let Some(preferred) = systems.iter().find(|s| s.name == last) {
            ordered.push(preferred.clone());
        }
        for sys in systems {
            if sys.name != last {
                ordered.push(sys.clone());
            }
        }
        ordered
    }

    fn slot(&self, machine: &str) -> Arc<MachineSlot> {
        let mut map = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(map.entry(machine.to_owned()).or_insert_with(|| {
            Arc::new(MachineSlot {
                open_lock: tokio::sync::Mutex::new(()),
                current: Mutex::new(None),
            })
        }))
    }

    fn invalidate_if_same(&self, slot: &MachineSlot, session: &Arc<Session>) {
        let mut guard = slot
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard
            .as_ref()
            .is_some_and(|live| Arc::ptr_eq(&live.session, session))
        {
            guard.take();
        }
    }

    async fn fan_out(&self, node: &Node, call: Call, machines: Vec<String>) -> RouteOutcome {
        let started = Instant::now();
        let here = node.name();
        let auth = match authorize_for_forward(node, &call) {
            Ok(auth) => auth,
            Err((principal, err)) => {
                let mut results = BTreeMap::new();
                for machine in machines {
                    node.record_audit(
                        machine.clone(),
                        principal.clone(),
                        call.door,
                        call.tool.clone(),
                        &call.args,
                        started,
                        &Err(err.clone()),
                    )
                    .await;
                    results.insert(machine, Err(err.clone()));
                }
                return RouteOutcome::Many(results);
            }
        };

        let mut set = JoinSet::new();
        for machine in machines {
            let fleet = self.clone();
            let node = node.clone();
            let here = here.clone();
            let auth = auth.clone();
            let sub_call = Call {
                tool: call.tool.clone(),
                args: call.args.clone(),
                as_principal: call.as_principal.clone(),
                door: call.door,
                pinned: call.pinned.clone(),
                cancel: call.cancel.child_token(),
                events: call.events.for_machine(machine.as_str()),
                hops: call.hops,
                credential_expires: call.credential_expires,
            };
            set.spawn(async move {
                let outcome = if machine == here {
                    node.call(sub_call).await
                } else {
                    fleet
                        .call_remote_authorized(&node, &machine, sub_call, auth, started)
                        .await
                        .map_err(|err| ensure_machine(err, &machine))
                };
                (machine, outcome)
            });
        }

        let mut results = BTreeMap::new();
        while let Some(joined) = set.join_next().await {
            if let Ok((machine, outcome)) = joined {
                results.insert(machine, outcome);
            }
        }
        RouteOutcome::Many(results)
    }
}

#[derive(Clone)]
enum ForwardAuth {
    /// Authorized locally by the local tool spec or a wildcard `*` role.
    Authorized { principal: String },
    /// Tool does not exist in this node's registry; check the target's reported effect before
    /// sending `call`.
    NeedsRemoteEffect {
        principal: String,
        policy: Arc<Policy>,
    },
}

impl ForwardAuth {
    fn principal(&self) -> &str {
        match self {
            ForwardAuth::Authorized { principal }
            | ForwardAuth::NeedsRemoteEffect { principal, .. } => principal,
        }
    }
}

fn authorize_for_forward(
    node: &Node,
    call: &Call,
) -> Result<ForwardAuth, (Option<String>, ErrorBody)> {
    let here = node.name();
    let policy = node
        .config()
        .get::<Policy>()
        .map_err(|err| (call.pinned.clone(), ensure_machine(err, &here)))?;
    let principal = match policy.resolve(call.as_principal.as_deref(), call.pinned.as_deref()) {
        Ok(principal) => principal,
        Err(err) => {
            let caller = call
                .pinned
                .clone()
                .unwrap_or_else(|| policy.default_principal().to_owned());
            return Err((Some(caller), err.with_machine(&here)));
        }
    };
    authorize_principal_with_policy(node, &here, &policy, &principal, &call.tool)
        .map_err(|err| (Some(principal), err))
}

fn authorize_principal_for_tool(
    node: &Node,
    here: &str,
    principal: &str,
    tool: &str,
) -> Result<ForwardAuth, ErrorBody> {
    let policy = node
        .config()
        .get::<Policy>()
        .map_err(|err| ensure_machine(err, here))?;
    authorize_principal_with_policy(node, here, &policy, principal, tool)
}

fn authorize_principal_with_policy(
    node: &Node,
    here: &str,
    policy: &Arc<Policy>,
    principal: &str,
    tool_name: &str,
) -> Result<ForwardAuth, ErrorBody> {
    if let Some(tool) = node.registry().get(tool_name) {
        policy
            .authorize(principal, tool.spec(), node.paths())
            .map_err(|err| err.with_machine(here))?;
        return Ok(ForwardAuth::Authorized {
            principal: principal.to_owned(),
        });
    }
    // Tool exists only on the target (or does not exist).
    match policy.authorize_name(principal, tool_name, None, node.paths()) {
        Ok(_) => Ok(ForwardAuth::Authorized {
            principal: principal.to_owned(),
        }),
        Err(denied) => {
            if policy.could_allow_with_remote_effect(principal, tool_name) {
                Ok(ForwardAuth::NeedsRemoteEffect {
                    principal: principal.to_owned(),
                    policy: Arc::clone(policy),
                })
            } else {
                Err(denied.with_machine(here))
            }
        }
    }
}

fn authorize_pipe_open(
    node: &Node,
    here: &str,
    principal: &str,
    required: &[(&str, Option<Effect>)],
) -> Result<(), ErrorBody> {
    let policy = node
        .config()
        .get::<Policy>()
        .map_err(|err| ensure_machine(err, here))?;
    for &(tool, effect) in required {
        policy
            .authorize_name(principal, tool, effect, node.paths())
            .map_err(|err| err.with_machine(here))?;
    }
    Ok(())
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let slots: Vec<Arc<MachineSlot>> = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in slots {
            let live = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            drop(live);
        }
    }
}

impl Router for Fleet {
    fn call<'a>(
        &'a self,
        node: &'a Node,
        call: Call,
        on: Option<Target>,
    ) -> BoxFuture<'a, RouteOutcome> {
        Box::pin(async move {
            match on {
                None => RouteOutcome::One(node.call(call).await),
                Some(Target::One(machine)) => {
                    RouteOutcome::One(self.call_machine(node, &machine, call).await)
                }
                Some(Target::All) => {
                    let here = node.name();
                    let fleet_file = match node.config().get::<FleetFile>() {
                        Ok(file) => file,
                        Err(err) => return RouteOutcome::One(Err(ensure_machine(err, &here))),
                    };
                    let mut names = Vec::with_capacity(fleet_file.machines.len() + 1);
                    names.push(here.clone());
                    for name in fleet_file.machines.keys() {
                        if name != &here {
                            names.push(name.clone());
                        }
                    }
                    self.fan_out(node, call, names).await
                }
                Some(Target::Many(names)) => {
                    let mut seen = HashSet::new();
                    let mut unique: Vec<String> = names
                        .into_iter()
                        .filter(|name| seen.insert(name.clone()))
                        .collect();
                    if unique.len() == 1 {
                        let machine = unique.pop().unwrap_or_default();
                        RouteOutcome::One(self.call_machine(node, &machine, call).await)
                    } else {
                        self.fan_out(node, call, unique).await
                    }
                }
            }
        })
    }

    fn effects<'a>(
        &'a self,
        node: &'a Node,
        tool: &'a str,
        on: Option<&'a Target>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<MachineEffects, ErrorBody>> {
        Box::pin(async move {
            let here = node.name();
            let machines = match on {
                None => vec![here.clone()],
                Some(Target::One(machine)) => vec![machine.clone()],
                Some(Target::Many(names)) => {
                    let mut seen = HashSet::new();
                    names
                        .iter()
                        .filter(|name| seen.insert(name.as_str()))
                        .cloned()
                        .collect()
                }
                Some(Target::All) => {
                    let fleet_file = node
                        .config()
                        .get::<FleetFile>()
                        .map_err(|err| ensure_machine(err, &here))?;
                    let mut names = vec![here.clone()];
                    names.extend(
                        fleet_file
                            .machines
                            .keys()
                            .filter(|name| **name != here)
                            .cloned(),
                    );
                    names
                }
            };
            // A built-in tool is authorised by this node's own spec wherever it runs; any other
            // tool that runs elsewhere, by the effect the target reports for it.
            let built_in = node.registry().get(tool).map(|tool| tool.spec().effect);
            let mut effects = Vec::with_capacity(machines.len());
            for machine in machines {
                let effect = if machine == here {
                    Some(node.spec(tool)?.effect)
                } else if built_in.is_some() {
                    built_in
                } else {
                    self.remote_effect(node, &here, &machine, tool, cancel.clone())
                        .await?
                };
                effects.push((machine, effect));
            }
            Ok(effects)
        })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(self.close())
    }
}

impl Fleet {
    /// The effect a tool that is not built in has on another machine, as the target reports it:
    /// `None` when the target does not report the tool, or is reached through a relay, whose
    /// final target's tools this node cannot ask for.
    async fn remote_effect(
        &self,
        node: &Node,
        here: &str,
        machine: &str,
        tool: &str,
        cancel: CancellationToken,
    ) -> Result<Option<Effect>, ErrorBody> {
        let fleet_file = node
            .config()
            .get::<FleetFile>()
            .map_err(|err| ensure_machine(err, here))?;
        let (direct_machine, direct_entry, relayed) = resolve_route(&fleet_file, machine, here)?;
        if relayed {
            return Ok(None);
        }
        self.query_remote_effect(direct_machine, direct_entry, tool, cancel)
            .await
    }
}

async fn acquire_connect_permit<'a>(
    semaphore: &'a Semaphore,
    machine: &str,
    cancel: &CancellationToken,
) -> Result<tokio::sync::SemaphorePermit<'a>, ErrorBody> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!("Connecting to {machine} was cancelled."),
        )
        .with_machine(machine)),
        permit = semaphore.acquire() => permit.map_err(|_| {
            ErrorBody::new(
                ErrorCode::Unreachable,
                format!("The fleet session pool is shutting down; cannot connect to {machine}."),
            )
            .with_machine(machine)
        }),
    }
}

fn default_first_os(program: Option<&str>) -> RemoteOs {
    if program.is_some_and(|p| p.contains('\\') || p.contains(':')) {
        RemoteOs::Windows
    } else {
        RemoteOs::Unix
    }
}

fn resolve_route<'a>(
    fleet_file: &'a FleetFile,
    machine: &'a str,
    here: &str,
) -> Result<(&'a str, &'a MachineEntry, bool), ErrorBody> {
    if fleet_file.machine(machine).is_none() {
        return Err(unknown_machine_error(machine, &fleet_file.names(), here));
    }
    fleet_file.resolve_route(machine).map_err(|reason| {
        ErrorBody::new(ErrorCode::Failed, format!("{reason}."))
            .with_hint(
                "Fix the `via` entries in fleet.toml so they name a valid relay without cycles.",
            )
            .with_machine(here)
    })
}

fn lookup_machine<'a>(
    fleet_file: &'a FleetFile,
    machine: &'a str,
    here: &str,
) -> Result<&'a MachineEntry, ErrorBody> {
    fleet_file
        .machine(machine)
        .ok_or_else(|| unknown_machine_error(machine, &fleet_file.names(), here))
}

fn unknown_machine_error(machine: &str, known: &[&str], here: &str) -> ErrorBody {
    let hint = if known.is_empty() {
        format!(
            "No remote machines are listed in fleet.toml (this node is {here}). Add one with fleet.add or edit fleet.toml."
        )
    } else {
        format!(
            "Known machines in fleet.toml: {} (this node is {here}).",
            known.join(", ")
        )
    };
    ErrorBody::new(
        ErrorCode::NotFound,
        format!("There is no machine called {machine} in fleet.toml on {here}."),
    )
    .with_hint(hint)
    .with_machine(here)
}

fn ensure_machine(err: ErrorBody, machine: &str) -> ErrorBody {
    if err.machine.is_none() {
        err.with_machine(machine)
    } else {
        err
    }
}

fn ping_ok(
    machine: &str,
    hello: legion_proto::HelloResult,
    system: Option<String>,
    started: Instant,
) -> PingOutcome {
    PingOutcome {
        machine: machine.to_owned(),
        reachable: true,
        node: Some(hello.node),
        system,
        version: Some(hello.version),
        os: Some(hello.os),
        arch: Some(hello.arch),
        latency_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
        error: None,
    }
}

fn ping_failed(machine: &str, error: ErrorBody) -> PingOutcome {
    PingOutcome {
        machine: machine.to_owned(),
        reachable: false,
        node: None,
        system: None,
        version: None,
        os: None,
        arch: None,
        latency_ms: None,
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use legion_core::{BoxFuture, Door, NoArgs, Registry, Tool, ToolError};
    use legion_proto::{Effect, HelloResult, Id, PROTOCOL, Request, Response, ToolSpec};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    struct RelayTool {
        spec: ToolSpec,
        fleet: Fleet,
    }

    impl Tool for RelayTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn call<'a>(
            &'a self,
            ctx: &'a Ctx,
            _args: Value,
        ) -> BoxFuture<'a, Result<Value, ToolError>> {
            Box::pin(async move {
                self.fleet
                    .call_remote(ctx, "pi", "meta.version", json!({}), EventSink::discard())
                    .await
            })
        }
    }

    #[tokio::test]
    async fn call_remote_increments_ctx_hops_and_enforces_max_hops() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(
            paths.config_file("fleet.toml"),
            "[machines.pi]\nssh = \"pi.example\"\n",
        )
        .unwrap();

        let fleet = Fleet::new(&paths);
        let fleet_file = ConfigStore::new(paths.config_dir())
            .get::<FleetFile>()
            .unwrap();
        let entry = fleet_file.machine("pi").unwrap();
        let systems = entry.resolved_systems();

        // Install an in-memory session for `pi` that echoes back `params.hops`.
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);

        let server_task = tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let _ = lines.next_line().await.unwrap().unwrap();
            let hello = Response::ok(
                Some(Id::Number(1)),
                serde_json::to_value(HelloResult {
                    node: "pi".to_owned(),
                    version: "0.1.0".to_owned(),
                    protocol: PROTOCOL,
                    os: "linux".to_owned(),
                    arch: "x86_64".to_owned(),
                    systems: Vec::new(),
                    system: None,
                    boot_id: None,
                })
                .unwrap(),
            );
            server_w
                .write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
                .await
                .unwrap();
            server_w.flush().await.unwrap();

            while let Ok(Some(line)) = lines.next_line().await {
                let req: Request = serde_json::from_str(&line).unwrap();
                let seen_hops = req.params["hops"].as_u64().unwrap_or(0);
                let reply = Response::ok(req.id, json!({"seen_hops": seen_hops}));
                server_w
                    .write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
                    .await
                    .unwrap();
                server_w.flush().await.unwrap();
            }
        });

        let session = Session::open_io(
            client_r,
            client_w,
            "pi",
            None,
            Duration::from_secs(5),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let slot = fleet.slot("pi");
        *slot.current.lock().unwrap() = Some(LiveEntry {
            session: Arc::new(session),
            systems_fingerprint: systems.clone(),
            active_system: systems[0].clone(),
            active_os: None,
        });

        let mut registry = Registry::new();
        registry.add(RelayTool {
            spec: ToolSpec {
                name: "test.relay".to_owned(),
                description: "Calls pi via Fleet::call_remote.".to_owned(),
                effect: Effect::Read,
                input_schema: legion_core::input_schema::<NoArgs>(),
            },
            fleet: fleet.clone(),
        });
        let node = Node::new(paths, registry);

        let mut call0 = Call::new(Door::Cli, "test.relay", json!({}));
        call0.hops = 0;
        let out0 = node.call(call0).await.unwrap();
        assert_eq!(out0["seen_hops"], json!(1));

        let mut call2 = Call::new(Door::Rpc, "test.relay", json!({}));
        call2.hops = 2;
        let out2 = node.call(call2).await.unwrap();
        assert_eq!(out2["seen_hops"], json!(3));

        let mut call_max = Call::new(Door::Rpc, "test.relay", json!({}));
        call_max.hops = MAX_HOPS;
        let err = node.call(call_max).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(err.message.contains("3 hops"), "{}", err.message);

        fleet.close().await;
        server_task.await.unwrap();
    }

    struct PipeOpenTool {
        spec: ToolSpec,
        fleet: Fleet,
        machine: String,
    }

    impl Tool for PipeOpenTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn call<'a>(
            &'a self,
            ctx: &'a Ctx,
            _args: Value,
        ) -> BoxFuture<'a, Result<Value, ToolError>> {
            Box::pin(async move {
                let pipe = self
                    .fleet
                    .open_pipe(ctx, &self.machine, Subcommand::Stream)
                    .await?;
                pipe.close(Duration::from_millis(50)).await;
                Ok(json!({"opened": true}))
            })
        }
    }

    struct WriteRelayTool {
        spec: ToolSpec,
        fleet: Fleet,
    }

    impl Tool for WriteRelayTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn call<'a>(
            &'a self,
            ctx: &'a Ctx,
            _args: Value,
        ) -> BoxFuture<'a, Result<Value, ToolError>> {
            Box::pin(async move {
                self.fleet
                    .call_remote(
                        ctx,
                        "pi",
                        "fs.write",
                        json!({"path": "~/x", "content": "hi"}),
                        EventSink::discard(),
                    )
                    .await
            })
        }
    }

    struct DummyTool {
        spec: ToolSpec,
    }

    impl Tool for DummyTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn call<'a>(
            &'a self,
            _ctx: &'a Ctx,
            _args: Value,
        ) -> BoxFuture<'a, Result<Value, ToolError>> {
            Box::pin(async move { Ok(json!({})) })
        }
    }

    fn read_audit_lines(paths: &Paths) -> Vec<Value> {
        let Ok(text) = std::fs::read_to_string(paths.audit_log()) else {
            return Vec::new();
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn call_remote_and_open_pipe_authorise_and_audit_on_forwarding_node() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"laptop\"\n").unwrap();
        std::fs::write(
            paths.config_file("fleet.toml"),
            "[machines.pi]\nssh = \"pi.example\"\n\n[machines.leaf]\nvia = \"pi\"\n",
        )
        .unwrap();
        std::fs::write(
            paths.config_file("policy.toml"),
            "[principals.reader]\nrole = \"observer\"\n[roles.observer]\nallow = [\"effect:read\"]\n",
        )
        .unwrap();

        let fleet = Fleet::new(&paths);
        let mut registry = Registry::new();
        registry.add(DummyTool {
            spec: ToolSpec {
                name: "fs.write".to_owned(),
                description: "Dummy write tool.".to_owned(),
                effect: Effect::Write,
                input_schema: legion_core::input_schema::<NoArgs>(),
            },
        });
        registry.add(WriteRelayTool {
            spec: ToolSpec {
                name: "test.write_relay".to_owned(),
                description: "Calls remote fs.write via Fleet::call_remote.".to_owned(),
                effect: Effect::Read,
                input_schema: legion_core::input_schema::<NoArgs>(),
            },
            fleet: fleet.clone(),
        });
        registry.add(PipeOpenTool {
            spec: ToolSpec {
                name: "test.open_pi".to_owned(),
                description: "Opens a stream pipe to pi.".to_owned(),
                effect: Effect::Read,
                input_schema: legion_core::input_schema::<NoArgs>(),
            },
            fleet: fleet.clone(),
            machine: "pi".to_owned(),
        });
        registry.add(PipeOpenTool {
            spec: ToolSpec {
                name: "test.open_leaf".to_owned(),
                description: "Opens a stream pipe to relayed leaf.".to_owned(),
                effect: Effect::Read,
                input_schema: legion_core::input_schema::<NoArgs>(),
            },
            fleet: fleet.clone(),
            machine: "leaf".to_owned(),
        });
        let node = Node::new(paths.clone(), registry);

        // 1. observer may invoke test.write_relay (Read), but Fleet::call_remote refuses
        //    fs.write on the forwarding node and audits it as denied without touching pi.
        let mut call = Call::new(Door::Cli, "test.write_relay", json!({}));
        call.as_principal = Some("reader".to_owned());
        let err = node.call(call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert_eq!(err.machine.as_deref(), Some("laptop"));
        let audit = read_audit_lines(&paths);
        assert!(
            audit.iter().any(|e| {
                e["target"] == "pi"
                    && e["tool"] == "fs.write"
                    && e["principal"] == "reader"
                    && e["outcome"] == "denied"
            }),
            "{audit:?}"
        );

        // 2. observer cannot open a stream pipe because it lacks fs.transfer; denied and audited
        //    on laptop without attempting SSH.
        let mut call = Call::new(Door::Cli, "test.open_pi", json!({}));
        call.as_principal = Some("reader".to_owned());
        let err = node.call(call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert_eq!(err.machine.as_deref(), Some("laptop"));
        let audit = read_audit_lines(&paths);
        assert!(
            audit.iter().any(|e| {
                e["target"] == "pi"
                    && e["tool"] == "stream.open"
                    && e["principal"] == "reader"
                    && e["outcome"] == "denied"
            }),
            "{audit:?}"
        );

        // 3. owner opening a pipe to a via-relayed machine gets Unsupported with a hint telling
        //    them to run transfers on the relay.
        let call = Call::new(Door::Cli, "test.open_leaf", json!({}));
        let err = node.call(call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert_eq!(err.machine.as_deref(), Some("laptop"));
        let hint = err.hint.as_deref().unwrap_or("");
        assert!(hint.contains("pi") && hint.contains("--on pi"), "{err:?}");
        let audit = read_audit_lines(&paths);
        assert!(
            audit.iter().any(|e| {
                e["target"] == "leaf"
                    && e["tool"] == "stream.open"
                    && e["principal"] == "owner"
                    && e["outcome"] == "unsupported"
            }),
            "{audit:?}"
        );

        fleet.close().await;
    }
}
