//! Real [`Backend`] implementation backed by `legion-link`.
//!
//! Manages paired SSH nodes (on every platform) and an implicit local child-process node (`legion
//! rpc`) on desktops when a `legion` binary is installed. State is persisted atomically in
//! [`AppStore`] without ever logging keys or invites.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use legion_link::proto::Target;
use legion_link::{
    CancellationToken, DeviceKey, Invite, Link, LinkConfig, LinkOptions, MachineRouteStatus,
    NodeLink as LinkNode, NodeOrigin, parse_optional_target, public_key_fingerprint,
};
use serde_json::Value;
use tauri::ipc::Channel;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::backend::{
    AppError, AppState, Backend, BoxFuture, CallId, CallRequest, CancelResult,
    ConflictingIdentityInfo, ContentTarget, DisagreementEntryView, DisagreementView, ErrorCode,
    IdentityChangedInfo, LearnRejectionView, LearnResult, LearnedDirectIdentity, MachineRouteView,
    Needs, NodeLink, NodeRouteIdentity, PairPendingPeer, PairSummary, RepairInfo, RepairResult,
    TransferCapabilities, UnpairResult,
};
use crate::commands::default_device_name;
use crate::store::{AppStore, AppStoreData, StoredNodeMeta};

/// Internal node key used for the implicit desktop local pipe (`legion rpc`).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOCAL_NODE_ID: &str = "local";

/// How long an observed answering relay stays preferred for door-level discovery reads.
const PREFERRED_RELAY_TTL: Duration = Duration::from_secs(5 * 60);

/// Door-level discovery reads that genuinely mean "whichever node answers" when `on` is omitted.
///
/// Every tool that acts on or reads from a specific machine (including `meta.whoami`,
/// `meta.version`, `device.invite`, `device.list`, `device.remove`, `device.set`, `config.read`,
/// etc.) must name its machine in `on`.
fn is_any_node_tool(tool: &str) -> bool {
    matches!(tool, "fleet.list" | "fleet.ping" | "device.endpoint")
}

/// Read-only background refresh tools that match `PASSIVE_REFRESH_TOOLS` in `apps/ui/app.js`.
///
/// Non-streaming calls to these tools (such as the Machine Overview 5-second `sys.info` /
/// `sys.procs` poll) do not count as user-started active work blocking automatic updates, and are
/// instead tracked separately so an update waits briefly for any mid-call refresh to finish before
/// installing between ticks. Every other tool (commands, process stop, power actions, filesystem
/// mutations, device/config/update mutations, unknown tools) and every streaming call counts as
/// user-started work that blocks updates.
pub fn is_background_refresh_tool(tool: &str) -> bool {
    matches!(
        tool,
        "fleet.list"
            | "fleet.ping"
            | "sys.info"
            | "sys.procs"
            | "power.status"
            | "job.list"
            | "meta.whoami"
            | "meta.version"
            | "fs.list"
            | "fs.stat"
            | "fs.read"
            | "fs.search"
            | "fav.list"
            | "sync.list"
    )
}

/// Returns `true` only when a tool invocation is a non-streaming passive background refresh.
pub fn is_background_refresh_call(tool: &str, has_stream: bool) -> bool {
    !has_stream && is_background_refresh_tool(tool)
}

fn now_rfc3339_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let sod = secs % 86_400;
    let hour = sod / 3_600;
    let minute = (sod % 3_600) / 60;
    let second = sod % 60;
    // Civil date from Unix epoch days (Howard Hinnant algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn label_discovery_result(tool: &str, answering: &str, mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "answered_by".to_owned(),
            Value::String(answering.to_owned()),
        );
        if tool == "fleet.list" && !obj.contains_key("node") {
            obj.insert("node".to_owned(), Value::String(answering.to_owned()));
        }
    }
    value
}

/// Returns `true` only in debug builds when the loopback test switch is explicitly enabled (either
/// via `LEGION_APP_ALLOW_LOOPBACK_ENDPOINTS=1` or an `allow-loopback-endpoints` marker file in the
/// app data directory). Always returns `false` in release builds.
fn allow_loopback_endpoints_for_store(store: &AppStore) -> bool {
    #[cfg(debug_assertions)]
    {
        if std::env::var_os("LEGION_APP_ALLOW_LOOPBACK_ENDPOINTS")
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        {
            return true;
        }
        if let Some(dir) = store.path().parent()
            && dir.join("allow-loopback-endpoints").exists()
        {
            return true;
        }
        false
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = store;
        false
    }
}

/// RAII guard tracking one in-flight user-started or state-mutating native operation (effectful or
/// streaming tool call, pairing, unpairing, learning, or identity change) so automatic updates
/// never restart mid-operation.
pub struct OperationGuard(Arc<AtomicUsize>);

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wake callback invoked by a [`DrainClock`] when a synchronous refresh-drain wait expires.
pub(crate) type SyncWakeFn = Arc<dyn Fn() + Send + Sync>;

/// Monotonic clock and wait abstraction for [`BackgroundRefreshTracker`] so unit tests can drive
/// refresh-drain timeouts and ordering deterministically without wall-clock sleeps.
pub(crate) trait DrainClock: Send + Sync + 'static {
    fn now(&self) -> Duration;
    fn sleep_until(&self, deadline: Duration) -> BoxFuture<'static, ()>;
    fn wait_condvar<'a>(
        &self,
        condvar: &Condvar,
        guard: std::sync::MutexGuard<'a, usize>,
        deadline: Duration,
        wake: SyncWakeFn,
    ) -> std::sync::MutexGuard<'a, usize>;
}

/// Production [`DrainClock`] backed by `std::time::Instant`, `Condvar::wait_timeout`, and
/// `tokio::time::sleep_until`.
pub(crate) struct RealDrainClock {
    origin: Instant,
}

impl RealDrainClock {
    pub(crate) fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl DrainClock for RealDrainClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn sleep_until(&self, deadline: Duration) -> BoxFuture<'static, ()> {
        let target = tokio::time::Instant::from_std(self.origin + deadline);
        Box::pin(tokio::time::sleep_until(target))
    }

    fn wait_condvar<'a>(
        &self,
        condvar: &Condvar,
        guard: std::sync::MutexGuard<'a, usize>,
        deadline: Duration,
        _wake: SyncWakeFn,
    ) -> std::sync::MutexGuard<'a, usize> {
        let remaining = deadline.saturating_sub(self.now());
        if remaining.is_zero() {
            return guard;
        }
        condvar
            .wait_timeout(guard, remaining)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
    }
}

#[cfg(test)]
struct ManualDrainClockState {
    now: Duration,
    next_sync_id: u64,
    async_waiters: Vec<(Duration, tokio::sync::oneshot::Sender<()>)>,
    sync_waiters: Vec<(u64, Duration, SyncWakeFn)>,
}

/// Deterministic [`DrainClock`] for tests: virtual time advances only via [`Self::advance`], and
/// [`Self::wait_for_waiter`] lets a test observe that an async or sync drain waiter has entered its
/// wait before releasing an in-flight refresh or advancing past the timeout.
#[cfg(test)]
pub(crate) struct ManualDrainClock {
    state: Mutex<ManualDrainClockState>,
    waiter_added: Notify,
}

#[cfg(test)]
impl ManualDrainClock {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ManualDrainClockState {
                now: Duration::ZERO,
                next_sync_id: 1,
                async_waiters: Vec::new(),
                sync_waiters: Vec::new(),
            }),
            waiter_added: Notify::new(),
        })
    }

    pub(crate) fn advance(&self, delta: Duration) {
        let (ready_async, ready_sync) = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.now = guard.now.saturating_add(delta);
            let now = guard.now;
            let mut remaining_async = Vec::new();
            let mut ready_async = Vec::new();
            for (deadline, tx) in guard.async_waiters.drain(..) {
                if tx.is_closed() {
                    continue;
                }
                if deadline <= now {
                    ready_async.push(tx);
                } else {
                    remaining_async.push((deadline, tx));
                }
            }
            guard.async_waiters = remaining_async;

            let mut remaining_sync = Vec::new();
            let mut ready_sync = Vec::new();
            for (id, deadline, wake) in guard.sync_waiters.drain(..) {
                if deadline <= now {
                    ready_sync.push(wake);
                } else {
                    remaining_sync.push((id, deadline, wake));
                }
            }
            guard.sync_waiters = remaining_sync;
            (ready_async, ready_sync)
        };
        for tx in ready_async {
            let _ = tx.send(());
        }
        for wake in ready_sync {
            wake();
        }
    }

    pub(crate) fn pending_waiters(&self) -> usize {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.async_waiters.retain(|(_, tx)| !tx.is_closed());
        guard.async_waiters.len() + guard.sync_waiters.len()
    }

    pub(crate) async fn wait_for_waiters(&self, expected: usize) {
        loop {
            let notified = self.waiter_added.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.pending_waiters() >= expected {
                return;
            }
            notified.await;
        }
    }

    pub(crate) async fn wait_for_waiter(&self) {
        self.wait_for_waiters(1).await;
    }
}

#[cfg(test)]
impl DrainClock for ManualDrainClock {
    fn now(&self) -> Duration {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .now
    }

    fn sleep_until(&self, deadline: Duration) -> BoxFuture<'static, ()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard
                .async_waiters
                .retain(|(_, sender)| !sender.is_closed());
            if deadline <= guard.now {
                return Box::pin(async {});
            }
            guard.async_waiters.push((deadline, tx));
        }
        self.waiter_added.notify_waiters();
        Box::pin(async move {
            let _ = rx.await;
        })
    }

    fn wait_condvar<'a>(
        &self,
        condvar: &Condvar,
        guard: std::sync::MutexGuard<'a, usize>,
        deadline: Duration,
        wake: SyncWakeFn,
    ) -> std::sync::MutexGuard<'a, usize> {
        let id = {
            let mut st = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if deadline <= st.now {
                return guard;
            }
            let id = st.next_sync_id;
            st.next_sync_id = st.next_sync_id.wrapping_add(1);
            st.sync_waiters.push((id, deadline, wake));
            id
        };
        self.waiter_added.notify_waiters();
        let next = condvar
            .wait(guard)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let mut st = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            st.sync_waiters.retain(|(entry_id, _, _)| *entry_id != id);
        }
        next
    }
}

/// Tracks in-flight passive background refresh calls so updates never permanently block while
/// polling screens (such as Machine Overview) are open, yet wait briefly for any mid-call refresh
/// to finish before proceeding between poll ticks.
struct BackgroundRefreshTracker {
    in_flight: Arc<Mutex<usize>>,
    condvar: Arc<Condvar>,
    notify: Notify,
    clock: Arc<dyn DrainClock>,
}

impl Default for BackgroundRefreshTracker {
    fn default() -> Self {
        Self::with_clock(Arc::new(RealDrainClock::new()))
    }
}

impl BackgroundRefreshTracker {
    fn with_clock(clock: Arc<dyn DrainClock>) -> Self {
        Self {
            in_flight: Arc::new(Mutex::new(0)),
            condvar: Arc::new(Condvar::new()),
            notify: Notify::new(),
            clock,
        }
    }

    fn count(&self) -> usize {
        *self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sync_waker(&self) -> SyncWakeFn {
        let in_flight = Arc::clone(&self.in_flight);
        let condvar = Arc::clone(&self.condvar);
        Arc::new(move || {
            let _guard = in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            condvar.notify_all();
        })
    }

    fn wait_sync(&self, timeout: Duration) -> bool {
        let mut count = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *count == 0 {
            return true;
        }
        let deadline = self.clock.now().saturating_add(timeout);
        let wake = self.sync_waker();
        while *count > 0 {
            if self.clock.now() >= deadline {
                return false;
            }
            count = self
                .clock
                .wait_condvar(&self.condvar, count, deadline, Arc::clone(&wake));
        }
        true
    }

    async fn wait_async(&self, timeout: Duration) -> bool {
        if self.count() == 0 {
            return true;
        }
        let deadline = self.clock.now().saturating_add(timeout);
        let timer = self.clock.sleep_until(deadline);
        tokio::pin!(timer);
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register as a waiter before reading `in_flight` so a concurrent drop can never lose
            // a wakeup.
            notified.as_mut().enable();
            if self.count() == 0 {
                return true;
            }
            tokio::select! {
                biased;
                _ = &mut notified => continue,
                _ = &mut timer => return self.count() == 0,
            }
        }
    }
}

/// RAII guard tracking one in-flight passive background refresh call.
pub struct BackgroundRefreshGuard(Arc<BackgroundRefreshTracker>);

impl Drop for BackgroundRefreshGuard {
    fn drop(&mut self) {
        let mut count = self
            .0
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.0.condvar.notify_all();
            self.0.notify.notify_waiters();
        }
    }
}

/// RAII guard unregistering an in-flight pairing cancellation token when pairing ends.
struct ActivePairGuard {
    active_pairs: Arc<Mutex<Vec<(u64, CancellationToken)>>>,
    seq: u64,
}

impl Drop for ActivePairGuard {
    fn drop(&mut self) {
        self.active_pairs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(seq, _)| *seq != self.seq);
    }
}

/// Production backend implementation over [`legion_link::Link`].
pub struct LinkBackend {
    store: AppStore,
    state: Mutex<BackendState>,
    /// Serializes pairing/unpairing/learning/identity-change mutations so device key generation
    /// and store writes never race.
    mutate_lock: AsyncMutex<()>,
    next_call_seq: AtomicU64,
    /// In-flight pairing cancellation tokens, cleared via [`ActivePairGuard`] on exit.
    active_pairs: Arc<Mutex<Vec<(u64, CancellationToken)>>>,
    /// Count of currently active user-started or state-mutating native operations.
    in_flight_ops: Arc<AtomicUsize>,
    /// Tracker for currently active passive non-streaming background refresh calls.
    bg_refreshes: Arc<BackgroundRefreshTracker>,
}

struct BackendState {
    data: AppStoreData,
    store_error: Option<AppError>,
    local_program: Option<String>,
    /// Live node identity observed from `meta.whoami` over the implicit desktop `local` pipe.
    /// Never guessed from disk files and never shadows a paired or learned SSH node of the same
    /// name.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    attached_local_node: Option<String>,
    active_node: Option<String>,
    /// Expiring route preference for door-level discovery reads when the user's preferred node is
    /// offline. Never changes `active_node` and never acts as a target for machine calls.
    preferred_relay: Option<(String, Instant)>,
    last_paired: Option<PairSummary>,
    disagreements: Vec<DisagreementView>,
    learn_rejections: Vec<LearnRejectionView>,
    /// Monotonic pairing epoch per configured link node (`"local"` or node name). Bumped on pair
    /// / identity-change acceptance and removed on unpair / repair so content tickets issued under
    /// a removed or replaced pairing die immediately.
    pairing_epochs: HashMap<String, u64>,
    /// Monotonic fleet epoch for tickets issued to relayed-only fleet machines (not directly
    /// configured on this device). Bumped on pair, unpair, repair, or identity change so any new
    /// or changed pairing invalidates previously minted fleet tickets immediately.
    fleet_pairing_epoch: u64,
    /// Fleet pairing epoch in which each learned direct node was added during this session, so a
    /// `fleet:<epoch>` ticket can only upgrade to an entry learned during its own epoch.
    learned_fleet_epochs: HashMap<String, u64>,
    next_pairing_epoch: u64,
    link: Link,
}

impl BackendState {
    fn alloc_epoch(&mut self) -> u64 {
        let next = self.next_pairing_epoch;
        self.next_pairing_epoch = self.next_pairing_epoch.saturating_add(1);
        next
    }

    fn bump_fleet_epoch(&mut self) -> u64 {
        let epoch = self.alloc_epoch();
        self.fleet_pairing_epoch = epoch;
        epoch
    }

    fn prune_learned_fleet_epochs(&mut self) {
        let link_cfg = &self.data.link;
        self.learned_fleet_epochs.retain(|node, _| {
            link_cfg
                .node(node)
                .is_some_and(|n| n.origin == NodeOrigin::Learned)
        });
    }

    fn set_fresh_epoch(&mut self, node: &str) -> u64 {
        let epoch = self.alloc_epoch();
        self.pairing_epochs.insert(node.to_owned(), epoch);
        epoch
    }

    fn sync_pairing_epochs(&mut self) {
        #[allow(unused_mut)]
        let mut live: Vec<String> = self
            .data
            .link
            .nodes
            .iter()
            .map(|n| n.node.clone())
            .collect();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if self.local_program.is_some() && !live.iter().any(|n| n == LOCAL_NODE_ID) {
            live.push(LOCAL_NODE_ID.to_owned());
        }
        self.pairing_epochs
            .retain(|node, _| live.iter().any(|l| l == node));
        for node in live {
            if !self.pairing_epochs.contains_key(&node) {
                let epoch = self.alloc_epoch();
                self.pairing_epochs.insert(node, epoch);
            }
        }
    }
}

