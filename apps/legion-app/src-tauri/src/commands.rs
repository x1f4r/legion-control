//! Tauri command surface backed by [`Backend`].

#[cfg(debug_assertions)]
use std::fs;
use std::io::Write;
#[cfg(debug_assertions)]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
#[cfg(not(target_os = "ios"))]
use tauri::Manager;
use tauri::State;
use tauri::ipc::Channel;

use crate::audio::SharedAudio;
use crate::backend::{
    AppError, AppState, Backend, CallId, CallRequest, CancelResult, ErrorCode, LearnResult,
    RepairResult, UnpairResult,
};
use crate::content::{ContentUrlResult, ContentUrlStatus, SharedContentServer};
#[cfg(all(debug_assertions, feature = "test-upload-paths"))]
use crate::transfers::UploadFileInput;
use crate::transfers::{
    DeviceTransfer, OpenExternallyResult, OpenTransferResult, PickedUploadHandle, SharedTransfers,
    TransferControlAction, TransferIdResult, TransferSettings, UploadEnqueueResult,
};
#[cfg(target_os = "android")]
use crate::updater::android::AndroidUpdaterHandle;
use crate::updater::{SharedUpdater, UpdateState};

/// Log line emitted when the web interface finishes its first paint.
pub const INTERFACE_READY_MARKER: &str = "legion-app: interface ready";

/// Optional environment variable naming a file to write [`INTERFACE_READY_MARKER`] to on ready.
/// Compiled ONLY in debug builds so release binaries ignore environment switches and never write
/// arbitrary paths.
#[cfg(debug_assertions)]
pub const READY_FILE_ENV: &str = "LEGION_APP_READY_FILE";

/// Optional environment variable naming a JSON spec file for the CI content-media smoke check.
/// Compiled ONLY in debug builds so release binaries ship with zero test hooks.
#[cfg(debug_assertions)]
pub const MEDIA_CHECK_FILE_ENV: &str = "LEGION_APP_MEDIA_CHECK_FILE";

/// Log line emitted when the opt-in CI content-media smoke check decodes both image and video.
#[cfg(debug_assertions)]
pub const MEDIA_CHECK_OK_MARKER: &str = "legion-app: media check ok";

/// Log line emitted when the opt-in CI content-media smoke check fails.
#[cfg(debug_assertions)]
pub const MEDIA_CHECK_FAILED_MARKER: &str = "legion-app: media check failed";

/// Maximum accepted pairing invite length at the command seam (8 KiB).
pub const MAX_INVITE_BYTES: usize = 8 * 1024;

/// Maximum accepted device or node label length at the command seam (256 bytes).
pub const MAX_LABEL_BYTES: usize = 256;

fn validate_invite_size(invite: &str) -> Result<(), AppError> {
    if invite.len() > MAX_INVITE_BYTES {
        return Err(AppError::new(
            ErrorCode::InvalidArgs,
            format!("Pairing invite exceeds the {MAX_INVITE_BYTES}-byte limit."),
        )
        .with_hint("Paste the single invite line printed by `legion pair`."));
    }
    Ok(())
}

fn validate_label_size(field: &str, value: &str) -> Result<(), AppError> {
    if value.len() > MAX_LABEL_BYTES {
        return Err(AppError::new(
            ErrorCode::InvalidArgs,
            format!("{field} exceeds the {MAX_LABEL_BYTES}-byte limit."),
        ));
    }
    Ok(())
}

static READY_LOGGED: AtomicBool = AtomicBool::new(false);

/// Result returned by `scan_invite`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ScanInviteResult {
    /// A QR code was scanned directly by the native command (Android).
    Scanned {
        /// Trimmed QR code payload.
        content: String,
    },
    /// The caller should invoke the registered `barcode-scanner` plugin directly (iOS).
    ///
    /// `tauri-plugin-barcode-scanner` 2.5.3 keeps its iOS `PluginHandle` private and registering
    /// a second iOS plugin handle requires `unsafe` FFI (`tauri::ios_plugin_binding!`), which this
    /// crate forbids. The plugin is registered and permitted in `capabilities/mobile.json` so the
    /// web view can call `plugin:barcode-scanner|scan` directly.
    UsePlugin {
        /// Registered Tauri plugin name (`"barcode-scanner"`).
        plugin: String,
        /// Barcode formats to pass to `plugin:barcode-scanner|scan`.
        formats: Vec<String>,
        /// Preferred camera direction (`"back"`).
        camera_direction: String,
        /// Whether the scanner should use a transparent webview overlay (`false`).
        windowed: bool,
    },
}

/// Shared backend state registered with Tauri.
pub struct SharedBackend(pub Arc<dyn Backend>);

/// Native desktop event emitted to the web view when the window manager exits full screen
/// outside Legion's own `window_set_immersive(false)` call.
pub const EVENT_IMMERSIVE_CHANGED: &str = "legion://immersive-changed";

/// Native desktop event emitted to the web view when the main window is hidden (for example while
/// background transfers drain) or closed, so active playback pauses where it is and leaves full
/// screen.
pub const EVENT_WINDOW_HIDDEN: &str = "legion://window-hidden";

/// Tracks whether Legion entered desktop full screen so leaving only undoes what Legion did.
///
/// If the window was already full screen when immersive started, entering changes nothing and
/// leaving changes nothing. If the window leaves full screen by any other hand, immersive follows
/// and never forces full screen back on.
///
/// On Linux, `tao`'s `window.is_fullscreen()` only caches Legion's own `set_fullscreen` calls and
/// misses window-manager toggles (Hyprland, GNOME, etc.). `observed_window_fullscreen` tracks the
/// real GTK `window-state-event` (`GDK_WINDOW_STATE_FULLSCREEN`) bit as the source of truth.
#[derive(Default)]
pub struct DesktopImmersiveState {
    inner: Mutex<DesktopImmersiveInner>,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct DesktopImmersiveInner {
    active: bool,
    entered_by_legion: bool,
    observed_window_fullscreen: Option<bool>,
}

impl DesktopImmersiveState {
    /// Transitions into immersive mode. Returns `true` only when the window was not already full
    /// screen and Legion should request full screen on the window.
    pub fn enter(&self, currently_fullscreen: bool) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.active {
            guard.active = true;
            guard.entered_by_legion = !currently_fullscreen;
        }
        !currently_fullscreen
    }

    /// Leaves immersive mode. Returns `true` only when Legion was the one that entered full
    /// screen and should now call `set_fullscreen(false)`. Never requests `set_fullscreen(true)`.
    pub fn leave(&self) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let should_exit = guard.active && guard.entered_by_legion;
        guard.active = false;
        guard.entered_by_legion = false;
        should_exit
    }

    /// Reconciles with the window's observed full-screen state after an external window-manager
    /// transition. Returns `true` if immersive mode was active and just ended externally.
    pub fn sync_external_fullscreen(&self, window_fullscreen: bool) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.observed_window_fullscreen = Some(window_fullscreen);
        if guard.active && !window_fullscreen {
            guard.active = false;
            guard.entered_by_legion = false;
            return true;
        }
        false
    }

    /// Seeds the observed native window full-screen state if no state event has been recorded yet.
    pub fn set_initial_observed_fullscreen(&self, is_fullscreen: bool) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.observed_window_fullscreen.is_none() {
            guard.observed_window_fullscreen = Some(is_fullscreen);
        }
    }

    /// Records a GTK `window-state-event` update (`is_fullscreen` from `new_window_state`,
    /// `fullscreen_changed` from `changed_mask`). Returns `true` if immersive mode was active and
    /// the window manager just took the window out of full screen.
    pub fn record_window_state_fullscreen(
        &self,
        is_fullscreen: bool,
        fullscreen_changed: bool,
    ) -> bool {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if fullscreen_changed {
            guard.observed_window_fullscreen = Some(is_fullscreen);
            if guard.active && !is_fullscreen {
                guard.active = false;
                guard.entered_by_legion = false;
                return true;
            }
        } else if guard.observed_window_fullscreen.is_none() || !guard.active {
            guard.observed_window_fullscreen = Some(is_fullscreen);
        }
        false
    }

    /// Returns the last observed native window full-screen bit, if known.
    pub fn observed_fullscreen(&self) -> Option<bool> {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.observed_window_fullscreen
    }

    /// Returns whether immersive mode is currently active.
    pub fn is_active(&self) -> bool {
        let guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.active
    }
}

