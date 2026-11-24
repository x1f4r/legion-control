//! High-level JSON-in, JSON-out [`Link`] facade held by a GUI shell or controller app.
//!
//! [`Link`] holds any number of paired (or local) nodes from a serializable [`LinkConfig`], opens
//! sessions lazily, reuses live sessions, reconnects once when a session died before sending a
//! request, cancels in-flight calls by caller-chosen call id, and closes idle sessions.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_proto::{CallParams, ErrorBody, ErrorCode, Event, EventSink, Target};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::invite::{Enrolled, SshTarget, SshTimeouts};
use crate::session::{CallError, Session};

/// Default idle duration after which unused sessions are closed (matches `legion serve`).
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Default duration to remember that a learned node has not received this device yet (falling back
/// to a relay without retrying direct SSH auth on every call).
pub const DEFAULT_UNSYNCED_BACKOFF: Duration = Duration::from_secs(5 * 60);

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
}

impl fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkConfig")
            .field(
                "device_key",
                &self.device_key.as_ref().map(|_| "<redacted>"),
            )
            .field("nodes", &self.nodes)
            .finish()
    }
}

impl LinkConfig {
    /// Adds or replaces a node entry by name.
    pub fn upsert_node(&mut self, entry: NodeLink) {
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

/// Pending host-key change reported by `device.endpoint` for an already known node.
///
/// Old pinned host keys remain active in [`NodeLink::target`] until the caller explicitly accepts
/// the change via [`Link::accept_identity_change`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingIdentityChange {
    /// New SSH connection target (carrying the newly reported host keys).
    pub target: SshTarget,
    /// Trusted node whose authenticated session reported the changed host keys.
    pub reported_by: String,
}

/// Summary returned by [`Link::learn`] and [`Link::learn_from`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LearnReport {
    /// Trusted node whose authenticated session answered `device.endpoint`.
    pub introducer: String,
    /// Newly learned machines added to [`LinkConfig`].
    pub added: Vec<String>,
    /// Existing learned machines whose non-pin connection details (hosts/port/user) changed.
    pub updated: Vec<String>,
    /// Already known machines whose reported host keys differed from their saved pins; old pins
    /// were kept and the entries were marked with [`NodeLink::identity_changed`].
    pub identity_changed: Vec<String>,
    /// Machines that returned an error during `device.endpoint` fan-out.
    pub unreachable: Vec<String>,
    /// Machines whose `device.endpoint` reply failed validation (for example mismatched node name
    /// or malformed host keys).
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
}

