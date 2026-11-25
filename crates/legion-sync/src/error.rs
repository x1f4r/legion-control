//! Structured errors for the folder synchronization engine.
//!
//! Every error maps to a stable [`legion_proto::ErrorCode`], a plain-language sentence, and an
//! actionable hint so callers and future CLI/MCP/HTTP tools can report failures consistently.

use legion_proto::{ErrorBody, ErrorCode};
use legion_xfer::Hash;

/// Convenience result alias for sync engine operations.
pub type Result<T> = std::result::Result<T, SyncError>;

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

    /// Operation conflicted with current state (for example a paused folder or mismatched state).
    #[error("conflict at {path}: {reason}")]
    Conflict {
        /// Relative path or folder identifier.
        path: String,
        /// Explanation of the conflict.
        reason: String,
    },

    /// Operation is unsupported on this platform or filesystem.
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

    /// Folder syncing is currently paused.
    #[error("sync folder {folder_id:?} is paused")]
    Paused {
        /// Folder identifier.
        folder_id: String,
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
            Self::Unsupported { .. } => ErrorCode::Unsupported,
            Self::PeerUnavailable { .. } => ErrorCode::Unreachable,
            Self::Storage { .. } | Self::CorruptData { .. } => ErrorCode::Internal,
            Self::Fs { kind, .. } => match kind {
                FsErrorKind::NotFound => ErrorCode::NotFound,
                FsErrorKind::PermissionDenied => ErrorCode::Denied,
                FsErrorKind::AlreadyExists => ErrorCode::Exists,
                FsErrorKind::DirectoryNotEmpty => ErrorCode::Conflict,
                FsErrorKind::Crash | FsErrorKind::Other => ErrorCode::Failed,
            },
        }
    }

    /// Returns an actionable hint for a person or agent dealing with this error.
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::InvalidConfig { hint, .. } | Self::Unsupported { hint, .. } => hint.clone(),
            Self::InvalidPath { .. } => Some(
                "Use a relative path inside the sync folder without leading slashes, '..' segments, or '.legion-sync'."
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
                "Choose a different target path or enable overwrite or rename when restoring."
                    .to_string(),
            ),
            Self::HashMismatch { .. } | Self::SizeMismatch { .. } => Some(
                "The file may have changed on the peer while transferring; retry syncing."
                    .to_string(),
            ),
            Self::Conflict { .. } => {
                Some("Inspect the folder status and resolve any conflicts before retrying.".to_string())
            }
            Self::PeerUnavailable { .. } => {
                Some("Wait for the peer machine to come back online and sync again.".to_string())
            }
            Self::Paused { .. } => {
                Some("Resume the sync folder before running a sync.".to_string())
            }
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
                FsErrorKind::Crash => {
                    Some("Restart the sync engine to run crash recovery.".to_string())
                }
                _ => None,
            },
        }
    }

    /// Converts this error into Legion's standard wire [`ErrorBody`].
    pub fn to_error_body(&self) -> ErrorBody {
        let body = ErrorBody::new(self.code(), self.to_string());
        match self.hint() {
            Some(hint) => body.with_hint(hint),
            None => body,
        }
    }

    /// Returns `true` if this error represents an injected or simulated crash fault.
    pub fn is_crash(&self) -> bool {
        matches!(
            self,
            Self::Fs {
                kind: FsErrorKind::Crash,
                ..
            }
        )
    }

    /// Classifies a standard [`std::io::Error`] tied to an optional relative path.
    pub fn from_io(path: Option<String>, err: std::io::Error) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::NotFound => FsErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => FsErrorKind::PermissionDenied,
            std::io::ErrorKind::AlreadyExists => FsErrorKind::AlreadyExists,
            std::io::ErrorKind::DirectoryNotEmpty => FsErrorKind::DirectoryNotEmpty,
            _ => FsErrorKind::Other,
        };
        Self::Fs {
            path,
            detail: err.to_string(),
            kind,
        }
    }
}

impl From<legion_xfer::Error> for SyncError {
    fn from(err: legion_xfer::Error) -> Self {
        match err {
            legion_xfer::Error::NotFound { path, source } => Self::Fs {
                path: Some(path.display().to_string()),
                detail: source.to_string(),
                kind: FsErrorKind::NotFound,
            },
            legion_xfer::Error::PermissionDenied { path, source } => Self::Fs {
                path: Some(path.display().to_string()),
                detail: source.to_string(),
                kind: FsErrorKind::PermissionDenied,
            },
            legion_xfer::Error::Exists { path, source } => Self::Fs {
                path: Some(path.display().to_string()),
                detail: source
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "path already exists".to_string()),
                kind: FsErrorKind::AlreadyExists,
            },
            legion_xfer::Error::Conflict { path, reason } => Self::Conflict {
                path: path.display().to_string(),
                reason,
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
            legion_xfer::Error::UnsafeArchiveEntry { entry, reason } => Self::InvalidPath {
                path: entry,
                reason,
            },
            legion_xfer::Error::InvalidPattern { pattern, source } => Self::InvalidPattern {
                pattern,
                reason: source.to_string(),
            },
            legion_xfer::Error::InvalidHash { input, reason } => Self::CorruptData {
                context: format!("hash {input:?}"),
                detail: reason,
            },
            legion_xfer::Error::BadOffset {
                path,
                requested,
                actual,
            } => Self::Conflict {
                path: path.display().to_string(),
                reason: format!("requested offset {requested}, actual length {actual}"),
            },
            legion_xfer::Error::Io { path, source } => {
                Self::from_io(path.map(|p| p.display().to_string()), source)
            }
        }
    }
}
