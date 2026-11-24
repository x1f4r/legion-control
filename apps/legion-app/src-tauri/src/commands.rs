//! Tauri command surface backed by [`Backend`].

use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tauri::State;
use tauri::ipc::Channel;

use crate::backend::{
    AppError, AppState, Backend, CallId, CallRequest, CancelResult, ErrorCode, RepairResult,
    UnpairResult,
};

/// Log line emitted when the web interface finishes its first paint.
pub const INTERFACE_READY_MARKER: &str = "legion-app: interface ready";

/// Optional environment variable naming a file to write [`INTERFACE_READY_MARKER`] to on ready.
pub const READY_FILE_ENV: &str = "LEGION_APP_READY_FILE";

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
    invite: String,
    name: Option<String>,
) -> Result<AppState, AppError> {
    let resolved_name = name
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_device_name);
    backend.0.pair(invite, resolved_name).await
}

/// Removes a paired node from this device.
#[tauri::command]
pub async fn unpair(
    backend: State<'_, SharedBackend>,
    node: String,
) -> Result<UnpairResult, AppError> {
    backend.0.unpair(node).await
}

/// Switches which configured node calls are sent through by default.
#[tauri::command]
pub async fn select_node(
    backend: State<'_, SharedBackend>,
    node: String,
) -> Result<AppState, AppError> {
    backend.0.select_node(node).await
}

/// Moves a damaged or unsupported state file aside (`legion-app.json.damaged-<timestamp>`) and
/// resets the backend to a clean state.
#[tauri::command]
pub async fn repair_store(backend: State<'_, SharedBackend>) -> Result<RepairResult, AppError> {
    backend.0.repair_store().await
}

/// Closes cached sessions on mobile platforms after resuming from background or sleep so the next
/// refresh opens fresh sockets instead of stalling on half-open TCP connections.
#[tauri::command]
pub async fn invalidate_sessions(backend: State<'_, SharedBackend>) -> Result<(), AppError> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        backend.0.invalidate_sessions().await
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let _ = backend;
        Ok(())
    }
}

/// Returns a sensible default name for this device (model on mobile, host name on desktop).
#[tauri::command]
pub fn device_name() -> String {
    default_device_name()
}

/// Signals that the web interface has mounted and painted its initial view.
///
/// Logs [`INTERFACE_READY_MARKER`] once to stdout (and logcat on Android). When
/// `LEGION_APP_READY_FILE` is set, also writes the marker line to that path so automated smoke
/// checks can assert startup without guessing a sleep duration.
#[tauri::command]
pub fn ready() -> Result<(), AppError> {
    if !READY_LOGGED.swap(true, Ordering::SeqCst) {
        println!("{INTERFACE_READY_MARKER}");
        let _ = std::io::stdout().flush();
        #[cfg(target_os = "android")]
        log::info!(target: "legion-app", "{INTERFACE_READY_MARKER}");
    }

    if let Some(path) = std::env::var_os(READY_FILE_ENV).filter(|val| !val.is_empty()) {
        write_ready_marker(Path::new(&path))?;
    }

    Ok(())
}

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
pub async fn scan_invite(scanner: State<'_, ScannerHandle>) -> Result<ScanInviteResult, AppError> {
    use serde_json::json;

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
}
