//! High-level JSON-in, JSON-out [`Link`] facade held by a GUI shell or controller app.
//!
//! [`Link`] holds any number of paired (or local) nodes from a serializable [`LinkConfig`], opens
//! sessions lazily, reuses live sessions, reconnects once when a session died before sending a
//! request, cancels in-flight calls by caller-chosen call id, and closes idle sessions.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use legion_proto::{CallParams, ErrorBody, ErrorCode, Event, EventSink, Target};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::content::{
    ContentStream, ContentStreamOptions, DEFAULT_MAX_PARKED_STREAMS, ParkedStreamPool,
    StreamOpenError, handshake_get,
};
use crate::invite::{Enrolled, SshTarget, SshTimeouts};
use crate::put::{CAP_PUT_NO_REPLACE, PutRequest, PutStream, handshake_put, validate_blake3_hash};
use crate::session::{CallError, Session};

/// Default idle duration after which unused sessions are closed (matches `legion serve`).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Default duration to remember that a learned node has not received this device yet (falling back
/// to a relay without retrying direct SSH auth on every call).
pub const DEFAULT_UNSYNCED_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Maximum number of configured nodes (paired plus learned) and machines accepted from a single
/// `device.endpoint` answer.
pub const MAX_CONFIGURED_NODES: usize = 64;

/// Maximum number of candidate hosts accepted per learned machine.
pub const MAX_ENDPOINT_HOSTS: usize = 8;

/// Minimum interval between forced SSH session reconnects triggered by persistent sshd channel
/// refusals while local stream permits are free.
pub const DEFAULT_CHANNEL_REFUSED_RECONNECT_COOLDOWN: Duration = Duration::from_secs(60);

/// Maximum number of pinned host keys accepted per learned machine.
pub const MAX_ENDPOINT_HOST_KEYS: usize = 8;

/// Maximum byte length of a learned `node`, `user`, or `host` field.
pub const MAX_ENDPOINT_FIELD_LEN: usize = 255;

/// Maximum byte length of a single learned OpenSSH host key line.
pub const MAX_ENDPOINT_HOST_KEY_LEN: usize = 4096;

/// How a routed [`Link::call_machine`] call reached its target machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    /// Connected directly to the target machine.
    Direct,
    /// Relayed through another trusted node (`on: <machine>`).
    Relay {
        /// Name of the node that relayed the call.
        via: String,
    },
}

/// Outcome of a routed call on [`Link`], pairing the JSON tool result with the [`Route`] taken.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutedValue {
    /// Tool result JSON returned by the target machine.
    pub value: Value,
    /// Route taken to reach the target machine.
    pub route: Route,
}

/// Observed runtime reachability status for a machine known to [`Link`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MachineRouteStatus {
    /// Reached directly (or configured for direct connection with no refusal observed).
    Direct,
    /// Currently reached by relaying through `via`.
    Relay {
        /// Relay node name.
        via: String,
    },
    /// Learned entry exists, but direct SSH auth was refused (`device_removed_or_unknown`): the
    /// introducer has not yet passed this device on to it.
    NotSyncedYet {
        /// Trusted node that introduced this machine, if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        introducer: Option<String>,
        /// Relay node currently used to reach this machine, if one answered.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
    },
    /// A trusted node reported different host keys than the ones pinned for this machine.
    IdentityChanged {
        /// Trusted node that reported the changed host keys.
        reported_by: String,
    },
}

/// Serializable configuration for [`Link`]: paired or local nodes plus the device's OpenSSH
/// private key string. The caller persists this JSON document in app storage.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkConfig {
    /// OpenSSH private key string for this device (used for SSH nodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_key: Option<String>,
    /// Nodes this controller knows how to reach.
    #[serde(default)]
    pub nodes: Vec<NodeLink>,
    /// Machine names explicitly forgotten by the user; `learn` skips these until the user pairs
    /// with them directly or clears them explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forgotten_nodes: Vec<String>,
}

impl fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkConfig")
            .field(
                "device_key",
                &self.device_key.as_ref().map(|_| "<redacted>"),
            )
            .field("nodes", &self.nodes)
            .field("forgotten_nodes", &self.forgotten_nodes)
            .finish()
    }
}

impl LinkConfig {
    /// Adds or replaces a node entry by name. Adding a directly paired node clears any prior
    /// forgotten marker for that machine.
    pub fn upsert_node(&mut self, entry: NodeLink) {
        if entry.origin == NodeOrigin::Paired {
            self.clear_forgotten(&entry.node);
        }
        if let Some(existing) = self.nodes.iter_mut().find(|n| n.node == entry.node) {
            *existing = entry;
        } else {
            self.nodes.push(entry);
        }
    }

    /// Removes a node entry by name, returning `true` if one was removed.
    pub fn remove_node(&mut self, node: &str) -> bool {
        let before = self.nodes.len();
        self.nodes.retain(|n| n.node != node);
        self.nodes.len() != before
    }

    /// Forgets `node`, records it in [`LinkConfig::forgotten_nodes`] so `learn` will not re-add it
    /// automatically, drops any learned entries whose `learned_from` was `node` (without marking
    /// those dependents forgotten so another paired introducer may re-learn them), and strips any
    /// pending identity changes or conflicts that `node` reported on remaining paired or learned
    /// entries (promoting a remaining conflict from another introducer or clearing the change).
    ///
    /// Returns every node name removed from [`LinkConfig::nodes`] (`node` first when present, then
    /// any dropped learned dependents).
    pub fn forget_node(&mut self, node: &str) -> Vec<String> {
        let mut removed = Vec::new();
        if self.remove_node(node) {
            removed.push(node.to_owned());
        }
        let dependents: Vec<String> = self
            .nodes
            .iter()
            .filter(|n| n.origin == NodeOrigin::Learned && n.learned_from.as_deref() == Some(node))
            .map(|n| n.node.clone())
            .collect();
        if !dependents.is_empty() {
            self.nodes.retain(|n| {
                !(n.origin == NodeOrigin::Learned && n.learned_from.as_deref() == Some(node))
            });
            removed.extend(dependents);
        }
        for entry in &mut self.nodes {
            let Some(mut pending) = entry.identity_changed.take() else {
                continue;
            };
            pending.conflicts.retain(|c| c.reported_by != node);
            if pending.reported_by == node {
                if !pending.conflicts.is_empty() {
                    let promoted = pending.conflicts.remove(0);
                    entry.identity_changed = Some(PendingIdentityChange {
                        target: promoted.target,
                        reported_by: promoted.reported_by,
                        conflicts: pending.conflicts,
                    });
                }
            } else {
                entry.identity_changed = Some(pending);
            }
        }
        if !node.is_empty() && !self.forgotten_nodes.iter().any(|n| n == node) {
            self.forgotten_nodes.push(node.to_owned());
            self.forgotten_nodes.sort();
        }
        removed
    }

    /// Clears a forgotten marker for `node` so it may be learned again.
    pub fn clear_forgotten(&mut self, node: &str) {
        self.forgotten_nodes.retain(|n| n != node);
    }

    /// Returns `true` when `node` was explicitly forgotten by the user.
    pub fn is_forgotten(&self, node: &str) -> bool {
        self.forgotten_nodes.iter().any(|n| n == node)
    }

    /// Validates and applies a pending identity change on a learned node, verifying that
    /// `expected_fingerprints` matches the pending target's host keys and that no conflicting
    /// reports or host-key overlaps exist.
    pub fn apply_accept_identity_change(
        &mut self,
        node: &str,
        expected_fingerprints: &[String],
    ) -> Result<NodeLink, ErrorBody> {
        let idx = self
            .nodes
            .iter()
            .position(|n| n.node == node)
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("No node named {node:?} is configured."),
                )
            })?;
        if self.nodes[idx].origin == NodeOrigin::Paired {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Cannot accept a learned identity change for directly paired node {node:?}."
                ),
            )
            .with_hint(
                "If this machine's SSH identity really changed, pair with it again directly.",
            )
            .with_machine(node));
        }
        let pending = self.nodes[idx].identity_changed.clone().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Node {node:?} does not have a pending identity change."),
            )
            .with_machine(node)
        })?;
        if !pending.conflicts.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Conflicting identity reports exist for {node:?}; cannot accept automatically."
                ),
            )
            .with_hint("Dismiss the warning or pair with this machine directly.")
            .with_machine(node));
        }
        let expected_set: BTreeSet<String> = expected_fingerprints
            .iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        let pending_fps: BTreeSet<String> = pending
            .target
            .host_keys
            .iter()
            .filter_map(|line| public_key_fingerprint(line))
            .collect();
        if expected_set.is_empty() || pending_fps != expected_set {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Pending identity fingerprints for {node:?} do not match what was reviewed."
                ),
            )
            .with_hint("Reload the machine's connection details and review the current fingerprints before accepting.")
            .with_machine(node));
        }
        let pending_keys = canonical_host_keys(&pending.target.host_keys);
        let overlaps_other = self.nodes.iter().enumerate().any(|(i, other)| {
            i != idx
                && other
                    .target
                    .as_ref()
                    .is_some_and(|t| !canonical_host_keys(&t.host_keys).is_disjoint(&pending_keys))
        });
        if overlaps_other {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "Pending host keys for {node:?} overlap another configured machine's pinned keys."
                ),
            )
            .with_hint("Dismiss this report and verify the machine's identity directly.")
            .with_machine(node));
        }
        let entry = &mut self.nodes[idx];
        entry.target = Some(pending.target);
        entry.learned_from = Some(pending.reported_by);
        entry.identity_changed = None;
        Ok(entry.clone())
    }

    /// Clears a pending identity-change warning on `node` without altering its pinned target.
    pub fn apply_dismiss_identity_change(&mut self, node: &str) -> Result<NodeLink, ErrorBody> {
        let entry = self
            .nodes
            .iter_mut()
            .find(|n| n.node == node)
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("No node named {node:?} is configured."),
                )
            })?;
        if entry.identity_changed.is_none() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Node {node:?} does not have a pending identity change."),
            )
            .with_machine(node));
        }
        entry.identity_changed = None;
        Ok(entry.clone())
    }

    /// Looks up a configured node by name.
    pub fn node(&self, name: &str) -> Option<&NodeLink> {
        self.nodes.iter().find(|n| n.node == name)
    }
}

/// How a [`NodeLink`] entry was obtained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeOrigin {
    /// Paired directly by the user via an invite (or a local desktop pipe).
    #[default]
    Paired,
    /// Learned from another already-trusted node via `device.endpoint`.
    Learned,
}

impl NodeOrigin {
    fn is_paired(&self) -> bool {
        matches!(self, Self::Paired)
    }
}

/// A differing endpoint report received while another pending change was already recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictingIdentityReport {
    /// Differing SSH target reported for the machine.
    pub target: SshTarget,
    /// Directly paired node whose authenticated session reported this target.
    pub reported_by: String,
}

/// Pending host-key or endpoint change reported by `device.endpoint` for an already known node.
///
/// Old pinned host keys remain active in [`NodeLink::target`] until the caller explicitly accepts
/// the change via [`Link::accept_identity_change`] (learned nodes only) or dismisses it via
/// [`Link::dismiss_identity_change`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingIdentityChange {
    /// New SSH connection target (carrying the newly reported host keys or endpoint details).
    pub target: SshTarget,
    /// Directly paired node whose authenticated session reported this change.
    pub reported_by: String,
    /// Additional differing reports received while this change was pending; when non-empty,
    /// acceptance is blocked until the user dismisses or pairs directly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<ConflictingIdentityReport>,
}

/// One paired introducer's description of a machine involved in a [`LearnDisagreement`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisagreementReport {
    /// Directly paired node that reported this endpoint.
    pub reported_by: String,
    /// Endpoint target reported by `reported_by`.
    pub target: SshTarget,
}

/// Two or more directly paired introducers returned differing endpoint descriptions for `machine`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnDisagreement {
    /// Machine name that was not pinned because paired introducers disagreed.
    pub machine: String,
    /// Conflicting endpoint reports, sorted by `reported_by`.
    pub reports: Vec<DisagreementReport>,
}

/// Summary returned by [`Link::learn`] and [`Link::learn_from`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnReport {
    /// First directly paired node whose authenticated session answered `device.endpoint`.
    pub introducer: String,
    /// All directly paired nodes whose authenticated sessions answered `device.endpoint`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub introducers: Vec<String>,
    /// Newly learned machines added to [`LinkConfig`].
    pub added: Vec<String>,
    /// Existing learned machines whose non-pin connection details (hosts/port/user) changed.
    pub updated: Vec<String>,
    /// Already known machines whose reported host keys differed from their saved pins (or whose
    /// route changed via a different introducer); old pins were kept and the entries were marked
    /// with [`NodeLink::identity_changed`].
    pub identity_changed: Vec<String>,
    /// New machines that were not pinned because two or more paired introducers reported
    /// differing endpoints or host keys for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagreements: Vec<String>,
    /// Detailed per-introducer reports for each machine listed in [`LearnReport::disagreements`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disagreement_details: Vec<LearnDisagreement>,
    /// Machines that returned an error during `device.endpoint` fan-out.
    pub unreachable: Vec<String>,
    /// Machines whose `device.endpoint` reply failed validation (for example mismatched node name,
    /// disallowed host address, overlapping host keys, or malformed host keys).
    pub rejected: Vec<String>,
}

/// One node entry inside [`LinkConfig`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeLink {
    /// Name of the node (lookup key passed to [`Link::hello`], [`Link::tools`], [`Link::call`]).
    pub node: String,
    /// Pinned SSH connection target for remote nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<SshTarget>,
    /// Local `legion` executable path for the desktop `local` child-process pipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_program: Option<String>,
    /// Principal this controller acts as on the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// Whether this entry was paired directly or learned from another trusted node.
    #[serde(default, skip_serializing_if = "NodeOrigin::is_paired")]
    pub origin: NodeOrigin,
    /// Name of the trusted node that introduced this learned entry, if learned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_from: Option<String>,
    /// Non-`None` when a later `device.endpoint` answer reported different host keys for this
    /// already known machine. Old pins stay in `target` until accepted explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_changed: Option<PendingIdentityChange>,
}

impl NodeLink {
    /// Creates a directly paired SSH node entry.
    pub fn ssh(node: impl Into<String>, target: SshTarget, principal: Option<String>) -> Self {
        NodeLink {
            node: node.into(),
            target: Some(target),
            local_program: None,
            principal,
            origin: NodeOrigin::Paired,
            learned_from: None,
            identity_changed: None,
        }
    }

    /// Creates a learned SSH node entry introduced by `learned_from`.
    pub fn learned(
        node: impl Into<String>,
        target: SshTarget,
        principal: Option<String>,
        learned_from: impl Into<String>,
    ) -> Self {
        NodeLink {
            node: node.into(),
            target: Some(target),
            local_program: None,
            principal,
            origin: NodeOrigin::Learned,
            learned_from: Some(learned_from.into()),
            identity_changed: None,
        }
    }

    /// Creates a local desktop node entry backed by spawning `<program> rpc`.
    pub fn local(node: impl Into<String>, program: impl Into<String>) -> Self {
        NodeLink {
            node: node.into(),
            target: None,
            local_program: Some(program.into()),
            principal: None,
            origin: NodeOrigin::Paired,
            learned_from: None,
            identity_changed: None,
        }
    }

    /// Returns `true` when two entries use the same transport target and principal.
    fn same_transport(&self, other: &Self) -> bool {
        self.node == other.node
            && self.target == other.target
            && self.local_program == other.local_program
            && self.principal == other.principal
    }
}

impl From<Enrolled> for NodeLink {
    fn from(enrolled: Enrolled) -> Self {
        NodeLink::ssh(enrolled.node, enrolled.target, Some(enrolled.principal))
    }
}

/// Tuning options for [`Link`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkOptions {
    /// Close an open session once it has been idle with no active calls for at least this long.
    pub idle_timeout: Duration,
    /// Connect, handshake, and keepalive timeouts (used by SSH and local pipes alike).
    pub ssh_timeouts: SshTimeouts,
    /// How long to remember a learned node whose direct auth failed with
    /// `device_removed_or_unknown` before trying direct SSH again.
    pub unsynced_backoff: Duration,
    /// Whether `learn` accepts loopback hosts (`localhost`, `*.localhost`, `127.0.0.0/8`, `::1`).
    ///
    /// Defaults to `false` so a remote node cannot redirect a controller to a local service;
    /// intended only for tests and debug-only loopback harnesses.
    pub allow_loopback_endpoints: bool,
    /// Maximum number of parked content streams retained on this link before the least recently
    /// parked stream is closed.
    pub max_parked_streams: usize,
    /// Minimum cooldown between session reconnects triggered when sshd keeps refusing new stream
    /// channels while local stream permits are free.
    pub channel_refused_reconnect_cooldown: Duration,
}

impl Default for LinkOptions {
    fn default() -> Self {
        LinkOptions {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            ssh_timeouts: SshTimeouts::default(),
            unsynced_backoff: DEFAULT_UNSYNCED_BACKOFF,
            allow_loopback_endpoints: false,
            max_parked_streams: DEFAULT_MAX_PARKED_STREAMS,
            channel_refused_reconnect_cooldown: DEFAULT_CHANNEL_REFUSED_RECONNECT_COOLDOWN,
        }
    }
}

/// Custom session opener hook (useful for embedding custom transports or unit testing).
pub type SessionOpener = Arc<
    dyn Fn(
            NodeLink,
            Option<String>,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Session, ErrorBody>> + Send>>
        + Send
        + Sync,
>;

/// High-level controller handle that manages lazy, reused, idle-reaped sessions to paired nodes.
///
/// Cloning is cheap and shares the same session pool and active-call table.
#[derive(Clone)]
pub struct Link {
    inner: Arc<LinkInner>,
}

struct LinkInner {
    config: Mutex<LinkConfig>,
    options: LinkOptions,
    opener: SessionOpener,
    /// Serializes `pair`, `forget`, and `learn` mutations so device key generation, forgetting,
    /// and endpoint learning never race.
    mutation_lock: tokio::sync::Mutex<()>,
    slots: Mutex<HashMap<String, Arc<NodeSlot>>>,
    /// Sessions displaced by a config update while callers may still hold clones; reaped by
    /// `close_idle` once no caller holds a reference or in-flight call, or closed by `forget`.
    retired: Mutex<Vec<LiveSession>>,
    active_calls: Mutex<HashMap<String, CancellationToken>>,
    /// Learned nodes that rejected our device key with `device_removed_or_unknown`; bypassed in
    /// favour of a relay until the recorded deadline.
    unsynced_until: Mutex<HashMap<String, Instant>>,
    /// Last observed runtime route status per machine.
    routes: Mutex<HashMap<String, MachineRouteStatus>>,
    /// Per-link LRU pool capping parked content streams.
    parked_streams: Arc<ParkedStreamPool>,
    /// Last timestamp per machine when persistent sshd channel refusals with free permits triggered
    /// a session reconnect (rate-limited to at most once per cooldown window).
    channel_refused_reconnect_at: Mutex<HashMap<String, Instant>>,
    reaper_started: AtomicBool,
}

struct NodeSlot {
    open_lock: tokio::sync::Mutex<()>,
    current: Mutex<Option<LiveSession>>,
}

#[derive(Clone)]
struct LiveSession {
    session: Arc<Session>,
    config_snapshot: NodeLink,
    last_used: Arc<Mutex<Instant>>,
    in_flight: Arc<AtomicUsize>,
    stream_capabilities: Arc<Mutex<Option<Vec<String>>>>,
}

impl LiveSession {
    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
    }

    fn record_stream_capabilities(&self, caps: Vec<String>) {
        let mut guard = self
            .stream_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_mut() {
            None => {
                let mut unique = BTreeSet::new();
                for cap in caps {
                    unique.insert(cap);
                }
                *guard = Some(unique.into_iter().collect());
            }
            Some(existing) => {
                for cap in caps {
                    if !existing.iter().any(|e| e == &cap) {
                        existing.push(cap);
                    }
                }
                existing.sort();
            }
        }
    }

    fn stream_capabilities(&self) -> Option<Vec<String>> {
        self.stream_capabilities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn acquire_flight(&self) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        InFlightGuard {
            in_flight: Arc::clone(&self.in_flight),
            last_used: Arc::clone(&self.last_used),
        }
    }

    fn is_idle_for(&self, timeout: Duration) -> bool {
        if self.in_flight.load(Ordering::Acquire) != 0 {
            return false;
        }
        self.last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .elapsed()
            >= timeout
    }
}

struct InFlightGuard {
    in_flight: Arc<AtomicUsize>,
    last_used: Arc<Mutex<Instant>>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ActiveCallGuard {
    active_calls: Arc<LinkInner>,
    id: String,
}

impl Drop for ActiveCallGuard {
    fn drop(&mut self) {
        self.active_calls
            .active_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.id);
    }
}

impl Link {
    /// Creates a new [`Link`] from `config` using default [`LinkOptions`].
    pub fn new(config: LinkConfig) -> Self {
        Self::with_options(config, LinkOptions::default())
    }

    /// Creates a new [`Link`] from `config` with custom [`LinkOptions`].
    pub fn with_options(config: LinkConfig, options: LinkOptions) -> Self {
        let opener: SessionOpener = Arc::new(move |entry, device_key, cancel| {
            Box::pin(default_open_session(entry, device_key, options, cancel))
        });
        Self::with_opener(config, options, opener)
    }

    /// Creates a new [`Link`] backed by a custom [`SessionOpener`].
    pub fn with_opener(config: LinkConfig, options: LinkOptions, opener: SessionOpener) -> Self {
        Link {
            inner: Arc::new(LinkInner {
                config: Mutex::new(config),
                options,
                opener,
                mutation_lock: tokio::sync::Mutex::new(()),
                slots: Mutex::new(HashMap::new()),
                retired: Mutex::new(Vec::new()),
                active_calls: Mutex::new(HashMap::new()),
                unsynced_until: Mutex::new(HashMap::new()),
                routes: Mutex::new(HashMap::new()),
                parked_streams: Arc::new(ParkedStreamPool::new(options.max_parked_streams)),
                channel_refused_reconnect_at: Mutex::new(HashMap::new()),
                reaper_started: AtomicBool::new(false),
            }),
        }
    }

