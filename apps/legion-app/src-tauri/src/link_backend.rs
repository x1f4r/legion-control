//! Real [`Backend`] implementation backed by `legion-link`.
//!
//! Manages paired SSH nodes (on every platform) and an implicit local child-process node (`legion
//! rpc`) on desktops when a `legion` binary is installed. State is persisted atomically in
//! [`AppStore`] without ever logging keys or invites.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use legion_link::{DeviceKey, Invite, Link, LinkConfig, NodeLink as LinkNode};
use serde_json::Value;
use tauri::ipc::Channel;
use tokio::sync::Mutex as AsyncMutex;

use crate::backend::{
    AppError, AppState, Backend, BoxFuture, CallId, CallRequest, CancelResult, ErrorCode, Needs,
    NodeLink, RepairInfo, RepairResult, UnpairResult,
};
use crate::commands::default_device_name;
use crate::store::{AppStore, AppStoreData, StoredNodeMeta};

/// Internal node key used for the implicit desktop local pipe (`legion rpc`).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const LOCAL_NODE_ID: &str = "local";

/// Production backend implementation over [`legion_link::Link`].
pub struct LinkBackend {
    store: AppStore,
    state: Mutex<BackendState>,
    /// Serializes pairing/unpairing mutations so device key generation and store writes never race.
    mutate_lock: AsyncMutex<()>,
    next_call_seq: AtomicU64,
}

struct BackendState {
    data: AppStoreData,
    store_error: Option<AppError>,
    local_program: Option<String>,
    active_node: Option<String>,
    link: Link,
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
        let link = match opener {
            Some(custom) => {
                Link::with_opener(link_cfg, legion_link::LinkOptions::default(), custom)
            }
            None => Link::new(link_cfg),
        };

        Self {
            store,
            state: Mutex::new(BackendState {
                data,
                store_error,
                local_program,
                active_node,
                link,
            }),
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

    fn candidate_nodes(&self) -> Result<(Link, Vec<String>), AppError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(err) = &guard.store_error {
            return Err(err.clone());
        }
        let link = guard.link.clone();
        let app_state = build_app_state(&guard, self.store.path());
        let names = app_state.nodes.into_iter().map(|n| n.node).collect();
        Ok((link, names))
    }

    fn mark_active(&self, node: &str) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.store_error.is_some() || guard.active_node.as_deref() == Some(node) {
            return;
        }
        guard.active_node = Some(node.to_owned());
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
}

