//! Real [`Backend`] implementation backed by `legion-link`.
//!
//! Manages paired SSH nodes (on every platform) and an implicit local child-process node (`legion
//! rpc`) on desktops when a `legion` binary is installed. State is persisted atomically in
//! [`AppStore`] without ever logging keys or invites.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use legion_link::proto::Target;
use legion_link::{
    DeviceKey, Invite, Link, LinkConfig, LinkOptions, MachineRouteStatus, NodeLink as LinkNode,
    NodeOrigin, parse_optional_target, public_key_fingerprint,
};
use serde_json::Value;
use tauri::ipc::Channel;
use tokio::sync::Mutex as AsyncMutex;

use crate::backend::{
    AppError, AppState, Backend, BoxFuture, CallId, CallRequest, CancelResult,
    ConflictingIdentityInfo, ContentTarget, DisagreementEntryView, DisagreementView, ErrorCode,
    IdentityChangedInfo, LearnResult, MachineRouteView, Needs, NodeLink, PairPendingPeer,
    PairSummary, RepairInfo, RepairResult, TransferCapabilities, UnpairResult,
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

/// Production backend implementation over [`legion_link::Link`].
pub struct LinkBackend {
    store: AppStore,
    state: Mutex<BackendState>,
    /// Serializes pairing/unpairing/learning/identity-change mutations so device key generation
    /// and store writes never race.
    mutate_lock: AsyncMutex<()>,
    next_call_seq: AtomicU64,
}

struct BackendState {
    data: AppStoreData,
    store_error: Option<AppError>,
    local_program: Option<String>,
    /// Observed real node name (`hello.node` / `meta.whoami.node`) of the implicit desktop `local`
    /// pipe so content and transfer requests naming that machine resolve cleanly to `local`.
    local_node_name: Option<String>,
    active_node: Option<String>,
    /// Expiring route preference for door-level discovery reads when the user's preferred node is
    /// offline. Never changes `active_node` and never acts as a target for machine calls.
    preferred_relay: Option<(String, Instant)>,
    last_paired: Option<PairSummary>,
    disagreements: Vec<DisagreementView>,
    /// Monotonic pairing epoch per configured link node (`"local"` or node name). Bumped on pair
    /// / identity-change acceptance and removed on unpair / repair so content tickets issued under
    /// a removed or replaced pairing die immediately.
    pairing_epochs: HashMap<String, u64>,
    next_pairing_epoch: u64,
    link: Link,
}

impl BackendState {
    fn alloc_epoch(&mut self) -> u64 {
        let next = self.next_pairing_epoch;
        self.next_pairing_epoch = self.next_pairing_epoch.saturating_add(1);
        next
    }

    fn set_fresh_epoch(&mut self, node: &str) -> u64 {
        let epoch = self.alloc_epoch();
        self.pairing_epochs.insert(node.to_owned(), epoch);
        epoch
    }

    fn sync_pairing_epochs(&mut self) {
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

fn parse_pairing_holder(holder: &str) -> Option<(&str, u64)> {
    let rest = holder.strip_prefix("pairing:")?;
    let (node, epoch_str) = rest.rsplit_once(':')?;
    if node.is_empty() {
        return None;
    }
    let epoch = epoch_str.parse::<u64>().ok()?;
    Some((node, epoch))
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
        Self::init(store, local_program, None)
    }

    /// Creates a backend with an explicit local binary override (used in unit tests).
    #[cfg(test)]
    pub fn with_local_program(store: AppStore, local_program: Option<String>) -> Self {
        Self::init(store, local_program, None)
    }

    /// Creates a backend with a custom [`legion_link::SessionOpener`] for deterministic unit tests.
    #[cfg(test)]
    pub fn with_opener(
        store: AppStore,
        local_program: Option<String>,
        opener: legion_link::SessionOpener,
    ) -> Self {
        Self::init(store, local_program, Some(opener))
    }

    fn init(
        store: AppStore,
        local_program: Option<String>,
        opener: Option<legion_link::SessionOpener>,
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
            local_node_name: None,
            active_node,
            preferred_relay: None,
            last_paired: None,
            disagreements: Vec::new(),
            pairing_epochs: HashMap::new(),
            next_pairing_epoch: 1,
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
        }
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
            if guard.data.link.node(machine).is_none()
                && guard.local_program.is_some()
                && guard.local_node_name.as_deref() == Some(machine)
            {
                return LOCAL_NODE_ID.to_owned();
            }
        }
        machine.to_owned()
    }
}

