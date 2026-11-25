//! Structured errors for the folder synchronization engine.
//!
//! Every error maps to a stable [`legion_proto::ErrorCode`], a plain-language sentence, and an
//! actionable hint so callers and upper-layer tools can report failures consistently.

use legion_proto::{ErrorBody, ErrorCode};
use legion_xfer::Hash;

/// Convenience result alias for sync engine operations.
pub type Result<T> = std::result::Result<T, SyncError>;

/// Plain-language message shown when a folder's sync index is missing, empty, or damaged.
pub const INDEX_DAMAGED_PROBLEM_SENTENCE: &str =
    "This folder's sync record is damaged. Nothing was changed.";

/// Category of filesystem failure reported by a [`crate::SyncFs`] implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsErrorKind {
    /// Target file or directory did not exist.
    NotFound,
    /// Operating system refused access to the path.
    PermissionDenied,
    /// Target path already existed when exclusivity was required.
    AlreadyExists,
    /// Directory could not be removed because it still held entries.
    DirectoryNotEmpty,
    /// Storage volume ran out of space or quota.
    NoSpace,
    /// Rename crossed filesystems or mount points (`EXDEV`).
    CrossDevice,
    /// Simulated or injected crash fault used by crash-recovery tests.
    Crash,
    /// Any other I/O error.
    Other,
}

/// Failure modes produced by `legion-sync`.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// Invalid folder configuration, member identifier, or parameter.
    #[error("{message}")]
    InvalidConfig {
        /// Plain sentence describing what was invalid.
        message: String,
        /// Actionable suggestion, if any.
        hint: Option<String>,
    },

    /// A relative sync path violated universal safety rules (escape, traversal, NUL, reserved).
    #[error("unsafe sync path {path:?}: {reason}")]
    InvalidPath {
        /// Offending path string.
        path: String,
        /// Why the path was rejected.
        reason: String,
    },

    /// A folder ignore glob could not be compiled.
    #[error("invalid ignore pattern {pattern:?}: {reason}")]
    InvalidPattern {
        /// Offending glob pattern.
        pattern: String,
        /// Compiler error detail.
        reason: String,
    },

    /// Access was refused because the caller is not a member of the folder or asked for a
    /// forbidden location such as `.legion-sync/`.
    #[error("access denied for member {member:?}: {reason}")]
    Denied {
        /// Member whose request was refused.
        member: String,
        /// Reason for refusal.
        reason: String,
    },

    /// A requested entry, bin item, conflict, or peer record was not found.
    #[error("not found: {what}")]
    NotFound {
        /// Description of the missing resource.
        what: String,
    },

    /// A target path already existed when creating or restoring without overwrite.
    #[error("path already exists: {path}")]
    Exists {
        /// Relative path that already existed.
        path: String,
    },

    /// Received or restored file bytes did not match the expected BLAKE3 hash.
    #[error("hash mismatch for {path}: expected {expected}, got {actual}")]
    HashMismatch {
        /// Relative path being verified.
        path: String,
        /// Expected digest.
        expected: Hash,
        /// Actual computed digest.
        actual: Hash,
    },

    /// Received byte count did not match the record's declared file size.
    #[error("size mismatch for {path}: expected {expected} bytes, got {actual}")]
    SizeMismatch {
        /// Relative path being verified.
        path: String,
        /// Expected size in bytes.
        expected: u64,
        /// Actual byte count received.
        actual: u64,
    },

    /// Operation conflicted with current state (for example disk changed since last scan).
    #[error("conflict at {path}: {reason}")]
    Conflict {
        /// Relative path or folder identifier.
        path: String,
        /// Explanation of the conflict.
        reason: String,
    },

    /// Operation is unsupported on this platform, filesystem, or schema version.
    #[error("{reason}")]
    Unsupported {
        /// Explanation of what is unsupported.
        reason: String,
        /// Suggested workaround, if any.
        hint: Option<String>,
    },

    /// Peer could not be reached or aborted the exchange.
    #[error("peer {member:?} is unreachable: {detail}")]
    PeerUnavailable {
        /// Peer member identifier.
        member: String,
        /// Transport or peer error detail.
        detail: String,
    },

    /// A fetch or peer call exceeded its allowed duration.
    #[error("timed out during {operation}: {detail}")]
    Timeout {
        /// Operation that timed out.
        operation: String,
        /// Timeout detail.
        detail: String,
    },

    /// Caller cancelled an in-flight sync operation.
    #[error("sync operation cancelled: {operation}")]
    Cancelled {
        /// Operation that was cancelled.
        operation: String,
    },

    /// Folder syncing is currently paused (by user or by the mass-deletion brake).
    #[error("sync folder {folder_id:?} is paused")]
    Paused {
        /// Folder identifier.
        folder_id: String,
    },

    /// Sync folder root is missing its identity marker or points to a different folder/drive.
    #[error("{sentence}")]
    FolderUnavailable {
        /// Folder identifier.
        folder_id: String,
        /// Human-readable explanation shown to the person.
        sentence: String,
    },

    /// Local sync index file is missing beside an existing folder, 0-byte, corrupt, or panicked
    /// inside the storage backend; the folder is stopped and nothing on disk was changed.
    #[error("{sentence}")]
    IndexDamaged {
        /// Folder identifier or index path.
        folder_id: String,
        /// Plain-language explanation shown to the person.
        sentence: String,
        /// Underlying technical detail for diagnostics.
        detail: String,
    },

    /// Index file was written by a newer schema version than this build understands.
    #[error("sync index schema version {found} is newer than supported version {supported}")]
    SchemaTooNew {
        /// Schema version found on disk.
        found: u32,
        /// Highest schema version supported by this build.
        supported: u32,
    },

    /// Index store read, write, or transaction error.
    #[error("index storage error during {context}: {detail}")]
    Storage {
        /// High-level storage operation that failed.
        context: String,
        /// Underlying store error message.
        detail: String,
    },

    /// Persisted index or bin metadata could not be decoded cleanly.
    #[error("corrupt sync metadata in {context}: {detail}")]
    CorruptData {
        /// Location or table where corruption was detected.
        context: String,
        /// Decode failure detail.
        detail: String,
    },

    /// Filesystem operation failed.
    #[error("{}", format_fs_error(path.as_deref(), detail))]
    Fs {
        /// Relative path involved, when applicable.
        path: Option<String>,
        /// Human-readable description of the failure.
        detail: String,
        /// Classified filesystem failure kind.
        kind: FsErrorKind,
    },
}

