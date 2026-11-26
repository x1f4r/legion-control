//! Native background audio bridge for Android (`MediaSessionService` + Media3 `ExoPlayer`).
//!
//! Android WebView halts background track transitions when the screen is off and cannot expose a
//! system `MediaSession` or media notification reliably. On Android, `MusicPlaybackService` runs
//! in-process and talks to [`AudioManager`] over an owner-only (`0o600`, 256-bit random token)
//! Unix domain socket at `<data_dir>/audio/bridge.sock`.
//!
//! Keeping this bridge in Rust means `MusicPlaybackService` can mint fresh short-lived loopback
//! content URLs, revoke previous tickets, fetch `fs.tags` / `fs.thumb` metadata for auto-advanced
//! tracks, and detect mid-playback device revocation while the screen is off and the WebView is
//! asleep.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{AppError, ErrorCode};
use crate::content::ContentUrlStatus;

/// Event emitted to the web view whenever the native Android music service state changes.
pub const EVENT_AUDIO_STATE: &str = "legion://audio-state";

/// Interval for the active-playback device revocation check on Android.
///
/// Matches the node's 2-second revocation watchdog so even a short song that finished buffering
/// into ExoPlayer stops promptly when this device is removed from the node.
pub const REVOCATION_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Managed Tauri state wrapper for [`AudioManager`].
pub struct SharedAudio(pub std::sync::Arc<AudioManager>);

/// Classified playback failure returned to Kotlin or emitted to the web view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioFailure {
    /// Error category matching the UI player contract (`denied`, `not_found`, `too_large`,
    /// `unreachable`, `decode`).
    pub code: String,
    /// Clear single-sentence human explanation shown in the player and notification.
    pub message: String,
    /// True when the failure was caused by device access revocation or authentication loss.
    pub revoked: bool,
}

/// Formats the clear human sentence shown when a machine revokes this device during playback.
pub fn format_revocation_message(machine: &str) -> String {
    let trimmed = machine.trim();
    if trimmed.is_empty() {
        "This device no longer has access to this machine.".to_owned()
    } else {
        format!("This device no longer has access to {trimmed}.")
    }
}

/// Returns `true` when `err` indicates that this device was revoked, unpaired, or failed SSH
/// authentication on the target node.
pub fn is_revocation_error(err: &AppError) -> bool {
    if matches!(err.code, ErrorCode::Denied | ErrorCode::NotPaired) {
        return true;
    }
    matches!(
        err.details
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str),
        Some("device_removed_or_unknown" | "auth_failed" | "host_key_mismatch")
    )
}

/// Classifies a playback failure using the loopback content status and an optional `meta.whoami`
/// probe error so revocation, file permission errors, missing files, and network loss each produce
/// a clear sentence.
pub fn classify_audio_failure(
    status: &ContentUrlStatus,
    whoami_err: Option<&AppError>,
    machine: &str,
) -> AudioFailure {
    if whoami_err.is_some_and(is_revocation_error) {
        return AudioFailure {
            code: "denied".to_owned(),
            message: format_revocation_message(machine),
            revoked: true,
        };
    }

    let detail_msg = status
        .error_message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if status.status == Some(403) || status.error_code.as_deref() == Some("denied") {
        let lower = detail_msg.unwrap_or("").to_ascii_lowercase();
        let looks_revoked = lower.contains("revok")
            || lower.contains("removed")
            || lower.contains("authentication")
            || lower.contains("not paired");
        return AudioFailure {
            code: "denied".to_owned(),
            message: if looks_revoked {
                format_revocation_message(machine)
            } else {
                detail_msg
                    .unwrap_or("You're not allowed to open this file.")
                    .to_owned()
            },
            revoked: looks_revoked,
        };
    }

    if status.status == Some(413) || status.error_code.as_deref() == Some("too_large_for_inline") {
        return AudioFailure {
            code: "too_large".to_owned(),
            message: detail_msg
                .unwrap_or("This file is too large to stream here.")
                .to_owned(),
            revoked: false,
        };
    }

    if matches!(status.status, Some(404 | 410)) || status.error_code.as_deref() == Some("not_found")
    {
        return AudioFailure {
            code: "not_found".to_owned(),
            message: detail_msg
                .unwrap_or("This audio file is no longer available.")
                .to_owned(),
            revoked: false,
        };
    }

    let trimmed_machine = machine.trim();
    let fallback = if trimmed_machine.is_empty() {
        "Playback stopped because the machine could not be reached.".to_owned()
    } else {
        format!("Could not reach {trimmed_machine} to keep playing.")
    };
    AudioFailure {
        code: "unreachable".to_owned(),
        message: detail_msg.unwrap_or(&fallback).to_owned(),
        revoked: false,
    }
}