impl Default for LinkOptions {
    fn default() -> Self {
        LinkOptions {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            ssh_timeouts: SshTimeouts::default(),
            unsynced_backoff: DEFAULT_UNSYNCED_BACKOFF,
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
    /// Serializes `pair` calls so concurrent first-time pairings cannot generate two device keys.
    #[cfg(feature = "ssh")]
    pair_lock: tokio::sync::Mutex<()>,
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
}

impl LiveSession {
    fn touch(&self) {
        *self
            .last_used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Instant::now();
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
                #[cfg(feature = "ssh")]
                pair_lock: tokio::sync::Mutex::new(()),
                slots: Mutex::new(HashMap::new()),
                retired: Mutex::new(Vec::new()),
                active_calls: Mutex::new(HashMap::new()),
                unsynced_until: Mutex::new(HashMap::new()),
                routes: Mutex::new(HashMap::new()),
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
        self.pair_with(
            invite_text,
            device_name,
            |invite, key, name, timeouts, cancel| async move {
                crate::invite::enrol(&invite, &key, &name, timeouts, cancel).await
            },
        )
        .await
    }

    #[cfg(feature = "ssh")]
    async fn pair_with<F, Fut>(
        &self,
        invite_text: &str,
        device_name: &str,
        enrol_fn: F,
    ) -> Result<Enrolled, ErrorBody>
    where
        F: FnOnce(
            crate::invite::Invite,
            crate::key::DeviceKey,
            String,
            SshTimeouts,
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

        let _pair_guard = self.inner.pair_lock.lock().await;

        let existing_key = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .device_key
            .clone();
        let (device_key, generated_pem) = match existing_key {
            Some(pem) => (crate::key::DeviceKey::from_openssh(&pem)?, None),
            None => {
                let key = crate::key::DeviceKey::generate()?;
                let pem = key.to_openssh()?;
                (key, Some(pem))
            }
        };

        let enrolled = enrol_fn(
            invite,
            device_key,
            name.to_owned(),
            self.inner.options.ssh_timeouts,
            CancellationToken::new(),
        )
        .await?;

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

    /// Closes any live or retired sessions for `node` and removes it from [`LinkConfig`],
    /// returning `true` if the node was configured.
    ///
    /// Keeps the device key intact so other paired nodes continue working and future pairings
    /// reuse the same device identity.
    pub async fn forget(&self, node: &str) -> bool {
        let removed = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove_node(node);
        self.clear_route_state(node);
        self.evict(node).await;
        removed
    }

    /// Removes a node from the config and closes any live session to it (equivalent to
    /// [`Link::forget`]).
    pub async fn remove_node(&self, node: &str) -> bool {
        self.forget(node).await
    }

    /// Learns direct SSH endpoints for every answering machine in the fleet by calling
    /// `device.endpoint` with `on: "all"` through the first configured node that answers.
    ///
    /// Rules enforced before updating [`LinkConfig`]:
    /// - Answers are accepted only from an authenticated session to an already configured node
    ///   (the introducer).
    /// - Each answer's `node` field must match the machine key it is filed under.
    /// - A learned entry never replaces the pinned host keys (or target) of a directly paired node.
    /// - If a later answer reports different host keys for an already known machine, the old pins
    ///   are kept and the entry is marked with [`NodeLink::identity_changed`].
    pub async fn learn(&self) -> Result<LearnReport, ErrorBody> {
        let candidates = self.ordered_candidates(None);
        if candidates.is_empty() {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                "No configured nodes are available to learn fleet endpoints from.",
            )
            .with_hint("Pair with at least one machine first."));
        }
        let mut last_err: Option<ErrorBody> = None;
        for candidate in candidates {
            match self.learn_from(&candidate).await {
                Ok(report) => return Ok(report),
                Err(err) => {
                    let retry = is_unsent_connect_error(&err)
                        || (self.is_learned(&candidate)
                            && is_unsent(&err)
                            && is_device_removed_or_unknown(&err));
                    last_err = Some(err);
                    if !retry {
                        break;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            ErrorBody::new(
                ErrorCode::Unreachable,
                "Could not reach any configured node to learn fleet endpoints.",
            )
        }))
    }

    /// Calls `device.endpoint` with `on: "all"` over an authenticated session to `introducer` and
    /// applies the learned endpoints to [`LinkConfig`].
    pub async fn learn_from(&self, introducer: &str) -> Result<LearnReport, ErrorBody> {
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
        Ok(self.apply_learned_endpoints(introducer, value))
    }

    /// Explicitly accepts a pending host-key change recorded on `node`, replacing its pinned
    /// [`NodeLink::target`] with the new target and retiring any cached session so the next call
    /// connects using the accepted keys.
    pub fn accept_identity_change(&self, node: &str) -> Result<NodeLink, ErrorBody> {
        let updated = {
            let mut cfg = self
                .inner
                .config
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let entry = cfg
                .nodes
                .iter_mut()
                .find(|n| n.node == node)
                .ok_or_else(|| {
                    ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("No node named {node:?} is configured."),
                    )
                })?;
            let pending = entry.identity_changed.take().ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Node {node:?} does not have a pending identity change."),
                )
            })?;
            entry.target = Some(pending.target);
            entry.clone()
        };
        self.retire_node(node);
        Ok(updated)
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

    fn apply_learned_endpoints(&self, introducer: &str, value: Value) -> LearnReport {
        let mut report = LearnReport {
            introducer: introducer.to_owned(),
            ..LearnReport::default()
        };

        let entries: Vec<(String, Value)> =
            if let Some(results) = value.get("results").and_then(Value::as_object) {
                results
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            } else if value.get("node").is_some() {
                vec![(introducer.to_owned(), json!({ "ok": value }))]
            } else {
                return report;
            };

        let mut validated: Vec<(String, SshTarget)> = Vec::new();
        for (machine_key, item) in entries {
            if item.get("error").is_some() {
                report.unreachable.push(machine_key);
                continue;
            }
            let Some(ok_val) = item.get("ok") else {
                report.rejected.push(machine_key);
                continue;
            };
            match parse_and_validate_endpoint(&machine_key, ok_val) {
                Ok(target) => validated.push((machine_key, target)),
                Err(()) => report.rejected.push(machine_key),
            }
        }

        let mut cfg = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let introducer_principal = cfg.node(introducer).and_then(|n| n.principal.clone());

        let mut batch_key_owners: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (machine, new_target) in &validated {
            for key in canonical_host_keys(&new_target.host_keys) {
                batch_key_owners
                    .entry(key)
                    .or_default()
                    .insert(machine.clone());
            }
        }

        for (machine, new_target) in validated {
            let new_keys = canonical_host_keys(&new_target.host_keys);
            let overlaps_other_configured = cfg.nodes.iter().any(|other| {
                other.node != machine
                    && other
                        .target
                        .as_ref()
                        .is_some_and(|t| !canonical_host_keys(&t.host_keys).is_disjoint(&new_keys))
            });
            let overlaps_batch = new_keys.iter().any(|k| {
                batch_key_owners
                    .get(k)
                    .is_some_and(|owners| owners.len() > 1)
            });
            if overlaps_other_configured || (overlaps_batch && cfg.node(&machine).is_none()) {
                report.rejected.push(machine);
                continue;
            }

            if let Some(existing) = cfg.nodes.iter_mut().find(|n| n.node == machine) {
                let Some(old_target) = &mut existing.target else {
                    // Local desktop pipe (`target: None`): never overwrite with an SSH target.
                    continue;
                };
                if canonical_host_keys(&old_target.host_keys)
                    != canonical_host_keys(&new_target.host_keys)
                {
                    // Keep the old pins and mark the entry as identity-changed until accepted.
                    existing.identity_changed = Some(PendingIdentityChange {
                        target: new_target,
                        reported_by: introducer.to_owned(),
                    });
                    report.identity_changed.push(machine);
                    continue;
                }
                existing.identity_changed = None;
                if existing.origin == NodeOrigin::Paired {
                    // A learned entry never replaces the pins or target of a directly paired node.
                    continue;
                }
                let changed = old_target.hosts != new_target.hosts
                    || old_target.port != new_target.port
                    || old_target.user != new_target.user
                    || existing.learned_from.as_deref() != Some(introducer);
                old_target.hosts = new_target.hosts;
                old_target.port = new_target.port;
                old_target.user = new_target.user;
                existing.learned_from = Some(introducer.to_owned());
                if changed {
                    report.updated.push(machine);
                }
            } else {
                cfg.nodes.push(NodeLink::learned(
                    machine.clone(),
                    new_target,
                    introducer_principal.clone(),
                    introducer,
                ));
                report.added.push(machine);
            }
        }

        report.added.sort();
        report.updated.sort();
        report.identity_changed.sort();
        report.unreachable.sort();
        report.rejected.sort();
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
        let ordered = self.ordered_candidates(preferred_introducer);
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
            return crate::ssh::open(
                &entry.node,
                _target,
                &key,
                crate::command::Subcommand::Rpc,
                options.ssh_timeouts,
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

fn parse_and_validate_endpoint(expected_machine: &str, value: &Value) -> Result<SshTarget, ()> {
    let ep: EndpointAnswer = serde_json::from_value(value.clone()).map_err(|_| ())?;
    if ep.node != expected_machine {
        return Err(());
    }
    crate::invite::validate_token_field("node", &ep.node).map_err(|_| ())?;
    crate::invite::validate_token_field("user", &ep.user).map_err(|_| ())?;
    if ep.port == 0 || ep.hosts.is_empty() || ep.hosts.len() > 32 {
        return Err(());
    }
    for host in &ep.hosts {
        crate::invite::validate_token_field("hosts", host).map_err(|_| ())?;
    }
    if ep.host_keys.is_empty() || ep.host_keys.len() > 32 {
        return Err(());
    }
    for (idx, line) in ep.host_keys.iter().enumerate() {
        crate::invite::validate_openssh_public_key_line(idx, line).map_err(|_| ())?;
    }
    Ok(SshTarget {
        hosts: ep.hosts,
        port: ep.port,
        user: ep.user,
        host_keys: ep.host_keys,
    })
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
                timeouts.handshake,
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

        // A failed first pairing must leave LinkConfig untouched.
        let fail_err = link
            .pair_with(
                &make_invite("tower", "inv-bad"),
                "phone",
                |_, _, _, _, _| async { Err(ErrorBody::new(ErrorCode::Denied, "Invite expired.")) },
            )
            .await
            .unwrap_err();
        assert_eq!(fail_err.code, ErrorCode::Denied);
        assert_eq!(link.config(), LinkConfig::default());

        // First successful pairing generates a fresh device key and adds `tower`.
        let enrolled_tower = link
            .pair_with(&make_invite("tower", "inv-1"), "phone", mock_enrol)
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
            .pair_with(&make_invite("pi", "inv-2"), "phone", mock_enrol)
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
        };
        let broken_link = Link::new(broken_cfg.clone());
        let bad_key_err = broken_link
            .pair_with(&make_invite("pi", "inv-3"), "phone", mock_enrol)
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
        assert_eq!(pi1.principal.as_deref(), Some("device:pixel"));
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

        // 4. Accepting the identity change promotes the new pins and clears the flag.
        let accepted = link.accept_identity_change("pi").unwrap();
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
        let again = link.accept_identity_change("pi").unwrap_err();
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
                            let resp = if node_name == "pi" {
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
        };
        let link = Link::with_opener(cfg, LinkOptions::default(), opener);
        let report = link.learn().await.unwrap();
        assert!(report.rejected.contains(&"spoof".to_owned()));
        assert!(report.added.is_empty());
        assert!(link.config().node("spoof").is_none());
        link.close().await;
    }
}