impl Backend for LinkBackend {
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move { Ok(self.snapshot_state()) })
    }

    fn tools(&self, on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
        Box::pin(async move {
            let (link, candidates) = self.candidate_nodes()?;
            if candidates.is_empty() {
                return Err(AppError::not_paired());
            }

            let mut last_err: Option<AppError> = None;
            for (idx, node) in candidates.iter().enumerate() {
                match link.tools(node, on.clone()).await {
                    Ok(value) => {
                        if idx > 0 {
                            self.mark_active(node);
                        }
                        let tools = value
                            .get("tools")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        return Ok(tools);
                    }
                    Err(body) => {
                        let app_err = AppError::from(body);
                        let retry_next = is_failover_error(&app_err);
                        last_err = Some(app_err);
                        if !retry_next {
                            break;
                        }
                    }
                }
            }

            Err(last_err.unwrap_or_else(AppError::not_paired))
        })
    }

    fn call(
        &self,
        request: CallRequest,
        on_event: Option<Channel<Value>>,
    ) -> BoxFuture<'_, Result<Value, AppError>> {
        Box::pin(async move {
            let (link, candidates) = self.candidate_nodes()?;
            if candidates.is_empty() {
                return Err(AppError::not_paired());
            }

            let call_id = self.next_call_id(request.id);
            let mut last_err: Option<AppError> = None;

            for (idx, node) in candidates.iter().enumerate() {
                let channel = on_event.clone();
                let outcome = link
                    .call_with_id(
                        &call_id,
                        node,
                        &request.tool,
                        request.args.clone(),
                        request.on.clone(),
                        move |event| {
                            if let Some(ch) = &channel {
                                let _ = ch.send(event);
                            }
                        },
                    )
                    .await;

                match outcome {
                    Ok(value) => {
                        if idx > 0 {
                            self.mark_active(node);
                        }
                        return Ok(value);
                    }
                    Err(body) => {
                        let app_err = AppError::from(body);
                        let retry_next = is_failover_error(&app_err);
                        last_err = Some(app_err);
                        if !retry_next {
                            break;
                        }
                    }
                }
            }

            Err(last_err.unwrap_or_else(AppError::not_paired))
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

            let node_entry = LinkNode::from(enrolled.clone());
            let node_name = enrolled.node.clone();
            let meta = StoredNodeMeta {
                node: enrolled.node.clone(),
                role: Some(enrolled.role.clone()),
                device: Some(enrolled.device.clone()),
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
                guard.active_node = Some(node_name);
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
                next.link.remove_node(trimmed);
                if next.current_node.as_deref() == Some(trimmed) {
                    next.current_node = next.link.nodes.first().map(|n| n.node.clone());
                }
                self.store.save(&next)?;
                let local_program = guard.local_program.clone();
                guard.data = next;
                guard.active_node = resolve_active_node(&guard.data, local_program.as_deref());
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
                let old = std::mem::replace(&mut guard.link, Link::new(runtime_cfg));
                guard.data = fresh;
                guard.store_error = None;
                guard.active_node = active;
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
}

fn is_failover_error(err: &AppError) -> bool {
    matches!(err.code, ErrorCode::Unreachable | ErrorCode::Timeout)
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
    if let Some(err) = &state.store_error {
        return AppState {
            ready: false,
            nodes: Vec::new(),
            needs: Some(Needs::Repair),
            repair: Some(RepairInfo {
                message: err.message.clone(),
                path: store_path.display().to_string(),
            }),
        };
    }

    let mut nodes: Vec<NodeLink> = Vec::new();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if state.local_program.is_some() && state.data.link.node(LOCAL_NODE_ID).is_none() {
        nodes.push(NodeLink {
            node: LOCAL_NODE_ID.to_owned(),
            principal: "local".to_owned(),
            role: "owner".to_owned(),
            via: "local".to_owned(),
        });
    }

    for entry in &state.data.link.nodes {
        let meta = state.data.meta(&entry.node);
        let via = if entry.local_program.is_some() {
            "local"
        } else {
            "ssh"
        };
        nodes.push(NodeLink {
            node: entry.node.clone(),
            principal: entry
                .principal
                .clone()
                .unwrap_or_else(|| "local".to_owned()),
            role: meta
                .and_then(|m| m.role.clone())
                .unwrap_or_else(|| "unknown".to_owned()),
            via: via.to_owned(),
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
    }
}

/// Discovers an installed `legion` binary on desktop hosts (PATH first, then conventional paths).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn discover_local_legion() -> Option<String> {
    use std::path::{Path, PathBuf};

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

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(BIN_NAME);
            if is_executable_file(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    let mut conventional: Vec<PathBuf> = Vec::new();
    #[cfg(not(windows))]
    {
        if let Some(home) = std::env::var_os("HOME") {
            conventional.push(PathBuf::from(home).join(".local/bin/legion"));
        }
        conventional.push(PathBuf::from("/usr/bin/legion"));
        conventional.push(PathBuf::from("/usr/local/bin/legion"));
        conventional.push(PathBuf::from("/opt/homebrew/bin/legion"));
    }
    #[cfg(windows)]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            conventional.push(PathBuf::from(local_app_data).join("legion").join(BIN_NAME));
        }
    }

    for candidate in conventional {
        if is_executable_file(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
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
        });
        data.upsert_meta(StoredNodeMeta {
            node: "pi".to_owned(),
            role: Some("observer".to_owned()),
            device: Some("pixel-phone".to_owned()),
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
                        on: None,
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
}
