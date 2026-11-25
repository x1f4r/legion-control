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
    BoxFuture, Call, CancellationToken, ContentError, ContentRequest, Ctx, EventSink,
    MachineEffects, Node, OpenedContent, Paths, RouteOutcome, Router, VERSION, open_local_content,
};
pub use legion_proto::MAX_HOPS;
use legion_proto::{CallParams, Effect, ErrorBody, ErrorCode, Target};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::{FleetFile, MachineEntry, RemoteOs, ResolvedSystem, Transport};
use crate::connector::{Pipe, Subcommand};
use crate::session::{self, CallError, OpenError, Session};
use crate::state::StateStore;

/// Maximum number of new remote connections opened concurrently during a fan-out or ping.
const MAX_CONCURRENT_CONNECTS: usize = 8;

/// Maximum concurrent `legion stream` channels (active + idle) per target machine.
///
/// Kept strictly below OpenSSH's default `MaxSessions` of 10 so multiplexed connections always
/// keep room for the persistent `legion rpc` session and never trip `MaxSessions`.
const MAX_STREAMS_PER_TARGET: usize = 8;

/// Maximum idle `keep_open` `legion stream` pipes kept warm per target machine.
const MAX_IDLE_CONTENT_PIPES: usize = 4;

/// How long an idle `keep_open` `legion stream` pipe stays warm before being dropped.
const IDLE_CONTENT_PIPE_TTL: Duration = Duration::from_secs(60);

/// Default connect timeout for a single-system machine.
const SINGLE_SYSTEM_TIMEOUT: Duration = Duration::from_secs(10);

/// Shorter per-system connect timeout when probing a multi-system machine.
const MULTI_SYSTEM_TIMEOUT: Duration = Duration::from_secs(4);

/// Brief head start given to the last-responding system of a multi-system machine before probing
/// its sibling systems in parallel, avoiding redundant SSH spawns in the common case.
const REMEMBERED_SYSTEM_HEAD_START: Duration = Duration::from_millis(250);

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
    mux: ControlMux,
    /// Bumped on [`Fleet::close`] so an in-flight connect started before shutdown never installs a
    /// session afterwards.
    generation: AtomicU64,
}

struct MachineSlot {
    /// Serializes opening a new session for this machine so concurrent first calls share one.
    open_lock: tokio::sync::Mutex<()>,
    current: Mutex<Option<LiveEntry>>,
    /// Caps concurrent `legion stream` pipes (both active and idle) to this machine below
    /// OpenSSH's default `MaxSessions` of 10.
    stream_permits: Arc<Semaphore>,
    /// Warm `keep_open` `legion stream` pipes ready for immediate reuse by content range reads.
    idle_streams: Mutex<Vec<IdleContentPipe>>,
}

struct IdleContentPipe {
    stdin: tokio::process::ChildStdin,
    reader: tokio::io::BufReader<tokio::process::ChildStdout>,
    handle: crate::connector::PipeHandle,
    stderr: Arc<crate::connector::StderrTail>,
    systems_fingerprint: Vec<ResolvedSystem>,
    last_used: Instant,
}

#[cfg(unix)]
struct ControlMux {
    dir: Mutex<Option<std::path::PathBuf>>,
    sockets: Mutex<HashMap<String, Vec<(String, std::path::PathBuf)>>>,
}

#[cfg(not(unix))]
struct ControlMux;

#[cfg(unix)]
impl ControlMux {
    fn new() -> Self {
        Self {
            dir: Mutex::new(None),
            sockets: Mutex::new(HashMap::new()),
        }
    }

    fn control_path(&self, machine: &str, target: &str) -> Option<std::path::PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT_DIR: AtomicU64 = AtomicU64::new(1);

        let dir = {
            let mut guard = self.dir.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(existing) = guard.as_ref() {
                existing.clone()
            } else {
                let base = {
                    let tmp = std::env::temp_dir();
                    if tmp.to_string_lossy().len() <= 40 {
                        tmp
                    } else {
                        std::path::PathBuf::from("/tmp")
                    }
                };
                let seq = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
                let path = base.join(format!("lgn-{}-{seq}", std::process::id()));
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700);
                if builder.create(&path).is_err() && !path.is_dir() {
                    return None;
                }
                *guard = Some(path.clone());
                path
            }
        };

        let hash = fnv1a64(target.as_bytes());
        let sock = dir.join(format!("{hash:016x}.ctl"));
        if sock.to_string_lossy().len() >= 96 {
            return None;
        }
        let mut sockets = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
        let list = sockets.entry(machine.to_owned()).or_default();
        if !list.iter().any(|(_, existing)| existing == &sock) {
            list.push((target.to_owned(), sock.clone()));
        }
        Some(sock)
    }

    async fn close_machine(&self, machine: &str) {
        let removed = self
            .sockets
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(machine)
            .unwrap_or_default();
        for (target, sock) in removed {
            crate::connector::close_ssh_control_master(&target, &sock).await;
        }
    }

    async fn close_all(&self) {
        let all: Vec<(String, std::path::PathBuf)> = {
            let mut guard = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
            guard.drain().flat_map(|(_, v)| v).collect()
        };
        for (target, sock) in all {
            crate::connector::close_ssh_control_master(&target, &sock).await;
        }
        if let Some(dir) = self.dir.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(unix)]