/// Reads the real OS/window-manager full-screen state of the main desktop window.
///
/// On Linux this reads the GTK `window-state-event` state tracked in [`DesktopImmersiveState`]
/// rather than `tao`'s cached `window.is_fullscreen()`, which ignores window-manager toggles.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn read_desktop_fullscreen(
    window: &tauri::WebviewWindow,
    state: &DesktopImmersiveState,
) -> Result<bool, AppError> {
    #[cfg(target_os = "linux")]
    {
        let _ = window;
        Ok(state.observed_fullscreen().unwrap_or(false))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = state;
        window.is_fullscreen().map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not read window full-screen state: {err}"),
            )
        })
    }
}

/// Result returned by [`window_set_immersive`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImmersiveResult {
    /// Whether immersive mode is active after this call.
    pub active: bool,
    /// Whether the window is full screen after this call.
    pub fullscreen: bool,
}

/// Result returned on mobile targets where desktop window full-screen is a no-op; a no-op must
/// never claim `active: true`.
pub fn mobile_noop_immersive_result() -> ImmersiveResult {
    ImmersiveResult {
        active: false,
        fullscreen: false,
    }
}

pub use crate::keep_screen_on::DesktopKeepScreenOn;

/// Resets desktop immersive state when the page reloads, crashes, or the window closes/hides.
/// Only exits full screen if Legion itself entered it.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn reset_desktop_immersive(
    window: Option<&tauri::WebviewWindow>,
    state: &DesktopImmersiveState,
) {
    if state.leave()
        && let Some(win) = window
    {
        let _ = win.set_fullscreen(false);
    }
}

/// Enters or leaves immersive mode on the main desktop window. Legion only undoes what Legion
/// did: if the window was already full screen, entering and leaving change nothing.
#[tauri::command]
pub fn window_set_immersive(
    window: tauri::WebviewWindow,
    state: State<'_, DesktopImmersiveState>,
    on: bool,
) -> Result<ImmersiveResult, AppError> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if on {
            let currently_fullscreen = read_desktop_fullscreen(&window, &state)?;
            let should_request = state.enter(currently_fullscreen);
            if should_request {
                window.set_fullscreen(true).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not enter full screen: {err}"),
                    )
                })?;
            }
            let fullscreen = if should_request {
                #[cfg(target_os = "linux")]
                {
                    true
                }
                #[cfg(not(target_os = "linux"))]
                {
                    window.is_fullscreen().unwrap_or(true)
                }
            } else {
                currently_fullscreen
            };
            Ok(ImmersiveResult {
                active: true,
                fullscreen,
            })
        } else {
            let should_exit = state.leave();
            if should_exit {
                window.set_fullscreen(false).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not leave full screen: {err}"),
                    )
                })?;
            }
            let fullscreen = if should_exit {
                #[cfg(target_os = "linux")]
                {
                    false
                }
                #[cfg(not(target_os = "linux"))]
                {
                    window.is_fullscreen().unwrap_or(false)
                }
            } else {
                read_desktop_fullscreen(&window, &state).unwrap_or(false)
            };
            Ok(ImmersiveResult {
                active: false,
                fullscreen,
            })
        }
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (window, state, on);
        Ok(mobile_noop_immersive_result())
    }
}

/// Reports whether the main desktop window is currently full screen.
#[tauri::command]
pub fn window_is_fullscreen(
    window: tauri::WebviewWindow,
    state: State<'_, DesktopImmersiveState>,
) -> Result<bool, AppError> {
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        read_desktop_fullscreen(&window, &state)
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (window, state);
        Ok(false)
    }
}

/// Enables or releases the desktop keep-screen-on hold during video playback, returning whether a
/// real OS screen-awake hold is active afterward.
#[tauri::command]
pub fn set_keep_screen_on(
    keep: State<'_, DesktopKeepScreenOn>,
    on: bool,
) -> Result<bool, AppError> {
    Ok(keep.set(on))
}

/// Debug/test-only helper to flip the main desktop window's real OS/WM full-screen state directly
/// (on Linux via the underlying GTK window rather than `tao::Window::set_fullscreen`, so `tao`'s
/// internal cache stays untouched just like a window-manager key press). Compiled ONLY in debug
/// builds.
#[cfg(debug_assertions)]
#[tauri::command]
pub fn debug_set_window_fullscreen(
    window: tauri::WebviewWindow,
    state: State<'_, DesktopImmersiveState>,
    fullscreen: bool,
) -> Result<bool, AppError> {
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::GtkWindowExt;

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let win_for_main = window.clone();
        window
            .run_on_main_thread(move || {
                let res = win_for_main.gtk_window().map(|gtk_win| {
                    if fullscreen {
                        gtk_win.fullscreen();
                    } else {
                        gtk_win.unfullscreen();
                    }
                });
                let _ = tx.send(res);
            })
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not schedule GTK full-screen change: {err}"),
                )
            })?;
        rx.recv()
            .map_err(|_| {
                AppError::new(
                    ErrorCode::Failed,
                    "GTK main thread closed before applying full-screen change.",
                )
            })?
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not set GTK window full screen: {err}"),
                )
            })?;
        Ok(read_desktop_fullscreen(&window, &state).unwrap_or(fullscreen))
    }
    #[cfg(all(
        not(target_os = "linux"),
        not(any(target_os = "android", target_os = "ios"))
    ))]
    {
        let _ = state;
        window.set_fullscreen(fullscreen).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not set window full screen: {err}"),
            )
        })?;
        Ok(window.is_fullscreen().unwrap_or(fullscreen))
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _ = (window, state);
        Ok(fullscreen)
    }
}

/// Managed handle to the mobile barcode scanner plugin on Android.
#[cfg(target_os = "android")]
pub struct ScannerHandle(pub tauri::plugin::PluginHandle<tauri::Wry>);

/// Returns the current controller readiness, known nodes, and whether pairing is required.
#[tauri::command]
pub async fn state(backend: State<'_, SharedBackend>) -> Result<AppState, AppError> {
    backend.0.state().await
}

/// Lists the tools offered on `on`, or on the default node when omitted.
#[tauri::command]
pub async fn tools(
    backend: State<'_, SharedBackend>,
    on: Option<Value>,
) -> Result<Vec<Value>, AppError> {
    backend.0.tools(on).await
}

/// Invokes a Legion tool and streams live events to `on_event`.
#[tauri::command]
pub async fn call(
    backend: State<'_, SharedBackend>,
    id: Option<CallId>,
    tool: String,
    args: Option<Value>,
    on: Option<Value>,
    on_event: Channel<Value>,
) -> Result<Value, AppError> {
    let request = CallRequest {
        id,
        tool,
        args: args.unwrap_or_else(|| Value::Object(Map::new())),
        on,
    };
    backend.0.call(request, Some(on_event)).await
}

/// Cancels an in-flight tool call by identifier.
#[tauri::command]
pub async fn cancel(
    backend: State<'_, SharedBackend>,
    id: CallId,
) -> Result<CancelResult, AppError> {
    backend.0.cancel(id).await
}