fn format_pairing_holder(link_node: &str, epoch: u64) -> String {
    format!("pairing:{link_node}:{epoch}")
}

fn format_fleet_holder(epoch: u64) -> String {
    format!("fleet:{epoch}")
}

fn parse_pairing_holder(holder: &str) -> Option<(&str, u64)> {
    let rest = holder.strip_prefix("pairing:")?;
    let (node, epoch_str) = rest.rsplit_once(':')?;
    if node.is_empty() {
        return None;
    }
    let epoch = epoch_str.parse::<u64>().ok()?;
    Some((node, epoch))
}

fn parse_fleet_holder(holder: &str) -> Option<u64> {
    holder.strip_prefix("fleet:")?.parse::<u64>().ok()
}

fn canonical_host_keys(keys: &[String]) -> Vec<String> {
    let mut canonical = std::collections::BTreeSet::new();
    for line in keys {
        let mut parts = line.split_whitespace();
        if let (Some(kind), Some(data)) = (parts.next(), parts.next()) {
            canonical.insert(format!("{kind} {data}"));
        }
    }
    canonical.into_iter().collect()
}

impl LinkBackend {
    /// Loads persisted state from `store`, discovers any local desktop `legion` binary, and
    /// prepares the live [`Link`] handle.
    ///
    /// A damaged, unreadable, or newer-version state file is kept as a fatal `store_error` so
    /// [`Backend::state`] reports `needs: "repair"` and every save is refused until the user
    /// explicitly archives the damaged file via [`Backend::repair_store`].
    pub fn from_store(store: AppStore) -> Self {
        let local_program = discover_local_legion();
        Self::init(store, local_program, None, Arc::new(RealDrainClock::new()))
    }

    /// Creates a backend with an explicit local binary override (used in unit tests).
    #[cfg(test)]
    pub fn with_local_program(store: AppStore, local_program: Option<String>) -> Self {
        Self::init(store, local_program, None, Arc::new(RealDrainClock::new()))
    }

    /// Creates a backend with a custom [`legion_link::SessionOpener`] for deterministic unit tests.
    #[cfg(test)]
    pub fn with_opener(
        store: AppStore,
        local_program: Option<String>,
        opener: legion_link::SessionOpener,
    ) -> Self {
        Self::init(
            store,
            local_program,
            Some(opener),
            Arc::new(RealDrainClock::new()),
        )
    }

    /// Creates a backend with both a custom [`legion_link::SessionOpener`] and an injected
    /// [`DrainClock`] so refresh-drain tests run deterministically without wall-clock sleeps.
    #[cfg(all(test, not(any(target_os = "android", target_os = "ios"))))]
    pub(crate) fn with_opener_and_clock(
        store: AppStore,
        local_program: Option<String>,
        opener: legion_link::SessionOpener,
        clock: Arc<dyn DrainClock>,
    ) -> Self {
        Self::init(store, local_program, Some(opener), clock)
    }

    fn init(
        store: AppStore,
        local_program: Option<String>,
        opener: Option<legion_link::SessionOpener>,
        drain_clock: Arc<dyn DrainClock>,
    ) -> Self {
        let (data, store_error) = match store.load() {
            Ok(data) => (data, None),
            Err(err) => (AppStoreData::default(), Some(err)),
        };
        let (link_cfg, active_node) = if store_error.is_some() {
            (LinkConfig::default(), None)
        } else {
            (
                build_runtime_config(&data.link, local_program.as_deref()),
                resolve_active_node(&data, local_program.as_deref()),
            )
        };
        let link_options = LinkOptions {
            allow_loopback_endpoints: allow_loopback_endpoints_for_store(&store),
            ..LinkOptions::default()
        };
        let link = match opener {
            Some(custom) => Link::with_opener(link_cfg, link_options, custom),
            None => Link::with_options(link_cfg, link_options),
        };

        let mut state = BackendState {
            data,
            store_error,
            local_program,
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            attached_local_node: None,
            active_node,
            preferred_relay: None,
            last_paired: None,
            disagreements: Vec::new(),
            learn_rejections: Vec::new(),
            pairing_epochs: HashMap::new(),
            fleet_pairing_epoch: 1,
            learned_fleet_epochs: HashMap::new(),
            next_pairing_epoch: 2,
            link,
        };
        if state.store_error.is_none() {
            state.sync_pairing_epochs();
        }

        Self {
            store,
            state: Mutex::new(state),
            mutate_lock: AsyncMutex::new(()),
            next_call_seq: AtomicU64::new(1),
            active_pairs: Arc::new(Mutex::new(Vec::new())),
            in_flight_ops: Arc::new(AtomicUsize::new(0)),
            bg_refreshes: Arc::new(BackgroundRefreshTracker::with_clock(drain_clock)),
        }
    }

    /// Marks one user-started or state-mutating native operation as in flight until the returned
    /// [`OperationGuard`] is dropped.
    pub fn track_operation(&self) -> OperationGuard {
        self.in_flight_ops.fetch_add(1, Ordering::SeqCst);
        OperationGuard(Arc::clone(&self.in_flight_ops))
    }

    /// Marks one passive non-streaming background refresh call as in flight until the returned
    /// [`BackgroundRefreshGuard`] is dropped.
    pub fn track_background_refresh(&self) -> BackgroundRefreshGuard {
        let mut count = self
            .bg_refreshes
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = count.saturating_add(1);
        drop(count);
        BackgroundRefreshGuard(Arc::clone(&self.bg_refreshes))
    }

    fn require_healthy_store(&self) -> Result<(), AppError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(err) = &guard.store_error {
            return Err(err.clone());
        }
        Ok(())
    }

    fn snapshot_state(&self) -> AppState {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        build_app_state(&guard, self.store.path())
    }

    /// Returns the live [`Link`] and the preferred route hint for door-level discovery reads (an
    /// unexpired `preferred_relay` if set, otherwise `active_node`).
    ///
    /// This hint is strictly a route preference for reaching a named machine or answering a
    /// door-level discovery read — it is never a target for machine-scoped tool calls.
    fn active_link_and_route_hint(&self) -> Result<(Link, Option<String>), AppError> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(err) = &guard.store_error {
            return Err(err.clone());
        }
        let link = guard.link.clone();
        let now = Instant::now();
        let relay_hint = match &guard.preferred_relay {
            Some((name, exp)) if *exp > now && guard.data.link.node(name).is_some() => {
                Some(name.clone())
            }
            Some(_) => {
                guard.preferred_relay = None;
                None
            }
            None => None,
        };
        let hint = relay_hint
            .or_else(|| guard.active_node.clone())
            .or_else(|| {
                build_app_state(&guard, self.store.path())
                    .nodes
                    .into_iter()
                    .next()
                    .map(|n| n.node)
            });
        Ok((link, hint))
    }

    fn note_preferred_relay(&self, node: &str) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.store_error.is_some() {
            return;
        }
        if guard.active_node.as_deref() == Some(node) {
            guard.preferred_relay = None;
            return;
        }
        // Only remember a directly paired node as a route preference hint.
        if guard
            .data
            .link
            .node(node)
            .is_some_and(|n| n.origin == NodeOrigin::Paired)
        {
            guard.preferred_relay = Some((node.to_owned(), Instant::now() + PREFERRED_RELAY_TTL));
        }
    }

    fn next_call_id(&self, provided: Option<CallId>) -> String {
        match provided {
            Some(id) => id.as_key(),
            None => format!(
                "call-{}",
                self.next_call_seq.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    fn resolve_link_node_for_call(&self, machine: &str) -> String {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            // Paired and learned SSH nodes ALWAYS take precedence by identity so a remote node
            // sharing a name with the local machine can never be silently redirected over the
            // local pipe.
            if guard.data.link.node(machine).is_some() {
                return machine.to_owned();
            }
            if guard.local_program.is_some()
                && (machine == LOCAL_NODE_ID
                    || guard.attached_local_node.as_deref() == Some(machine))
            {
                return LOCAL_NODE_ID.to_owned();
            }
        }
        machine.to_owned()
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    async fn filter_local_self_from_learn_report(
        &self,
        link: &Link,
        report: &mut legion_link::LearnReport,
    ) {
        let local_self_name = {
            let guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.local_program.is_some() {
                guard
                    .attached_local_node
                    .as_deref()
                    .filter(|name| {
                        !guard
                            .data
                            .link
                            .node(name)
                            .is_some_and(|n| n.origin == NodeOrigin::Paired)
                    })
                    .map(str::to_owned)
            } else {
                None
            }
        };
        if let Some(ref local_name) = local_self_name {
            if link
                .config()
                .node(local_name)
                .is_some_and(|n| n.origin == NodeOrigin::Learned)
            {
                let _ = link.remove_node(local_name).await;
            }
            let is_local_self = |m: &str| m == LOCAL_NODE_ID || m == local_name.as_str();
            report.added.retain(|m| !is_local_self(m));
            report.updated.retain(|m| !is_local_self(m));
            report.identity_changed.retain(|m| !is_local_self(m));
            report.disagreements.retain(|m| !is_local_self(m));
            report
                .disagreement_details
                .retain(|d| !is_local_self(&d.machine));
            report.unreachable.retain(|m| !is_local_self(m));
            report.rejected.retain(|m| !is_local_self(m));
            report
                .rejected_details
                .retain(|r| !is_local_self(&r.machine));
        }
    }

    async fn commit_enrolled_pairing(
        &self,
        enrolled: legion_link::Enrolled,
        #[cfg_attr(any(target_os = "android", target_os = "ios"), allow(unused_mut))]
        mut learn_report: Option<legion_link::LearnReport>,
    ) -> Result<AppState, AppError> {
        let pair_summary = build_pair_summary(&enrolled);
        let node_entry = LinkNode::from(enrolled.clone());
        let node_name = enrolled.node.clone();
        let meta = StoredNodeMeta {
            node: enrolled.node.clone(),
            role: Some(enrolled.role.clone()),
            device: Some(enrolled.device.clone()),
            role_checked_at: Some(now_rfc3339_utc()),
        };

        let _mutation = self.mutate_lock.lock().await;
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if let Some(report) = learn_report.as_mut() {
            let link = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.link.clone()
            };
            self.filter_local_self_from_learn_report(&link, report)
                .await;
        }
        let app_state = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(err) = &guard.store_error {
                return Err(err.clone());
            }
            let mut updated_data = guard.data.clone();
            if learn_report.is_some() {
                let live_cfg = guard.link.config();
                updated_data.link = persisted_link_config(&live_cfg, &guard.data.link);
            }
            updated_data.link.upsert_node(node_entry);
            updated_data.upsert_meta(meta);
            updated_data.current_node = Some(node_name.clone());
            self.store.save(&updated_data)?;
            guard.data = updated_data;
            guard.active_node = Some(node_name.clone());
            guard.preferred_relay = None;
            guard.last_paired = Some(pair_summary);
            guard.sync_pairing_epochs();
            guard.set_fresh_epoch(&node_name);
            guard.bump_fleet_epoch();
            guard.learned_fleet_epochs.remove(&node_name);
            if let Some(report) = &learn_report {
                let current_fleet_epoch = guard.fleet_pairing_epoch;
                for added in &report.added {
                    if guard
                        .data
                        .link
                        .node(added)
                        .is_some_and(|n| n.origin == NodeOrigin::Learned)
                    {
                        guard
                            .learned_fleet_epochs
                            .insert(added.clone(), current_fleet_epoch);
                    }
                }
                guard.prune_learned_fleet_epochs();
                guard.disagreements = build_disagreement_views(report);
                guard.learn_rejections = build_learn_rejection_views(report);
            }
            guard.disagreements.retain(|d| d.machine != node_name);
            guard.learn_rejections.retain(|r| r.machine != node_name);
            build_app_state(&guard, self.store.path())
        };

        Ok(app_state)
    }
}