/// Extracts the `ok` tag object for `path` from a `fs.tags` response.
pub fn extract_tags_for_path(response: &Value, path: &str) -> Option<Value> {
    let results = response.get("results")?.as_array()?;
    for item in results {
        if item.get("path").and_then(Value::as_str) == Some(path)
            && let Some(ok) = item.get("ok")
            && ok.is_object()
        {
            return Some(ok.clone());
        }
    }
    None
}

/// Extracts `(base64_content, mime)` for `path` from a `fs.thumb` response.
pub fn extract_thumb_for_path(response: &Value, path: &str) -> Option<(String, String)> {
    let results = response.get("results")?.as_array()?;
    for item in results {
        if item.get("path").and_then(Value::as_str) == Some(path)
            && let Some(ok) = item.get("ok")
        {
            let content = ok.get("content").and_then(Value::as_str)?;
            let mime = ok.get("mime").and_then(Value::as_str)?;
            if !content.is_empty() && !mime.is_empty() {
                return Some((content.to_owned(), mime.to_owned()));
            }
        }
    }
    None
}

/// Validates a non-empty bridge hello token match in constant time for equal lengths.
pub fn verify_bridge_token(expected: &str, candidate: &str) -> bool {
    let a = expected.as_bytes();
    let b = candidate.as_bytes();
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(target_os = "android")]
mod platform {
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tauri::{AppHandle, Emitter, Manager};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot};

    use super::{
        EVENT_AUDIO_STATE, REVOCATION_POLL_INTERVAL, classify_audio_failure, extract_tags_for_path,
        extract_thumb_for_path, format_revocation_message, is_revocation_error,
        verify_bridge_token,
    };
    use crate::backend::{AppError, Backend, CallRequest, ErrorCode};
    use crate::content::AppContentServer;

    /// Managed Tauri plugin handle for `com.x1f4r.legion.AudioPlugin` on Android.
    pub struct AndroidAudioPluginHandle(pub tauri::plugin::PluginHandle<tauri::Wry>);

    struct BridgeClient {
        writer: AsyncMutex<tokio::net::unix::OwnedWriteHalf>,
        pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, AppError>>>>,
        next_id: AtomicU64,
    }

    impl BridgeClient {
        async fn write_json(&self, payload: &Value) -> Result<(), AppError> {
            let mut line = serde_json::to_vec(payload).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not encode Android audio bridge message: {err}."),
                )
            })?;
            line.push(b'\n');
            let mut writer = self.writer.lock().await;
            writer.write_all(&line).await.map_err(|err| {
                AppError::new(
                    ErrorCode::Unreachable,
                    format!("Android audio bridge write failed: {err}."),
                )
            })?;
            writer.flush().await.map_err(|err| {
                AppError::new(
                    ErrorCode::Unreachable,
                    format!("Android audio bridge flush failed: {err}."),
                )
            })
        }

        async fn call(&self, method: &str, params: Value) -> Result<Value, AppError> {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, tx);
            }
            if let Err(err) = self
                .write_json(&json!({
                    "id": id,
                    "method": method,
                    "params": params,
                }))
                .await
            {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                return Err(err);
            }

            tokio::time::timeout(Duration::from_secs(20), rx)
                .await
                .map_err(|_| {
                    self.pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    AppError::new(
                        ErrorCode::Timeout,
                        format!("Android audio bridge timed out on `{method}`."),
                    )
                })?
                .map_err(|_| {
                    AppError::new(
                        ErrorCode::Unreachable,
                        "Android audio bridge closed before replying.",
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
                    "Android audio bridge disconnected.",
                )));
            }
        }
    }

    /// Native Android audio manager bridging Tauri commands and `MusicPlaybackService`.
    pub struct AudioManager {
        app_handle: AppHandle,
        backend: Arc<dyn Backend>,
        content_server: Arc<AppContentServer>,
        socket_path: PathBuf,
        auth_token: String,
        plugin: Mutex<Option<Arc<AndroidAudioPluginHandle>>>,
        client: Mutex<Option<Arc<BridgeClient>>>,
        latest_state: Mutex<Value>,
        active_loaded: AtomicBool,
        active_playing: AtomicBool,
        active_machine: Mutex<Option<String>>,
        active_url: Mutex<Option<String>>,
        notification_permission_asked: AtomicBool,
        revocation_notify: Notify,
        shutting_down: AtomicBool,
    }

    impl AudioManager {
        /// Creates the Android audio bridge and binds `<data_dir>/audio/bridge.sock` (`0o600`).
        pub fn new(
            app_handle: AppHandle,
            data_dir: &Path,
            backend: Arc<dyn Backend>,
            content_server: Arc<AppContentServer>,
        ) -> Result<Arc<Self>, AppError> {
            let dir = data_dir.join("audio");
            fs::create_dir_all(&dir).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not create Android audio directory: {err}."),
                )
            })?;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
            let socket_path = dir.join("bridge.sock");
            let _ = fs::remove_file(&socket_path);

            let mut token_bytes = [0u8; 32];
            getrandom::fill(&mut token_bytes).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not generate Android audio bridge token: {err}."),
                )
            })?;
            let auth_token = token_bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();

            let manager = Arc::new(Self {
                app_handle,
                backend,
                content_server,
                socket_path,
                auth_token,
                plugin: Mutex::new(None),
                client: Mutex::new(None),
                latest_state: Mutex::new(json!({ "loaded": false })),
                active_loaded: AtomicBool::new(false),
                active_playing: AtomicBool::new(false),
                active_machine: Mutex::new(None),
                active_url: Mutex::new(None),
                notification_permission_asked: AtomicBool::new(false),
                revocation_notify: Notify::new(),
                shutting_down: AtomicBool::new(false),
            });

            let std_listener = std::os::unix::net::UnixListener::bind(&manager.socket_path)
                .map_err(|err| {
                    AppError::new(
                        ErrorCode::Internal,
                        format!("Could not bind Android audio bridge socket: {err}."),
                    )
                })?;
            let _ = fs::set_permissions(&manager.socket_path, fs::Permissions::from_mode(0o600));
            std_listener.set_nonblocking(true).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not configure Android audio bridge socket: {err}."),
                )
            })?;

            let accept_mgr = Arc::clone(&manager);
            crate::transfers::spawn_bg(async move {
                match UnixListener::from_std(std_listener) {
                    Ok(listener) => accept_mgr.accept_loop(listener).await,
                    Err(err) => {
                        #[cfg(debug_assertions)]
                        log::error!(
                            target: "legion-audio",
                            "Could not register Android audio bridge socket: {err}"
                        );
                        #[cfg(not(debug_assertions))]
                        {
                            let _ = err;
                            log::error!(
                                target: "legion-audio",
                                "Could not register Android audio bridge socket"
                            );
                        }
                    }
                }
            });

            let watch_mgr = Arc::clone(&manager);
            crate::transfers::spawn_bg(async move {
                watch_mgr.revocation_watch_loop().await;
            });

            Ok(manager)
        }

        /// Attaches `AudioPlugin` and instructs Kotlin's `AudioBridge` singleton to connect to our
        /// private Unix domain socket.
        pub fn attach_plugin(self: &Arc<Self>, plugin: Arc<AndroidAudioPluginHandle>) {
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
                        "connectAudioBridge",
                        json!({
                            "socketPath": socket_path,
                            "token": token,
                        }),
                    )
                    .await;
                if let Err(err) = res {
                    #[cfg(debug_assertions)]
                    log::warn!(target: "legion-audio", "connectAudioBridge failed: {err}");
                    #[cfg(not(debug_assertions))]
                    {
                        let _ = err;
                        log::warn!(target: "legion-audio", "connectAudioBridge failed");
                    }
                }
            });
        }

        /// Returns `true` while a song is loaded in the Android background service (playing or
        /// paused) so `RunEvent::ExitRequested` keeps the process alive when `MainActivity` is
        /// dismissed from Recents.
        pub fn is_active(&self) -> bool {
            self.active_loaded.load(Ordering::Acquire)
        }

        /// Returns `true` while audio is actively playing in the native service.
        pub fn is_playing(&self) -> bool {
            self.active_playing.load(Ordering::Acquire)
        }

        /// Returns the last native playback state snapshot.
        pub fn state(&self) -> Value {
            self.latest_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// Asks for Android 13+ notification permission once when music starts for the first time,
        /// showing a single-sentence rationale before the system prompt. Never blocks playback if
        /// refused.
        pub async fn request_notification_permission(&self) -> bool {
            if self
                .notification_permission_asked
                .swap(true, Ordering::SeqCst)
            {
                return true;
            }
            let plugin = self
                .plugin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let Some(plugin) = plugin else {
                return false;
            };
            let res: Result<Value, _> = plugin
                .0
                .run_mobile_plugin_async("ensureMusicNotificationPermission", json!({}))
                .await;
            res.ok()
                .and_then(|v| v.get("granted").and_then(Value::as_bool))
                .unwrap_or(false)
        }

        /// Loads a folder queue into `MusicPlaybackService` and starts preparing/playing the
        /// chosen track.
        pub async fn load_queue(&self, payload: Value) -> Result<Value, AppError> {
            let client = self.wait_for_client().await?;
            client.call("loadQueue", payload).await
        }

        /// Sends a playback command (`play`, `pause`, `toggle`, `next`, `prev`, `seek`, `speed`)
        /// to `MusicPlaybackService`.
        pub async fn command(&self, action: &str, payload: Value) -> Result<Value, AppError> {
            let client = self.wait_for_client().await?;
            let mut params = match payload {
                Value::Object(map) => map,
                _ => serde_json::Map::new(),
            };
            params.insert("action".to_owned(), Value::String(action.to_owned()));
            client.call("command", Value::Object(params)).await
        }

        /// Updates shuffle/repeat modes and the active playback order in `MusicPlaybackService`.
        pub async fn set_modes(&self, payload: Value) -> Result<Value, AppError> {
            let client = self.wait_for_client().await?;
            client.call("setModes", payload).await
        }

        /// Dismisses playback completely, releasing ExoPlayer, foreground service, and ticket.
        pub async fn dismiss(&self) -> Result<Value, AppError> {
            self.active_loaded.store(false, Ordering::Release);
            self.active_playing.store(false, Ordering::Release);
            self.revocation_notify.notify_one();
            self.revoke_active_url();
            *self
                .active_machine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            *self
                .latest_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = json!({ "loaded": false });
            let Some(client) = self
                .client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            else {
                return Ok(json!({ "ok": true }));
            };
            client.call("dismiss", json!({})).await
        }

        /// Dismisses native playback if it belongs to `machine` (called on `unpair`).
        pub async fn on_machine_unpaired(&self, machine: &str) {
            let matches = self
                .active_machine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_deref()
                .is_some_and(|m| m == machine);
            if matches {
                let _ = self.dismiss().await;
            }
        }

        /// Returns the debug-only 1-second position/metadata/event ring buffer from
        /// `MusicPlaybackService`.
        #[cfg(debug_assertions)]
        pub async fn debug_trace(&self) -> Result<Value, AppError> {
            let client = self.wait_for_client().await?;
            client.call("debugTrace", json!({})).await
        }

        /// Stops background watchers and revokes any live audio ticket when the app exits.
        pub fn shutdown(&self) {
            self.shutting_down.store(true, Ordering::Release);
            self.active_loaded.store(false, Ordering::Release);
            self.active_playing.store(false, Ordering::Release);
            self.revocation_notify.notify_one();
            self.revoke_active_url();
            if let Some(client) = self
                .client
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
            {
                crate::transfers::spawn_bg(async move {
                    let _ = client.call("dismiss", json!({})).await;
                });
            }
        }

        fn revoke_active_url(&self) {
            let prev = self
                .active_url
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(url) = prev {
                self.content_server.revoke_url(&url);
            }
        }

        async fn wait_for_client(&self) -> Result<Arc<BridgeClient>, AppError> {
            for _ in 0..60 {
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
                "Android audio playback service is not connected.",
            ))
        }

        async fn accept_loop(self: Arc<Self>, listener: UnixListener) {
            loop {
                if self.shutting_down.load(Ordering::Acquire) {
                    break;
                }
                let Ok((stream, _)) = listener.accept().await else {
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
            let Ok(n) =
                tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut first_line))
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
            let hello_type = hello.get("type").and_then(Value::as_str);
            let hello_token = hello.get("token").and_then(Value::as_str).unwrap_or("");
            if hello_type != Some("hello") || !verify_bridge_token(&self.auth_token, hello_token) {
                log::warn!(target: "legion-audio", "rejected unauthenticated audio bridge connection");
                return;
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
            }
            log::info!(target: "legion-audio", "Android LocalSocket audio bridge connected");

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
                                let code = match msg.get("errorCode").and_then(Value::as_str) {
                                    Some("denied") => ErrorCode::Denied,
                                    Some("not_found") => ErrorCode::NotFound,
                                    Some("unsupported") => ErrorCode::Unsupported,
                                    Some("unreachable") => ErrorCode::Unreachable,
                                    _ => ErrorCode::Failed,
                                };
                                client.resolve(id, Err(AppError::new(code, err_msg.to_owned())));
                            } else {
                                let result = msg.get("result").cloned().unwrap_or(Value::Null);
                                client.resolve(id, Ok(result));
                            }
                        } else if let Some(req_id) = msg.get("reqId").and_then(Value::as_u64) {
                            let method = msg
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let params = msg.get("params").cloned().unwrap_or(Value::Null);
                            let this = Arc::clone(&self);
                            let reply_client = Arc::clone(&client);
                            tokio::spawn(async move {
                                this.handle_kotlin_request(reply_client, req_id, method, params)
                                    .await;
                            });
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
            match event {
                "audio_state" => {
                    let Some(state) = msg.get("state") else {
                        return;
                    };
                    let loaded = state
                        .get("loaded")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let playing = loaded
                        && state
                            .get("playing")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                    let machine = if loaded {
                        state
                            .get("machine")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    } else {
                        None
                    };

                    let was_playing = self.active_playing.swap(playing, Ordering::AcqRel);
                    self.active_loaded.store(loaded, Ordering::Release);
                    *self
                        .active_machine
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = machine;
                    if !loaded {
                        self.revoke_active_url();
                    }
                    *self
                        .latest_state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = state.clone();
                    if playing != was_playing {
                        self.revocation_notify.notify_one();
                    }
                    let _ = self.app_handle.emit(EVENT_AUDIO_STATE, state.clone());
                }
                "activity_created" if self.app_handle.get_webview_window("main").is_none() => {
                    let app = self.app_handle.clone();
                    tokio::task::spawn_blocking(move || {
                        if app.get_webview_window("main").is_none() {
                            log::info!(
                                target: "legion-audio",
                                "recreating main WebView window for resumed Android Activity"
                            );
                            let _ = crate::create_main_webview_window(&app);
                        }
                    });
                }
                _ => {}
            }
        }

        async fn handle_kotlin_request(
            self: Arc<Self>,
            client: Arc<BridgeClient>,
            req_id: u64,
            method: String,
            params: Value,
        ) {
            let reply = match method.as_str() {
                "mintTrack" => self.rpc_mint_track(params).await,
                "revokeUrl" => {
                    if let Some(url) = params.get("url").and_then(Value::as_str) {
                        self.content_server.revoke_url(url);
                    }
                    Ok(json!({ "ok": true }))
                }
                "inspectTrackError" => self.rpc_inspect_track_error(params).await,
                _ => Err((
                    "unsupported".to_owned(),
                    format!("Unknown audio bridge method: {method}"),
                    false,
                )),
            };

            let envelope = match reply {
                Ok(result) => json!({
                    "respId": req_id,
                    "result": result,
                }),
                Err((error_code, message, revoked)) => json!({
                    "respId": req_id,
                    "error": message,
                    "errorCode": error_code,
                    "revoked": revoked,
                }),
            };
            let _ = client.write_json(&envelope).await;
        }

        async fn probe_whoami(&self, machine: &str) -> Result<(), AppError> {
            let req = CallRequest {
                id: None,
                tool: "meta.whoami".to_owned(),
                args: json!({}),
                on: Some(Value::String(machine.to_owned())),
            };
            match tokio::time::timeout(Duration::from_secs(4), self.backend.call(req, None)).await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(err)) => Err(err),
                Err(_) => Err(AppError::new(
                    ErrorCode::Timeout,
                    format!("Timed out checking access to {machine}."),
                )),
            }
        }

        async fn rpc_mint_track(&self, params: Value) -> Result<Value, (String, String, bool)> {
            let machine = params
                .get("machine")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_owned();
            let path = params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let prev_url = params
                .get("prevUrl")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let fetch_meta = params
                .get("fetchMeta")
                .and_then(Value::as_bool)
                .unwrap_or(true);

            if let Some(prev) = prev_url {
                self.content_server.revoke_url(prev);
            }
            if machine.is_empty() || path.is_empty() {
                return Err((
                    "invalid_args".to_owned(),
                    "Machine and track path are required.".to_owned(),
                    false,
                ));
            }

            let minted = match self.content_server.mint_url(&machine, &path, false).await {
                Ok(res) => res,
                Err(err) => {
                    if is_revocation_error(&err) {
                        return Err((
                            "denied".to_owned(),
                            format_revocation_message(&machine),
                            true,
                        ));
                    }
                    if err.code == ErrorCode::Unreachable
                        && let Err(who_err) = self.probe_whoami(&machine).await
                        && is_revocation_error(&who_err)
                    {
                        return Err((
                            "denied".to_owned(),
                            format_revocation_message(&machine),
                            true,
                        ));
                    }
                    let code = match err.code {
                        ErrorCode::Denied => "denied",
                        ErrorCode::NotFound => "not_found",
                        ErrorCode::Unsupported => "unsupported",
                        ErrorCode::Timeout => "timeout",
                        _ => "unreachable",
                    };
                    return Err((code.to_owned(), err.message, false));
                }
            };

            {
                let mut prev = self
                    .active_url
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(old) = prev.replace(minted.url.clone())
                    && old != minted.url
                {
                    self.content_server.revoke_url(&old);
                }
            }
            *self
                .active_machine
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(machine.clone());

            let mut tags: Option<Value> = None;
            let mut cover_base64: Option<String> = None;
            let mut cover_mime: Option<String> = None;

            if fetch_meta {
                let tags_req = CallRequest {
                    id: None,
                    tool: "fs.tags".to_owned(),
                    args: json!({ "paths": [path.clone()] }),
                    on: Some(Value::String(machine.clone())),
                };
                let thumb_req = CallRequest {
                    id: None,
                    tool: "fs.thumb".to_owned(),
                    args: json!({ "paths": [path.clone()], "size": 512, "fit": "contain" }),
                    on: Some(Value::String(machine.clone())),
                };
                let meta_timeout = Duration::from_millis(1500);
                let (tags_res, thumb_res) = tokio::join!(
                    tokio::time::timeout(meta_timeout, self.backend.call(tags_req, None)),
                    tokio::time::timeout(meta_timeout, self.backend.call(thumb_req, None)),
                );
                if let Ok(Ok(val)) = tags_res {
                    tags = extract_tags_for_path(&val, &path);
                }
                if let Ok(Ok(val)) = thumb_res
                    && let Some((b64, mime)) = extract_thumb_for_path(&val, &path)
                {
                    cover_base64 = Some(b64);
                    cover_mime = Some(mime);
                }
            }

            Ok(json!({
                "url": minted.url,
                "expiresIn": minted.expires_in,
                "tags": tags,
                "coverBase64": cover_base64,
                "coverMime": cover_mime,
            }))
        }

        async fn rpc_inspect_track_error(
            &self,
            params: Value,
        ) -> Result<Value, (String, String, bool)> {
            let machine = params
                .get("machine")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_owned();
            let url = params.get("url").and_then(Value::as_str).unwrap_or("");
            let status = self.content_server.content_status_for_url(url);
            let whoami_err = if machine.is_empty() {
                None
            } else {
                self.probe_whoami(&machine).await.err()
            };
            let classified = classify_audio_failure(&status, whoami_err.as_ref(), &machine);
            if classified.revoked {
                self.revoke_active_url();
                let _ = self.backend.invalidate_sessions().await;
            }
            Ok(json!({
                "code": classified.code,
                "message": classified.message,
                "revoked": classified.revoked,
            }))
        }

        async fn revocation_watch_loop(self: Arc<Self>) {
            loop {
                if self.shutting_down.load(Ordering::Acquire) {
                    break;
                }
                if !self.active_playing.load(Ordering::Acquire) {
                    self.revocation_notify.notified().await;
                    continue;
                }
                tokio::select! {
                    () = self.revocation_notify.notified() => {
                        continue;
                    }
                    () = tokio::time::sleep(REVOCATION_POLL_INTERVAL) => {}
                }
                if self.shutting_down.load(Ordering::Acquire)
                    || !self.active_playing.load(Ordering::Acquire)
                {
                    continue;
                }
                let Some(machine) = self
                    .active_machine
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                else {
                    continue;
                };

                let mut probe = self.probe_whoami(&machine).await;
                if let Err(ref err) = probe
                    && err.code == ErrorCode::Unreachable
                    && self.active_playing.load(Ordering::Acquire)
                {
                    // When the node terminates a live SSH session on revocation, the first RPC on
                    // that cached channel can fail with EOF (`Unreachable`) before reconnect sees
                    // `Denied` (`auth_failed`). Retry once right away so revocation stops within
                    // the watchdog window.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if self.active_playing.load(Ordering::Acquire) {
                        probe = self.probe_whoami(&machine).await;
                    }
                }

                if let Err(err) = probe
                    && is_revocation_error(&err)
                {
                    let still_same = self.active_playing.load(Ordering::Acquire)
                        && self
                            .active_machine
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .as_deref()
                            == Some(machine.as_str());
                    if !still_same {
                        continue;
                    }
                    let message = format_revocation_message(&machine);
                    log::warn!(
                        target: "legion-audio",
                        "Device access revoked on {machine} during playback; stopping native audio"
                    );
                    self.active_playing.store(false, Ordering::Release);
                    self.revoke_active_url();
                    let _ = self.backend.invalidate_sessions().await;
                    let client = self
                        .client
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if let Some(client) = client {
                        let _ = client
                            .call(
                                "stopWithError",
                                json!({
                                    "code": "denied",
                                    "message": message,
                                    "revoked": true,
                                }),
                            )
                            .await;
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
mod platform {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use crate::backend::{AppError, ErrorCode};

    /// Lightweight non-Android stub so audio commands compile cleanly on desktop and iOS.
    #[derive(Default)]
    pub struct AudioManager;

    impl AudioManager {
        /// Creates a no-op [`AudioManager`] for platforms that play music in the WebView.
        pub fn new() -> Arc<Self> {
            Arc::new(Self)
        }

        /// Always `false` off Android.
        pub fn is_active(&self) -> bool {
            false
        }

        /// Always `false` off Android.
        pub fn is_playing(&self) -> bool {
            false
        }

        /// Returns `{ "loaded": false }` off Android.
        pub fn state(&self) -> Value {
            json!({ "loaded": false })
        }

        /// Returns `false` off Android.
        pub async fn request_notification_permission(&self) -> bool {
            false
        }

        /// Returns `Unsupported` off Android.
        pub async fn load_queue(&self, _payload: Value) -> Result<Value, AppError> {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Native background audio service is only used on Android.",
            ))
        }

        /// Returns `Unsupported` off Android.
        pub async fn command(&self, _action: &str, _payload: Value) -> Result<Value, AppError> {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Native background audio service is only used on Android.",
            ))
        }

        /// Returns `Unsupported` off Android.
        pub async fn set_modes(&self, _payload: Value) -> Result<Value, AppError> {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Native background audio service is only used on Android.",
            ))
        }

        /// No-op off Android.
        pub async fn dismiss(&self) -> Result<Value, AppError> {
            Ok(json!({ "ok": true }))
        }

        /// No-op off Android.
        pub async fn on_machine_unpaired(&self, _machine: &str) {}

        /// Returns an empty debug trace off Android.
        #[cfg(debug_assertions)]
        pub async fn debug_trace(&self) -> Result<Value, AppError> {
            Ok(json!({ "supported": false, "ticks": [], "events": [] }))
        }

        /// No-op off Android.
        pub fn shutdown(&self) {}
    }
}

