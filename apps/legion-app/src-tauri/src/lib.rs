//! Native shell for Legion across Android, iOS, macOS, Windows and Linux.

pub mod backend;
pub mod commands;
pub mod content;
pub mod link_backend;
pub mod store;
pub mod transfers;
pub mod updater;

use std::sync::Arc;

#[cfg(not(target_os = "android"))]
use tauri::WindowEvent;
use tauri::webview::NewWindowResponse;
use tauri::{Manager, RunEvent, Url, WebviewWindowBuilder};

use crate::backend::Backend;
use crate::commands::SharedBackend;
use crate::content::{AppContentServer, SharedContentServer};
use crate::link_backend::LinkBackend;
use crate::store::AppStore;
#[cfg(target_os = "ios")]
use crate::transfers::PauseReason;
use crate::transfers::{SharedTransfers, TransferManager, TransferPlatform};
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

#[cfg(target_os = "android")]
struct SharedAndroidTransferPlatform(Arc<transfers::android::AndroidTransferPlatform>);

#[cfg(target_os = "android")]
struct SharedAndroidTransferPlugin(Arc<transfers::android::AndroidTransferPluginHandle>);

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct SharedDesktopTransferPlatform(Arc<transfers::desktop::DesktopTransferPlatform>);

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

/// Testable core of [`should_defer_to_active_update`] operating on an explicit `updates_dir`.
///
/// Returns `true` when this desktop launch must exit early before registering single-instance,
/// creating the content server, loading `transfers/queue.json`, or opening a window because
/// another process holds an active update lock.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn should_defer_to_active_update_in(
    updates_dir: &std::path::Path,
    verify_only: bool,
    supervised_launch: bool,
) -> bool {
    if verify_only || supervised_launch {
        return false;
    }
    let Some(active_lock) = updater::desktop::inspect_active_update_lock(updates_dir) else {
        return false;
    };
    if active_lock.verify_only {
        updater::desktop::request_reopen_after_update(updates_dir);
    }
    true
}

/// Returns `true` when this desktop launch should defer to an in-progress update instead of
/// grabbing the single-instance lock, starting the content server or transfer queue, opening a
/// window, or holding install files open.
///
/// Runs before `tauri_plugin_single_instance::init` and before `AppContentServer` /
/// `TransferManager` setup so a user double-clicking the app icon during an update never steals
/// the single-instance name, binds a loopback socket, touches `transfers/queue.json`, or blocks
/// the swap/NSIS installer. When the active update is a headless window-close update
/// (`verify_only == true`), leaves a `reopen-requested` flag so the supervisor opens the newly
/// installed app interactively once the swap finishes.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn should_defer_to_active_update(verify_only: bool) -> bool {
    let supervised_launch = updater::desktop::is_supervised_launch();
    if verify_only || supervised_launch {
        return false;
    }
    let Some(updates_dir) = updater::desktop::early_desktop_updates_dir() else {
        return false;
    };
    should_defer_to_active_update_in(&updates_dir, verify_only, supervised_launch)
}

/// Outcome of handling a desktop window-close request when combining the device transfer queue
/// with automatic close-time updates.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DesktopCloseOutcome {
    /// Device transfers are active: keep the app alive hidden until the queue drains, and never
    /// spawn an update supervisor while transfers run.
    HideForDrain,
    /// No active transfers remain and the detached close-time update supervisor was spawned.
    CloseWithUpdate,
    /// No active transfers remain and no update supervisor was spawned.
    CloseNormally,
}

/// Returns the combined native active-operation count evaluated by desktop close-time updates.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn desktop_close_active_ops(
    backend: &dyn Backend,
    transfers: &TransferManager,
) -> usize {
    backend
        .active_operations()
        .saturating_add(usize::from(transfers.has_active_work()))
}

/// Coordinates desktop window close between active transfers and close-time auto-updates:
/// hides to drain if any transfer is active (never spawning a supervisor), and only spawns the
/// close-time update supervisor when both `!transfers.has_active_work()` and the native updater
/// gate allow it.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn handle_desktop_close_request(
    updater: &UpdateManager,
    backend: &dyn Backend,
    transfers: &TransferManager,
) -> DesktopCloseOutcome {
    if transfers.has_active_work() {
        return DesktopCloseOutcome::HideForDrain;
    }
    let active_ops = desktop_close_active_ops(backend, transfers);
    if updater.try_spawn_desktop_close_install(active_ops) {
        DesktopCloseOutcome::CloseWithUpdate
    } else {
        DesktopCloseOutcome::CloseNormally
    }
}