/// Pairs this device with a node using a pairing invite and optional device label.
#[tauri::command]
pub async fn pair(
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    invite: String,
    name: Option<String>,
) -> Result<AppState, AppError> {
    validate_invite_size(&invite)?;
    if let Some(raw_name) = &name {
        validate_label_size("Device name", raw_name)?;
    }
    let resolved_name = name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_device_name);
    validate_label_size("Device name", &resolved_name)?;
    let state = backend.0.pair(invite, resolved_name).await?;
    content.0.revoke_stale_pairings();
    Ok(state)
}

/// Restores an existing pairing on the node named in `invite` by probing with this device's saved
/// key against the invite's pinned host keys and verifying `existing_principal`.
#[tauri::command]
pub async fn restore_pairing(
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    invite: String,
    existing_principal: String,
    name: Option<String>,
) -> Result<AppState, AppError> {
    validate_invite_size(&invite)?;
    validate_label_size("Principal", &existing_principal)?;
    if let Some(raw_name) = &name {
        validate_label_size("Device name", raw_name)?;
    }
    let resolved_name = name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_device_name);
    validate_label_size("Device name", &resolved_name)?;
    let state = backend
        .0
        .restore_pairing(invite, existing_principal, resolved_name)
        .await?;
    content.0.revoke_stale_pairings();
    Ok(state)
}

/// Cancels an in-flight device pairing operation.
#[tauri::command]
pub async fn cancel_pair(backend: State<'_, SharedBackend>) -> Result<CancelResult, AppError> {
    backend.0.cancel_pair().await
}

/// Removes a paired node from this device and revokes any content tickets issued under it.
#[tauri::command]
pub async fn unpair(
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    audio: State<'_, SharedAudio>,
    node: String,
) -> Result<UnpairResult, AppError> {
    validate_label_size("Node name", &node)?;
    audio.0.on_machine_unpaired(&node).await;
    let result = backend.0.unpair(node).await?;
    content.0.revoke_stale_pairings();
    Ok(result)
}

/// Switches which configured node calls are sent through by default.
#[tauri::command]
pub async fn select_node(
    backend: State<'_, SharedBackend>,
    node: String,
) -> Result<AppState, AppError> {
    validate_label_size("Node name", &node)?;
    backend.0.select_node(node).await
}

/// Moves a damaged or unsupported state file aside (`legion-app.json.damaged-<timestamp>`) and
/// resets the backend to a clean state.
#[tauri::command]
pub async fn repair_store(
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    audio: State<'_, SharedAudio>,
) -> Result<RepairResult, AppError> {
    let _ = audio.0.dismiss().await;
    let result = backend.0.repair_store().await?;
    content.0.revoke_all();
    Ok(result)
}

/// Closes cached sessions on mobile platforms after resuming from background or sleep so the next
/// refresh opens fresh sockets instead of stalling on half-open TCP connections, and probes the
/// loopback content listener in case the OS reclaimed it while backgrounded.
#[tauri::command]
pub async fn invalidate_sessions(
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    updater: State<'_, SharedUpdater>,
) -> Result<(), AppError> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let _resume_guard = updater.0.track_external_operation();
        content.0.on_app_resumed();
        backend.0.invalidate_sessions().await
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = (backend, content, updater);
        Ok(())
    }
}

/// Queries `device.endpoint` across the fleet through an answering node, updates learned direct
/// routes in persistent storage, and returns the learn report plus updated state.
#[tauri::command]
pub async fn learn(backend: State<'_, SharedBackend>) -> Result<LearnResult, AppError> {
    backend.0.learn().await
}

/// Explicitly accepts a pending host-key or endpoint change reported for learned node `node`,
/// requiring `fingerprints` to match the reviewed host keys exactly and persisting before updating
/// live state.
#[tauri::command]
pub async fn accept_identity_change(
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    node: String,
    fingerprints: Vec<String>,
) -> Result<AppState, AppError> {
    validate_label_size("Node name", &node)?;
    if fingerprints.len() > 32 {
        return Err(AppError::new(
            ErrorCode::InvalidArgs,
            "Too many fingerprints supplied.",
        ));
    }
    for fp in &fingerprints {
        validate_label_size("Fingerprint", fp)?;
    }
    let state = backend.0.accept_identity_change(node, fingerprints).await?;
    content.0.revoke_stale_pairings();
    Ok(state)
}

/// Mints a short-lived ticketed content address (`{ url, expires_in }`) for viewing or streaming
/// `path` on `machine` through `legion-content`.
#[tauri::command]
pub async fn content_url(
    content: State<'_, SharedContentServer>,
    machine: String,
    path: String,
    download: Option<bool>,
) -> Result<ContentUrlResult, AppError> {
    validate_label_size("Machine name", &machine)?;
    content
        .0
        .mint_url(&machine, &path, download.unwrap_or(false))
        .await
}

/// Returns the observed HTTP status, content type, mid-stream failure state, and ticket liveness
/// for a previously minted content URL so the viewer never misreports transport/HTTP errors as
/// media decode failures.
#[tauri::command]
pub fn content_status(content: State<'_, SharedContentServer>, url: String) -> ContentUrlStatus {
    content.0.content_status_for_url(&url)
}

/// Immediately revokes a previously minted content URL when the viewer closes, switches files, or
/// tears down a media element. Returns `true` if a live ticket was removed.
#[tauri::command]
pub fn content_revoke(content: State<'_, SharedContentServer>, url: String) -> bool {
    content.0.revoke_url(&url)
}

/// Loads a music folder queue into the native Android background playback service.
#[tauri::command]
pub async fn audio_load_queue(
    audio: State<'_, SharedAudio>,
    payload: Value,
) -> Result<Value, AppError> {
    audio.0.load_queue(payload).await
}

/// Sends a transport command (`play`, `pause`, `toggle`, `next`, `prev`, `seek`, `speed`) to the
/// native Android background playback service.
#[tauri::command]
pub async fn audio_command(
    audio: State<'_, SharedAudio>,
    action: String,
    payload: Option<Value>,
) -> Result<Value, AppError> {
    validate_label_size("Audio command", &action)?;
    audio
        .0
        .command(
            &action,
            payload.unwrap_or_else(|| Value::Object(Map::new())),
        )
        .await
}

/// Updates shuffle/repeat modes and playback order in the native Android background service.
#[tauri::command]
pub async fn audio_set_modes(
    audio: State<'_, SharedAudio>,
    payload: Value,
) -> Result<Value, AppError> {
    audio.0.set_modes(payload).await
}

/// Dismisses the native Android background music player, releasing ExoPlayer and the media ticket.
#[tauri::command]
pub async fn audio_dismiss(audio: State<'_, SharedAudio>) -> Result<Value, AppError> {
    audio.0.dismiss().await
}

/// Returns the latest native Android background playback state snapshot.
#[tauri::command]
pub fn audio_state(audio: State<'_, SharedAudio>) -> Value {
    audio.0.state()
}

/// Requests Android 13+ notification permission once when music starts for the first time.
#[tauri::command]
pub async fn audio_request_notification_permission(
    audio: State<'_, SharedAudio>,
) -> Result<bool, AppError> {
    Ok(audio.0.request_notification_permission().await)
}

/// Debug/test-only readout of the native Android music service's 1-second position and event log.
#[cfg(debug_assertions)]
#[tauri::command]
pub async fn audio_debug_trace(audio: State<'_, SharedAudio>) -> Result<Value, AppError> {
    audio.0.debug_trace().await
}

/// Enqueues a non-blocking download from `machine:path` into the device's Downloads folder.
#[tauri::command]
pub async fn transfer_download(
    transfers: State<'_, SharedTransfers>,
    machine: String,
    path: String,
    name: Option<String>,
    size: Option<u64>,
    mime: Option<String>,
) -> Result<TransferIdResult, AppError> {
    validate_label_size("Machine name", &machine)?;
    if let Some(ref raw_name) = name {
        validate_label_size("File name", raw_name)?;
    }
    let id = transfers
        .0
        .enqueue_download(machine, path, name, size, mime, false)
        .await?;
    Ok(TransferIdResult { id })
}

