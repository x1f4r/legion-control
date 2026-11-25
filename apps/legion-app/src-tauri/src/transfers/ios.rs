//! iOS and iPadOS transfer platform:
//! - Downloads land in the app's `Documents/Downloads` directory, exposed to the Apple Files app
//!   via `UIFileSharingEnabled` and `LSSupportsOpeningDocumentsInPlace` in `Info.plist`.
//! - Transfers run while Legion is in the foreground, pause automatically when iOS suspends the
//!   app (`WindowEvent::Suspended`), and resume automatically when the app returns
//!   (`WindowEvent::Resumed`).

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::backend::{AppError, BoxFuture, ErrorCode};
use crate::transfers::{
    PickedUploadFile, TransferPlatform, move_file_across_fs, sanitize_file_name, unique_dest_path,
};

/// iOS and iPadOS implementation of [`TransferPlatform`].
pub struct IosTransferPlatform {
    downloads_dir: PathBuf,
}

impl IosTransferPlatform {
    /// Creates the iOS transfer platform placing downloads inside `<Documents>/Downloads`.
    pub fn new(app: &AppHandle, data_dir: &Path) -> Self {
        let docs = app
            .path()
            .document_dir()
            .ok()
            .unwrap_or_else(|| data_dir.join("Documents"));
        Self {
            downloads_dir: docs.join("Downloads"),
        }
    }
}

impl TransferPlatform for IosTransferPlatform {
    fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>> {
        Box::pin(async move {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Picking files from iOS requires the system document picker.",
            )
            .with_hint("Use the browser door on iPhone or iPad to upload files until the native iOS picker is wired."))
        })
    }

    fn finalize_download<'a>(
        &'a self,
        _id: &'a str,
        part_path: &'a Path,
        file_name: &'a str,
        _mime: Option<&'a str>,
        _open_after_done: bool,
    ) -> BoxFuture<'a, Result<String, AppError>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.downloads_dir)
                .await
                .map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not create Legion Downloads folder in Files: {err}."),
                    )
                })?;
            let safe = sanitize_file_name(file_name);
            let dst = unique_dest_path(&self.downloads_dir, &safe);
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
                    "Downloaded file was not found in the Files app folder.",
                ));
            }
            Ok(())
        })
    }
}
