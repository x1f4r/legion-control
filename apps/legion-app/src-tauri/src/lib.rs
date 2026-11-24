//! Native shell for Legion across Android, iOS, macOS, Windows and Linux.

pub mod backend;
pub mod commands;
pub mod store;

use std::sync::Arc;

use tauri::Manager;

use crate::backend::Unpaired;
use crate::commands::SharedBackend;
use crate::store::AppStore;

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
    let builder = tauri::Builder::default()
        .manage(SharedBackend(Arc::new(Unpaired)))
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            app.manage(AppStore::in_dir(data_dir));
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
            commands::device_name,
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
            commands::device_name,
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
        commands::device_name,
    ]);

    #[allow(clippy::expect_used)]
    // Fatal startup failure before any UI can render; Tauri applications terminate here.
    builder
        .run(tauri::generate_context!())
        .expect("error while running Legion application");
}