/// Opens the device's native file picker and returns session-only opaque handles for the chosen
/// files. The web view never receives or supplies device filesystem paths or URIs.
#[tauri::command]
pub async fn transfer_pick_uploads(
    transfers: State<'_, SharedTransfers>,
) -> Result<Vec<PickedUploadHandle>, AppError> {
    transfers.0.pick_uploads().await
}

/// Picks local files via the native picker (unless opaque `handles` from [`transfer_pick_uploads`]
/// are supplied) and enqueues non-blocking bulk-pipe `put` uploads into `machine:dir`. Returns
/// `None` if the user cancelled the file picker.
#[tauri::command]
pub async fn transfer_upload(
    transfers: State<'_, SharedTransfers>,
    machine: String,
    dir: String,
    handles: Option<Vec<String>>,
) -> Result<Option<UploadEnqueueResult>, AppError> {
    validate_label_size("Machine name", &machine)?;
    if let Some(ref list) = handles {
        if list.len() > 128 {
            return Err(AppError::new(
                ErrorCode::InvalidArgs,
                "Too many upload handles supplied.",
            ));
        }
        for h in list {
            validate_label_size("Upload handle", h)?;
        }
    }
    transfers.0.enqueue_upload(machine, dir, handles).await
}

/// Debug/test-only upload command accepting pre-selected local file paths. Compiled ONLY when
/// BOTH `debug_assertions` AND the `test-upload-paths` Cargo feature are enabled.
#[cfg(all(debug_assertions, feature = "test-upload-paths"))]
#[tauri::command]
pub async fn transfer_upload_test_files(
    transfers: State<'_, SharedTransfers>,
    machine: String,
    dir: String,
    files: Vec<UploadFileInput>,
) -> Result<Option<UploadEnqueueResult>, AppError> {
    validate_label_size("Machine name", &machine)?;
    transfers
        .0
        .enqueue_upload_paths_for_test(machine, dir, files)
        .await
}

/// Opens `machine:path` in another app: opens an unchanged cached copy immediately when present,
/// or enqueues a resumable cache download that launches the system chooser once verified.
#[tauri::command]
pub async fn transfer_open_externally(
    transfers: State<'_, SharedTransfers>,
    machine: String,
    path: String,
    name: Option<String>,
    size: Option<u64>,
    mime: Option<String>,
) -> Result<OpenExternallyResult, AppError> {
    validate_label_size("Machine name", &machine)?;
    if let Some(ref raw_name) = name {
        validate_label_size("File name", raw_name)?;
    }
    transfers
        .0
        .open_externally(machine, path, name, size, mime)
        .await
}

/// Lists all current and recent device transfers (newest first).
#[tauri::command]
pub fn transfers(transfers: State<'_, SharedTransfers>) -> Vec<DeviceTransfer> {
    transfers.0.list()
}

/// Pauses, resumes, or cancels one device transfer by id.
#[tauri::command]
pub async fn transfer_control(
    transfers: State<'_, SharedTransfers>,
    id: String,
    action: TransferControlAction,
) -> Result<DeviceTransfer, AppError> {
    validate_label_size("Transfer id", &id)?;
    transfers.0.control(&id, action).await
}

/// Opens a finished downloaded file using the platform's default viewer or chooser.
#[tauri::command]
pub async fn open_transfer(
    transfers: State<'_, SharedTransfers>,
    id: String,
) -> Result<OpenTransferResult, AppError> {
    validate_label_size("Transfer id", &id)?;
    transfers.0.open_transfer(&id).await
}

/// Reveals a finished downloaded file in the device's file manager / Downloads folder.
#[tauri::command]
pub async fn show_transfer_in_folder(
    transfers: State<'_, SharedTransfers>,
    id: String,
) -> Result<OpenTransferResult, AppError> {
    validate_label_size("Transfer id", &id)?;
    transfers.0.show_transfer_in_folder(&id).await
}

/// Subscribes `on_event` to throttled live snapshots of the transfer queue and returns the
/// watcher's opaque subscription id for removal via [`transfer_unwatch`].
#[tauri::command]
pub fn transfer_watch(
    transfers: State<'_, SharedTransfers>,
    on_event: Channel<Vec<DeviceTransfer>>,
) -> Result<u64, AppError> {
    Ok(transfers.0.watch(on_event))
}

/// Removes a previously registered progress watcher by its subscription id.
#[tauri::command]
pub fn transfer_unwatch(
    transfers: State<'_, SharedTransfers>,
    watcher_id: u64,
) -> Result<(), AppError> {
    transfers.0.unwatch(watcher_id);
    Ok(())
}

/// Returns the current transfer queue settings (`wifi_only_large`, `large_threshold_bytes`).
#[tauri::command]
pub fn transfer_settings(transfers: State<'_, SharedTransfers>) -> TransferSettings {
    transfers.0.settings()
}

/// Updates the "large transfers on Wi-Fi only" setting.
#[tauri::command]
pub fn transfer_set_settings(
    transfers: State<'_, SharedTransfers>,
    wifi_only_large: bool,
) -> TransferSettings {
    transfers.0.set_wifi_only_large(wifi_only_large)
}

/// Dismisses a pending identity-change warning for `node` without altering its pinned target.
#[tauri::command]
pub async fn dismiss_identity_change(
    backend: State<'_, SharedBackend>,
    node: String,
) -> Result<AppState, AppError> {
    validate_label_size("Node name", &node)?;
    backend.0.dismiss_identity_change(node).await
}

/// Returns a sensible default name for this device (model on mobile, host name on desktop).
#[tauri::command]
pub fn device_name() -> String {
    default_device_name()
}

/// Host-provided specification for the opt-in debug CI content-media smoke check
/// (`LEGION_APP_MEDIA_CHECK_FILE`). Compiled ONLY in debug builds.
#[cfg(debug_assertions)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaCheckSpec {
    /// Target machine name (e.g. `"tower"`).
    pub machine: String,
    /// Optional single-use pairing invite for mobile targets without a local `legion` binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
    /// Remote path to a test image file (e.g. PNG).
    pub image_path: String,
    /// Remote path to a short WebM test video file.
    pub video_path: String,
    /// Optional timestamp (seconds) to seek the video to after initial metadata loads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seek_to: Option<f64>,
    /// Optional remote HTML path to verify content-origin IPC isolation (framed & top-level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_path: Option<String>,
    /// Optional remote SVG path to verify content-origin IPC isolation (framed & top-level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svg_path: Option<String>,
    /// Optional flag asking the page to also re-verify media decode after an iOS resume event.
    #[serde(default)]
    pub verify_resume: bool,
    /// Optional flag asking the page to verify desktop window immersive full-screen transitions.
    #[serde(default)]
    pub verify_immersive: bool,
    /// Optional host path where [`media_check_report`] writes the JSON result. Never read from JS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_file: Option<String>,
}

/// Public subset of [`MediaCheckSpec`] exposed to the web view (omits host `out_file` path).
#[cfg(debug_assertions)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublicMediaCheckSpec {
    /// Target machine name.
    pub machine: String,
    /// Optional single-use pairing invite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite: Option<String>,
    /// Remote path to a test image file.
    pub image_path: String,
    /// Remote path to a short WebM test video file.
    pub video_path: String,
    /// Optional timestamp (seconds) to seek the video to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seek_to: Option<f64>,
    /// Optional remote HTML path for IPC isolation verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_path: Option<String>,
    /// Optional remote SVG path for IPC isolation verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub svg_path: Option<String>,
    /// Whether to also wait for an app resume and re-verify media loading.
    #[serde(default)]
    pub verify_resume: bool,
    /// Whether to verify desktop window immersive full-screen transitions.
    #[serde(default)]
    pub verify_immersive: bool,
}