    /// Returns a snapshot of the current [`LinkConfig`] (e.g. for persistence).
    pub fn config(&self) -> LinkConfig {
        self.inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Parses `invite_text`, ensures this [`Link`] has a device key (generating one on first use
    /// and never replacing an existing key), enrols with the node under `device_name`, and updates
    /// the live [`LinkConfig`] in place.
    ///
    /// Callers can persist the updated configuration afterwards via [`Link::config`] and call the
    /// newly paired node immediately without rebuilding [`Link`]. If enrolment fails, the live
    /// configuration is left unchanged.
    #[cfg(feature = "ssh")]
    pub async fn pair(&self, invite_text: &str, device_name: &str) -> Result<Enrolled, ErrorBody> {
        self.pair_cancellable(invite_text, device_name, CancellationToken::new())
            .await
    }

    /// Like [`Link::pair`], but stops waiting and leaves [`LinkConfig`] untouched when `cancel`
    /// is triggered.
    #[cfg(feature = "ssh")]
    pub async fn pair_cancellable(
        &self,
        invite_text: &str,
        device_name: &str,
        cancel: CancellationToken,
    ) -> Result<Enrolled, ErrorBody> {
        self.pair_with(
            invite_text,
            device_name,
            cancel,
            |invite, key, name, timeouts, allow_auth_failed_recovery, cancel| async move {
                crate::invite::enrol_with_recovery(
                    &invite,
                    &key,
                    &name,
                    timeouts,
                    cancel,
                    allow_auth_failed_recovery,
                )
                .await
            },
        )
        .await
    }

    #[cfg(feature = "ssh")]
    async fn pair_with<F, Fut>(
        &self,
        invite_text: &str,
        device_name: &str,
        cancel: CancellationToken,
        enrol_fn: F,
    ) -> Result<Enrolled, ErrorBody>
    where
        F: FnOnce(
            crate::invite::Invite,
            crate::key::DeviceKey,
            String,
            SshTimeouts,
            bool,
            CancellationToken,
        ) -> Fut,
        Fut: Future<Output = Result<Enrolled, ErrorBody>>,
    {
        let invite = crate::invite::Invite::parse(invite_text)?;
        let name = device_name.trim();
        if name.is_empty() || name.chars().any(|c| c.is_control()) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Device name must be a non-empty single-line string.",
            )
            .with_hint("Pass a short label such as `phone` or `tablet`."));
        }
        if cancel.is_cancelled() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Enrolment with {} was cancelled.", invite.node),
            )
            .with_machine(&invite.node));
        }

        let _mutation_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("Enrolment with {} was cancelled.", invite.node),
                )
                .with_machine(&invite.node));
            }
            guard = self.inner.mutation_lock.lock() => guard,
        };

        let (existing_key, already_paired_same_principal) = {
            let cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let already = cfg.nodes.iter().any(|n| {
                n.node == invite.node
                    && n.origin == NodeOrigin::Paired
                    && n.principal.as_deref() == Some(invite.principal.as_str())
            });
            (cfg.device_key.clone(), already)
        };
        let (device_key, generated_pem) = match existing_key {
            Some(pem) => (crate::key::DeviceKey::from_openssh(&pem)?, None),
            None => {
                let key = crate::key::DeviceKey::generate()?;
                let pem = key.to_openssh()?;
                (key, Some(pem))
            }
        };

        let node_name = invite.node.clone();
        let enrolled = enrol_fn(
            invite,
            device_key,
            name.to_owned(),
            self.inner.options.ssh_timeouts,
            !already_paired_same_principal,
            cancel.clone(),
        )
        .await?;

        if cancel.is_cancelled() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!("Enrolment with {node_name} was cancelled."),
            )
            .with_machine(&node_name));
        }

        let entry = NodeLink::from(enrolled.clone());
        {
            let mut cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cfg.device_key.is_none()
                && let Some(pem) = generated_pem
            {
                cfg.device_key = Some(pem);
            }
            cfg.upsert_node(entry);
        }

        // Retire any cached session for this node so the next call opens cleanly with the fresh
        // enrolment (while leaving any in-flight call untouched until it completes).
        self.clear_route_state(&enrolled.node);
        self.retire_node(&enrolled.node);
        Ok(enrolled)
    }

    /// Stores a validated OpenSSH device key in the live [`LinkConfig`] in place without closing
    /// existing sessions.
    ///
    /// Intended for callers that persist a newly generated key before calling [`Link::pair`].
    /// Setting the same key again is a no-op; attempting to replace a different existing key
    /// returns `conflict`.
    #[cfg(feature = "ssh")]
    pub fn set_device_key(&self, pem: String) -> Result<(), ErrorBody> {
        let _ = crate::key::DeviceKey::from_openssh(&pem)?;
        let mut cfg = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = &cfg.device_key
            && existing != &pem
        {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                "A different device key is already configured on this link.",
            ));
        }
        cfg.device_key = Some(pem);
        Ok(())
    }

    /// Adds or updates a node entry in the live config. If its connection details changed, any
    /// existing session to that node will be replaced on next use.
    pub fn upsert_node(&self, entry: NodeLink) {
        self.inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .upsert_node(entry);
    }

    /// Closes any live or retired sessions for `node` (and any learned entries introduced by it),
    /// removes them from [`LinkConfig`], and records `node` in [`LinkConfig::forgotten_nodes`] so
    /// subsequent `learn` calls will not re-add it automatically. Returns `true` if `node` was
    /// configured.
    ///
    /// Keeps the device key intact so other paired nodes continue working and future pairings
    /// reuse the same device identity.
    pub async fn forget(&self, node: &str) -> bool {
        let _mutation_guard = self.inner.mutation_lock.lock().await;
        let removed_nodes = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .forget_node(node);
        let removed_self = removed_nodes.iter().any(|n| n == node);
        for evicted in &removed_nodes {
            self.clear_route_state(evicted);
            self.evict(evicted).await;
        }
        if !removed_self {
            self.clear_route_state(node);
            self.evict(node).await;
        }
        self.inner
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|_, status| {
                !matches!(
                    status,
                    MachineRouteStatus::IdentityChanged { reported_by } if reported_by == node
                )
            });
        removed_self
    }

    /// Removes a node from the config and closes any live session to it (equivalent to
    /// [`Link::forget`]).
    pub async fn remove_node(&self, node: &str) -> bool {
        self.forget(node).await
    }

    /// Learns direct SSH endpoints for every answering machine in the fleet by querying
    /// `device.endpoint` with `on: "all"` across every reachable directly paired node.
    ///
    /// Rules enforced before updating [`LinkConfig`]:
    /// - Only directly paired nodes (`NodeOrigin::Paired`) may act as introducers; learned nodes
    ///   are never queried as introducers.
    /// - Each answer's `node` field must match the machine key it is filed under, obey count and
    ///   length caps, use allowed host addresses, and not overlap any other machine's pinned keys.
    /// - When two or more paired nodes answer, a new machine is pinned only when all answering
    ///   paired nodes agree on its host keys; disagreement is recorded in
    ///   [`LearnReport::disagreements`] instead of pinning.
    /// - A learned answer never replaces the pinned host keys (or target) of a directly paired
    ///   node; a differing key report is recorded as information only.
    /// - A different introducer cannot silently rewrite a learned node's hosts, port, user, or
    ///   provenance; it is held as a pending change until explicitly accepted or dismissed.
    /// - Re-reporting old keys from a single introducer does not clear a pending warning; only
    ///   explicit dismissal or agreement across two or more reachable paired introducers clears it.
    pub async fn learn(&self) -> Result<LearnReport, ErrorBody> {
        let _mutation_guard = self.inner.mutation_lock.lock().await;
        let paired_candidates = self.paired_introducers();
        if paired_candidates.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                "No directly paired nodes are available to learn fleet endpoints from.",
            )
            .with_hint("Pair with at least one machine directly first."));
        }
        let mut batches: Vec<(String, Value)> = Vec::new();
        let mut last_err: Option<ErrorBody> = None;
        for candidate in paired_candidates {
            match self
                .call_inner(
                    None,
                    &candidate,
                    "device.endpoint",
                    json!({}),
                    Some(json!("all")),
                    |_| {},
                )
                .await
            {
                Ok(value) => batches.push((candidate, value)),
                Err(err) => {
                    let retry = is_unsent_connect_error(&err);
                    last_err = Some(err);
                    if !retry && batches.is_empty() {
                        // A non-connect error (e.g. node_mismatch, host_key_mismatch, or
                        // device_removed_or_unknown) from a paired node still allows other paired
                        // nodes to answer if present, but surfaces if none answer.
                    }
                }
            }
        }
        if batches.is_empty() {
            return Err(last_err.unwrap_or_else(|| {
                ErrorBody::new(
                    ErrorCode::Unreachable,
                    "Could not reach any directly paired node to learn fleet endpoints.",
                )
            }));
        }
        Ok(self.apply_learned_batches(batches))
    }

    /// Calls `device.endpoint` with `on: "all"` over an authenticated session to directly paired
    /// `introducer` and applies the learned endpoints to [`LinkConfig`].
    ///
    /// Refuses learned nodes with `ErrorCode::Denied`.
    pub async fn learn_from(&self, introducer: &str) -> Result<LearnReport, ErrorBody> {
        let _mutation_guard = self.inner.mutation_lock.lock().await;
        {
            let cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = cfg.node(introducer).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("No node named {introducer:?} is configured."),
                )
            })?;
            if entry.origin != NodeOrigin::Paired {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!("Learned node {introducer:?} cannot act as an introducer."),
                )
                .with_hint("Only directly paired machines may introduce other machines.")
                .with_machine(introducer));
            }
        }
        let value = self
            .call_inner(
                None,
                introducer,
                "device.endpoint",
                json!({}),
                Some(json!("all")),
                |_| {},
            )
            .await?;
        Ok(self.apply_learned_batches(vec![(introducer.to_owned(), value)]))
    }

    /// Explicitly accepts a pending host-key or endpoint change recorded on learned node `node`,
    /// requiring `expected_fingerprints` to match the pending target's host keys exactly and
    /// refusing directly paired nodes or entries with conflicting reports.
    pub fn accept_identity_change(
        &self,
        node: &str,
        expected_fingerprints: &[String],
    ) -> Result<NodeLink, ErrorBody> {
        let updated = {
            let mut cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            cfg.apply_accept_identity_change(node, expected_fingerprints)?
        };
        self.retire_node(node);
        Ok(updated)
    }

    /// Dismisses a pending identity-change warning on `node` without altering its pinned target.
    pub fn dismiss_identity_change(&self, node: &str) -> Result<NodeLink, ErrorBody> {
        let mut cfg = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cfg.apply_dismiss_identity_change(node)
    }

    fn paired_introducers(&self) -> Vec<String> {
        self.inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .nodes
            .iter()
            .filter(|n| n.origin == NodeOrigin::Paired)
            .map(|n| n.node.clone())
            .collect()
    }

    fn is_paired(&self, node: &str) -> bool {
        self.inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .node(node)
            .is_some_and(|n| n.origin == NodeOrigin::Paired)
    }

    fn ordered_candidates(&self, preferred: Option<&str>) -> Vec<String> {
        let cfg = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut out: Vec<String> = Vec::with_capacity(cfg.nodes.len());
        if let Some(pref) = preferred
            && cfg.nodes.iter().any(|n| n.node == pref)
        {
            out.push(pref.to_owned());
        }
        // Directly paired (and local) nodes first.
        for n in &cfg.nodes {
            if n.origin == NodeOrigin::Paired && !out.iter().any(|x| x == &n.node) {
                out.push(n.node.clone());
            }
        }
        // Learned nodes without a pending identity change next.
        for n in &cfg.nodes {
            if n.origin == NodeOrigin::Learned
                && n.identity_changed.is_none()
                && !out.iter().any(|x| x == &n.node)
            {
                out.push(n.node.clone());
            }
        }
        // Remaining learned nodes last.
        for n in &cfg.nodes {
            if !out.iter().any(|x| x == &n.node) {
                out.push(n.node.clone());
            }
        }
        out
    }

    fn is_learned(&self, node: &str) -> bool {
        self.inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .node(node)
            .is_some_and(|n| n.origin == NodeOrigin::Learned)
    }

    fn apply_learned_batches(&self, batches: Vec<(String, Value)>) -> LearnReport {
        let mut report = LearnReport {
            introducer: batches.first().map(|(i, _)| i.clone()).unwrap_or_default(),
            introducers: batches.iter().map(|(i, _)| i.clone()).collect(),
            ..LearnReport::default()
        };

        let allow_loopback = self.inner.options.allow_loopback_endpoints;
        let mut valid_by_introducer: Vec<(String, BTreeMap<String, SshTarget>)> = Vec::new();
        let mut unreachable_set: BTreeSet<String> = BTreeSet::new();
        let mut rejected_set: BTreeSet<String> = BTreeSet::new();

        for (introducer, value) in batches {
            let mut entries: Vec<(String, Value)> =
                if let Some(results) = value.get("results").and_then(Value::as_object) {
                    results
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                } else if value.get("node").is_some() {
                    vec![(introducer.clone(), json!({ "ok": value }))]
                } else {
                    valid_by_introducer.push((introducer, BTreeMap::new()));
                    continue;
                };
            entries.sort_by(|a, b| a.0.cmp(&b.0));

            let mut parsed: BTreeMap<String, SshTarget> = BTreeMap::new();
            for (idx, (machine_key, item)) in entries.into_iter().enumerate() {
                if idx >= MAX_CONFIGURED_NODES {
                    rejected_set.insert(machine_key);
                    continue;
                }
                if item.get("error").is_some() {
                    unreachable_set.insert(machine_key);
                    continue;
                }
                let Some(ok_val) = item.get("ok") else {
                    rejected_set.insert(machine_key);
                    continue;
                };
                match parse_and_validate_endpoint(&machine_key, ok_val, allow_loopback) {
                    Ok(target) => {
                        parsed.insert(machine_key, target);
                    }
                    Err(()) => {
                        rejected_set.insert(machine_key);
                    }
                }
            }

            // Detect duplicate host keys shared by multiple distinct machines within one batch.
            let mut key_owners: HashMap<String, BTreeSet<String>> = HashMap::new();
            for (machine, target) in &parsed {
                for key in canonical_host_keys(&target.host_keys) {
                    key_owners.entry(key).or_default().insert(machine.clone());
                }
            }
            let mut dup_machines: Vec<String> = Vec::new();
            for (machine, target) in &parsed {
                let has_dup = canonical_host_keys(&target.host_keys)
                    .iter()
                    .any(|k| key_owners.get(k).is_some_and(|owners| owners.len() > 1));
                if has_dup {
                    dup_machines.push(machine.clone());
                }
            }
            // Keep existing nodes intact (handled individually below) while stripping colliding
            // new candidates from this batch.
            let cfg_snapshot = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            for dup in dup_machines {
                if cfg_snapshot.node(&dup).is_none() {
                    parsed.remove(&dup);
                    rejected_set.insert(dup);
                }
            }

            valid_by_introducer.push((introducer, parsed));
        }

        let mut all_machines: BTreeSet<String> = BTreeSet::new();
        for (_, map) in &valid_by_introducer {
            for machine in map.keys() {
                all_machines.insert(machine.clone());
            }
        }

        let total_introducers = valid_by_introducer.len();
        let mut cfg = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        for machine in all_machines {
            if cfg.is_forgotten(&machine) {
                continue;
            }

            let mut reports: Vec<(&str, &SshTarget)> = Vec::new();
            for (introducer, map) in &valid_by_introducer {
                if let Some(target) = map.get(&machine) {
                    let new_keys = canonical_host_keys(&target.host_keys);
                    let overlaps_other = cfg.nodes.iter().any(|other| {
                        other.node != machine
                            && other.target.as_ref().is_some_and(|t| {
                                !canonical_host_keys(&t.host_keys).is_disjoint(&new_keys)
                            })
                    });
                    if overlaps_other {
                        rejected_set.insert(machine.clone());
                    } else {
                        reports.push((introducer.as_str(), target));
                    }
                }
            }
            if reports.is_empty() {
                continue;
            }

            if let Some(existing) = cfg.nodes.iter_mut().find(|n| n.node == machine) {
                let Some(old_target) = existing.target.clone() else {
                    // Local desktop pipe (`target: None`): never overwrite with an SSH target.
                    continue;
                };
                let old_keys = canonical_host_keys(&old_target.host_keys);

                // Check whether any report fails pin satisfaction (every pinned key still presented
                // and no unpinned key required).
                let mut key_mismatch = false;
                for (introducer, new_target) in &reports {
                    let new_keys = canonical_host_keys(&new_target.host_keys);
                    if !pins_satisfied(&old_keys, &new_keys) {
                        key_mismatch = true;
                        record_pending_identity_change(existing, (*new_target).clone(), introducer);
                    }
                }
                if key_mismatch {
                    report.identity_changed.push(machine);
                    continue;
                }

                if existing.origin == NodeOrigin::Paired {
                    // A learned report never replaces a directly paired node's target or pins.
                    // Clear an existing informational warning only when 2+ reachable paired
                    // introducers all agree on the current pins.
                    if total_introducers >= 2 && reports.len() == total_introducers {
                        existing.identity_changed = None;
                    }
                    continue;
                }

                // Learned entry where all reports satisfy pinned host keys:
                // 1. A different introducer may NOT silently rewrite hosts/port/user or provenance.
                let mut route_change_pending = false;
                for (introducer, new_target) in &reports {
                    let same_route = old_target.hosts == new_target.hosts
                        && old_target.port == new_target.port
                        && old_target.user == new_target.user;
                    if !same_route && existing.learned_from.as_deref() != Some(*introducer) {
                        route_change_pending = true;
                        record_pending_identity_change(existing, (*new_target).clone(), introducer);
                    }
                }
                // 2. If multiple introducers disagree with each other on hosts/port/user, flag it.
                if !route_change_pending && reports.len() >= 2 {
                    let first = reports[0].1;
                    let all_route_agree = reports.iter().all(|(_, t)| {
                        t.hosts == first.hosts && t.port == first.port && t.user == first.user
                    });
                    if !all_route_agree {
                        route_change_pending = true;
                        for (introducer, new_target) in &reports {
                            let same_route = old_target.hosts == new_target.hosts
                                && old_target.port == new_target.port
                                && old_target.user == new_target.user;
                            if !same_route {
                                record_pending_identity_change(
                                    existing,
                                    (*new_target).clone(),
                                    introducer,
                                );
                            }
                        }
                    }
                }
                if route_change_pending {
                    report.identity_changed.push(machine);
                    continue;
                }

                // Clear a prior pending warning ONLY when 2+ reachable paired introducers all
                // report matching pins and matching routes. A single introducer re-reporting old
                // keys never auto-clears a pending warning.
                if existing.identity_changed.is_some() {
                    if total_introducers >= 2 && reports.len() == total_introducers {
                        existing.identity_changed = None;
                    } else {
                        continue;
                    }
                }

                // Allow the original introducer to update non-pin hosts/port/user when no warning
                // is pending and pins are satisfied.
                if let Some((_, from_original)) = reports
                    .iter()
                    .find(|(intro, _)| existing.learned_from.as_deref() == Some(*intro))
                    && let Some(target_mut) = existing.target.as_mut()
                {
                    let changed = target_mut.hosts != from_original.hosts
                        || target_mut.port != from_original.port
                        || target_mut.user != from_original.user;
                    if changed {
                        target_mut.hosts = from_original.hosts.clone();
                        target_mut.port = from_original.port;
                        target_mut.user = from_original.user.clone();
                        report.updated.push(machine);
                    }
                }
            } else {
                if cfg.nodes.len() >= MAX_CONFIGURED_NODES {
                    rejected_set.insert(machine);
                    continue;
                }
                // When two or more paired introducers report this brand-new machine, pin it only
                // if all reports agree. An error/unreachable reply from another paired node is an
                // abstention, not a disagreement, so a single reporting paired node pins on its
                // own when no other paired node can see the machine.
                if reports.len() >= 2 {
                    let first_target = reports[0].1;
                    let all_agree = reports
                        .iter()
                        .all(|(_, t)| same_endpoint_target(first_target, t));
                    if !all_agree {
                        let mut detail_reports: Vec<DisagreementReport> = reports
                            .iter()
                            .map(|(intro, target)| DisagreementReport {
                                reported_by: (*intro).to_owned(),
                                target: (*target).clone(),
                            })
                            .collect();
                        detail_reports.sort_by(|a, b| a.reported_by.cmp(&b.reported_by));
                        report.disagreement_details.push(LearnDisagreement {
                            machine: machine.clone(),
                            reports: detail_reports,
                        });
                        report.disagreements.push(machine);
                        continue;
                    }
                }
                let (introducer, new_target) = reports[0];
                cfg.nodes.push(NodeLink::learned(
                    machine.clone(),
                    new_target.clone(),
                    None,
                    introducer,
                ));
                report.added.push(machine);
            }
        }

        // Remove machines that succeeded through another reachable introducer from unreachable.
        for added in &report.added {
            unreachable_set.remove(added);
        }
        for updated in &report.updated {
            unreachable_set.remove(updated);
        }
        for changed in &report.identity_changed {
            unreachable_set.remove(changed);
        }

        report.added.sort();
        report.updated.sort();
        report.identity_changed.sort();
        report.disagreements.sort();
        report
            .disagreement_details
            .sort_by(|a, b| a.machine.cmp(&b.machine));
        report.unreachable = unreachable_set.into_iter().collect();
        report.rejected = rejected_set.into_iter().collect();
        report
    }

    /// Sends a live `hello` to `node` (opening a session lazily if needed) and returns the
    /// `HelloResult` as JSON.
    pub async fn hello(&self, node: &str) -> Result<Value, ErrorBody> {
        self.ensure_reaper();
        let cancel = CancellationToken::new();
        let live = self.get_or_open(node, cancel.clone()).await?;
        let _flight = live.acquire_flight();
        let timeout = self.inner.options.ssh_timeouts.handshake;
        let result = match live.session.hello(timeout, cancel.clone()).await {
            Ok(hello) => Ok(hello),
            Err(CallError::NotWritten(_)) => {
                self.invalidate_if_same(node, &live.session);
                let fresh = self.get_or_open(node, cancel.clone()).await?;
                let _flight2 = fresh.acquire_flight();
                fresh
                    .session
                    .hello(timeout, cancel)
                    .await
                    .map_err(|err| self.handle_call_error(node, &fresh.session, err))
            }
            Err(err) => Err(self.handle_call_error(node, &live.session, err)),
        }?;
        serde_json::to_value(&result).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not serialize hello result: {err}."),
            )
            .with_machine(node)
        })
    }

    /// Lists tools offered by `node`, passing `on` through untouched when provided, and returns
    /// the `ToolsResult` JSON object (`{"tools": [...]}`).
    pub async fn tools(&self, node: &str, on: Option<Value>) -> Result<Value, ErrorBody> {
        self.ensure_reaper();
        let target = parse_optional_target(on)?;
        let cancel = CancellationToken::new();
        let live = self.get_or_open(node, cancel.clone()).await?;
        let _flight = live.acquire_flight();
        let result = match live.session.tools(target.clone(), cancel.clone()).await {
            Ok(tools) => Ok(tools),
            Err(CallError::NotWritten(_)) => {
                self.invalidate_if_same(node, &live.session);
                let fresh = self.get_or_open(node, cancel.clone()).await?;
                let _flight2 = fresh.acquire_flight();
                fresh
                    .session
                    .tools(target, cancel)
                    .await
                    .map_err(|err| self.handle_call_error(node, &fresh.session, err))
            }
            Err(err) => Err(self.handle_call_error(node, &live.session, err)),
        }?;
        serde_json::to_value(&result).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Could not serialize tools result: {err}."),
            )
            .with_machine(node)
        })
    }

    /// Calls `tool` on `machine` using direct-first routing with relay fallback:
    ///
    /// - Connects directly to `machine` when this [`Link`] has an entry for it (unless it is a
    ///   learned entry currently backed off as not-yet-synced) and it answers.
    /// - Otherwise relays through another answering node with `on: <machine>`.
    /// - Failover occurs strictly when the previous attempt provably never sent the request
    ///   (`details.sent == false`).
    /// - A direct `device_removed_or_unknown` refusal on a learned entry falls back to a relay and
    ///   is remembered for [`LinkOptions::unsynced_backoff`]; if every machine including the paired
    ///   one refuses with `device_removed_or_unknown` (or no relay answers), that refusal is
    ///   returned.
    pub async fn call_machine(
        &self,
        machine: &str,
        tool: &str,
        arguments: Value,
        events: impl FnMut(Value),
    ) -> Result<RoutedValue, ErrorBody> {
        self.call_machine_inner(None, machine, tool, arguments, events)
            .await
    }

    /// Like [`Link::call_machine`], registering the routed call under `call_id` so [`Link::cancel`]
    /// cancels whichever direct or relay attempt is running.
    pub async fn call_machine_with_id(
        &self,
        call_id: impl Into<String>,
        machine: &str,
        tool: &str,
        arguments: Value,
        events: impl FnMut(Value),
    ) -> Result<RoutedValue, ErrorBody> {
        self.call_machine_inner(Some(call_id.into()), machine, tool, arguments, events)
            .await
    }

    /// Lists tools offered by `machine`, going direct when possible and relaying through another
    /// answering node otherwise (obeying the same `details.sent == false` failover rules as
    /// [`Link::call_machine`]).
    pub async fn tools_machine(&self, machine: &str) -> Result<RoutedValue, ErrorBody> {
        self.ensure_reaper();
        let entry = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .node(machine)
            .cloned();

        if let Some(ref e) = entry
            && e.local_program.is_some()
            && e.target.is_none()
        {
            let value = self.tools(machine, None).await?;
            self.record_route(machine, MachineRouteStatus::Direct);
            return Ok(RoutedValue {
                value,
                route: Route::Direct,
            });
        }

        let entry_is_learned = entry
            .as_ref()
            .is_some_and(|e| e.origin == NodeOrigin::Learned);
        let entry_learned_from = entry.as_ref().and_then(|e| e.learned_from.clone());
        let in_unsynced_backoff = entry_is_learned && self.is_in_unsynced_backoff(machine);

        let mut direct_err: Option<ErrorBody> = None;
        let mut learned_auth_err: Option<ErrorBody> = None;

        if entry.is_some() && !in_unsynced_backoff {
            match self.tools(machine, None).await {
                Ok(value) => {
                    self.clear_unsynced(machine);
                    self.record_route(machine, MachineRouteStatus::Direct);
                    return Ok(RoutedValue {
                        value,
                        route: Route::Direct,
                    });
                }
                Err(err) => {
                    if !is_unsent(&err) {
                        return Err(err);
                    }
                    if is_device_removed_or_unknown(&err) {
                        if entry_is_learned {
                            self.mark_unsynced(machine);
                            learned_auth_err = Some(err);
                        } else {
                            return Err(err);
                        }
                    } else if matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout) {
                        direct_err = Some(err);
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        let relays = self.relay_candidates(machine, entry_learned_from.as_deref());
        let mut last_relay_err: Option<ErrorBody> = None;
        let mut relay_auth_err: Option<ErrorBody> = None;

        for relay in relays {
            match self
                .tools(&relay, Some(Value::String(machine.to_owned())))
                .await
            {
                Ok(value) => {
                    let status = if in_unsynced_backoff || learned_auth_err.is_some() {
                        MachineRouteStatus::NotSyncedYet {
                            introducer: entry_learned_from.clone(),
                            via: Some(relay.clone()),
                        }
                    } else {
                        MachineRouteStatus::Relay { via: relay.clone() }
                    };
                    self.record_route(machine, status);
                    return Ok(RoutedValue {
                        value,
                        route: Route::Relay { via: relay },
                    });
                }
                Err(err) => {
                    if !is_unsent(&err) {
                        if in_unsynced_backoff || learned_auth_err.is_some() {
                            self.record_route(
                                machine,
                                MachineRouteStatus::NotSyncedYet {
                                    introducer: entry_learned_from.clone(),
                                    via: None,
                                },
                            );
                        }
                        return Err(err);
                    }
                    if is_device_removed_or_unknown(&err) {
                        let relay_is_learned = self.is_learned(&relay);
                        if relay_is_learned {
                            self.mark_unsynced(&relay);
                        }
                        if relay_auth_err.is_none() || !relay_is_learned {
                            relay_auth_err = Some(err);
                        }
                    } else if matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout) {
                        last_relay_err = Some(err);
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        if in_unsynced_backoff {
            match self.tools(machine, None).await {
                Ok(value) => {
                    self.clear_unsynced(machine);
                    self.record_route(machine, MachineRouteStatus::Direct);
                    return Ok(RoutedValue {
                        value,
                        route: Route::Direct,
                    });
                }
                Err(err) => {
                    if is_device_removed_or_unknown(&err) {
                        self.clear_unsynced(machine);
                        return Err(err);
                    }
                    if direct_err.is_none() {
                        direct_err = Some(err);
                    }
                }
            }
        }

        if let Some(auth_err) = relay_auth_err.or(learned_auth_err) {
            self.clear_unsynced(machine);
            return Err(auth_err);
        }

        Err(direct_err.or(last_relay_err).unwrap_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("No configured node can reach {machine:?}."),
            )
            .with_hint("Pair with at least one machine first.")
            .with_machine(machine)
        }))
    }

    /// Calls `tool` on the first answering configured node (starting from `preferred` if given,
    /// then paired nodes, then learned nodes), failing over strictly when `details.sent == false`.
    ///
    /// Intended for fleet-wide queries (`on: "all"`, `on: ["a", "b"]`) and door-level discovery
    /// calls (`fleet.list`, `fleet.ping`, `meta.whoami`, `meta.version`). Returns the name of the
    /// answering node and the tool's JSON result.
    pub async fn call_any_with_id(
        &self,
        call_id: Option<String>,
        preferred: Option<&str>,
        tool: &str,
        arguments: Value,
        on: Option<Value>,
        mut events: impl FnMut(Value),
    ) -> Result<(String, Value), ErrorBody> {
        self.ensure_reaper();
        let args = validate_call_args(arguments, preferred.unwrap_or("node"))?;
        let target = parse_optional_target(on)?;
        let (cancel, _active_guard) = self.register_call_id(call_id)?;

        let all_candidates = self.ordered_candidates(preferred);
        if all_candidates.is_empty() {
            return Err(
                ErrorBody::new(ErrorCode::NotFound, "No configured nodes are available.")
                    .with_hint("Pair with at least one machine first."),
            );
        }

        // Prefer nodes not currently in unsynced backoff, keeping backed-off nodes as a final pass.
        let mut primary = Vec::new();
        let mut deferred = Vec::new();
        for c in all_candidates {
            if self.is_learned(&c) && self.is_in_unsynced_backoff(&c) {
                deferred.push(c);
            } else {
                primary.push(c);
            }
        }
        primary.extend(deferred);

        let mut last_err: Option<ErrorBody> = None;
        let mut auth_err: Option<ErrorBody> = None;

        for candidate in primary {
            match self
                .call_with_cancel(
                    &candidate,
                    tool,
                    args.clone(),
                    target.clone(),
                    cancel.clone(),
                    &mut events,
                )
                .await
            {
                Ok(value) => {
                    self.clear_unsynced(&candidate);
                    self.record_route(&candidate, MachineRouteStatus::Direct);
                    return Ok((candidate, value));
                }
                Err(err) => {
                    if !is_unsent(&err) {
                        return Err(err);
                    }
                    if is_device_removed_or_unknown(&err) {
                        let is_learned = self.is_learned(&candidate);
                        if is_learned {
                            self.mark_unsynced(&candidate);
                        }
                        if auth_err.is_none() || !is_learned {
                            auth_err = Some(err);
                        }
                        continue;
                    }
                    if matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout) {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        if let Some(err) = auth_err {
            return Err(err);
        }
        Err(last_err.unwrap_or_else(|| {
            ErrorBody::new(
                ErrorCode::Unreachable,
                "Could not reach any configured node.",
            )
        }))
    }

    /// Lists tools through the first answering configured node, delegating single-machine `on`
    /// targets to [`Link::tools_machine`]. Returns the answering node name and `ToolsResult` JSON.
    pub async fn tools_any(
        &self,
        preferred: Option<&str>,
        on: Option<Value>,
    ) -> Result<(String, Value), ErrorBody> {
        let parsed = parse_optional_target(on.clone())?;
        if let Some(Target::One(machine)) = parsed {
            let routed = self.tools_machine(&machine).await?;
            let answering = match routed.route {
                Route::Direct => machine,
                Route::Relay { via } => via,
            };
            return Ok((answering, routed.value));
        }

        let all_candidates = self.ordered_candidates(preferred);
        if all_candidates.is_empty() {
            return Err(
                ErrorBody::new(ErrorCode::NotFound, "No configured nodes are available.")
                    .with_hint("Pair with at least one machine first."),
            );
        }

        let mut primary = Vec::new();
        let mut deferred = Vec::new();
        for c in all_candidates {
            if self.is_learned(&c) && self.is_in_unsynced_backoff(&c) {
                deferred.push(c);
            } else {
                primary.push(c);
            }
        }
        primary.extend(deferred);

        let mut last_err: Option<ErrorBody> = None;
        let mut auth_err: Option<ErrorBody> = None;

        for candidate in primary {
            match self.tools(&candidate, on.clone()).await {
                Ok(value) => {
                    self.clear_unsynced(&candidate);
                    self.record_route(&candidate, MachineRouteStatus::Direct);
                    return Ok((candidate, value));
                }
                Err(err) => {
                    if !is_unsent(&err) {
                        return Err(err);
                    }
                    if is_device_removed_or_unknown(&err) {
                        let is_learned = self.is_learned(&candidate);
                        if is_learned {
                            self.mark_unsynced(&candidate);
                        }
                        if auth_err.is_none() || !is_learned {
                            auth_err = Some(err);
                        }
                        continue;
                    }
                    if matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout) {
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        if let Some(err) = auth_err {
            return Err(err);
        }
        Err(last_err.unwrap_or_else(|| {
            ErrorBody::new(
                ErrorCode::Unreachable,
                "Could not reach any configured node.",
            )
        }))
    }

    /// Returns the observed runtime routing status for every configured node and every machine
    /// reached via relay.
    pub fn route_status(&self) -> HashMap<String, MachineRouteStatus> {
        let now = Instant::now();
        let cfg = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let unsynced: HashMap<String, Instant> = self
            .inner
            .unsynced_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|(_, exp)| **exp > now)
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let observed = self
            .inner
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        let mut out = HashMap::new();
        for (machine, status) in &observed {
            out.insert(machine.clone(), status.clone());
        }
        for entry in &cfg.nodes {
            if let Some(pending) = &entry.identity_changed {
                out.insert(
                    entry.node.clone(),
                    MachineRouteStatus::IdentityChanged {
                        reported_by: pending.reported_by.clone(),
                    },
                );
            } else if unsynced.contains_key(&entry.node) {
                let via = match observed.get(&entry.node) {
                    Some(MachineRouteStatus::NotSyncedYet { via, .. }) => via.clone(),
                    Some(MachineRouteStatus::Relay { via }) => Some(via.clone()),
                    _ => None,
                };
                out.insert(
                    entry.node.clone(),
                    MachineRouteStatus::NotSyncedYet {
                        introducer: entry.learned_from.clone(),
                        via,
                    },
                );
            } else {
                out.entry(entry.node.clone())
                    .or_insert(MachineRouteStatus::Direct);
            }
        }
        out
    }

    async fn call_machine_inner(
        &self,
        call_id: Option<String>,
        machine: &str,
        tool: &str,
        arguments: Value,
        mut events: impl FnMut(Value),
    ) -> Result<RoutedValue, ErrorBody> {
        self.ensure_reaper();
        let args = validate_call_args(arguments, machine)?;
        let (cancel, _active_guard) = self.register_call_id(call_id)?;

        let entry = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .node(machine)
            .cloned();

        // A local-only desktop pipe (e.g. `local`) cannot be relayed through remote SSH peers.
        if let Some(ref e) = entry
            && e.local_program.is_some()
            && e.target.is_none()
        {
            let value = self
                .call_with_cancel(machine, tool, args, None, cancel, &mut events)
                .await?;
            self.record_route(machine, MachineRouteStatus::Direct);
            return Ok(RoutedValue {
                value,
                route: Route::Direct,
            });
        }

        let entry_is_learned = entry
            .as_ref()
            .is_some_and(|e| e.origin == NodeOrigin::Learned);
        let entry_learned_from = entry.as_ref().and_then(|e| e.learned_from.clone());
        let in_unsynced_backoff = entry_is_learned && self.is_in_unsynced_backoff(machine);

        let mut direct_err: Option<ErrorBody> = None;
        let mut learned_auth_err: Option<ErrorBody> = None;

        if entry.is_some() && !in_unsynced_backoff {
            match self
                .call_with_cancel(
                    machine,
                    tool,
                    args.clone(),
                    None,
                    cancel.clone(),
                    &mut events,
                )
                .await
            {
                Ok(value) => {
                    self.clear_unsynced(machine);
                    self.record_route(machine, MachineRouteStatus::Direct);
                    return Ok(RoutedValue {
                        value,
                        route: Route::Direct,
                    });
                }
                Err(err) => {
                    if !is_unsent(&err) {
                        return Err(err);
                    }
                    if is_device_removed_or_unknown(&err) {
                        if entry_is_learned {
                            self.mark_unsynced(machine);
                            learned_auth_err = Some(err);
                        } else {
                            return Err(err);
                        }
                    } else if matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout) {
                        direct_err = Some(err);
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        let relays = self.relay_candidates(machine, entry_learned_from.as_deref());
        let mut last_relay_err: Option<ErrorBody> = None;
        let mut relay_auth_err: Option<ErrorBody> = None;

        for relay in relays {
            match self
                .call_with_cancel(
                    &relay,
                    tool,
                    args.clone(),
                    Some(Target::One(machine.to_owned())),
                    cancel.clone(),
                    &mut events,
                )
                .await
            {
                Ok(value) => {
                    let status = if in_unsynced_backoff || learned_auth_err.is_some() {
                        MachineRouteStatus::NotSyncedYet {
                            introducer: entry_learned_from.clone(),
                            via: Some(relay.clone()),
                        }
                    } else {
                        MachineRouteStatus::Relay { via: relay.clone() }
                    };
                    self.record_route(machine, status);
                    return Ok(RoutedValue {
                        value,
                        route: Route::Relay { via: relay },
                    });
                }
                Err(err) => {
                    if !is_unsent(&err) {
                        if in_unsynced_backoff || learned_auth_err.is_some() {
                            self.record_route(
                                machine,
                                MachineRouteStatus::NotSyncedYet {
                                    introducer: entry_learned_from.clone(),
                                    via: None,
                                },
                            );
                        }
                        return Err(err);
                    }
                    if is_device_removed_or_unknown(&err) {
                        let relay_is_learned = self.is_learned(&relay);
                        if relay_is_learned {
                            self.mark_unsynced(&relay);
                        }
                        if relay_auth_err.is_none() || !relay_is_learned {
                            relay_auth_err = Some(err);
                        }
                    } else if matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout) {
                        last_relay_err = Some(err);
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        // If we skipped direct because of a previous unsynced backoff and no relay could be
        // reached, probe direct once so we either recover if the node has since synced or report
        // its refusal honestly.
        if in_unsynced_backoff {
            match self
                .call_with_cancel(machine, tool, args, None, cancel, &mut events)
                .await
            {
                Ok(value) => {
                    self.clear_unsynced(machine);
                    self.record_route(machine, MachineRouteStatus::Direct);
                    return Ok(RoutedValue {
                        value,
                        route: Route::Direct,
                    });
                }
                Err(err) => {
                    if is_device_removed_or_unknown(&err) {
                        self.clear_unsynced(machine);
                        return Err(err);
                    }
                    if direct_err.is_none() {
                        direct_err = Some(err);
                    }
                }
            }
        }

        if let Some(auth_err) = relay_auth_err.or(learned_auth_err) {
            self.clear_unsynced(machine);
            return Err(auth_err);
        }

        Err(direct_err.or(last_relay_err).unwrap_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("No configured node can reach {machine:?}."),
            )
            .with_hint("Pair with at least one machine first.")
            .with_machine(machine)
        }))
    }

    fn relay_candidates(&self, exclude: &str, preferred_introducer: Option<&str>) -> Vec<String> {
        // Only directly paired nodes may be preferred as relays.
        let paired_preferred = preferred_introducer.filter(|name| self.is_paired(name));
        let ordered = self.ordered_candidates(paired_preferred);
        let mut primary = Vec::new();
        let mut deferred = Vec::new();
        for node in ordered {
            if node == exclude {
                continue;
            }
            if self.is_learned(&node) && self.is_in_unsynced_backoff(&node) {
                deferred.push(node);
            } else {
                primary.push(node);
            }
        }
        primary.extend(deferred);
        primary
    }

    fn is_in_unsynced_backoff(&self, node: &str) -> bool {
        let now = Instant::now();
        let mut map = self
            .inner
            .unsynced_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match map.get(node) {
            Some(&exp) if exp > now => true,
            Some(_) => {
                map.remove(node);
                false
            }
            None => false,
        }
    }

    fn mark_unsynced(&self, node: &str) {
        if self.inner.options.unsynced_backoff.is_zero() {
            return;
        }
        self.inner
            .unsynced_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                node.to_owned(),
                Instant::now() + self.inner.options.unsynced_backoff,
            );
    }

    fn clear_unsynced(&self, node: &str) {
        self.inner
            .unsynced_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(node);
    }

    fn record_route(&self, machine: &str, status: MachineRouteStatus) {
        self.inner
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(machine.to_owned(), status);
    }

    fn clear_route_state(&self, node: &str) {
        self.clear_unsynced(node);
        self.inner
            .routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(node);
    }

    fn register_call_id(
        &self,
        call_id: Option<String>,
    ) -> Result<(CancellationToken, Option<ActiveCallGuard>), ErrorBody> {
        let cancel = CancellationToken::new();
        let guard = if let Some(id) = call_id {
            let mut active = self
                .inner
                .active_calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if active.contains_key(&id) {
                return Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!("A call with id {id:?} is already running."),
                )
                .with_hint("Choose a unique call id for each concurrent call."));
            }
            active.insert(id.clone(), cancel.clone());
            drop(active);
            Some(ActiveCallGuard {
                active_calls: Arc::clone(&self.inner),
                id,
            })
        } else {
            None
        };
        Ok((cancel, guard))
    }

    /// Opens a streaming byte-range reader for `path` on `machine` via `legion stream`'s `get`
    /// operation over the session this [`Link`] holds for `machine`.
    ///
    /// Follows the same routing and identity rules as calls: `machine` must be configured on this
    /// [`Link`], its pinned host keys and `hello.node` identity are verified, and the operation
    /// never falls back to another machine. Multiple content streams may be open concurrently on
    /// one SSH connection; cancelling or dropping the returned [`ContentStream`] closes its
    /// channel immediately without affecting the parent session.
    pub async fn open_content(
        &self,
        machine: &str,
        path: &str,
        offset: u64,
        length: Option<u64>,
    ) -> Result<ContentStream, ErrorBody> {
        self.open_content_with_cancel(machine, path, offset, length, CancellationToken::new())
            .await
    }

    /// Like [`Link::open_content`], aborting the open handshake if `cancel` fires.
    pub async fn open_content_with_cancel(
        &self,
        machine: &str,
        path: &str,
        offset: u64,
        length: Option<u64>,
        cancel: CancellationToken,
    ) -> Result<ContentStream, ErrorBody> {
        self.open_content_inner(
            machine,
            path,
            offset,
            length,
            cancel,
            Some(Arc::clone(&self.inner.parked_streams)),
        )
        .await
    }

    /// Like [`Link::open_content_with_cancel`], for background transfer-queue downloads.
    ///
    /// The returned stream still parks its SSH channel and trims buffers when idle, and
    /// transparently reopens on the next read, but it is exempt from the per-link parked-stream
    /// eviction cap so paused viewer streams cannot evict an in-progress download.
    pub async fn open_content_for_transfer_with_cancel(
        &self,
        machine: &str,
        path: &str,
        offset: u64,
        length: Option<u64>,
        cancel: CancellationToken,
    ) -> Result<ContentStream, ErrorBody> {
        self.open_content_inner(machine, path, offset, length, cancel, None)
            .await
    }

    async fn open_content_inner(
        &self,
        machine: &str,
        path: &str,
        offset: u64,
        length: Option<u64>,
        cancel: CancellationToken,
        park_pool: Option<Arc<ParkedStreamPool>>,
    ) -> Result<ContentStream, ErrorBody> {
        self.ensure_reaper();
        if path.is_empty() {
            return Err(
                ErrorBody::new(ErrorCode::InvalidArgs, "Content path cannot be empty.")
                    .with_hint("Pass an absolute or `~`-prefixed file path.")
                    .with_machine(machine),
            );
        }

        let (mut raw, opener, live, flight) = self
            .open_stream_with_recovery(machine, cancel.clone())
            .await
            .map_err(|err| match err {
                StreamOpenError::DeadSession(body)
                | StreamOpenError::ChannelRefused(body)
                | StreamOpenError::Failed(body) => body.with_machine(machine),
            })?;

        // Attach the in-flight session guard to the raw channel's lifecycle guard rather than to
        // `ContentStream` itself so parking an idle stream releases both its SSH channel slot and
        // its session flight pin; reopening acquires a fresh session if the old one closed.
        let prev_guard = raw.guard.take();
        raw.guard = Some(Box::new((prev_guard, flight)));

        let reopener: Arc<dyn crate::content::StreamOpener> = Arc::new(LinkStreamOpener {
            link: self.clone(),
            machine: machine.to_owned(),
            window_policy: opener.window_policy(),
        });

        let cap_session = live.clone();
        let record_caps = move |caps: Vec<String>| {
            cap_session.record_stream_capabilities(caps);
        };

        handshake_get(
            raw,
            Some(reopener),
            machine,
            path,
            offset,
            length,
            live.config_snapshot.principal.as_deref(),
            self.inner.options.ssh_timeouts.handshake,
            cancel,
            ContentStreamOptions {
                park_pool,
                ..ContentStreamOptions::default()
            },
            Some(&record_caps),
        )
        .await
    }

    /// Returns the stream capabilities observed on the live session to `machine`, or `None` if no
    /// `legion stream` greeting has been received on the current session yet.
    pub fn stream_capabilities(&self, machine: &str) -> Option<Vec<String>> {
        let slot = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(machine)
            .cloned()?;
        let guard = slot
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let live = guard.as_ref()?;
        if !live.session.is_alive() {
            return None;
        }
        live.stream_capabilities()
    }

    /// Reports whether the live session to `machine` advertised `capability` on `legion stream`:
    /// `Some(true)` when supported, `Some(false)` when a stream greeting was observed without it
    /// (an older node), or `None` when no stream has been opened on the active session yet.
    pub fn stream_supports(&self, machine: &str, capability: &str) -> Option<bool> {
        self.stream_capabilities(machine)
            .map(|caps| caps.iter().any(|c| c == capability))
    }

    /// Opens a resumable bulk-pipe upload (`legion stream`'s `put`) to `request.path` on `machine`
    /// over the session this [`Link`] holds for `machine`.
    ///
    /// Follows the same routing and identity rules as [`Link::open_content`]: `machine` must be
    /// configured on this [`Link`], its pinned host keys and `hello.node` identity are verified,
    /// and uploads never fall back to another machine.
    pub async fn open_put(
        &self,
        machine: &str,
        request: PutRequest,
    ) -> Result<PutStream, ErrorBody> {
        self.open_put_with_cancel(machine, request, CancellationToken::new())
            .await
    }

    /// Like [`Link::open_put`], aborting the open handshake or in-progress upload if `cancel`
    /// fires.
    pub async fn open_put_with_cancel(
        &self,
        machine: &str,
        request: PutRequest,
        cancel: CancellationToken,
    ) -> Result<PutStream, ErrorBody> {
        self.ensure_reaper();
        if request.path.is_empty() {
            return Err(
                ErrorBody::new(ErrorCode::InvalidArgs, "Upload path cannot be empty.")
                    .with_hint("Pass an absolute or `~`-prefixed destination path.")
                    .with_machine(machine),
            );
        }
        validate_blake3_hash(&request.hash, machine)?;

        let (raw, _opener, live, flight) = self
            .open_stream_with_recovery(machine, cancel.clone())
            .await
            .map_err(|err| match err {
                StreamOpenError::DeadSession(body)
                | StreamOpenError::ChannelRefused(body)
                | StreamOpenError::Failed(body) => body.with_machine(machine),
            })?;

        let cap_session = live.clone();
        let record_caps = move |caps: Vec<String>| {
            cap_session.record_stream_capabilities(caps);
        };

        let mut stream = handshake_put(
            raw,
            machine,
            &request,
            live.config_snapshot.principal.as_deref(),
            self.inner.options.ssh_timeouts.handshake,
            cancel,
            Some(&record_caps),
        )
        .await?;
        if stream.no_replace_confirmed() {
            live.record_stream_capabilities(vec![CAP_PUT_NO_REPLACE.to_owned()]);
        }
        stream.attach_flight(Box::new(flight));
        Ok(stream)
    }

    fn try_claim_channel_refused_reconnect(&self, machine: &str) -> bool {
        let cooldown = self.inner.options.channel_refused_reconnect_cooldown;
        let now = Instant::now();
        let mut map = self
            .inner
            .channel_refused_reconnect_at
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(last) = map.get(machine)
            && now.saturating_duration_since(*last) < cooldown
        {
            return false;
        }
        map.insert(machine.to_owned(), now);
        true
    }

    async fn open_stream_with_recovery(
        &self,
        machine: &str,
        cancel: CancellationToken,
    ) -> Result<
        (
            crate::content::RawStream,
            Arc<dyn crate::content::StreamOpener>,
            LiveSession,
            InFlightGuard,
        ),
        StreamOpenError,
    > {
        let live = self
            .get_or_open(machine, cancel.clone())
            .await
            .map_err(StreamOpenError::Failed)?;
        let flight = live.acquire_flight();
        match self.open_stream_on(&live, machine, cancel.clone()).await {
            Ok((raw, opener)) => Ok((raw, opener, live, flight)),
            Err(StreamOpenError::DeadSession(_)) => {
                // The cached session died before any stream bytes were sent; reopen once.
                drop(flight);
                self.invalidate_if_same(machine, &live.session);
                let fresh = self
                    .get_or_open(machine, cancel.clone())
                    .await
                    .map_err(StreamOpenError::Failed)?;
                let flight2 = fresh.acquire_flight();
                let (raw, opener) = self.open_stream_on(&fresh, machine, cancel).await.map_err(
                    |err| match err {
                        StreamOpenError::DeadSession(body) | StreamOpenError::Failed(body) => {
                            self.invalidate_if_same(machine, &fresh.session);
                            StreamOpenError::Failed(body.with_machine(machine))
                        }
                        StreamOpenError::ChannelRefused(body) => {
                            StreamOpenError::ChannelRefused(body.with_machine(machine))
                        }
                    },
                )?;
                Ok((raw, opener, fresh, flight2))
            }
            Err(StreamOpenError::ChannelRefused(body))
                if is_channel_refused_with_free_permits(&body)
                    && self.try_claim_channel_refused_reconnect(machine) =>
            {
                // sshd kept refusing new channels while our own stream permits were free (for
                // example a misbehaving remote ignored stdin EOF and left zombie sshd sessions
                // behind): retire the poisoned session, reconnect once, and retry.
                drop(flight);
                self.invalidate_if_same(machine, &live.session);
                let fresh = self
                    .get_or_open(machine, cancel.clone())
                    .await
                    .map_err(StreamOpenError::Failed)?;
                let flight2 = fresh.acquire_flight();
                let (raw, opener) = self.open_stream_on(&fresh, machine, cancel).await.map_err(
                    |err| match err {
                        StreamOpenError::DeadSession(body) | StreamOpenError::Failed(body) => {
                            self.invalidate_if_same(machine, &fresh.session);
                            StreamOpenError::Failed(body.with_machine(machine))
                        }
                        StreamOpenError::ChannelRefused(body) => {
                            StreamOpenError::ChannelRefused(body.with_machine(machine))
                        }
                    },
                )?;
                Ok((raw, opener, fresh, flight2))
            }
            Err(StreamOpenError::ChannelRefused(body)) => {
                Err(StreamOpenError::ChannelRefused(body.with_machine(machine)))
            }
            Err(StreamOpenError::Failed(body)) => {
                Err(StreamOpenError::Failed(body.with_machine(machine)))
            }
        }
    }

    async fn open_stream_on(
        &self,
        live: &LiveSession,
        machine: &str,
        cancel: CancellationToken,
    ) -> Result<
        (
            crate::content::RawStream,
            Arc<dyn crate::content::StreamOpener>,
        ),
        StreamOpenError,
    > {
        let Some(opener) = live.session.stream_opener() else {
            return Err(StreamOpenError::Failed(
                ErrorBody::new(
                    ErrorCode::Unsupported,
                    format!("The session to {machine} does not support opening content streams."),
                )
                .with_hint("Use an SSH or local node transport.")
                .with_machine(machine),
            ));
        };
        let raw = opener.open_stream(cancel).await?;
        Ok((raw, opener))
    }
}

fn is_channel_refused_with_free_permits(body: &ErrorBody) -> bool {
    body.details
        .as_ref()
        .and_then(|d| d.get("permits_free"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Session-backed [`crate::content::StreamOpener`] used by [`ContentStream`] when reopening a
/// parked or idle-closed channel, transparently acquiring the current (or freshly reconnected)
/// session for `machine`.
struct LinkStreamOpener {
    link: Link,
    machine: String,
    window_policy: Option<crate::content::WindowPolicy>,
}

impl crate::content::StreamOpener for LinkStreamOpener {
    fn window_policy(&self) -> Option<crate::content::WindowPolicy> {
        self.window_policy
    }

    fn open_stream(
        &self,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<crate::content::RawStream, StreamOpenError>> + Send + '_>>
    {
        Box::pin(async move {
            let (mut raw, _, _, flight) = self
                .link
                .open_stream_with_recovery(&self.machine, cancel)
                .await?;
            let prev_guard = raw.guard.take();
            raw.guard = Some(Box::new((prev_guard, flight)));
            Ok(raw)
        })
    }
}

impl Link {
    /// Calls `tool` on `node` with JSON `arguments` and optional `on` (passed through untouched to
    /// the node for forwarding or fan-out), streaming live events as JSON objects to `events`.
    pub async fn call(
        &self,
        node: &str,
        tool: &str,
        arguments: Value,
        on: Option<Value>,
        events: impl FnMut(Value),
    ) -> Result<Value, ErrorBody> {
        self.call_inner(None, node, tool, arguments, on, events)
            .await
    }

    /// Like [`Link::call`], but registers the call under `call_id` so [`Link::cancel`] can cancel
    /// it while it is running.
    pub async fn call_with_id(
        &self,
        call_id: impl Into<String>,
        node: &str,
        tool: &str,
        arguments: Value,
        on: Option<Value>,
        events: impl FnMut(Value),
    ) -> Result<Value, ErrorBody> {
        self.call_inner(Some(call_id.into()), node, tool, arguments, on, events)
            .await
    }

    /// Cancels an active call registered via [`Link::call_with_id`] or
    /// [`Link::call_machine_with_id`], returning `true` if the call was found and signalled.
    pub fn cancel(&self, call_id: &str) -> bool {
        let token = self
            .inner
            .active_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(call_id)
            .cloned();
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    async fn call_inner(
        &self,
        call_id: Option<String>,
        node: &str,
        tool: &str,
        arguments: Value,
        on: Option<Value>,
        mut events: impl FnMut(Value),
    ) -> Result<Value, ErrorBody> {
        self.ensure_reaper();
        let args = validate_call_args(arguments, node)?;
        let target = parse_optional_target(on)?;
        let (cancel, _active_guard) = self.register_call_id(call_id)?;
        self.call_with_cancel(node, tool, args, target, cancel, &mut events)
            .await
    }

    async fn call_with_cancel(
        &self,
        node: &str,
        tool: &str,
        args: Value,
        target: Option<Target>,
        cancel: CancellationToken,
        events: &mut impl FnMut(Value),
    ) -> Result<Value, ErrorBody> {
        let live = self.get_or_open(node, cancel.clone()).await?;
        let _flight = live.acquire_flight();

        let params = CallParams {
            tool: tool.to_owned(),
            args,
            as_principal: live.config_snapshot.principal.clone(),
            on: target,
            hops: 0,
        };

        match self
            .run_call_with_events(&live.session, params.clone(), cancel.clone(), events)
            .await
        {
            Ok(value) => Ok(value),
            Err(CallError::NotWritten(_)) => {
                self.invalidate_if_same(node, &live.session);
                let fresh = self.get_or_open(node, cancel.clone()).await?;
                let _flight2 = fresh.acquire_flight();
                let mut retry_params = params;
                retry_params.as_principal = fresh.config_snapshot.principal.clone();
                self.run_call_with_events(&fresh.session, retry_params, cancel, events)
                    .await
                    .map_err(|err| self.handle_call_error(node, &fresh.session, err))
            }
            Err(err) => Err(self.handle_call_error(node, &live.session, err)),
        }
    }

    async fn run_call_with_events(
        &self,
        session: &Session,
        params: CallParams,
        cancel: CancellationToken,
        events: &mut impl FnMut(Value),
    ) -> Result<Value, CallError> {
        // An unbounded channel is used deliberately here: `EventSink`'s callback runs synchronously
        // on the session's shared reader task (blocking there would stall every multiplexed call on
        // the session), while this loop drains incoming events on every select turn and never drops
        // output silently.
        let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
        let sink = EventSink::with_events(move |event| {
            let _ = tx.send(event);
        });
        let call_fut = session.call(params, cancel, sink);
        tokio::pin!(call_fut);
        let outcome = loop {
            tokio::select! {
                biased;
                res = &mut call_fut => break res,
                Some(ev) = rx.recv() => {
                    events(event_to_value(&ev));
                }
            }
        };
        while let Ok(ev) = rx.try_recv() {
            events(event_to_value(&ev));
        }
        outcome
    }

    /// Closes any open session that has had no active calls for at least `idle_timeout`.
    pub async fn close_idle(&self) {
        close_idle_inner(&self.inner).await;
    }

    /// Closes every open session cleanly.
    pub async fn close(&self) {
        let slots: Vec<Arc<NodeSlot>> = {
            let mut map = self
                .inner
                .slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map.drain().map(|(_, slot)| slot).collect()
        };
        let retired: Vec<LiveSession> = {
            let mut list = self
                .inner
                .retired
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *list)
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
        for live in retired {
            closing.spawn(async move {
                live.session.close().await;
            });
        }
        while closing.join_next().await.is_some() {}
    }

    fn retire_node(&self, node: &str) {
        let slot = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(node)
            .cloned();
        if let Some(slot) = slot {
            let old = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(old) = old {
                self.inner
                    .retired
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(old);
            }
        }
    }

    async fn evict(&self, node: &str) {
        let slot = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(node);
        let mut to_close = Vec::new();
        if let Some(slot) = slot {
            let live = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(live) = live {
                to_close.push(live.session);
            }
        }
        {
            let mut retired = self
                .inner
                .retired
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut keep = Vec::new();
            for live in retired.drain(..) {
                if live.config_snapshot.node == node {
                    to_close.push(live.session);
                } else {
                    keep.push(live);
                }
            }
            *retired = keep;
        }
        let mut closing = JoinSet::new();
        for session in to_close {
            closing.spawn(async move {
                session.close().await;
            });
        }
        while closing.join_next().await.is_some() {}
    }

    fn invalidate_if_same(&self, node: &str, dead: &Arc<Session>) {
        let slot = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(node)
            .cloned();
        if let Some(slot) = slot {
            let mut guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard
                .as_ref()
                .is_some_and(|live| Arc::ptr_eq(&live.session, dead))
            {
                guard.take();
            }
        }
    }

    fn handle_call_error(&self, node: &str, session: &Arc<Session>, err: CallError) -> ErrorBody {
        match err {
            CallError::NotWritten(body) => {
                self.invalidate_if_same(node, session);
                mark_not_sent(body)
            }
            CallError::LostInFlight(body) => {
                self.invalidate_if_same(node, session);
                crate::strip_remote_sent(body)
            }
            CallError::Remote(body) => crate::strip_remote_sent(body),
        }
    }

    async fn get_or_open(
        &self,
        node: &str,
        cancel: CancellationToken,
    ) -> Result<LiveSession, ErrorBody> {
        let (entry, device_key) = {
            let cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = cfg.node(node).cloned().ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("No node named {node:?} is configured."),
                )
                .with_hint("Pair the node first or check the configured node names.")
            })?;
            (entry, cfg.device_key.clone())
        };

        let slot = self.slot(node);
        {
            let guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = guard.as_ref()
                && live.session.is_alive()
                && live.config_snapshot.same_transport(&entry)
            {
                live.touch();
                return Ok(live.clone());
            }
        }

        let _open_guard = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("Connecting to {node} was cancelled."),
                )
                .with_machine(node));
            }
            guard = slot.open_lock.lock() => guard,
        };

        let stale = {
            let mut guard = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(live) = guard.as_ref()
                && live.session.is_alive()
                && live.config_snapshot.same_transport(&entry)
            {
                live.touch();
                return Ok(live.clone());
            }
            guard.take()
        };
        if let Some(old) = stale {
            // Do not close the displaced session while callers may still hold an `Arc<Session>`
            // for in-flight calls; park it so the reaper closes it once idle, or `Drop` cleans up.
            self.inner
                .retired
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(old);
        }

        let session = (self.inner.opener)(entry.clone(), device_key, cancel)
            .await
            .map_err(|err| mark_not_sent(classify_open_error(node, err)))?;
        if entry.target.is_some() {
            let actual = session.info().hello.node.clone();
            if actual != entry.node {
                session.close().await;
                return Err(mark_not_sent(
                    ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Connected to {actual:?} when expecting {:?}.",
                            entry.node
                        ),
                    )
                    .with_hint(
                        "Check that the endpoint points to the intended machine and has not been redirected.",
                    )
                    .with_machine(&entry.node)
                    .with_details(json!({
                        "reason": crate::REASON_NODE_MISMATCH,
                        "expected": entry.node,
                        "actual": actual,
                    })),
                ));
            }
        }
        let live = LiveSession {
            session: Arc::new(session),
            config_snapshot: entry,
            last_used: Arc::new(Mutex::new(Instant::now())),
            in_flight: Arc::new(AtomicUsize::new(0)),
            stream_capabilities: Arc::new(Mutex::new(None)),
        };
        *slot
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(live.clone());
        Ok(live)
    }

    fn slot(&self, node: &str) -> Arc<NodeSlot> {
        let mut map = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(map.entry(node.to_owned()).or_insert_with(|| {
            Arc::new(NodeSlot {
                open_lock: tokio::sync::Mutex::new(()),
                current: Mutex::new(None),
            })
        }))
    }

    fn ensure_reaper(&self) {
        if self.inner.options.idle_timeout.is_zero() {
            return;
        }
        if self
            .inner
            .reaper_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        let interval = (self.inner.options.idle_timeout / 2)
            .max(Duration::from_millis(25))
            .min(Duration::from_secs(30));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                close_idle_inner(&inner).await;
            }
        });
    }
}

