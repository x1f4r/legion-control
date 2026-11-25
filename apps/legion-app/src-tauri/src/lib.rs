//! Native shell for Legion across Android, iOS, macOS, Windows and Linux.

pub mod backend;
pub mod commands;
pub mod content;
pub mod keep_screen_on;
pub mod link_backend;
pub mod store;
pub mod transfers;
pub mod updater;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Maximum time an update install waits for an in-flight passive background refresh (such as
/// Machine Overview's 5-second `sys.info` / `sys.procs` poll) to finish cleanly before proceeding
/// between poll ticks. Automatic installs defer cleanly if a refresh is still mid-call past this
/// bound rather than cutting it off.
pub(crate) const BACKGROUND_REFRESH_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time the desktop updater exit path waits for cancelled transfers, active relayed
/// content streams, and SSH sessions to tear down cleanly before terminating the process for swap.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) const UPDATE_EXIT_QUIESCE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::Emitter;
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

/// Tracks whether the application has begun quitting or the OS session is ending so window-close
/// and drain-exit hooks never start a close-time update supervisor while the OS is tearing down
/// processes.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct SharedExitRequested(Arc<AtomicBool>);

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
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    should_defer_to_active_update_in_at(updates_dir, verify_only, supervised_launch, now_unix)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn should_defer_to_active_update_in_at(
    updates_dir: &std::path::Path,
    verify_only: bool,
    supervised_launch: bool,
    now_unix: u64,
) -> bool {
    if verify_only || supervised_launch {
        return false;
    }
    match updater::desktop::inspect_update_lock_at(updates_dir, now_unix) {
        updater::desktop::UpdateLockInspection::Active(active_lock) => {
            if active_lock.verify_only {
                updater::desktop::request_reopen_after_update(updates_dir);
            }
            true
        }
        updater::desktop::UpdateLockInspection::StaleCleared { reason } => {
            updater::desktop::record_stale_lock_notice(updates_dir, &reason);
            false
        }
        updater::desktop::UpdateLockInspection::None => false,
    }
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
/// hides to drain if any transfer is active (`queued`, `running`, or `cancelling`, never spawning
/// a supervisor), waits briefly for any mid-call passive background refresh to finish cleanly so
/// installs run between poll ticks without cutting off an RPC, and only spawns the close-time
/// update supervisor when both `!transfers.has_active_work()` and the native updater gate allow it.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn handle_desktop_close_request(
    updater: &UpdateManager,
    backend: &dyn Backend,
    transfers: &TransferManager,
) -> DesktopCloseOutcome {
    handle_desktop_close_request_with_timeout(
        updater,
        backend,
        transfers,
        BACKGROUND_REFRESH_DRAIN_TIMEOUT,
    )
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn handle_desktop_close_request_with_timeout(
    updater: &UpdateManager,
    backend: &dyn Backend,
    transfers: &TransferManager,
    refresh_drain_timeout: Duration,
) -> DesktopCloseOutcome {
    if transfers.has_active_work() {
        return DesktopCloseOutcome::HideForDrain;
    }
    let active_ops = desktop_close_active_ops(backend, transfers);
    if updater
        .evaluate_auto_install_gate("close", active_ops, false)
        .is_err()
    {
        return DesktopCloseOutcome::CloseNormally;
    }
    if !backend.wait_for_background_refreshes_sync(refresh_drain_timeout) {
        return DesktopCloseOutcome::CloseNormally;
    }
    if transfers.has_active_work() {
        return DesktopCloseOutcome::HideForDrain;
    }
    let active_ops_after = desktop_close_active_ops(backend, transfers);
    if updater.try_spawn_desktop_close_install(active_ops_after) {
        DesktopCloseOutcome::CloseWithUpdate
    } else {
        DesktopCloseOutcome::CloseNormally
    }
}

/// Runs right before a hidden-for-drain desktop app exits once its transfer queue has drained,
/// waiting briefly for any mid-call passive background refresh to finish cleanly and spawning a
/// ready close-time update if no active transfers or user-started native operations remain.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn try_spawn_close_install_after_drain(
    updater: &UpdateManager,
    backend: &dyn Backend,
    transfers: &TransferManager,
) -> bool {
    try_spawn_close_install_after_drain_with_timeout(
        updater,
        backend,
        transfers,
        BACKGROUND_REFRESH_DRAIN_TIMEOUT,
    )
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn try_spawn_close_install_after_drain_with_timeout(
    updater: &UpdateManager,
    backend: &dyn Backend,
    transfers: &TransferManager,
    refresh_drain_timeout: Duration,
) -> bool {
    if transfers.has_active_work() {
        return false;
    }
    let active_ops = desktop_close_active_ops(backend, transfers);
    if updater
        .evaluate_auto_install_gate("close", active_ops, false)
        .is_err()
    {
        return false;
    }
    if !backend.wait_for_background_refreshes_sync(refresh_drain_timeout) {
        return false;
    }
    if transfers.has_active_work() {
        return false;
    }
    let active_ops_after = desktop_close_active_ops(backend, transfers);
    updater.try_spawn_desktop_close_install(active_ops_after)
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

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn reset_desktop_player_state(
    app: &tauri::AppHandle,
    window: Option<&tauri::WebviewWindow>,
) {
    if let Some(keep) = app.try_state::<commands::DesktopKeepScreenOn>() {
        keep.release();
    }
    if let Some(immersive) = app.try_state::<commands::DesktopImmersiveState>() {
        commands::reset_desktop_immersive(window, &immersive);
    }
}

/// Notifies the page that the desktop window is hiding/closing (for drain, normal close, or an
/// update restart) so playback pauses in place, then releases native screen hold and full screen.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn notify_hidden_and_reset_desktop_player(
    app: &tauri::AppHandle,
    window: Option<&tauri::WebviewWindow>,
) {
    if let Some(win) = window {
        let _ = win.emit(commands::EVENT_WINDOW_HIDDEN, ());
    }
    reset_desktop_player_state(app, window);
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
    let window = WebviewWindowBuilder::from_config(app, &window_config)?
        .on_navigation(is_allowed_navigation_url)
        .on_new_window(|_, _| NewWindowResponse::Deny)
        .build()?;

    #[cfg(target_os = "linux")]
    {
        if let Ok(gtk_win) = window.gtk_window() {
            use gtk::prelude::WidgetExt;
            if let Some(gdk_win) = gtk_win.window()
                && let Some(immersive) = app.try_state::<commands::DesktopImmersiveState>()
            {
                immersive.set_initial_observed_fullscreen(
                    gdk_win.state().contains(gtk::gdk::WindowState::FULLSCREEN),
                );
            }
            let state_app = app.clone();
            let state_label = window.label().to_owned();
            gtk_win.connect_window_state_event(move |_, event| {
                let is_fs = event
                    .new_window_state()
                    .contains(gtk::gdk::WindowState::FULLSCREEN);
                let changed_fs = event
                    .changed_mask()
                    .contains(gtk::gdk::WindowState::FULLSCREEN);
                if let Some(immersive) = state_app.try_state::<commands::DesktopImmersiveState>()
                    && immersive.record_window_state_fullscreen(is_fs, changed_fs)
                    && let Some(win) = state_app.get_webview_window(&state_label)
                {
                    let _ = win.emit(commands::EVENT_IMMERSIVE_CHANGED, false);
                }
                gtk::glib::Propagation::Proceed
            });
        }

        let term_app = app.clone();
        let term_label = window.label().to_owned();
        let _ = window.with_webview(move |webview| {
            use webkit2gtk::WebViewExt;
            let wv = webview.inner();
            wv.connect_web_process_terminated(move |_, _| {
                let win = term_app.get_webview_window(&term_label);
                reset_desktop_player_state(&term_app, win.as_ref());
            });
        });
    }

    #[cfg(target_os = "windows")]
    {
        let term_app = app.clone();
        let term_label = window.label().to_owned();
        let _ = window.with_webview(move |webview| {
            let controller = webview.controller();
            let _ = legion_os::webview2::register_process_failed_handler(&controller, move || {
                let win = term_app.get_webview_window(&term_label);
                reset_desktop_player_state(&term_app, win.as_ref());
            });
        });
    }

    Ok(window)
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

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.on_page_load(|webview, payload| {
        if payload.event() == tauri::webview::PageLoadEvent::Started {
            let app = webview.app_handle();
            let win = app.get_webview_window(webview.label());
            reset_desktop_player_state(app, win.as_ref());
        }
    });

    #[cfg(target_os = "macos")]
    let builder = builder.on_web_content_process_terminate(|webview| {
        let app = webview.app_handle();
        let win = app.get_webview_window(webview.label());
        reset_desktop_player_state(app, win.as_ref());
    });

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

        // Clear any leftover `reopen-requested` flag on every normal interactive start (never
        // inside a `verify_only` probe, which must leave the flag for its supervisor to honor).
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        updater::desktop::clear_reopen_marker(&updates_base_dir.join("updates"));

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
            let exit_requested = Arc::new(AtomicBool::new(false));
            let desktop_platform = Arc::new(transfers::desktop::DesktopTransferPlatform::new(
                app.handle().clone(),
                &data_dir,
                Arc::clone(&exit_requested),
            ));
            app.manage(SharedExitRequested(exit_requested));
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
        app.manage(commands::DesktopImmersiveState::default());
        app.manage(commands::DesktopKeepScreenOn::default());

        let window = build_main_webview_window(app.handle())?;

        #[cfg(all(
            not(target_os = "linux"),
            not(any(target_os = "android", target_os = "ios"))
        ))]
        {
            let resize_app = app.handle().clone();
            let resize_label = window.label().to_owned();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::Resized(_) = event
                    && let Some(immersive) =
                        resize_app.try_state::<commands::DesktopImmersiveState>()
                    && immersive.is_active()
                    && let Some(win) = resize_app.get_webview_window(&resize_label)
                    && !commands::read_desktop_fullscreen(&win, &immersive).unwrap_or(true)
                    && immersive.sync_external_fullscreen(false)
                {
                    let _ = win.emit(commands::EVENT_IMMERSIVE_CHANGED, false);
                }
            });
        }
        #[cfg(any(target_os = "linux", target_os = "android", target_os = "ios"))]
        let _ = window;

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
                commands::restore_pairing,
                commands::cancel_pair,
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
                commands::content_status,
                commands::content_revoke,
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
                commands::window_set_immersive,
                commands::window_is_fullscreen,
                commands::set_keep_screen_on,
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
        commands::debug_set_window_fullscreen,
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
        commands::debug_set_window_fullscreen,
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
        commands::debug_set_window_fullscreen,
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
        commands::debug_set_window_fullscreen,
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

    #[cfg(any(target_os = "android", target_os = "ios"))]
    let mobile_suspended = AtomicBool::new(false);

    app.run(move |app_handle, event| match event {
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::CloseRequested { api, .. },
            ..
        } => {
            let window = app_handle.get_webview_window(&label);
            notify_hidden_and_reset_desktop_player(app_handle, window.as_ref());
            // Never spawn a close-time update once an app quit or OS session shutdown has begun.
            if app_handle
                .try_state::<SharedExitRequested>()
                .is_some_and(|flag| flag.0.load(Ordering::SeqCst))
            {
                return;
            }
            if let (Some(updater), Some(backend), Some(transfers)) = (
                app_handle.try_state::<SharedUpdater>(),
                app_handle.try_state::<SharedBackend>(),
                app_handle.try_state::<SharedTransfers>(),
            ) {
                match handle_desktop_close_request(&updater.0, backend.0.as_ref(), &transfers.0) {
                    DesktopCloseOutcome::HideForDrain => {
                        api.prevent_close();
                        if let Some(window) = window {
                            let _ = window.hide();
                        }
                        if let Some(desktop) =
                            app_handle.try_state::<SharedDesktopTransferPlatform>()
                        {
                            desktop.0.mark_hidden_for_drain();
                        }
                    }
                    DesktopCloseOutcome::CloseWithUpdate => {
                        if let Some(content) = app_handle.try_state::<SharedContentServer>() {
                            content.0.shutdown();
                        }
                        tauri_plugin_single_instance::destroy(app_handle);
                    }
                    DesktopCloseOutcome::CloseNormally => {}
                }
            }
        }
        #[cfg(not(any(target_os = "android", target_os = "ios")))]
        RunEvent::WindowEvent {
            label,
            event: WindowEvent::Destroyed,
            ..
        } => {
            let window = app_handle.get_webview_window(&label);
            notify_hidden_and_reset_desktop_player(app_handle, window.as_ref());
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
        #[cfg(any(target_os = "android", target_os = "ios"))]
        RunEvent::WindowEvent {
            event: WindowEvent::Suspended,
            ..
        } => {
            mobile_suspended.store(true, Ordering::Release);
            #[cfg(target_os = "ios")]
            if let Some(transfers) = app_handle.try_state::<SharedTransfers>() {
                transfers.0.pause_all_active(PauseReason::Background);
            }
        }
        #[cfg(any(target_os = "android", target_os = "ios"))]
        RunEvent::WindowEvent {
            event: WindowEvent::Resumed,
            ..
        } => {
            mobile_suspended.store(false, Ordering::Release);
            if let Some(content) = app_handle.try_state::<SharedContentServer>() {
                content.0.on_app_resumed();
            }
            #[cfg(target_os = "ios")]
            if let Some(transfers) = app_handle.try_state::<SharedTransfers>() {
                transfers.0.resume_paused_for(PauseReason::Background);
            }
        }
        #[cfg(target_os = "ios")]
        RunEvent::WindowEvent {
            event: WindowEvent::Focused(true),
            ..
        } => {
            // If iOS went inactive (`sceneWillResignActive`) and returned to active
            // (`sceneDidBecomeActive`) without entering background, `WindowEvent::Resumed` does
            // not fire; still recover the listener and resume background-paused transfers.
            if mobile_suspended.swap(false, Ordering::AcqRel) {
                if let Some(content) = app_handle.try_state::<SharedContentServer>() {
                    content.0.on_app_resumed();
                }
                if let Some(transfers) = app_handle.try_state::<SharedTransfers>() {
                    transfers.0.resume_paused_for(PauseReason::Background);
                }
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
                if code.is_none() && transfers.0.has_active_work() {
                    if !transfers::desktop::confirm_quit_with_active_transfers() {
                        api.prevent_exit();
                        return;
                    }
                    transfers.0.shutdown();
                }
                // Close-time updates start ONLY from the user closing the window, never from
                // quitting the app or the OS ending the session (`ExitRequested`).
                if let Some(flag) = app_handle.try_state::<SharedExitRequested>() {
                    flag.0.store(true, Ordering::SeqCst);
                }
            }
            #[cfg(target_os = "ios")]
            {
                let _ = (code, api, transfers);
            }
        }
        RunEvent::Exit => {
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            {
                let window = app_handle.get_webview_window("main");
                notify_hidden_and_reset_desktop_player(app_handle, window.as_ref());
                if let Some(flag) = app_handle.try_state::<SharedExitRequested>() {
                    flag.0.store(true, Ordering::SeqCst);
                }
            }
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
            "http://127.0.0.1:45123/v1/content/tok/evil.html?download=1",
            "http://127.0.0.1:45123/v1/content/tok/evil.svg",
            "http://legion-content.localhost/v1/content/tok/evil.html",
            "http://legion-content.localhost/v1/content/tok/evil.svg",
            "legion-content://localhost/v1/content/tok/evil.html",
            "legion-content://localhost/v1/content/tok/evil.svg",
            "http://tauri.localhost:9999/",
            "http://user@tauri.localhost/",
            "http://tauri.localhost.evil.example/",
            "tauri://evil.example/",
            "tauri://localhost:1234/",
            "tauri://localhost.evil.example/",
            "file:///etc/passwd",
            "data:text/html,<h1>hi</h1>",
            "javascript:alert(1)",
            "about:blank",
            "chrome-error://chromewebdata/",
            "blob:http://127.0.0.1:45123/00000000-0000-0000-0000-000000000000",
            "blob:http://tauri.localhost/00000000-0000-0000-0000-000000000000",
            "blob:tauri://localhost/00000000-0000-0000-0000-000000000000",
            "view-source:http://127.0.0.1:45123/v1/content/tok/evil.html",
            "ws://127.0.0.1:45123/v1/content/tok/evil.html",
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
    fn default_capability_allows_event_listen_without_emit()
    -> Result<(), Box<dyn std::error::Error>> {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let raw = std::fs::read_to_string(manifest_dir.join("capabilities/default.json"))?;
        let cap: serde_json::Value = serde_json::from_str(&raw)?;
        let perms: Vec<&str> = cap
            .get("permissions")
            .and_then(|v| v.as_array())
            .ok_or("missing permissions array in capabilities/default.json")?
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(perms.contains(&"core:event:allow-listen"));
        assert!(perms.contains(&"core:event:allow-unlisten"));
        assert!(!perms.contains(&"core:event:allow-emit"));
        assert!(!perms.contains(&"core:event:allow-emit-to"));
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

        // Normal start clears any leftover `reopen-requested` flag.
        updater::desktop::clear_reopen_marker(&updates_dir);
        assert!(
            !updates_dir
                .join(updater::desktop::REOPEN_REQUESTED_FILENAME)
                .exists()
        );

        // Stale (> 3 minutes) or future-dated update.lock MUST NOT block startup, MUST clear the
        // lock file, and MUST record `stale-lock.json` so Updates reports an unfinished update.
        let now: u64 = 1_800_000_000;
        let future_lock = updater::desktop::UpdateLockState {
            owner_pid: std::process::id(),
            supervisor_pid: Some(std::process::id()),
            target_sequence: 202611250226,
            stage: "supervising".to_owned(),
            verify_only: false,
            started_unix: now + 300,
        };
        std::fs::write(
            updates_dir.join(updater::desktop::UPDATE_LOCK_FILENAME),
            serde_json::to_vec(&future_lock)?,
        )?;
        assert!(
            !should_defer_to_active_update_in_at(&updates_dir, false, false, now),
            "future-dated update.lock must never block startup"
        );
        assert!(
            !updates_dir
                .join(updater::desktop::UPDATE_LOCK_FILENAME)
                .exists(),
            "future-dated update.lock must be cleared"
        );
        assert!(
            updates_dir
                .join(updater::desktop::STALE_LOCK_MARKER_FILENAME)
                .is_file(),
            "clearing a stale or future lock must leave stale-lock.json for Updates"
        );
        Ok(())
    }
}
