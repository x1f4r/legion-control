//! Shared typed error definitions for `legion-os`.

use std::io;
use std::path::PathBuf;

/// Result alias for fallible operations in `legion-os`.
pub type Result<T> = std::result::Result<T, OsError>;

/// Structured platform error returned by `legion-os`.
///
/// Variants separate conditions callers routinely branch on (missing programs, permission
/// denials, lock contention, deadlines, and platform limitations) from unexpected I/O failures.
#[derive(Debug, thiserror::Error)]
pub enum OsError {
    /// The requested executable was not found on `PATH` or at the specified path.
    #[error("program not found: {program}")]
    ProgramNotFound {
        /// Executable name or path that could not be located.
        program: String,
    },

    /// A filesystem path, working directory, or process could not be found.
    #[error("not found: {detail}")]
    NotFound {
        /// Human-readable description of what was missing.
        detail: String,
    },

    /// The operating system refused the operation due to insufficient permissions.
    #[error("permission denied: {detail}")]
    PermissionDenied {
        /// Context describing the refused operation.
        detail: String,
        /// Underlying OS error, when one was reported.
        #[source]
        source: Option<io::Error>,
    },

    /// An operation did not complete before its deadline elapsed.
    #[error("timed out: {detail}")]
    TimedOut {
        /// Description of what ran out of time.
        detail: String,
    },

    /// A non-blocking lock request could not proceed because another owner holds the lock.
    #[error("lock is held by another owner: {path}")]
    WouldBlock {
        /// Path of the contended lock file.
        path: PathBuf,
    },

    /// The caller provided an invalid argument, such as an empty argv or a directory where a file was required.
    #[error("invalid argument: {detail}")]
    InvalidArgument {
        /// Reason the argument was rejected.
        detail: String,
    },

    /// The requested operation is not supported on this operating system.
    #[error("unsupported on this platform: {detail}")]
    Unsupported {
        /// Capability that is unavailable on the current platform.
        detail: String,
    },

    /// A platform helper or subsystem returned an error that does not map to a standard I/O error.
    #[error("platform error: {detail}")]
    Platform {
        /// Description of the platform failure.
        detail: String,
    },

    /// An unexpected operating system I/O error.
    #[error("{context}: {source}")]
    Io {
        /// Operation that failed.
        context: String,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

#[allow(dead_code)]
impl OsError {
    /// Classify an I/O error returned while spawning a process.
    pub(crate) fn from_spawn_io(program: impl Into<String>, source: io::Error) -> Self {
        let program = program.into();
        match source.kind() {
            io::ErrorKind::NotFound => Self::ProgramNotFound { program },
            io::ErrorKind::PermissionDenied => Self::PermissionDenied {
                detail: format!("cannot execute `{program}`"),
                source: Some(source),
            },
            _ => Self::Io {
                context: format!("failed to spawn `{program}`"),
                source,
            },
        }
    }

    /// Classify a general filesystem or OS I/O error into the closest typed variant.
    pub(crate) fn from_io(context: impl Into<String>, source: io::Error) -> Self {
        let context = context.into();
        match source.kind() {
            io::ErrorKind::NotFound => Self::NotFound {
                detail: format!("{context}: {source}"),
            },
            io::ErrorKind::PermissionDenied => Self::PermissionDenied {
                detail: context,
                source: Some(source),
            },
            io::ErrorKind::TimedOut => Self::TimedOut {
                detail: format!("{context}: {source}"),
            },
            io::ErrorKind::Unsupported => Self::Unsupported {
                detail: format!("{context}: {source}"),
            },
            _ => Self::Io { context, source },
        }
    }
}