async fn close_idle_inner(inner: &LinkInner) {
    let timeout = inner.options.idle_timeout;
    let slots: Vec<Arc<NodeSlot>> = inner
        .slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .cloned()
        .collect();
    let mut to_close = Vec::new();
    for slot in slots {
        let mut guard = slot
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(live) = guard.as_ref()
            && (!live.session.is_alive() || live.is_idle_for(timeout))
            && let Some(stale) = guard.take()
        {
            to_close.push(stale.session);
        }
    }
    {
        let mut retired = inner
            .retired
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut keep = Vec::new();
        for live in retired.drain(..) {
            if Arc::strong_count(&live.session) == 1 && live.in_flight.load(Ordering::Acquire) == 0
            {
                to_close.push(live.session);
            } else {
                keep.push(live);
            }
        }
        *retired = keep;
    }
    for session in to_close {
        session.close().await;
    }
}

async fn default_open_session(
    entry: NodeLink,
    device_key: Option<String>,
    options: LinkOptions,
    cancel: CancellationToken,
) -> Result<Session, ErrorBody> {
    if let Some(_target) = &entry.target {
        #[cfg(feature = "ssh")]
        {
            let key_text = device_key.as_deref().ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("No device key is configured for SSH node {:?}.", entry.node),
                )
                .with_hint("Generate a device key and store it in LinkConfig.device_key.")
                .with_machine(&entry.node)
            })?;
            let key = crate::key::DeviceKey::from_openssh(key_text)?;
            let policy = crate::ssh::ConnectPolicy {
                restrict_resolved_addrs: entry.origin == NodeOrigin::Learned,
                allow_loopback: options.allow_loopback_endpoints,
            };
            return crate::ssh::open_with_policy(
                &entry.node,
                _target,
                &key,
                crate::command::Subcommand::Rpc,
                options.ssh_timeouts,
                policy,
                cancel,
            )
            .await;
        }
        #[cfg(not(feature = "ssh"))]
        {
            let _ = (device_key, options, cancel);
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "Cannot open SSH link to {:?}: legion-link was built without the `ssh` feature.",
                    entry.node
                ),
            )
            .with_hint("Rebuild with `--features ssh`.")
            .with_machine(&entry.node));
        }
    }

    if let Some(_program) = &entry.local_program {
        #[cfg(feature = "local")]
        {
            return crate::local::open_local_session(
                _program,
                &entry.node,
                options.ssh_timeouts.handshake,
                cancel,
            )
            .await;
        }
        #[cfg(not(feature = "local"))]
        {
            let _ = (device_key, options, cancel);
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "Cannot open local pipe to {:?}: legion-link was built without the `local` feature.",
                    entry.node
                ),
            )
            .with_hint("Rebuild with `--features local` on desktop platforms.")
            .with_machine(&entry.node));
        }
    }

    Err(ErrorBody::new(
        ErrorCode::InvalidArgs,
        format!(
            "Node {:?} has neither an SSH target nor a local program configured.",
            entry.node
        ),
    )
    .with_hint("Set `target` for a paired SSH node or `local_program` for a local node.")
    .with_machine(&entry.node))
}