/// Decoded media verification report submitted by the web view after loading content URLs.
#[cfg(debug_assertions)]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaCheckReport {
    /// Current lifecycle stage (`"pairing"`, `"media"`, `"resume"`, or `"done"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    /// True once pairing completed (or was not required); false while pairing is in progress or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_ok: Option<bool>,
    /// True when both the image and video decoded with non-zero dimensions.
    pub ok: bool,
    /// Decoded image natural width in pixels.
    pub image_width: u32,
    /// Decoded image natural height in pixels.
    pub image_height: u32,
    /// Decoded video width in pixels.
    pub video_width: u32,
    /// Decoded video height in pixels.
    pub video_height: u32,
    /// Decoded video duration in seconds (0 when unavailable).
    #[serde(default)]
    pub video_duration: f64,
    /// Observed `video.currentTime` after seeking when `seek_to` was requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seeked_time: Option<f64>,
    /// Origin/scheme prefix of the minted image content URL (ticket redacted).
    #[serde(default)]
    pub image_origin: String,
    /// Origin/scheme prefix of the minted video content URL (ticket redacted).
    #[serde(default)]
    pub video_origin: String,
    /// Result of HTML/SVG content-origin IPC isolation check when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc_isolation_ok: Option<bool>,
    /// Result of post-resume loopback media reload check when requested (iOS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_ok: Option<bool>,
    /// Result of desktop window immersive full-screen transition check when requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immersive_check: Option<Value>,
    /// Lifecycle trigger that fired the post-resume media check (e.g. `tauri://resumed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_trigger: Option<String>,
    /// Failure description when `ok` is false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(debug_assertions)]
fn load_media_check_spec(app: &tauri::AppHandle) -> Result<Option<MediaCheckSpec>, AppError> {
    let spec_path = if let Some(p) = std::env::var_os(MEDIA_CHECK_FILE_ENV)
        .filter(|val| !val.is_empty())
        .map(std::path::PathBuf::from)
    {
        Some(p)
    } else {
        #[cfg(target_os = "android")]
        {
            use tauri::Manager as _;
            app.path()
                .app_data_dir()
                .ok()
                .map(|d| d.join("media-check.json"))
                .filter(|p| p.is_file())
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = app;
            None
        }
    };

    let Some(path) = spec_path else {
        return Ok(None);
    };
    let raw = fs::read_to_string(&path).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not read media check spec {}: {err}", path.display()),
        )
    })?;
    let spec: MediaCheckSpec = serde_json::from_str(&raw).map_err(|err| {
        AppError::new(
            ErrorCode::InvalidArgs,
            format!("Invalid media check spec {}: {err}", path.display()),
        )
    })?;
    Ok(Some(spec))
}

/// Returns the active opt-in debug CI content-media smoke check specification, or `None` in
/// normal runs. Compiled ONLY in debug builds.
#[cfg(debug_assertions)]
#[tauri::command]
pub fn media_check_spec(app: tauri::AppHandle) -> Result<Option<PublicMediaCheckSpec>, AppError> {
    let Some(spec) = load_media_check_spec(&app)? else {
        return Ok(None);
    };
    Ok(Some(PublicMediaCheckSpec {
        machine: spec.machine,
        invite: spec.invite,
        image_path: spec.image_path,
        video_path: spec.video_path,
        seek_to: spec.seek_to,
        html_path: spec.html_path,
        svg_path: spec.svg_path,
        verify_resume: spec.verify_resume,
        verify_immersive: spec.verify_immersive,
    }))
}

/// Records the result of the opt-in debug CI content-media smoke check (logs marker + writes
/// configured `out_file` if any). Refuses calls when no host media-check spec is active.
/// Compiled ONLY in debug builds.
#[cfg(debug_assertions)]
#[tauri::command]
pub fn media_check_report(app: tauri::AppHandle, report: MediaCheckReport) -> Result<(), AppError> {
    let Some(spec) = load_media_check_spec(&app)? else {
        return Ok(());
    };
    let summary = format!(
        "img={}x{} vid={}x{} dur={:.2}s origin={}",
        report.image_width,
        report.image_height,
        report.video_width,
        report.video_height,
        report.video_duration,
        report.image_origin
    );
    if report.ok {
        let line = format!("{MEDIA_CHECK_OK_MARKER} ({summary})");
        println!("{line}");
        let _ = std::io::stdout().flush();
        #[cfg(target_os = "android")]
        log::info!(target: "legion-app", "{line}");
    } else if let Some(reason) = report.error.as_deref() {
        let stage = report.stage.as_deref().unwrap_or("unknown");
        let line =
            format!("{MEDIA_CHECK_FAILED_MARKER} (stage={stage}; {summary}; error={reason})");
        eprintln!("{line}");
        let _ = std::io::stderr().flush();
        #[cfg(target_os = "android")]
        log::error!(target: "legion-app", "{line}");
    } else {
        let stage = report.stage.as_deref().unwrap_or("progress");
        let pair_ok = report.pair_ok.unwrap_or(false);
        let line = format!("legion-app: media check progress (stage={stage}; pair_ok={pair_ok})");
        println!("{line}");
        let _ = std::io::stdout().flush();
        #[cfg(target_os = "android")]
        log::info!(target: "legion-app", "{line}");
    }

    if let Some(out_file) = spec.out_file.as_deref().filter(|s| !s.trim().is_empty()) {
        let out_path = Path::new(out_file);
        if let Some(parent) = out_path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|err| {
                AppError::new(
                    ErrorCode::Internal,
                    format!("Could not create media check report directory: {err}"),
                )
            })?;
        }
        let encoded = serde_json::to_string_pretty(&report).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not encode media check report: {err}"),
            )
        })?;
        let tmp_name = format!(
            ".{}.tmp-{}",
            out_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("media-result.json"),
            std::process::id()
        );
        let tmp_path = out_path.with_file_name(tmp_name);
        fs::write(&tmp_path, format!("{encoded}\n")).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not write media check report: {err}"),
            )
        })?;
        #[cfg(windows)]
        let _ = fs::remove_file(out_path);
        fs::rename(&tmp_path, out_path).map_err(|err| {
            let _ = fs::remove_file(&tmp_path);
            AppError::new(
                ErrorCode::Internal,
                format!("Could not finalize media check report: {err}"),
            )
        })?;
    }

    Ok(())
}

/// Signals that the web interface has mounted and painted its initial view.
///
/// Logs [`INTERFACE_READY_MARKER`] once to stdout (and logcat on Android). In debug builds only,
/// when `LEGION_APP_READY_FILE` is set, also writes the marker line to that path.
#[tauri::command]
pub fn ready(updater: State<'_, SharedUpdater>) -> Result<(), AppError> {
    if !READY_LOGGED.swap(true, Ordering::SeqCst) {
        let _ = writeln!(std::io::stdout(), "{INTERFACE_READY_MARKER}");
        let _ = std::io::stdout().flush();
        #[cfg(target_os = "android")]
        log::info!(target: "legion-app", "{INTERFACE_READY_MARKER}");
        updater.0.on_app_ready();
    }

    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os(READY_FILE_ENV).filter(|val| !val.is_empty()) {
        write_ready_marker(Path::new(&path))?;
    }

    Ok(())
}

