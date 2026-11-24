//! Unix implementation of atomic file writes and replacement.

use std::fs::{self, File, Metadata, OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::{OsError, Result};

use super::{ExclusiveRenameError, WriteOptions};

pub(super) fn configure_temp_open(open: &mut OpenOptions, restrictive: bool) {
    if restrictive {
        open.mode(0o600);
    }
}

pub(super) fn apply_final_permissions(
    file: &File,
    existing_meta: Option<&Metadata>,
    options: WriteOptions,
    target: &Path,
) -> Result<()> {
    let desired = if options.force_mode && options.mode.is_some() {
        options.mode.map(Permissions::from_mode)
    } else if let Some(meta) = existing_meta {
        Some(meta.permissions())
    } else {
        options.mode.map(Permissions::from_mode)
    };

    if let Some(perms) = desired {
        file.set_permissions(perms).map_err(|err| {
            OsError::from_io(
                format!(
                    "cannot set permissions on temporary file for {}",
                    target.display()
                ),
                err,
            )
        })?;
    }
    Ok(())
}

pub(super) fn replace_file(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|err| {
        OsError::from_io(
            format!("cannot rename {} to {}", from.display(), to.display()),
            err,
        )
    })
}

pub(super) async fn replace_file_async(from: &Path, to: &Path) -> Result<()> {
    replace_file(from, to)
}

pub(super) fn rename_exclusive(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    exclusive_rename::rename_noreplace(from, to)
}

pub(super) async fn rename_exclusive_async(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    rename_exclusive(from, to)
}

#[allow(unsafe_code)]
mod exclusive_rename {
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::ExclusiveRenameError;

    fn path_to_cstring(path: &Path) -> std::result::Result<CString, ExclusiveRenameError> {
        CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            ExclusiveRenameError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            ))
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn rename_noreplace(
        from: &Path,
        to: &Path,
    ) -> std::result::Result<(), ExclusiveRenameError> {
        let from_c = path_to_cstring(from)?;
        let to_c = path_to_cstring(to)?;

        // SAFETY: `from_c` and `to_c` are valid NUL-terminated C strings that remain alive for
        // the `SYS_renameat2` syscall, and `RENAME_NOREPLACE` is the Linux kernel ABI flag.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                libc::RENAME_NOREPLACE as libc::c_uint,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EEXIST) => Err(ExclusiveRenameError::AlreadyExists),
            Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP) => {
                Err(ExclusiveRenameError::Unsupported)
            }
            _ => Err(ExclusiveRenameError::Io(err)),
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn rename_noreplace(
        from: &Path,
        to: &Path,
    ) -> std::result::Result<(), ExclusiveRenameError> {
        let from_c = path_to_cstring(from)?;
        let to_c = path_to_cstring(to)?;

        // SAFETY: `from_c` and `to_c` are valid NUL-terminated C strings passed to Darwin's
        // `renamex_np(..., RENAME_EXCL)`.
        let rc = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
        if rc == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EEXIST) => Err(ExclusiveRenameError::AlreadyExists),
            #[allow(unreachable_patterns)]
            Some(libc::ENOTSUP | libc::EOPNOTSUPP | libc::ENOSYS | libc::EINVAL) => {
                Err(ExclusiveRenameError::Unsupported)
            }
            _ => Err(ExclusiveRenameError::Io(err)),
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn rename_noreplace(
        _from: &Path,
        _to: &Path,
    ) -> std::result::Result<(), ExclusiveRenameError> {
        Err(ExclusiveRenameError::Unsupported)
    }
}

pub(super) fn sync_parent_dir(parent: &Path) {
    // Syncing the parent directory ensures the rename entry survives power loss; ignore errors
    // on virtual or network filesystems that reject fsync on directories.
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}