impl Drop for ControlMux {
    fn drop(&mut self) {
        let all: Vec<(String, std::path::PathBuf)> = {
            let mut guard = self.sockets.lock().unwrap_or_else(|p| p.into_inner());
            guard.drain().flat_map(|(_, v)| v).collect()
        };
        for (target, sock) in all {
            crate::connector::close_ssh_control_master_sync(&target, &sock);
        }
        if let Some(dir) = self.dir.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

#[cfg(unix)]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(not(unix))]
impl ControlMux {
    fn new() -> Self {
        Self
    }

    fn control_path(&self, _machine: &str, _target: &str) -> Option<std::path::PathBuf> {
        None
    }

    async fn close_machine(&self, _machine: &str) {}

    async fn close_all(&self) {}
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
                mux: ControlMux::new(),
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
        node.record_audit_with_device(
            machine.to_owned(),
            Some(principal),
            ctx.door(),
            ctx.device().map(str::to_owned),
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
        check_credential_minting_forward(here, tool, args, ctx.credential_expires())?;
        check_paired_device_invite_forward(
            node,
            here,
            machine,
            principal,
            ctx.device(),
            ctx.pairing_id(),
            tool,
        )?;
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
        node.record_audit_with_device(
            machine.to_owned(),
            Some(principal),
            ctx.door(),
            ctx.device().map(str::to_owned),
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

        // If this is a stream pipe and all per-target permits are occupied, evict one idle
        // content pipe first so idle keep-alive streams never starve a new stream or fs.transfer.
        let stream_permit = if subcommand == Subcommand::Stream {
            if slot.stream_permits.available_permits() == 0 {
                let evicted = {
                    let mut idle = slot
                        .idle_streams
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if idle.is_empty() {
                        None
                    } else {
                        Some(idle.remove(0))
                    }
                };
                drop(evicted);
            }
            Some(acquire_stream_permit(&slot.stream_permits, machine, &cancel).await?)
        } else {
            None
        };

        let _permit = acquire_connect_permit(&self.inner.connect_permits, machine, &cancel).await?;
        let pipe = match &system.transport {
            Transport::Command { argv } => Pipe::spawn_command(argv, subcommand),
            Transport::Ssh {
                target, program, ..
            } => {
                let os = resolved_os.unwrap_or(RemoteOs::Unix);
                let control = self.inner.mux.control_path(machine, target);
                Pipe::spawn_ssh_with_control(
                    target,
                    os,
                    subcommand,
                    program.as_deref(),
                    SINGLE_SYSTEM_TIMEOUT,
                    control.as_deref(),
                )
            }
        }
        .map_err(|err| err.with_machine(machine))?;

        let pipe = match stream_permit {
            Some(permit) => pipe.with_permit(permit),
            None => pipe,
        };
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
                node.record_audit_with_device(
                    machine.to_owned(),
                    principal,
                    call.door,
                    call.device,
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
        let device = call.device.clone();
        let tool = call.tool.clone();
        let args = call.args.clone();
        let principal = auth.principal().to_owned();
        let result =
            match check_credential_minting_forward(&here, &tool, &args, call.credential_expires)
                .and_then(|()| {
                    check_paired_device_invite_forward(
                        node,
                        &here,
                        machine,
                        &principal,
                        call.device.as_deref(),
                        call.pairing_id.as_deref(),
                        &tool,
                    )
                })
                .and_then(|()| {
                    node.config()
                        .get::<FleetFile>()
                        .map_err(|err| ensure_machine(err, &here))
                })
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
        node.record_audit_with_device(
            machine.to_owned(),
            Some(principal),
            door,
            device,
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

    /// Closes and removes any cached session and idle stream pipes for `machine`, e.g. after
    /// editing `fleet.toml`.
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
            let idle: Vec<IdleContentPipe> = {
                let mut guard = slot
                    .idle_streams
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                std::mem::take(&mut *guard)
            };
            for pipe in idle {
                drop(pipe.stdin);
                drop(pipe.reader);
                pipe.handle.close(Duration::from_secs(1)).await;
            }
            let session = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(live) = session {
                live.session.close().await;
            }
        }
        self.inner.mux.close_machine(machine).await;
    }

    /// Closes every open session, idle stream pipe, and SSH control master cleanly (EOF on stdin,
    /// short wait, then kill).
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
            let idle: Vec<IdleContentPipe> = {
                let mut guard = slot
                    .idle_streams
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                std::mem::take(&mut *guard)
            };
            for pipe in idle {
                closing.spawn(async move {
                    drop(pipe.stdin);
                    drop(pipe.reader);
                    pipe.handle.close(Duration::from_secs(1)).await;
                });
            }
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
        self.inner.mux.close_all().await;
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
        let ordered = self.order_systems(machine, systems);
        if ordered.len() == 1
            && let Some(sys) = ordered.into_iter().next()
        {
            let timeout = timeout_override.unwrap_or(SINGLE_SYSTEM_TIMEOUT);
            return match self.open_one_system(machine, &sys, timeout, cancel).await {
                Ok((session, worked_os)) => {
                    if sys.explicit {
                        self.inner.state.record_system(machine, &sys.name);
                    }
                    if let Some(os) = worked_os {
                        self.inner.state.record_os(machine, &sys.name, os);
                    }
                    Ok((session, sys, worked_os))
                }
                Err(OpenError::Unsupported(err))
                | Err(OpenError::Cancelled(err))
                | Err(OpenError::Unreachable { error: err, .. }) => {
                    Err(ensure_machine(err, machine))
                }
            };
        }

        // Probe a multi-system machine concurrently so an offline or secondary dual-boot target
        // does not pay one timeout per configured system. When a system answered previously, give
        // it a brief head start so healthy reconnections never spawn redundant sibling probes.
        let has_remembered = self.inner.state.last_system(machine).is_some();
        let ordered = self.order_systems(machine, systems);
        let per_system_timeout = match timeout_override {
            Some(total) => total.min(MULTI_SYSTEM_TIMEOUT),
            None => MULTI_SYSTEM_TIMEOUT,
        };
        let deadline = Instant::now() + per_system_timeout;

        let probe_cancel = cancel.child_token();
        let mut set: JoinSet<SystemProbeOutcome> = JoinSet::new();
        let mut failures: BTreeMap<usize, (String, ErrorBody)> = BTreeMap::new();
        let mut iter = ordered.into_iter().enumerate();

        if has_remembered && let Some((index, preferred)) = iter.next() {
            let fleet = self.clone();
            let machine_name = machine.to_owned();
            let task_cancel = probe_cancel.child_token();
            set.spawn(async move {
                let res = fleet
                    .open_one_system(&machine_name, &preferred, per_system_timeout, task_cancel)
                    .await;
                (index, preferred, res)
            });

            let head_start = REMEMBERED_SYSTEM_HEAD_START.min(per_system_timeout / 4);
            if let Ok(Some(joined)) = tokio::time::timeout(head_start, set.join_next()).await
                && let Ok((idx, sys, res)) = joined
            {
                match res {
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
                        failures.insert(idx, (sys.name, error));
                    }
                }
            }
        }

        let sibling_timeout = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(200).min(per_system_timeout));
        for (index, sys) in iter {
            let fleet = self.clone();
            let machine_name = machine.to_owned();
            let task_cancel = probe_cancel.child_token();
            set.spawn(async move {
                let res = fleet
                    .open_one_system(&machine_name, &sys, sibling_timeout, task_cancel)
                    .await;
                (index, sys, res)
            });
        }

        while let Some(joined) = set.join_next().await {
            let Ok((index, sys, res)) = joined else {
                continue;
            };
            match res {
                Ok((session, worked_os)) => {
                    probe_cancel.cancel();
                    if sys.explicit {
                        self.inner.state.record_system(machine, &sys.name);
                    }
                    if let Some(os) = worked_os {
                        self.inner.state.record_os(machine, &sys.name, os);
                    }
                    drain_probe_tasks(set);
                    return Ok((session, sys, worked_os));
                }
                Err(OpenError::Unsupported(err)) | Err(OpenError::Cancelled(err)) => {
                    probe_cancel.cancel();
                    drain_probe_tasks(set);
                    return Err(ensure_machine(err, machine));
                }
                Err(OpenError::Unreachable { error, .. }) => {
                    failures.insert(index, (sys.name, error));
                }
            }
        }

        if cancel.is_cancelled() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Connecting to {machine} was cancelled."),
            )
            .with_machine(machine));
        }

        let mut ordered_failures: Vec<(String, ErrorBody)> = failures.into_values().collect();
        if ordered_failures.len() == 1
            && let Some((_, err)) = ordered_failures.pop()
        {
            return Err(ensure_machine(err, machine));
        }

        let summary = ordered_failures
            .iter()
            .map(|(name, err)| format!("{name}: {}", err.message))
            .collect::<Vec<_>>()
            .join("; ");
        let hints = ordered_failures
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
                    session::open_pipe(pipe, machine, reported_system, timeout, cancel).await?;
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
                    let control = self.inner.mux.control_path(machine, target);
                    let pipe = Pipe::spawn_ssh_with_control(
                        target,
                        candidate_os,
                        Subcommand::Rpc,
                        program.as_deref(),
                        timeout,
                        control.as_deref(),
                    )
                    .map_err(|err| OpenError::Unreachable {
                        error: err.with_machine(machine),
                        retry_other_os: false,
                    })?;
                    match session::open_pipe(
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
                stream_permits: Arc::new(Semaphore::new(MAX_STREAMS_PER_TARGET)),
                idle_streams: Mutex::new(Vec::new()),
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
                    node.record_audit_with_device(
                        machine.clone(),
                        principal.clone(),
                        call.door,
                        call.device.clone(),
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
                device: call.device.clone(),
                pairing_id: call.pairing_id.clone(),
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

/// Tools that mint or escalate permanent credentials and enforce local expiry checks on
/// [`Call::credential_expires`]. Because the JSON-RPC wire format does not carry
/// `credential_expires`, a forwarding node refuses to forward these calls whenever the caller
/// arrived with an expiring credential.
fn is_credential_minting_forward(tool: &str, args: &Value) -> bool {
    match tool {
        "device.invite" | "token.add" | "device.set" => true,
        "config.write" => match args.get("document").and_then(Value::as_str) {
            Some("devices.toml") => match args.get("content").and_then(Value::as_str) {
                Some(content) => match legion_core::config::parse::<
                    legion_tools::device::registry::DevicesFile,
                >(
                    std::path::Path::new("devices.toml"), content.as_bytes()
                ) {
                    // Allow tombstone-only `devices.toml` (no live entries, at least one
                    // tombstone) so a removal made with an expiring credential still reaches peers.
                    Ok(parsed) => !parsed.devices.is_empty() || parsed.removed.is_empty(),
                    Err(_) => true,
                },
                None => true,
            },
            Some("policy.toml") => true,
            Some(_) => false,
            None => true,
        },
        "fleet.sync" => match args.get("direction") {
            None => false,
            Some(Value::String(dir)) if dir == "status" => false,
            Some(_) => true,
        },
        _ => false,
    }
}

fn check_credential_minting_forward(
    here: &str,
    tool: &str,
    args: &Value,
    credential_expires: Option<jiff::Timestamp>,
) -> Result<(), ErrorBody> {
    let Some(limit) = credential_expires else {
        return Ok(());
    };
    if !is_credential_minting_forward(tool, args) {
        return Ok(());
    }
    Err(ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "The credential this call came with expires at {}, so {here} refuses to forward {tool} to another machine.",
            limit.strftime("%Y-%m-%dT%H:%M:%SZ")
        ),
    )
    .with_hint(format!(
        "Credential expiry is not carried across remote hops; run {tool} directly on the target machine or with a non-expiring credential."
    ))
    .with_machine(here))
}

