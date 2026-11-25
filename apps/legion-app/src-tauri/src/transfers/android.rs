//! Android native transfer bridge over a private Unix domain socket (`LocalSocket`) and Tauri
//! `FilesTransferPlugin`.
//!
//! Why a private Unix domain socket in addition to `PluginHandle`:
//! When the user swipes Legion away from Recents, `MainActivity.onDestroy()` detaches `wry`'s
//! activity context so `PluginHandle::run_mobile_plugin_async` can no longer reach Kotlin. By
//! contrast, `TransferBridge` in Kotlin holds `applicationContext` and stays connected to our
//! owner-only (`0o600`, 256-bit random session token) Unix domain socket inside
//! `<app_data_dir>/transfers/bridge.sock` so the `dataSync` foreground service can keep updating
//! notifications, writing verified downloads to `MediaStore.Downloads` (`Downloads/Legion`),
//! streaming `content://` uploads in bounded 64 KiB chunks, and delivering notification
//! Pause/Resume/Cancel clicks and `onTimeout` limits even after the Activity is gone.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};

use crate::backend::{AppError, BoxFuture, ErrorCode};
use crate::transfers::{
    DeviceTransfer, PauseReason, PickedUploadFile, QueueNotificationSummary, TransferControlAction,
    TransferManager, TransferPlatform, UploadByteSource,
};

/// Managed Tauri plugin handle for `com.x1f4r.legion.FilesTransferPlugin` on Android.
pub struct AndroidTransferPluginHandle(pub tauri::plugin::PluginHandle<tauri::Wry>);

struct BridgeClient {
    writer: AsyncMutex<tokio::net::unix::OwnedWriteHalf>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, AppError>>>>,
    next_id: AtomicU64,
}

impl BridgeClient {
    async fn call(&self, method: &str, params: Value) -> Result<Value, AppError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id, tx);
        }
        let mut line = serde_json::to_vec(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not encode Android bridge call: {err}."),
            )
        })?;
        line.push(b'\n');

        let write_res = {
            let mut writer = self.writer.lock().await;
            let r = writer.write_all(&line).await;
            if r.is_ok() { writer.flush().await } else { r }
        };
        if let Err(err) = write_res {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(AppError::new(
                ErrorCode::Unreachable,
                format!("Android transfer bridge write failed: {err}."),
            ));
        }

        tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .map_err(|_| {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                AppError::new(
                    ErrorCode::Timeout,
                    format!("Android transfer bridge timed out on `{method}`."),
                )
            })?
            .map_err(|_| {
                AppError::new(
                    ErrorCode::Unreachable,
                    "Android transfer bridge closed before replying.",
                )
            })?
    }

    fn resolve(&self, id: u64, res: Result<Value, AppError>) {
        if let Some(tx) = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id)
        {
            let _ = tx.send(res);
        }
    }

    fn fail_all(&self) {
        let mut map = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_, tx) in map.drain() {
            let _ = tx.send(Err(AppError::new(
                ErrorCode::Unreachable,
                "Android transfer bridge disconnected.",
            )));
        }
    }
}

/// Android implementation of [`TransferPlatform`].
pub struct AndroidTransferPlatform {
    app_handle: AppHandle,
    socket_path: PathBuf,
    auth_token: String,
    plugin: Mutex<Option<Arc<AndroidTransferPluginHandle>>>,
    manager: Mutex<Weak<TransferManager>>,
    client: Mutex<Option<Arc<BridgeClient>>>,
    client_generation: AtomicU64,
    unmetered_wifi: AtomicBool,
    notification_permission_asked: AtomicBool,
    latest_summary: Mutex<Option<(u64, QueueNotificationSummary)>>,
    summary_seq: AtomicU64,
    sync_notify: Notify,
}