/// Converts a JSON `on` value into an optional [`Target`], accepting the same forms as the HTTP
/// door (`null`, `"tower"`, `"tower, pi"`, `"all"`, or `["tower", "pi"]`).
pub fn parse_optional_target(on: Option<Value>) -> Result<Option<Target>, ErrorBody> {
    let Some(value) = on else {
        return Ok(None);
    };
    let parsed = match value {
        Value::Null => Ok(None),
        Value::String(text) => Target::parse(&text).map(Some),
        other => serde_json::from_value(other)
            .map(Some)
            .map_err(|err| err.to_string()),
    };
    parsed.map_err(|reason| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("on: {reason}."))
            .with_hint("Give on as a machine name, comma-separated names, a list of names, or \"all\", or omit it.")
    })
}

fn validate_call_args(arguments: Value, machine: &str) -> Result<Value, ErrorBody> {
    match arguments {
        Value::Null => Ok(json!({})),
        Value::Object(_) => Ok(arguments),
        _ => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Tool arguments must be a JSON object.",
        )
        .with_hint("Pass a JSON object such as `{}`.")
        .with_machine(machine)),
    }
}

fn is_unsent(err: &ErrorBody) -> bool {
    err.details
        .as_ref()
        .and_then(|d| d.get(crate::DETAIL_SENT))
        .and_then(Value::as_bool)
        == Some(false)
}