/// A paired device may create invites only on the machine it is connected to directly, so
/// `paired_by.device` is always known from the session context.
fn check_paired_device_invite_forward(
    node: &Node,
    here: &str,
    machine: &str,
    principal: &str,
    device: Option<&str>,
    pairing_id: Option<&str>,
    tool: &str,
) -> Result<(), ErrorBody> {
    if tool != "device.invite" {
        return Ok(());
    }
    let is_paired_device = pairing_id.is_some()
        || device.is_some()
        || node
            .config()
            .get::<Policy>()
            .ok()
            .is_some_and(|p| p.device_owner(principal).is_some())
        || principal.starts_with(legion_tools::device::registry::DEVICE_PRINCIPAL_PREFIX);
    if !is_paired_device {
        return Ok(());
    }
    Err(ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "A paired device may create pairing invites only on the machine it is connected to directly, not forwarded from {here} to {machine}."
        ),
    )
    .with_hint(format!(
        "Connect directly to {machine} over SSH (for example using its endpoint from `device.endpoint`) and run `device.invite` there."
    ))
    .with_machine(here))
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
            let idle = std::mem::take(
                &mut *slot
                    .idle_streams
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            drop(idle);
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

    fn open_content<'a>(
        &'a self,
        node: &'a Node,
        req: ContentRequest,
    ) -> BoxFuture<'a, Result<OpenedContent, ContentError>> {
        Box::pin(async move {
            let here = node.name();
            match req.machine.as_deref() {
                None => open_local_content(node, &req).await,
                Some(machine) if machine == here => open_local_content(node, &req).await,
                Some(machine) => {
                    let started = Instant::now();
                    let args = json!({
                        "path": req.path,
                        "offset": req.offset,
                        "length": req.length,
                    });
                    let outcome = self.open_remote_content(node, &here, machine, &req).await;
                    if req.audit {
                        let audit_result = match &outcome {
                            Ok(_) => Ok(Value::Null),
                            Err(ContentError::OutOfRange { size }) => Err(ErrorBody::new(
                                ErrorCode::Conflict,
                                format!(
                                    "{} on {machine} cannot be read at byte {}: there are {size} bytes.",
                                    req.path, req.offset
                                ),
                            )),
                            Err(ContentError::Error(err)) => Err(err.clone()),
                        };
                        node.record_audit(
                            machine.to_owned(),
                            Some(req.principal.clone()),
                            req.door,
                            "content.read".to_owned(),
                            &args,
                            started,
                            &audit_result,
                        )
                        .await;
                    }
                    outcome
                }
            }
        })
    }
}

