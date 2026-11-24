//! Real [`Backend`] implementation backed by `legion-link`.
//!
//! Manages paired SSH nodes (on every platform) and an implicit local child-process node (`legion
//! rpc`) on desktops when a `legion` binary is installed. State is persisted atomically in
//! [`AppStore`] without ever logging keys or invites.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use legion_link::{
    CancellationToken, DeviceKey, Invite, Link, LinkConfig, NodeLink as LinkNode, SshTimeouts,
    enrol,
};
use serde_json::Value;
use tauri::ipc::Channel;
use tokio::sync::Mutex as AsyncMutex;

use crate::backend::{
    AppError, AppState, Backend, BoxFuture, CallId, CallRequest, CancelResult, ErrorCode, Needs,
    NodeLink, UnpairResult,
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
    local_program: Option<String>,
    active_node: Option<String>,
    link: Link,
}

impl LinkBackend {
    /// Loads persisted state from `store`, discovers any local desktop `legion` binary, and
    /// prepares the live [`Link`] handle.
    pub fn from_store(store: AppStore) -> Self {
        let data = store.load().unwrap_or_default();
        let local_program = discover_local_legion();
        let link_cfg = build_runtime_config(&data.link, local_program.as_deref());
        let active_node = resolve_active_node(&data, local_program.as_deref());
        let link = Link::new(link_cfg);

        Self {
            store,
            state: Mutex::new(BackendState {
                data,
                local_program,
                active_node,
                link,
            }),
            mutate_lock: AsyncMutex::new(()),
            next_call_seq: AtomicU64::new(1),
        }
    }

    /// Creates a backend with an explicit local binary override (used in unit tests).
    #[cfg(test)]
    pub fn with_local_program(store: AppStore, local_program: Option<String>) -> Self {
        let data = store.load().unwrap_or_default();
        let link_cfg = build_runtime_config(&data.link, local_program.as_deref());
        let active_node = resolve_active_node(&data, local_program.as_deref());
        let link = Link::new(link_cfg);

        Self {
            store,
            state: Mutex::new(BackendState {
                data,
                local_program,
                active_node,
                link,
            }),
            mutate_lock: AsyncMutex::new(()),
            next_call_seq: AtomicU64::new(1),
        }
    }

    fn snapshot_state(&self) -> AppState {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        build_app_state(&guard)
    }

    fn candidate_nodes(&self) -> (Link, Vec<String>) {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let link = guard.link.clone();
        let app_state = build_app_state(&guard);
        let names = app_state.nodes.into_iter().map(|n| n.node).collect();
        (link, names)
    }

    fn mark_active(&self, node: &str) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.active_node.as_deref() == Some(node) {
            return;
        }
        guard.active_node = Some(node.to_owned());
        if guard.data.link.node(node).is_some() {
            guard.data.current_node = Some(node.to_owned());
            let _ = self.store.save(&guard.data);
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
}

impl Backend for LinkBackend {
    fn state(&self) -> BoxFuture<'_, Result<AppState, AppError>> {
        Box::pin(async move { Ok(self.snapshot_state()) })
    }

    fn tools(&self, on: Option<Value>) -> BoxFuture<'_, Result<Vec<Value>, AppError>> {
        Box::pin(async move {
            let (link, candidates) = self.candidate_nodes();
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
            let (link, candidates) = self.candidate_nodes();
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
            let device_label = {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    default_device_name()
                } else {
                    trimmed.to_owned()
                }
            };

            let _mutation = self.mutate_lock.lock().await;

            // Reuse the existing device key whenever present so already-paired nodes keep working;
            // only generate a new key on first pairing.
            let (existing_pem, had_key) = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let pem = guard.data.link.device_key.clone();
                let has = pem.is_some();
                (pem, has)
            };

            let device_key = match existing_pem {
                Some(pem) => DeviceKey::from_openssh(&pem).map_err(AppError::from)?,
                None => DeviceKey::generate().map_err(AppError::from)?,
            };
            let key_pem = device_key.to_openssh().map_err(AppError::from)?;

            let enrolled = enrol(
                &parsed,
                &device_key,
                &device_label,
                SshTimeouts::default(),
                CancellationToken::new(),
            )
            .await
            .map_err(AppError::from)?;

            let node_entry = LinkNode::from(enrolled.clone());
            let node_name = enrolled.node.clone();
            let meta = StoredNodeMeta {
                node: enrolled.node.clone(),
                role: Some(enrolled.role.clone()),
                device: Some(enrolled.device.clone()),
            };

            // Prepare updated store data and persist atomically before exposing it to callers.
            let (updated_data, local_program, old_link) = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let mut next = guard.data.clone();
                if next.link.device_key.is_none() {
                    next.link.device_key = Some(key_pem);
                }
                next.link.upsert_node(node_entry.clone());
                next.upsert_meta(meta);
                next.current_node = Some(node_name.clone());
                (next, guard.local_program.clone(), guard.link.clone())
            };

            self.store.save(&updated_data)?;

            // `legion_link::Link` has no `set_device_key` setter, so rebuild the Link handle when
            // the device key is newly created; otherwise evict any stale session for the re-paired
            // node and upsert the updated node entry in place.
            let next_link = if !had_key {
                let runtime_cfg =
                    build_runtime_config(&updated_data.link, local_program.as_deref());
                Link::new(runtime_cfg)
            } else {
                old_link.remove_node(&node_name).await;
                old_link.upsert_node(node_entry);
                old_link.clone()
            };

            let app_state = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.data = updated_data;
                guard.active_node = Some(node_name);
                guard.link = next_link;
                build_app_state(&guard)
            };

            if !had_key {
                old_link.close().await;
            }

            Ok(app_state)
        })
    }

    fn unpair(&self, node: String) -> BoxFuture<'_, Result<UnpairResult, AppError>> {
        Box::pin(async move {
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

            let (updated_data, device_name, link) = {
                let guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
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
                (next, device, guard.link.clone())
            };

            self.store.save(&updated_data)?;
            link.remove_node(trimmed).await;

            {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let local_program = guard.local_program.clone();
                guard.data = updated_data;
                guard.active_node = resolve_active_node(&guard.data, local_program.as_deref());
            }

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
            let trimmed = node.trim();
            if trimmed.is_empty() {
                return Err(AppError::new(
                    ErrorCode::InvalidArgs,
                    "Node name cannot be empty.",
                ));
            }

            let _mutation = self.mutate_lock.lock().await;
            let (updated_state, data_to_save) = {
                let mut guard = self
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let exists_ssh = guard.data.link.node(trimmed).is_some();
                let exists_local = is_local_node_available(guard.local_program.as_deref(), trimmed);
                if !exists_ssh && !exists_local {
                    return Err(AppError::new(
                        ErrorCode::NotFound,
                        format!("No configured node named `{trimmed}`."),
                    ));
                }
                guard.active_node = Some(trimmed.to_owned());
                let save = if exists_ssh {
                    guard.data.current_node = Some(trimmed.to_owned());
                    Some(guard.data.clone())
                } else {
                    guard.data.current_node = None;
                    Some(guard.data.clone())
                };
                (build_app_state(&guard), save)
            };

            if let Some(data) = data_to_save {
                self.store.save(&data)?;
            }

            Ok(updated_state)
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

fn build_app_state(state: &BackendState) -> AppState {
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
}