fn is_unsent_connect_error(err: &ErrorBody) -> bool {
    is_unsent(err) && matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout)
}

fn is_device_removed_or_unknown(err: &ErrorBody) -> bool {
    err.code == ErrorCode::Denied
        && err
            .details
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            == Some(crate::REASON_DEVICE_REMOVED_OR_UNKNOWN)
}

#[derive(Deserialize)]
struct EndpointAnswer {
    node: String,
    hosts: Vec<String>,
    port: u16,
    user: String,
    host_keys: Vec<String>,
}

fn parse_and_validate_endpoint(
    expected_machine: &str,
    value: &Value,
    allow_loopback: bool,
) -> Result<SshTarget, ()> {
    let ep: EndpointAnswer = serde_json::from_value(value.clone()).map_err(|_| ())?;
    if ep.node != expected_machine {
        return Err(());
    }
    if ep.node.len() > MAX_ENDPOINT_FIELD_LEN || ep.user.len() > MAX_ENDPOINT_FIELD_LEN {
        return Err(());
    }
    crate::invite::validate_token_field("node", &ep.node).map_err(|_| ())?;
    crate::invite::validate_token_field("user", &ep.user).map_err(|_| ())?;
    if ep.port == 0 || ep.hosts.is_empty() || ep.hosts.len() > MAX_ENDPOINT_HOSTS {
        return Err(());
    }
    for host in &ep.hosts {
        if host.len() > MAX_ENDPOINT_FIELD_LEN {
            return Err(());
        }
        crate::invite::validate_token_field("hosts", host).map_err(|_| ())?;
        if !is_allowed_endpoint_host(host, allow_loopback) {
            return Err(());
        }
    }
    if ep.host_keys.is_empty() || ep.host_keys.len() > MAX_ENDPOINT_HOST_KEYS {
        return Err(());
    }
    for (idx, line) in ep.host_keys.iter().enumerate() {
        if line.len() > MAX_ENDPOINT_HOST_KEY_LEN {
            return Err(());
        }
        crate::invite::validate_openssh_public_key_line(idx, line).map_err(|_| ())?;
    }
    Ok(SshTarget {
        hosts: ep.hosts,
        port: ep.port,
        user: ep.user,
        host_keys: ep.host_keys,
    })
}

fn is_allowed_ipv4(ip: std::net::Ipv4Addr, allow_loopback: bool) -> bool {
    if ip.is_unspecified() || ip.is_link_local() || ip.is_multicast() || ip.is_broadcast() {
        return false;
    }
    if ip.is_loopback() && !allow_loopback {
        return false;
    }
    true
}

pub(crate) fn is_allowed_ip(ip: std::net::IpAddr, allow_loopback: bool) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_allowed_ipv4(v4, allow_loopback),
        std::net::IpAddr::V6(v6) => {
            if v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            if v6.is_loopback() {
                return allow_loopback;
            }
            let segs = v6.segments();
            if (segs[0] & 0xffc0) == 0xfe80 {
                return false;
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_allowed_ipv4(v4, allow_loopback);
            }
            // IPv4-compatible IPv6 (`::a.b.c.d`, deprecated in RFC 4291 but still parsed by
            // resolvers): treat as its embedded IPv4 address.
            if segs[0..6] == [0, 0, 0, 0, 0, 0] {
                let [a, b] = segs[6].to_be_bytes();
                let [c, d] = segs[7].to_be_bytes();
                return is_allowed_ipv4(std::net::Ipv4Addr::new(a, b, c, d), allow_loopback);
            }
            true
        }
    }
}

/// Returns `true` when `label` (already lowercased) would be parsed as an integer by the
/// POSIX/BSD `inet_aton` IPv4 parser family (decimal, octal with leading `0`, or `0x` hex).
fn is_numeric_ipv4_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if label.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if let Some(hex) = label.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    false
}

fn is_allowed_endpoint_host(raw: &str, allow_loopback: bool) -> bool {
    let stripped = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw);
    // Scoped IPv6 literals (e.g. `fe80::1%eth0`) are link-local and rejected.
    if stripped.contains('%') {
        return false;
    }
    if let Ok(ip) = stripped.parse::<std::net::IpAddr>() {
        return is_allowed_ip(ip, allow_loopback);
    }
    let lowered = stripped.trim_end_matches('.').to_ascii_lowercase();
    if lowered.is_empty() {
        return false;
    }
    let labels: Vec<&str> = lowered.split('.').collect();
    if labels.iter().any(|l| l.is_empty()) {
        return false;
    }
    // Reject any non-canonical numeric IPv4 form (`2130706433`, `0x7f.1`, `0x7f000001`, `127.1`,
    // `0177.0.0.1`, `0`, or any host ending in a numeric/hex label).
    if labels.iter().all(|l| is_numeric_ipv4_label(l))
        || labels.last().is_some_and(|l| is_numeric_ipv4_label(l))
    {
        return false;
    }
    if !allow_loopback && (lowered == "localhost" || lowered.ends_with(".localhost")) {
        return false;
    }
    true
}

/// Canonicalizes a list of OpenSSH public key lines to a sorted set of `"<algo> <base64>"` pairs
/// so pin comparisons ignore comments, ordering, and duplicates.
fn canonical_host_keys(lines: &[String]) -> BTreeSet<String> {
    lines
        .iter()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let algo = parts.next()?;
            let blob = parts.next()?;
            Some(format!("{algo} {blob}"))
        })
        .collect()
}

/// Returns `true` when every pinned host key in `pinned` is still presented in `reported`
/// (so no unpinned key is required to verify the server).
fn pins_satisfied(pinned: &BTreeSet<String>, reported: &BTreeSet<String>) -> bool {
    !pinned.is_empty() && pinned.is_subset(reported)
}

fn same_endpoint_target(a: &SshTarget, b: &SshTarget) -> bool {
    a.hosts == b.hosts
        && a.port == b.port
        && a.user == b.user
        && canonical_host_keys(&a.host_keys) == canonical_host_keys(&b.host_keys)
}

fn record_pending_identity_change(
    existing: &mut NodeLink,
    new_target: SshTarget,
    introducer: &str,
) {
    match existing.identity_changed.as_mut() {
        None => {
            existing.identity_changed = Some(PendingIdentityChange {
                target: new_target,
                reported_by: introducer.to_owned(),
                conflicts: Vec::new(),
            });
        }
        Some(pending) => {
            if same_endpoint_target(&pending.target, &new_target) {
                return;
            }
            let duplicate_conflict = pending.conflicts.iter().any(|c| {
                c.reported_by == introducer && same_endpoint_target(&c.target, &new_target)
            });
            if !duplicate_conflict && pending.conflicts.len() < 8 {
                pending.conflicts.push(ConflictingIdentityReport {
                    target: new_target,
                    reported_by: introducer.to_owned(),
                });
            }
        }
    }
}

/// Computes the OpenSSH SHA-256 fingerprint (`SHA256:...`) for a single-line OpenSSH public key.
///
/// Works identically with or without the `ssh` feature.
pub fn public_key_fingerprint(openssh_line: &str) -> Option<String> {
    let mut parts = openssh_line.split_whitespace();
    let algo = parts.next()?;
    let blob_b64 = parts.next()?;
    let blob = STANDARD.decode(blob_b64).ok()?;
    if blob.len() < 4 {
        return None;
    }
    let alg_len = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    if blob.len() <= 4 + alg_len || &blob[4..4 + alg_len] != algo.as_bytes() {
        return None;
    }
    let digest = sha256(&blob);
    Some(format!("SHA256:{}", STANDARD_NO_PAD.encode(digest)))
}

/// Minimal FIPS 180-4 SHA-256 implementation so OpenSSH key fingerprints work without adding a
/// feature-unconditional crypto dependency to `legion-link`.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

fn mark_not_sent(mut err: ErrorBody) -> ErrorBody {
    let mut details = err.details.take().unwrap_or_else(|| json!({}));
    if let Some(obj) = details.as_object_mut() {
        obj.insert(crate::DETAIL_SENT.to_owned(), Value::Bool(false));
    } else {
        details = json!({
            crate::DETAIL_SENT: false,
            "detail": details,
        });
    }
    err.details = Some(details);
    err
}

fn classify_open_error(node: &str, mut err: ErrorBody) -> ErrorBody {
    let is_auth_failed = err.code == ErrorCode::Denied
        && err
            .details
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            == Some(crate::REASON_AUTH_FAILED);
    if !is_auth_failed {
        return err;
    }
    let mut details = err.details.take().unwrap_or_else(|| json!({}));
    if let Some(obj) = details.as_object_mut() {
        obj.insert(
            "reason".to_owned(),
            json!(crate::REASON_DEVICE_REMOVED_OR_UNKNOWN),
        );
    } else {
        details = json!({ "reason": crate::REASON_DEVICE_REMOVED_OR_UNKNOWN });
    }
    let machine = err.machine.unwrap_or_else(|| node.to_owned());
    ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "Device key was rejected by {node}; this device may have been removed or is not paired on the node."
        ),
    )
    .with_hint("Pair this device again with a fresh invite from `legion pair` on the node.")
    .with_machine(machine)
    .with_details(details)
}

