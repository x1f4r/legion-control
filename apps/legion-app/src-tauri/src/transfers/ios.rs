//! iOS and iPadOS transfer platform:
//! - Downloads land in the app's `Documents/Downloads` directory with atomic no-overwrite
//!   placement, exposed to the Apple Files app via `UIFileSharingEnabled` and
//!   `LSSupportsOpeningDocumentsInPlace` in `Info.plist`.
//! - Transfers run while Legion is in the foreground, pause automatically when iOS suspends the
//!   app (`WindowEvent::Suspended`), and resume automatically when the app returns
//!   (`WindowEvent::Resumed`).
//! - Native file-picker uploads and "Open in another app" are disabled via capability flags and
//!   return structured `unsupported` errors until a dedicated Swift plugin is added.

use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};

use crate::backend::{AppError, BoxFuture, ErrorCode};
use crate::transfers::file_policy::place_file_without_overwrite;
use crate::transfers::{PickedUploadFile, TransferPlatform};

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
    fn supports_open_externally(&self) -> bool {
        false
    }

    fn supports_open_downloads(&self) -> bool {
        false
    }

    fn supports_show_in_folder(&self) -> bool {
        false
    }

    fn pick_upload_files(&self) -> BoxFuture<'_, Result<Vec<PickedUploadFile>, AppError>> {
        Box::pin(async move {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Uploading files from this device is not yet supported in the iOS app.",
            )
            .with_hint(
                "Use the browser door on iPhone or iPad to upload files until native iOS file picking is added.",
            ))
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
            let dst =
                place_file_without_overwrite(part_path, &self.downloads_dir, file_name, false)
                    .await?;
            Ok(dst.to_string_lossy().into_owned())
        })
    }

    fn open_local_file<'a>(
        &'a self,
        _local_path: &'a str,
        _mime: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Opening downloaded files directly is not yet supported in the iOS app.",
            )
            .with_hint("Open the Apple Files app under On My iPhone > Legion > Downloads."))
        })
    }

    fn show_in_folder<'a>(&'a self, _local_path: &'a str) -> BoxFuture<'a, Result<(), AppError>> {
        Box::pin(async move {
            Err(AppError::new(
                ErrorCode::Unsupported,
                "Showing files in the Files app is not yet supported in the iOS app.",
            )
            .with_hint("Open the Apple Files app under On My iPhone > Legion > Downloads."))
        })
    }
}
