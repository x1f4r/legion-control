//! Desktop transfer platform (Linux, macOS, Windows):
//! - Downloads land in the user's Downloads folder with atomic no-overwrite placement
//!   (`name (2).ext` on collision) and OS download safety marks (Windows Zone.Identifier /
//!   macOS quarantine).
//! - "Open in another app" stages into a size- and age-bounded app cache and launches the system
//!   opener (`open`), refusing executable and installer extensions.
//! - Uploads use the native OS file dialog (`rfd`).
//! - Closing the window while transfers are running hides the window, sends a system notification,
//!   and exits automatically once no active transfers remain; quitting outright prompts for
//!   confirmation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager};

use crate::backend::{AppError, BoxFuture, ErrorCode};
use crate::transfers::file_policy::{
    MAX_OPEN_EXTERNALLY_CACHE_AGE, MAX_OPEN_EXTERNALLY_CACHE_BYTES,
    MAX_OPEN_EXTERNALLY_CACHE_FILES, REFUSED_OPEN_HINT, REFUSED_OPEN_MESSAGE, can_open_file_name,
    mime_for_file_name, place_file_without_overwrite, prune_cache_dir, sanitize_file_name,
    validate_child_in_dir,
};
use crate::transfers::{PickedUploadFile, QueueNotificationSummary, TransferPlatform};

/// Desktop implementation of [`TransferPlatform`].
pub struct DesktopTransferPlatform {
    app: AppHandle,
    downloads_dir: PathBuf,
    cache_dir: PathBuf,
    hidden_while_draining: Arc<AtomicBool>,
    exit_requested: Arc<AtomicBool>,
}

impl DesktopTransferPlatform {
    /// Creates the desktop transfer platform bound to `app` and prunes stale cache files.
    pub fn new(app: AppHandle, data_dir: &Path, exit_requested: Arc<AtomicBool>) -> Self {
        let downloads_dir = app
            .path()
            .download_dir()
            .ok()
            .unwrap_or_else(|| data_dir.join("Downloads"));
        let cache_dir = app
            .path()
            .app_cache_dir()
            .ok()
            .unwrap_or_else(|| data_dir.join("cache"))
            .join("open-externally");
        prune_cache_dir(
            &cache_dir,
            MAX_OPEN_EXTERNALLY_CACHE_FILES,
            MAX_OPEN_EXTERNALLY_CACHE_AGE,
            MAX_OPEN_EXTERNALLY_CACHE_BYTES,
        );
        Self {
            app,
            downloads_dir,
            cache_dir,
            hidden_while_draining: Arc::new(AtomicBool::new(false)),
            exit_requested,
        }
    }

    /// Marks that the user closed the window while transfers are still active so the app hides and
    /// exits automatically once the queue drains.
    pub fn mark_hidden_for_drain(&self) {
        self.hidden_while_draining.store(true, Ordering::SeqCst);
        notify_background_transfer(
            "Legion is still transferring",
            "Legion will stay open in the background until your transfers finish.",
        );
    }

    /// Clears the hidden-for-drain flag when the user reopens the main window (e.g. second launch
    /// or macOS dock click).
    pub fn clear_hidden_for_drain(&self) {
        self.hidden_while_draining.store(false, Ordering::SeqCst);
    }

    /// Returns true while the window is hidden waiting for the transfer queue to finish.
    pub fn is_hidden_for_drain(&self) -> bool {
        self.hidden_while_draining.load(Ordering::SeqCst)
    }
}

impl TransferPlatform for DesktopTransferPlatform {
    fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>> {
        Box::pin(async move {
            let handles = rfd::AsyncFileDialog::new()
                .set_title("Add files to machine")
                .pick_files()
                .await;
            let Some(handles) = handles else {
                return Ok(Vec::new());
            };
            let mut out = Vec::with_capacity(handles.len());
            for handle in handles {
                let path = handle.path().to_path_buf();
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(sanitize_file_name)
                    .unwrap_or_else(|| "download".to_owned());
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let mime = Some(mime_for_file_name(&name).to_owned());
                out.push(PickedUploadFile {
                    local_path: path.to_string_lossy().into_owned(),
                    name,
                    size,
                    mime,
                });
            }
            Ok(out)
        })
    }