/// Returns the current app update state without making network calls.
#[tauri::command]
pub async fn update_state(
    app: tauri::AppHandle,
    updater: State<'_, SharedUpdater>,
) -> Result<UpdateState, AppError> {
    #[cfg(target_os = "android")]
    {
        let android_updater = app.state::<AndroidUpdaterHandle>();
        if let Ok(caps) = crate::updater::android::capabilities(&android_updater).await {
            return Ok(updater.0.set_android_capabilities(caps));
        }
    }
    #[cfg(not(target_os = "android"))]
    let _ = app;
    Ok(updater.0.snapshot())
}

/// Subscribes `on_state` to live updater state changes (and immediately sends the current state).
#[tauri::command]
pub fn update_watch(
    updater: State<'_, SharedUpdater>,
    on_state: Channel<UpdateState>,
) -> Result<(), AppError> {
    updater.0.watch(on_state);
    Ok(())
}

/// Checks every reachable paired machine for a newer verified app update.
#[tauri::command]
pub async fn update_check(
    app: tauri::AppHandle,
    backend: State<'_, SharedBackend>,
    updater: State<'_, SharedUpdater>,
) -> Result<UpdateState, AppError> {
    #[cfg(target_os = "android")]
    let is_metered = {
        let android_updater = app.state::<AndroidUpdaterHandle>();
        if let Ok(caps) = crate::updater::android::capabilities(&android_updater).await {
            let metered = caps.is_metered;
            updater.0.set_android_capabilities(caps);
            metered
        } else {
            false
        }
    };
    #[cfg(not(target_os = "android"))]
    let is_metered = {
        let _ = app;
        false
    };
    updater
        .0
        .check_with_backend(backend.0.as_ref(), false, is_metered)
        .await
}

/// Returns `true` when any user-started native call, device transfer (`queued`, `running`, or
/// `cancelling`), content stream, or tracked external operation (such as camera scanning) is
/// currently active. Passive background refreshes (`sys.info`, `sys.procs`, etc.) are excluded so
/// they do not block updates; installs instead drain them briefly between ticks.
pub(crate) fn has_active_native_work(
    backend: &dyn Backend,
    transfers: &crate::transfers::TransferManager,
    content: &crate::content::AppContentServer,
    updater: &crate::updater::UpdateManager,
) -> bool {
    backend.active_operations() > 0
        || transfers.has_active_work()
        || content.active_streams() > 0
        || updater.external_operations_count() > 0
}

/// Evaluates the native automatic-install gate and waits briefly for any in-flight passive
/// background refreshes to complete so an automatic install runs cleanly between poll ticks
/// without cutting off a refresh mid-call.
///
/// Returns `true` only when the gate permits installation, all in-flight background refreshes
/// finished within `refresh_timeout`, and no user-started work (or first-screen paint for
/// `"startup"`) started while waiting.
pub(crate) async fn evaluate_and_quiesce_auto_install(
    updater: &crate::updater::UpdateManager,
    backend: &dyn Backend,
    transfers: &crate::transfers::TransferManager,
    content: &crate::content::AppContentServer,
    trigger_kind: &str,
    refresh_timeout: Duration,
) -> bool {
    let active_ops = backend
        .active_operations()
        .saturating_add(usize::from(transfers.has_active_work()))
        .saturating_add(content.active_streams());
    if updater
        .evaluate_auto_install_gate(trigger_kind, active_ops, true)
        .is_err()
    {
        return false;
    }
    if !backend.wait_for_background_refreshes(refresh_timeout).await {
        return false;
    }
    if has_active_native_work(backend, transfers, content, updater) {
        return false;
    }
    if trigger_kind == "startup" && updater.is_first_screen_shown() {
        return false;
    }
    true
}

/// Stops in-flight transfers, revokes and cancels active content streams (direct and relayed),
/// and closes cached SSH sessions cleanly before the updater exits the app to swap binaries.
///
/// Mirrors cancellation teardown while preserving unfinished persisted transfers so they resume
/// (or verify already-committed upload destinations) cleanly when the updated app starts.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) async fn quiesce_for_update_exit(
    transfers: &Arc<crate::transfers::TransferManager>,
    content: &Arc<crate::content::AppContentServer>,
    backend: &Arc<dyn Backend>,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    transfers.shutdown();
    content.shutdown();
    if let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        let _ = transfers.wait_for_workers_stopped(remaining).await;
    }
    if let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        let _ = content.wait_for_active_streams_stopped(remaining).await;
    }
    if let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now()) {
        let _ = tokio::time::timeout(remaining, backend.invalidate_sessions()).await;
    }
}

/// Downloads (if needed), re-verifies, and installs the available app update.
///
/// Checks native activity (user-started calls, device transfers including `cancelling`, content
/// streams, and external operations) before replacing the app: on desktop, shows the same native
/// confirmation prompt as quitting with active transfers unless `confirm_interrupt == Some(true)`;
/// on Android, returns [`ErrorCode::Conflict`] so the interface can confirm before retrying with
/// `confirm_interrupt = true`. Waits briefly for any in-flight passive background refresh to finish
/// cleanly before restarting.
#[tauri::command]
pub async fn update_install(
    app: tauri::AppHandle,
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    transfers: State<'_, SharedTransfers>,
    audio: State<'_, SharedAudio>,
    updater: State<'_, SharedUpdater>,
    confirm_interrupt: Option<bool>,
) -> Result<UpdateState, AppError> {
    let confirmed = confirm_interrupt.unwrap_or(false);
    let busy = has_active_native_work(backend.0.as_ref(), &transfers.0, &content.0, &updater.0)
        || audio.0.is_playing();

    #[cfg(target_os = "android")]
    {
        if busy && !confirmed {
            return Err(AppError::new(
                ErrorCode::Conflict,
                "Legion is still transferring files or running a command.",
            )
            .with_hint("Confirm to interrupt active work and install the update."));
        }
        let _ = backend
            .0
            .wait_for_background_refreshes(crate::BACKGROUND_REFRESH_DRAIN_TIMEOUT)
            .await;
        let android_updater = app.state::<AndroidUpdaterHandle>();
        updater
            .0
            .install_android(backend.0.as_ref(), &android_updater, false, false)
            .await
    }
    #[cfg(target_os = "ios")]
    {
        let _ = (app, backend, updater, busy, confirmed);
        Err(AppError::new(
            ErrorCode::Unsupported,
            "Updates on iOS are delivered through TestFlight or the App Store.",
        ))
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        if busy && !confirmed {
            let allow = tokio::task::spawn_blocking(
                crate::transfers::desktop::confirm_update_with_active_work,
            )
            .await
            .unwrap_or(false);
            if !allow {
                return Ok(updater.0.snapshot());
            }
        }
        let _ = backend
            .0
            .wait_for_background_refreshes(crate::BACKGROUND_REFRESH_DRAIN_TIMEOUT)
            .await;
        let (st, should_exit) = updater.0.install_desktop(backend.0.as_ref(), false).await?;
        if should_exit {
            let win = app.get_webview_window("main");
            crate::notify_hidden_and_reset_desktop_player(&app, win.as_ref());
            let handle = app.clone();
            let transfers = Arc::clone(&transfers.0);
            let content = Arc::clone(&content.0);
            let backend = Arc::clone(&backend.0);
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                quiesce_for_update_exit(
                    &transfers,
                    &content,
                    &backend,
                    crate::UPDATE_EXIT_QUIESCE_TIMEOUT,
                )
                .await;
                let min_flush = std::time::Duration::from_millis(150);
                if let Some(remaining) = min_flush.checked_sub(started.elapsed()) {
                    tokio::time::sleep(remaining).await;
                }
                tauri_plugin_single_instance::destroy(&handle);
                handle.exit(0);
            });
        }
        Ok(st)
    }
}

/// Enables or disables automatic updates (download when seen, install on start or leaving the app).
#[tauri::command]
pub fn update_set_auto(
    updater: State<'_, SharedUpdater>,
    enabled: bool,
) -> Result<UpdateState, AppError> {
    updater.0.set_auto_update(enabled)
}

