//! Windows implementation of atomic file writes and sharing-safe replacement.
//!
//! Replacing an existing file via `MoveFileExW` (`std::fs::rename`) can fail temporarily with
//! `ERROR_ACCESS_DENIED` or `ERROR_SHARING_VIOLATION` while Windows Defender, Search Indexer,
//! or a concurrent reader holds the destination open. Retrying the rename preserves atomicity
//! without ever deleting the destination first.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::path::Path;

use crate::win_proc;
use crate::{OsError, Result};

use super::{
    ExclusiveRenameError, WINDOWS_INITIAL_BACKOFF, WINDOWS_MAX_BACKOFF,
    WINDOWS_REPLACE_BACKUP_TIMEOUT, WINDOWS_REPLACE_TIMEOUT, WriteOptions,
    is_retryable_windows_replace_error, is_retryable_windows_replace_with_backup_error,
    retry_async_with_backoff, retry_sync_with_backoff,
};

pub(super) fn configure_temp_open(_open: &mut OpenOptions, _restrictive: bool) {}

pub(super) fn apply_final_permissions(
    _file: &File,
    _existing_meta: Option<&Metadata>,
    _options: WriteOptions,
    _target: &Path,
) -> Result<()> {
    Ok(())
}

pub(super) fn replace_file(from: &Path, to: &Path) -> Result<()> {
    retry_sync_with_backoff(
        || fs::rename(from, to),
        is_retryable_windows_replace_error,
        WINDOWS_REPLACE_TIMEOUT,
        WINDOWS_INITIAL_BACKOFF,
        WINDOWS_MAX_BACKOFF,
    )
    .map_err(|err| {
        OsError::from_io(
            format!("cannot rename {} to {}", from.display(), to.display()),
            err,
        )
    })
}

pub(super) async fn replace_file_async(from: &Path, to: &Path) -> Result<()> {
    retry_async_with_backoff(
        || fs::rename(from, to),
        is_retryable_windows_replace_error,
        WINDOWS_REPLACE_TIMEOUT,
        WINDOWS_INITIAL_BACKOFF,
        WINDOWS_MAX_BACKOFF,
    )
    .await
    .map_err(|err| {
        OsError::from_io(
            format!("cannot rename {} to {}", from.display(), to.display()),
            err,
        )
    })
}

fn classify_windows_exclusive_error(err: io::Error) -> ExclusiveRenameError {
    // Win32: ERROR_FILE_EXISTS (80), ERROR_ALREADY_EXISTS (183).
    if err.kind() == io::ErrorKind::AlreadyExists || matches!(err.raw_os_error(), Some(80 | 183)) {
        return ExclusiveRenameError::AlreadyExists;
    }
    // Win32: ERROR_INVALID_FUNCTION (1), ERROR_NOT_SUPPORTED (50).
    if err.kind() == io::ErrorKind::Unsupported || matches!(err.raw_os_error(), Some(1 | 50)) {
        return ExclusiveRenameError::Unsupported;
    }
    ExclusiveRenameError::Io(err)
}

pub(super) fn rename_exclusive(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    retry_sync_with_backoff(
        || win_proc::move_file_no_replace(from, to),
        is_retryable_windows_replace_error,
        WINDOWS_REPLACE_TIMEOUT,
        WINDOWS_INITIAL_BACKOFF,
        WINDOWS_MAX_BACKOFF,
    )
    .map_err(classify_windows_exclusive_error)
}

pub(super) async fn rename_exclusive_async(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    retry_async_with_backoff(
        || win_proc::move_file_no_replace(from, to),
        is_retryable_windows_replace_error,
        WINDOWS_REPLACE_TIMEOUT,
        WINDOWS_INITIAL_BACKOFF,
        WINDOWS_MAX_BACKOFF,
    )
    .await
    .map_err(classify_windows_exclusive_error)
}

pub(super) fn exchange_files(_a: &Path, _b: &Path) -> Result<()> {
    Err(OsError::Unsupported {
        detail: "use replace_with_backup on Windows".to_string(),
    })
}

pub(super) fn replace_with_backup(staged: &Path, target: &Path, backup: &Path) -> Result<()> {
    retry_sync_with_backoff(
        || win_proc::replace_file_with_backup(target, staged, backup),
        is_retryable_windows_replace_with_backup_error,
        WINDOWS_REPLACE_BACKUP_TIMEOUT,
        WINDOWS_INITIAL_BACKOFF,
        WINDOWS_MAX_BACKOFF,
    )
    .map_err(|err| {
        if matches!(err.raw_os_error(), Some(1 | 50)) || err.kind() == io::ErrorKind::Unsupported {
            OsError::Unsupported {
                detail: format!("ReplaceFileW unsupported for {}: {err}", target.display()),
            }
        } else {
            OsError::from_io(
                format!(
                    "cannot replace {} with {} (backup {})",
                    target.display(),
                    staged.display(),
                    backup.display()
                ),
                err,
            )
        }
    })
}

pub(super) fn file_change_id(path: &Path, _meta: &Metadata) -> Option<u64> {
    win_proc::ntfs_file_change_id(path)
}

pub(super) fn available_space(dir: &Path) -> Result<u64> {
    win_proc::available_space(dir)
}

pub(super) fn same_device(a: &Path, b: &Path) -> Result<bool> {
    win_proc::same_device(a, b)
}

pub(super) fn sync_parent_dir(_parent: &Path) {}
