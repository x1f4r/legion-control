//! The one private module in the `fs` family where Unix and Windows file metadata and symlink
//! operations differ.

use std::ffi::OsStr;
use std::fs::Metadata;
use std::io;
use std::path::Path;

#[cfg(unix)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::{self, File, Metadata, OpenOptions, Permissions};
    use std::io;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    pub(super) const IS_WINDOWS: bool = false;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct DeviceId(u64);

    pub(super) fn device_id(_path: &Path, meta: &Metadata) -> Option<DeviceId> {
        Some(DeviceId(meta.dev()))
    }

    pub(super) fn same_device(
        root: &Option<DeviceId>,
        child_path: &Path,
        child_meta: &Metadata,
    ) -> bool {
        match (root, device_id(child_path, child_meta)) {
            (Some(a), Some(b)) => a == &b,
            _ => true,
        }
    }

    pub(super) fn unix_mode(meta: &Metadata) -> Option<String> {
        Some(format!("{:04o}", meta.permissions().mode() & 0o7777))
    }

    pub(super) fn unix_mode_bits(meta: &Metadata) -> Option<u32> {
        Some(meta.permissions().mode() & 0o7777)
    }

    pub(super) fn set_unix_mode(path: &Path, mode: u32) -> io::Result<()> {
        fs::set_permissions(path, Permissions::from_mode(mode))
    }

    pub(super) fn open_append_create_new(path: &Path, mode: Option<u32>) -> io::Result<File> {
        let mut opts = OpenOptions::new();
        opts.write(true).append(true).create_new(true);
        if let Some(bits) = mode {
            opts.mode(bits);
        }
        opts.open(path)
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
    use std::fs::{self, File, Metadata, OpenOptions};
    use std::io;
    use std::os::windows::fs::MetadataExt;
    use std::path::{Component, Path};

    pub(super) const IS_WINDOWS: bool = true;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum DeviceId {
        Volume(String),
        MountPoint,
    }

    pub(super) fn device_id(path: &Path, meta: &Metadata) -> Option<DeviceId> {
        // Directory junctions and NTFS volume mount points carry FILE_ATTRIBUTE_REPARSE_POINT
        // without reporting as symbolic links; treat them as crossing into another mount.
        if (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            && !meta.file_type().is_symlink()
        {
            return Some(DeviceId::MountPoint);
        }
        let resolved = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match resolved.components().next() {
            Some(Component::Prefix(prefix)) => Some(DeviceId::Volume(
                prefix.as_os_str().to_string_lossy().to_lowercase(),
            )),
            _ => None,
        }
    }

    pub(super) fn same_device(
        root: &Option<DeviceId>,
        child_path: &Path,
        child_meta: &Metadata,
    ) -> bool {
        match (root, device_id(child_path, child_meta)) {
            (_, Some(DeviceId::MountPoint)) => false,
            (Some(a), Some(b)) => a == &b,
            _ => true,
        }
    }

    pub(super) fn unix_mode(_meta: &Metadata) -> Option<String> {
        None
    }

    pub(super) fn unix_mode_bits(_meta: &Metadata) -> Option<u32> {
        None
    }

    pub(super) fn set_unix_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn open_append_create_new(path: &Path, _mode: Option<u32>) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .append(true)
            .create_new(true)
            .open(path)
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
    use std::fs::{self, File, Metadata, OpenOptions};
    use std::io;
    use std::path::Path;

    pub(super) const IS_WINDOWS: bool = false;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct DeviceId;

    pub(super) fn device_id(_path: &Path, _meta: &Metadata) -> Option<DeviceId> {
        None
    }

    pub(super) fn same_device(
        _root: &Option<DeviceId>,
        _child_path: &Path,
        _child_meta: &Metadata,
    ) -> bool {
        true
    }

    pub(super) fn unix_mode(_meta: &Metadata) -> Option<String> {
        None
    }

    pub(super) fn unix_mode_bits(_meta: &Metadata) -> Option<u32> {
        None
    }

    pub(super) fn set_unix_mode(_path: &Path, _mode: u32) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn open_append_create_new(path: &Path, _mode: Option<u32>) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .append(true)
            .create_new(true)
            .open(path)
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

/// Whether the target platform uses Windows path semantics.
pub(crate) const IS_WINDOWS: bool = imp::IS_WINDOWS;

/// Formats Unix permission bits as a four-digit octal string, or `None` on Windows.
pub(crate) fn unix_mode(meta: &Metadata) -> Option<String> {
    imp::unix_mode(meta)
}

/// Returns Unix permission bits (`0..=0o7777`), or `None` on Windows.
pub(crate) fn unix_mode_bits(meta: &Metadata) -> Option<u32> {
    imp::unix_mode_bits(meta)
}

/// Applies Unix permission bits to `path`, doing nothing on Windows.
pub(crate) fn set_unix_mode(path: &Path, mode: u32) -> io::Result<()> {
    imp::set_unix_mode(path, mode)
}

/// Atomically creates a new file opened in append mode, applying `mode` on Unix if given.
pub(crate) fn open_append_create_new(path: &Path, mode: Option<u32>) -> io::Result<std::fs::File> {
    imp::open_append_create_new(path, mode)
}

/// On Windows, whether a symbolic link's metadata marks it as a directory symlink.
pub(crate) fn symlink_is_dir(meta: &Metadata) -> bool {
    imp::symlink_is_dir(meta)
}

/// Creates a symbolic link at `link` pointing to `target`.
pub(crate) fn create_symlink(target: &Path, link: &Path, is_dir: bool) -> io::Result<()> {
    imp::create_symlink(target, link, is_dir)
}

/// Removes a symbolic link without touching its target.
pub(crate) fn remove_symlink(path: &Path, meta: &Metadata) -> io::Result<()> {
    imp::remove_symlink(path, meta)
}

/// Whether OS file attributes (beyond a leading dot) mark `meta` as hidden.
pub(crate) fn is_hidden_attr(meta: &Metadata) -> bool {
    imp::is_hidden_attr(meta)
}

/// Compares two path components using the platform's filename case rules.
pub(crate) fn path_component_eq(a: &OsStr, b: &OsStr) -> bool {
    imp::path_component_eq(a, b)
}

/// Opaque identifier of the file system or volume holding a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeviceId(imp::DeviceId);

/// Returns the device/volume identifier for `path` (from `meta`), when available on this platform.
pub(crate) fn device_id(path: &Path, meta: &Metadata) -> Option<DeviceId> {
    imp::device_id(path, meta).map(DeviceId)
}

/// Whether `child_path` lives on the same file system as `root`.
pub(crate) fn same_device(
    root: &Option<DeviceId>,
    child_path: &Path,
    child_meta: &Metadata,
) -> bool {
    let inner = root.as_ref().map(|d| d.0.clone());
    imp::same_device(&inner, child_path, child_meta)
}
