//! Native shell for Legion across Android, iOS, macOS, Windows and Linux.

pub mod backend;
pub mod commands;
pub mod link_backend;
pub mod store;
pub mod updater;

use std::sync::Arc;

use tauri::webview::NewWindowResponse;
use tauri::{Manager, Url, WebviewWindowBuilder};

use crate::commands::SharedBackend;
use crate::link_backend::LinkBackend;
use crate::store::AppStore;
use crate::updater::{SharedUpdater, UpdateManager};

/// Returns `true` when this desktop process was spawned by the update supervisor purely to verify
/// startup (`LEGION_UPDATE_VERIFY_ONLY=1`) and exit without opening a window.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn is_verify_only_launch() -> bool {
    std::env::var_os("LEGION_UPDATE_VERIFY_ONLY").is_some_and(|v| v == "1")
}

/// Returns `true` when desktop single-instance hand-over should be registered.
///
/// Verify-only update probes skip single-instance hand-over so reopening the app while the
/// window-close supervisor runs can never make the probe hand off and exit before writing
/// `started-<seq>.ok` (which would cause a spurious rollback of a healthy build).
pub const fn should_register_single_instance(verify_only: bool) -> bool {
    !verify_only
}

/// Returns `true` only for the bundled app origin on the current platform.
///
/// Linux, macOS and iOS serve bundled assets from `tauri://localhost`; Android and Windows serve
/// them from `http://tauri.localhost` (or `https://tauri.localhost` when HTTPS scheme is enabled).
/// All other schemes, hosts, userinfo, or explicit ports are blocked from navigating the WebView.
pub fn is_allowed_navigation_url(url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return false;
    }
    #[cfg(any(target_os = "android", target_os = "windows"))]
    {
        matches!(url.scheme(), "http" | "https") && url.host_str() == Some("tauri.localhost")
    }
    #[cfg(not(any(target_os = "android", target_os = "windows")))]
    {
        url.scheme() == "tauri" && url.host_str() == Some("localhost")
    }
}

/// Registers the official `tauri-plugin-barcode-scanner` Kotlin plugin on Android and keeps its
/// `PluginHandle` in managed state so `commands::scan_invite` can invoke it directly from Rust.
#[cfg(target_os = "android")]
fn android_barcode_scanner_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    // Keep the crate linked for Gradle plugin discovery while exposing its PluginHandle to Rust.
    let _ = tauri_plugin_barcode_scanner::init::<tauri::Wry>;
    tauri::plugin::Builder::new("barcode-scanner")
        .setup(|app, api| {
            let handle =
                api.register_android_plugin("app.tauri.barcodescanner", "BarcodeScannerPlugin")?;
            app.manage(commands::ScannerHandle(handle));
            Ok(())
        })
        .build()
}

/// Registers the in-app `AppUpdaterPlugin` Kotlin plugin on Android so `commands::update_install`
/// can install verified APKs via `PackageInstaller`.
#[cfg(target_os = "android")]
fn android_app_updater_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("app-updater")
        .setup(|app, api| {
            let handle = api.register_android_plugin("com.x1f4r.legion", "AppUpdaterPlugin")?;
            app.manage(updater::android::AndroidUpdaterHandle(handle));
            Ok(())
        })
        .build()
}