impl AndroidTransferPlatform {
    /// Creates the Android platform bridge and binds `<data_dir>/transfers/bridge.sock` (`0o600`).
    pub fn new(app_handle: AppHandle, data_dir: &Path) -> Result<Arc<Self>, AppError> {
        let dir = data_dir.join("transfers");
        fs::create_dir_all(&dir).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not create Android transfers directory: {err}."),
            )
        })?;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        let socket_path = dir.join("bridge.sock");
        let _ = fs::remove_file(&socket_path);

        let mut token_bytes = [0u8; 32];
        getrandom::fill(&mut token_bytes).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not generate Android bridge token: {err}."),
            )
        })?;
        let auth_token = token_bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        let platform = Arc::new(Self {
            app_handle,
            socket_path,
            auth_token,
            plugin: Mutex::new(None),
            manager: Mutex::new(Weak::new()),
            client: Mutex::new(None),
            client_generation: AtomicU64::new(0),
            unmetered_wifi: AtomicBool::new(true),
            notification_permission_asked: AtomicBool::new(false),
            latest_summary: Mutex::new(None),
            summary_seq: AtomicU64::new(0),
            sync_notify: Notify::new(),
        });

        let std_listener =
            std::os::unix::net::UnixListener::bind(&platform.socket_path).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not bind Android transfer bridge socket: {err}."),
                )
            })?;
        let _ = fs::set_permissions(&platform.socket_path, fs::Permissions::from_mode(0o600));
        std_listener.set_nonblocking(true).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not configure Android transfer bridge socket: {err}."),
            )
        })?;

        let accept_platform = Arc::clone(&platform);
        crate::transfers::spawn_bg(async move {
            match UnixListener::from_std(std_listener) {
                Ok(listener) => accept_platform.accept_loop(listener).await,
                Err(err) => {
                    #[cfg(debug_assertions)]
                    log::error!(
                        target: "legion-transfers",
                        "Could not register Android transfer bridge socket with runtime: {err}"
                    );
                    #[cfg(not(debug_assertions))]
                    {
                        let _ = err;
                        log::error!(
                            target: "legion-transfers",
                            "Could not register Android transfer bridge socket with runtime"
                        );
                    }
                }
            }
        });

        let sync_platform = Arc::clone(&platform);
        crate::transfers::spawn_bg(async move {
            sync_platform.sync_loop().await;
        });

        Ok(platform)
    }

    /// Attaches the live [`TransferManager`] so push events from Kotlin notifications and Android
    /// network callbacks route back to the queue.
    pub fn attach_manager(&self, manager: &Arc<TransferManager>) {
        *self
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::downgrade(manager);
    }

    /// Attaches the foreground `FilesTransferPlugin` handle and connects Kotlin's `TransferBridge`
    /// singleton to our private Unix domain socket.
    pub fn attach_plugin(self: &Arc<Self>, plugin: Arc<AndroidTransferPluginHandle>) {
        *self
            .plugin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&plugin));
        let socket_path = self.socket_path.to_string_lossy().into_owned();
        let token = self.auth_token.clone();
        crate::transfers::spawn_bg(async move {
            let res: Result<Value, _> = plugin
                .0
                .run_mobile_plugin_async(
                    "connectBridge",
                    json!({
                        "socketPath": socket_path,
                        "token": token,
                    }),
                )
                .await;
            if let Err(err) = res {
                #[cfg(debug_assertions)]
                log::warn!(target: "legion-transfers", "connectBridge failed: {err}");
                #[cfg(not(debug_assertions))]
                {
                    let _ = err;
                    log::warn!(target: "legion-transfers", "connectBridge failed");
                }
            }
        });
    }

    async fn accept_loop(self: Arc<Self>, listener: UnixListener) {
        loop {
            let Ok((stream, _addr)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            };
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                this.serve_connection(stream).await;
            });
        }
    }

    async fn serve_connection(self: Arc<Self>, stream: UnixStream) {
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut first_line = String::new();
        let Ok(n) = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut first_line))
            .await
            .unwrap_or(Ok(0))
        else {
            return;
        };
        if n == 0 {
            return;
        }
        let Ok(hello) = serde_json::from_str::<Value>(&first_line) else {
            return;
        };
        if hello.get("type").and_then(Value::as_str) != Some("hello")
            || hello.get("token").and_then(Value::as_str) != Some(self.auth_token.as_str())
        {
            log::warn!(target: "legion-transfers", "rejected unauthenticated bridge connection");
            return;
        }
        if let Some(wifi) = hello.get("unmeteredWifi").and_then(Value::as_bool) {
            self.unmetered_wifi.store(wifi, Ordering::Release);
        }

        let client = Arc::new(BridgeClient {
            writer: AsyncMutex::new(write_half),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        });
        {
            let mut slot = self
                .client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(prev) = slot.replace(Arc::clone(&client)) {
                prev.fail_all();
            }
            self.client_generation.fetch_add(1, Ordering::AcqRel);
        }
        log::info!(target: "legion-transfers", "Android LocalSocket transfer bridge connected");

        // Wake the ordered sync worker so any active or paused transfers flush immediately.
        self.sync_notify.notify_one();

        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                        if let Some(err_msg) = msg.get("error").and_then(Value::as_str) {
                            let err = match msg.get("errorCode").and_then(Value::as_str) {
                                Some("no_handler") => {
                                    AppError::new(ErrorCode::Unsupported, err_msg.to_owned())
                                        .with_hint(
                                            "Download the file to save it in Downloads instead.",
                                        )
                                        .with_details(json!({ "reason": "no_handler" }))
                                }
                                Some("denied") => {
                                    AppError::new(ErrorCode::Denied, err_msg.to_owned())
                                }
                                Some("not_found") => {
                                    AppError::new(ErrorCode::NotFound, err_msg.to_owned())
                                }
                                _ => AppError::new(ErrorCode::Failed, err_msg.to_owned()),
                            };
                            client.resolve(id, Err(err));
                        } else {
                            let result = msg.get("result").cloned().unwrap_or(Value::Null);
                            client.resolve(id, Ok(result));
                        }
                    } else if let Some(event) = msg.get("event").and_then(Value::as_str) {
                        self.handle_kotlin_event(event, &msg);
                    }
                }
            }
        }

        client.fail_all();
        let mut slot = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, &client)) {
            *slot = None;
        }
    }

    fn handle_kotlin_event(&self, event: &str, msg: &Value) {
        let manager = self
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .upgrade();
        match event {
            "notification_action" => {
                let action_str = msg.get("action").and_then(Value::as_str).unwrap_or("");
                let action = match action_str {
                    "pause" => TransferControlAction::Pause,
                    "resume" => TransferControlAction::Resume,
                    "cancel" => TransferControlAction::Cancel,
                    _ => return,
                };
                let transfer_id = msg
                    .get("transferId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                if let Some(mgr) = manager {
                    tokio::spawn(async move {
                        mgr.handle_notification_action(action, transfer_id.as_deref())
                            .await;
                    });
                }
            }
            "fgs_timeout" => {
                log::warn!(
                    target: "legion-transfers",
                    "Android dataSync foreground service reached system timeout; pausing active transfers"
                );
                if let Some(mgr) = manager {
                    mgr.pause_all_active(PauseReason::FgsTimeout);
                }
            }
            "network_changed" => {
                if let Some(wifi) = msg.get("unmeteredWifi").and_then(Value::as_bool) {
                    self.unmetered_wifi.store(wifi, Ordering::Release);
                    if let Some(mgr) = manager {
                        mgr.on_network_changed(wifi);
                    }
                }
            }
            "activity_created" if self.app_handle.get_webview_window("main").is_none() => {
                let app = self.app_handle.clone();
                tokio::task::spawn_blocking(move || {
                    if app.get_webview_window("main").is_none() {
                        log::info!(
                            target: "legion-transfers",
                            "recreating main WebView window for resumed Android Activity"
                        );
                        if let Err(err) = crate::create_main_webview_window(&app) {
                            #[cfg(debug_assertions)]
                            log::warn!(
                                target: "legion-transfers",
                                "failed to recreate main WebView window: {err}"
                            );
                            #[cfg(not(debug_assertions))]
                            {
                                let _ = err;
                                log::warn!(
                                    target: "legion-transfers",
                                    "failed to recreate main WebView window"
                                );
                            }
                        }
                    }
                });
            }
            _ => {}
        }
    }

    async fn sync_loop(self: Arc<Self>) {
        let mut last_sent: Option<(u64, u64)> = None;
        loop {
            let client = self
                .client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let client_gen = self.client_generation.load(Ordering::Acquire);
            let latest = self
                .latest_summary
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();

            if let (Some(client), Some((seq, summary))) = (client, latest)
                && last_sent != Some((client_gen, seq))
            {
                let fresh_connection = last_sent.map(|(g, _)| g) != Some(client_gen);
                if fresh_connection
                    && summary.active_count == 0
                    && summary.paused_count == 0
                    && summary.cancelling_count == 0
                {
                    last_sent = Some((client_gen, seq));
                    continue;
                }
                match client
                    .call(
                        "syncService",
                        serde_json::to_value(&summary).unwrap_or(Value::Null),
                    )
                    .await
                {
                    Ok(_) => {
                        last_sent = Some((client_gen, seq));
                        continue;
                    }
                    Err(err) => {
                        #[cfg(debug_assertions)]
                        log::warn!(
                            target: "legion-transfers",
                            "Android syncService failed: {}",
                            err.message
                        );
                        #[cfg(not(debug_assertions))]
                        log::warn!(
                            target: "legion-transfers",
                            "Android syncService failed ({:?})",
                            err.code
                        );
                        tokio::select! {
                            () = self.sync_notify.notified() => {}
                            () = tokio::time::sleep(Duration::from_millis(250)) => {}
                        }
                        continue;
                    }
                }
            }
            self.sync_notify.notified().await;
        }
    }

    async fn wait_for_client(&self) -> Result<Arc<BridgeClient>, AppError> {
        for _ in 0..50 {
            if let Some(c) = self
                .client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                return Ok(c);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(AppError::new(
            ErrorCode::Unreachable,
            "Android transfer background bridge is not connected.",
        )
        .with_hint("Reopen Legion and retry the transfer."))
    }

    fn plugin_handle(&self) -> Result<Arc<AndroidTransferPluginHandle>, AppError> {
        self.plugin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::Internal,
                    "Android transfer plugin is not initialized.",
                )
            })
    }
}