impl Fleet {
    async fn open_remote_content(
        &self,
        node: &Node,
        here: &str,
        machine: &str,
        req: &ContentRequest,
    ) -> Result<OpenedContent, ContentError> {
        use tokio::io::{
            AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, ReadBuf,
        };

        const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
        const MAX_REPLY_LINE: usize = 1024 * 1024;
        const MAX_DONE_LINE: usize = 256;
        const MAX_NOISE_LINES: usize = 64;
        const MAX_NOISE_BYTES: usize = 64 * 1024;
        const STREAM_BUF: usize = 64 * 1024;

        enum ReaderPhase {
            Data { remaining: u64 },
            DoneTrailer { line: Vec<u8>, final_data: Vec<u8> },
            CheckEof { final_data: Vec<u8> },
            FlushFinal { data: Vec<u8>, offset: usize },
            Finished,
        }

        struct RemotePipeParts {
            stdin: tokio::process::ChildStdin,
            reader: BufReader<tokio::process::ChildStdout>,
            handle: crate::connector::PipeHandle,
            stderr: Arc<crate::connector::StderrTail>,
            systems_fingerprint: Vec<ResolvedSystem>,
        }

        struct RemoteContentReader {
            parts: Option<RemotePipeParts>,
            slot: Arc<MachineSlot>,
            keep_open: bool,
            phase: ReaderPhase,
            cancel: CancellationToken,
        }

        impl RemoteContentReader {
            fn recycle(&mut self) {
                if let Some(parts) = self.parts.take() {
                    return_idle_pipe(
                        &self.slot,
                        IdleContentPipe {
                            stdin: parts.stdin,
                            reader: parts.reader,
                            handle: parts.handle,
                            stderr: parts.stderr,
                            systems_fingerprint: parts.systems_fingerprint,
                            last_used: Instant::now(),
                        },
                    );
                }
            }
        }

        impl AsyncRead for RemoteContentReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                let this = self.get_mut();
                if this.cancel.is_cancelled() {
                    this.parts = None;
                    this.phase = ReaderPhase::Finished;
                    return std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "content stream was cancelled",
                    )));
                }
                loop {
                    match &mut this.phase {
                        ReaderPhase::Finished => return std::task::Poll::Ready(Ok(())),
                        ReaderPhase::FlushFinal { data, offset } => {
                            if *offset >= data.len() {
                                this.phase = ReaderPhase::Finished;
                                return std::task::Poll::Ready(Ok(()));
                            }
                            if buf.remaining() == 0 {
                                return std::task::Poll::Ready(Ok(()));
                            }
                            let n = (data.len() - *offset).min(buf.remaining());
                            buf.put_slice(&data[*offset..*offset + n]);
                            *offset += n;
                            if *offset >= data.len() {
                                this.phase = ReaderPhase::Finished;
                            }
                            return std::task::Poll::Ready(Ok(()));
                        }
                        ReaderPhase::Data { remaining } => {
                            if *remaining == 0 {
                                this.phase = if this.keep_open {
                                    ReaderPhase::DoneTrailer {
                                        line: Vec::with_capacity(32),
                                        final_data: Vec::new(),
                                    }
                                } else {
                                    ReaderPhase::CheckEof {
                                        final_data: Vec::new(),
                                    }
                                };
                                continue;
                            }
                            if buf.remaining() == 0 {
                                return std::task::Poll::Ready(Ok(()));
                            }
                            let Some(parts) = this.parts.as_mut() else {
                                this.phase = ReaderPhase::Finished;
                                return std::task::Poll::Ready(Ok(()));
                            };
                            let max_read = usize::try_from(*remaining)
                                .unwrap_or(usize::MAX)
                                .min(buf.remaining());
                            match std::pin::Pin::new(&mut parts.reader).poll_fill_buf(cx) {
                                std::task::Poll::Pending => return std::task::Poll::Pending,
                                std::task::Poll::Ready(Err(err)) => {
                                    this.parts = None;
                                    this.phase = ReaderPhase::Finished;
                                    return std::task::Poll::Ready(Err(err));
                                }
                                std::task::Poll::Ready(Ok(avail)) => {
                                    if avail.is_empty() {
                                        let missing = *remaining;
                                        this.parts = None;
                                        this.phase = ReaderPhase::Finished;
                                        return std::task::Poll::Ready(Err(std::io::Error::new(
                                            std::io::ErrorKind::UnexpectedEof,
                                            format!(
                                                "remote content stream ended with {missing} bytes remaining"
                                            ),
                                        )));
                                    }
                                    let take = avail.len().min(max_read);
                                    if (take as u64) < *remaining {
                                        buf.put_slice(&avail[..take]);
                                        std::pin::Pin::new(&mut parts.reader).consume(take);
                                        *remaining -= take as u64;
                                        return std::task::Poll::Ready(Ok(()));
                                    }
                                    // Final chunk: validate the done trailer / EOF and recycle
                                    // the keep_open pipe BEFORE yielding the last bytes, since
                                    // HTTP servers with Content-Length drop the body as soon as
                                    // the final byte is written.
                                    let final_data = avail[..take].to_vec();
                                    std::pin::Pin::new(&mut parts.reader).consume(take);
                                    this.phase = if this.keep_open {
                                        ReaderPhase::DoneTrailer {
                                            line: Vec::with_capacity(32),
                                            final_data,
                                        }
                                    } else {
                                        ReaderPhase::CheckEof { final_data }
                                    };
                                    continue;
                                }
                            }
                        }
                        ReaderPhase::DoneTrailer { line, .. } => {
                            let Some(parts) = this.parts.as_mut() else {
                                this.phase = ReaderPhase::Finished;
                                return std::task::Poll::Ready(Ok(()));
                            };
                            match std::pin::Pin::new(&mut parts.reader).poll_fill_buf(cx) {
                                std::task::Poll::Pending => return std::task::Poll::Pending,
                                std::task::Poll::Ready(Err(err)) => {
                                    this.parts = None;
                                    this.phase = ReaderPhase::Finished;
                                    return std::task::Poll::Ready(Err(err));
                                }
                                std::task::Poll::Ready(Ok(avail)) => {
                                    if avail.is_empty() {
                                        this.parts = None;
                                        this.phase = ReaderPhase::Finished;
                                        return std::task::Poll::Ready(Err(std::io::Error::new(
                                            std::io::ErrorKind::UnexpectedEof,
                                            "remote content stream closed before done frame",
                                        )));
                                    }
                                    if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                                        let take = pos + 1;
                                        if line.len() + take > MAX_DONE_LINE {
                                            this.parts = None;
                                            this.phase = ReaderPhase::Finished;
                                            return std::task::Poll::Ready(Err(
                                                std::io::Error::new(
                                                    std::io::ErrorKind::InvalidData,
                                                    "remote content stream sent extra data instead of done frame",
                                                ),
                                            ));
                                        }
                                        line.extend_from_slice(&avail[..take]);
                                        std::pin::Pin::new(&mut parts.reader).consume(take);
                                        if !is_done_frame(line) {
                                            this.parts = None;
                                            this.phase = ReaderPhase::Finished;
                                            return std::task::Poll::Ready(Err(
                                                std::io::Error::new(
                                                    std::io::ErrorKind::InvalidData,
                                                    "remote content stream sent an invalid done frame",
                                                ),
                                            ));
                                        }
                                        this.recycle();
                                        let prev = std::mem::replace(
                                            &mut this.phase,
                                            ReaderPhase::Finished,
                                        );
                                        if let ReaderPhase::DoneTrailer { final_data, .. } = prev {
                                            this.phase = ReaderPhase::FlushFinal {
                                                data: final_data,
                                                offset: 0,
                                            };
                                        }
                                        continue;
                                    }
                                    let n = avail.len();
                                    if line.len() + n > MAX_DONE_LINE {
                                        this.parts = None;
                                        this.phase = ReaderPhase::Finished;
                                        return std::task::Poll::Ready(Err(std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            "remote content stream sent extra bytes past declared length",
                                        )));
                                    }
                                    line.extend_from_slice(avail);
                                    std::pin::Pin::new(&mut parts.reader).consume(n);
                                }
                            }
                        }
                        ReaderPhase::CheckEof { .. } => {
                            let Some(parts) = this.parts.as_mut() else {
                                this.phase = ReaderPhase::Finished;
                                return std::task::Poll::Ready(Ok(()));
                            };
                            match std::pin::Pin::new(&mut parts.reader).poll_fill_buf(cx) {
                                std::task::Poll::Pending => return std::task::Poll::Pending,
                                std::task::Poll::Ready(Err(err)) => {
                                    this.parts = None;
                                    this.phase = ReaderPhase::Finished;
                                    return std::task::Poll::Ready(Err(err));
                                }
                                std::task::Poll::Ready(Ok(avail)) => {
                                    let extra = !avail.is_empty();
                                    this.parts = None;
                                    if extra {
                                        this.phase = ReaderPhase::Finished;
                                        return std::task::Poll::Ready(Err(std::io::Error::new(
                                            std::io::ErrorKind::InvalidData,
                                            "remote content stream sent extra bytes past declared length",
                                        )));
                                    }
                                    let prev =
                                        std::mem::replace(&mut this.phase, ReaderPhase::Finished);
                                    if let ReaderPhase::CheckEof { final_data } = prev {
                                        this.phase = ReaderPhase::FlushFinal {
                                            data: final_data,
                                            offset: 0,
                                        };
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }

        // Authorise on this node first (every node a forwarded read crosses must allow fs.read).
        let policy = node
            .config()
            .get::<Policy>()
            .map_err(|err| ContentError::Error(ensure_machine(err, here)))?;
        policy
            .authorize_name(&req.principal, "fs.read", Some(Effect::Read), node.paths())
            .map_err(|err| ContentError::Error(err.with_machine(here)))?;

        let fleet_file = node
            .config()
            .get::<FleetFile>()
            .map_err(|err| ContentError::Error(ensure_machine(err, here)))?;
        let entry = lookup_machine(&fleet_file, machine, here).map_err(ContentError::Error)?;
        let systems_fingerprint = entry.resolved_systems();
        let slot = self.slot(machine);

        let mut header = json!({
            "protocol": crate::stream::STREAM_PROTOCOL,
            "op": "get",
            "path": req.path,
            "offset": req.offset,
            "as": req.principal,
            "keep_open": true,
        });
        if let Some(len) = req.length {
            header["length"] = json!(len);
        }
        let mut line = header.to_string().into_bytes();
        line.push(b'\n');

        let mut tried_fresh = false;
        loop {
            let (mut stdin, mut reader, handle, stderr, reused) =
                if let Some(idle) = pop_idle_pipe(&slot, &systems_fingerprint) {
                    (idle.stdin, idle.reader, idle.handle, idle.stderr, true)
                } else {
                    tried_fresh = true;
                    let pipe = self
                        .open_pipe_for(
                            node.config(),
                            here,
                            machine,
                            Subcommand::Stream,
                            req.cancel.clone(),
                        )
                        .await
                        .map_err(ContentError::Error)?;
                    let stderr = pipe.stderr_tail().clone();
                    let (stdin, stdout, handle) = pipe.into_parts();
                    (
                        stdin,
                        BufReader::with_capacity(STREAM_BUF, stdout),
                        handle,
                        stderr,
                        false,
                    )
                };

            enum HandshakeFailure {
                BrokenPipe(ContentError),
                ProtocolError(ContentError),
            }

            let handshake = async {
                stdin.write_all(&line).await.map_err(|err| {
                    HandshakeFailure::BrokenPipe(ContentError::Error(
                        ErrorBody::new(
                            ErrorCode::Unreachable,
                            format!("Could not send stream get header to {machine}: {err}."),
                        )
                        .with_machine(machine),
                    ))
                })?;
                stdin.flush().await.map_err(|err| {
                    HandshakeFailure::BrokenPipe(ContentError::Error(
                        ErrorBody::new(
                            ErrorCode::Unreachable,
                            format!("Could not flush stream get header to {machine}: {err}."),
                        )
                        .with_machine(machine),
                    ))
                })?;

                let mut greeted = reused;
                let mut noise_lines = 0usize;
                let mut noise_bytes = 0usize;
                let mut reply_buf = Vec::new();
                loop {
                    reply_buf.clear();
                    let mut total = 0usize;
                    loop {
                        let available = reader.fill_buf().await.map_err(|err| {
                            HandshakeFailure::BrokenPipe(ContentError::Error(
                                ErrorBody::new(
                                    ErrorCode::Unreachable,
                                    format!("Reading stream reply from {machine} failed: {err}."),
                                )
                                .with_machine(machine),
                            ))
                        })?;
                        if available.is_empty() {
                            break;
                        }
                        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
                            let take = pos + 1;
                            if total + take > MAX_REPLY_LINE {
                                return Err(HandshakeFailure::ProtocolError(ContentError::Error(
                                    ErrorBody::new(
                                        ErrorCode::Failed,
                                        format!("{machine} sent an oversized stream reply line."),
                                    )
                                    .with_machine(machine),
                                )));
                            }
                            reply_buf.extend_from_slice(&available[..take]);
                            reader.consume(take);
                            total += take;
                            break;
                        }
                        let n = available.len();
                        if total + n > MAX_REPLY_LINE {
                            return Err(HandshakeFailure::ProtocolError(ContentError::Error(
                                ErrorBody::new(
                                    ErrorCode::Failed,
                                    format!("{machine} sent an oversized stream reply line."),
                                )
                                .with_machine(machine),
                            )));
                        }
                        reply_buf.extend_from_slice(available);
                        reader.consume(n);
                        total += n;
                    }
                    if total == 0 {
                        let mut err = ErrorBody::new(
                            ErrorCode::Unreachable,
                            format!("The stream to {machine} closed before answering."),
                        )
                        .with_machine(machine);
                        let text = stderr.text();
                        if !text.is_empty() {
                            err = err.with_hint(format!("Remote stderr: {text}"));
                        }
                        return Err(HandshakeFailure::BrokenPipe(ContentError::Error(err)));
                    }

                    if let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&reply_buf) {
                        if !greeted {
                            if let Some(spoken) = fields.get("protocol") {
                                if spoken.as_u64()
                                    != Some(u64::from(crate::stream::STREAM_PROTOCOL))
                                {
                                    return Err(HandshakeFailure::ProtocolError(
                                        ContentError::Error(
                                            ErrorBody::new(
                                                ErrorCode::Unsupported,
                                                format!(
                                                    "{machine} speaks stream protocol {spoken}, and this node speaks stream protocol {}.",
                                                    crate::stream::STREAM_PROTOCOL
                                                ),
                                            )
                                            .with_hint("Update legion on whichever machine is older.")
                                            .with_machine(machine),
                                        ),
                                    ));
                                }
                                greeted = true;
                                continue;
                            }
                        } else if fields.len() == 1
                            && fields.get("busy") == Some(&Value::Bool(true))
                        {
                            continue;
                        } else {
                            if let Some(err_val) = fields.get("error")
                                && let Ok(body) =
                                    serde_json::from_value::<ErrorBody>(err_val.clone())
                            {
                                if body.code == ErrorCode::Conflict
                                    && let Some(actual) = body
                                        .details
                                        .as_ref()
                                        .and_then(|d| d.get("actual"))
                                        .and_then(Value::as_u64)
                                    && body
                                        .details
                                        .as_ref()
                                        .and_then(|d| d.get("requested"))
                                        .is_some()
                                {
                                    return Err(HandshakeFailure::ProtocolError(
                                        ContentError::OutOfRange { size: actual },
                                    ));
                                }
                                return Err(HandshakeFailure::ProtocolError(ContentError::Error(
                                    body.with_machine(machine),
                                )));
                            }
                            if let Ok(ready) = serde_json::from_value::<crate::stream::GetReady>(
                                Value::Object(fields),
                            ) {
                                return Ok(ready);
                            }
                            return Err(HandshakeFailure::ProtocolError(ContentError::Error(
                                ErrorBody::new(
                                    ErrorCode::Failed,
                                    format!(
                                        "{machine} answered stream get with an unexpected line."
                                    ),
                                )
                                .with_machine(machine),
                            )));
                        }
                    }
                    if greeted {
                        return Err(HandshakeFailure::ProtocolError(ContentError::Error(
                            ErrorBody::new(
                                ErrorCode::Failed,
                                format!("{machine} answered stream get with non-JSON output."),
                            )
                            .with_machine(machine),
                        )));
                    }
                    noise_lines += 1;
                    noise_bytes += reply_buf.len();
                    if noise_lines > MAX_NOISE_LINES || noise_bytes > MAX_NOISE_BYTES {
                        return Err(HandshakeFailure::ProtocolError(ContentError::Error(
                            ErrorBody::new(
                                ErrorCode::Unreachable,
                                format!(
                                    "{machine} sent too much non-protocol output before starting legion stream."
                                ),
                            )
                            .with_machine(machine),
                        )));
                    }
                }
            };

            let ready = tokio::select! {
                biased;
                () = req.cancel.cancelled() => {
                    drop(stdin);
                    drop(reader);
                    handle.close(Duration::from_secs(1)).await;
                    return Err(ContentError::Error(
                        ErrorBody::new(
                            ErrorCode::Cancelled,
                            format!("Opening content on {machine} was cancelled."),
                        )
                        .with_machine(machine),
                    ));
                }
                res = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake) => match res {
                    Ok(Ok(ready)) => ready,
                    Ok(Err(HandshakeFailure::BrokenPipe(err))) => {
                        drop(stdin);
                        drop(reader);
                        handle.close(Duration::from_secs(1)).await;
                        if reused && !tried_fresh {
                            continue;
                        }
                        return Err(err);
                    }
                    Ok(Err(HandshakeFailure::ProtocolError(err))) => {
                        if reused && matches!(err, ContentError::OutOfRange { .. }) {
                            // Keep-open pipe stays healthy after a pre-body range error.
                            return_idle_pipe(
                                &slot,
                                IdleContentPipe {
                                    stdin,
                                    reader,
                                    handle,
                                    stderr,
                                    systems_fingerprint,
                                    last_used: Instant::now(),
                                },
                            );
                        } else {
                            drop(stdin);
                            drop(reader);
                            handle.close(Duration::from_secs(1)).await;
                        }
                        return Err(err);
                    }
                    Err(_) => {
                        drop(stdin);
                        drop(reader);
                        handle.close(Duration::from_secs(1)).await;
                        return Err(ContentError::Error(
                            ErrorBody::new(
                                ErrorCode::Timeout,
                                format!("Timed out waiting for {machine} to answer stream get."),
                            )
                            .with_machine(machine),
                        ));
                    }
                }
            };

            let Some(mtime) = ready.mtime else {
                drop(stdin);
                drop(reader);
                handle.close(Duration::from_secs(1)).await;
                return Err(ContentError::Error(
                    ErrorBody::new(
                        ErrorCode::Unsupported,
                        format!(
                            "{machine} answered stream get without modification time (older legion version)."
                        ),
                    )
                    .with_hint(
                        "Update legion on that machine so it supports random-access content reads.",
                    )
                    .with_machine(machine),
                ));
            };

            if let Err(err) = validate_remote_get_ready(machine, req, &ready) {
                drop(stdin);
                drop(reader);
                handle.close(Duration::from_secs(1)).await;
                return Err(ContentError::Error(err));
            }

            let keep_open = ready.keep_open == Some(true);

            // Zero-length probe (HEAD, If-Range, POST /v1/content, or empty file): validate the
            // trailer/EOF immediately so the warm keep_open pipe returns to the pool right away.
            if ready.length == 0 {
                if keep_open {
                    let mut trailer = Vec::with_capacity(32);
                    let mut bad = false;
                    loop {
                        match tokio::time::timeout(HANDSHAKE_TIMEOUT, reader.fill_buf()).await {
                            Ok(Ok([])) | Ok(Err(_)) | Err(_) => {
                                bad = true;
                                break;
                            }
                            Ok(Ok(avail)) => {
                                if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
                                    let take = pos + 1;
                                    if trailer.len() + take > MAX_DONE_LINE {
                                        bad = true;
                                    } else {
                                        trailer.extend_from_slice(&avail[..take]);
                                        reader.consume(take);
                                        if !is_done_frame(&trailer) {
                                            bad = true;
                                        }
                                    }
                                    break;
                                }
                                let n = avail.len();
                                if trailer.len() + n > MAX_DONE_LINE {
                                    bad = true;
                                    break;
                                }
                                trailer.extend_from_slice(avail);
                                reader.consume(n);
                            }
                        }
                    }
                    if bad {
                        drop(stdin);
                        drop(reader);
                        handle.close(Duration::from_secs(1)).await;
                        return Err(ContentError::Error(
                            ErrorBody::new(
                                ErrorCode::Failed,
                                format!("{machine} sent an invalid stream done frame."),
                            )
                            .with_machine(machine),
                        ));
                    }
                    return_idle_pipe(
                        &slot,
                        IdleContentPipe {
                            stdin,
                            reader,
                            handle,
                            stderr,
                            systems_fingerprint,
                            last_used: Instant::now(),
                        },
                    );
                } else {
                    match tokio::time::timeout(Duration::from_secs(5), reader.fill_buf()).await {
                        Ok(Ok([])) => {}
                        Ok(Ok(_)) => {
                            drop(stdin);
                            drop(reader);
                            handle.close(Duration::from_secs(1)).await;
                            return Err(ContentError::Error(
                                ErrorBody::new(
                                    ErrorCode::Failed,
                                    format!(
                                        "{machine} sent extra bytes after a zero-length stream get."
                                    ),
                                )
                                .with_machine(machine),
                            ));
                        }
                        Ok(Err(_)) | Err(_) => {
                            drop(stdin);
                            drop(reader);
                            handle.close(Duration::from_secs(1)).await;
                            return Err(ContentError::Error(
                                ErrorBody::new(
                                    ErrorCode::Failed,
                                    format!(
                                        "{machine} failed to finish a zero-length stream get cleanly."
                                    ),
                                )
                                .with_machine(machine),
                            ));
                        }
                    }
                    drop(stdin);
                    drop(reader);
                    handle.close(Duration::from_secs(1)).await;
                }
                return Ok(OpenedContent {
                    size: ready.size,
                    offset: ready.offset,
                    length: 0,
                    mtime_secs: mtime.secs,
                    mtime_nanos: mtime.nanos,
                    file_id: ready.file_id,
                    reader: Box::new(tokio::io::empty()),
                });
            }

            return Ok(OpenedContent {
                size: ready.size,
                offset: ready.offset,
                length: ready.length,
                mtime_secs: mtime.secs,
                mtime_nanos: mtime.nanos,
                file_id: ready.file_id,
                reader: Box::new(RemoteContentReader {
                    parts: Some(RemotePipeParts {
                        stdin,
                        reader,
                        handle,
                        stderr,
                        systems_fingerprint,
                    }),
                    slot,
                    keep_open,
                    phase: ReaderPhase::Data {
                        remaining: ready.length,
                    },
                    cancel: req.cancel.clone(),
                }),
            });
        }
    }
}

