//! High-level JSON-in, JSON-out [`Link`] facade held by a GUI shell or controller app.
//!
//! [`Link`] holds any number of paired (or local) nodes from a serializable [`LinkConfig`], opens
//! sessions lazily, reuses live sessions, reconnects once when a session died before sending a
//! request, cancels in-flight calls by caller-chosen call id, and closes idle sessions.

use std::collections::HashMap;
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
}

impl NodeLink {
    /// Creates an SSH node entry.
    pub fn ssh(node: impl Into<String>, target: SshTarget, principal: Option<String>) -> Self {
        NodeLink {
            node: node.into(),
            target: Some(target),
            local_program: None,
            principal,
        }
    }

    /// Creates a local desktop node entry backed by spawning `<program> rpc`.
    pub fn local(node: impl Into<String>, program: impl Into<String>) -> Self {
        NodeLink {
            node: node.into(),
            target: None,
            local_program: Some(program.into()),
            principal: None,
        }
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
}

impl Default for LinkOptions {
    fn default() -> Self {
        LinkOptions {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            ssh_timeouts: SshTimeouts::default(),
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
    slots: Mutex<HashMap<String, Arc<NodeSlot>>>,
    /// Sessions displaced by a config update while callers may still hold clones; reaped by
    /// `close_idle` once no caller holds a reference or in-flight call.
    retired: Mutex<Vec<LiveSession>>,
    active_calls: Mutex<HashMap<String, CancellationToken>>,
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
                slots: Mutex::new(HashMap::new()),
                retired: Mutex::new(Vec::new()),
                active_calls: Mutex::new(HashMap::new()),
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

    /// Adds or updates a node entry in the live config. If its connection details changed, any
    /// existing session to that node will be replaced on next use.
    pub fn upsert_node(&self, entry: NodeLink) {
        self.inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .upsert_node(entry);
    }

    /// Removes a node from the config and closes any live session to it.
    pub async fn remove_node(&self, node: &str) -> bool {
        let removed = self
            .inner
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove_node(node);
        self.evict(node).await;
        removed
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

    /// Cancels an active call registered via [`Link::call_with_id`], returning `true` if the call
    /// was found and signalled.
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
        let args = match arguments {
            Value::Null => json!({}),
            Value::Object(_) => arguments,
            _ => {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "Tool arguments must be a JSON object.",
                )
                .with_hint("Pass a JSON object such as `{}`.")
                .with_machine(node));
            }
        };
        let target = parse_optional_target(on)?;
        let cancel = CancellationToken::new();
        let _active_guard = if let Some(id) = call_id {
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
            .run_call_with_events(&live.session, params.clone(), cancel.clone(), &mut events)
            .await
        {
            Ok(value) => Ok(value),
            Err(CallError::NotWritten(_)) => {
                self.invalidate_if_same(node, &live.session);
                let fresh = self.get_or_open(node, cancel.clone()).await?;
                let _flight2 = fresh.acquire_flight();
                let mut retry_params = params;
                retry_params.as_principal = fresh.config_snapshot.principal.clone();
                self.run_call_with_events(&fresh.session, retry_params, cancel, &mut events)
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

    async fn evict(&self, node: &str) {
        let slot = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(node);
        if let Some(slot) = slot {
            let live = slot
                .current
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(live) = live {
                live.session.close().await;
            }
        }
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
            CallError::NotWritten(body) | CallError::LostInFlight(body) => {
                self.invalidate_if_same(node, session);
                body
            }
            CallError::Remote(body) => body,
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
                && live.config_snapshot == entry
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
                && live.config_snapshot == entry
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

        let session = (self.inner.opener)(entry.clone(), device_key, cancel).await?;
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
            if Arc::strong_count(&live.session) == 1
                && live.in_flight.load(Ordering::Acquire) == 0
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
                _target,
                &key,
                crate::command::Subcommand::Rpc,
                options.ssh_timeouts,
                cancel,
            )
            .await
            .map_err(|err| err.with_machine(&entry.node));
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
                Session::open_io(client_r, client_w, &entry.node, None, Duration::from_secs(5), cancel)
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
            .call("tower", "fs.remove", json!({"path": "/tmp/x"}), None, |_| {})
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
        assert!(!rendered.contains("SUPER-SECRET-KEY-MATERIAL"), "{rendered}");
        assert!(!rendered.contains("BEGIN OPENSSH"), "{rendered}");

        let empty = LinkConfig::default();
        assert!(format!("{empty:?}").contains("device_key: None"));
    }
}