fn format_fs_error(path: Option<&str>, detail: &str) -> String {
    match path {
        Some(p) => format!("filesystem error at {p}: {detail}"),
        None => format!("filesystem error: {detail}"),
    }
}

impl SyncError {
    /// Returns the stable protocol [`ErrorCode`] corresponding to this error.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidConfig { .. } | Self::InvalidPath { .. } | Self::InvalidPattern { .. } => {
                ErrorCode::InvalidArgs
            }
            Self::Denied { .. } => ErrorCode::Denied,
            Self::NotFound { .. } => ErrorCode::NotFound,
            Self::Exists { .. } => ErrorCode::Exists,
            Self::HashMismatch { .. }
            | Self::SizeMismatch { .. }
            | Self::Conflict { .. }
            | Self::Paused { .. } => ErrorCode::Conflict,
            Self::Unsupported { .. } | Self::SchemaTooNew { .. } => ErrorCode::Unsupported,
            Self::PeerUnavailable { .. } => ErrorCode::Unreachable,
            Self::Timeout { .. } => ErrorCode::Timeout,
            Self::Cancelled { .. } => ErrorCode::Cancelled,
            Self::FolderUnavailable { .. } | Self::IndexDamaged { .. } => ErrorCode::Failed,
            Self::Storage { .. } | Self::CorruptData { .. } => ErrorCode::Internal,
            Self::Fs { kind, .. } => match kind {
                FsErrorKind::NotFound => ErrorCode::NotFound,
                FsErrorKind::PermissionDenied => ErrorCode::Denied,
                FsErrorKind::AlreadyExists => ErrorCode::Exists,
                FsErrorKind::DirectoryNotEmpty => ErrorCode::Conflict,
                FsErrorKind::NoSpace
                | FsErrorKind::CrossDevice
                | FsErrorKind::Crash
                | FsErrorKind::Other => ErrorCode::Failed,
            },
        }
    }

    /// Returns an actionable hint for a person or agent dealing with this error.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::InvalidConfig { hint, .. } | Self::Unsupported { hint, .. } => hint.clone(),
            Self::InvalidPath { .. } => Some(
                "Use a relative path inside the sync folder without leading slashes, '..' segments, Windows drive prefixes, control characters, or '.legion-sync'."
                    .to_string(),
            ),
            Self::InvalidPattern { .. } => {
                Some("Check the ignore glob syntax in the folder configuration.".to_string())
            }
            Self::Denied { .. } => Some(
                "Only members listed in the sync folder may read its index or files outside '.legion-sync'."
                    .to_string(),
            ),
            Self::NotFound { .. } => {
                Some("Check the path or identifier and try again.".to_string())
            }
            Self::Exists { .. } => Some(
                "Choose a different target path or use overwrite or rename when restoring."
                    .to_string(),
            ),
            Self::HashMismatch { .. } | Self::SizeMismatch { .. } => Some(
                "The file may have changed on the peer while transferring; retry syncing."
                    .to_string(),
            ),
            Self::Conflict { .. } => Some(
                "Inspect the folder status, scan local changes, and resolve any conflicts before retrying."
                    .to_string(),
            ),
            Self::PeerUnavailable { .. } => {
                Some("Wait for the peer machine to come back online and sync again.".to_string())
            }
            Self::Timeout { .. } => {
                Some("Check network connectivity to the peer and retry syncing.".to_string())
            }
            Self::Cancelled { .. } => None,
            Self::Paused { .. } => Some(
                "Resume the sync folder (or confirm/rescan any pending deletion brake) before syncing."
                    .to_string(),
            ),
            Self::FolderUnavailable { .. } => Some(
                "Reconnect the drive or restore the original folder before resuming sync."
                    .to_string(),
            ),
            Self::IndexDamaged { .. } => Some(
                "Run the explicit rebuild-from-disk action to recreate the sync index from the folder without changing any files."
                    .to_string(),
            ),
            Self::SchemaTooNew { .. } => Some(
                "Upgrade Legion to a newer release before opening this sync folder.".to_string(),
            ),
            Self::Storage { .. } | Self::CorruptData { .. } => Some(
                "Check disk space and permissions for the sync index, or rebuild the local index."
                    .to_string(),
            ),
            Self::Fs { kind, .. } => match kind {
                FsErrorKind::PermissionDenied => {
                    Some("Check file and directory permissions in the sync folder.".to_string())
                }
                FsErrorKind::DirectoryNotEmpty => Some(
                    "Remove or move remaining files inside the directory and sync again."
                        .to_string(),
                ),
                FsErrorKind::NoSpace => {
                    Some("Free disk space on the sync volume and retry.".to_string())
                }
                FsErrorKind::CrossDevice => Some(
                    "Ensure '.legion-sync/tmp', '.legion-sync/bin', and the sync folder live on the same volume."
                        .to_string(),
                ),
                FsErrorKind::Crash => {
                    Some("Re-open the sync folder so crash recovery completes cleanly.".to_string())
                }
                FsErrorKind::NotFound | FsErrorKind::AlreadyExists | FsErrorKind::Other => None,
            },
        }
    }

    /// Converts this error into the protocol [`ErrorBody`] wire shape.
    pub fn to_error_body(&self) -> ErrorBody {
        let mut body = ErrorBody::new(self.code(), self.to_string());
        if let Some(hint) = self.hint() {
            body = body.with_hint(hint);
        }
        body
    }

    /// Returns `true` if this error represents an injected simulated crash.
    pub fn is_crash(&self) -> bool {
        matches!(
            self,
            Self::Fs {
                kind: FsErrorKind::Crash,
                ..
            }
        )
    }

    /// Classifies a standard `std::io::Error` into [`SyncError::Fs`].
    pub fn from_io(path: Option<String>, err: std::io::Error) -> Self {
        let kind = classify_io_error(&err);
        Self::Fs {
            path,
            detail: err.to_string(),
            kind,
        }
    }

    /// Converts a [`legion_os::OsError`] into [`SyncError::Fs`].
    pub fn from_os(path: Option<String>, err: legion_os::OsError) -> Self {
        let kind = match &err {
            legion_os::OsError::NotFound { .. } => FsErrorKind::NotFound,
            legion_os::OsError::PermissionDenied { .. } => FsErrorKind::PermissionDenied,
            legion_os::OsError::AlreadyExists { .. } => FsErrorKind::AlreadyExists,
            legion_os::OsError::Io { source, .. } => classify_io_error(source),
            _ => FsErrorKind::Other,
        };
        Self::Fs {
            path,
            detail: err.to_string(),
            kind,
        }
    }
}

