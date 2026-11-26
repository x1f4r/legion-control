//! Native background audio bridge for Android (`MediaSessionService` + Media3 `ExoPlayer`).
//!
//! Android WebView halts background track transitions when the screen is off and cannot expose a
//! system `MediaSession` or media notification reliably. On Android, `MusicPlaybackService` runs
//! in-process and talks to [`AudioManager`] over an owner-only (`0o600`, 256-bit random token)
//! Unix domain socket at `<data_dir>/audio/bridge.sock`.
//!
//! Keeping this bridge in Rust means `MusicPlaybackService` can mint fresh short-lived loopback
//! content URLs, revoke previous tickets, fetch `fs.tags` / `fs.thumb` metadata for auto-advanced
//! tracks, and classify stream or next-ticket failures (including device revocation) while the
//! screen is off and the WebView is asleep.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::{AppError, ErrorCode};
use crate::content::ContentUrlStatus;

/// Event emitted to the web view whenever the native Android music service state changes.
pub const EVENT_AUDIO_STATE: &str = "legion://audio-state";

/// Maximum newline-delimited JSON frame accepted from Kotlin over the Android audio bridge Unix
/// domain socket (64 KiB). Any line exceeding this cap closes the connection immediately.
pub const MAX_BRIDGE_LINE_BYTES: usize = 64 * 1024;

/// Failure returned by [`read_bounded_line`].
#[derive(Debug)]
pub enum BridgeReadError {
    /// A single line exceeded the configured byte cap (including its trailing newline, if any).
    LineTooLong,
    /// The frame bytes were not valid UTF-8.
    InvalidUtf8(std::string::FromUtf8Error),
    /// Underlying stream I/O failure.
    Io(std::io::Error),
}

