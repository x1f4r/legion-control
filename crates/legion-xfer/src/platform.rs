//! Platform-specific filesystem hooks isolated behind uniform signatures.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use legion_os::atomic::{
    FileIdentity, RenameNoReplaceError, file_identity_from_file,
    rename_no_replace as os_rename_no_replace, replace_file as os_replace_file,
    symlink_file_identity as os_symlink_file_identity,
};

use crate::error::{Result, classify_io};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static DIR_SYNC_CALLS: AtomicU64 = AtomicU64::new(0);

/// Returns the number of [`sync_dir`] calls made in this test process.
#[cfg(test)]
pub(crate) fn dir_sync_calls() -> u64 {
    DIR_SYNC_CALLS.load(Ordering::Relaxed)
}

/// Returns a crate-unique monotonic counter value for temporary sibling filenames.
pub(crate) fn next_temp_id() -> u64 {
    TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Returns `true` when `name` matches one of this crate's own temporary file patterns.
pub(crate) fn is_internal_temp_name(name: &str) -> bool {
    name.ends_with(".legion-part")
        || ((name.starts_with(".legion-unpack-")
            || name.starts_with(".legion-link-")
            || name.starts_with(".legion-CaseProbe-")
            || name.starts_with(".legion-move-probe-")
            || name.starts_with(".legion-rename-"))
            && name.ends_with(".tmp"))
}

/// Replaces `to` with `from` using the shared platform-aware atomic rename (including Windows
/// sharing-violation retries).
pub(crate) fn replace_file(from: &Path, to: &Path) -> Result<()> {
    os_replace_file(from, to).map_err(|e| match e {
        legion_os::OsError::Io { source, .. } => classify_io(to, source),
        other => classify_io(to, io::Error::other(other.to_string())),
    })
}

/// Atomically renames `from` to `to` only when `to` does not exist.
pub(crate) fn rename_no_replace(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), RenameNoReplaceError> {
    os_rename_no_replace(from, to)
}

/// Returns the filesystem identity `(dev, ino, nlink)` of `path` without following symlinks.
pub(crate) fn symlink_file_identity(path: &Path) -> io::Result<FileIdentity> {
    os_symlink_file_identity(path)
}

/// Returns the filesystem identity `(dev, ino, nlink)` of an open [`File`].
pub(crate) fn open_file_identity(file: &File) -> io::Result<FileIdentity> {
    file_identity_from_file(file)
}

/// Flushes directory metadata to durable storage on Unix after a rename or entry creation.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(test)]
    DIR_SYNC_CALLS.fetch_add(1, Ordering::Relaxed);
    let handle = fs::File::open(dir).map_err(|e| classify_io(dir, e))?;
    handle.sync_all().map_err(|e| classify_io(dir, e))
}

/// No-op on non-Unix platforms where directories cannot be synced via `File::open`.
#[cfg(not(unix))]
pub(crate) fn sync_dir(_dir: &Path) -> Result<()> {
    #[cfg(test)]
    DIR_SYNC_CALLS.fetch_add(1, Ordering::Relaxed);
    Ok(())
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

/// Creates a brand-new file with mode `0o600` on Unix so in-flight partial data is private.
#[cfg(unix)]
pub(crate) fn create_new_private(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Creates a brand-new file on non-Unix platforms.
#[cfg(not(unix))]
pub(crate) fn create_new_private(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
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

/// Creates a symbolic link on Unix without replacing an existing entry, returning `Ok(true)`.
///
/// Fails with [`io::ErrorKind::AlreadyExists`] atomically if `dest` already exists.
#[cfg(unix)]
pub(crate) fn create_symlink_no_replace(target: &Path, dest: &Path) -> io::Result<bool> {
    std::os::unix::fs::symlink(target, dest).map(|()| true)
}

/// Skips symlink creation on Windows, where creating symlinks normally requires elevation.
#[cfg(not(unix))]
pub(crate) fn create_symlink_no_replace(_target: &Path, _dest: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Skips symlink creation on Windows, where creating symlinks normally requires elevation.
#[cfg(not(unix))]
pub(crate) fn create_symlink_atomic(_target: &Path, _dest: &Path) -> Result<bool> {
    Ok(false)
}