/// Starts the Tauri application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("legion-app"),
    );

    let builder = tauri::Builder::default();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let verify_only = is_verify_only_launch();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = if should_register_single_instance(verify_only) {
        builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
    } else {
        builder
    };

    let builder = builder.setup(move |app| {
        let data_dir = app.path().app_data_dir()?;
        #[cfg(target_os = "windows")]
        let updates_base_dir = app
            .path()
            .app_local_data_dir()
            .unwrap_or_else(|_| data_dir.clone());
        #[cfg(not(target_os = "windows"))]
        let updates_base_dir = data_dir.clone();

        let updater = Arc::new(UpdateManager::for_app(
            &updates_base_dir,
            Some(app.package_info().version.to_string()),
        ));

        // When spawned by the desktop swapper during a window-close update, confirm startup
        // cleanly (writing `started-<seq>.ok`, `current-build.json`, advancing `floor.json`, and
        // recording the post-update notice) and exit without popping a window open.
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        if verify_only {
            updater.on_app_ready();
            app.handle().exit(0);
            return Ok(());
        }

        let store = AppStore::in_dir(&data_dir);
        store.prepare_dir()?;
        let backend: Arc<dyn crate::backend::Backend> =
            Arc::new(LinkBackend::from_store(store.clone()));
        app.manage(store);
        app.manage(SharedBackend(Arc::clone(&backend)));
        app.manage(SharedUpdater(Arc::clone(&updater)));

        let window_config = app
            .config()
            .app
            .windows
            .first()
            .cloned()
            .ok_or_else(|| std::io::Error::other("missing main window config"))?;
        let window = WebviewWindowBuilder::from_config(app.handle(), &window_config)?
            .on_navigation(is_allowed_navigation_url)
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .build()?;

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        {
            let close_updater = Arc::clone(&updater);
            let close_backend = Arc::clone(&backend);
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    let _ = close_updater
                        .try_spawn_desktop_close_install(close_backend.active_operations());
                }
            });
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        let _ = window;

        Ok(())
    });

    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(android_barcode_scanner_plugin())
        .plugin(android_app_updater_plugin())
        .invoke_handler(tauri::generate_handler![
            commands::state,
            commands::tools,
            commands::call,
            commands::cancel,
            commands::pair,
            commands::unpair,
            commands::select_node,
            commands::repair_store,
            commands::invalidate_sessions,
            commands::learn,
            commands::accept_identity_change,
            commands::dismiss_identity_change,
            commands::device_name,
            commands::ready,
            commands::scan_invite,
            commands::update_state,
            commands::update_watch,
            commands::update_check,
            commands::update_install,
            commands::update_set_auto,
            commands::update_dismiss_notice,
            commands::update_maybe_auto_install,
        ]);

    #[cfg(target_os = "ios")]
    let builder = builder
        .plugin(tauri_plugin_barcode_scanner::init())
        .invoke_handler(tauri::generate_handler![
            commands::state,
            commands::tools,
            commands::call,
            commands::cancel,
            commands::pair,
            commands::unpair,
            commands::select_node,
            commands::repair_store,
            commands::invalidate_sessions,
            commands::learn,
            commands::accept_identity_change,
            commands::dismiss_identity_change,
            commands::device_name,
            commands::ready,
            commands::scan_invite,
            commands::update_state,
            commands::update_watch,
            commands::update_check,
            commands::update_install,
            commands::update_set_auto,
            commands::update_dismiss_notice,
            commands::update_maybe_auto_install,
        ]);

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        commands::state,
        commands::tools,
        commands::call,
        commands::cancel,
        commands::pair,
        commands::unpair,
        commands::select_node,
        commands::repair_store,
        commands::invalidate_sessions,
        commands::learn,
        commands::accept_identity_change,
        commands::dismiss_identity_change,
        commands::device_name,
        commands::ready,
        commands::update_state,
        commands::update_watch,
        commands::update_check,
        commands::update_install,
        commands::update_set_auto,
        commands::update_dismiss_notice,
        commands::update_maybe_auto_install,
    ]);

    #[allow(clippy::expect_used)]
    // Fatal startup failure before any UI can render; Tauri applications terminate here.
    builder
        .run(tauri::generate_context!())
        .expect("error while running Legion application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_guard_allows_only_bundled_app_origin() -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(any(target_os = "android", target_os = "windows"))]
        let allowed = [
            "http://tauri.localhost/",
            "http://tauri.localhost/index.html#/machines",
            "https://tauri.localhost/",
        ];
        #[cfg(not(any(target_os = "android", target_os = "windows")))]
        let allowed = [
            "tauri://localhost/",
            "tauri://localhost/index.html#/machines",
        ];

        for raw in allowed {
            let url = Url::parse(raw)?;
            assert!(
                is_allowed_navigation_url(&url),
                "expected app origin {raw} to be allowed"
            );
        }

        let blocked = [
            "https://example.com/",
            "http://localhost:8080/",
            "http://127.0.0.1/",
            "http://tauri.localhost:9999/",
            "http://user@tauri.localhost/",
            "tauri://evil.example/",
            "tauri://localhost:1234/",
            "file:///etc/passwd",
            "data:text/html,<h1>hi</h1>",
            "javascript:alert(1)",
            "about:blank",
        ];

        for raw in blocked {
            let url = Url::parse(raw)?;
            assert!(
                !is_allowed_navigation_url(&url),
                "expected external or unexpected URL {raw} to be blocked"
            );
        }
        Ok(())
    }

    #[test]
    fn linux_deb_package_name_does_not_collide_with_node_package()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(manifest_dir.join("tauri.linux.conf.json"))?;
        let conf: serde_json::Value = serde_json::from_str(&raw)?;

        assert_eq!(
            conf.get("productName").and_then(|v| v.as_str()),
            Some("legion-app"),
            "Linux productName must be legion-app so the .deb Package field does not collide with the legion node package"
        );

        let long_desc = conf
            .pointer("/bundle/longDescription")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            !long_desc.trim().is_empty(),
            "Linux bundle.longDescription should be set so DEBIAN/control does not emit (none)"
        );

        let template_rel = conf
            .pointer("/bundle/linux/deb/desktopTemplate")
            .and_then(|v| v.as_str())
            .ok_or("missing bundle.linux.deb.desktopTemplate in tauri.linux.conf.json")?;
        let desktop = std::fs::read_to_string(manifest_dir.join(template_rel))?;
        assert!(
            desktop.lines().any(|line| line.trim() == "Name=Legion"),
            "desktop template should keep Name=Legion for application launchers"
        );
        Ok(())
    }

    #[test]
    fn verify_only_launch_disables_single_instance_handover() {
        assert!(!should_register_single_instance(true));
        assert!(should_register_single_instance(false));
    }
}