/// Dismisses the one-time "Updated to …" or rollback notice.
#[tauri::command]
pub fn update_dismiss_notice(updater: State<'_, SharedUpdater>) -> Result<UpdateState, AppError> {
    updater.0.dismiss_notice()
}

/// Installs an already-downloaded verified update at one of the two non-interrupting moments
/// (`trigger`: `"startup"` before user interaction, or `"background"` when leaving the app).
/// Never interrupts the user with an unexpected prompt or brings a backgrounded app forward.
#[tauri::command]
pub async fn update_maybe_auto_install(
    app: tauri::AppHandle,
    backend: State<'_, SharedBackend>,
    content: State<'_, SharedContentServer>,
    transfers: State<'_, SharedTransfers>,
    audio: State<'_, SharedAudio>,
    updater: State<'_, SharedUpdater>,
    trigger: Option<String>,
) -> Result<UpdateState, AppError> {
    let trigger_kind = trigger.as_deref().unwrap_or("startup");
    if audio.0.is_playing()
        || !evaluate_and_quiesce_auto_install(
            &updater.0,
            backend.0.as_ref(),
            &transfers.0,
            &content.0,
            trigger_kind,
            crate::BACKGROUND_REFRESH_DRAIN_TIMEOUT,
        )
        .await
    {
        return Ok(updater.0.snapshot());
    }
    #[cfg(target_os = "android")]
    {
        let background = trigger_kind == "background";
        let android_updater = app.state::<AndroidUpdaterHandle>();
        updater
            .0
            .install_android(backend.0.as_ref(), &android_updater, true, background)
            .await
    }
    #[cfg(target_os = "ios")]
    {
        let _ = (app, backend);
        Ok(updater.0.snapshot())
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        // On desktop, window-close installs run via `WindowEvent::CloseRequested`; the frontend
        // auto-install trigger applies only at startup before the first full screen renders.
        if trigger_kind != "startup" {
            return Ok(updater.0.snapshot());
        }
        let (st, should_exit) = updater.0.install_desktop(backend.0.as_ref(), true).await?;
        if should_exit {
            let win = app.get_webview_window("main");
            crate::notify_hidden_and_reset_desktop_player(&app, win.as_ref());
            let handle = app.clone();
            let transfers = Arc::clone(&transfers.0);
            let content = Arc::clone(&content.0);
            let backend = Arc::clone(&backend.0);
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                quiesce_for_update_exit(
                    &transfers,
                    &content,
                    &backend,
                    crate::UPDATE_EXIT_QUIESCE_TIMEOUT,
                )
                .await;
                let min_flush = std::time::Duration::from_millis(150);
                if let Some(remaining) = min_flush.checked_sub(started.elapsed()) {
                    tokio::time::sleep(remaining).await;
                }
                tauri_plugin_single_instance::destroy(&handle);
                handle.exit(0);
            });
        }
        Ok(st)
    }
}

#[cfg(debug_assertions)]
fn write_ready_marker(path: &Path) -> Result<(), AppError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|err| {
            AppError::new(
                ErrorCode::Internal,
                format!("Could not create directory for ready marker: {err}"),
            )
        })?;
    }

    fs::write(path, format!("{INTERFACE_READY_MARKER}\n")).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not write ready marker file: {err}"),
        )
    })
}

/// Resolves the default name for this device without panicking.
pub fn default_device_name() -> String {
    #[cfg(target_os = "android")]
    {
        if let Ok(output) = std::process::Command::new("/system/bin/getprop")
            .arg("ro.product.model")
            .output()
            && output.status.success()
        {
            let model = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !model.is_empty() {
                return model;
            }
        }
        "Android".to_owned()
    }

    #[cfg(not(target_os = "android"))]
    {
        let raw = gethostname::gethostname().to_string_lossy().into_owned();
        let trimmed = raw.trim().trim_end_matches(".local").trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
        #[cfg(target_os = "ios")]
        {
            "iOS".to_owned()
        }
        #[cfg(not(target_os = "ios"))]
        {
            "Legion".to_owned()
        }
    }
}

/// Opens the device camera on Android to scan a pairing invite QR code.
///
/// Camera permission is checked and requested only when this command is called.
#[cfg(target_os = "android")]
#[tauri::command]
pub async fn scan_invite(
    scanner: State<'_, ScannerHandle>,
    updater: State<'_, SharedUpdater>,
) -> Result<ScanInviteResult, AppError> {
    use serde_json::json;

    let _scan_guard = updater.0.track_external_operation();

    #[derive(Deserialize)]
    struct PermissionResponse {
        camera: Option<String>,
    }

    #[derive(Deserialize)]
    struct ScanResponse {
        content: Option<String>,
    }

    let mut perms: PermissionResponse = scanner
        .0
        .run_mobile_plugin_async("checkPermissions", json!({}))
        .await
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not check camera permission: {err}"),
            )
        })?;

    if perms.camera.as_deref() != Some("granted") {
        perms = scanner
            .0
            .run_mobile_plugin_async("requestPermissions", json!({}))
            .await
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Denied,
                    format!("Could not request camera permission: {err}"),
                )
                .with_hint(
                    "Allow camera access for Legion in system settings, or paste the invite instead.",
                )
            })?;
    }

    if perms.camera.as_deref() != Some("granted") {
        return Err(
            AppError::new(ErrorCode::Denied, "Camera permission was not granted.").with_hint(
                "Allow camera access to scan a QR code, or paste the invite text instead.",
            ),
        );
    }

    let scanned: ScanResponse = scanner
        .0
        .run_mobile_plugin_async(
            "scan",
            json!({
                "windowed": false,
                "formats": ["QR_CODE"],
                "cameraDirection": "back",
            }),
        )
        .await
        .map_err(|err| {
            let message = err.to_string();
            if message.to_ascii_lowercase().contains("cancel") {
                AppError::new(ErrorCode::Cancelled, "QR code scan was cancelled.")
            } else {
                AppError::new(ErrorCode::Failed, format!("QR code scan failed: {message}"))
                    .with_hint("Try scanning again or paste the invite text manually.")
            }
        })?;

    let content = scanned.content.unwrap_or_default().trim().to_owned();
    if content.is_empty() {
        return Err(
            AppError::new(ErrorCode::InvalidArgs, "Scanned QR code was empty.")
                .with_hint("Point the camera at the QR code shown by `legion pair`."),
        );
    }

    Ok(ScanInviteResult::Scanned { content })
}

