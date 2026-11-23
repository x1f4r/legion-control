//! Platform-specific filesystem hooks isolated behind uniform signatures.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, classify_io};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Returns a crate-unique monotonic counter value for temporary sibling filenames.
pub(crate) fn next_temp_id() -> u64 {
    TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Replaces `to` with `from` using a single rename call.
///
/// Centralized here so a future shared retrying rename (needed on Windows when indexers hold a
/// target briefly) can replace this body in one place.
pub(crate) fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|e| classify_io(to, e))
}

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

/// Applies POSIX permission bits on Unix; no-op on non-Unix platforms.
#[cfg(unix)]
pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| classify_io(path, e))
}

/// No-op on platforms without POSIX permission bits.
#[cfg(not(unix))]
pub(crate) fn set_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

/// Creates or atomically replaces a symbolic link on Unix, returning `Ok(true)`.
///
/// Creating the symlink at a sibling temporary name and renaming prevents following an existing
/// symlink at `dest` and never leaves a window where `dest` is missing.
#[cfg(unix)]
pub(crate) fn create_symlink_atomic(target: &Path, dest: &Path) -> Result<bool> {
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let pid = std::process::id();
    loop {
        let seq = next_temp_id();
        let tmp = parent.join(format!(".legion-link-{pid}-{seq}.tmp"));
        match std::os::unix::fs::symlink(target, &tmp) {
            Ok(()) => {
                if let Err(err) = replace_file(&tmp, dest) {
                    let _ = fs::remove_file(&tmp);
                    return Err(err);
                }
                return Ok(true);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(classify_io(&tmp, err)),
        }
    }
}

/// Skips symlink creation on Windows, where creating symlinks normally requires elevation.
#[cfg(not(unix))]
pub(crate) fn create_symlink_atomic(_target: &Path, _dest: &Path) -> Result<bool> {
    Ok(false)
}
