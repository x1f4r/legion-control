//! The one private module in the `fs` family where Unix and Windows file metadata and symlink
//! operations differ.

use std::ffi::OsStr;
use std::fs::Metadata;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::{self, Metadata, Permissions};
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    pub(super) fn unix_mode(meta: &Metadata) -> Option<String> {
        Some(format!("{:04o}", meta.permissions().mode() & 0o7777))
    }

    pub(super) fn unix_mode_bits(meta: &Metadata) -> Option<u32> {
        Some(meta.permissions().mode() & 0o7777)
    }

    pub(super) fn set_unix_mode(path: &Path, mode: u32) -> io::Result<()> {
        fs::set_permissions(path, Permissions::from_mode(mode))
    }

    pub(super) fn symlink_is_dir(_meta: &Metadata) -> bool {
        false
    }

    pub(super) fn create_symlink(target: &Path, link: &Path, _is_dir: bool) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    pub(super) fn remove_symlink(path: &Path, _meta: &Metadata) -> io::Result<()> {
        fs::remove_file(path)
    }

    pub(super) fn is_hidden_attr(_meta: &Metadata) -> bool {
        false
    }

    pub(super) fn path_component_eq(a: &OsStr, b: &OsStr) -> bool {
        a == b
    }
}

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::{self, Metadata};
    use std::io;
    use std::os::windows::fs::MetadataExt;
    use std::path::Path;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

    pub(super) fn unix_mode(_meta: &Metadata) -> Option<String> {
        None
    }

    pub(super) fn unix_mode_bits(_meta: &Metadata) -> Option<u32> {
        None
    }

    pub(super) fn set_unix_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn symlink_is_dir(meta: &Metadata) -> bool {
        (meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) != 0
    }

    pub(super) fn create_symlink(target: &Path, link: &Path, is_dir: bool) -> io::Result<()> {
        if is_dir {
            std::os::windows::fs::symlink_dir(target, link)
        } else {
            std::os::windows::fs::symlink_file(target, link)
        }
    }

    pub(super) fn remove_symlink(path: &Path, meta: &Metadata) -> io::Result<()> {
        if symlink_is_dir(meta) {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    }

    pub(super) fn is_hidden_attr(meta: &Metadata) -> bool {
        (meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN) != 0
    }

    pub(super) fn path_component_eq(a: &OsStr, b: &OsStr) -> bool {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::ffi::OsStr;
    use std::fs::{self, Metadata};
    use std::io;
    use std::path::Path;

    pub(super) fn unix_mode(_meta: &Metadata) -> Option<String> {
        None
    }

    pub(super) fn unix_mode_bits(_meta: &Metadata) -> Option<u32> {
        None
    }

    pub(super) fn set_unix_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn symlink_is_dir(_meta: &Metadata) -> bool {
        false
    }

    pub(super) fn create_symlink(_target: &Path, _link: &Path, _is_dir: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "creating symbolic links is not supported on this platform",
        ))
    }

    pub(super) fn remove_symlink(path: &Path, _meta: &Metadata) -> io::Result<()> {
        fs::remove_file(path)
    }

    pub(super) fn is_hidden_attr(_meta: &Metadata) -> bool {
        false
    }

    pub(super) fn path_component_eq(a: &OsStr, b: &OsStr) -> bool {
        a == b
    }
}

/// Formats Unix permission bits as a four-digit octal string, or `None` on Windows.
pub(crate) fn unix_mode(meta: &Metadata) -> Option<String> {
    imp::unix_mode(meta)
}

/// Returns Unix permission bits (`0..=0o7777`), or `None` on Windows.
#[allow(dead_code)]
pub(crate) fn unix_mode_bits(meta: &Metadata) -> Option<u32> {
    imp::unix_mode_bits(meta)
}

/// Applies Unix permission bits to `path`, doing nothing on Windows.
#[allow(dead_code)]
pub(crate) fn set_unix_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::set_unix_mode(path, mode)
}

/// On Windows, whether a symbolic link's metadata marks it as a directory symlink.
#[allow(dead_code)]
pub(crate) fn symlink_is_dir(meta: &Metadata) -> bool {
    imp::symlink_is_dir(meta)
}

/// Creates a symbolic link at `link` pointing to `target`.
#[allow(dead_code)]
pub(crate) fn create_symlink(target: &Path, link: &Path, is_dir: bool) -> io::Result<()> {
    imp::create_symlink(target, link, is_dir)
}

/// Removes a symbolic link without touching its target.
#[allow(dead_code)]
pub(crate) fn remove_symlink(path: &Path, meta: &Metadata) -> io::Result<()> {
    imp::remove_symlink(path, meta)
}

/// Whether OS file attributes (beyond a leading dot) mark `meta` as hidden.
pub(crate) fn is_hidden_attr(meta: &Metadata) -> bool {
    imp::is_hidden_attr(meta)
}

/// Compares two path components using the platform's filename case rules.
#[allow(dead_code)]
pub(crate) fn path_component_eq(a: &OsStr, b: &OsStr) -> bool {
    imp::path_component_eq(a, b)
}