/// Returns the barcode-scanner plugin instructions for scanning a pairing invite QR code on iOS.
///
/// The `barcode-scanner` plugin is registered on iOS and permitted in `capabilities/mobile.json`
/// (`plugin:barcode-scanner|check_permissions`, `request_permissions`, `scan`, and `cancel`), so
/// the web interface invokes the plugin directly without requiring `unsafe` FFI in this crate.
#[cfg(target_os = "ios")]
#[tauri::command]
pub async fn scan_invite() -> Result<ScanInviteResult, AppError> {
    Ok(ScanInviteResult::UsePlugin {
        plugin: "barcode-scanner".to_owned(),
        formats: vec!["QR_CODE".to_owned()],
        camera_direction: "back".to_owned(),
        windowed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(debug_assertions)]
    #[test]
    fn ready_marker_writes_expected_line() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let marker_path = dir.path().join("nested").join("ready.txt");
        write_ready_marker(&marker_path)?;
        let text = fs::read_to_string(&marker_path)?;
        assert_eq!(text, format!("{INTERFACE_READY_MARKER}\n"));
        Ok(())
    }

    #[test]
    fn scan_invite_result_serializes_cleanly() -> Result<(), Box<dyn std::error::Error>> {
        let scanned = ScanInviteResult::Scanned {
            content: "legion-invite:v1:example".to_owned(),
        };
        let scanned_json = serde_json::to_value(&scanned)?;
        assert_eq!(scanned_json["mode"], "scanned");
        assert_eq!(scanned_json["content"], "legion-invite:v1:example");

        let use_plugin = ScanInviteResult::UsePlugin {
            plugin: "barcode-scanner".to_owned(),
            formats: vec!["QR_CODE".to_owned()],
            camera_direction: "back".to_owned(),
            windowed: false,
        };
        let plugin_json = serde_json::to_value(&use_plugin)?;
        assert_eq!(plugin_json["mode"], "use_plugin");
        assert_eq!(plugin_json["plugin"], "barcode-scanner");
        assert_eq!(plugin_json["formats"], serde_json::json!(["QR_CODE"]));
        assert_eq!(plugin_json["camera_direction"], "back");
        assert_eq!(plugin_json["windowed"], false);
        Ok(())
    }

    #[test]
    fn manual_update_detects_active_native_work() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = crate::store::AppStore::in_dir(dir.path());
        let backend: Arc<dyn Backend> =
            Arc::new(crate::link_backend::LinkBackend::from_store(store));
        let content = crate::content::AppContentServer::new(Arc::clone(&backend));

        struct NoopPlatform;
        impl crate::transfers::TransferPlatform for NoopPlatform {
            fn pick_upload_files(
                &self,
            ) -> crate::backend::BoxFuture<
                '_,
                Result<Vec<crate::transfers::PickedUploadFile>, AppError>,
            > {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn finalize_download<'a>(
                &'a self,
                _id: &'a str,
                _part_path: &'a Path,
                file_name: &'a str,
                _mime: Option<&'a str>,
                _open_after_done: bool,
            ) -> crate::backend::BoxFuture<'a, Result<String, AppError>> {
                let name = file_name.to_owned();
                Box::pin(async move { Ok(name) })
            }
            fn open_local_file<'a>(
                &'a self,
                _local_path: &'a str,
                _mime: Option<&'a str>,
            ) -> crate::backend::BoxFuture<'a, Result<(), AppError>> {
                Box::pin(async { Ok(()) })
            }
            fn show_in_folder<'a>(
                &'a self,
                _local_path: &'a str,
            ) -> crate::backend::BoxFuture<'a, Result<(), AppError>> {
                Box::pin(async { Ok(()) })
            }
        }

        let transfers = crate::transfers::TransferManager::load_or_new(
            dir.path(),
            Arc::clone(&backend),
            Arc::new(NoopPlatform),
        );
        let updater = crate::updater::UpdateManager::with_config(
            dir.path().join("updates"),
            legion_update::TrustAnchors::compiled_in(),
            crate::updater::current_os(),
            crate::updater::current_arch(),
            crate::updater::PlatformEnv::detect(),
            "0.1.0".to_owned(),
            "test".to_owned(),
            202611250200,
        );

        assert!(!has_active_native_work(
            backend.as_ref(),
            &transfers,
            &content,
            &updater
        ));
        let _guard = updater.track_external_operation();
        assert!(has_active_native_work(
            backend.as_ref(),
            &transfers,
            &content,
            &updater
        ));
        Ok(())
    }

    #[test]
    fn command_seam_caps_invite_and_label_sizes() -> Result<(), Box<dyn std::error::Error>> {
        validate_invite_size(&"a".repeat(MAX_INVITE_BYTES))?;
        let err = validate_invite_size(&"a".repeat(MAX_INVITE_BYTES + 1))
            .err()
            .ok_or("expected invite cap error")?;
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        validate_label_size("Device name", &"b".repeat(MAX_LABEL_BYTES))?;
        let err = validate_label_size("Device name", &"b".repeat(MAX_LABEL_BYTES + 1))
            .err()
            .ok_or("expected label cap error")?;
        assert_eq!(err.code, ErrorCode::InvalidArgs);
        Ok(())
    }

    #[test]
    fn desktop_immersive_only_undoes_what_legion_did_and_never_forces_fullscreen_back() {
        let state = DesktopImmersiveState::default();
        assert!(!state.is_active());
        assert!(!state.leave());

        // Normal window -> enter requests fullscreen -> leave exits fullscreen.
        assert!(state.enter(false));
        assert!(state.is_active());
        // Repeated enter while already active does not overwrite entered_by_legion.
        assert!(!state.enter(true));
        assert!(state.leave());
        assert!(!state.is_active());
        assert!(!state.leave());

        // Window already full screen -> entering changes nothing, leaving changes nothing.
        assert!(!state.enter(true));
        assert!(state.is_active());
        assert!(!state.leave());
        assert!(!state.is_active());

        // Window already full screen -> user leaves full screen through WM -> controller follows
        // and never forces full screen back on.
        assert!(!state.enter(true));
        assert!(state.is_active());
        assert!(state.sync_external_fullscreen(false));
        assert!(!state.is_active());
        assert!(!state.leave());

        // Windowed -> Legion entered full screen -> user leaves full screen through WM ->
        // controller follows and leaving afterward changes nothing.
        assert!(state.enter(false));
        assert!(state.is_active());
        assert!(state.sync_external_fullscreen(false));
        assert!(!state.is_active());
        assert!(!state.leave());
    }

    #[test]
    fn mobile_noop_immersive_always_reports_inactive() {
        let res = mobile_noop_immersive_result();
        assert!(!res.active);
        assert!(!res.fullscreen);
    }

    #[test]
    fn linux_window_state_events_track_wm_fullscreen_and_external_exit() {
        let state = DesktopImmersiveState::default();
        assert_eq!(state.observed_fullscreen(), None);
        state.set_initial_observed_fullscreen(false);
        assert_eq!(state.observed_fullscreen(), Some(false));

        // Sequence A: WM makes the window full screen (without Legion calling set_fullscreen),
        // then player enters and exits full screen -> Legion neither enters nor leaves WM full screen.
        assert!(!state.record_window_state_fullscreen(true, true));
        assert_eq!(state.observed_fullscreen(), Some(true));
        assert!(!state.enter(state.observed_fullscreen().unwrap_or(false)));
        assert!(state.is_active());
        assert!(!state.leave());
        assert!(!state.is_active());
        assert_eq!(state.observed_fullscreen(), Some(true));

        // WM leaves full screen while player is inactive -> observed state clears, no external exit signal.
        assert!(!state.record_window_state_fullscreen(false, true));
        assert_eq!(state.observed_fullscreen(), Some(false));

        // Sequence B: Player enters full screen -> an unrelated GTK state event (changed_fs=false)
        // before WM acknowledges fullscreen does NOT cancel immersive -> WM applies fullscreen ->
        // WM exits fullscreen (changed_fs=true, is_fs=false) -> clears active and signals exit!
        assert!(state.enter(state.observed_fullscreen().unwrap_or(false)));
        assert!(state.is_active());
        assert!(!state.record_window_state_fullscreen(false, false));
        assert!(state.is_active());
        assert!(!state.record_window_state_fullscreen(true, true));
        assert_eq!(state.observed_fullscreen(), Some(true));
        assert!(state.is_active());
        assert!(state.record_window_state_fullscreen(false, true));
        assert!(!state.is_active());
        assert_eq!(state.observed_fullscreen(), Some(false));
        assert!(!state.leave());

        // WM entered full screen -> player enters -> WM exits while player active -> signals exit.
        assert!(!state.record_window_state_fullscreen(true, true));
        assert!(!state.enter(state.observed_fullscreen().unwrap_or(false)));
        assert!(state.is_active());
        assert!(state.record_window_state_fullscreen(false, true));
        assert!(!state.is_active());
        assert_eq!(state.observed_fullscreen(), Some(false));
    }
}