fn classify_io_error(err: &std::io::Error) -> FsErrorKind {
    use std::io::ErrorKind;
    match err.kind() {
        ErrorKind::NotFound => FsErrorKind::NotFound,
        ErrorKind::PermissionDenied => FsErrorKind::PermissionDenied,
        ErrorKind::AlreadyExists => FsErrorKind::AlreadyExists,
        ErrorKind::DirectoryNotEmpty => FsErrorKind::DirectoryNotEmpty,
        ErrorKind::StorageFull => FsErrorKind::NoSpace,
        ErrorKind::CrossesDevices => FsErrorKind::CrossDevice,
        _ => {
            #[cfg(unix)]
            {
                if matches!(err.raw_os_error(), Some(28 | 122)) {
                    // ENOSPC (28) / EDQUOT (122 on Linux)
                    return FsErrorKind::NoSpace;
                }
                if err.raw_os_error() == Some(18) {
                    // EXDEV (18)
                    return FsErrorKind::CrossDevice;
                }
            }
            #[cfg(windows)]
            {
                if matches!(err.raw_os_error(), Some(39 | 112)) {
                    // ERROR_HANDLE_DISK_FULL (39) / ERROR_DISK_FULL (112)
                    return FsErrorKind::NoSpace;
                }
                if err.raw_os_error() == Some(17) {
                    // ERROR_NOT_SAME_DEVICE (17)
                    return FsErrorKind::CrossDevice;
                }
            }
            FsErrorKind::Other
        }
    }
}