fn validate_remote_get_ready(
    machine: &str,
    req: &ContentRequest,
    ready: &crate::stream::GetReady,
) -> Result<(), ErrorBody> {
    if ready.offset != req.offset || ready.offset > ready.size {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{machine} answered stream get with an invalid offset."),
        )
        .with_machine(machine));
    }
    let available = ready.size - ready.offset;
    let expected_len = req.length.map_or(available, |n| n.min(available));
    if ready.length != expected_len {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{machine} answered stream get with an unexpected byte length."),
        )
        .with_machine(machine));
    }
    if ready.length > 0 {
        let valid_end = ready
            .offset
            .checked_add(ready.length)
            .and_then(|s| s.checked_sub(1))
            .is_some_and(|end| end < ready.size);
        if !valid_end {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{machine} answered stream get with an overflowing byte range."),
            )
            .with_machine(machine));
        }
    }
    Ok(())
}

fn is_done_frame(line: &[u8]) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    map.len() == 1 && map.get("done") == Some(&Value::Bool(true))
}

fn pop_idle_pipe(slot: &MachineSlot, fingerprint: &[ResolvedSystem]) -> Option<IdleContentPipe> {
    let mut idle = slot
        .idle_streams
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    // Drop expired or config-stale idle pipes.
    idle.retain(|pipe| {
        now.saturating_duration_since(pipe.last_used) < IDLE_CONTENT_PIPE_TTL
            && pipe.systems_fingerprint == fingerprint
    });
    idle.pop()
}