impl Backend for LinkBackend {
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move { Ok(self.snapshot_state()) })
    }

    fn tools(&self, on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
        Box::pin(async move {
            let _refresh = self.track_background_refresh();
            let (link, route_hint) = self.active_link_and_route_hint()?;
            if route_hint.is_none() {
                return Err(AppError::not_paired());
            }
            let parsed_target = parse_optional_target(on).map_err(AppError::from)?;
            let Some(Target::One(machine)) = parsed_target else {
                return Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    "Listing tools requires a single target machine in `on`.",
                )
                .with_hint(
                    "Pass `on: \"<machine>\"` to name the machine whose tool catalogue you want.",
                ));
            };
            let resolved = self.resolve_link_node_for_call(&machine);
            let routed = link
                .tools_machine(&resolved)
                .await
                .map_err(AppError::from)?;
            let tools = routed
                .value
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(tools)
        })
    }

    fn call(
        &self,
        request: CallRequest,
        on_event: Option<Channel<Value>>,
    ) -> BoxFuture<'_, Result<Value, AppError>> {
        Box::pin(async move {
            let (_op, _refresh) = if is_background_refresh_call(&request.tool, on_event.is_some()) {
                (None, Some(self.track_background_refresh()))
            } else {
                (Some(self.track_operation()), None)
            };
            let (link, route_hint) = self.active_link_and_route_hint()?;
            let Some(hint_name) = route_hint else {
                return Err(AppError::not_paired());
            };
            let call_id = self.next_call_id(request.id);
            let parsed_target =
                parse_optional_target(request.on.clone()).map_err(AppError::from)?;

            let channel = on_event;
            let emit = move |event| {
                if let Some(ch) = &channel {
                    let _ = ch.send(event);
                }
            };

            match parsed_target {
                Some(Target::One(machine)) => {
                    let resolved = self.resolve_link_node_for_call(&machine);
                    let routed = link
                        .call_machine_with_id(
                            &call_id,
                            &resolved,
                            &request.tool,
                            request.args,
                            emit,
                        )
                        .await
                        .map_err(AppError::from)?;
                    #[cfg(not(any(target_os = "android", target_os = "ios")))]
                    if resolved == LOCAL_NODE_ID
                        && matches!(
                            request.tool.as_str(),
                            "meta.whoami" | "sys.info" | "meta.version"
                        )
                        && let Some(actual_name) = routed
                            .value
                            .get("node")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty() && *s != LOCAL_NODE_ID)
                    {
                        let mut guard = self
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard.attached_local_node = Some(actual_name.to_owned());
                    }
                    Ok(routed.value)
                }
                Some(Target::All | Target::Many(_)) => {
                    let (answering, value) = link
                        .call_any_with_id(
                            Some(call_id),
                            Some(&hint_name),
                            &request.tool,
                            request.args,
                            request.on,
                            emit,
                        )
                        .await
                        .map_err(AppError::from)?;
                    if answering != hint_name {
                        self.note_preferred_relay(&answering);
                    }
                    Ok(value)
                }
                None if is_any_node_tool(&request.tool) => {
                    let (answering, value) = link
                        .call_any_with_id(
                            Some(call_id),
                            Some(&hint_name),
                            &request.tool,
                            request.args,
                            None,
                            emit,
                        )
                        .await
                        .map_err(AppError::from)?;
                    if answering != hint_name {
                        self.note_preferred_relay(&answering);
                    }
                    Ok(label_discovery_result(&request.tool, &answering, value))
                }
                None => Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    format!("Tool `{}` requires a target machine in `on`.", request.tool),
                )
                .with_hint(
                    "Pass `on: \"<machine>\"` to name the machine this call should run on.",
                )),
            }
        })
    }

    fn cancel(&self, id: CallId) -> BoxFuture<'_, Result<CancelResult, AppError>> {
        Box::pin(async move {
            let link = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.link.clone()
            };
            let _ = link.cancel(&id.as_key());
            Ok(CancelResult {})
        })
    }

    fn cancel_pair(&self) -> BoxFuture<'_, Result<CancelResult, AppError>> {
        Box::pin(async move {
            let tokens: Vec<CancellationToken> = self
                .active_pairs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .map(|(_, tok)| tok.clone())
                .collect();
            for token in tokens {
                token.cancel();
            }
            Ok(CancelResult {})
        })
    }

    fn pair(&self, invite: String, name: String) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            self.require_healthy_store()?;
            let trimmed_invite = invite.trim();
            if trimmed_invite.is_empty() {
                return Err(
                    AppError::new(ErrorCode::InvalidArgs, "Invite text cannot be empty.")
                        .with_hint(
                            "Scan the QR code or paste the invite line printed by `legion pair`.",
                        ),
                );
            }

            let parsed = Invite::parse(trimmed_invite).map_err(AppError::from)?;
            let target_node_name = parsed.node.clone();
            let device_label = {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    default_device_name()
                } else {
                    trimmed.to_owned()
                }
            };

            let pair_seq = self.next_call_seq.fetch_add(1, Ordering::Relaxed);
            let cancel = CancellationToken::new();
            {
                self.active_pairs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((pair_seq, cancel.clone()));
            }
            let _pair_guard = ActivePairGuard {
                active_pairs: Arc::clone(&self.active_pairs),
                seq: pair_seq,
            };

            // Hold `mutate_lock` only while ensuring and persisting the device key BEFORE network
            // I/O so `unpair` and `select_node` are never blocked while pairing waits.
            let link = {
                let _mutation = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return Err(AppError::new(
                            ErrorCode::Cancelled,
                            format!("Pairing with {target_node_name} was cancelled."),
                        )
                        .with_details(serde_json::json!({ "sent": false })));
                    }
                    guard = self.mutate_lock.lock() => guard,
                };

                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                if guard.data.link.device_key.is_none() {
                    let device_key = DeviceKey::generate().map_err(AppError::from)?;
                    let key_pem = device_key.to_openssh().map_err(AppError::from)?;
                    let mut next = guard.data.clone();
                    next.link.device_key = Some(key_pem.clone());
                    self.store.save(&next)?;
                    guard.link.set_device_key(key_pem).map_err(AppError::from)?;
                    guard.data = next;
                }
                guard.link.clone()
            };

            // Enrol in place via `Link::pair_cancellable_with_learn`, reusing the persisted device
            // key and keeping any other live sessions, `unpair`, and `select_node` undisturbed.
            let (enrolled, learn_report) = link
                .pair_cancellable_with_learn(trimmed_invite, &device_label, cancel)
                .await
                .map_err(AppError::from)?;

            self.commit_enrolled_pairing(enrolled, learn_report).await
        })
    }

    fn restore_pairing(
        &self,
        invite: String,
        existing_principal: String,
        name: String,
    ) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            self.require_healthy_store()?;
            let trimmed_invite = invite.trim();
            if trimmed_invite.is_empty() {
                return Err(
                    AppError::new(ErrorCode::InvalidArgs, "Invite text cannot be empty.")
                        .with_hint(
                            "Scan the QR code or paste the invite line printed by `legion pair`.",
                        ),
                );
            }
            let trimmed_principal = existing_principal.trim();
            let parsed = Invite::parse(trimmed_invite).map_err(AppError::from)?;
            let target_node_name = parsed.node.clone();
            let device_label = {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    default_device_name()
                } else {
                    trimmed.to_owned()
                }
            };

            let pair_seq = self.next_call_seq.fetch_add(1, Ordering::Relaxed);
            let cancel = CancellationToken::new();
            {
                self.active_pairs
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((pair_seq, cancel.clone()));
            }
            let _pair_guard = ActivePairGuard {
                active_pairs: Arc::clone(&self.active_pairs),
                seq: pair_seq,
            };

            let link = {
                let _mutation = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        return Err(AppError::new(
                            ErrorCode::Cancelled,
                            format!("Restoring pairing with {target_node_name} was cancelled."),
                        )
                        .with_details(serde_json::json!({ "sent": false })));
                    }
                    guard = self.mutate_lock.lock() => guard,
                };
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                if guard.data.link.device_key.is_none() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        "This device does not have a saved key to restore.",
                    ));
                }
                guard.link.clone()
            };

            let (enrolled, learn_report) = link
                .restore_pairing_with_learn(
                    trimmed_invite,
                    trimmed_principal,
                    &device_label,
                    cancel,
                )
                .await
                .map_err(AppError::from)?;

            self.commit_enrolled_pairing(enrolled, learn_report).await
        })
    }

    fn unpair(&self, node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            self.require_healthy_store()?;
            let trimmed = node.trim();
            if trimmed.is_empty() {
                return Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    "Node name cannot be empty.",
                ));
            }

            let _mutation = self.mutate_lock.lock().await;

            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            if trimmed == LOCAL_NODE_ID {
                let is_paired_ssh = {
                    let guard = self
                        .state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.data.link.node(trimmed).is_some()
                };
                if !is_paired_ssh {
                    return Err(AppError::new(
                        ErrorCode::InvalidArgs,
                        "The local desktop pipe cannot be unpaired.",
                    )
                    .with_hint(
                        "It connects directly to the local `legion` binary on this machine.",
                    ));
                }
            }

            let (device_name, link) = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                if guard.data.link.node(trimmed).is_none() {
                    return Err(AppError::new(
                        ErrorCode::NotFound,
                        format!("No paired node named `{trimmed}` was found."),
                    ));
                }
                let mut next = guard.data.clone();
                let removed_meta = next.remove_meta(trimmed);
                let device = removed_meta
                    .and_then(|m| m.device)
                    .unwrap_or_else(default_device_name);
                let dropped = next.link.forget_node(trimmed);
                for dropped_node in &dropped {
                    if dropped_node != trimmed {
                        next.remove_meta(dropped_node);
                    }
                }
                if next
                    .current_node
                    .as_deref()
                    .is_some_and(|cur| dropped.iter().any(|d| d == cur))
                {
                    next.current_node = next.link.nodes.first().map(|n| n.node.clone());
                }
                self.store.save(&next)?;
                let local_program = guard.local_program.clone();
                guard.data = next;
                guard.active_node = resolve_active_node(&guard.data, local_program.as_deref());
                guard.preferred_relay = None;
                guard.sync_pairing_epochs();
                guard.bump_fleet_epoch();
                guard.prune_learned_fleet_epochs();
                guard.disagreements.retain_mut(|d| {
                    if d.machine == trimmed {
                        return false;
                    }
                    d.reports.retain(|r| r.reported_by != trimmed);
                    d.reports.len() >= 2
                });
                guard
                    .learn_rejections
                    .retain(|r| r.machine != trimmed && r.reported_by.as_deref() != Some(trimmed));
                (device, guard.link.clone())
            };

            link.forget(trimmed).await;

            let message = format!(
                "Removed `{trimmed}` from this device. `{trimmed}` still keeps this device until you run `legion device.remove name={device_name}` there."
            );

            Ok(UnpairResult {
                node: trimmed.to_owned(),
                device: device_name,
                message,
            })
        })
    }

    fn select_node(&self, node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move {
            self.require_healthy_store()?;
            let trimmed = node.trim();
            if trimmed.is_empty() {
                return Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    "Node name cannot be empty.",
                ));
            }

            let _mutation = self.mutate_lock.lock().await;
            let updated_state = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                let exists_ssh = guard.data.link.node(trimmed).is_some();
                let exists_local = is_local_node_available(guard.local_program.as_deref(), trimmed);
                if !exists_ssh && !exists_local {
                    return Err(AppError::new(
                        ErrorCode::NotFound,
                        format!("No configured node named `{trimmed}`."),
                    ));
                }
                let mut next = guard.data.clone();
                next.current_node = if exists_ssh {
                    Some(trimmed.to_owned())
                } else {
                    None
                };
                self.store.save(&next)?;
                guard.data = next;
                guard.active_node = Some(trimmed.to_owned());
                guard.preferred_relay = None;
                build_app_state(&guard, self.store.path())
            };

            Ok(updated_state)
        })
    }

    fn repair_store(&self) -> BoxFuture<'_, Result<RepairResult, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            let _mutation = self.mutate_lock.lock().await;
            {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if guard.store_error.is_none() {
                    return Err(AppError::new(
                        ErrorCode::InvalidArgs,
                        "App state is healthy and does not need repair.",
                    ));
                }
            }

            let archived = self.store.archive_damaged()?;
            let archived_to = archived.display().to_string();

            let (old_link, next_state) = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let fresh = AppStoreData::default();
                let runtime_cfg = build_runtime_config(&fresh.link, guard.local_program.as_deref());
                let active = resolve_active_node(&fresh, guard.local_program.as_deref());
                let link_options = LinkOptions {
                    allow_loopback_endpoints: allow_loopback_endpoints_for_store(&self.store),
                    ..LinkOptions::default()
                };
                let old = std::mem::replace(
                    &mut guard.link,
                    Link::with_options(runtime_cfg, link_options),
                );
                guard.data = fresh;
                guard.store_error = None;
                guard.active_node = active;
                guard.preferred_relay = None;
                guard.last_paired = None;
                guard.disagreements.clear();
                guard.learn_rejections.clear();
                guard.pairing_epochs.clear();
                guard.learned_fleet_epochs.clear();
                guard.bump_fleet_epoch();
                guard.sync_pairing_epochs();
                (old, build_app_state(&guard, self.store.path()))
            };

            old_link.close().await;

            let message = format!(
                "Moved the unreadable state file to {archived_to}. Paired machines still list this device until you remove it there with `legion device.remove`."
            );

            Ok(RepairResult {
                archived_to,
                message,
                state: next_state,
            })
        })
    }

    fn invalidate_sessions(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async move {
            let link = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.link.clone()
            };
            link.close().await;
            Ok(())
        })
    }

    fn learn(&self) -> BoxFuture<'_, Result<LearnResult, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            self.require_healthy_store()?;
            // Hold `mutate_lock` across the whole learn call so an `unpair` during `learn` can
            // never be undone when learned routes are persisted.
            let _mutation = self.mutate_lock.lock().await;
            let link = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                guard.link.clone()
            };

            #[cfg_attr(any(target_os = "android", target_os = "ios"), allow(unused_mut))]
            let mut report = link.learn().await.map_err(AppError::from)?;

            // On desktop, never persist a learned SSH route or surface a learn rejection/disagreement
            // for this computer's own local-pipe machine name unless the user explicitly paired an
            // SSH node with that name.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            let local_self_name: Option<String> = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if guard.local_program.is_some() {
                    guard
                        .attached_local_node
                        .as_deref()
                        .filter(|name| {
                            !guard
                                .data
                                .link
                                .node(name)
                                .is_some_and(|n| n.origin == NodeOrigin::Paired)
                        })
                        .map(str::to_owned)
                } else {
                    None
                }
            };
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            if let Some(ref local_name) = local_self_name {
                if link
                    .config()
                    .node(local_name)
                    .is_some_and(|n| n.origin == NodeOrigin::Learned)
                {
                    let _ = link.remove_node(local_name).await;
                }
                let is_local_self = |m: &str| m == LOCAL_NODE_ID || m == local_name.as_str();
                report.added.retain(|m| !is_local_self(m));
                report.updated.retain(|m| !is_local_self(m));
                report.identity_changed.retain(|m| !is_local_self(m));
                report.disagreements.retain(|m| !is_local_self(m));
                report
                    .disagreement_details
                    .retain(|d| !is_local_self(&d.machine));
                report.unreachable.retain(|m| !is_local_self(m));
                report.rejected.retain(|m| !is_local_self(m));
                report
                    .rejected_details
                    .retain(|r| !is_local_self(&r.machine));
            }

            let state = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                let live_cfg = guard.link.config();
                let mut updated = guard.data.clone();
                updated.link = persisted_link_config(&live_cfg, &guard.data.link);
                // Learned machines must never inherit role or device metadata from their
                // introducer; the interface queries each machine directly for its own identity.
                if updated != guard.data {
                    self.store.save(&updated)?;
                    guard.data = updated;
                    guard.sync_pairing_epochs();
                }
                let current_fleet_epoch = guard.fleet_pairing_epoch;
                for added in &report.added {
                    if guard
                        .data
                        .link
                        .node(added)
                        .is_some_and(|n| n.origin == NodeOrigin::Learned)
                    {
                        guard
                            .learned_fleet_epochs
                            .insert(added.clone(), current_fleet_epoch);
                    }
                }
                guard.prune_learned_fleet_epochs();
                guard.disagreements = build_disagreement_views(&report);
                guard.learn_rejections = build_learn_rejection_views(&report);
                build_app_state(&guard, self.store.path())
            };

            Ok(LearnResult { report, state })
        })
    }

    fn accept_identity_change(
        &self,
        node: String,
        fingerprints: Vec<String>,
    ) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            self.require_healthy_store()?;
            let trimmed = node.trim();
            if trimmed.is_empty() {
                return Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    "Node name cannot be empty.",
                ));
            }
            let _mutation = self.mutate_lock.lock().await;
            let state = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                // Validate and persist to the store FIRST so a failed disk save never leaves the
                // live Link holding pins that were not saved.
                let mut updated = guard.data.clone();
                let _ = updated
                    .link
                    .apply_accept_identity_change(trimmed, &fingerprints)
                    .map_err(AppError::from)?;
                self.store.save(&updated)?;
                let _ = guard
                    .link
                    .accept_identity_change(trimmed, &fingerprints)
                    .map_err(AppError::from)?;
                guard.data = updated;
                guard.set_fresh_epoch(trimmed);
                guard.learned_fleet_epochs.remove(trimmed);
                guard.bump_fleet_epoch();
                build_app_state(&guard, self.store.path())
            };
            Ok(state)
        })
    }

    fn dismiss_identity_change(&self, node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move {
            let _op = self.track_operation();
            self.require_healthy_store()?;
            let trimmed = node.trim();
            if trimmed.is_empty() {
                return Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    "Node name cannot be empty.",
                ));
            }
            let _mutation = self.mutate_lock.lock().await;
            let state = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(err) = &guard.store_error {
                    return Err(err.clone());
                }
                let mut updated = guard.data.clone();
                let _ = updated
                    .link
                    .apply_dismiss_identity_change(trimmed)
                    .map_err(AppError::from)?;
                self.store.save(&updated)?;
                let _ = guard
                    .link
                    .dismiss_identity_change(trimmed)
                    .map_err(AppError::from)?;
                guard.data = updated;
                build_app_state(&guard, self.store.path())
            };
            Ok(state)
        })
    }

    fn update_sources(&self) -> Result<Option<(Link, Vec<String>)>, AppError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(err) = &guard.store_error {
            return Err(err.clone());
        }
        let mut nodes: Vec<String> = Vec::new();
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if guard.local_program.is_some() && guard.data.link.node(LOCAL_NODE_ID).is_none() {
            nodes.push(LOCAL_NODE_ID.to_owned());
        }
        for entry in &guard.data.link.nodes {
            if entry.origin == NodeOrigin::Paired && entry.identity_changed.is_none() {
                nodes.push(entry.node.clone());
            }
        }
        Ok(Some((guard.link.clone(), nodes)))
    }

    fn active_operations(&self) -> usize {
        self.in_flight_ops.load(Ordering::SeqCst)
    }

    fn background_refreshes(&self) -> usize {
        self.bg_refreshes.count()
    }

    fn wait_for_background_refreshes(&self, timeout: Duration) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.bg_refreshes.wait_async(timeout).await })
    }

    fn wait_for_background_refreshes_sync(&self, timeout: Duration) -> bool {
        self.bg_refreshes.wait_sync(timeout)
    }

    fn content_target(&self, machine: &str) -> Result<ContentTarget, AppError> {
        let trimmed = machine.trim();
        if trimmed.is_empty() {
            return Err(
                AppError::new(ErrorCode::InvalidArgs, "Machine name cannot be empty.")
                    .with_hint("Pass the name of a paired machine."),
            );
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(err) = &guard.store_error {
            return Err(err.clone());
        }
        let link = guard.link.clone();
        // Paired and learned SSH nodes ALWAYS match first so a paired remote machine sharing the
        // local hostname routes over its authenticated SSH connection with host-key verification
        // instead of silently redirecting to the local pipe.
        if let Some(entry) = guard.data.link.node(trimmed).cloned() {
            let epoch = match guard.pairing_epochs.get(&entry.node).copied() {
                Some(ep) => ep,
                None => guard.set_fresh_epoch(&entry.node),
            };
            let learned_identity = if entry.origin == NodeOrigin::Learned {
                let paired_introducer = entry.learned_from.as_deref().and_then(|intro| {
                    guard
                        .data
                        .link
                        .node(intro)
                        .filter(|n| n.origin == NodeOrigin::Paired)
                        .cloned()
                });
                let from_paired_introducer = paired_introducer.is_some();
                let introducer_holder = paired_introducer.as_ref().and_then(|intro| {
                    guard
                        .pairing_epochs
                        .get(&intro.node)
                        .copied()
                        .map(|ep| format_pairing_holder(&intro.node, ep))
                });
                let introducer_host_keys = paired_introducer
                    .as_ref()
                    .and_then(|intro| intro.target.as_ref())
                    .map(|t| canonical_host_keys(&t.host_keys))
                    .unwrap_or_default();
                let learned_from = paired_introducer.map(|intro| intro.node);
                let host_keys = entry
                    .target
                    .as_ref()
                    .map(|t| canonical_host_keys(&t.host_keys))
                    .unwrap_or_default();
                guard
                    .learned_fleet_epochs
                    .get(&entry.node)
                    .copied()
                    .filter(|_| !host_keys.is_empty())
                    .map(|learned_fleet_epoch| LearnedDirectIdentity {
                        learned_fleet_epoch,
                        from_paired_introducer,
                        learned_from,
                        introducer_holder,
                        introducer_host_keys,
                        host_keys,
                    })
            } else {
                None
            };
            let principal = entry.principal.unwrap_or_else(|| {
                if entry.local_program.is_some() {
                    "local".to_owned()
                } else {
                    "device".to_owned()
                }
            });
            return Ok(ContentTarget {
                link,
                link_node: entry.node.clone(),
                machine: entry.node.clone(),
                principal,
                holder: format_pairing_holder(&entry.node, epoch),
                learned_identity,
            });
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if guard.local_program.is_some()
            && (trimmed == LOCAL_NODE_ID || guard.attached_local_node.as_deref() == Some(trimmed))
        {
            let epoch = match guard.pairing_epochs.get(LOCAL_NODE_ID).copied() {
                Some(ep) => ep,
                None => guard.set_fresh_epoch(LOCAL_NODE_ID),
            };
            return Ok(ContentTarget {
                link,
                link_node: LOCAL_NODE_ID.to_owned(),
                machine: trimmed.to_owned(),
                principal: "local".to_owned(),
                holder: format_pairing_holder(LOCAL_NODE_ID, epoch),
                learned_identity: None,
            });
        }
        let any_configured = !guard.data.link.nodes.is_empty() || guard.local_program.is_some();
        if !any_configured {
            return Err(AppError::not_paired());
        }
        // Any fleet machine not directly configured on this device routes through `Link`
        // (direct-first with relay fallback), matching RPC call routing without a separate path.
        Ok(ContentTarget {
            link,
            link_node: trimmed.to_owned(),
            machine: trimmed.to_owned(),
            principal: "device".to_owned(),
            holder: format_fleet_holder(guard.fleet_pairing_epoch),
            learned_identity: None,
        })
    }

    fn is_pairing_holder_live(&self, holder: &str) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.store_error.is_some() {
            return false;
        }
        if let Some((node, epoch)) = parse_pairing_holder(holder) {
            return guard.pairing_epochs.get(node).copied() == Some(epoch);
        }
        if let Some(epoch) = parse_fleet_holder(holder) {
            let any_configured = !guard.data.link.nodes.is_empty() || guard.local_program.is_some();
            return any_configured && guard.fleet_pairing_epoch == epoch;
        }
        false
    }

    fn node_route_identity(&self, node: &str) -> Option<NodeRouteIdentity> {
        let trimmed = node.trim();
        if trimmed.is_empty() {
            return None;
        }
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.store_error.is_some() {
            return None;
        }
        if let Some(entry) = guard.data.link.node(trimmed) {
            let epoch = guard.pairing_epochs.get(&entry.node).copied()?;
            let host_keys = entry
                .target
                .as_ref()
                .map(|t| canonical_host_keys(&t.host_keys))
                .unwrap_or_default();
            return Some(NodeRouteIdentity {
                node: entry.node.clone(),
                is_paired: entry.origin == NodeOrigin::Paired,
                holder: format_pairing_holder(&entry.node, epoch),
                host_keys,
            });
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if guard.local_program.is_some()
            && (trimmed == LOCAL_NODE_ID || guard.attached_local_node.as_deref() == Some(trimmed))
        {
            let epoch = guard.pairing_epochs.get(LOCAL_NODE_ID).copied()?;
            return Some(NodeRouteIdentity {
                node: LOCAL_NODE_ID.to_owned(),
                is_paired: true,
                holder: format_pairing_holder(LOCAL_NODE_ID, epoch),
                host_keys: Vec::new(),
            });
        }
        None
    }
}

fn build_disagreement_views(report: &legion_link::LearnReport) -> Vec<DisagreementView> {
    report
        .disagreement_details
        .iter()
        .map(|d| DisagreementView {
            machine: d.machine.clone(),
            reports: d
                .reports
                .iter()
                .map(|r| DisagreementEntryView {
                    reported_by: r.reported_by.clone(),
                    hosts: r.target.hosts.clone(),
                    port: r.target.port,
                    user: r.target.user.clone(),
                    fingerprints: r
                        .target
                        .host_keys
                        .iter()
                        .filter_map(|k| public_key_fingerprint(k))
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

fn build_learn_rejection_views(report: &legion_link::LearnReport) -> Vec<LearnRejectionView> {
    report
        .rejected_details
        .iter()
        .map(|r| LearnRejectionView {
            machine: r.machine.clone(),
            code: r.code.clone(),
            reason: r.reason.clone(),
            reported_by: r.reported_by.clone(),
        })
        .collect()
}

fn build_pair_summary(enrolled: &legion_link::Enrolled) -> PairSummary {
    let mut synced = Vec::new();
    let mut not_synced = Vec::new();
    for (peer, outcome) in &enrolled.fleet {
        if outcome.status == "synced" {
            synced.push(peer.clone());
        } else {
            not_synced.push(PairPendingPeer {
                node: peer.clone(),
                status: Some(outcome.status.clone()),
                reason: outcome.reason.clone(),
            });
        }
    }
    for pending in &enrolled.not_synced_to {
        if !not_synced.iter().any(|p| &p.node == pending) {
            not_synced.push(PairPendingPeer {
                node: pending.clone(),
                status: Some("not_reached".to_owned()),
                reason: None,
            });
        }
    }
    PairSummary {
        node: enrolled.node.clone(),
        device: enrolled.device.clone(),
        role: enrolled.role.clone(),
        synced,
        not_synced,
        sync_command: enrolled.sync_command.clone(),
    }
}

/// Filters the implicit desktop `local` pipe out of `live_cfg` before persisting to `AppStore`
/// unless the user explicitly persisted a node with that name.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn persisted_link_config(live_cfg: &LinkConfig, previous_persisted: &LinkConfig) -> LinkConfig {
    let mut out = live_cfg.clone();
    let had_persisted_local = previous_persisted.node(LOCAL_NODE_ID).is_some();
    if !had_persisted_local {
        out.nodes
            .retain(|n| !(n.node == LOCAL_NODE_ID && n.target.is_none()));
    }
    out
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn persisted_link_config(live_cfg: &LinkConfig, _previous_persisted: &LinkConfig) -> LinkConfig {
    live_cfg.clone()
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn is_local_node_available(local_program: Option<&str>, name: &str) -> bool {
    local_program.is_some() && name == LOCAL_NODE_ID
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn is_local_node_available(_local_program: Option<&str>, _name: &str) -> bool {
    false
}

fn build_runtime_config(persisted: &LinkConfig, local_program: Option<&str>) -> LinkConfig {
    #[cfg_attr(any(target_os = "android", target_os = "ios"), allow(unused_mut))]
    let mut cfg = persisted.clone();
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Some(program) = local_program
        && cfg.node(LOCAL_NODE_ID).is_none()
    {
        cfg.upsert_node(LinkNode::local(LOCAL_NODE_ID, program));
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let _ = local_program;
    cfg
}

fn resolve_active_node(data: &AppStoreData, local_program: Option<&str>) -> Option<String> {
    if let Some(current) = &data.current_node
        && data.link.node(current).is_some()
    {
        return Some(current.clone());
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if local_program.is_some() {
        return Some(LOCAL_NODE_ID.to_owned());
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    let _ = local_program;
    data.link.nodes.first().map(|n| n.node.clone())
}

fn build_app_state(state: &BackendState, store_path: &std::path::Path) -> AppState {
    let can_scan = cfg!(any(target_os = "android", target_os = "ios"));
    if let Some(err) = &state.store_error {
        return AppState {
            ready: false,
            nodes: Vec::new(),
            needs: Some(Needs::Repair),
            repair: Some(RepairInfo {
                message: err.message.clone(),
                path: store_path.display().to_string(),
            }),
            can_scan,
            routes: BTreeMap::new(),
            disagreements: Vec::new(),
            learn_rejections: Vec::new(),
            last_paired: None,
            transfer_capabilities: TransferCapabilities::current_platform(),
        };
    }

    let raw_routes = state.link.route_status();
    let mut routes: BTreeMap<String, MachineRouteView> = BTreeMap::new();
    for (machine, status) in &raw_routes {
        let view = match status {
            MachineRouteStatus::Direct => MachineRouteView {
                kind: "direct".to_owned(),
                via: None,
                introducer: None,
            },
            MachineRouteStatus::Relay { via } => MachineRouteView {
                kind: "relay".to_owned(),
                via: Some(via.clone()),
                introducer: None,
            },
            MachineRouteStatus::NotSyncedYet { introducer, via } => MachineRouteView {
                kind: "not_synced".to_owned(),
                via: via.clone(),
                introducer: introducer.clone(),
            },
            MachineRouteStatus::IdentityChanged { reported_by } => MachineRouteView {
                kind: "identity_changed".to_owned(),
                via: Some(reported_by.clone()),
                introducer: None,
            },
        };
        routes.insert(machine.clone(), view);
    }

    let mut nodes: Vec<NodeLink> = Vec::new();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if let Some(local_path) = state.local_program.as_ref()
        && state.data.link.node(LOCAL_NODE_ID).is_none()
    {
        nodes.push(NodeLink {
            node: LOCAL_NODE_ID.to_owned(),
            principal: "local".to_owned(),
            role: "owner".to_owned(),
            role_checked_at: None,
            via: "local".to_owned(),
            path: Some(local_path.clone()),
            origin: "paired".to_owned(),
            learned_from: None,
            route: "direct".to_owned(),
            route_via: None,
            identity_changed: None,
        });
    }

    for entry in &state.data.link.nodes {
        let meta = state.data.meta(&entry.node);
        let (via, path) = if let Some(program) = &entry.local_program {
            ("local", Some(program.clone()))
        } else {
            ("ssh", None)
        };
        let origin = match entry.origin {
            NodeOrigin::Paired => "paired",
            NodeOrigin::Learned => "learned",
        };
        let identity_changed = entry.identity_changed.as_ref().map(|pending| {
            let (old_hosts, old_port, old_user, old_fingerprints) = entry
                .target
                .as_ref()
                .map(|t| {
                    (
                        t.hosts.clone(),
                        t.port,
                        t.user.clone(),
                        t.host_keys
                            .iter()
                            .filter_map(|k| public_key_fingerprint(k))
                            .collect(),
                    )
                })
                .unwrap_or_else(|| (Vec::new(), 0, String::new(), Vec::new()));
            let new_fingerprints = pending
                .target
                .host_keys
                .iter()
                .filter_map(|k| public_key_fingerprint(k))
                .collect();
            let conflicts = pending
                .conflicts
                .iter()
                .map(|c| ConflictingIdentityInfo {
                    reported_by: c.reported_by.clone(),
                    hosts: c.target.hosts.clone(),
                    port: c.target.port,
                    user: c.target.user.clone(),
                    fingerprints: c
                        .target
                        .host_keys
                        .iter()
                        .filter_map(|k| public_key_fingerprint(k))
                        .collect(),
                })
                .collect();
            IdentityChangedInfo {
                reported_by: pending.reported_by.clone(),
                hosts: pending.target.hosts.clone(),
                port: pending.target.port,
                user: pending.target.user.clone(),
                old_hosts,
                old_port,
                old_user,
                old_fingerprints,
                new_fingerprints,
                conflicts,
                is_paired: entry.origin == NodeOrigin::Paired,
            }
        });
        let (route_kind, route_via) = match raw_routes.get(&entry.node) {
            Some(MachineRouteStatus::IdentityChanged { reported_by }) => {
                ("identity_changed".to_owned(), Some(reported_by.clone()))
            }
            Some(MachineRouteStatus::NotSyncedYet { introducer, via }) => (
                "not_synced".to_owned(),
                introducer.clone().or_else(|| via.clone()),
            ),
            Some(MachineRouteStatus::Relay { via }) => ("relay".to_owned(), Some(via.clone())),
            _ => ("direct".to_owned(), None),
        };
        nodes.push(NodeLink {
            node: entry.node.clone(),
            principal: entry.principal.clone().unwrap_or_else(|| {
                if entry.local_program.is_some() {
                    "local".to_owned()
                } else {
                    String::new()
                }
            }),
            role: meta
                .and_then(|m| m.role.clone())
                .unwrap_or_else(|| "unknown".to_owned()),
            role_checked_at: meta.and_then(|m| m.role_checked_at.clone()),
            via: via.to_owned(),
            path,
            origin: origin.to_owned(),
            learned_from: entry.learned_from.clone(),
            route: route_kind,
            route_via,
            identity_changed,
        });
    }

    // Place the active node first so the interface always reads the active link at `nodes[0]`.
    if let Some(active) = &state.active_node
        && let Some(pos) = nodes.iter().position(|n| &n.node == active)
        && pos > 0
    {
        let chosen = nodes.remove(pos);
        nodes.insert(0, chosen);
    }

    let ready = !nodes.is_empty();
    let needs = if ready { None } else { Some(Needs::Pairing) };
    #[cfg_attr(any(target_os = "android", target_os = "ios"), allow(unused_mut))]
    let mut disagreements = state.disagreements.clone();
    #[cfg_attr(any(target_os = "android", target_os = "ios"), allow(unused_mut))]
    let mut learn_rejections = state.learn_rejections.clone();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if state.local_program.is_some() {
        let own_local = state
            .attached_local_node
            .as_deref()
            .filter(|name| state.data.link.node(name).is_none());
        let is_own_local =
            |name: &str| name == LOCAL_NODE_ID || own_local.is_some_and(|local| local == name);
        if let Some(local_name) = own_local
            && routes
                .get(local_name)
                .is_some_and(|view| view.kind != "direct")
        {
            routes.remove(local_name);
        }
        disagreements.retain(|d| !is_own_local(&d.machine));
        learn_rejections.retain(|r| !is_own_local(&r.machine));
    }

    AppState {
        ready,
        nodes,
        needs,
        repair: None,
        can_scan,
        routes,
        disagreements,
        learn_rejections,
        last_paired: state.last_paired.clone(),
        transfer_capabilities: TransferCapabilities::current_platform(),
    }
}

/// Discovers an installed `legion` binary on desktop hosts (absolute `PATH` entries first, then
/// conventional install locations) and returns its canonical filesystem path.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn discover_local_legion() -> Option<String> {
    use std::path::PathBuf;

    let mut conventional: Vec<PathBuf> = Vec::new();
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let home_dir = PathBuf::from(home);
            if home_dir.is_absolute() {
                conventional.push(home_dir.join(".local/bin/legion"));
            }
        }
        conventional.push(PathBuf::from("/usr/bin/legion"));
        conventional.push(PathBuf::from("/usr/local/bin/legion"));
        conventional.push(PathBuf::from("/opt/homebrew/bin/legion"));
    }
    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let base = PathBuf::from(local_app_data);
            if base.is_absolute() {
                conventional.push(base.join("legion").join("legion.exe"));
            }
        }
    }

    discover_local_legion_from(std::env::var_os("PATH").as_deref(), &conventional)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn discover_local_legion_from(
    path_var: Option<&std::ffi::OsStr>,
    conventional: &[std::path::PathBuf],
) -> Option<String> {
    let appdir = std::env::var_os("APPDIR")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    discover_local_legion_with_env(path_var, conventional, appdir.as_deref())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn discover_local_legion_with_env(
    path_var: Option<&std::ffi::OsStr>,
    conventional: &[std::path::PathBuf],
    appdir: Option<&std::path::Path>,
) -> Option<String> {
    use std::path::{Path, PathBuf};

    #[cfg(windows)]
    const BIN_NAME: &str = "legion.exe";
    #[cfg(not(windows))]
    const BIN_NAME: &str = "legion";

    let mut forbidden_roots: Vec<PathBuf> = Vec::new();
    let sys_tmp = std::env::temp_dir();
    forbidden_roots.push(sys_tmp.clone());
    if let Ok(canon_tmp) = std::fs::canonicalize(&sys_tmp) {
        forbidden_roots.push(canon_tmp);
    }
    #[cfg(unix)]
    for raw in [
        "/tmp",
        "/var/tmp",
        "/dev/shm",
        "/private/tmp",
        "/private/var/tmp",
    ] {
        let p = PathBuf::from(raw);
        forbidden_roots.push(p.clone());
        if let Ok(c) = std::fs::canonicalize(&p) {
            forbidden_roots.push(c);
        }
    }
    if let Some(dir) = appdir {
        forbidden_roots.push(dir.to_path_buf());
        if let Ok(c) = std::fs::canonicalize(dir) {
            forbidden_roots.push(c);
        }
    }

    let is_in_forbidden_root = |path: &Path| -> bool {
        #[cfg(windows)]
        {
            let norm = |p: &Path| -> String {
                let s = p.to_string_lossy();
                let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
                s.trim_end_matches(['\\', '/']).to_ascii_lowercase()
            };
            let target = norm(path);
            forbidden_roots.iter().any(|root| {
                let r = norm(root);
                !r.is_empty()
                    && (target == r
                        || target.starts_with(&format!("{r}\\"))
                        || target.starts_with(&format!("{r}/")))
            })
        }
        #[cfg(not(windows))]
        {
            forbidden_roots.iter().any(|root| path.starts_with(root))
        }
    };

    #[cfg(unix)]
    let is_safe_unix_path = |raw_candidate: &Path, canonical: &Path| -> bool {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if is_in_forbidden_root(raw_candidate) || is_in_forbidden_root(canonical) {
            return false;
        }
        let euid = rustix::process::geteuid().as_raw();

        // Check every ancestor directory of both the raw candidate parent and the canonical binary:
        // refuse any world-writable directory (including sticky temp dirs like 0o1777).
        for check_start in [raw_candidate.parent(), canonical.parent()] {
            let Some(mut cur) = check_start else {
                return false;
            };
            loop {
                let Ok(meta) = std::fs::symlink_metadata(cur) else {
                    return false;
                };
                if meta.permissions().mode() & 0o002 != 0 {
                    return false;
                }
                match cur.parent() {
                    Some(parent) if parent != cur => cur = parent,
                    _ => break,
                }
            }
        }

        let Ok(file_meta) = std::fs::symlink_metadata(canonical) else {
            return false;
        };
        if !file_meta.file_type().is_file() {
            return false;
        }
        let file_mode = file_meta.permissions().mode();
        if file_mode & 0o111 == 0 || file_mode & 0o002 != 0 {
            return false;
        }
        let file_uid = file_meta.uid();
        if file_uid != euid && !(file_uid == 0 && file_mode & 0o022 == 0) {
            return false;
        }

        // Immediate parent directory of the canonical binary must also be owned by the current
        // user or root (never another local user).
        let Some(parent) = canonical.parent() else {
            return false;
        };
        let Ok(parent_meta) = std::fs::metadata(parent) else {
            return false;
        };
        let parent_uid = parent_meta.uid();
        let parent_mode = parent_meta.permissions().mode();
        parent_uid == euid || (parent_uid == 0 && parent_mode & 0o002 == 0)
    };

    let resolve_candidate = |candidate: &Path| -> Option<String> {
        if is_in_forbidden_root(candidate) {
            return None;
        }
        let canonical = std::fs::canonicalize(candidate).ok()?;
        if is_in_forbidden_root(&canonical) {
            return None;
        }
        #[cfg(unix)]
        if !is_safe_unix_path(candidate, &canonical) {
            return None;
        }
        #[cfg(not(unix))]
        {
            let meta = std::fs::metadata(&canonical).ok()?;
            if !meta.is_file() {
                return None;
            }
        }
        let raw = canonical.to_string_lossy();
        #[cfg(windows)]
        if let Some(stripped) = raw.strip_prefix(r"\\?\")
            && stripped.as_bytes().get(1) == Some(&b':')
        {
            return Some(stripped.to_owned());
        }
        Some(raw.into_owned())
    };

    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(path_var) {
            // Never execute a relative `PATH` entry (such as `.` or empty `::`).
            if !dir.is_absolute() {
                continue;
            }
            let candidate = dir.join(BIN_NAME);
            if let Some(resolved) = resolve_candidate(&candidate) {
                return Some(resolved);
            }
        }
    }

    for candidate in conventional {
        if !candidate.is_absolute() {
            continue;
        }
        if let Some(resolved) = resolve_candidate(candidate) {
            return Some(resolved);
        }
    }

    None
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn discover_local_legion() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use legion_link::SshTarget;

    use super::*;

    fn sample_ssh_node(name: &str) -> LinkNode {
        LinkNode::ssh(
            name,
            SshTarget {
                hosts: vec![format!("{name}.example")],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec!["ssh-ed25519 AAAA".to_owned()],
            },
            Some("phone".to_owned()),
        )
    }

    #[tokio::test]
    async fn unpaired_without_local_binary_needs_pairing() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let backend = LinkBackend::with_local_program(store, None);

        let st = backend.state().await?;
        assert!(!st.ready);
        assert!(st.nodes.is_empty());
        assert_eq!(st.needs, Some(Needs::Pairing));

        let err = backend.tools(None).await.err().ok_or("expected error")?;
        assert_eq!(err.code, ErrorCode::NotPaired);
        Ok(())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn desktop_with_local_binary_is_ready_without_pairing()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let backend = LinkBackend::with_local_program(store, Some("/usr/bin/legion".to_owned()));

        let st = backend.state().await?;
        assert!(st.ready);
        assert_eq!(st.needs, None);
        assert_eq!(st.nodes.len(), 1);
        assert_eq!(st.nodes[0].node, "local");
        assert_eq!(st.nodes[0].via, "local");
        assert_eq!(st.nodes[0].path.as_deref(), Some("/usr/bin/legion"));

        let err = backend
            .unpair("local".to_owned())
            .await
            .err()
            .ok_or("expected error")?;
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        Ok(())
    }

    #[tokio::test]
    async fn unpair_forgets_locally_and_names_device_remove_command()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut data = AppStoreData::default();
        data.link.upsert_node(sample_ssh_node("tower"));
        data.link.upsert_node(sample_ssh_node("pi"));
        data.current_node = Some("tower".to_owned());
        data.upsert_meta(StoredNodeMeta {
            node: "tower".to_owned(),
            role: Some("operator".to_owned()),
            device: Some("pixel-phone".to_owned()),
            role_checked_at: None,
        });
        data.upsert_meta(StoredNodeMeta {
            node: "pi".to_owned(),
            role: Some("observer".to_owned()),
            device: Some("pixel-phone".to_owned()),
            role_checked_at: None,
        });
        store.save(&data)?;

        let backend = LinkBackend::with_local_program(store.clone(), None);
        let st = backend.state().await?;
        assert_eq!(st.nodes.len(), 2);
        assert_eq!(st.nodes[0].node, "tower");

        let selected = backend.select_node("pi".to_owned()).await?;
        assert_eq!(selected.nodes[0].node, "pi");

        let unpaired = backend.unpair("tower".to_owned()).await?;
        assert_eq!(unpaired.node, "tower");
        assert_eq!(unpaired.device, "pixel-phone");
        assert!(unpaired.message.contains("legion device.remove"));
        assert!(unpaired.message.contains("pixel-phone"));

        let after = backend.state().await?;
        assert_eq!(after.nodes.len(), 1);
        assert_eq!(after.nodes[0].node, "pi");

        let persisted = store.load()?;
        assert_eq!(persisted.link.nodes.len(), 1);
        assert_eq!(persisted.link.nodes[0].node, "pi");
        Ok(())
    }

    #[tokio::test]
    async fn damaged_state_file_reports_repair_refuses_saves_and_archives_explicitly()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut initial = AppStoreData::default();
        initial.link.upsert_node(sample_ssh_node("tower"));
        store.save(&initial)?;

        // Truncate the state file by 3 bytes to simulate a damaged file.
        let mut raw = std::fs::read(store.path())?;
        raw.truncate(raw.len().saturating_sub(3));
        std::fs::write(store.path(), &raw)?;

        let backend = LinkBackend::with_local_program(store.clone(), None);
        let st = backend.state().await?;
        assert!(!st.ready);
        assert_eq!(st.needs, Some(Needs::Repair));
        let repair_info = st.repair.ok_or("expected repair info")?;
        assert!(!repair_info.message.is_empty());
        assert_eq!(repair_info.path, store.path().display().to_string());

        // Every mutating and read operation refuses and leaves the damaged file untouched.
        assert!(backend.tools(None).await.is_err());
        assert!(
            backend
                .pair("legion-invite-1:abc".to_owned(), "phone".to_owned())
                .await
                .is_err()
        );
        assert!(backend.unpair("tower".to_owned()).await.is_err());
        assert!(backend.select_node("tower".to_owned()).await.is_err());
        assert_eq!(std::fs::read(store.path())?, raw);

        // Explicit repair moves the damaged file aside with original bytes intact and starts fresh.
        let repaired = backend.repair_store().await?;
        assert!(!store.path().exists());
        let archived_path = std::path::PathBuf::from(&repaired.archived_to);
        assert!(archived_path.exists());
        assert_eq!(std::fs::read(&archived_path)?, raw);
        assert!(repaired.message.contains("legion device.remove"));
        assert_eq!(repaired.state.needs, Some(Needs::Pairing));

        let after = backend.state().await?;
        assert_eq!(after.needs, Some(Needs::Pairing));
        assert!(after.repair.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn newer_schema_version_reports_repair_and_refuses_overwrite()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let future_json = serde_json::json!({
            "version": crate::store::STORE_VERSION + 5,
            "link": { "nodes": [] },
        })
        .to_string();
        std::fs::write(store.path(), &future_json)?;

        let backend = LinkBackend::with_local_program(store.clone(), None);
        let st = backend.state().await?;
        assert!(!st.ready);
        assert_eq!(st.needs, Some(Needs::Repair));
        assert!(
            st.repair
                .as_ref()
                .is_some_and(|r| r.message.contains("newer"))
        );
        assert!(backend.select_node("tower".to_owned()).await.is_err());
        assert_eq!(std::fs::read_to_string(store.path())?, future_json);
        Ok(())
    }

    fn unreachable_invite_text() -> Result<String, Box<dyn std::error::Error>> {
        let host_key = DeviceKey::generate()?.public_key_line("host")?;
        let invite = Invite {
            v: legion_link::INVITE_VERSION,
            id: "inv-test-1".to_owned(),
            node: "tower".to_owned(),
            hosts: vec!["127.0.0.1".to_owned()],
            port: 1,
            user: "me".to_owned(),
            host_keys: vec![host_key],
            key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
            principal: "phone".to_owned(),
            expires: "2099-01-01T00:00:00Z".to_owned(),
        };
        Ok(invite.encode()?)
    }

    #[tokio::test]
    async fn first_pair_persists_device_key_before_enrolling()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let backend = LinkBackend::with_local_program(store.clone(), None);
        let invite = unreachable_invite_text()?;

        // Enrolment fails on 127.0.0.1:1, yet the generated device key is already persisted on disk.
        let err = backend
            .pair(invite.clone(), "phone".to_owned())
            .await
            .err()
            .ok_or("expected enrolment error")?;
        assert!(matches!(
            err.code,
            ErrorCode::Unreachable | ErrorCode::Timeout
        ));

        let persisted = store.load()?;
        let first_key = persisted
            .link
            .device_key
            .clone()
            .ok_or("device key should be saved before enrolment")?;

        // A second attempt reuses the exact persisted device key.
        let _ = backend.pair(invite, "phone".to_owned()).await;
        assert_eq!(
            store.load()?.link.device_key.as_deref(),
            Some(first_key.as_str())
        );
        Ok(())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn first_pair_does_not_interrupt_in_flight_call() -> Result<(), Box<dyn std::error::Error>>
    {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::Notify;

        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let started_tx = Arc::clone(&started);
        let release_rx = Arc::clone(&release);

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let started_tx = Arc::clone(&started_tx);
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            )
                        } else {
                            started_tx.notify_one();
                            release_rx.notified().await;
                            Response::ok(req.id, serde_json::json!({ "ok": true }))
                        };
                        let mut out = serde_json::to_string(&reply).unwrap_or_default();
                        out.push('\n');
                        if server_w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = server_w.flush().await;
                    }
                });
                legion_link::Session::open_io(
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

        let backend = Arc::new(LinkBackend::with_opener(
            store,
            Some("/usr/bin/legion".to_owned()),
            opener,
        ));

        let call_backend = Arc::clone(&backend);
        let call_task = tokio::spawn(async move {
            call_backend
                .call(
                    CallRequest {
                        id: None,
                        tool: "sys.info".to_owned(),
                        args: serde_json::json!({}),
                        on: Some(Value::String("local".to_owned())),
                    },
                    None,
                )
                .await
        });

        started.notified().await;

        // First pairing generates & sets the device key in place without closing the local session.
        let invite = unreachable_invite_text()?;
        let _ = backend.pair(invite, "desktop".to_owned()).await;

        release.notify_one();
        let outcome = call_task.await??;
        assert_eq!(outcome, serde_json::json!({ "ok": true }));
        Ok(())
    }

    fn two_ssh_nodes_store(
        dir: &tempfile::TempDir,
    ) -> Result<AppStore, Box<dyn std::error::Error>> {
        let store = AppStore::in_dir(dir.path());
        let mut data = AppStoreData::default();
        data.link.upsert_node(sample_ssh_node("tower"));
        data.link.upsert_node(sample_ssh_node("pi"));
        data.current_node = Some("tower".to_owned());
        store.save(&data)?;
        Ok(store)
    }

    #[tokio::test]
    async fn call_lost_in_flight_is_not_retried() -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir()?;
        let store = two_ssh_nodes_store(&dir)?;
        let pi_opens = Arc::new(AtomicUsize::new(0));
        let pi_opens_c = Arc::clone(&pi_opens);

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let pi_opens_c = Arc::clone(&pi_opens_c);
            Box::pin(async move {
                if entry.node == "pi" {
                    pi_opens_c.fetch_add(1, Ordering::SeqCst);
                }
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            );
                            let mut out = serde_json::to_string(&reply).unwrap_or_default();
                            out.push('\n');
                            let _ = server_w.write_all(out.as_bytes()).await;
                            let _ = server_w.flush().await;
                        } else {
                            // Drop the server end after reading the request to simulate lost-in-flight.
                            break;
                        }
                    }
                });
                legion_link::Session::open_io(
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

        let backend = LinkBackend::with_opener(store, None, opener);
        let err = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.reboot".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("tower".to_owned())),
                },
                None,
            )
            .await
            .err()
            .ok_or("expected lost-in-flight error")?;
        assert_eq!(err.code, ErrorCode::Unreachable);
        assert_eq!(
            pi_opens.load(Ordering::SeqCst),
            0,
            "lost-in-flight call must never retry on a backup node"
        );
        Ok(())
    }

    #[tokio::test]
    async fn tool_timeout_is_not_retried() -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, RpcError, method};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir()?;
        let store = two_ssh_nodes_store(&dir)?;
        let pi_opens = Arc::new(AtomicUsize::new(0));
        let pi_opens_c = Arc::clone(&pi_opens);

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let pi_opens_c = Arc::clone(&pi_opens_c);
            Box::pin(async move {
                if entry.node == "pi" {
                    pi_opens_c.fetch_add(1, Ordering::SeqCst);
                }
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            )
                        } else {
                            Response::error(
                                req.id,
                                RpcError::new(
                                    legion_link::proto::TOOL_ERROR,
                                    legion_link::proto::ErrorBody::new(
                                        legion_link::proto::ErrorCode::Timeout,
                                        "tool timed out on tower",
                                    ),
                                ),
                            )
                        };
                        let mut out = serde_json::to_string(&reply).unwrap_or_default();
                        out.push('\n');
                        if server_w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = server_w.flush().await;
                    }
                });
                legion_link::Session::open_io(
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

        let backend = LinkBackend::with_opener(store, None, opener);
        let err = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.reboot".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("tower".to_owned())),
                },
                None,
            )
            .await
            .err()
            .ok_or("expected tool timeout error")?;
        assert_eq!(err.code, ErrorCode::Timeout);
        assert_eq!(
            pi_opens.load(Ordering::SeqCst),
            0,
            "tool timeout from primary node must never retry on a backup node"
        );
        Ok(())
    }

    #[tokio::test]
    async fn unsent_call_relays_with_explicit_on_without_changing_active_node()
    -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir()?;
        let store = two_ssh_nodes_store(&dir)?;
        let seen_on = Arc::new(Mutex::new(None::<Value>));
        let seen_on_c = Arc::clone(&seen_on);

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let seen_on_c = Arc::clone(&seen_on_c);
            Box::pin(async move {
                if entry.node == "tower" {
                    return Err(legion_link::proto::ErrorBody::new(
                        legion_link::proto::ErrorCode::Unreachable,
                        "tower is offline",
                    ));
                }
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            )
                        } else {
                            let on_val = req.params.get("on").cloned().unwrap_or(Value::Null);
                            *seen_on_c.lock().unwrap_or_else(|p| p.into_inner()) = Some(on_val);
                            Response::ok(req.id, serde_json::json!({ "relayed": true }))
                        };
                        let mut out = serde_json::to_string(&reply).unwrap_or_default();
                        out.push('\n');
                        if server_w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = server_w.flush().await;
                    }
                });
                legion_link::Session::open_io(
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

        let backend = LinkBackend::with_opener(store.clone(), None, opener);
        let outcome = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.reboot".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("tower".to_owned())),
                },
                None,
            )
            .await?;
        assert_eq!(outcome, serde_json::json!({ "relayed": true }));
        assert_eq!(
            seen_on.lock().unwrap_or_else(|p| p.into_inner()).as_ref(),
            Some(&Value::String("tower".to_owned())),
            "relay must preserve the named target machine in params.on"
        );

        // Relaying must NEVER switch active_node away from the user's chosen default.
        let st = backend.state().await?;
        assert_eq!(st.nodes[0].node, "tower");
        assert_eq!(st.nodes[1].node, "pi");

        // Machine-scoped tools (including meta.whoami, meta.version, device.invite, device.list)
        // and the tool list without `on` are refused.
        for tool in [
            "meta.whoami",
            "meta.version",
            "device.invite",
            "device.list",
            "sys.reboot",
        ] {
            let err = backend
                .call(
                    CallRequest {
                        id: None,
                        tool: tool.to_owned(),
                        args: serde_json::json!({}),
                        on: None,
                    },
                    None,
                )
                .await
                .err()
                .ok_or("expected InvalidArgs when on is omitted")?;
            assert_eq!(
                err.code,
                ErrorCode::InvalidArgs,
                "tool {tool} must require on"
            );
        }
        let tools_err = backend
            .tools(None)
            .await
            .err()
            .ok_or("expected InvalidArgs when tools() omits on")?;
        assert_eq!(tools_err.code, ErrorCode::InvalidArgs);

        // Unqualified discovery reads (`fleet.list`, `fleet.ping`, `device.endpoint`) remain
        // allowed and are labelled with the machine that answered.
        for discovery in ["fleet.list", "fleet.ping", "device.endpoint"] {
            let res = backend
                .call(
                    CallRequest {
                        id: None,
                        tool: discovery.to_owned(),
                        args: serde_json::json!({}),
                        on: None,
                    },
                    None,
                )
                .await?;
            assert_eq!(
                res.get("answered_by"),
                Some(&Value::String("pi".to_owned())),
                "discovery tool {discovery} must be labelled with answered_by"
            );
        }

        // Persisted default node stays "tower".
        let persisted = store.load()?;
        assert_eq!(persisted.current_node.as_deref(), Some("tower"));
        Ok(())
    }

    #[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
    #[test]
    fn discover_local_legion_skips_relative_path_and_canonicalizes_symlinks()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::{PermissionsExt, symlink};

        // Place the valid test fixture inside the user-owned target directory (not /tmp, which
        // discover_local_legion intentionally refuses as a world-writable temp location).
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-discover");
        std::fs::create_dir_all(&base)?;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
        let dir = tempfile::tempdir_in(&base)?;
        let real_dir = dir.path().join("real");
        let link_dir = dir.path().join("bin");
        std::fs::create_dir_all(&real_dir)?;
        std::fs::create_dir_all(&link_dir)?;

        let real_bin = real_dir.join("legion-actual");
        std::fs::write(&real_bin, "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&real_bin, std::fs::Permissions::from_mode(0o755))?;

        let link_bin = link_dir.join("legion");
        symlink(&real_bin, &link_bin)?;

        // Relative PATH entries (including `.` and empty segments) must be skipped.
        let relative_only =
            std::env::join_paths([std::path::PathBuf::from("."), std::path::PathBuf::from("")])?;
        assert_eq!(discover_local_legion_from(Some(&relative_only), &[]), None);

        // An absolute PATH entry pointing through a symlink resolves to the canonical real file.
        let mixed_path = std::env::join_paths([std::path::PathBuf::from("."), link_dir])?;
        let found = discover_local_legion_from(Some(&mixed_path), &[])
            .ok_or("expected canonical legion binary")?;
        assert_eq!(found, std::fs::canonicalize(&real_bin)?.to_string_lossy());
        Ok(())
    }

    #[cfg(all(unix, not(any(target_os = "android", target_os = "ios"))))]
    #[test]
    fn discover_local_legion_refuses_tmp_appdir_and_world_writable_dirs()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        // 1. A binary placed inside /tmp (system temp dir) must be refused.
        let tmp_dir = tempfile::tempdir()?;
        let tmp_bin = tmp_dir.path().join("legion");
        std::fs::write(&tmp_bin, "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&tmp_bin, std::fs::Permissions::from_mode(0o755))?;
        let tmp_path = std::env::join_paths([tmp_dir.path()])?;
        assert_eq!(
            discover_local_legion_with_env(Some(&tmp_path), &[], None),
            None,
            "legion binary in /tmp must be refused"
        );

        // 2. A binary inside a world-writable directory (or world-writable binary) outside /tmp
        //    must be refused.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-discover");
        std::fs::create_dir_all(&base)?;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
        let safe_root = tempfile::tempdir_in(&base)?;

        let ww_dir = safe_root.path().join("world-writable");
        std::fs::create_dir_all(&ww_dir)?;
        std::fs::set_permissions(&ww_dir, std::fs::Permissions::from_mode(0o777))?;
        let ww_bin = ww_dir.join("legion");
        std::fs::write(&ww_bin, "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&ww_bin, std::fs::Permissions::from_mode(0o755))?;
        let ww_path = std::env::join_paths([&ww_dir])?;
        assert_eq!(
            discover_local_legion_with_env(Some(&ww_path), &[], None),
            None,
            "legion binary inside world-writable directory must be refused"
        );

        let ww_file_dir = safe_root.path().join("ww-file");
        std::fs::create_dir_all(&ww_file_dir)?;
        std::fs::set_permissions(&ww_file_dir, std::fs::Permissions::from_mode(0o700))?;
        let ww_file_bin = ww_file_dir.join("legion");
        std::fs::write(&ww_file_bin, "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&ww_file_bin, std::fs::Permissions::from_mode(0o777))?;
        let ww_file_path = std::env::join_paths([&ww_file_dir])?;
        assert_eq!(
            discover_local_legion_with_env(Some(&ww_file_path), &[], None),
            None,
            "world-writable legion binary must be refused"
        );

        // 3. A binary inside $APPDIR must be refused even when permissions are 0700.
        let fake_appdir = safe_root.path().join("appdir");
        let fake_appdir_bin = fake_appdir.join("usr/bin");
        std::fs::create_dir_all(&fake_appdir_bin)?;
        let appdir_legion = fake_appdir_bin.join("legion");
        std::fs::write(&appdir_legion, "#!/bin/sh\nexit 0\n")?;
        std::fs::set_permissions(&appdir_legion, std::fs::Permissions::from_mode(0o755))?;
        let appdir_path = std::env::join_paths([&fake_appdir_bin])?;
        assert_eq!(
            discover_local_legion_with_env(Some(&appdir_path), &[], Some(&fake_appdir)),
            None,
            "legion binary inside $APPDIR must be refused"
        );
        Ok(())
    }

    #[tokio::test]
    async fn invalidate_sessions_closes_cached_session_so_next_call_reconnects()
    -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir()?;
        let store = two_ssh_nodes_store(&dir)?;
        let opens = Arc::new(AtomicUsize::new(0));
        let opens_c = Arc::clone(&opens);

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let opens_c = Arc::clone(&opens_c);
            Box::pin(async move {
                opens_c.fetch_add(1, Ordering::SeqCst);
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            )
                        } else {
                            Response::ok(req.id, serde_json::json!({ "ok": true }))
                        };
                        let mut out = serde_json::to_string(&reply).unwrap_or_default();
                        out.push('\n');
                        if server_w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = server_w.flush().await;
                    }
                });
                legion_link::Session::open_io(
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

        let backend = LinkBackend::with_opener(store, None, opener);
        backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.info".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("tower".to_owned())),
                },
                None,
            )
            .await?;
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        // Second call reuses the open session.
        backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.info".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("tower".to_owned())),
                },
                None,
            )
            .await?;
        assert_eq!(opens.load(Ordering::SeqCst), 1);

        // Invalidating sessions tears down cached sockets so the next call opens a fresh one.
        backend.invalidate_sessions().await?;
        backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.info".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("tower".to_owned())),
                },
                None,
            )
            .await?;
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn learn_persists_direct_routes_handles_identity_change_and_keeps_working_when_a_is_off()
    -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let key_a = DeviceKey::from_seed([1u8; 32]).public_key_line("a")?;
        let key_b1 = DeviceKey::from_seed([2u8; 32]).public_key_line("b1")?;
        let key_b2 = DeviceKey::from_seed([3u8; 32]).public_key_line("b2")?;

        let mut initial = AppStoreData::default();
        initial.link.device_key = Some(DeviceKey::from_seed([9u8; 32]).to_openssh()?);
        initial.link.upsert_node(LinkNode::ssh(
            "tower",
            SshTarget {
                hosts: vec!["tower.example".to_owned()],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec![key_a.clone()],
            },
            Some("device:pixel".to_owned()),
        ));
        initial.current_node = Some("tower".to_owned());
        initial.upsert_meta(StoredNodeMeta {
            node: "tower".to_owned(),
            role: Some("owner".to_owned()),
            device: Some("pixel".to_owned()),
            role_checked_at: None,
        });
        store.save(&initial)?;

        let tower_down = Arc::new(AtomicBool::new(false));
        let rekey_b = Arc::new(AtomicBool::new(false));
        let tower_down_c = Arc::clone(&tower_down);
        let rekey_b_c = Arc::clone(&rekey_b);
        let key_a_c = key_a.clone();
        let key_b1_c = key_b1.clone();
        let key_b2_c = key_b2.clone();

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let tower_down_c = Arc::clone(&tower_down_c);
            let rekey_b_c = Arc::clone(&rekey_b_c);
            let key_a_c = key_a_c.clone();
            let key_b1_c = key_b1_c.clone();
            let key_b2_c = key_b2_c.clone();
            Box::pin(async move {
                if entry.node == "tower" && tower_down_c.load(Ordering::SeqCst) {
                    return Err(legion_link::proto::ErrorBody::new(
                        legion_link::proto::ErrorCode::Unreachable,
                        "tower is off",
                    )
                    .with_machine("tower"));
                }
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            )
                        } else if req.params.get("tool").and_then(Value::as_str)
                            == Some("device.endpoint")
                        {
                            let b_key = if rekey_b_c.load(Ordering::SeqCst) {
                                key_b2_c.clone()
                            } else {
                                key_b1_c.clone()
                            };
                            Response::ok(
                                req.id,
                                serde_json::json!({
                                    "results": {
                                        "tower": {
                                            "ok": {
                                                "node": "tower",
                                                "hosts": ["tower.example"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [key_a_c],
                                            }
                                        },
                                        "pi": {
                                            "ok": {
                                                "node": "pi",
                                                "hosts": ["pi.example"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [b_key],
                                            }
                                        }
                                    }
                                }),
                            )
                        } else {
                            Response::ok(
                                req.id,
                                serde_json::json!({ "node": node_name, "ok": true }),
                            )
                        };
                        let mut out = serde_json::to_string(&reply).unwrap_or_default();
                        out.push('\n');
                        if server_w.write_all(out.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = server_w.flush().await;
                    }
                });
                legion_link::Session::open_io(
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

        let backend = LinkBackend::with_opener(store.clone(), None, opener);
        let learned = backend.learn().await?;
        assert_eq!(learned.report.added, vec!["pi".to_owned()]);
        assert_eq!(learned.state.nodes.len(), 2);
        let pi_view = learned
            .state
            .nodes
            .iter()
            .find(|n| n.node == "pi")
            .ok_or("pi missing from state")?;
        assert_eq!(pi_view.origin, "learned");
        assert_eq!(pi_view.learned_from.as_deref(), Some("tower"));
        assert_eq!(pi_view.route, "direct");
        // Learned machine must NOT inherit introducer's role or device metadata.
        assert_eq!(pi_view.role, "unknown");
        assert!(store.load()?.meta("pi").is_none());

        // Persisted store now contains learned `pi`.
        assert!(store.load()?.link.node("pi").is_some());

        // Turn `tower` off: `fleet.list` (any-node) and direct calls to `pi` still succeed!
        backend.invalidate_sessions().await?;
        tower_down.store(true, Ordering::SeqCst);
        let list_res = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "fleet.list".to_owned(),
                    args: serde_json::json!({}),
                    on: None,
                },
                None,
            )
            .await?;
        assert_eq!(list_res["node"], "pi");
        let pi_call = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "sys.info".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("pi".to_owned())),
                },
                None,
            )
            .await?;
        assert_eq!(pi_call["node"], "pi");

        // Rekey `pi` and learn again: `identity_changed` is flagged and persisted; accepting
        // requires the exact fingerprints shown to the user and saves before updating live Link.
        tower_down.store(false, Ordering::SeqCst);
        rekey_b.store(true, Ordering::SeqCst);
        let learned2 = backend.learn().await?;
        assert_eq!(learned2.report.identity_changed, vec!["pi".to_owned()]);
        let pi_warn = learned2
            .state
            .nodes
            .iter()
            .find(|n| n.node == "pi")
            .ok_or("pi missing")?;
        assert_eq!(pi_warn.route, "identity_changed");
        let ic = pi_warn
            .identity_changed
            .as_ref()
            .ok_or("expected identity_changed info")?;
        assert_eq!(ic.reported_by, "tower");
        assert!(!ic.is_paired);
        assert_eq!(ic.old_hosts, vec!["pi.example".to_owned()]);
        assert_eq!(ic.hosts, vec!["pi.example".to_owned()]);
        assert_eq!(ic.old_port, 22);
        assert_eq!(ic.port, 22);
        assert_eq!(ic.old_user, "me");
        assert_eq!(ic.user, "me");
        assert!(!ic.old_fingerprints.is_empty());
        assert!(!ic.new_fingerprints.is_empty());
        assert_ne!(ic.old_fingerprints, ic.new_fingerprints);

        // Wrong fingerprint is refused.
        let wrong_err = backend
            .accept_identity_change("pi".to_owned(), vec!["SHA256:wrong".to_owned()])
            .await
            .err()
            .ok_or("expected conflict on wrong fingerprint")?;
        assert_eq!(wrong_err.code, ErrorCode::Conflict);

        // Exact displayed fingerprints succeed and persist.
        let accepted_state = backend
            .accept_identity_change("pi".to_owned(), ic.new_fingerprints.clone())
            .await?;
        let pi_ok = accepted_state
            .nodes
            .iter()
            .find(|n| n.node == "pi")
            .ok_or("pi missing")?;
        assert!(pi_ok.identity_changed.is_none());
        assert_eq!(
            store
                .load()?
                .link
                .node("pi")
                .and_then(|n| n.target.as_ref())
                .map(|t| t.host_keys.clone()),
            Some(vec![key_b2])
        );

        // Unpairing `tower` drops learned `pi` introduced by it and remembers `tower` as forgotten.
        backend.unpair("tower".to_owned()).await?;
        let after_unpair = store.load()?;
        assert!(after_unpair.link.node("tower").is_none());
        assert!(after_unpair.link.node("pi").is_none());
        assert!(after_unpair.link.is_forgotten("tower"));
        Ok(())
    }

    #[tokio::test]
    async fn accept_identity_change_does_not_mutate_live_link_when_store_save_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let key_a = DeviceKey::from_seed([11u8; 32]).public_key_line("a")?;
        let key_b1 = DeviceKey::from_seed([12u8; 32]).public_key_line("b1")?;
        let key_b2 = DeviceKey::from_seed([13u8; 32]).public_key_line("b2")?;
        let fp_b2 = public_key_fingerprint(&key_b2).ok_or("fingerprint")?;

        let mut initial = AppStoreData::default();
        let mut pi = LinkNode::learned(
            "pi",
            SshTarget {
                hosts: vec!["pi.example".to_owned()],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec![key_b1.clone()],
            },
            None,
            "tower",
        );
        pi.identity_changed = Some(legion_link::PendingIdentityChange {
            target: SshTarget {
                hosts: vec!["pi2.example".to_owned()],
                port: 2222,
                user: "me".to_owned(),
                host_keys: vec![key_b2.clone()],
            },
            reported_by: "tower".to_owned(),
            conflicts: Vec::new(),
        });
        initial.link.upsert_node(LinkNode::ssh(
            "tower",
            SshTarget {
                hosts: vec!["tower.example".to_owned()],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec![key_a],
            },
            Some("device:pixel".to_owned()),
        ));
        initial.link.upsert_node(pi);
        store.save(&initial)?;

        let backend = LinkBackend::with_local_program(store.clone(), None);

        // Replace store.path() with a non-empty directory so atomic persist fails cleanly.
        std::fs::remove_file(store.path())?;
        std::fs::create_dir(store.path())?;
        std::fs::write(store.path().join("blocker"), b"x")?;

        let err = backend
            .accept_identity_change("pi".to_owned(), vec![fp_b2.clone()])
            .await
            .err()
            .ok_or("expected save failure")?;
        assert_eq!(err.code, ErrorCode::Internal);

        // Live Link must STILL hold the original key_b1 and pending identity_changed!
        let live_pi = {
            let guard = backend.state.lock().unwrap_or_else(|p| p.into_inner());
            guard.link.config().node("pi").cloned()
        }
        .ok_or("pi missing")?;
        assert_eq!(
            live_pi.target.as_ref().map(|t| t.host_keys.clone()),
            Some(vec![key_b1])
        );
        assert!(live_pi.identity_changed.is_some());

        // Restore store path and verify dismiss_identity_change works and persists.
        std::fs::remove_dir_all(store.path())?;
        store.save(&initial)?;
        let dismissed = backend.dismiss_identity_change("pi".to_owned()).await?;
        let pi_after = dismissed
            .nodes
            .iter()
            .find(|n| n.node == "pi")
            .ok_or("pi missing")?;
        assert!(pi_after.identity_changed.is_none());
        assert!(
            store
                .load()?
                .link
                .node("pi")
                .and_then(|n| n.identity_changed.as_ref())
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn pair_summary_preserves_partial_status() {
        let mut fleet = BTreeMap::new();
        fleet.insert(
            "pi".to_owned(),
            legion_link::FleetPushOutcome {
                status: "partial".to_owned(),
                reason: Some("live device changes refused".to_owned()),
            },
        );
        fleet.insert(
            "studio".to_owned(),
            legion_link::FleetPushOutcome {
                status: "synced".to_owned(),
                reason: None,
            },
        );
        let enrolled = legion_link::Enrolled {
            node: "tower".to_owned(),
            target: SshTarget {
                hosts: vec!["tower.example".to_owned()],
                port: 22,
                user: "me".to_owned(),
                host_keys: vec!["ssh-ed25519 AAAA".to_owned()],
            },
            principal: "device:pixel".to_owned(),
            role: "owner".to_owned(),
            device: "pixel".to_owned(),
            fleet,
            not_synced_to: vec!["pi".to_owned()],
            sync_command: Some("legion fleet.sync".to_owned()),
            recovered: false,
        };
        let summary = build_pair_summary(&enrolled);
        assert_eq!(summary.synced, vec!["studio".to_owned()]);
        assert_eq!(summary.not_synced.len(), 1);
        assert_eq!(summary.not_synced[0].node, "pi");
        assert_eq!(summary.not_synced[0].status.as_deref(), Some("partial"));
        assert_eq!(
            summary.not_synced[0].reason.as_deref(),
            Some("live device changes refused")
        );
    }

    #[tokio::test]
    async fn active_operations_tracks_in_flight_calls_and_guards_and_releases_on_drop()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let backend = LinkBackend::with_local_program(store, None);
        assert_eq!(backend.active_operations(), 0);

        {
            let _g1 = backend.track_operation();
            assert_eq!(backend.active_operations(), 1);
            {
                let _g2 = backend.track_operation();
                assert_eq!(backend.active_operations(), 2);
            }
            assert_eq!(backend.active_operations(), 1);
        }
        assert_eq!(backend.active_operations(), 0);

        // Even a failing `call` / `pair` releases its counter cleanly when it returns.
        let _ = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "exec.run".to_owned(),
                    args: serde_json::json!({ "cmd": "true" }),
                    on: Some(serde_json::json!("tower")),
                },
                None,
            )
            .await;
        let _ = backend.pair(String::new(), "phone".to_owned()).await;
        assert_eq!(backend.active_operations(), 0);
        assert_eq!(backend.background_refreshes(), 0);
        Ok(())
    }

    #[test]
    fn background_refresh_classification_separates_passive_polls_from_user_started_work() {
        // Passive non-streaming refresh reads (including Machine Overview's 5s sys.info/sys.procs).
        for tool in [
            "fleet.list",
            "fleet.ping",
            "sys.info",
            "sys.procs",
            "power.status",
            "job.list",
            "meta.whoami",
            "meta.version",
            "fs.list",
            "fs.stat",
            "fs.read",
            "fs.search",
            "fav.list",
            "sync.list",
        ] {
            assert!(
                is_background_refresh_call(tool, false),
                "expected non-streaming {tool} to be classified as a background refresh"
            );
            assert!(
                !is_background_refresh_call(tool, true),
                "streaming {tool} must always count as user-started active work"
            );
        }

        // User-started / effectful calls and unknown tools always block updates (fail-closed).
        for tool in [
            "exec.run",
            "sys.kill",
            "power.sleep",
            "power.reboot",
            "power.shutdown",
            "power.wake",
            "power.boot",
            "job.start",
            "job.cancel",
            "service.restart",
            "project.run",
            "fs.write",
            "fs.delete",
            "fs.move",
            "fs.copy",
            "fs.mkdir",
            "device.invite",
            "device.remove",
            "config.write",
            "update.publish",
            "meta.update",
            "unknown.future_tool",
        ] {
            assert!(
                !is_background_refresh_call(tool, false),
                "expected {tool} to count as user-started active work"
            );
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test(start_paused = true)]
    async fn passive_overview_polls_do_not_increment_active_operations_and_drain_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Request, Response, method};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::sync::Semaphore;

        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let call_seen = Arc::new(AtomicUsize::new(0));
        let call_started = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let clock = ManualDrainClock::new();

        let seen_tx = Arc::clone(&call_seen);
        let started_tx = Arc::clone(&call_started);
        let release_rx = Arc::clone(&release);

        let opener: legion_link::SessionOpener = Arc::new(move |entry, _, cancel| {
            let seen_tx = Arc::clone(&seen_tx);
            let started_tx = Arc::clone(&started_tx);
            let release_rx = Arc::clone(&release_rx);
            Box::pin(async move {
                let node_name = entry.node.clone();
                let (client, server) = tokio::io::duplex(64 * 1024);
                let (client_r, client_w) = tokio::io::split(client);
                tokio::spawn(async move {
                    let (server_r, mut server_w) = tokio::io::split(server);
                    let mut lines = BufReader::new(server_r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Request = serde_json::from_str(&line).unwrap_or_else(|_| {
                            Request::new(legion_link::proto::Id::Number(0), "", Value::Null)
                        });
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
                                .unwrap_or(Value::Null),
                            );
                            let mut out = serde_json::to_string(&reply).unwrap_or_default();
                            out.push('\n');
                            if server_w.write_all(out.as_bytes()).await.is_err() {
                                break;
                            }
                            let _ = server_w.flush().await;
                        } else {
                            // Hold each tool call on a permit-storing semaphore so wakeups can
                            // never be lost under load.
                            seen_tx.fetch_add(1, Ordering::SeqCst);
                            started_tx.notify_waiters();
                            let Ok(permit) = release_rx.acquire().await else {
                                break;
                            };
                            permit.forget();
                            let reply = Response::ok(req.id, serde_json::json!({ "ok": true }));
                            let mut out = serde_json::to_string(&reply).unwrap_or_default();
                            out.push('\n');
                            if server_w.write_all(out.as_bytes()).await.is_err() {
                                break;
                            }
                            let _ = server_w.flush().await;
                        }
                    }
                });
                legion_link::Session::open_io(
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

        let backend = Arc::new(LinkBackend::with_opener_and_clock(
            store,
            Some("/usr/bin/legion".to_owned()),
            opener,
            Arc::clone(&clock) as Arc<dyn DrainClock>,
        ));

        let wait_for_seen = |target: usize| {
            let call_seen = Arc::clone(&call_seen);
            let call_started = Arc::clone(&call_started);
            async move {
                loop {
                    let notified = call_started.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if call_seen.load(Ordering::SeqCst) >= target {
                        return;
                    }
                    notified.await;
                }
            }
        };

        // 1. Machine Overview polling `sys.info` + `sys.procs` concurrently:
        //    `active_operations()` stays 0 while `background_refreshes()` tracks both calls.
        let b1 = Arc::clone(&backend);
        let sys_task = tokio::spawn(async move {
            b1.call(
                CallRequest {
                    id: None,
                    tool: "sys.info".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(Value::String("local".to_owned())),
                },
                None,
            )
            .await
        });
        let b2 = Arc::clone(&backend);
        let procs_task = tokio::spawn(async move {
            b2.call(
                CallRequest {
                    id: None,
                    tool: "sys.procs".to_owned(),
                    args: serde_json::json!({ "group": true, "limit": 12 }),
                    on: Some(Value::String("local".to_owned())),
                },
                None,
            )
            .await
        });

        wait_for_seen(1).await;
        while backend.background_refreshes() < 2 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            backend.active_operations(),
            0,
            "Overview sys.info/sys.procs refreshes must not mark active_operations > 0"
        );
        assert_eq!(backend.background_refreshes(), 2);

        // Waiting past the grace timeout while refreshes are still mid-call returns false cleanly
        // rather than proceeding mid-call or hanging (driven deterministically via ManualDrainClock).
        let b_timeout_async = Arc::clone(&backend);
        let timeout_async_task = tokio::spawn(async move {
            b_timeout_async
                .wait_for_background_refreshes(Duration::from_secs(2))
                .await
        });
        let b_timeout_sync = Arc::clone(&backend);
        let timeout_sync_task = tokio::task::spawn_blocking(move || {
            b_timeout_sync.wait_for_background_refreshes_sync(Duration::from_secs(2))
        });
        clock.wait_for_waiters(2).await;
        clock.advance(Duration::from_secs(2));
        assert!(!timeout_async_task.await?);
        assert!(!timeout_sync_task.await?);

        // Once the in-flight refreshes finish, both async and sync waiters unblock immediately
        // without advancing the clock.
        let b_wait = Arc::clone(&backend);
        let wait_async_task = tokio::spawn(async move {
            b_wait
                .wait_for_background_refreshes(Duration::from_secs(2))
                .await
        });
        let b_sync_wait = Arc::clone(&backend);
        let wait_sync_task = tokio::task::spawn_blocking(move || {
            b_sync_wait.wait_for_background_refreshes_sync(Duration::from_secs(2))
        });
        clock.wait_for_waiters(2).await;
        release.add_permits(2);
        assert!(wait_async_task.await?);
        assert!(wait_sync_task.await?);
        assert!(sys_task.await?.is_ok());
        assert!(procs_task.await?.is_ok());
        assert_eq!(backend.background_refreshes(), 0);
        assert_eq!(backend.active_operations(), 0);

        // 2. A user-started action (`sys.kill`) DOES increment `active_operations()` while in
        //    flight and leaves `background_refreshes()` at 0.
        call_seen.store(0, Ordering::SeqCst);
        let b3 = Arc::clone(&backend);
        let kill_task = tokio::spawn(async move {
            b3.call(
                CallRequest {
                    id: None,
                    tool: "sys.kill".to_owned(),
                    args: serde_json::json!({ "pid": 4242 }),
                    on: Some(Value::String("local".to_owned())),
                },
                None,
            )
            .await
        });
        wait_for_seen(1).await;
        assert_eq!(
            backend.active_operations(),
            1,
            "user-started sys.kill must increment active_operations()"
        );
        assert_eq!(backend.background_refreshes(), 0);
        release.add_permits(1);
        assert!(kill_task.await?.is_ok());
        assert_eq!(backend.active_operations(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn background_refresh_tracker_real_clock_smoke() -> Result<(), Box<dyn std::error::Error>>
    {
        // Single real-time smoke test exercising `RealDrainClock` across both async and sync
        // waiters with margins orders of magnitude apart (20 ms timeout vs 10 s unblock ceiling).
        let tracker = Arc::new(BackgroundRefreshTracker::default());
        {
            let mut count = tracker
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *count = 1;
        }
        let guard = BackgroundRefreshGuard(Arc::clone(&tracker));

        assert!(!tracker.wait_async(Duration::from_millis(20)).await);
        let t_sync_timeout = Arc::clone(&tracker);
        assert!(
            !tokio::task::spawn_blocking(move || {
                t_sync_timeout.wait_sync(Duration::from_millis(20))
            })
            .await?
        );

        let t_async = Arc::clone(&tracker);
        let async_waiter =
            tokio::spawn(async move { t_async.wait_async(Duration::from_secs(10)).await });
        let t_sync = Arc::clone(&tracker);
        let sync_waiter =
            tokio::task::spawn_blocking(move || t_sync.wait_sync(Duration::from_secs(10)));
        drop(guard);

        assert!(async_waiter.await?);
        assert!(sync_waiter.await?);
        assert_eq!(tracker.count(), 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn background_refresh_tracker_manual_clock_unit() -> Result<(), Box<dyn std::error::Error>>
    {
        // Exercises `ManualDrainClock` on every target (including Android/iOS).
        let clock = ManualDrainClock::new();
        let tracker = Arc::new(BackgroundRefreshTracker::with_clock(
            Arc::clone(&clock) as Arc<dyn DrainClock>
        ));
        {
            let mut count = tracker
                .in_flight
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *count = 1;
        }
        let guard = BackgroundRefreshGuard(Arc::clone(&tracker));

        let t_async = Arc::clone(&tracker);
        let async_timeout =
            tokio::spawn(async move { t_async.wait_async(Duration::from_secs(2)).await });
        clock.wait_for_waiter().await;
        let t_sync = Arc::clone(&tracker);
        let sync_timeout =
            tokio::task::spawn_blocking(move || t_sync.wait_sync(Duration::from_secs(2)));
        clock.wait_for_waiters(2).await;
        clock.advance(Duration::from_secs(2));
        assert!(!async_timeout.await?);
        assert!(!sync_timeout.await?);

        let t_async2 = Arc::clone(&tracker);
        let async_ok =
            tokio::spawn(async move { t_async2.wait_async(Duration::from_secs(2)).await });
        let t_sync2 = Arc::clone(&tracker);
        let sync_ok =
            tokio::task::spawn_blocking(move || t_sync2.wait_sync(Duration::from_secs(2)));
        clock.wait_for_waiters(2).await;
        drop(guard);
        assert!(async_ok.await?);
        assert!(sync_ok.await?);
        Ok(())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn paired_ssh_node_takes_precedence_over_local_pipe_and_never_guesses_from_disk()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut initial = AppStoreData::default();
        initial.link.upsert_node(sample_ssh_node("tower"));
        store.save(&initial)?;

        let backend = LinkBackend::with_local_program(store, Some("/usr/bin/legion".to_owned()));

        // Even if the attached local node happened to report the same name `"tower"`, calls and
        // content requests for `"tower"` MUST still route to the paired SSH node `"tower"` with
        // its own pairing holder, while `"local"` addresses the local pipe.
        {
            let mut guard = backend.state.lock().unwrap_or_else(|p| p.into_inner());
            guard.attached_local_node = Some("tower".to_owned());
        }

        assert_eq!(backend.resolve_link_node_for_call("tower"), "tower");
        assert_eq!(backend.resolve_link_node_for_call("local"), "local");

        let ssh_target = backend.content_target("tower")?;
        assert_eq!(ssh_target.link_node, "tower");
        assert_eq!(ssh_target.principal, "phone");
        assert!(ssh_target.holder.starts_with("pairing:tower:"));

        let local_target = backend.content_target("local")?;
        assert_eq!(local_target.link_node, "local");
        assert_eq!(local_target.principal, "local");
        assert!(local_target.holder.starts_with("pairing:local:"));

        // Without a live `meta.whoami` identity proof, an unpaired machine name is never guessed
        // onto the local pipe: it routes as `"tower"` via the link (and an empty app with neither
        // paired nodes nor a local binary still fails with `NotPaired`).
        let fresh_dir = tempfile::tempdir()?;
        let fresh_backend = LinkBackend::with_local_program(
            AppStore::in_dir(fresh_dir.path()),
            Some("/usr/bin/legion".to_owned()),
        );
        let relayed_target = fresh_backend.content_target("tower")?;
        assert_eq!(relayed_target.link_node, "tower");
        assert!(relayed_target.holder.starts_with("fleet:"));
        assert!(fresh_backend.is_pairing_holder_live(&relayed_target.holder));
        assert_eq!(fresh_backend.content_target("local")?.link_node, "local");

        let empty_dir = tempfile::tempdir()?;
        let empty_backend =
            LinkBackend::with_local_program(AppStore::in_dir(empty_dir.path()), None);
        assert_eq!(
            empty_backend
                .content_target("tower")
                .err()
                .ok_or("expected NotPaired error")?
                .code,
            ErrorCode::NotPaired
        );
        Ok(())
    }

    #[tokio::test]
    async fn fleet_relayed_content_holder_revokes_on_unpair_and_learn_rejections_surface_in_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut initial = AppStoreData::default();
        initial.link.upsert_node(sample_ssh_node("tower"));
        store.save(&initial)?;

        let backend = LinkBackend::with_local_program(store, None);

        // Phone paired ONLY with `tower` can open content targets for fleet machine `studio`
        // (never returning "not paired on this device").
        let studio_target = backend.content_target("studio")?;
        assert_eq!(studio_target.link_node, "studio");
        assert!(studio_target.holder.starts_with("fleet:"));
        assert!(backend.is_pairing_holder_live(&studio_target.holder));

        // Recording learn rejections surfaces them in `AppState::learn_rejections`.
        {
            let mut guard = backend.state.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .learn_rejections
                .push(crate::backend::LearnRejectionView {
                    machine: "studio".to_owned(),
                    code: "name_mismatch".to_owned(),
                    reason: "studio calls itself someones-laptop.".to_owned(),
                    reported_by: Some("tower".to_owned()),
                });
        }
        let st = backend.state().await?;
        assert_eq!(st.learn_rejections.len(), 1);
        assert_eq!(st.learn_rejections[0].machine, "studio");
        assert_eq!(st.learn_rejections[0].code, "name_mismatch");
        assert_eq!(
            st.learn_rejections[0].reason,
            "studio calls itself someones-laptop."
        );
        assert_eq!(st.learn_rejections[0].reported_by.as_deref(), Some("tower"));

        // Unpairing `tower` bumps the fleet epoch (revoking the relayed holder) and prunes
        // rejections reported by `tower`.
        backend.unpair("tower".to_owned()).await?;
        assert!(!backend.is_pairing_holder_live(&studio_target.holder));
        let after = backend.state().await?;
        assert!(after.learn_rejections.is_empty());
        assert_eq!(
            backend
                .content_target("studio")
                .err()
                .ok_or("expected NotPaired error")?
                .code,
            ErrorCode::NotPaired
        );
        Ok(())
    }

    #[tokio::test]
    async fn pair_bumps_fleet_epoch_and_learn_stamps_learned_direct_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{
            HelloResult, PROTOCOL, Request as ProtoReq, Response as ProtoResp, VERSION,
        };
        use legion_link::{Session, SessionOpener, SshTarget};
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let studio_key = DeviceKey::from_seed([81u8; 32]).public_key_line("studio")?;
        let mut initial = AppStoreData::default();
        initial.link.device_key = Some(DeviceKey::from_seed([80u8; 32]).to_openssh()?);
        initial.link.upsert_node(sample_ssh_node("tower"));
        store.save(&initial)?;

        let studio_key_for_opener = studio_key.clone();
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let node_name = entry.node.clone();
            let studio_key = studio_key_for_opener.clone();
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                tokio::spawn(async move {
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let Ok(req) = serde_json::from_str::<ProtoReq>(&line) else {
                            break;
                        };
                        let reply = if req.method == "hello" {
                            ProtoResp::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: node_name.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: VERSION.to_owned(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".to_owned(),
                                    arch: "x86_64".to_owned(),
                                    systems: Vec::new(),
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap_or_default(),
                            )
                        } else {
                            ProtoResp::ok(
                                req.id,
                                serde_json::json!({
                                    "results": {
                                        "studio": {
                                            "ok": {
                                                "node": "studio",
                                                "hosts": ["studio.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [studio_key],
                                            }
                                        }
                                    }
                                }),
                            )
                        };
                        if let Ok(encoded) = serde_json::to_string(&reply) {
                            let _ = server_out
                                .write_all(format!("{encoded}\n").as_bytes())
                                .await;
                        }
                        line.clear();
                    }
                });
                Session::open_io(
                    client_in,
                    client_out,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let backend = LinkBackend::with_opener(store, None, opener);
        let before_learn = backend.content_target("studio")?;
        assert!(before_learn.holder.starts_with("fleet:"));
        assert!(before_learn.learned_identity.is_none());
        let fleet_epoch =
            parse_fleet_holder(&before_learn.holder).ok_or("expected fleet:<epoch> holder")?;

        // Learning `studio` from paired introducer `tower` records learned_identity in the same
        // fleet epoch without bumping `fleet_pairing_epoch`.
        let learned = backend.learn().await?;
        assert_eq!(learned.report.added, vec!["studio".to_owned()]);
        assert!(backend.is_pairing_holder_live(&before_learn.holder));
        let after_learn = backend.content_target("studio")?;
        assert!(after_learn.holder.starts_with("pairing:studio:"));
        let id = after_learn
            .learned_identity
            .ok_or("learned entry must carry learned_identity")?;
        assert!(id.from_paired_introducer);
        assert_eq!(id.learned_fleet_epoch, fleet_epoch);
        assert_eq!(id.host_keys, canonical_host_keys(&[studio_key]));

        // Simulating a direct `pair()` update bumps `fleet_pairing_epoch` and replaces `studio`
        // with a Paired entry, revoking the old `fleet:` holder and clearing `learned_identity`.
        {
            let mut guard = backend.state.lock().unwrap_or_else(|p| p.into_inner());
            let paired_studio = LinkNode::ssh(
                "studio",
                SshTarget {
                    hosts: vec!["other-studio.example.net".to_owned()],
                    port: 22,
                    user: "me".to_owned(),
                    host_keys: vec![DeviceKey::from_seed([82u8; 32]).public_key_line("studio")?],
                },
                Some("device:pixel".to_owned()),
            );
            guard.data.link.upsert_node(paired_studio);
            guard.set_fresh_epoch("studio");
            guard.bump_fleet_epoch();
            guard.learned_fleet_epochs.remove("studio");
        }
        assert!(
            !backend.is_pairing_holder_live(&before_learn.holder),
            "pairing must invalidate prior fleet holders"
        );
        let after_pair = backend.content_target("studio")?;
        assert!(
            after_pair.learned_identity.is_none(),
            "paired entry must never look like a learned upgrade target"
        );
        Ok(())
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[tokio::test]
    async fn desktop_local_pipe_never_learns_ssh_self_route_or_surfaces_self_rejection()
    -> Result<(), Box<dyn std::error::Error>> {
        use legion_link::proto::{HelloResult, PROTOCOL, Response as ProtoResp, VERSION};
        use legion_link::{Session, SessionOpener};

        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut initial = AppStoreData::default();
        initial.link.upsert_node(sample_ssh_node("tower"));
        store.save(&initial)?;

        let workstation_key = DeviceKey::from_seed([83u8; 32]).public_key_line("workstation")?;
        let studio_key = DeviceKey::from_seed([84u8; 32]).public_key_line("studio")?;
        let opener: SessionOpener = Arc::new(move |entry, _, cancel| {
            let workstation_key = workstation_key.clone();
            let studio_key = studio_key.clone();
            Box::pin(async move {
                let (client_in, mut server_out) = tokio::io::duplex(16 * 1024);
                let (server_in, client_out) = tokio::io::duplex(16 * 1024);
                let is_local = entry.node == LOCAL_NODE_ID;
                let hello_node = if is_local {
                    "workstation".to_owned()
                } else {
                    entry.node.clone()
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
                    let mut reader = BufReader::new(server_in);
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let req: legion_link::proto::Request = match serde_json::from_str(&line) {
                            Ok(r) => r,
                            Err(_) => break,
                        };
                        let reply = if req.method == "hello" {
                            ProtoResp::ok(
                                req.id,
                                serde_json::to_value(HelloResult {
                                    node: hello_node.clone(),
                                    node_explicit: None,
                                    node_id: None,
                                    version: VERSION.to_owned(),
                                    build: None,
                                    protocol: PROTOCOL,
                                    os: "linux".to_owned(),
                                    arch: "x86_64".to_owned(),
                                    systems: vec![],
                                    system: None,
                                    boot_id: None,
                                })
                                .unwrap_or_default(),
                            )
                        } else if req.method == "tools" {
                            ProtoResp::ok(
                                req.id,
                                serde_json::json!({
                                    "tools": [{
                                        "name": "fs.list",
                                        "description": "List files",
                                        "effect": "read",
                                        "input_schema": { "type": "object" },
                                    }]
                                }),
                            )
                        } else if req.params["tool"] == "meta.whoami" {
                            ProtoResp::ok(
                                req.id,
                                serde_json::json!({
                                    "node": "workstation",
                                    "principal": "local",
                                    "role": "owner",
                                }),
                            )
                        } else {
                            // `device.endpoint` reports BOTH `studio` (a valid remote peer) and
                            // `workstation` (this desktop computer itself — once as an SSH target
                            // from `tower` and once as a loopback/unlearnable rejection from `local`).
                            let workstation_entry = if is_local {
                                serde_json::json!({
                                    "ok": {
                                        "node": "workstation",
                                        "hosts": ["127.0.0.1"],
                                        "port": 22,
                                        "user": "me",
                                        "host_keys": [workstation_key],
                                    }
                                })
                            } else {
                                serde_json::json!({
                                    "ok": {
                                        "node": "workstation",
                                        "hosts": ["workstation.example.net"],
                                        "port": 22,
                                        "user": "me",
                                        "host_keys": [workstation_key],
                                    }
                                })
                            };
                            ProtoResp::ok(
                                req.id,
                                serde_json::json!({
                                    "results": {
                                        "workstation": workstation_entry,
                                        "studio": {
                                            "ok": {
                                                "node": "studio",
                                                "hosts": ["studio.example.net"],
                                                "port": 22,
                                                "user": "me",
                                                "host_keys": [studio_key],
                                            }
                                        }
                                    }
                                }),
                            )
                        };
                        if let Ok(encoded) = serde_json::to_string(&reply) {
                            let _ = server_out
                                .write_all(format!("{encoded}\n").as_bytes())
                                .await;
                        }
                        line.clear();
                    }
                });
                Session::open_io(
                    client_in,
                    client_out,
                    &entry.node,
                    None,
                    Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        });

        let backend = LinkBackend::with_opener(store, Some("/usr/bin/legion".to_owned()), opener);

        // Resolving `meta.whoami` over `local` records `attached_local_node = "workstation"`.
        let who = backend
            .call(
                CallRequest {
                    id: None,
                    tool: "meta.whoami".to_owned(),
                    args: serde_json::json!({}),
                    on: Some(serde_json::json!("local")),
                },
                None,
            )
            .await?;
        assert_eq!(who["node"], "workstation");

        // `tools` addressed to `"workstation"` routes over the local pipe directly.
        let tools = backend
            .tools(Some(serde_json::json!("workstation")))
            .await?;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "fs.list");

        // Running `learn()` learns `studio` (the peer) while refusing to persist a learned SSH
        // shadow or surface a learn rejection/disagreement for `workstation` itself.
        let learned = backend.learn().await?;
        assert_eq!(learned.report.added, vec!["studio".to_owned()]);
        assert!(
            !learned
                .report
                .rejected_details
                .iter()
                .any(|r| r.machine == "workstation" || r.machine == "local"),
            "learn report must not include rejections for this computer: {:?}",
            learned.report.rejected_details
        );
        assert!(
            !learned
                .state
                .nodes
                .iter()
                .any(|n| n.node == "workstation" && n.via == "ssh"),
            "desktop local computer must never gain a learned SSH duplicate: {:?}",
            learned.state.nodes
        );
        assert!(
            !learned
                .state
                .learn_rejections
                .iter()
                .any(|r| r.machine == "workstation" || r.machine == "local"),
            "state must not surface learn_rejections for this computer"
        );
        assert!(
            !learned
                .state
                .routes
                .get("workstation")
                .is_some_and(|r| r.kind != "direct"),
            "state must never mark this computer as relayed: {:?}",
            learned.state.routes
        );
        let ct = backend.content_target("workstation")?;
        assert_eq!(ct.link_node, LOCAL_NODE_ID);
        assert_eq!(ct.principal, "local");
        assert!(ct.holder.starts_with("pairing:local:"));
        Ok(())
    }

    #[tokio::test]
    async fn cancel_pair_stops_waiting_pair_without_persisting_node()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let backend = Arc::new(LinkBackend::with_local_program(store, None));

        // Hold mutate_lock so `pair()` blocks deterministically before network I/O, then call
        // `cancel_pair()` and verify `pair()` exits promptly with Cancelled and saves nothing.
        let held = backend.mutate_lock.lock().await;
        let invite = Invite {
            v: 1,
            id: "inv-cancel-1".to_owned(),
            node: "tower".to_owned(),
            hosts: vec!["192.0.2.1".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea"
                    .to_owned(),
            ],
            key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
            principal: "device:phone".to_owned(),
            expires: "2026-11-26T12:00:00Z".to_owned(),
        }
        .encode()?;

        let backend_pair = Arc::clone(&backend);
        let pair_task =
            tokio::spawn(async move { backend_pair.pair(invite, "phone".to_owned()).await });

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let active = backend
                .active_pairs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len();
            if active > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            backend
                .active_pairs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
            1
        );

        backend.cancel_pair().await?;
        let err = pair_task
            .await?
            .err()
            .ok_or("expected pair to return cancelled error")?;
        drop(held);

        assert_eq!(err.code, ErrorCode::Cancelled);
        assert_eq!(
            backend
                .active_pairs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
            0
        );
        let st = backend.state().await?;
        assert!(st.nodes.is_empty());
        assert_eq!(st.needs, Some(Needs::Pairing));
        Ok(())
    }

    #[tokio::test]
    async fn pair_releases_mutate_lock_during_network_wait_allowing_select_and_unpair()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let mut initial = AppStoreData::default();
        initial.link.upsert_node(sample_ssh_node("alpha"));
        initial.link.upsert_node(sample_ssh_node("beta"));
        initial.current_node = Some("alpha".to_owned());
        store.save(&initial)?;
        let backend = Arc::new(LinkBackend::with_local_program(store, None));

        // Bind a loopback listener that accepts TCP connections and holds them open without
        // sending an SSH banner so `pair()` waits in network I/O.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let accepted = Arc::new(tokio::sync::Notify::new());
        let accepted_c = Arc::clone(&accepted);
        let server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                accepted_c.notify_one();
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(stream);
            }
        });

        let invite = Invite {
            v: 1,
            id: "inv-nonblocking-1".to_owned(),
            node: "slowbox".to_owned(),
            hosts: vec!["127.0.0.1".to_owned()],
            port,
            user: "me".to_owned(),
            host_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea"
                    .to_owned(),
            ],
            key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned(),
            principal: "device:phone".to_owned(),
            expires: "2026-11-26T12:00:00Z".to_owned(),
        }
        .encode()?;

        let backend_pair = Arc::clone(&backend);
        let pair_task =
            tokio::spawn(async move { backend_pair.pair(invite, "phone".to_owned()).await });

        // Wait until `pair()` has actually entered the network wait (connected to the listener).
        tokio::time::timeout(Duration::from_secs(3), accepted.notified()).await?;

        // While `pair()` waits on the network, `select_node` and `unpair` MUST proceed promptly
        // without blocking on `mutate_lock`.
        let selected = tokio::time::timeout(
            Duration::from_secs(1),
            backend.select_node("beta".to_owned()),
        )
        .await??;
        assert_eq!(
            selected.nodes.first().map(|n| n.node.as_str()),
            Some("beta")
        );

        let unpaired =
            tokio::time::timeout(Duration::from_secs(1), backend.unpair("alpha".to_owned()))
                .await??;
        assert_eq!(unpaired.node, "alpha");

        backend.cancel_pair().await?;
        let pair_err = tokio::time::timeout(Duration::from_secs(2), pair_task)
            .await??
            .err()
            .ok_or("expected slow pair to be cancelled")?;
        assert_eq!(pair_err.code, ErrorCode::Cancelled);

        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn commit_recovered_pairing_persists_learned_peers_and_keeps_store_aligned()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = AppStore::in_dir(dir.path());
        let backend = LinkBackend::with_local_program(store.clone(), None);

        let target = SshTarget {
            hosts: vec!["tower.example".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea"
                    .to_owned(),
            ],
        };
        let pi_target = SshTarget {
            hosts: vec!["pi.example".to_owned()],
            port: 22,
            user: "me".to_owned(),
            host_keys: vec![
                "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC4h9C6W8Z0V1e2yV7J6kN0pT8rW9xY1zA2bC3dE4fG5"
                    .to_owned(),
            ],
        };
        let enrolled = legion_link::Enrolled {
            node: "tower".to_owned(),
            target: target.clone(),
            principal: "device:phone".to_owned(),
            role: "operator".to_owned(),
            device: "phone".to_owned(),
            fleet: BTreeMap::new(),
            not_synced_to: Vec::new(),
            sync_command: None,
            recovered: true,
        };

        // Simulate `Link::pair_cancellable_with_learn` having updated `Link` with `tower` and
        // learned `pi` via `learn()`.
        {
            let guard = backend.state.lock().unwrap_or_else(|p| p.into_inner());
            guard.link.upsert_node(LinkNode::from(enrolled.clone()));
            guard
                .link
                .upsert_node(LinkNode::learned("pi", pi_target, None, "tower"));
        }
        let report = legion_link::LearnReport {
            introducer: "tower".to_owned(),
            introducers: vec!["tower".to_owned()],
            added: vec!["pi".to_owned()],
            ..legion_link::LearnReport::default()
        };

        let app_state = backend
            .commit_enrolled_pairing(enrolled, Some(report))
            .await?;
        assert!(app_state.ready);
        assert!(app_state.nodes.iter().any(|n| n.node == "tower"));
        assert!(
            app_state
                .nodes
                .iter()
                .any(|n| n.node == "pi" && n.origin == "learned"),
            "recovered pairing must include learned fleet peers: {:?}",
            app_state.nodes
        );

        // Reloading from disk must match memory (`tower` + learned `pi` both saved).
        let on_disk = store.load()?;
        assert!(on_disk.link.node("tower").is_some());
        assert!(on_disk.link.node("pi").is_some());
        Ok(())
    }
}
