//! The one private module in the `job` family for filesystem permissions that `legion-os` does
//! not expose directly: creating user-private directories (`0o700` on Unix) and opening private
//! append-only log files (`0o600` on Unix).

use std::fs::File;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    use std::fs::{self, DirBuilder, File, OpenOptions, Permissions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    const DIR_MODE: u32 = 0o700;
    const FILE_MODE: u32 = 0o600;

    pub(super) fn ensure_private_dir(path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        match DirBuilder::new().mode(DIR_MODE).create(path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs::metadata(path)?;
                if !meta.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a directory", path.display()),
                    ));
                }
            }
            Err(err) => return Err(err),
        }
        // Apply explicitly so an existing directory or restrictive umask still lands on 0700.
        fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
    }

    pub(super) fn create_private_dir_new(path: &Path) -> io::Result<()> {
        DirBuilder::new().mode(DIR_MODE).create(path)?;
        fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
    }

    pub(super) fn open_private_append(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(FILE_MODE)
            .open(path)
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::Path;

    pub(super) fn ensure_private_dir(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path)
    }

    pub(super) fn create_private_dir_new(path: &Path) -> io::Result<()> {
        fs::create_dir(path)
    }

    pub(super) fn open_private_append(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }
}

/// Ensures `path` exists as a directory private to the current user (`0o700` on Unix).
pub(super) fn ensure_private_dir(path: &Path) -> io::Result<()> {
    imp::ensure_private_dir(path)
}

/// Creates a brand-new user-private directory (`0o700` on Unix), failing with `AlreadyExists`
/// when `path` is already present.
pub(super) fn create_private_dir_new(path: &Path) -> io::Result<()> {
    imp::create_private_dir_new(path)
}

/// Opens `path` for appending, creating it with owner-only permissions (`0o600` on Unix) if
/// absent.
pub(super) fn open_private_append(path: &Path) -> io::Result<File> {
    imp::open_private_append(path)
}