impl Backend for LinkBackend {
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move { Ok(self.snapshot_state()) })
    }

    fn tools(&self, on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
        Box::pin(async move {
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
            let routed = link.tools_machine(&machine).await.map_err(AppError::from)?;
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
                        && request.tool == "meta.whoami"
                        && let Some(actual_name) = routed
                            .value
                            .get("node")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                    {
                        let mut guard = self
                            .state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        guard.local_node_name = Some(actual_name.to_owned());
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

    fn pair(&self, invite: String, name: String) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move {
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

            let _parsed = Invite::parse(trimmed_invite).map_err(AppError::from)?;
            let device_label = {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    default_device_name()
                } else {
                    trimmed.to_owned()
                }
            };

            let _mutation = self.mutate_lock.lock().await;

            // Ensure this device has a persisted Ed25519 key BEFORE enrolling so a failed store
            // save can never leave a node holding a public key whose private half was lost.
            let link = {
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

            // Enrol in place via `Link::pair`, reusing the persisted device key and keeping any
            // other live sessions and in-flight calls undisturbed.
            let enrolled = link
                .pair(trimmed_invite, &device_label)
                .await
                .map_err(AppError::from)?;

            let pair_summary = build_pair_summary(&enrolled);
            let node_entry = LinkNode::from(enrolled.clone());
            let node_name = enrolled.node.clone();
            let meta = StoredNodeMeta {
                node: enrolled.node.clone(),
                role: Some(enrolled.role.clone()),
                device: Some(enrolled.device.clone()),
                role_checked_at: Some(now_rfc3339_utc()),
            };

            // Compute, save, and swap under the single state lock while still holding mutate_lock.
            let app_state = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut updated_data = guard.data.clone();
                updated_data.link.upsert_node(node_entry);
                updated_data.upsert_meta(meta);
                updated_data.current_node = Some(node_name.clone());
                self.store.save(&updated_data)?;
                guard.data = updated_data;
                guard.active_node = Some(node_name.clone());
                guard.preferred_relay = None;
                guard.last_paired = Some(pair_summary);
                guard.disagreements.retain(|d| d.machine != node_name);
                guard.set_fresh_epoch(&node_name);
                build_app_state(&guard, self.store.path())
            };

            Ok(app_state)
        })
    }

    fn unpair(&self, node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
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
                guard.disagreements.retain_mut(|d| {
                    if d.machine == trimmed {
                        return false;
                    }
                    d.reports.retain(|r| r.reported_by != trimmed);
                    d.reports.len() >= 2
                });
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
                guard.pairing_epochs.clear();
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

            let report = link.learn().await.map_err(AppError::from)?;

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
                guard.disagreements = report
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
                    .collect();
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
                build_app_state(&guard, self.store.path())
            };
            Ok(state)
        })
    }

    fn dismiss_identity_change(&self, node: String) -> BoxFuture<'_, Result<AppState, AppError>> {
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
        if let Some(entry) = guard.data.link.node(trimmed).cloned() {
            let epoch = match guard.pairing_epochs.get(&entry.node).copied() {
                Some(ep) => ep,
                None => guard.set_fresh_epoch(&entry.node),
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
            });
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if guard.local_program.is_some()
            && (trimmed == LOCAL_NODE_ID || guard.local_node_name.as_deref() == Some(trimmed))
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
            });
        }
        let any_configured = !guard.data.link.nodes.is_empty() || guard.local_program.is_some();
        if !any_configured {
            return Err(AppError::not_paired());
        }
        Err(AppError::new(
            ErrorCode::NotFound,
            format!("Machine `{trimmed}` is not paired on this device."),
        )
        .with_hint("Pair with that machine or refresh learned fleet routes first."))
    }

    fn is_pairing_holder_live(&self, holder: &str) -> bool {
        let Some((node, epoch)) = parse_pairing_holder(holder) else {
            return false;
        };
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.store_error.is_some() {
            return false;
        }
        guard.pairing_epochs.get(node).copied() == Some(epoch)
    }
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
    AppState {
        ready,
        nodes,
        needs,
        repair: None,
        can_scan,
        routes,
        disagreements: state.disagreements.clone(),
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
    use std::path::Path;

    #[cfg(windows)]
    const BIN_NAME: &str = "legion.exe";
    #[cfg(not(windows))]
    const BIN_NAME: &str = "legion";

    fn is_executable_file(path: &Path) -> bool {
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    fn resolve_candidate(candidate: &Path) -> Option<String> {
        let canonical = std::fs::canonicalize(candidate).ok()?;
        if !is_executable_file(&canonical) {
            return None;
        }
        let raw = canonical.to_string_lossy();
        #[cfg(windows)]
        if let Some(stripped) = raw.strip_prefix(r"\\?\")
            && stripped.as_bytes().get(1) == Some(&b':')
        {
            return Some(stripped.to_owned());
        }
        Some(raw.into_owned())
    }

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

        let dir = tempfile::tempdir()?;
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
}