/// Reads one `\n`-delimited UTF-8 frame from `reader` without ever buffering more than `max_bytes`
/// bytes (including the trailing `\n`). Returns `Ok(None)` at clean EOF with no unread bytes, or
/// [`BridgeReadError::LineTooLong`] immediately as soon as a frame would exceed `max_bytes`.
pub async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<String>, BridgeReadError> {
    let mut buf = Vec::new();
    loop {
        let available = tokio::io::AsyncBufReadExt::fill_buf(reader)
            .await
            .map_err(BridgeReadError::Io)?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return String::from_utf8(buf)
                .map(Some)
                .map_err(BridgeReadError::InvalidUtf8);
        }
        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if buf.len().saturating_add(take) > max_bytes {
                return Err(BridgeReadError::LineTooLong);
            }
            buf.extend_from_slice(&available[..take]);
            tokio::io::AsyncBufReadExt::consume(reader, take);
            return String::from_utf8(buf)
                .map(Some)
                .map_err(BridgeReadError::InvalidUtf8);
        }
        if buf.len().saturating_add(available.len()) > max_bytes {
            return Err(BridgeReadError::LineTooLong);
        }
        let take = available.len();
        buf.extend_from_slice(available);
        tokio::io::AsyncBufReadExt::consume(reader, take);
    }
}

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
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::{Mutex as AsyncMutex, oneshot};

    use super::{
        BridgeReadError, EVENT_AUDIO_STATE, MAX_BRIDGE_LINE_BYTES, classify_audio_failure,
        extract_tags_for_path, extract_thumb_for_path, format_revocation_message,
        is_revocation_error, read_bounded_line, verify_bridge_token,
    };
    use crate::backend::{AppError, Backend, CallRequest, ErrorCode};
    use crate::content::AppContentServer;

    struct CachedTrackMeta {
        path: String,
        tags: Option<Value>,
        cover_data_url: Option<String>,
    }

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
        cached_tracks: Mutex<Vec<Value>>,
        cached_order: Mutex<Vec<String>>,
        cached_track_meta: Mutex<Option<CachedTrackMeta>>,
        active_loaded: AtomicBool,
        active_playing: AtomicBool,
        active_machine: Mutex<Option<String>>,
        active_url: Mutex<Option<String>>,
        notification_permission_asked: AtomicBool,
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
                cached_tracks: Mutex::new(Vec::new()),
                cached_order: Mutex::new(Vec::new()),
                cached_track_meta: Mutex::new(None),
                active_loaded: AtomicBool::new(false),
                active_playing: AtomicBool::new(false),
                active_machine: Mutex::new(None),
                active_url: Mutex::new(None),
                notification_permission_asked: AtomicBool::new(false),
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
            let mut tracks_cache = Vec::new();
            let mut order_cache = Vec::new();
            if let Some(arr) = payload.get("tracks").and_then(Value::as_array) {
                for item in arr {
                    let Some(path) = item.get("path").and_then(Value::as_str) else {
                        continue;
                    };
                    if path.is_empty() {
                        continue;
                    }
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path));
                    let mut entry = json!({
                        "path": path,
                        "name": name,
                        "kind": "file",
                    });
                    if let Some(obj) = entry.as_object_mut() {
                        for key in ["size", "mtime", "mime", "duration_s"] {
                            if let Some(val) = item.get(key)
                                && !val.is_null()
                            {
                                obj.insert(key.to_owned(), val.clone());
                            }
                        }
                    }
                    tracks_cache.push(entry);
                    order_cache.push(path.to_owned());
                }
            }
            *self
                .cached_tracks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = tracks_cache;
            *self
                .cached_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = order_cache;
            *self
                .cached_track_meta
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
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
            if let Some(arr) = payload.get("order").and_then(Value::as_array) {
                let order: Vec<String> = arr
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                if !order.is_empty() {
                    *self
                        .cached_order
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = order;
                }
            }
            let client = self.wait_for_client().await?;
            client.call("setModes", payload).await
        }

        /// Dismisses playback completely, releasing ExoPlayer, foreground service, and ticket.
        pub async fn dismiss(&self) -> Result<Value, AppError> {
            self.active_loaded.store(false, Ordering::Release);
            self.active_playing.store(false, Ordering::Release);
            self.revoke_active_url();
            self.clear_cached_queue();
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
        /// `MusicPlaybackService`, paged so no bridge frame approaches [`MAX_BRIDGE_LINE_BYTES`].
        #[cfg(debug_assertions)]
        pub async fn debug_trace(&self) -> Result<Value, AppError> {
            let client = self.wait_for_client().await?;
            let mut ticks: Vec<Value> = Vec::new();
            let mut events: Vec<Value> = Vec::new();
            let mut state = json!({ "loaded": false });
            let mut supported = true;

            let mut ticks_offset: u64 = 0;
            let mut include_state = true;
            loop {
                let page = client
                    .call(
                        "debugTrace",
                        json!({
                            "ticksOffset": ticks_offset,
                            "ticksLimit": 100,
                            "eventsOffset": 0,
                            "eventsLimit": 0,
                            "includeState": include_state,
                        }),
                    )
                    .await?;
                if include_state {
                    supported = page
                        .get("supported")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if let Some(s) = page.get("state") {
                        state = s.clone();
                    }
                    include_state = false;
                }
                if let Some(arr) = page.get("ticks").and_then(Value::as_array) {
                    ticks.extend(arr.iter().cloned());
                }
                match page.get("nextTicksOffset").and_then(Value::as_u64) {
                    Some(next) if next > ticks_offset => ticks_offset = next,
                    _ => break,
                }
            }

            let mut events_offset: u64 = 0;
            loop {
                let page = client
                    .call(
                        "debugTrace",
                        json!({
                            "ticksOffset": 0,
                            "ticksLimit": 0,
                            "eventsOffset": events_offset,
                            "eventsLimit": 100,
                            "includeState": false,
                        }),
                    )
                    .await?;
                if let Some(arr) = page.get("events").and_then(Value::as_array) {
                    events.extend(arr.iter().cloned());
                }
                match page.get("nextEventsOffset").and_then(Value::as_u64) {
                    Some(next) if next > events_offset => events_offset = next,
                    _ => break,
                }
            }

            Ok(json!({
                "supported": supported,
                "state": state,
                "ticks": ticks,
                "events": events,
            }))
        }

        /// Stops background watchers and revokes any live audio ticket when the app exits.
        pub fn shutdown(&self) {
            self.shutting_down.store(true, Ordering::Release);
            self.active_loaded.store(false, Ordering::Release);
            self.active_playing.store(false, Ordering::Release);
            self.revoke_active_url();
            self.clear_cached_queue();
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

        fn clear_cached_queue(&self) {
            self.cached_tracks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            self.cached_order
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            *self
                .cached_track_meta
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
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
            let first_line = match tokio::time::timeout(
                Duration::from_secs(5),
                read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES),
            )
            .await
            {
                Ok(Ok(Some(line))) => line,
                Ok(Err(BridgeReadError::LineTooLong)) => {
                    log::warn!(
                        target: "legion-audio",
                        "rejected oversized audio bridge hello frame (> {MAX_BRIDGE_LINE_BYTES} bytes)"
                    );
                    return;
                }
                _ => return,
            };
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

            loop {
                let line = match read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await {
                    Ok(Some(line)) => line,
                    Ok(None) => break,
                    Err(BridgeReadError::LineTooLong) => {
                        log::warn!(
                            target: "legion-audio",
                            "closing audio bridge connection after oversized frame (> {MAX_BRIDGE_LINE_BYTES} bytes)"
                        );
                        break;
                    }
                    Err(BridgeReadError::InvalidUtf8(_)) => {
                        log::warn!(
                            target: "legion-audio",
                            "closing audio bridge connection after invalid UTF-8 frame"
                        );
                        break;
                    }
                    Err(BridgeReadError::Io(_)) => break,
                };
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

                    self.active_playing.store(playing, Ordering::Release);
                    self.active_loaded.store(loaded, Ordering::Release);
                    *self
                        .active_machine
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = machine;
                    if !loaded {
                        self.revoke_active_url();
                        self.clear_cached_queue();
                        let empty = json!({ "loaded": false });
                        *self
                            .latest_state
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = empty.clone();
                        let _ = self.app_handle.emit(EVENT_AUDIO_STATE, empty);
                        return;
                    }

                    let include_cover = msg
                        .get("includeCover")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let include_tracks = msg
                        .get("includeTracks")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let state_path = state.get("path").and_then(Value::as_str);

                    let mut full_state = state.clone();
                    let mut emit_state = state.clone();

                    let order_vec = self
                        .cached_order
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if !order_vec.is_empty() {
                        let order_val =
                            Value::Array(order_vec.into_iter().map(Value::String).collect());
                        if let Some(obj) = full_state.as_object_mut() {
                            obj.insert("order".to_owned(), order_val.clone());
                        }
                        if let Some(obj) = emit_state.as_object_mut() {
                            obj.insert("order".to_owned(), order_val);
                        }
                    }

                    let tracks_vec = self
                        .cached_tracks
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if !tracks_vec.is_empty() {
                        let tracks_val = Value::Array(tracks_vec);
                        if let Some(obj) = full_state.as_object_mut() {
                            obj.insert("tracks".to_owned(), tracks_val.clone());
                        }
                        if include_tracks && let Some(obj) = emit_state.as_object_mut() {
                            obj.insert("tracks".to_owned(), tracks_val);
                        }
                    }

                    if let Some(path) = state_path {
                        let meta_guard = self
                            .cached_track_meta
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        if let Some(meta) = meta_guard.as_ref().filter(|m| m.path == path) {
                            if let Some(ref tags) = meta.tags {
                                if let Some(obj) = full_state.as_object_mut() {
                                    obj.insert("tags".to_owned(), tags.clone());
                                }
                                if let Some(obj) = emit_state.as_object_mut() {
                                    obj.insert("tags".to_owned(), tags.clone());
                                }
                            }
                            if let Some(ref cover) = meta.cover_data_url {
                                if let Some(obj) = full_state.as_object_mut() {
                                    obj.insert(
                                        "coverDataUrl".to_owned(),
                                        Value::String(cover.clone()),
                                    );
                                }
                                if include_cover && let Some(obj) = emit_state.as_object_mut() {
                                    obj.insert(
                                        "coverDataUrl".to_owned(),
                                        Value::String(cover.clone()),
                                    );
                                }
                            }
                        }
                    }

                    *self
                        .latest_state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = full_state;
                    let _ = self.app_handle.emit(EVENT_AUDIO_STATE, emit_state);
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

        /// Runs `meta.whoami` strictly on an error path (stream cut or failed next-track ticket)
        /// so a revoked device is distinguished cleanly from a dropped connection. Retries once
        /// on `Unreachable` because the first RPC after the node terminates an SSH session can
        /// observe EOF before reconnect reports `Denied` (`auth_failed`).
        async fn probe_whoami_on_error(&self, machine: &str) -> Result<(), AppError> {
            let first = self.probe_whoami(machine).await;
            if let Err(ref err) = first
                && err.code == ErrorCode::Unreachable
            {
                tokio::time::sleep(Duration::from_millis(300)).await;
                return self.probe_whoami(machine).await;
            }
            first
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
                        self.revoke_active_url();
                        let _ = self.backend.invalidate_sessions().await;
                        return Err((
                            "denied".to_owned(),
                            format_revocation_message(&machine),
                            true,
                        ));
                    }
                    if err.code == ErrorCode::Unreachable
                        && let Err(who_err) = self.probe_whoami_on_error(&machine).await
                        && is_revocation_error(&who_err)
                    {
                        self.revoke_active_url();
                        let _ = self.backend.invalidate_sessions().await;
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
                let cover_data_url = match (&cover_base64, &cover_mime) {
                    (Some(b64), Some(mime)) => Some(format!("data:{mime};base64,{b64}")),
                    _ => None,
                };
                *self
                    .cached_track_meta
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedTrackMeta {
                    path: path.clone(),
                    tags: tags.clone(),
                    cover_data_url,
                });
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
                self.probe_whoami_on_error(&machine).await.err()
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

    #[tokio::test]
    async fn read_bounded_line_accepts_exact_cap_and_multiple_frames() {
        use tokio::io::BufReader;

        let exact_with_nl = format!("{}\n", "a".repeat(MAX_BRIDGE_LINE_BYTES - 1));
        assert_eq!(exact_with_nl.len(), MAX_BRIDGE_LINE_BYTES);
        let second = "{\"event\":\"audio_state\"}\n";
        let mut payload = Vec::with_capacity(exact_with_nl.len() + second.len());
        payload.extend_from_slice(exact_with_nl.as_bytes());
        payload.extend_from_slice(second.as_bytes());

        // Small buffer capacity forces multi-chunk reading across the 64 KiB line boundary.
        let mut reader = BufReader::with_capacity(1024, payload.as_slice());
        let first_read = read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await;
        assert!(matches!(first_read, Ok(Some(ref s)) if s == &exact_with_nl));

        let second_read = read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await;
        assert!(matches!(second_read, Ok(Some(ref s)) if s == second));

        let eof = read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await;
        assert!(matches!(eof, Ok(None)));

        // Exact cap at EOF with no trailing newline also succeeds.
        let exact_no_nl = "b".repeat(MAX_BRIDGE_LINE_BYTES);
        let mut reader_eof = BufReader::with_capacity(512, exact_no_nl.as_bytes());
        let line_eof = read_bounded_line(&mut reader_eof, MAX_BRIDGE_LINE_BYTES).await;
        assert!(matches!(line_eof, Ok(Some(ref s)) if s == &exact_no_nl));
    }

    #[tokio::test]
    async fn read_bounded_line_rejects_oversized_frames_with_or_without_newline() {
        use tokio::io::BufReader;

        // 64 KiB + 1 byte ending in '\n' (65536 payload bytes + '\n' = 65537).
        let over_with_nl = format!("{}\n", "x".repeat(MAX_BRIDGE_LINE_BYTES));
        assert_eq!(over_with_nl.len(), MAX_BRIDGE_LINE_BYTES + 1);
        let mut reader = BufReader::with_capacity(1024, over_with_nl.as_bytes());
        let err = read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await;
        assert!(matches!(err, Err(BridgeReadError::LineTooLong)));

        // 64 KiB + 1 byte with no newline at all (simulating an unbounded flood).
        let over_no_nl = "y".repeat(MAX_BRIDGE_LINE_BYTES + 1);
        let mut reader_no_nl = BufReader::with_capacity(1024, over_no_nl.as_bytes());
        let err_no_nl = read_bounded_line(&mut reader_no_nl, MAX_BRIDGE_LINE_BYTES).await;
        assert!(matches!(err_no_nl, Err(BridgeReadError::LineTooLong)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_unix_socket_reader_closes_connection_on_oversized_frame()
    -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let (server_stream, mut client_stream) = UnixStream::pair()?;

        let server_task = tokio::spawn(async move {
            let (read_half, mut write_half) = server_stream.into_split();
            let mut reader = BufReader::new(read_half);

            let first = read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await;
            assert!(matches!(first, Ok(Some(_))));
            if let Ok(Some(line)) = first {
                assert!(write_half.write_all(line.as_bytes()).await.is_ok());
            }

            // Next frame exceeds 64 KiB: reader must return LineTooLong and drop the socket.
            let second = read_bounded_line(&mut reader, MAX_BRIDGE_LINE_BYTES).await;
            assert!(matches!(second, Err(BridgeReadError::LineTooLong)));
            drop(reader);
            drop(write_half);
        });

        client_stream.write_all(b"{\"type\":\"hello\"}\n").await?;
        let mut echo = [0u8; 17];
        client_stream.read_exact(&mut echo).await?;
        assert_eq!(&echo, b"{\"type\":\"hello\"}\n");

        // Send an oversized line (64 KiB + 1 byte) with no newline; server must reject and close.
        let oversized = vec![b'z'; MAX_BRIDGE_LINE_BYTES + 1];
        let _ = client_stream.write_all(&oversized).await;

        server_task.await?;

        // Confirm the connection is actually closed from the client's perspective (EOF or error).
        let mut probe = [0u8; 16];
        let closed = matches!(client_stream.read(&mut probe).await, Ok(0) | Err(_));
        assert!(closed, "socket must be closed after oversized frame");
        Ok(())
    }
}