    fn finalize_download<'a>(
        &'a self,
        id: &'a str,
        part_path: &'a Path,
        file_name: &'a str,
        _mime: Option<&'a str>,
        open_after_done: bool,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(async move {
            let dir = if open_after_done {
                prune_cache_dir(
                    &self.cache_dir,
                    MAX_OPEN_EXTERNALLY_CACHE_FILES.saturating_sub(1),
                    MAX_OPEN_EXTERNALLY_CACHE_AGE,
                    MAX_OPEN_EXTERNALLY_CACHE_BYTES,
                );
                &self.cache_dir
            } else {
                &self.downloads_dir
            };
            let safe = sanitize_file_name(file_name);
            let target_name = if open_after_done {
                format!("{id}-{safe}")
            } else {
                safe
            };
            let dst =
                place_file_without_overwrite(part_path, dir, &target_name, open_after_done).await?;
            Ok(dst.to_string_lossy().into_owned())
        })
    }

    fn cached_file_exists<'a>(
        &'a self,
        local_path: &'a str,
        expected_size: u64,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let path = PathBuf::from(local_path);
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                return false;
            };
            let Ok(expected) = validate_child_in_dir(&self.cache_dir, name) else {
                return false;
            };
            if expected != path {
                return false;
            }
            tokio::fs::metadata(&path)
                .await
                .map(|m| m.is_file() && m.len() == expected_size)
                .unwrap_or(false)
        })
    }

    fn open_local_file<'a>(
        &'a self,
        local_path: &'a str,
        _mime: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            let path = PathBuf::from(local_path);
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !can_open_file_name(file_name) {
                return Err(AppError::new(ErrorCode::Denied, REFUSED_OPEN_MESSAGE)
                    .with_hint(REFUSED_OPEN_HINT));
            }
            if !path.is_file() {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("Downloaded file is no longer at {}.", path.display()),
                )
                .with_hint("Download the file again if it was moved or removed."));
            }
            open::that_detached(&path).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not open {} in another app: {err}.", path.display()),
                )
                .with_hint("Download the file to save it instead.")
            })
        })
    }

    fn show_in_folder<'a>(&'a self, local_path: &'a str) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            let path = PathBuf::from(local_path);
            if !path.exists() {
                return Err(AppError::new(
                    ErrorCode::NotFound,
                    format!("Downloaded file is no longer at {}.", path.display()),
                )
                .with_hint("Download the file again if it was moved or removed."));
            }
            #[cfg(target_os = "macos")]
            {
                Command::new("open")
                    .arg("-R")
                    .arg(&path)
                    .spawn()
                    .map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Could not show {} in Finder: {err}.", path.display()),
                        )
                    })?;
                Ok(())
            }
            #[cfg(target_os = "windows")]
            {
                Command::new("explorer.exe")
                    .arg(format!("/select,{}", path.display()))
                    .spawn()
                    .map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Could not show {} in Explorer: {err}.", path.display()),
                        )
                    })?;
                Ok(())
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                let parent = path.parent().ok_or_else(|| {
                    AppError::new(ErrorCode::NotFound, "Downloaded file has no parent folder.")
                })?;
                open::that_detached(parent).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not open folder {}: {err}.", parent.display()),
                    )
                })
            }
        })
    }

    fn on_queue_updated(&self, summary: QueueNotificationSummary) {
        if summary.active_count == 0
            && summary.cancelling_count == 0
            && self.hidden_while_draining.swap(false, Ordering::SeqCst)
        {
            if summary.paused_count > 0 {
                notify_background_transfer(
                    "Transfers paused",
                    "Remaining transfers are paused. Open Legion again to resume.",
                );
            } else {
                notify_background_transfer(
                    "Transfers finished",
                    "All Legion transfers have finished.",
                );
            }
            let app = self.app.clone();
            let exit_requested = Arc::clone(&self.exit_requested);
            crate::transfers::spawn_bg(async move {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                // If the app is already quitting or the session is ending, never start a close-time
                // update supervisor that the OS will immediately terminate.
                if exit_requested.load(Ordering::SeqCst) {
                    return;
                }
                // If the user reopened the window or started a new transfer during the grace
                // period, stay running instead of exiting or installing an update under them.
                if app
                    .try_state::<crate::transfers::SharedTransfers>()
                    .is_some_and(|t| t.0.has_active_work())
                {
                    return;
                }
                if app
                    .get_webview_window("main")
                    .and_then(|w| w.is_visible().ok())
                    == Some(true)
                {
                    return;
                }
                if !exit_requested.load(Ordering::SeqCst)
                    && let (Some(updater), Some(backend), Some(transfers)) = (
                        app.try_state::<crate::updater::SharedUpdater>(),
                        app.try_state::<crate::commands::SharedBackend>(),
                        app.try_state::<crate::transfers::SharedTransfers>(),
                    )
                {
                    let updater = Arc::clone(&updater.0);
                    let backend = Arc::clone(&backend.0);
                    let transfers = Arc::clone(&transfers.0);
                    let spawned = tokio::task::spawn_blocking(move || {
                        crate::try_spawn_close_install_after_drain(
                            &updater,
                            backend.as_ref(),
                            &transfers,
                        )
                    })
                    .await
                    .unwrap_or(false);
                    if spawned {
                        if let (Some(transfers), Some(content), Some(backend)) = (
                            app.try_state::<crate::transfers::SharedTransfers>(),
                            app.try_state::<crate::content::SharedContentServer>(),
                            app.try_state::<crate::commands::SharedBackend>(),
                        ) {
                            crate::commands::quiesce_for_update_exit(
                                &transfers.0,
                                &content.0,
                                &backend.0,
                                crate::UPDATE_EXIT_QUIESCE_TIMEOUT,
                            )
                            .await;
                        }
                        tauri_plugin_single_instance::destroy(&app);
                    }
                }
                app.exit(0);
            });
        }
    }
}