fn event_to_value(event: &Event) -> Value {
    match &event.machine {
        Some(machine) => json!({
            "kind": event.kind.as_str(),
            "data": event.data,
            "machine": machine,
        }),
        None => json!({
            "kind": event.kind.as_str(),
            "data": event.data,
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::atomic::AtomicUsize;

    use legion_proto::{
        CancelParams, Effect, EventKind, EventParams, HelloResult, Notification, PROTOCOL, Request,
        Response, ToolSpec, ToolsResult, method,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Notify;

    use super::*;

    fn test_config(node: &str) -> LinkConfig {
        LinkConfig {
            device_key: None,
            nodes: vec![NodeLink::local(node, "legion")],
            ..LinkConfig::default()
        }
    }

    fn spawn_mock_node(
        node: String,
        drop_after_first_call: bool,
        block_until_cancel: Option<Arc<Notify>>,
    ) -> (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);

        tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            let mut handled_calls = 0usize;
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Request = serde_json::from_str(&line).unwrap();
                match req.method.as_str() {
                    method::HELLO => {
                        let resp = Response::ok(
                            req.id,
                            serde_json::to_value(HelloResult {
                                node: node.clone(),
                                node_explicit: None,
                                node_id: None,
                                version: "0.1.0".into(),
                                build: None,
                                protocol: PROTOCOL,
                                os: "linux".into(),
                                arch: "x86_64".into(),
                                systems: vec![],
                                system: None,
                                boot_id: None,
                            })
                            .unwrap(),
                        );
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = server_w.flush().await;
                    }
                    method::TOOLS => {
                        let resp = Response::ok(
                            req.id,
                            serde_json::to_value(ToolsResult {
                                tools: vec![ToolSpec {
                                    name: "sys.info".into(),
                                    description: "Reports system state.".into(),
                                    effect: Effect::Read,
                                    input_schema: json!({"type": "object"}),
                                }],
                            })
                            .unwrap(),
                        );
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = server_w.flush().await;
                    }
                    method::CALL => {
                        handled_calls += 1;
                        if let Some(notify) = &block_until_cancel {
                            let call_id = req.id.clone();
                            notify.notify_one();
                            // Wait for cancel request on the next line.
                            if let Ok(Some(cancel_line)) = lines.next_line().await {
                                let cancel_req: Request =
                                    serde_json::from_str(&cancel_line).unwrap();
                                assert_eq!(cancel_req.method, method::CANCEL);
                                let cp: CancelParams =
                                    serde_json::from_value(cancel_req.params).unwrap();
                                assert_eq!(cp.id, call_id.clone().unwrap());
                                let ack = Response::ok(cancel_req.id, json!({}));
                                let _ = server_w
                                    .write_all(
                                        format!("{}\n", serde_json::to_string(&ack).unwrap())
                                            .as_bytes(),
                                    )
                                    .await;
                                let cancelled = Response::error(
                                    call_id,
                                    legion_proto::RpcError::new(
                                        legion_proto::TOOL_ERROR,
                                        ErrorBody::new(
                                            ErrorCode::Cancelled,
                                            "Cancelled by client.",
                                        ),
                                    ),
                                );
                                let _ = server_w
                                    .write_all(
                                        format!("{}\n", serde_json::to_string(&cancelled).unwrap())
                                            .as_bytes(),
                                    )
                                    .await;
                                let _ = server_w.flush().await;
                            }
                            continue;
                        }
                        let ev = Notification::new(
                            method::EVENT,
                            serde_json::to_value(EventParams {
                                id: req.id.clone().unwrap(),
                                kind: EventKind::Progress,
                                data: json!({"pct": 50}),
                                machine: None,
                            })
                            .unwrap(),
                        );
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&ev).unwrap()).as_bytes(),
                            )
                            .await;
                        let resp =
                            Response::ok(req.id, json!({"ok": true, "on": req.params.get("on")}));
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = server_w.flush().await;
                        if drop_after_first_call && handled_calls == 1 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        (client_r, client_w)
    }

    #[tokio::test]
    async fn link_reuses_sessions_reconnects_once_and_streams_events() {
        let opens = Arc::new(AtomicUsize::new(0));
        let opens_clone = Arc::clone(&opens);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let n = opens_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                // First session drops right after completing its first call so the next call
                // sees a dead session and reconnects once.
                let (r, w) = spawn_mock_node(entry.node.clone(), n == 0, None);
                Session::open_io(r, w, &entry.node, None, Duration::from_secs(5), cancel).await
            })
        });

        let link = Link::with_opener(test_config("tower"), LinkOptions::default(), opener);
        assert_eq!(opens.load(Ordering::SeqCst), 0);

        let hello = link.hello("tower").await.unwrap();
        assert_eq!(hello["node"], json!("tower"));
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        let tools = link.tools("tower", None).await.unwrap();
        assert_eq!(tools["tools"][0]["name"], json!("sys.info"));
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        let mut seen = Vec::new();
        let res = link
            .call("tower", "sys.info", json!({}), Some(json!("pi")), |ev| {
                seen.push(ev)
            })
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
        assert_eq!(res["on"], json!("pi"));
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["kind"], json!("progress"));
        assert_eq!(seen[0]["machine"], json!("tower"));

        // Give the reader loop a moment to observe the server EOF from drop_after_first_call.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Next call reconnects once automatically.
        let res2 = link
            .call("tower", "sys.info", json!({}), None, |_| {})
            .await
            .unwrap();
        assert_eq!(res2["ok"], json!(true));
        assert_eq!(opens.load(Ordering::SeqCst), 2);

        link.close().await;
    }

    #[tokio::test]
    async fn link_cancel_by_call_id_cancels_remote_call() {
        let started = Arc::new(Notify::new());
        let started_clone = Arc::clone(&started);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let notify = Arc::clone(&started_clone);
            Box::pin(async move {
                let (r, w) = spawn_mock_node(entry.node.clone(), false, Some(notify));
                Session::open_io(r, w, &entry.node, None, Duration::from_secs(5), cancel).await
            })
        });

        let link = Link::with_opener(test_config("tower"), LinkOptions::default(), opener);
        let link2 = link.clone();
        let task = tokio::spawn(async move {
            link2
                .call_with_id("call-42", "tower", "exec.run", json!({}), None, |_| {})
                .await
        });

        started.notified().await;
        assert!(link.cancel("call-42"));
        assert!(!link.cancel("unknown-id"));

        let err = task.await.unwrap().unwrap_err();
        assert_eq!(err.code, ErrorCode::Cancelled);
        link.close().await;
    }

    #[tokio::test]
    async fn link_closes_idle_sessions() {
        let opens = Arc::new(AtomicUsize::new(0));
        let opens_clone = Arc::clone(&opens);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            opens_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let (r, w) = spawn_mock_node(entry.node.clone(), false, None);
                Session::open_io(r, w, &entry.node, None, Duration::from_secs(5), cancel).await
            })
        });

        let options = LinkOptions {
            idle_timeout: Duration::from_millis(60),
            ..LinkOptions::default()
        };
        let link = Link::with_opener(test_config("tower"), options, opener);
        link.hello("tower").await.unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_millis(120)).await;
        link.close_idle().await;

        // Session was reaped, so the next hello opens a fresh session.
        link.hello("tower").await.unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        link.close().await;
    }

    #[tokio::test]
    async fn config_update_does_not_close_displaced_session_while_calls_are_in_flight() {
        let first_started = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let opens = Arc::new(AtomicUsize::new(0));

        let first_started_c = Arc::clone(&first_started);
        let release_first_c = Arc::clone(&release_first);
        let opens_c = Arc::clone(&opens);

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let idx = opens_c.fetch_add(1, Ordering::SeqCst);
            let started = Arc::clone(&first_started_c);
            let release = Arc::clone(&release_first_c);
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                let (server_r, mut server_w) = tokio::io::split(server);
                let node_name = entry.node.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            if idx == 0 {
                                started.notify_one();
                                release.notified().await;
                            }
                            let resp = Response::ok(req.id, json!({"session": idx}));
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let link = Link::with_opener(test_config("tower"), LinkOptions::default(), opener);
        let link1 = link.clone();
        let slow_call = tokio::spawn(async move {
            link1
                .call("tower", "slow.op", json!({}), None, |_| {})
                .await
        });

        first_started.notified().await;

        // Change the node's config while `slow_call` is in flight on session 0.
        link.upsert_node(NodeLink::local("tower", "/usr/local/bin/legion"));
        let second = link
            .call("tower", "fast.op", json!({}), None, |_| {})
            .await
            .unwrap();
        assert_eq!(second["session"], json!(1));
        assert_eq!(opens.load(Ordering::SeqCst), 2);

        // Now release session 0's in-flight call: it must succeed rather than being killed.
        release_first.notify_one();
        let first = slow_call.await.unwrap().unwrap();
        assert_eq!(first["session"], json!(0));

        // Once no holders remain, `close_idle` reaps the retired session.
        link.close_idle().await;
        link.close().await;
    }

    #[tokio::test]
    async fn call_delivers_large_event_bursts_without_dropping_and_forwards_principal() {
        const COUNT: usize = 400;
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(256 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                let (server_r, mut server_w) = tokio::io::split(server);
                let node_name = entry.node.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            assert_eq!(req.params["as"], json!("phone"));
                            let id = req.id.clone().unwrap();
                            for seq in 0..COUNT {
                                let ev = Notification::new(
                                    method::EVENT,
                                    serde_json::to_value(EventParams {
                                        id: id.clone(),
                                        kind: EventKind::Stdout,
                                        data: json!(seq),
                                        machine: None,
                                    })
                                    .unwrap(),
                                );
                                server_w
                                    .write_all(
                                        format!("{}\n", serde_json::to_string(&ev).unwrap())
                                            .as_bytes(),
                                    )
                                    .await
                                    .unwrap();
                            }
                            let resp = Response::ok(req.id, json!({"sent": COUNT}));
                            server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await
                                .unwrap();
                            server_w.flush().await.unwrap();
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let mut node = NodeLink::local("tower", "legion");
        node.principal = Some("phone".to_owned());
        let cfg = LinkConfig {
            device_key: None,
            nodes: vec![node],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);
        let mut seen = Vec::new();
        let res = link
            .call("tower", "exec.run", json!({}), None, |ev| seen.push(ev))
            .await
            .unwrap();
        assert_eq!(res["sent"], json!(COUNT));
        assert_eq!(seen.len(), COUNT);
        for (idx, ev) in seen.iter().enumerate() {
            assert_eq!(ev["data"], json!(idx));
        }
        link.close().await;
    }

    #[tokio::test]
    async fn lost_in_flight_call_is_never_retried() {
        let opens = Arc::new(AtomicUsize::new(0));
        let calls_seen = Arc::new(AtomicUsize::new(0));
        let opens_c = Arc::clone(&opens);
        let calls_c = Arc::clone(&calls_seen);

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            opens_c.fetch_add(1, Ordering::SeqCst);
            let seen = Arc::clone(&calls_c);
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                let (server_r, mut server_w) = tokio::io::split(server);
                let node_name = entry.node.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(server_r).lines();
                    let hello_line = lines.next_line().await.unwrap().unwrap();
                    let hello_req: Request = serde_json::from_str(&hello_line).unwrap();
                    let resp = Response::ok(
                        hello_req.id,
                        serde_json::to_value(HelloResult {
                            node: node_name,
                            node_explicit: None,
                            node_id: None,
                            version: "0.1.0".into(),
                            build: None,
                            protocol: PROTOCOL,
                            os: "linux".into(),
                            arch: "x86_64".into(),
                            systems: vec![],
                            system: None,
                            boot_id: None,
                        })
                        .unwrap(),
                    );
                    server_w
                        .write_all(
                            format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                        )
                        .await
                        .unwrap();
                    server_w.flush().await.unwrap();

                    // Read the call request so bytes definitely reached the server, then drop
                    // the stream mid-flight without replying.
                    let _ = lines.next_line().await.unwrap().unwrap();
                    seen.fetch_add(1, Ordering::SeqCst);
                    drop(server_w);
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let link = Link::with_opener(test_config("tower"), LinkOptions::default(), opener);
        let err = link
            .call(
                "tower",
                "fs.remove",
                json!({"path": "/tmp/x"}),
                None,
                |_| {},
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unreachable);
        assert!(err.message.contains("unknown"), "{}", err.message);
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(calls_seen.load(Ordering::SeqCst), 1);
        link.close().await;
    }

    #[test]
    fn link_config_debug_redacts_device_key() {
        let secret = "-----BEGIN OPENSSH PRIVATE KEY-----\nSUPER-SECRET-KEY-MATERIAL\n-----END OPENSSH PRIVATE KEY-----\n";
        let cfg = LinkConfig {
            device_key: Some(secret.to_owned()),
            nodes: vec![NodeLink::local("tower", "legion")],
            ..LinkConfig::default()
        };
        let rendered = format!("{cfg:?}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        assert!(
            !rendered.contains("SUPER-SECRET-KEY-MATERIAL"),
            "{rendered}"
        );
        assert!(!rendered.contains("BEGIN OPENSSH"), "{rendered}");

        let empty = LinkConfig::default();
        assert!(format!("{empty:?}").contains("device_key: None"));
    }

    #[tokio::test]
    async fn forget_closes_live_session_removes_node_and_preserves_device_key_and_peers() {
        let closed_tower = Arc::new(Notify::new());
        let closed_tower_c = Arc::clone(&closed_tower);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let closed = Arc::clone(&closed_tower_c);
            Box::pin(async move {
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                let (server_r, mut server_w) = tokio::io::split(server);
                let node_name = entry.node.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        }
                    }
                    if node_name == "tower" {
                        closed.notify_one();
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let cfg = LinkConfig {
            device_key: Some("existing-device-key".to_owned()),
            nodes: vec![
                NodeLink::local("tower", "legion"),
                NodeLink::local("pi", "legion"),
            ],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);

        link.hello("tower").await.unwrap();
        link.hello("pi").await.unwrap();

        assert!(link.forget("tower").await);
        assert!(!link.forget("tower").await);

        // Server side for `tower` must see EOF promptly when forgotten.
        tokio::time::timeout(Duration::from_secs(2), closed_tower.notified())
            .await
            .unwrap();

        let after = link.config();
        assert_eq!(after.device_key.as_deref(), Some("existing-device-key"));
        assert!(after.node("tower").is_none());
        assert!(after.node("pi").is_some());

        let err = link.hello("tower").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);

        // Unrelated peers remain live and usable.
        let pi_hello = link.hello("pi").await.unwrap();
        assert_eq!(pi_hello["node"], json!("pi"));

        link.close().await;
    }

    #[cfg(feature = "ssh")]
    #[tokio::test]
    async fn pair_generates_key_in_place_reuses_it_and_forget_cleans_up() {
        use base64::Engine;
        use base64::engine::general_purpose::STANDARD;

        use crate::invite::{Invite, enrol_exchange};
        use crate::key::DeviceKey;

        let host_key = DeviceKey::from_seed([11u8; 32])
            .public_key_line("host")
            .unwrap();
        let make_invite = |node: &str, id: &str| -> String {
            Invite {
                v: 1,
                id: id.to_owned(),
                node: node.to_owned(),
                hosts: vec![format!("{node}.example.net")],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec![host_key.clone()],
                key: STANDARD.encode([22u8; 32]),
                principal: "phone".to_owned(),
                expires: "2026-11-24T12:00:00Z".to_owned(),
            }
            .encode()
            .unwrap()
        };

        let mock_enrol = |invite: Invite,
                          key: DeviceKey,
                          name: String,
                          timeouts: SshTimeouts,
                          _allow_auth_failed_recovery: bool,
                          cancel: CancellationToken| async move {
            let public_key = key.public_key_line(&name)?;
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_r, client_w) = tokio::io::split(client);
            let (server_r, mut server_w) = tokio::io::split(server);
            let expected_node = invite.node.clone();
            let expected_principal = invite.principal.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(server_r).lines();
                let line = lines.next_line().await.unwrap().unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(req["enrol"], json!(1));
                assert!(
                    req["public_key"]
                        .as_str()
                        .unwrap()
                        .starts_with("ssh-ed25519 ")
                );
                let reply = json!({
                    "ok": true,
                    "node": expected_node,
                    "principal": expected_principal,
                    "role": "observer",
                    "device": req["name"],
                });
                server_w
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
                server_w.flush().await.unwrap();
            });
            enrol_exchange(
                client_r,
                client_w,
                None,
                None,
                &invite,
                &public_key,
                &name,
                timeouts.enrol,
                cancel,
            )
            .await
        };

        // Track the device_key passed to the session opener so we prove the live Link picked up
        // the newly generated key without rebuilding Link.
        let seen_keys: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_keys_c = Arc::clone(&seen_keys);
        let opener: SessionOpener = Arc::new(move |entry, device_key, cancel| {
            seen_keys_c.lock().unwrap().push(device_key);
            Box::pin(async move {
                let (r, w) = spawn_mock_node(entry.node.clone(), false, None);
                Session::open_io(r, w, &entry.node, None, Duration::from_secs(5), cancel).await
            })
        });

        // Start completely unpaired (`device_key: None`, `nodes: []`).
        let link = Link::with_opener(LinkConfig::default(), LinkOptions::default(), opener);
        assert!(link.config().device_key.is_none());

        // A cancelled first pairing must leave LinkConfig untouched.
        let cancel_now = CancellationToken::new();
        cancel_now.cancel();
        let cancel_err = link
            .pair_with(
                &make_invite("tower", "inv-cancel"),
                "phone",
                cancel_now,
                |_, _, _, _, _, _| async {
                    panic!("enrol_fn must not run when cancelled beforehand")
                },
            )
            .await
            .unwrap_err();
        assert_eq!(cancel_err.code, ErrorCode::Cancelled);
        assert_eq!(link.config(), LinkConfig::default());

        // A failed first pairing must leave LinkConfig untouched.
        let fail_err = link
            .pair_with(
                &make_invite("tower", "inv-bad"),
                "phone",
                CancellationToken::new(),
                |_, _, _, _, _, _| async {
                    Err(ErrorBody::new(ErrorCode::Denied, "Invite expired."))
                },
            )
            .await
            .unwrap_err();
        assert_eq!(fail_err.code, ErrorCode::Denied);
        assert_eq!(link.config(), LinkConfig::default());

        // First successful pairing generates a fresh device key and adds `tower`.
        let enrolled_tower = link
            .pair_with(
                &make_invite("tower", "inv-1"),
                "phone",
                CancellationToken::new(),
                mock_enrol,
            )
            .await
            .unwrap();
        assert_eq!(enrolled_tower.node, "tower");
        assert_eq!(enrolled_tower.principal, "phone");
        assert_eq!(enrolled_tower.role, "observer");
        assert_eq!(enrolled_tower.device, "phone");

        let first_cfg = link.config();
        let first_key = first_cfg.device_key.clone().unwrap();
        assert!(DeviceKey::from_openssh(&first_key).is_ok());
        assert!(first_cfg.node("tower").is_some());

        // Immediately call `tower` on the same live Link: opener must receive the new device key.
        let hello = link.hello("tower").await.unwrap();
        assert_eq!(hello["node"], json!("tower"));
        assert_eq!(
            seen_keys.lock().unwrap().as_slice(),
            &[Some(first_key.clone())]
        );

        // Pairing a second node must reuse the exact same device key (never replace it).
        let enrolled_pi = link
            .pair_with(
                &make_invite("pi", "inv-2"),
                "phone",
                CancellationToken::new(),
                mock_enrol,
            )
            .await
            .unwrap();
        assert_eq!(enrolled_pi.node, "pi");
        assert_eq!(
            link.config().device_key.as_deref(),
            Some(first_key.as_str())
        );
        assert!(link.config().node("tower").is_some());
        assert!(link.config().node("pi").is_some());

        // Forgetting `tower` drops it while keeping `device_key` and `pi`.
        assert!(link.forget("tower").await);
        assert_eq!(
            link.config().device_key.as_deref(),
            Some(first_key.as_str())
        );
        assert!(link.config().node("tower").is_none());
        assert!(link.config().node("pi").is_some());

        let pi_hello = link.hello("pi").await.unwrap();
        assert_eq!(pi_hello["node"], json!("pi"));

        // An existing invalid device_key is rejected rather than silently overwritten.
        let broken_cfg = LinkConfig {
            device_key: Some("not-a-valid-openssh-key".to_owned()),
            nodes: vec![NodeLink::local("tower", "legion")],
            ..LinkConfig::default()
        };
        let broken_link = Link::new(broken_cfg.clone());
        let bad_key_err = broken_link
            .pair_with(
                &make_invite("pi", "inv-3"),
                "phone",
                CancellationToken::new(),
                mock_enrol,
            )
            .await
            .unwrap_err();
        assert_eq!(bad_key_err.code, ErrorCode::InvalidArgs);
        assert_eq!(broken_link.config(), broken_cfg);

        link.close().await;
    }

    #[tokio::test]
    async fn rejected_device_key_on_reconnect_maps_to_device_removed_or_unknown() {
        let opens = Arc::new(AtomicUsize::new(0));
        let opens_c = Arc::clone(&opens);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let attempt = opens_c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt == 0 {
                    // First session works, then drops after one call.
                    let (r, w) = spawn_mock_node(entry.node.clone(), true, None);
                    Session::open_io(r, w, &entry.node, None, Duration::from_secs(5), cancel).await
                } else {
                    // Reconnect sees SSH key auth rejected (key was removed on the node).
                    Err(ErrorBody::new(
                        ErrorCode::Denied,
                        "SSH key authentication for me on tower.example.net:22 was rejected.",
                    )
                    .with_machine(&entry.node)
                    .with_details(json!({
                        "reason": crate::REASON_AUTH_FAILED,
                        "user": "me",
                        "host": "tower.example.net",
                        "port": 22,
                    })))
                }
            })
        });

        let link = Link::with_opener(test_config("tower"), LinkOptions::default(), opener);

        // Initial call works.
        let ok = link
            .call("tower", "sys.info", json!({}), None, |_| {})
            .await
            .unwrap();
        assert_eq!(ok["ok"], json!(true));

        // Let the reader observe EOF from the mock node.
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Reconnect fails with `device_removed_or_unknown` and a hint to pair again.
        let err = link
            .call("tower", "sys.info", json!({}), None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert_eq!(err.machine.as_deref(), Some("tower"));
        let details = err.details.as_ref().unwrap();
        assert_eq!(
            details["reason"],
            json!(crate::REASON_DEVICE_REMOVED_OR_UNKNOWN)
        );
        assert_eq!(details["host"], json!("tower.example.net"));
        assert_eq!(details["port"], json!(22));
        assert_eq!(details["user"], json!("me"));
        let hint = err.hint.as_deref().unwrap().to_ascii_lowercase();
        assert!(hint.contains("pair"), "expected pair hint, got: {hint}");

        // Non-auth errors such as host_key_mismatch are preserved untouched.
        let mismatch_opener: SessionOpener = Arc::new(|entry, _, _| {
            Box::pin(async move {
                Err(ErrorBody::new(ErrorCode::Denied, "Host key mismatch.")
                    .with_machine(&entry.node)
                    .with_details(json!({
                        "reason": crate::REASON_HOST_KEY_MISMATCH,
                        "host": "tower.example.net",
                        "port": 22,
                    })))
            })
        });
        let mismatch_link = Link::with_opener(
            test_config("tower"),
            LinkOptions::default(),
            mismatch_opener,
        );
        let mismatch = mismatch_link.hello("tower").await.unwrap_err();
        assert_eq!(mismatch.code, ErrorCode::Denied);
        assert_eq!(
            mismatch.details.as_ref().unwrap()["reason"],
            json!(crate::REASON_HOST_KEY_MISMATCH)
        );

        link.close().await;
    }

    #[tokio::test]
    async fn open_and_not_written_errors_mark_details_sent_false_but_in_flight_and_remote_do_not() {
        // 1. Session open failure marks `details.sent = false` while keeping existing details.
        let open_fail_opener: SessionOpener = Arc::new(|entry, _, _| {
            Box::pin(async move {
                Err(
                    ErrorBody::new(ErrorCode::Unreachable, "Connection refused.")
                        .with_machine(&entry.node)
                        .with_details(json!({ "reason": crate::REASON_UNREACHABLE })),
                )
            })
        });
        let open_fail_link = Link::with_opener(
            test_config("tower"),
            LinkOptions::default(),
            open_fail_opener,
        );
        let err = open_fail_link
            .call("tower", "sys.info", json!({}), None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unreachable);
        assert_eq!(
            err.details.as_ref().unwrap()[crate::DETAIL_SENT],
            json!(false)
        );
        assert_eq!(
            err.details.as_ref().unwrap()["reason"],
            json!(crate::REASON_UNREACHABLE)
        );

        // 2. Call lost in flight (server receives request then drops connection) does NOT set `sent: false`.
        let lost_opener: SessionOpener = Arc::new(|entry, _, cancel| {
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let reply = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
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
                            let mut out = serde_json::to_string(&reply).unwrap();
                            out.push('\n');
                            server_w.write_all(out.as_bytes()).await.unwrap();
                            server_w.flush().await.unwrap();
                        } else if req.method == method::CALL {
                            // Request arrived on the server; drop pipe before answering.
                            drop(server_w);
                            break;
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });
        let lost_link =
            Link::with_opener(test_config("tower"), LinkOptions::default(), lost_opener);
        let lost_err = lost_link
            .call("tower", "power.reboot", json!({}), None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(lost_err.code, ErrorCode::Unreachable);
        assert_ne!(
            lost_err
                .details
                .as_ref()
                .and_then(|d| d.get(crate::DETAIL_SENT)),
            Some(&json!(false)),
            "in-flight failure must never claim sent=false"
        );

        // 3. Remote timeout returned from the node does NOT set `sent: false`.
        let remote_timeout_opener: SessionOpener = Arc::new(|entry, _, cancel| {
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        let reply = if req.method == method::HELLO {
                            Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
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
                            )
                        } else {
                            Response::error(
                                req.id,
                                legion_proto::RpcError::new(
                                    legion_proto::TOOL_ERROR,
                                    ErrorBody::new(ErrorCode::Timeout, "Tool timed out on node.")
                                        .with_machine(&node_name),
                                ),
                            )
                        };
                        let mut out = serde_json::to_string(&reply).unwrap();
                        out.push('\n');
                        server_w.write_all(out.as_bytes()).await.unwrap();
                        server_w.flush().await.unwrap();
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });
        let timeout_link = Link::with_opener(
            test_config("tower"),
            LinkOptions::default(),
            remote_timeout_opener,
        );
        let remote_err = timeout_link
            .call("tower", "exec.run", json!({}), None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(remote_err.code, ErrorCode::Timeout);
        assert_ne!(
            remote_err
                .details
                .as_ref()
                .and_then(|d| d.get(crate::DETAIL_SENT)),
            Some(&json!(false)),
            "remote tool timeout must never claim sent=false"
        );

        // 4. NotWritten on retry session marks `details.sent = false`.
        let not_written_opener: SessionOpener = Arc::new(|entry, _, cancel| {
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                let ready = Arc::new(Notify::new());
                let ready_tx = Arc::clone(&ready);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    if let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        let reply = Response::ok(
                            req.id,
                            serde_json::to_value(HelloResult {
                                node: node_name.clone(),
                                node_explicit: None,
                                node_id: None,
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
                        let mut out = serde_json::to_string(&reply).unwrap();
                        out.push('\n');
                        server_w.write_all(out.as_bytes()).await.unwrap();
                        server_w.flush().await.unwrap();
                        ready_tx.notified().await;
                        // Close both halves so the client session dies before writing the call.
                        drop(server_w);
                        drop(lines);
                    }
                });
                let session = Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await?;
                ready.notify_one();
                tokio::time::sleep(Duration::from_millis(30)).await;
                Ok(session)
            })
        });
        let not_written_link = Link::with_opener(
            test_config("tower"),
            LinkOptions::default(),
            not_written_opener,
        );
        let nw_err = not_written_link
            .call("tower", "sys.info", json!({}), None, |_| {})
            .await
            .unwrap_err();
        assert_eq!(nw_err.code, ErrorCode::Unreachable);
        assert_eq!(
            nw_err.details.as_ref().unwrap()[crate::DETAIL_SENT],
            json!(false)
        );

        open_fail_link.close().await;
        lost_link.close().await;
        timeout_link.close().await;
        not_written_link.close().await;
    }

    #[cfg(feature = "ssh")]
    #[tokio::test]
    async fn set_device_key_stores_validated_key_and_rejects_replacement() {
        let link = Link::new(LinkConfig::default());
        let key1 = crate::key::DeviceKey::generate()
            .unwrap()
            .to_openssh()
            .unwrap();
        let key2 = crate::key::DeviceKey::generate()
            .unwrap()
            .to_openssh()
            .unwrap();

        link.set_device_key(key1.clone()).unwrap();
        assert_eq!(link.config().device_key.as_deref(), Some(key1.as_str()));

        // Re-setting the identical key is idempotent.
        link.set_device_key(key1.clone()).unwrap();

        // Replacing with a different key is refused.
        let err = link.set_device_key(key2).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);

        // Invalid PEM is refused.
        let bad_link = Link::new(LinkConfig::default());
        let bad_err = bad_link.set_device_key("not a key".to_owned()).unwrap_err();
        assert_eq!(bad_err.code, ErrorCode::InvalidArgs);
    }

    fn test_host_key(seed: u8) -> String {
        #[cfg(feature = "ssh")]
        {
            crate::key::DeviceKey::from_seed([seed; 32])
                .public_key_line("host")
                .unwrap()
        }
        #[cfg(not(feature = "ssh"))]
        {
            use base64::Engine;
            use base64::engine::general_purpose::STANDARD;
            let mut blob = Vec::new();
            blob.extend_from_slice(&(11u32).to_be_bytes());
            blob.extend_from_slice(b"ssh-ed25519");
            blob.extend_from_slice(&(32u32).to_be_bytes());
            blob.extend_from_slice(&[seed; 32]);
            format!("ssh-ed25519 {}", STANDARD.encode(blob))
        }
    }

    fn spawn_endpoint_responder(
        node: String,
        responses: Arc<Mutex<Vec<Value>>>,
    ) -> (
        tokio::io::ReadHalf<tokio::io::DuplexStream>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (client_r, client_w) = tokio::io::split(client);
        let (server_r, mut server_w) = tokio::io::split(server);
        tokio::spawn(async move {
            let mut lines = BufReader::new(server_r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Request = serde_json::from_str(&line).unwrap();
                let resp = if req.method == method::HELLO {
                    Response::ok(
                        req.id,
                        serde_json::to_value(HelloResult {
                            node: node.clone(),
                            node_explicit: None,
                            node_id: None,
                            version: "0.1.0".into(),
                            build: None,
                            protocol: PROTOCOL,
                            os: "linux".into(),
                            arch: "x86_64".into(),
                            systems: vec![],
                            system: None,
                            boot_id: None,
                        })
                        .unwrap(),
                    )
                } else if req.method == method::CALL
                    && req.params.get("tool").and_then(Value::as_str) == Some("device.endpoint")
                {
                    assert_eq!(req.params.get("on"), Some(&json!("all")));
                    let next = responses.lock().unwrap().remove(0);
                    Response::ok(req.id, next)
                } else {
                    Response::ok(req.id, json!({ "ok": true, "node": node }))
                };
                let _ = server_w
                    .write_all(format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes())
                    .await;
                let _ = server_w.flush().await;
            }
        });
        (client_r, client_w)
    }

    #[tokio::test]
    async fn learn_validates_introducer_preserves_paired_pins_and_flags_identity_changes() {
        let key_a = test_host_key(1);
        let key_b1 = test_host_key(2);
        let key_b2 = test_host_key(3);
        let replies = Arc::new(Mutex::new(vec![
            // First learn: introduces `pi`, includes `tower` (paired), rejects mismatched `spoof`,
            // and marks `nas` unreachable.
            json!({
                "results": {
                    "tower": {
                        "ok": {
                            "node": "tower",
                            "hosts": ["other.example.net"],
                            "port": 2222,
                            "user": "someone",
                            "host_keys": [key_a.clone()]
                        }
                    },
                    "pi": {
                        "ok": {
                            "node": "pi",
                            "hosts": ["pi.example.net", "100.64.0.9"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_b1.clone()]
                        }
                    },
                    "spoof": {
                        "ok": {
                            "node": "other-name",
                            "hosts": ["spoof.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_b1.clone()]
                        }
                    },
                    "nas": {
                        "error": {
                            "code": "unreachable",
                            "message": "nas is off"
                        }
                    }
                }
            }),
            // Second learn: `pi` reports a new host key (`key_b2`) and `tower` also reports a new
            // host key (`key_b2`). Neither pin may be silently overwritten.
            json!({
                "results": {
                    "tower": {
                        "ok": {
                            "node": "tower",
                            "hosts": ["tower.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_b2.clone()]
                        }
                    },
                    "pi": {
                        "ok": {
                            "node": "pi",
                            "hosts": ["pi.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_b2.clone()]
                        }
                    }
                }
            }),
        ]));

        let replies_c = Arc::clone(&replies);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let replies_c = Arc::clone(&replies_c);
            Box::pin(async move {
                let (r, w) = spawn_endpoint_responder(entry.node.clone(), replies_c);
                Session::open_io(r, w, &entry.node, None, Duration::from_secs(5), cancel).await
            })
        });

        let tower_target = SshTarget {
            hosts: vec!["tower.example.net".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![key_a.clone()],
        };
        let initial_cfg = LinkConfig {
            device_key: Some("dummy-key".to_owned()),
            nodes: vec![NodeLink::ssh(
                "tower",
                tower_target.clone(),
                Some("device:pixel".to_owned()),
            )],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(initial_cfg, LinkOptions::default(), opener);

        // 1. First learn adds `pi`, rejects `spoof`, notes `nas` unreachable, and never overwrites
        //    directly paired `tower`'s target.
        let r1 = link.learn().await.unwrap();
        assert_eq!(r1.introducer, "tower");
        assert_eq!(r1.added, vec!["pi".to_owned()]);
        assert!(r1.identity_changed.is_empty());
        assert_eq!(r1.rejected, vec!["spoof".to_owned()]);
        assert_eq!(r1.unreachable, vec!["nas".to_owned()]);

        let cfg1 = link.config();
        let tower1 = cfg1.node("tower").unwrap();
        assert_eq!(tower1.origin, NodeOrigin::Paired);
        assert_eq!(tower1.target.as_ref(), Some(&tower_target));
        assert!(tower1.identity_changed.is_none());

        let pi1 = cfg1.node("pi").unwrap();
        assert_eq!(pi1.origin, NodeOrigin::Learned);
        assert_eq!(pi1.learned_from.as_deref(), Some("tower"));
        assert_eq!(pi1.principal, None);
        assert_eq!(pi1.target.as_ref().unwrap().host_keys, vec![key_b1.clone()]);
        assert!(cfg1.node("spoof").is_none());

        // 2. Second learn reports changed host keys for both `tower` and `pi`: old pins stay in
        //    place and both entries are flagged `identity_changed`.
        let r2 = link.learn().await.unwrap();
        assert_eq!(
            r2.identity_changed,
            vec!["pi".to_owned(), "tower".to_owned()]
        );

        let cfg2 = link.config();
        let pi2 = cfg2.node("pi").unwrap();
        assert_eq!(pi2.target.as_ref().unwrap().host_keys, vec![key_b1.clone()]);
        assert_eq!(
            pi2.identity_changed.as_ref().unwrap().target.host_keys,
            vec![key_b2.clone()]
        );
        assert_eq!(pi2.identity_changed.as_ref().unwrap().reported_by, "tower");

        let tower2 = cfg2.node("tower").unwrap();
        assert_eq!(
            tower2.target.as_ref().unwrap().host_keys,
            vec![key_a.clone()]
        );
        assert_eq!(
            tower2.identity_changed.as_ref().unwrap().target.host_keys,
            vec![key_b2.clone()]
        );

        // 3. Serializable config round-trips origin, learned_from, and identity_changed cleanly.
        let encoded = serde_json::to_string(&cfg2).unwrap();
        let decoded: LinkConfig = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, cfg2);

        // 4. Directly paired `tower` cannot accept a learned identity change (informational only).
        let fp_b2 = public_key_fingerprint(&key_b2).unwrap();
        let paired_accept = link
            .accept_identity_change("tower", std::slice::from_ref(&fp_b2))
            .unwrap_err();
        assert_eq!(paired_accept.code, ErrorCode::Denied);
        assert!(
            link.dismiss_identity_change("tower")
                .unwrap()
                .identity_changed
                .is_none()
        );

        // 5. Wrong fingerprint is refused; matching fingerprint promotes new pins and clears flag.
        let wrong_accept = link
            .accept_identity_change("pi", &["SHA256:wrong".to_owned()])
            .unwrap_err();
        assert_eq!(wrong_accept.code, ErrorCode::Conflict);

        let accepted = link
            .accept_identity_change("pi", std::slice::from_ref(&fp_b2))
            .unwrap();
        assert_eq!(
            accepted.target.as_ref().unwrap().host_keys,
            vec![key_b2.clone()]
        );
        assert!(accepted.identity_changed.is_none());
        assert_eq!(
            link.config()
                .node("pi")
                .unwrap()
                .target
                .as_ref()
                .unwrap()
                .host_keys,
            vec![key_b2]
        );

        // Accepting again when no change is pending returns InvalidArgs.
        let again = link
            .accept_identity_change("pi", std::slice::from_ref(&fp_b2))
            .unwrap_err();
        assert_eq!(again.code, ErrorCode::InvalidArgs);

        link.close().await;
    }

    #[tokio::test]
    async fn call_machine_routes_direct_relays_on_unsent_and_handles_learned_unsynced_and_revocation()
     {
        #[derive(Clone, Copy)]
        enum Mode {
            Ok,
            Unreachable,
            AuthFailed,
            HostKeyMismatch,
            DropInFlight,
            RemoteToolError,
        }

        let modes: Arc<Mutex<HashMap<String, Mode>>> = Arc::new(Mutex::new(HashMap::from([
            ("tower".to_owned(), Mode::Ok),
            ("pi".to_owned(), Mode::Ok),
        ])));
        let opens: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        type SeenCalls = Arc<Mutex<Vec<(String, Option<Value>)>>>;
        let seen_relays: SeenCalls = Arc::new(Mutex::new(Vec::new()));

        let modes_c = Arc::clone(&modes);
        let opens_c = Arc::clone(&opens);
        let relays_c = Arc::clone(&seen_relays);

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let mode = *modes_c
                .lock()
                .unwrap()
                .get(&entry.node)
                .unwrap_or(&Mode::Ok);
            *opens_c
                .lock()
                .unwrap()
                .entry(entry.node.clone())
                .or_insert(0) += 1;
            let relays_c = Arc::clone(&relays_c);
            Box::pin(async move {
                match mode {
                    Mode::Unreachable => {
                        return Err(ErrorBody::new(
                            ErrorCode::Unreachable,
                            format!("{} is offline", entry.node),
                        )
                        .with_machine(&entry.node));
                    }
                    Mode::AuthFailed => {
                        return Err(ErrorBody::new(
                            ErrorCode::Denied,
                            format!("SSH auth rejected on {}", entry.node),
                        )
                        .with_machine(&entry.node)
                        .with_details(json!({ "reason": crate::REASON_AUTH_FAILED })));
                    }
                    Mode::HostKeyMismatch => {
                        return Err(ErrorBody::new(
                            ErrorCode::Denied,
                            format!("Host key mismatch on {}", entry.node),
                        )
                        .with_machine(&entry.node)
                        .with_details(json!({ "reason": crate::REASON_HOST_KEY_MISMATCH })));
                    }
                    Mode::Ok | Mode::DropInFlight | Mode::RemoteToolError => {}
                }

                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                let (server_r, mut server_w) = tokio::io::split(server);
                let node_name = entry.node.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            let on_val = req.params.get("on").cloned();
                            relays_c
                                .lock()
                                .unwrap()
                                .push((node_name.clone(), on_val.clone()));
                            match mode {
                                Mode::DropInFlight => break,
                                Mode::RemoteToolError => {
                                    let resp = Response::error(
                                        req.id,
                                        legion_proto::RpcError::new(
                                            legion_proto::TOOL_ERROR,
                                            ErrorBody::new(ErrorCode::Timeout, "tool timed out"),
                                        ),
                                    );
                                    let _ = server_w
                                        .write_all(
                                            format!("{}\n", serde_json::to_string(&resp).unwrap())
                                                .as_bytes(),
                                        )
                                        .await;
                                    let _ = server_w.flush().await;
                                }
                                _ => {
                                    let resp = Response::ok(
                                        req.id,
                                        json!({ "handled_by": node_name, "on": on_val }),
                                    );
                                    let _ = server_w
                                        .write_all(
                                            format!("{}\n", serde_json::to_string(&resp).unwrap())
                                                .as_bytes(),
                                        )
                                        .await;
                                    let _ = server_w.flush().await;
                                }
                            }
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let target_a = SshTarget {
            hosts: vec!["tower.example.net".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![test_host_key(10)],
        };
        let target_b = SshTarget {
            hosts: vec!["pi.example.net".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![test_host_key(11)],
        };
        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![
                NodeLink::ssh("tower", target_a, Some("device:pixel".to_owned())),
                NodeLink::learned("pi", target_b, Some("device:pixel".to_owned()), "tower"),
            ],
            ..LinkConfig::default()
        };
        let opts = LinkOptions {
            unsynced_backoff: Duration::from_millis(80),
            ..LinkOptions::default()
        };
        let link = Link::with_opener(cfg, opts, opener);

        // 1. Direct call to `pi` succeeds directly.
        let r_direct = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap();
        assert_eq!(r_direct.route, Route::Direct);
        assert_eq!(r_direct.value["handled_by"], json!("pi"));
        assert_eq!(
            link.route_status().get("pi"),
            Some(&MachineRouteStatus::Direct)
        );

        // 2. Call to unknown machine `nas` relays through `tower` with `on: "nas"`.
        let r_nas = link
            .call_machine("nas", "sys.info", json!({}), |_| {})
            .await
            .unwrap();
        assert_eq!(
            r_nas.route,
            Route::Relay {
                via: "tower".to_owned()
            }
        );
        assert_eq!(r_nas.value["on"], json!("nas"));
        assert_eq!(
            link.route_status().get("nas"),
            Some(&MachineRouteStatus::Relay {
                via: "tower".to_owned()
            })
        );

        // 3. Direct unreachable (`sent == false`) on `pi` relays through `tower` with `on: "pi"`.
        link.close().await;
        modes
            .lock()
            .unwrap()
            .insert("pi".to_owned(), Mode::Unreachable);
        let r_relayed = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap();
        assert_eq!(
            r_relayed.route,
            Route::Relay {
                via: "tower".to_owned()
            }
        );
        assert_eq!(r_relayed.value["on"], json!("pi"));

        // 4. Lost-in-flight and remote tool error on `pi` NEVER fail over to `tower`.
        link.close().await;
        seen_relays.lock().unwrap().clear();
        modes
            .lock()
            .unwrap()
            .insert("pi".to_owned(), Mode::DropInFlight);
        let lost_err = link
            .call_machine("pi", "power.restart", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(lost_err.code, ErrorCode::Unreachable);
        assert!(
            !seen_relays
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "tower"),
            "lost-in-flight call must never fail over to relay"
        );

        link.close().await;
        seen_relays.lock().unwrap().clear();
        modes
            .lock()
            .unwrap()
            .insert("pi".to_owned(), Mode::RemoteToolError);
        let tool_err = link
            .call_machine("pi", "power.restart", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(tool_err.code, ErrorCode::Timeout);
        assert!(
            !seen_relays
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "tower"),
            "remote tool error must never fail over to relay"
        );

        // 5. Direct host_key_mismatch NEVER fails over silently.
        link.close().await;
        seen_relays.lock().unwrap().clear();
        modes
            .lock()
            .unwrap()
            .insert("pi".to_owned(), Mode::HostKeyMismatch);
        let hk_err = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(hk_err.code, ErrorCode::Denied);
        assert_eq!(
            hk_err.details.as_ref().unwrap()["reason"],
            json!(crate::REASON_HOST_KEY_MISMATCH)
        );
        assert!(
            !seen_relays
                .lock()
                .unwrap()
                .iter()
                .any(|(n, _)| n == "tower")
        );

        // 6. Learned `pi` rejecting key auth (`device_removed_or_unknown`) falls back to relay
        //    `tower`, records NotSyncedYet, skips direct attempts during backoff, and recovers
        //    once backoff expires.
        link.close().await;
        opens.lock().unwrap().clear();
        modes
            .lock()
            .unwrap()
            .insert("pi".to_owned(), Mode::AuthFailed);
        let r_unsynced = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap();
        assert_eq!(
            r_unsynced.route,
            Route::Relay {
                via: "tower".to_owned()
            }
        );
        assert_eq!(*opens.lock().unwrap().get("pi").unwrap_or(&0), 1);
        assert_eq!(
            link.route_status().get("pi"),
            Some(&MachineRouteStatus::NotSyncedYet {
                introducer: Some("tower".to_owned()),
                via: Some("tower".to_owned()),
            })
        );

        // Second call within backoff must bypass direct `pi` (open count stays 1).
        let r_cached = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap();
        assert_eq!(
            r_cached.route,
            Route::Relay {
                via: "tower".to_owned()
            }
        );
        assert_eq!(*opens.lock().unwrap().get("pi").unwrap_or(&0), 1);

        // Once backoff expires and `pi` accepts the device key, direct route resumes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        modes.lock().unwrap().insert("pi".to_owned(), Mode::Ok);
        let r_recovered = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap();
        assert_eq!(r_recovered.route, Route::Direct);
        assert_eq!(*opens.lock().unwrap().get("pi").unwrap_or(&0), 2);
        assert_eq!(
            link.route_status().get("pi"),
            Some(&MachineRouteStatus::Direct)
        );

        // 7. Device removed on `pi` while paired `tower` is unreachable surfaces
        //    `device_removed_or_unknown` (never masked by `tower`'s unreachable error).
        link.close().await;
        modes
            .lock()
            .unwrap()
            .insert("tower".to_owned(), Mode::Unreachable);
        modes
            .lock()
            .unwrap()
            .insert("pi".to_owned(), Mode::AuthFailed);
        let revoked_b = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(revoked_b.code, ErrorCode::Denied);
        assert_eq!(
            revoked_b.details.as_ref().unwrap()["reason"],
            json!(crate::REASON_DEVICE_REMOVED_OR_UNKNOWN)
        );

        // 8. Refusal from every machine including paired `tower` returns `device_removed_or_unknown`.
        modes
            .lock()
            .unwrap()
            .insert("tower".to_owned(), Mode::AuthFailed);
        let revoked_all = link
            .call_machine("pi", "sys.info", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(revoked_all.code, ErrorCode::Denied);
        assert_eq!(
            revoked_all.details.as_ref().unwrap()["reason"],
            json!(crate::REASON_DEVICE_REMOVED_OR_UNKNOWN)
        );

        let revoked_paired = link
            .call_machine("tower", "sys.info", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(revoked_paired.code, ErrorCode::Denied);
        assert_eq!(
            revoked_paired.details.as_ref().unwrap()["reason"],
            json!(crate::REASON_DEVICE_REMOVED_OR_UNKNOWN)
        );

        link.close().await;
    }

    #[tokio::test]
    async fn remote_error_with_sent_false_never_triggers_failover_and_strips_sent() {
        let seen_calls: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_calls_c = Arc::clone(&seen_calls);

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let seen_calls_c = Arc::clone(&seen_calls_c);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            let tool = req
                                .params
                                .get("tool")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            seen_calls_c
                                .lock()
                                .unwrap()
                                .push((node_name.clone(), tool.clone()));
                            let resp =
                                if node_name == "pi" {
                                    Response::error(
                                        req.id,
                                        legion_proto::RpcError::new(
                                            legion_proto::TOOL_ERROR,
                                            ErrorBody::new(
                                                ErrorCode::Unreachable,
                                                "spoofed unsent remote error",
                                            )
                                            .with_details(json!({
                                                "sent": false,
                                                "keep": "yes",
                                            })),
                                        ),
                                    )
                                } else {
                                    Response::ok(req.id, json!({ "handled_by": node_name }))
                                };
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![
                NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![test_host_key(10)],
                    },
                    Some("device:pixel".to_owned()),
                ),
                NodeLink::learned(
                    "pi",
                    SshTarget {
                        hosts: vec!["pi.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![test_host_key(11)],
                    },
                    Some("device:pixel".to_owned()),
                    "tower",
                ),
            ],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);
        let err = link
            .call_machine("pi", "job.start", json!({"cmd": "echo hi"}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Unreachable);
        assert_eq!(
            seen_calls.lock().unwrap().as_slice(),
            &[("pi".to_owned(), "job.start".to_owned())],
            "a remote node returning details.sent=false must never cause failover"
        );
        let details = err.details.as_ref().expect("keep field should survive");
        assert!(
            details.get(crate::DETAIL_SENT).is_none(),
            "remote details.sent must be stripped: {details}"
        );
        assert_eq!(details.get("keep"), Some(&json!("yes")));
        link.close().await;
    }

    #[tokio::test]
    async fn hello_node_mismatch_fails_with_denied_and_no_failover() {
        let seen_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_calls_c = Arc::clone(&seen_calls);

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let seen_calls_c = Arc::clone(&seen_calls_c);
            Box::pin(async move {
                let entry_name = entry.node.clone();
                // Entry `m` connects to a server whose `hello.node` is `pi`!
                let advertised_name = if entry_name == "m" {
                    "pi".to_owned()
                } else {
                    entry_name.clone()
                };
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: advertised_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            seen_calls_c.lock().unwrap().push(entry_name.clone());
                            let resp = Response::ok(req.id, json!({ "ok": true }));
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![
                NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![test_host_key(10)],
                    },
                    Some("device:pixel".to_owned()),
                ),
                NodeLink::learned(
                    "m",
                    SshTarget {
                        hosts: vec!["pi.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![test_host_key(11)],
                    },
                    None,
                    "tower",
                ),
            ],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);
        let err = link
            .call_machine("m", "power.shutdown", json!({}), |_| {})
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        let details = err.details.as_ref().expect("details expected");
        assert_eq!(details["reason"], json!(crate::REASON_NODE_MISMATCH));
        assert_eq!(details[crate::DETAIL_SENT], json!(false));
        assert!(
            seen_calls.lock().unwrap().is_empty(),
            "mismatched hello node must execute zero calls and never fail over"
        );
        link.close().await;
    }

    #[tokio::test]
    async fn learn_rejects_host_keys_overlapping_another_configured_machine() {
        let key_tower = test_host_key(20);
        let key_pi = test_host_key(21);
        let key_pi_extra = test_host_key(22);

        let key_tower_c = key_tower.clone();
        let key_pi_c = key_pi.clone();
        let key_pi_extra_c = key_pi_extra.clone();

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let key_tower_c = key_tower_c.clone();
            let key_pi_c = key_pi_c.clone();
            let key_pi_extra_c = key_pi_extra_c.clone();
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            let resp = Response::ok(
                                req.id,
                                json!({
                                    "results": {
                                        "tower": {
                                            "ok": {
                                                "node": "tower",
                                                "hosts": ["tower.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [key_tower_c],
                                            }
                                        },
                                        // Malicious answer pointing fake machine `spoof` at `pi`'s pinned host key.
                                        "spoof": {
                                            "ok": {
                                                "node": "spoof",
                                                "hosts": ["pi.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [key_pi_c, key_pi_extra_c],
                                            }
                                        }
                                    }
                                }),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![
                NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_tower],
                    },
                    Some("device:pixel".to_owned()),
                ),
                NodeLink::ssh(
                    "pi",
                    SshTarget {
                        hosts: vec!["pi.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_pi],
                    },
                    Some("device:pixel".to_owned()),
                ),
            ],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);
        let report = link.learn().await.unwrap();
        assert!(report.rejected.contains(&"spoof".to_owned()));
        assert!(report.added.is_empty());
        assert!(link.config().node("spoof").is_none());
        link.close().await;
    }

    #[tokio::test]
    async fn learn_bounds_counts_string_lengths_and_rejects_disallowed_addresses() {
        let key_tower = test_host_key(30);
        let payload = Arc::new(Mutex::new(Value::Null));
        let payload_c = Arc::clone(&payload);

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let payload_c = Arc::clone(&payload_c);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        if req.method == method::HELLO {
                            let resp = Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            );
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        } else if req.method == method::CALL {
                            let body = payload_c.lock().unwrap().clone();
                            let resp = Response::ok(req.id, body);
                            let _ = server_w
                                .write_all(
                                    format!("{}\n", serde_json::to_string(&resp).unwrap())
                                        .as_bytes(),
                                )
                                .await;
                            let _ = server_w.flush().await;
                        }
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let bad_hosts = [
            ("h_localhost", "localhost"),
            ("h_sub_localhost", "foo.localhost"),
            ("h_sub_dot", "foo.localhost."),
            ("h_v4_loop", "127.0.0.1"),
            ("h_v4_loop_hi", "127.0.0.42"),
            ("h_v6_loop", "::1"),
            ("h_v4_mapped_loop", "::ffff:127.0.0.1"),
            ("h_unspec4", "0.0.0.0"),
            ("h_unspec6", "::"),
            ("h_ll4", "169.254.1.1"),
            ("h_ll6", "fe80::1"),
            ("h_mcast4", "224.0.0.1"),
            ("h_mcast6", "ff02::1"),
            ("h_bcast", "255.255.255.255"),
        ];

        let mut results = serde_json::Map::new();
        for (idx, (name, host)) in bad_hosts.iter().enumerate() {
            results.insert(
                (*name).to_owned(),
                json!({
                    "ok": {
                        "node": name,
                        "hosts": [host],
                        "port": 22,
                        "user": "me",
                        "host_keys": [test_host_key(40 + idx as u8)],
                    }
                }),
            );
        }
        // Too many hosts (9 > 8), too many keys (9 > 8), port 0, and good host.
        results.insert(
            "too_many_hosts".to_owned(),
            json!({
                "ok": {
                    "node": "too_many_hosts",
                    "hosts": (0..9).map(|i| format!("h{i}.example.net")).collect::<Vec<_>>(),
                    "port": 22,
                    "user": "me",
                    "host_keys": [test_host_key(80)],
                }
            }),
        );
        results.insert(
            "too_many_keys".to_owned(),
            json!({
                "ok": {
                    "node": "too_many_keys",
                    "hosts": ["keys.example.net"],
                    "port": 22,
                    "user": "me",
                    "host_keys": (0..9).map(|i| test_host_key(90 + i)).collect::<Vec<_>>(),
                }
            }),
        );
        results.insert(
            "port_zero".to_owned(),
            json!({
                "ok": {
                    "node": "port_zero",
                    "hosts": ["zero.example.net"],
                    "port": 0,
                    "user": "me",
                    "host_keys": [test_host_key(105)],
                }
            }),
        );
        results.insert(
            "good_peer".to_owned(),
            json!({
                "ok": {
                    "node": "good_peer",
                    "hosts": ["good.example.net", "10.0.2.2"],
                    "port": 22,
                    "user": "me",
                    "host_keys": [test_host_key(106)],
                }
            }),
        );
        *payload.lock().unwrap() = json!({ "results": results });

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![NodeLink::ssh(
                "tower",
                SshTarget {
                    hosts: vec!["tower.example.net".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec![key_tower.clone()],
                },
                Some("device:pixel".to_owned()),
            )],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg.clone(), LinkOptions::default(), Arc::clone(&opener));
        let report = link.learn().await.unwrap();
        assert_eq!(report.added, vec!["good_peer".to_owned()]);
        for (name, _) in &bad_hosts {
            assert!(
                report.rejected.contains(&(*name).to_owned()),
                "expected {name} to be rejected by default"
            );
        }
        assert!(report.rejected.contains(&"too_many_hosts".to_owned()));
        assert!(report.rejected.contains(&"too_many_keys".to_owned()));
        assert!(report.rejected.contains(&"port_zero".to_owned()));
        link.close().await;

        // With allow_loopback_endpoints: true, loopback hosts are accepted while unspecified,
        // link-local, multicast, broadcast, and port 0 remain rejected.
        let link_loop = Link::with_opener(
            cfg.clone(),
            LinkOptions {
                allow_loopback_endpoints: true,
                ..LinkOptions::default()
            },
            Arc::clone(&opener),
        );
        let report_loop = link_loop.learn().await.unwrap();
        assert!(report_loop.added.contains(&"h_v4_loop".to_owned()));
        assert!(report_loop.added.contains(&"h_v6_loop".to_owned()));
        assert!(report_loop.added.contains(&"h_localhost".to_owned()));
        assert!(report_loop.rejected.contains(&"h_unspec4".to_owned()));
        assert!(report_loop.rejected.contains(&"h_ll6".to_owned()));
        assert!(report_loop.rejected.contains(&"h_mcast4".to_owned()));
        assert!(report_loop.rejected.contains(&"h_bcast".to_owned()));
        assert!(report_loop.rejected.contains(&"port_zero".to_owned()));
        link_loop.close().await;

        // Cap machines per answer & total configured nodes at 64.
        let mut flood = serde_json::Map::new();
        for i in 0..70u8 {
            let name = format!("peer-{i:02}");
            flood.insert(
                name.clone(),
                json!({
                    "ok": {
                        "node": name,
                        "hosts": [format!("p{i}.example.net")],
                        "port": 22,
                        "user": "me",
                        "host_keys": [test_host_key(120 + i)],
                    }
                }),
            );
        }
        *payload.lock().unwrap() = json!({ "results": flood });
        let link_cap = Link::with_opener(cfg, LinkOptions::default(), opener);
        let report_cap = link_cap.learn().await.unwrap();
        assert!(
            link_cap.config().nodes.len() <= 64,
            "total configured nodes must be capped at 64, got {}",
            link_cap.config().nodes.len()
        );
        assert!(
            !report_cap.rejected.is_empty(),
            "excess machines beyond cap must be rejected"
        );
        link_cap.close().await;
    }

    #[tokio::test]
    async fn learned_introducer_refused_forget_cascades_and_superset_pins_accepted() {
        let key_tower = test_host_key(50);
        let key_pi1 = test_host_key(51);
        let key_pi2 = test_host_key(52);
        let key_nas = test_host_key(53);

        let key_tower_c = key_tower.clone();
        let key_pi1_c = key_pi1.clone();
        let key_pi2_c = key_pi2.clone();
        let key_nas_c = key_nas.clone();

        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let key_tower_c = key_tower_c.clone();
            let key_pi1_c = key_pi1_c.clone();
            let key_pi2_c = key_pi2_c.clone();
            let key_nas_c = key_nas_c.clone();
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        let resp = if req.method == method::HELLO {
                            Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            )
                        } else {
                            Response::ok(
                                req.id,
                                json!({
                                    "results": {
                                        "tower": {
                                            "ok": {
                                                "node": "tower",
                                                "hosts": ["tower.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [key_tower_c],
                                            }
                                        },
                                        // Superset of pinned key_pi1 (`[key_pi1, key_pi2]`).
                                        "pi": {
                                            "ok": {
                                                "node": "pi",
                                                "hosts": ["pi.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [key_pi1_c, key_pi2_c],
                                            }
                                        },
                                        "nas": {
                                            "ok": {
                                                "node": "nas",
                                                "hosts": ["nas.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [key_nas_c],
                                            }
                                        }
                                    }
                                }),
                            )
                        };
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = server_w.flush().await;
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![
                NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_tower],
                    },
                    Some("device:pixel".to_owned()),
                ),
                NodeLink::learned(
                    "pi",
                    SshTarget {
                        hosts: vec!["pi.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_pi1],
                    },
                    None,
                    "tower",
                ),
            ],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);

        // 1. A learned node may NEVER act as an introducer.
        let err = link.learn_from("pi").await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // 2. Learning from paired `tower` accepts superset host keys on `pi` without flagging
        //    identity_changed, and adds `nas` with `principal: None` (never inheriting introducer).
        let report = link.learn().await.unwrap();
        assert!(report.identity_changed.is_empty(), "{report:?}");
        assert_eq!(report.added, vec!["nas".to_owned()]);
        assert_eq!(link.config().node("nas").unwrap().principal, None);

        // 3. Forgetting `nas` remembers it so a subsequent `learn()` does NOT re-add `nas`.
        assert!(link.forget("nas").await);
        let report2 = link.learn().await.unwrap();
        assert!(report2.added.is_empty(), "{report2:?}");
        assert!(link.config().node("nas").is_none());

        // 4. Forgetting paired `tower` also drops learned `pi` introduced by `tower`.
        assert!(link.forget("tower").await);
        assert!(link.config().node("tower").is_none());
        assert!(link.config().node("pi").is_none());
        link.close().await;
    }

    #[tokio::test]
    async fn multi_introducer_consensus_conflicts_route_hold_and_warning_persistence() {
        let key_tower = test_host_key(60);
        let key_studio = test_host_key(61);
        let key_pi1 = test_host_key(62);
        let key_pi2 = test_host_key(63);
        let key_pi3 = test_host_key(64);
        let key_nas = test_host_key(65);
        let key_rogue_a = test_host_key(66);
        let key_rogue_b = test_host_key(67);

        let answers: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let answers_c = Arc::clone(&answers);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let answers_c = Arc::clone(&answers_c);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        let resp = if req.method == method::HELLO {
                            Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: "0.1.0".into(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".into(),
                                    arch: "x86_64".into(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap(),
                            )
                        } else {
                            let body = answers_c
                                .lock()
                                .unwrap()
                                .get(&node_name)
                                .cloned()
                                .unwrap_or_else(|| json!({ "results": {} }));
                            Response::ok(req.id, body)
                        };
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = server_w.flush().await;
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let cfg = LinkConfig {
            device_key: Some("dummy".to_owned()),
            nodes: vec![
                NodeLink::ssh(
                    "tower",
                    SshTarget {
                        hosts: vec!["tower.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_tower.clone()],
                    },
                    Some("device:pixel".to_owned()),
                ),
                NodeLink::ssh(
                    "studio",
                    SshTarget {
                        hosts: vec!["studio.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_studio.clone()],
                    },
                    Some("device:pixel".to_owned()),
                ),
                NodeLink::learned(
                    "pi",
                    SshTarget {
                        hosts: vec!["pi.example.net".to_owned()],
                        port: 22,
                        user: "me".to_owned(),
                        host_keys: vec![key_pi1.clone()],
                    },
                    None,
                    "tower",
                ),
            ],
            ..LinkConfig::default()
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);

        // 1. Two reachable paired nodes: `nas` agrees -> pinned; `rogue` disagrees -> not pinned,
        //    recorded in `disagreements`; `studio` (not `pi`'s introducer) tries to rewrite `pi`'s
        //    hosts -> NOT silently overwritten, held as pending change!
        {
            let mut map = answers.lock().unwrap();
            map.insert(
                "tower".to_owned(),
                json!({
                    "results": {
                        "nas": {
                            "ok": {
                                "node": "nas",
                                "hosts": ["nas.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_nas],
                            }
                        },
                        "rogue": {
                            "ok": {
                                "node": "rogue",
                                "hosts": ["rogue.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_rogue_a],
                            }
                        },
                        "pi": {
                            "ok": {
                                "node": "pi",
                                "hosts": ["pi.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_pi1],
                            }
                        }
                    }
                }),
            );
            map.insert(
                "studio".to_owned(),
                json!({
                    "results": {
                        "nas": {
                            "ok": {
                                "node": "nas",
                                "hosts": ["nas.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_nas],
                            }
                        },
                        "rogue": {
                            "ok": {
                                "node": "rogue",
                                "hosts": ["rogue.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_rogue_b],
                            }
                        },
                        "pi": {
                            "ok": {
                                "node": "pi",
                                "hosts": ["evil-pi.example.net"],
                                "port": 2222,
                                "user": "me",
                                "host_keys": [key_pi1],
                            }
                        }
                    }
                }),
            );
        }

        let r1 = link.learn().await.unwrap();
        assert_eq!(r1.added, vec!["nas".to_owned()]);
        assert_eq!(r1.disagreements, vec!["rogue".to_owned()]);
        assert!(link.config().node("rogue").is_none());
        assert_eq!(r1.identity_changed, vec!["pi".to_owned()]);
        let pi_after_r1 = link.config().node("pi").unwrap().clone();
        assert_eq!(
            pi_after_r1.target.as_ref().unwrap().hosts,
            vec!["pi.example.net".to_owned()],
            "different introducer must not silently rewrite learned hosts"
        );
        assert_eq!(pi_after_r1.learned_from.as_deref(), Some("tower"));
        assert_eq!(
            pi_after_r1.identity_changed.as_ref().unwrap().reported_by,
            "studio"
        );

        // Dismiss the route warning before testing conflicting key reports.
        link.dismiss_identity_change("pi").unwrap();

        // 2. First report changes `pi` key to `key_pi2` from `tower`; second different report
        //    (`key_pi3` from `studio`) does NOT overwrite `key_pi2`, records a conflict, and blocks
        //    accepting!
        answers.lock().unwrap().insert(
            "tower".to_owned(),
            json!({
                "results": {
                    "pi": {
                        "ok": {
                            "node": "pi",
                            "hosts": ["pi.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_pi2],
                        }
                    }
                }
            }),
        );
        link.learn_from("tower").await.unwrap();
        let fp_pi2 = public_key_fingerprint(&key_pi2).unwrap();

        answers.lock().unwrap().insert(
            "studio".to_owned(),
            json!({
                "results": {
                    "pi": {
                        "ok": {
                            "node": "pi",
                            "hosts": ["pi.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_pi3],
                        }
                    }
                }
            }),
        );
        link.learn_from("studio").await.unwrap();
        let pi_conflict = link.config().node("pi").unwrap().clone();
        let pending = pi_conflict.identity_changed.as_ref().unwrap();
        assert_eq!(pending.target.host_keys, vec![key_pi2.clone()]);
        assert_eq!(pending.conflicts.len(), 1);
        assert_eq!(pending.conflicts[0].reported_by, "studio");
        assert_eq!(pending.conflicts[0].target.host_keys, vec![key_pi3]);

        // Accepting while reports conflict MUST fail.
        let conflict_err = link
            .accept_identity_change("pi", std::slice::from_ref(&fp_pi2))
            .unwrap_err();
        assert_eq!(conflict_err.code, ErrorCode::Conflict);

        // 3. Re-reporting old keys from ONE introducer (`tower`) does NOT clear the warning.
        answers.lock().unwrap().insert(
            "tower".to_owned(),
            json!({
                "results": {
                    "pi": {
                        "ok": {
                            "node": "pi",
                            "hosts": ["pi.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_pi1],
                        }
                    }
                }
            }),
        );
        link.learn_from("tower").await.unwrap();
        assert!(
            link.config().node("pi").unwrap().identity_changed.is_some(),
            "single introducer re-reporting old keys must not clear pending warning"
        );

        // 4. When BOTH reachable paired introducers agree on the pinned keys and route, the
        //    warning clears.
        answers.lock().unwrap().insert(
            "studio".to_owned(),
            json!({
                "results": {
                    "pi": {
                        "ok": {
                            "node": "pi",
                            "hosts": ["pi.example.net"],
                            "port": 22,
                            "user": "me",
                            "host_keys": [key_pi1],
                        }
                    }
                }
            }),
        );
        link.learn().await.unwrap();
        assert!(
            link.config().node("pi").unwrap().identity_changed.is_none(),
            "consensus across all reachable paired introducers clears pending warning"
        );

        link.close().await;
    }

    #[cfg(feature = "ssh")]
    #[test]
    fn pure_sha256_public_key_fingerprint_matches_russh() {
        let key = crate::key::DeviceKey::from_seed([77u8; 32]);
        let line = key.public_key_line("host").unwrap();
        assert_eq!(public_key_fingerprint(&line), Some(key.fingerprint()));
    }

    #[test]
    fn endpoint_host_filter_rejects_numeric_and_ipv4_compatible_bypasses() {
        // Every non-canonical numeric/hex/octal/short IPv4 form and disallowed IPv6 form MUST be
        // rejected when allow_loopback is false.
        let rejected_default = [
            "2130706433",
            "0x7f.1",
            "0x7f000001",
            "0X7F000001",
            "127.1",
            "127.0.1",
            "0177.0.0.1",
            "0177.1",
            "0x7f.0.0.1",
            "0",
            "0.0",
            "0x0",
            "00",
            "foo.1",
            "bar.0x1",
            "::127.0.0.1",
            "[::127.0.0.1]",
            "::0:7f00:1",
            "::ffff:127.0.0.1",
            "[::ffff:127.0.0.1]",
            "::0.0.0.0",
            "::169.254.169.254",
            "::224.0.0.1",
            "::255.255.255.255",
            "::ffff:169.254.1.1",
            "::ffff:0.0.0.0",
            "::ffff:224.0.0.1",
            "::ffff:255.255.255.255",
            "127.0.0.1",
            "::1",
            "localhost",
            "sub.localhost",
            "0.0.0.0",
            "::",
            "169.254.169.254",
            "fe80::1",
            "fe80::1%eth0",
            "224.0.0.1",
            "ff02::1",
            "255.255.255.255",
        ];
        for host in rejected_default {
            assert!(
                !is_allowed_endpoint_host(host, false),
                "expected host {host:?} to be rejected with allow_loopback=false"
            );
        }

        // Non-canonical numeric forms are also rejected even when allow_loopback=true.
        for host in [
            "2130706433",
            "0x7f.1",
            "0x7f000001",
            "127.1",
            "0177.0.0.1",
            "0",
        ] {
            assert!(
                !is_allowed_endpoint_host(host, true),
                "expected non-canonical numeric host {host:?} to be rejected even with allow_loopback=true"
            );
        }

        // Legitimate canonical loopback works ONLY when allow_loopback=true.
        for host in [
            "127.0.0.1",
            "::1",
            "localhost",
            "::127.0.0.1",
            "::ffff:127.0.0.1",
        ] {
            assert!(
                is_allowed_endpoint_host(host, true),
                "expected loopback host {host:?} to be allowed with allow_loopback=true"
            );
        }

        // Legitimate hosts and addresses are allowed.
        for host in [
            "10.0.2.2",
            "100.64.0.1",
            "192.0.2.10",
            "2001:db8::1",
            "[2001:db8::1]",
            "tower",
            "pi",
            "node-1",
            "tower.example.net",
            "node1.example.net.",
        ] {
            assert!(
                is_allowed_endpoint_host(host, false),
                "expected legitimate host {host:?} to be allowed"
            );
        }
    }

    #[tokio::test]
    async fn forget_clears_or_promotes_pending_identity_changes_and_conflicts_reported_by_node() {
        let key_a = test_host_key(101);
        let key_b = test_host_key(102);
        let key_c = test_host_key(103);
        let key_c_new = test_host_key(104);
        let key_d = test_host_key(105);
        let key_d_b = test_host_key(106);
        let key_d_e = test_host_key(107);
        let key_e = test_host_key(108);
        let key_f = test_host_key(109);
        let key_f_e = test_host_key(110);
        let key_f_b = test_host_key(111);

        let target_for = |host: &str, key: &str| SshTarget {
            hosts: vec![host.to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![key.to_owned()],
        };

        let mut node_a = NodeLink::ssh("a", target_for("a.example.net", &key_a), None);
        node_a.identity_changed = Some(PendingIdentityChange {
            target: target_for("a-evil.example.net", &key_c_new),
            reported_by: "b".to_owned(),
            conflicts: Vec::new(),
        });

        let node_b = NodeLink::ssh("b", target_for("b.example.net", &key_b), None);
        let node_e = NodeLink::ssh("e", target_for("e.example.net", &key_e), None);

        let mut node_c = NodeLink::learned("c", target_for("c.example.net", &key_c), None, "a");
        node_c.identity_changed = Some(PendingIdentityChange {
            target: target_for("c-new.example.net", &key_c_new),
            reported_by: "b".to_owned(),
            conflicts: Vec::new(),
        });

        let mut node_d = NodeLink::learned("d", target_for("d.example.net", &key_d), None, "a");
        node_d.identity_changed = Some(PendingIdentityChange {
            target: target_for("d-from-b.example.net", &key_d_b),
            reported_by: "b".to_owned(),
            conflicts: vec![ConflictingIdentityReport {
                target: target_for("d-from-e.example.net", &key_d_e),
                reported_by: "e".to_owned(),
            }],
        });

        let mut node_f = NodeLink::learned("f", target_for("f.example.net", &key_f), None, "a");
        node_f.identity_changed = Some(PendingIdentityChange {
            target: target_for("f-from-e.example.net", &key_f_e),
            reported_by: "e".to_owned(),
            conflicts: vec![ConflictingIdentityReport {
                target: target_for("f-from-b.example.net", &key_f_b),
                reported_by: "b".to_owned(),
            }],
        });

        let link = Link::new(LinkConfig {
            device_key: None,
            nodes: vec![node_a, node_b, node_e, node_c, node_d, node_f],
            forgotten_nodes: Vec::new(),
        });

        link.forget("b").await;
        let cfg = link.config();

        // 1. Single-reporter pending changes on both paired (`a`) and learned (`c`) are cleared.
        assert!(
            cfg.node("a").unwrap().identity_changed.is_none(),
            "forgetting b must clear informational warning b reported on paired node a"
        );
        assert!(
            cfg.node("c").unwrap().identity_changed.is_none(),
            "forgetting b must clear pending identity change b reported on learned node c"
        );
        let fp_c_new = public_key_fingerprint(&key_c_new).unwrap();
        let err = link
            .accept_identity_change("c", std::slice::from_ref(&fp_c_new))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        // 2. When b was primary reporter on `d` and `e` was a conflict, `e` is promoted cleanly.
        let d_pending = cfg
            .node("d")
            .unwrap()
            .identity_changed
            .as_ref()
            .expect("e's remaining report on d should be promoted");
        assert_eq!(d_pending.reported_by, "e");
        assert_eq!(d_pending.target.hosts, vec!["d-from-e.example.net"]);
        assert!(d_pending.conflicts.is_empty());

        // 3. When b was in `conflicts` on `f`, b's conflict is removed leaving `e` clean.
        let f_pending = cfg
            .node("f")
            .unwrap()
            .identity_changed
            .as_ref()
            .expect("e's primary report on f should stay");
        assert_eq!(f_pending.reported_by, "e");
        assert!(f_pending.conflicts.is_empty());

        // 4. Route status for `c` no longer reports IdentityChanged from `b`.
        let routes = link.route_status();
        assert_eq!(routes.get("c"), Some(&MachineRouteStatus::Direct));
        assert_eq!(
            routes.get("d"),
            Some(&MachineRouteStatus::IdentityChanged {
                reported_by: "e".to_owned()
            })
        );
    }

    #[tokio::test]
    async fn partial_reachability_abstention_learns_while_differing_reports_disagree() {
        let key_tower = test_host_key(120);
        let key_pi = test_host_key(121);
        let key_edge = test_host_key(122);
        let key_studio_a = test_host_key(123);
        let key_studio_b = test_host_key(124);

        let answers: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));
        let answers_c = Arc::clone(&answers);
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let answers_c = Arc::clone(&answers_c);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap();
                        let resp = if req.method == method::HELLO {
                            Response::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
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
                            )
                        } else {
                            let body = answers_c
                                .lock()
                                .unwrap()
                                .get(&node_name)
                                .cloned()
                                .unwrap_or_else(|| json!({ "results": {} }));
                            Response::ok(req.id, body)
                        };
                        let _ = server_w
                            .write_all(
                                format!("{}\n", serde_json::to_string(&resp).unwrap()).as_bytes(),
                            )
                            .await;
                        let _ = server_w.flush().await;
                    }
                });
                Session::open_io(
                    client_r,
                    client_w,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        {
            let mut map = answers.lock().unwrap();
            map.insert(
                "tower".to_owned(),
                json!({
                    "results": {
                        "edge": {
                            "ok": {
                                "node": "edge",
                                "hosts": ["edge.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_edge],
                            }
                        },
                        "studio": {
                            "ok": {
                                "node": "studio",
                                "hosts": ["studio.example.net"],
                                "port": 22,
                                "user": "me",
                                "host_keys": [key_studio_a],
                            }
                        }
                    }
                }),
            );
            map.insert(
                "pi".to_owned(),
                json!({
                    "results": {
                        // pi cannot reach edge: honest abstention (error), NOT a disagreement!
                        "edge": {
                            "error": {
                                "code": "unreachable",
                                "message": "edge did not answer."
                            }
                        },
                        // pi returns a differing report for studio: REAL disagreement!
                        "studio": {
                            "ok": {
                                "node": "studio",
                                "hosts": ["studio-other.example.net"],
                                "port": 2222,
                                "user": "me",
                                "host_keys": [key_studio_b],
                            }
                        }
                    }
                }),
            );
        }

        let link = Link::with_opener(
            LinkConfig {
                device_key: Some("dummy".to_owned()),
                nodes: vec![
                    NodeLink::ssh(
                        "tower",
                        SshTarget {
                            hosts: vec!["tower.example.net".to_owned()],
                            port: 22,
                            user: "me".to_owned(),
                            host_keys: vec![key_tower],
                        },
                        Some("device:pixel".to_owned()),
                    ),
                    NodeLink::ssh(
                        "pi",
                        SshTarget {
                            hosts: vec!["pi.example.net".to_owned()],
                            port: 22,
                            user: "me".to_owned(),
                            host_keys: vec![key_pi],
                        },
                        Some("device:pixel".to_owned()),
                    ),
                ],
                forgotten_nodes: Vec::new(),
            },
            LinkOptions::default(),
            opener,
        );

        let report = link.learn().await.unwrap();
        // edge was reported by tower and abstained on by pi -> learned cleanly!
        assert_eq!(report.added, vec!["edge".to_owned()]);
        assert!(link.config().node("edge").is_some());
        assert_eq!(
            link.config().node("edge").unwrap().learned_from.as_deref(),
            Some("tower")
        );
        // studio had two differing reports -> NOT pinned, recorded as disagreement with details!
        assert_eq!(report.disagreements, vec!["studio".to_owned()]);
        assert_eq!(report.disagreement_details.len(), 1);
        assert_eq!(report.disagreement_details[0].machine, "studio");
        assert_eq!(report.disagreement_details[0].reports.len(), 2);
        assert_eq!(report.disagreement_details[0].reports[0].reported_by, "pi");
        assert_eq!(
            report.disagreement_details[0].reports[1].reported_by,
            "tower"
        );
        assert!(link.config().node("studio").is_none());
        link.close().await;
    }
}
