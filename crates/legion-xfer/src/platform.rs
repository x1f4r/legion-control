//! Platform-specific filesystem hooks isolated behind uniform signatures.

use std::fs;

/// Returns Unix permission bits (`0o7777` mask) on Unix and `None` on non-Unix platforms.
#[cfg(unix)]
pub(crate) fn entry_mode(meta: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

/// Returns `None` on non-Unix platforms where POSIX mode bits do not exist.
#[cfg(not(unix))]
pub(crate) fn entry_mode(_meta: &fs::Metadata) -> Option<u32> {
    None
}