/// Runs right before a hidden-for-drain desktop app exits once its transfer queue has drained,
/// spawning a ready close-time update if no active transfers or native operations remain.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn try_spawn_close_install_after_drain(
    updater: &UpdateManager,
    backend: &dyn Backend,
    transfers: &TransferManager,
) -> bool {
    if transfers.has_active_work() {
        return false;
    }
    let active_ops = desktop_close_active_ops(backend, transfers);
    updater.try_spawn_desktop_close_install(active_ops)
}

/// Registers the in-app `FilesTransferPlugin` Kotlin plugin on Android for the system file picker,
/// lazy notification permission dialog, and `LocalSocket` bridge connection to
/// `TransferForegroundService`.
#[cfg(target_os = "android")]
fn android_files_transfer_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("files-transfer")
        .setup(|app, api| {
            let handle = api.register_android_plugin("com.x1f4r.legion", "FilesTransferPlugin")?;
            let shared = Arc::new(transfers::android::AndroidTransferPluginHandle(handle));
            app.manage(SharedAndroidTransferPlugin(Arc::clone(&shared)));
            if let Some(platform) = app.try_state::<SharedAndroidTransferPlatform>() {
                platform.0.attach_plugin(shared);
            }
            Ok(())
        })
        .build()
}

/// Builds the main Legion WebView window from `tauri.conf.json` with our strict bundled-origin
/// navigation guard and new-window denial.
fn build_main_webview_window(app: &tauri::AppHandle) -> tauri::Result<tauri::WebviewWindow> {
    let window_config = app
        .config()
        .app
        .windows
        .first()
        .cloned()
        .ok_or_else(|| tauri::Error::Io(std::io::Error::other("missing main window config")))?;
    WebviewWindowBuilder::from_config(app, &window_config)?
        .on_navigation(is_allowed_navigation_url)
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()
}

/// Builds the main Legion WebView window from `tauri.conf.json` with our strict bundled-origin
/// navigation guard and new-window denial.
///
/// Called on Android if `MainActivity` was swiped away while `TransferForegroundService` kept the
/// process alive and the user later reopens Legion.
pub fn create_main_webview_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    let _ = build_main_webview_window(app)?;
    Ok(())
}