#[cfg(target_os = "android")]
pub use platform::AndroidAudioPluginHandle;
pub use platform::AudioManager;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn revocation_message_is_clear_and_human() {
        assert_eq!(
            format_revocation_message("tower"),
            "This device no longer has access to tower."
        );
        assert_eq!(
            format_revocation_message("  pi  "),
            "This device no longer has access to pi."
        );
        assert_eq!(
            format_revocation_message(""),
            "This device no longer has access to this machine."
        );
    }

    #[test]
    fn revocation_error_detects_denied_not_paired_and_auth_details() {
        assert!(is_revocation_error(&AppError::new(
            ErrorCode::Denied,
            "Denied"
        )));
        assert!(is_revocation_error(&AppError::not_paired()));
        assert!(is_revocation_error(
            &AppError::new(ErrorCode::Failed, "SSH failed")
                .with_details(json!({ "reason": "auth_failed" }))
        ));
        assert!(is_revocation_error(
            &AppError::new(ErrorCode::Failed, "Removed")
                .with_details(json!({ "reason": "device_removed_or_unknown" }))
        ));
        assert!(!is_revocation_error(&AppError::new(
            ErrorCode::Unreachable,
            "Connection timed out"
        )));
        assert!(!is_revocation_error(&AppError::new(
            ErrorCode::NotFound,
            "File not found"
        )));
    }

    #[test]
    fn classify_audio_failure_prioritizes_revocation_over_stream_abort() {
        let status = ContentUrlStatus {
            request_count: 1,
            status: Some(206),
            content_type: Some("audio/mpeg".to_owned()),
            stream_failed: true,
            error_code: None,
            error_message: None,
            ticket_live: false,
        };
        let whoami_denied = AppError::new(ErrorCode::Denied, "Key rejected.")
            .with_details(json!({ "reason": "auth_failed" }));
        let failure = classify_audio_failure(&status, Some(&whoami_denied), "tower");
        assert_eq!(failure.code, "denied");
        assert!(failure.revoked);
        assert_eq!(
            failure.message,
            "This device no longer has access to tower."
        );
    }

    #[test]
    fn classify_audio_failure_distinguishes_file_permission_not_found_and_unreachable() {
        let forbidden = ContentUrlStatus {
            request_count: 1,
            status: Some(403),
            error_code: Some("denied".to_owned()),
            error_message: Some("Access denied to /root/secret.flac.".to_owned()),
            ..ContentUrlStatus::default()
        };
        let f = classify_audio_failure(&forbidden, None, "tower");
        assert_eq!(f.code, "denied");
        assert!(!f.revoked);
        assert_eq!(f.message, "Access denied to /root/secret.flac.");

        let missing = ContentUrlStatus {
            request_count: 1,
            status: Some(404),
            ..ContentUrlStatus::default()
        };
        let m = classify_audio_failure(&missing, None, "tower");
        assert_eq!(m.code, "not_found");
        assert!(!m.revoked);

        let offline = ContentUrlStatus {
            request_count: 1,
            status: Some(503),
            ..ContentUrlStatus::default()
        };
        let o = classify_audio_failure(&offline, None, "tower");
        assert_eq!(o.code, "unreachable");
        assert!(!o.revoked);
        assert_eq!(o.message, "Could not reach tower to keep playing.");
    }

    #[test]
    fn extracts_tags_and_thumbs_from_standard_tool_responses() {
        let tags_resp = json!({
            "results": [
                { "path": "/music/01.mp3", "ok": { "title": "First Song", "artist": "Artist A", "duration_s": 60 } },
                { "path": "/music/02.mp3", "error": { "code": "not_found" } }
            ]
        });
        let extracted = extract_tags_for_path(&tags_resp, "/music/01.mp3");
        assert_eq!(
            extracted
                .as_ref()
                .and_then(|v| v.get("title"))
                .and_then(Value::as_str),
            Some("First Song")
        );
        assert!(extract_tags_for_path(&tags_resp, "/music/02.mp3").is_none());

        let thumb_resp = json!({
            "results": [
                { "path": "/music/01.mp3", "ok": { "content": "YWJj", "mime": "image/jpeg" } }
            ]
        });
        assert_eq!(
            extract_thumb_for_path(&thumb_resp, "/music/01.mp3"),
            Some(("YWJj".to_owned(), "image/jpeg".to_owned()))
        );
        assert!(extract_thumb_for_path(&thumb_resp, "/music/missing.mp3").is_none());
    }

    #[test]
    fn verify_bridge_token_requires_exact_non_empty_match() {
        assert!(verify_bridge_token("abcdef0123", "abcdef0123"));
        assert!(!verify_bridge_token("abcdef0123", "abcdef0124"));
        assert!(!verify_bridge_token("abcdef0123", "abcdef"));
        assert!(!verify_bridge_token("", ""));
    }
}