fn return_idle_pipe(slot: &MachineSlot, pipe: IdleContentPipe) {
    let mut idle = slot
        .idle_streams
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = Instant::now();
    idle.retain(|p| now.saturating_duration_since(p.last_used) < IDLE_CONTENT_PIPE_TTL);
    if idle.len() >= MAX_IDLE_CONTENT_PIPES {
        idle.remove(0);
    }
    idle.push(pipe);
}

async fn acquire_stream_permit(
    permits: &Arc<Semaphore>,
    machine: &str,
    cancel: &CancellationToken,
) -> Result<tokio::sync::OwnedSemaphorePermit, ErrorBody> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!("Opening a stream to {machine} was cancelled."),
        )
        .with_machine(machine)),
        permit = Arc::clone(permits).acquire_owned() => permit.map_err(|_| {
            ErrorBody::new(ErrorCode::Internal, "Stream concurrency limiter closed.")
                .with_machine(machine)
        }),
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

type SystemProbeOutcome = (
    usize,
    ResolvedSystem,
    Result<(Session, Option<RemoteOs>), OpenError>,
);

/// Drains cancelled sibling system probes in the background and closes any session that happened to
/// open right as another system won the race.
fn drain_probe_tasks(mut set: JoinSet<SystemProbeOutcome>) {
    if set.is_empty() {
        return;
    }
    tokio::spawn(async move {
        while let Some(joined) = set.join_next().await {
            if let Ok((_, _, Ok((extra, _)))) = joined {
                extra.close().await;
            }
        }
    });
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
                    build: None,
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

    #[cfg(unix)]
    #[tokio::test]
    async fn ping_probes_dual_boot_systems_concurrently() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        // A dead machine with two hanging systems, plus a dual-boot machine whose first system
        // hangs while its second answers hello immediately.
        let hello_reply = serde_json::to_string(&Response::ok(
            Some(Id::Number(1)),
            serde_json::to_value(HelloResult {
                node: "dual".to_owned(),
                version: "0.1.0".to_owned(),
                build: None,
                protocol: PROTOCOL,
                os: "windows".to_owned(),
                arch: "x86_64".to_owned(),
                systems: Vec::new(),
                system: Some("windows".to_owned()),
                boot_id: None,
            })
            .unwrap(),
        ))
        .unwrap();
        let respond_script = format!(
            "read -r _line; printf '%s\\n' '{}'; exec sleep 30",
            hello_reply.replace('\'', "'\\''")
        );
        let fleet_toml = format!(
            "[machines.dead]\n\
             [[machines.dead.systems]]\n\
             name = \"linux\"\n\
             command = [\"/bin/sh\", \"-c\", \"exec sleep 30\"]\n\
             [[machines.dead.systems]]\n\
             name = \"windows\"\n\
             command = [\"/bin/sh\", \"-c\", \"exec sleep 30\"]\n\
             \n\
             [machines.dual]\n\
             [[machines.dual.systems]]\n\
             name = \"linux\"\n\
             command = [\"/bin/sh\", \"-c\", \"exec sleep 30\"]\n\
             [[machines.dual.systems]]\n\
             name = \"windows\"\n\
             command = [\"/bin/sh\", \"-c\", {}]\n",
            serde_json::to_string(&respond_script).unwrap()
        );
        std::fs::write(paths.config_file("fleet.toml"), fleet_toml).unwrap();

        let store = ConfigStore::new(paths.config_dir());
        let fleet = Fleet::new(&paths);

        // 1. Live second system answers right away without waiting for the hanging first system.
        let t_live = Instant::now();
        let live = fleet
            .ping(
                &store,
                "tower",
                "dual",
                Duration::from_secs(4),
                CancellationToken::new(),
            )
            .await;
        let live_elapsed = t_live.elapsed();
        assert!(live.reachable, "{live:?}");
        assert_eq!(live.system.as_deref(), Some("windows"));
        assert!(
            live_elapsed < Duration::from_secs(2),
            "second system waited on first system: {live_elapsed:?}"
        );

        // 2. Dead two-system machine probes both systems in parallel (one timeout, not two).
        let t_dead = Instant::now();
        let dead = fleet
            .ping(
                &store,
                "tower",
                "dead",
                Duration::from_millis(900),
                CancellationToken::new(),
            )
            .await;
        let dead_elapsed = t_dead.elapsed();
        assert!(!dead.reachable, "{dead:?}");
        let msg = &dead.error.as_ref().unwrap().message;
        assert!(
            msg.contains("linux") && msg.contains("windows"),
            "combined error should mention both systems: {msg}"
        );
        assert!(
            dead_elapsed < Duration::from_millis(2400),
            "dead dual-boot systems were probed serially: {dead_elapsed:?}"
        );

        fleet.close().await;
    }

    #[tokio::test]
    async fn m1_paired_device_cannot_forward_device_invite_to_another_machine() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            paths.config_file("fleet.toml"),
            "[machines.pi]\ncommand = [\"/nonexistent/legion-missing\"]\n",
        )
        .unwrap();
        let pad_id = "88".repeat(16);
        std::fs::write(
            paths.config_file("policy.toml"),
            format!(
                "\
default = \"owner\"

[principals.\"device:pad\"]
role = \"owner\"
device = \"{pad_id}\"
"
            ),
        )
        .unwrap();

        let registry = legion_tools::registry();
        let node = Node::new(paths.clone(), registry);
        let fleet = Fleet::new(&paths);

        // 1. Paired device with pairing_id/device on `device:pad` forwarding `device.invite` to
        //    `pi` must be refused with Denied and a hint to connect directly.
        let mut dev_call = Call::new(
            Door::Ssh,
            "device.invite",
            json!({"name": "child", "role": "observer"}),
        );
        dev_call.pinned = Some("device:pad".to_owned());
        dev_call.device = Some("pad".to_owned());
        dev_call.pairing_id = Some(pad_id.clone());
        let err = fleet.call_machine(&node, "pi", dev_call).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{err:?}");
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("pi") && h.to_lowercase().contains("connect")),
            "expected hint to connect to pi directly: {err:?}"
        );

        // 2. Paired device pinned to shared `owner` principal (with `pairing_id` set) must also be
        //    refused when forwarding `device.invite`.
        let mut shared_dev_call = Call::new(
            Door::Ssh,
            "device.invite",
            json!({"name": "child", "role": "observer"}),
        );
        shared_dev_call.pinned = Some("owner".to_owned());
        shared_dev_call.device = Some("owner-phone".to_owned());
        shared_dev_call.pairing_id = Some("99".repeat(16));
        let err2 = fleet
            .call_machine(&node, "pi", shared_dev_call)
            .await
            .unwrap_err();
        assert_eq!(err2.code, ErrorCode::Denied, "{err2:?}");

        // 3. Local CLI owner (not a paired device) is NOT blocked by the paired-device guard
        //    (fails reaching missing binary instead of Denied).
        let cli_call = Call::new(
            Door::Cli,
            "device.invite",
            json!({"name": "child", "role": "observer"}),
        );
        let err3 = fleet.call_machine(&node, "pi", cli_call).await.unwrap_err();
        assert_ne!(
            err3.code,
            ErrorCode::Denied,
            "local CLI owner must not be denied forwarding device.invite: {err3:?}"
        );

        fleet.close().await;
    }

    #[tokio::test]
    async fn s5_expiring_credential_allows_only_tombstone_only_devices_write_forward() {
        use jiff::{SignedDuration, Timestamp};
        use legion_tools::device::keys::ed25519_public_key_from_seed;
        use legion_tools::device::registry::{DeviceTombstone, DevicesFile, FleetDevice};

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();
        std::fs::write(
            paths.config_file("fleet.toml"),
            "[machines.pi]\ncommand = [\"/nonexistent/legion-missing\"]\n",
        )
        .unwrap();

        let mut registry = legion_tools::registry();
        let fleet = Fleet::new(&paths);
        crate::sync::register(&mut registry, fleet.clone());
        let node = Node::new(paths.clone(), registry);

        let expires = Some(Timestamp::now() + SignedDuration::from_hours(1));
        let now = Timestamp::now().to_string();

        // 1. Live entry in devices.toml -> Denied on forwarding node.
        let mut with_live = DevicesFile::default();
        with_live.devices.insert(
            "11".repeat(16),
            FleetDevice {
                name: "live".to_owned(),
                public_key: ed25519_public_key_from_seed(&[0x11; 32]),
                principal: "device:live".to_owned(),
                role: Some("observer".to_owned()),
                paired_at: now.clone(),
                paired_on: "tower".to_owned(),
                paired_by: None,
                confirmed_by: None,
                confirmed_at: None,
                updated_at: now.clone(),
                updated_on: "tower".to_owned(),
                name_updated_at: now.clone(),
                name_updated_on: "tower".to_owned(),
                role_updated_at: now.clone(),
                role_updated_on: "tower".to_owned(),
                conflict: false,
                role_conflict: false,
            },
        );
        let mut live_call = Call::new(
            Door::Http,
            "config.write",
            json!({"document": "devices.toml", "content": with_live.to_toml_string()}),
        );
        live_call.credential_expires = expires;
        let err_live = fleet
            .call_machine(&node, "pi", live_call)
            .await
            .unwrap_err();
        assert_eq!(err_live.code, ErrorCode::Denied, "{err_live:?}");

        // 2. Empty / invalid devices.toml -> Denied on forwarding node.
        for bad in ["", "not = [valid toml"] {
            let mut bad_call = Call::new(
                Door::Http,
                "config.write",
                json!({"document": "devices.toml", "content": bad}),
            );
            bad_call.credential_expires = expires;
            let err_bad = fleet.call_machine(&node, "pi", bad_call).await.unwrap_err();
            assert_eq!(err_bad.code, ErrorCode::Denied, "{bad:?}: {err_bad:?}");
        }

        // 3. Tombstone-only devices.toml (no live entries, >= 1 tombstone) -> passes forwarding
        //    guard (fails with Unreachable on missing binary rather than Denied).
        let mut tomb_only = DevicesFile::default();
        tomb_only.removed.insert(
            "22".repeat(16),
            DeviceTombstone {
                name: "old".to_owned(),
                principal: "device:old".to_owned(),
                fingerprint: None,
                paired_at: None,
                paired_on: None,
                paired_by: None,
                removed_by: None,
                removed_at: now,
                removed_on: "tower".to_owned(),
            },
        );
        let mut tomb_call = Call::new(
            Door::Http,
            "config.write",
            json!({"document": "devices.toml", "content": tomb_only.to_toml_string()}),
        );
        tomb_call.credential_expires = expires;
        let err_tomb = fleet
            .call_machine(&node, "pi", tomb_call)
            .await
            .unwrap_err();
        assert_ne!(
            err_tomb.code,
            ErrorCode::Denied,
            "tombstone-only devices.toml must pass expiring-credential forward guard: {err_tomb:?}"
        );

        fleet.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn remote_content_rejects_lying_peer_and_queues_concurrent_streams_under_cap() {
        use tokio::io::AsyncReadExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file("node.toml"), "name = \"tower\"\n").unwrap();

        // 1. Misbehaving remote script that responds to `stream` with invalid length, wrong
        //    offset, or extra bytes past declared length depending on requested path.
        let lying_script = dir.path().join("lying-stream.sh");
        std::fs::write(
            &lying_script,
            r#"#!/bin/sh
if [ "$1" != "stream" ]; then
  exit 1
fi
printf '{"protocol":1}\n'
IFS= read -r req || exit 0
case "$req" in
  *wrong_offset*)
    printf '{"size":100,"offset":5,"length":10,"mtime":{"secs":1700000000,"nanos":0},"keep_open":false}\n'
    printf '0123456789'
    ;;
  *wrong_length*)
    printf '{"size":100,"offset":0,"length":50,"mtime":{"secs":1700000000,"nanos":0},"keep_open":false}\n'
    printf '0123456789'
    ;;
  *overflow*)
    printf '{"size":18446744073709551615,"offset":10,"length":18446744073709551615,"mtime":{"secs":1700000000,"nanos":0},"keep_open":false}\n'
    ;;
  *extra_bytes*)
    printf '{"size":10,"offset":0,"length":5,"mtime":{"secs":1700000000,"nanos":0},"keep_open":false}\n'
    printf 'helloEXTRA_UNEXPECTED'
    ;;
  *bad_done*)
    printf '{"size":10,"offset":0,"length":5,"mtime":{"secs":1700000000,"nanos":0},"keep_open":true}\n'
    printf 'helloEXTRA_NOT_DONE\n'
    ;;
  *short_eof*)
    printf '{"size":10,"offset":0,"length":10,"mtime":{"secs":1700000000,"nanos":0},"keep_open":true}\n'
    printf '12345'
    ;;
  *)
    printf '{"size":10,"offset":0,"length":10,"mtime":{"secs":1700000000,"nanos":0},"keep_open":true}\n'
    printf '0123456789{"done":true}\n'
    ;;
