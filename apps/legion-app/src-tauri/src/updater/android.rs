//! Android self-update bridge to `com.x1f4r.legion.AppUpdaterPlugin`.
//!
//! Uses Tauri's managed `PluginHandle` so this crate stays completely free of `unsafe` JNI code.

use serde::{Deserialize, Serialize};
#[cfg(target_os = "android")]
use serde_json::json;

#[cfg(target_os = "android")]
use crate::backend::{AppError, ErrorCode};

/// Managed Tauri plugin handle for `com.x1f4r.legion.AppUpdaterPlugin` on Android.
#[cfg(target_os = "android")]
pub struct AndroidUpdaterHandle(pub tauri::plugin::PluginHandle<tauri::Wry>);

/// Runtime Android installation capabilities reported by `AppUpdaterPlugin`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AndroidCapabilities {
    /// Whether `PackageManager.canRequestPackageInstalls()` is currently granted.
    #[serde(default)]
    pub can_request_package_installs: bool,
    /// Whether this package was last installed by itself (`installerPackage == packageName`).
    #[serde(default)]
    pub is_self_installer: bool,
    /// Current Android `installingPackageName` (for example `com.x1f4r.legion` or empty for adb).
    #[serde(default)]
    pub installer_package: String,
    /// Android `Build.VERSION.SDK_INT`.
    #[serde(default)]
    pub api_level: u32,
    /// Whether Android 12+ silent self-update preconditions are currently met.
    #[serde(default)]
    pub supports_silent_self_update: bool,
}

/// Outcome returned by `AppUpdaterPlugin.installApk`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AndroidInstallOutcome {
    /// `"installed"`, `"needs_permission"`, `"cancelled"`, or `"failed"`.
    pub status: String,
    /// Whether Android showed its system confirmation dialog (`STATUS_PENDING_USER_ACTION`).
    #[serde(default)]
    pub prompted: bool,
    /// Optional status detail from `PackageInstaller`.
    #[serde(default)]
    pub message: String,
}

#[cfg(target_os = "android")]
/// Queries Android install-packages capability and installer ownership.
pub async fn capabilities(handle: &AndroidUpdaterHandle) -> Result<AndroidCapabilities, AppError> {
    handle
        .0
        .run_mobile_plugin_async("capabilities", json!({}))
        .await
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not check Android update capabilities: {err}"),
            )
        })
}

#[cfg(target_os = "android")]
/// Opens Android's "Install unknown apps" permission screen for Legion and waits for the user to
/// return.
pub async fn request_install_permission(
    handle: &AndroidUpdaterHandle,
) -> Result<AndroidCapabilities, AppError> {
    handle
        .0
        .run_mobile_plugin_async("requestInstallPermission", json!({}))
        .await
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not open Android install permission settings: {err}"),
            )
        })
}

#[cfg(target_os = "android")]
/// Installs a verified APK located inside the app's private data directory via `PackageInstaller`.
pub async fn install_apk(
    handle: &AndroidUpdaterHandle,
    apk_path: &std::path::Path,
) -> Result<AndroidInstallOutcome, AppError> {
    handle
        .0
        .run_mobile_plugin_async("installApk", json!({ "path": apk_path.to_string_lossy() }))
        .await
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not start Android update install: {err}"),
            )
        })
}