struct AndroidUriUploadSource {
    client: Arc<BridgeClient>,
    handle: u64,
    size: Option<u64>,
    closed: bool,
}

impl Drop for AndroidUriUploadSource {
    fn drop(&mut self) {
        if !self.closed {
            let client = Arc::clone(&self.client);
            let handle = self.handle;
            tokio::spawn(async move {
                let _ = client
                    .call("closeUriRead", json!({ "handle": handle }))
                    .await;
            });
        }
    }
}

impl UploadByteSource for AndroidUriUploadSource {
    fn known_size(&self) -> Option<u64> {
        self.size
    }

    fn read_chunk<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, Result<usize, AppError>> {
        Box::pin(async move {
            let resp = self
                .client
                .call(
                    "readUriChunk",
                    json!({ "handle": self.handle, "maxBytes": buf.len() }),
                )
                .await?;
            let eof = resp.get("eof").and_then(Value::as_bool).unwrap_or(false);
            let encoded = resp.get("data").and_then(Value::as_str).unwrap_or("");
            if encoded.is_empty() && eof {
                return Ok(0);
            }
            let decoded = BASE64.decode(encoded).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Invalid base64 chunk from Android URI reader: {err}."),
                )
            })?;
            let take = decoded.len().min(buf.len());
            buf[..take].copy_from_slice(&decoded[..take]);
            Ok(take)
        })
    }

    fn seek_to<'a>(&'a mut self, offset: u64) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            let _ = self
                .client
                .call(
                    "skipUriBytes",
                    json!({ "handle": self.handle, "bytes": offset }),
                )
                .await?;
            Ok(())
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PickedFilesResponse {
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    files: Vec<AndroidPickedFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AndroidPickedFile {
    uri: String,
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    mime: Option<String>,
}

impl TransferPlatform for AndroidTransferPlatform {
    fn before_user_transfer(&self) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async move {
            if self
                .notification_permission_asked
                .swap(true, Ordering::SeqCst)
            {
                return Ok(());
            }
            let Ok(plugin) = self.plugin_handle() else {
                return Ok(());
            };
            let _: Result<Value, _> = plugin
                .0
                .run_mobile_plugin_async("ensureNotificationPermission", json!({}))
                .await;
            Ok(())
        })
    }

    fn is_unmetered_wifi(&self) -> bool {
        self.unmetered_wifi.load(Ordering::Acquire)
    }

    fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>> {
        Box::pin(async move {
            let plugin = self.plugin_handle()?;
            let resp: PickedFilesResponse = plugin
                .0
                .run_mobile_plugin_async("pickUploadFiles", json!({}))
                .await
                .map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not open Android file picker: {err}."),
                    )
                })?;
            if resp.cancelled {
                return Ok(Vec::new());
            }
            Ok(resp
                .files
                .into_iter()
                .map(|f| PickedUploadFile {
                    local_path: f.uri,
                    name: f.name,
                    size: f.size,
                    mime: f.mime,
                })
                .collect())
        })
    }

    fn open_upload_source<'a>(
        &'a self,
        local_path: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn UploadByteSource>, AppError>> {
        Box::pin(async move {
            if !local_path.starts_with("content://") {
                let src = crate::transfers::FileUploadSource::open(Path::new(local_path)).await?;
                return Ok(Box::new(src) as Box<dyn UploadByteSource>);
            }
            let client = self.wait_for_client().await?;
            let resp = client
                .call("openUriRead", json!({ "uri": local_path }))
                .await?;
            let handle = resp.get("handle").and_then(Value::as_u64).ok_or_else(|| {
                AppError::new(
                    ErrorCode::Failed,
                    "Android bridge did not return a URI reader handle.",
                )
            })?;
            let size = resp.get("size").and_then(Value::as_u64).filter(|&s| s > 0);
            Ok(Box::new(AndroidUriUploadSource {
                client,
                handle,
                size,
                closed: false,
            }) as Box<dyn UploadByteSource>)
        })
    }

    fn release_upload_source<'a>(&'a self, local_path: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if !local_path.starts_with("content://") {
                return;
            }
            if let Ok(client) = self.wait_for_client().await {
                let _ = client
                    .call("releaseUriPermission", json!({ "uri": local_path }))
                    .await;
            }
        })
    }

    fn finalize_download<'a>(
        &'a self,
        id: &'a str,
        part_path: &'a Path,
        file_name: &'a str,
        mime: Option<&'a str>,
        open_after_done: bool,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(async move {
            let client = self.wait_for_client().await?;
            let method = if open_after_done {
                "stageOpenExternally"
            } else {
                "saveToDownloads"
            };
            let resp = client
                .call(
                    method,
                    json!({
                        "transferId": id,
                        "tempPath": part_path.to_string_lossy(),
                        "fileName": file_name,
                        "mime": mime.unwrap_or("application/octet-stream"),
                    }),
                )
                .await?;
            let uri = resp
                .get("uri")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    AppError::new(
                        ErrorCode::Failed,
                        "Android bridge did not return a saved file URI.",
                    )
                })?;
            Ok(uri)
        })
    }

    fn delete_saved_download<'a>(
        &'a self,
        local_path: &'a str,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            if !local_path.starts_with("content://") {
                return crate::transfers::remove_file_if_present(Path::new(local_path)).await;
            }
            let client = self.wait_for_client().await?;
            let resp = client
                .call("deleteSavedDownload", json!({ "uriOrPath": local_path }))
                .await?;
            if resp.get("ok").and_then(Value::as_bool) == Some(false) {
                let msg = resp
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Android could not remove the saved download.");
                return Err(AppError::new(ErrorCode::Failed, msg.to_owned())
                    .with_hint("Legion will keep retrying removal."));
            }
            Ok(())
        })
    }

    fn cached_file_exists<'a>(
        &'a self,
        local_path: &'a str,
        expected_size: u64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let Ok(client) = self.wait_for_client().await else {
                return false;
            };
            let Ok(resp) = client
                .call(
                    "checkCacheFile",
                    json!({
                        "uriOrPath": local_path,
                        "expectedSize": expected_size,
                    }),
                )
                .await
            else {
                return false;
            };
            resp.get("exists").and_then(Value::as_bool).unwrap_or(false)
        })
    }

    fn open_local_file<'a>(
        &'a self,
        local_path: &'a str,
        mime: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            let client = self.wait_for_client().await?;
            let _ = client
                .call(
                    "openTransferFile",
                    json!({
                        "uriOrPath": local_path,
                        "mime": mime.unwrap_or("application/octet-stream"),
                    }),
                )
                .await?;
            Ok(())
        })
    }

    fn show_in_folder<'a>(&'a self, local_path: &'a str) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            let client = self.wait_for_client().await?;
            let _ = client
                .call("showInFolder", json!({ "uriOrPath": local_path }))
                .await?;
            Ok(())
        })
    }

    fn on_queue_updated(&self, summary: QueueNotificationSummary) {
        let seq = self.summary_seq.fetch_add(1, Ordering::AcqRel) + 1;
        *self
            .latest_summary
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((seq, summary));
        self.sync_notify.notify_one();
    }

    fn on_transfer_completed(&self, transfer: &DeviceTransfer) {
        let client = self
            .client
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(client) = client {
            let payload = json!({
                "transferId": transfer.id,
                "direction": transfer.direction,
                "name": transfer.name,
                "localUri": transfer.local_path,
                "mime": crate::transfers::mime_for_file_name(&transfer.name),
                "canOpen": transfer.can_open,
                "openBlockedReason": transfer.open_blocked_reason,
            });
            crate::transfers::spawn_bg(async move {
                let _ = client.call("notifyComplete", payload).await;
            });
        }
    }
}