/// Starts the Tauri application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(all(target_os = "android", debug_assertions))]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("legion-app"),
    );
    #[cfg(all(target_os = "android", not(debug_assertions)))]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("legion-app")
            .with_filter(
                android_logger::FilterBuilder::new()
                    .parse("error,legion-app=info,legion-transfers=info")
                    .build(),
            ),
    );

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let verify_only = is_verify_only_launch();

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    if should_defer_to_active_update(verify_only) {
        return;
    }

    let builder = tauri::Builder::default();

    #[cfg(target_os = "android")]
    let builder = builder.register_asynchronous_uri_scheme_protocol(
        content::CUSTOM_SCHEME_NAME,
        |ctx, request, responder| {
            let content_server = Arc::clone(&ctx.app_handle().state::<SharedContentServer>().0);
            tauri::async_runtime::spawn(async move {
                let response = content_server.handle_custom_scheme(request).await;
                responder.respond(response);
            });
        },
    );

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = if should_register_single_instance(verify_only) {
        builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(desktop) = app.try_state::<SharedDesktopTransferPlatform>() {
                desktop.0.clear_hidden_for_drain();
            }
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
        let backend: Arc<dyn Backend> = Arc::new(LinkBackend::from_store(store.clone()));
        let content_server = Arc::new(AppContentServer::new(Arc::clone(&backend)));

        #[cfg(target_os = "android")]
        let platform: Arc<dyn TransferPlatform> = {
            let android_platform =
                transfers::android::AndroidTransferPlatform::new(app.handle().clone(), &data_dir)
                    .map_err(|err| std::io::Error::other(err.message))?;
            app.manage(SharedAndroidTransferPlatform(Arc::clone(&android_platform)));
            if let Some(plugin) = app.try_state::<SharedAndroidTransferPlugin>() {
                android_platform.attach_plugin(Arc::clone(&plugin.0));
            }
            android_platform
        };

        #[cfg(target_os = "ios")]
        let platform: Arc<dyn TransferPlatform> = Arc::new(
            transfers::ios::IosTransferPlatform::new(app.handle(), &data_dir),
        );

        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        let platform: Arc<dyn TransferPlatform> = {
            let desktop_platform = Arc::new(transfers::desktop::DesktopTransferPlatform::new(
                app.handle().clone(),
                &data_dir,
            ));
            app.manage(SharedDesktopTransferPlatform(Arc::clone(&desktop_platform)));
            desktop_platform
        };

        let transfers = TransferManager::load_or_new(&data_dir, Arc::clone(&backend), platform);
        #[cfg(target_os = "android")]
        if let Some(android_platform) = app.try_state::<SharedAndroidTransferPlatform>() {
            android_platform.0.attach_manager(&transfers);
        }
        transfers.start();

        app.manage(store);
        app.manage(SharedBackend(backend));
        app.manage(SharedContentServer(content_server));
        app.manage(SharedTransfers(transfers));
        app.manage(SharedUpdater(updater));

        let _window = build_main_webview_window(app.handle())?;

        Ok(())
    });

    macro_rules! legion_invoke_handler {
        ($($extra:path),* $(,)?) => {
            tauri::generate_handler![
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
                commands::content_url,
                commands::transfer_download,
                commands::transfer_pick_uploads,
                commands::transfer_upload,
                commands::transfer_open_externally,
                commands::transfers,
                commands::transfer_control,
                commands::open_transfer,
                commands::show_transfer_in_folder,
                commands::transfer_watch,
                commands::transfer_unwatch,
                commands::transfer_settings,
                commands::transfer_set_settings,
                commands::update_state,
                commands::update_watch,
                commands::update_check,
                commands::update_install,
                commands::update_set_auto,
                commands::update_dismiss_notice,
                commands::update_maybe_auto_install,
                $($extra,)*
            ]
        };
    }

    #[cfg(target_os = "android")]
    let builder = builder
        .plugin(android_barcode_scanner_plugin())
        .plugin(android_app_updater_plugin())
        .plugin(android_files_transfer_plugin());

    #[cfg(target_os = "ios")]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());

    #[cfg(all(
        any(target_os = "android", target_os = "ios"),
        debug_assertions,
        feature = "test-upload-paths"
    ))]
    let builder = builder.invoke_handler(legion_invoke_handler!(
        commands::scan_invite,
        commands::media_check_spec,
        commands::media_check_report,
        commands::transfer_upload_test_files,
    ));

    #[cfg(all(
        any(target_os = "android", target_os = "ios"),
        debug_assertions,
        not(feature = "test-upload-paths")
    ))]
    let builder = builder.invoke_handler(legion_invoke_handler!(
        commands::scan_invite,
        commands::media_check_spec,
        commands::media_check_report,
    ));

    #[cfg(all(any(target_os = "android", target_os = "ios"), not(debug_assertions)))]
    let builder = builder.invoke_handler(legion_invoke_handler!(commands::scan_invite));

    #[cfg(all(
        not(any(target_os = "android", target_os = "ios")),
        debug_assertions,
        feature = "test-upload-paths"
    ))]
    let builder = builder.invoke_handler(legion_invoke_handler!(
        commands::media_check_spec,
        commands::media_check_report,
        commands::transfer_upload_test_files,
    ));

    #[cfg(all(
        not(any(target_os = "android", target_os = "ios")),
        debug_assertions,
        not(feature = "test-upload-paths")
    ))]
    let builder = builder.invoke_handler(legion_invoke_handler!(
        commands::media_check_spec,
        commands::media_check_report,
    ));

    #[cfg(all(
        not(any(target_os = "android", target_os = "ios")),
        not(debug_assertions)
    ))]
    let builder = builder.invoke_handler(legion_invoke_handler!());

    #[allow(clippy::expect_used)]
    // Fatal startup failure before any UI can render; Tauri applications terminate here.
    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building Legion application");

    app.run(|app_handle, event| match event {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } => {
            if let (Some(updater), Some(backend), Some(transfers)) = (
                app_handle.try_state::<SharedUpdater>(),
                app_handle.try_state::<SharedBackend>(),
                app_handle.try_state::<SharedTransfers>(),
            ) {
                match handle_desktop_close_request(&updater.0, backend.0.as_ref(), &transfers.0) {
                    DesktopCloseOutcome::HideForDrain => {
                        api.prevent_close();
                        if let Some(window) = app_handle.get_webview_window(&label) {
                            let _ = window.hide();
                        }
                        if let Some(desktop) =
                            app_handle.try_state::<SharedDesktopTransferPlatform>()
                        {
                            desktop.0.mark_hidden_for_drain();
                        }
                    }
                    DesktopCloseOutcome::CloseWithUpdate => {
                        tauri_plugin_single_instance::destroy(app_handle);
                    }
                    DesktopCloseOutcome::CloseNormally => {}
                }
            }
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => {
            if let Some(desktop) = app_handle.try_state::<SharedDesktopTransferPlatform>() {
                desktop.0.clear_hidden_for_drain();
            }
            if let Some(window) = app_handle.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        #[cfg(target_os = "ios")]
        RunEvent::WindowEvent {
            event: WindowEvent::Suspended,
            ..
        } => {
            if let Some(transfers) = app_handle.try_state::<SharedTransfers>() {
                transfers.0.pause_all_active(PauseReason::Background);
            }
        }
        #[cfg(target_os = "ios")]
        RunEvent::WindowEvent {
            event: WindowEvent::Resumed,
            ..
        } => {
            if let Some(content) = app_handle.try_state::<SharedContentServer>() {
                content.0.on_app_resumed();
            }
            if let Some(transfers) = app_handle.try_state::<SharedTransfers>() {
                transfers.0.resume_paused_for(PauseReason::Background);
            }
        }
        RunEvent::ExitRequested { code, api, .. } => {
            let Some(transfers) = app_handle.try_state::<SharedTransfers>() else {
                return;
            };
            #[cfg(target_os = "android")]
            {
                if code.is_none() && transfers.0.has_unfinished_or_paused() {
                    api.prevent_exit();
                }
            }
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                if code.is_none() {
                    if transfers.0.has_active_work() {
                        if !transfers::desktop::confirm_quit_with_active_transfers() {
                            api.prevent_exit();
                        } else {
                            transfers.0.shutdown();
                        }
                    } else if let (Some(updater), Some(backend)) = (
                        app_handle.try_state::<SharedUpdater>(),
                        app_handle.try_state::<SharedBackend>(),
                    ) && handle_desktop_close_request(
                        &updater.0,
                        backend.0.as_ref(),
                        &transfers.0,
                    ) == DesktopCloseOutcome::CloseWithUpdate
                    {
                        tauri_plugin_single_instance::destroy(app_handle);
                    }
                }
            }
            #[cfg(target_os = "ios")]
            {
                let _ = (code, api, transfers);
            }
        }
        RunEvent::Exit => {
            if let Some(transfers) = app_handle.try_state::<SharedTransfers>() {
                transfers.0.shutdown();
            }
            if let Some(content) = app_handle.try_state::<SharedContentServer>() {
                content.0.shutdown();
            }
        }
        _ => {}
    });
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
            "http://127.0.0.1:45123/v1/content/tok/evil.html",
            "http://127.0.0.1:45123/v1/content/tok/evil.svg",
            "http://legion-content.localhost/v1/content/tok/evil.html",
            "http://legion-content.localhost/v1/content/tok/evil.svg",
            "legion-content://localhost/v1/content/tok/evil.html",
            "legion-content://localhost/v1/content/tok/evil.svg",
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

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    #[test]
    fn early_second_instance_defers_before_content_server_or_transfer_queue_setup()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let data_dir = dir.path();
        let updates_dir = data_dir.join("updates");
        let transfers_dir = data_dir.join("transfers");

        // No active update lock -> normal launch proceeds.
        assert!(!should_defer_to_active_update_in(
            &updates_dir,
            false,
            false
        ));

        // Hold an active verify-only close-update lock (as Instance 1 does during swap).
        let lock = updater::desktop::try_acquire_update_lock(&updates_dir, 202611250225, true)?
            .ok_or("expected active update lock")?;

        // Normal second launch MUST defer immediately, request interactive reopen after the
        // headless swap completes, and never touch the transfers directory or queue.json.
        assert!(should_defer_to_active_update_in(&updates_dir, false, false));
        assert!(
            updates_dir
                .join(updater::desktop::REOPEN_REQUESTED_FILENAME)
                .is_file(),
            "second interactive launch during verify_only update must leave reopen-requested"
        );
        assert!(
            !transfers_dir.exists(),
            "deferred second instance must exit before TransferManager creates transfers/queue.json"
        );

        // Verify-only probes and supervised launches MUST NOT defer to their own update lock.
        assert!(!should_defer_to_active_update_in(&updates_dir, true, false));
        assert!(!should_defer_to_active_update_in(&updates_dir, false, true));

        drop(lock);
        assert!(!should_defer_to_active_update_in(
            &updates_dir,
            false,
            false
        ));
        Ok(())
    }
}
