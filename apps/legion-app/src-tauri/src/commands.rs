//! Tauri command surface backed by [`Backend`].

use std::sync::Arc;

use serde_json::{Map, Value};
use tauri::State;
use tauri::ipc::Channel;

use crate::backend::{
    AppError, AppState, Backend, CallEvent, CallId, CallRequest, CancelResult, PairResult,
    UnpairResult,
};

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
    on: Option<String>,
) -> Result<Vec<Value>, AppError> {
    backend.0.tools(on).await
}

/// Invokes a Legion tool and streams live events to `channel`.
#[tauri::command]
pub async fn call(
    backend: State<'_, SharedBackend>,
    tool: String,
    args: Option<Value>,
    on: Option<Value>,
    channel: Channel<CallEvent>,
) -> Result<Value, AppError> {
    let request = CallRequest {
        tool,
        args: args.unwrap_or_else(|| Value::Object(Map::new())),
        on,
    };
    backend.0.call(request, channel).await
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
) -> Result<PairResult, AppError> {
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

/// Returns a sensible default name for this device (model on mobile, host name on desktop).
#[tauri::command]
pub fn device_name() -> String {
    default_device_name()
}

/// Resolves the default name for this device without panicking.
pub fn default_device_name() -> String {
    #[cfg(target_os = "android")]
    {
        if let Ok(model) = std::env::var("LEGION_DEVICE_MODEL") {
            let trimmed = model.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
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
        return "Android".to_owned();
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
pub async fn scan_invite(scanner: State<'_, ScannerHandle>) -> Result<String, AppError> {
    use serde::Deserialize;
    use serde_json::json;

    use crate::backend::ErrorCode;

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

    Ok(content)
}

/// Scans a pairing invite QR code on iOS.
///
/// `tauri-plugin-barcode-scanner` 2.5.3 does not expose a public Rust `PluginHandle` accessor and
/// this crate forbids `unsafe_code` (required by `tauri::ios_plugin_binding!`). The plugin is
/// registered and permitted in `capabilities/mobile.json` so stage two can wire the iOS bridge
/// once the Xcode target is initialized.
#[cfg(target_os = "ios")]
#[tauri::command]
pub async fn scan_invite() -> Result<String, AppError> {
    use crate::backend::ErrorCode;

    Err(AppError::new(
        ErrorCode::Unsupported,
        "Native scan_invite command is not yet wired for iOS in this build.",
    )
    .with_hint("Use the barcode-scanner plugin bridge or paste the invite text."))
}