impl From<legion_xfer::Error> for SyncError {
    fn from(err: legion_xfer::Error) -> Self {
        match err {
            legion_xfer::Error::InvalidPattern { pattern, source } => Self::InvalidPattern {
                pattern,
                reason: source.to_string(),
            },
            legion_xfer::Error::HashMismatch {
                path,
                expected,
                actual,
            } => Self::HashMismatch {
                path: path.display().to_string(),
                expected,
                actual,
            },
            legion_xfer::Error::NotFound { path, .. } => Self::NotFound {
                what: path.display().to_string(),
            },
            legion_xfer::Error::PermissionDenied { path, source } => Self::Fs {
                path: Some(path.display().to_string()),
                detail: source.to_string(),
                kind: FsErrorKind::PermissionDenied,
            },
            legion_xfer::Error::Exists { path, .. } => Self::Exists {
                path: path.display().to_string(),
            },
            legion_xfer::Error::Conflict { path, reason } => Self::Conflict {
                path: path.display().to_string(),
                reason,
            },
            legion_xfer::Error::Io { path, source } => {
                Self::from_io(path.map(|p| p.display().to_string()), source)
            }
            other => Self::Fs {
                path: None,
                detail: other.to_string(),
                kind: FsErrorKind::Other,
            },
        }
    }
}
