//! Typed errors callers can branch on without parsing error messages.

use std::io;
use std::path::{Path, PathBuf};

use crate::hash::Hash;

/// Convenience result alias for transfer operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Failure modes surfaced by transfer operations.
///
/// Variants separate actionable conditions (missing paths, mismatched offsets, hostile archives)
/// from generic I/O failures so upper layers can map them to protocol error codes or retry cleanly.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A required file or directory did not exist.
    #[error("path not found: {}", path.display())]
    NotFound {
        /// Path that could not be found.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },

    /// The operating system refused access to a path.
    #[error("permission denied: {}", path.display())]
    PermissionDenied {
        /// Path whose access was refused.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: io::Error,
    },

    /// A target path already existed when exclusivity was required.
    #[error("path already exists: {}", path.display())]
    Exists {
        /// Path that already existed.
        path: PathBuf,
        /// Underlying OS error, when produced by a filesystem call.
        #[source]
        source: Option<io::Error>,
    },

    /// An entry collided with incompatible existing state on disk.
    #[error("conflict at {}: {reason}", path.display())]
    Conflict {
        /// Path where the conflict occurred.
        path: PathBuf,
        /// Explanation of the conflicting state.
        reason: String,
    },

    /// A verified file's digest did not match what the caller expected.
    #[error("hash mismatch for {}: expected {expected}, got {actual}", path.display())]
    HashMismatch {
        /// Target file whose verification failed.
        path: PathBuf,
        /// Digest the caller required.
        expected: Hash,
        /// Digest computed from the received bytes.
        actual: Hash,
    },

    /// A requested stream offset did not match the file or partial file length.
    #[error(
        "bad offset for {}: requested {requested}, actual length is {actual}",
        path.display()
    )]
    BadOffset {
        /// Target or source file path.
        path: PathBuf,
        /// Offset supplied by the caller.
        requested: u64,
        /// Current length on disk so the caller can retry from it.
        actual: u64,
    },

    /// An archive entry tried to escape the destination root or used a disallowed type.
    #[error("unsafe archive entry {entry:?}: {reason}")]
    UnsafeArchiveEntry {
        /// Raw or lossy entry path from the archive.
        entry: String,
        /// Why the entry was rejected.
        reason: String,
    },

    /// A formatted hash string did not match `blake3:<64 hex chars>`.
    #[error("invalid hash {input:?}: {reason}")]
    InvalidHash {
        /// Offending input string.
        input: String,
        /// Why parsing failed.
        reason: String,
    },

    /// A caller-supplied exclude glob failed to compile.
    #[error("invalid exclude pattern {pattern:?}: {source}")]
    InvalidPattern {
        /// Offending glob pattern.
        pattern: String,
        /// Compiler diagnostic from `globset`.
        #[source]
        source: globset::Error,
    },

    /// Any other I/O error on a file or byte stream.
    #[error("{}", format_io_error(path.as_deref(), source))]
    Io {
        /// Associated filesystem path, if the error came from a file operation.
        path: Option<PathBuf>,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },
}

fn format_io_error(path: Option<&Path>, source: &io::Error) -> String {
    match path {
        Some(p) => format!("I/O error at {}: {source}", p.display()),
        None => format!("I/O error: {source}"),
    }
}

/// Maps an [`io::Error`] tied to a filesystem path into the most specific [`Error`] variant.
pub(crate) fn classify_io(path: impl Into<PathBuf>, source: io::Error) -> Error {
    let path = path.into();
    match source.kind() {
        io::ErrorKind::NotFound => Error::NotFound { path, source },
        io::ErrorKind::PermissionDenied => Error::PermissionDenied { path, source },
        io::ErrorKind::AlreadyExists => Error::Exists {
            path,
            source: Some(source),
        },
        _ => Error::Io {
            path: Some(path),
            source,
        },
    }
}

impl From<io::Error> for Error {
    fn from(source: io::Error) -> Self {
        Self::Io { path: None, source }
    }
}
