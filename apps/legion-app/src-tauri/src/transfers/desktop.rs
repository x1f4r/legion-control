//! Desktop transfer platform (Linux, macOS, Windows):
//! - Downloads land in the user's Downloads folder (`name (2).ext` on collision).
//! - "Open in another app" stages into the app cache and launches the system opener (`open`).
//! - Uploads use the native OS file dialog (`rfd`).
//! - Closing the window while transfers are running hides the window, sends a system notification,
//!   and exits automatically once the queue empties; quitting outright prompts for confirmation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager};

use crate::backend::{AppError, BoxFuture, ErrorCode};
use crate::transfers::{
    PickedUploadFile, QueueNotificationSummary, TransferPlatform, guess_mime_from_name,
    move_file_across_fs, sanitize_file_name, unique_dest_path,
};

/// Desktop implementation of [`TransferPlatform`].
pub struct DesktopTransferPlatform {
    app: AppHandle,
    downloads_dir: PathBuf,
    cache_dir: PathBuf,
    hidden_while_draining: Arc<AtomicBool>,
}

impl DesktopTransferPlatform {
    /// Creates the desktop transfer platform bound to `app`.
    pub fn new(app: AppHandle, data_dir: &Path) -> Self {
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
        Self {
            app,
            downloads_dir,
            cache_dir,
            hidden_while_draining: Arc::new(AtomicBool::new(false)),
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
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "file".to_owned());
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let mime = guess_mime_from_name(&name).map(str::to_owned);
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
                &self.cache_dir
            } else {
                &self.downloads_dir
            };
            tokio::fs::create_dir_all(dir).await.map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not create download folder {}: {err}.", dir.display()),
                )
            })?;
            let safe = sanitize_file_name(file_name);
            let target_name = if open_after_done {
                format!("{id}-{safe}")
            } else {
                safe
            };
            let dst = unique_dest_path(dir, &target_name);
            move_file_across_fs(part_path, &dst).await?;
            Ok(dst.to_string_lossy().into_owned())
        })
    }

    fn open_local_file<'a>(
        &'a self,
        local_path: &'a str,
        _mime: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            let path = PathBuf::from(local_path);
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
                    format!(
                        "Could not open {} in the system viewer: {err}.",
                        path.display()
                    ),
                )
            })
        })
    }

    fn on_queue_updated(&self, summary: QueueNotificationSummary) {
        if summary.active_count == 0 && self.hidden_while_draining.swap(false, Ordering::SeqCst) {
            notify_background_transfer("Transfers finished", "All Legion transfers have finished.");
            let app = self.app.clone();
            crate::transfers::spawn_bg(async move {
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                app.exit(0);
            });
        }
    }
}

/// Shows a native confirmation dialog before quitting outright while transfers are still active.
/// Returns `true` when the user confirmed quitting.
pub fn confirm_quit_with_active_transfers() -> bool {
    // Allow automated headless tests to inspect quit handling without blocking on a modal dialog.
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

/// Emits a best-effort desktop system notification using the platform's standard mechanism.
pub fn notify_background_transfer(title: &str, body: &str) {
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