esac
"#,
        )
        .unwrap();
        // 2. Slow-open remote script that records max concurrent active processes in a counter
        //    file so we can prove 12 concurrent opens queue cleanly under MAX_STREAMS_PER_TARGET (8).
        let active_dir = dir.path().join("active-streams");
        std::fs::create_dir_all(&active_dir).unwrap();
        let max_file = dir.path().join("max-concurrent.txt");
        let capped_script = dir.path().join("capped-stream.sh");
        std::fs::write(
            &capped_script,
            format!(
                r#"#!/bin/sh
if [ "$1" != "stream" ]; then
  exit 1
fi
marker="{active}/$$"
: > "$marker"
count=$(ls -1 "{active}" | wc -l)
printf '%s\n' "$count" >> "{max}"
printf '{{"protocol":1}}\n'
IFS= read -r _req || true
sleep 0.15
printf '{{"size":4,"offset":0,"length":4,"mtime":{{"secs":1700000000,"nanos":0}},"keep_open":false}}\n'
printf 'OKAY'
rm -f "$marker"
"#,
                active = active_dir.display(),
                max = max_file.display(),
            ),
        )
        .unwrap();

        let fleet_toml = format!(
            "[machines.liar]\ncommand = [\"/bin/sh\", {}, \"rpc\"]\n\
             [machines.capped]\ncommand = [\"/bin/sh\", {}, \"rpc\"]\n",
            serde_json::to_string(&lying_script.to_string_lossy().into_owned()).unwrap(),
            serde_json::to_string(&capped_script.to_string_lossy().into_owned()).unwrap(),
        );
        std::fs::write(paths.config_file("fleet.toml"), fleet_toml).unwrap();

        let node = Node::new(paths.clone(), legion_tools::registry());
        let fleet = Fleet::new(&paths);

        let make_req =
            |machine: &str, path: &str, offset: u64, length: Option<u64>| ContentRequest {
                machine: Some(machine.to_owned()),
                path: path.to_owned(),
                offset,
                length,
                principal: "owner".to_owned(),
                door: Door::Http,
                audit: false,
                cancel: CancellationToken::new(),
            };

        // Wrong offset / wrong length / overflow rejected before any bytes are delivered.
        for (path, offset, len) in [
            ("/wrong_offset", 0u64, Some(10u64)),
            ("/wrong_length", 0, Some(10)),
            ("/overflow", 10, None),
        ] {
            let err = fleet
                .open_content(&node, make_req("liar", path, offset, len))
                .await
                .err()
                .expect("must fail");
            match err {
                ContentError::Error(body) => assert_eq!(body.code, ErrorCode::Failed, "{path}"),
                other => panic!("unexpected {other:?} for {path}"),
            }
        }

        // Extra bytes past declared length (one-shot and keep_open) fail with InvalidData and
        // never forward any extra bytes beyond the declared 5 bytes.
        for path in ["/extra_bytes", "/bad_done"] {
            let mut opened = fleet
                .open_content(&node, make_req("liar", path, 0, Some(5)))
                .await
                .unwrap();
            let mut buf = Vec::new();
            let err = opened.reader.read_to_end(&mut buf).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData, "{path}: {err}");
            assert!(
                buf.len() <= 5 && b"hello".starts_with(&buf),
                "{path}: must never forward extra bytes, got {buf:?}"
            );
        }

        // Short EOF fails with UnexpectedEof.
        let mut short = fleet
            .open_content(&node, make_req("liar", "/short_eof", 0, Some(10)))
            .await
            .unwrap();
        let mut buf = Vec::new();
        let err = short.reader.read_to_end(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);

        // 12 concurrent streams to `capped` all succeed by queueing under MAX_STREAMS_PER_TARGET (8).
        let mut join = JoinSet::new();
        for _ in 0..12 {
            let fleet = fleet.clone();
            let node = node.clone();
            join.spawn(async move {
                let mut opened = fleet
                    .open_content(
                        &node,
                        ContentRequest {
                            machine: Some("capped".to_owned()),
                            path: "/ok".to_owned(),
                            offset: 0,
                            length: Some(4),
                            principal: "owner".to_owned(),
                            door: Door::Http,
                            audit: false,
                            cancel: CancellationToken::new(),
                        },
                    )
                    .await
                    .unwrap();
                let mut out = Vec::new();
                opened.reader.read_to_end(&mut out).await.unwrap();
                assert_eq!(out, b"OKAY");
            });
        }
        while let Some(res) = join.join_next().await {
            res.unwrap();
        }
        let max_seen = std::fs::read_to_string(&max_file)
            .unwrap()
            .lines()
            .filter_map(|l| l.trim().parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        assert!(
            max_seen > 0 && max_seen <= MAX_STREAMS_PER_TARGET,
            "expected peak concurrency in 1..={MAX_STREAMS_PER_TARGET}, got {max_seen}"
        );

        fleet.close().await;
    }
}
