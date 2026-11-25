//! Native shell for Legion across Android, iOS, macOS, Windows and Linux.

pub mod backend;
pub mod commands;
pub mod link_backend;
pub mod store;

use std::sync::Arc;

use tauri::webview::NewWindowResponse;
use tauri::{Manager, Url, WebviewWindowBuilder};

use crate::commands::SharedBackend;
use crate::link_backend::LinkBackend;
use crate::store::AppStore;

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
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app.get_webview_window("main") {
            let _ = window.unminimize();
            let _ = window.show();
            let _ = window.set_focus();
        }
    }));

    let builder = builder.setup(|app| {
        let data_dir = app.path().app_data_dir()?;
        let store = AppStore::in_dir(data_dir);
        store.prepare_dir()?;
        let backend = LinkBackend::from_store(store.clone());
        app.manage(store);
        app.manage(SharedBackend(Arc::new(backend)));

        let window_config = app
            .config()
            .app
            .windows
            .first()
            .cloned()
            .ok_or_else(|| std::io::Error::other("missing main window config"))?;
        WebviewWindowBuilder::from_config(app.handle(), &window_config)?
            .on_navigation(is_allowed_navigation_url)
            .on_new_window(|_, _| NewWindowResponse::Deny)
            .build()?;

        Ok(())
    });

    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(android_barcode_scanner_plugin())
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
}