/// Shows a native confirmation dialog before quitting outright while transfers are still active.
/// Returns `true` when the user confirmed quitting.
pub fn confirm_quit_with_active_transfers() -> bool {
    // Allow automated headless debug tests to inspect quit handling without blocking on a modal
    // dialog. Completely absent in release builds.
    #[cfg(debug_assertions)]
    if let Ok(mode) = std::env::var("LEGION_APP_TEST_QUIT_CONFIRM") {
        return mode == "allow";
    }
    let choice = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("Transfers in progress")
        .set_description(
            "Downloads or uploads are still running. Quit Legion anyway? Unfinished transfers will resume the next time you open Legion.",
        )
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Quit".to_owned(),
            "Keep running".to_owned(),
        ))
        .show();
    matches!(choice, rfd::MessageDialogResult::Ok)
        || matches!(choice, rfd::MessageDialogResult::Custom(ref s) if s == "Quit")
}

/// Shows the same native confirmation dialog when the user clicks manual "Update now" while native
/// device transfers, calls, or content streams are active. Returns `true` when confirmed.
pub fn confirm_update_with_active_work() -> bool {
    #[cfg(debug_assertions)]
    if let Ok(mode) = std::env::var("LEGION_APP_TEST_QUIT_CONFIRM") {
        return mode == "allow";
    }
    let choice = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title("Update and restart now?")
        .set_description(
            "Downloads, uploads, or active work are still running in Legion. Installing the update will restart Legion and interrupt them.",
        )
        .set_buttons(rfd::MessageButtons::OkCancelCustom(
            "Update now".to_owned(),
            "Keep running".to_owned(),
        ))
        .show();
    matches!(choice, rfd::MessageDialogResult::Ok)
        || matches!(choice, rfd::MessageDialogResult::Custom(ref s) if s == "Update now")
}

/// Emits a best-effort desktop system notification using the platform's standard mechanism.
pub fn notify_background_transfer(title: &str, body: &str) {
    #[cfg(debug_assertions)]
    println!("legion-app: background notification: {title} — {body}");
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("notify-send")
                .args(["--app-name=Legion", &title, &body])
                .status();
        }
        #[cfg(target_os = "macos")]
        {
            let _ = Command::new("osascript")
                .args([
                    "-e",
                    "on run argv",
                    "-e",
                    "display notification (item 2 of argv) with title (item 1 of argv)",
                    "-e",
                    "end run",
                    "--",
                    &title,
                    &body,
                ])
                .status();
        }
        #[cfg(target_os = "windows")]
        {
            let _ = (&title, &body);
        }
    });
}
