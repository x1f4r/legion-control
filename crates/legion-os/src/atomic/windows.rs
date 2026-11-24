//! Windows implementation of atomic file writes and sharing-safe replacement.
//!
//! Replacing an existing file via `MoveFileExW` (`std::fs::rename`) can fail temporarily with
//! `ERROR_ACCESS_DENIED` or `ERROR_SHARING_VIOLATION` while Windows Defender, Search Indexer,
//! or a concurrent reader holds the destination open. Retrying the rename preserves atomicity
//! without ever deleting the destination first.

use std::fs::{self, File, Metadata, OpenOptions};
use std::path::Path;

use crate::{OsError, Result};

use super::{
    WINDOWS_INITIAL_BACKOFF, WINDOWS_MAX_BACKOFF, WINDOWS_REPLACE_TIMEOUT, WriteOptions,
    is_retryable_windows_replace_error, retry_async_with_backoff, retry_sync_with_backoff,
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

pub(super) fn sync_parent_dir(_parent: &Path) {}
