//! Stable per-file identity and metadata change timestamp (`ctime` / NTFS `ChangeTime`).
//!
//! Used by policy checks, caches, and TOCTOU checks to tell two distinct files apart without ever
//! performing a blocking open on FIFOs, devices, or sockets.

use std::fs::File;
use std::path::Path;

/// Stable non-blocking filesystem identity (`(dev, ino)` on Unix, `(volume, file_index)` on
/// Windows) obtained without opening a FIFO, device, or blocking data stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PathIdentity {
    /// Device number (`st_dev`) on Unix, or volume serial number on Windows.
    pub volume: u64,
    /// Inode number (`st_ino`) on Unix, or 64-bit NTFS/ReFS file index on Windows.
    pub index: u64,
}

impl PathIdentity {
    /// Queries the filesystem identity of `path` (following symbolic links) without performing a
    /// blocking open on FIFOs, devices, or sockets.
    pub fn from_path(path: &Path) -> Option<PathIdentity> {
        imp::path_identity(path)
    }
}

/// Stable filesystem identity and metadata change time of an open regular file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileId {
    /// Device number (`st_dev`) on Unix, or volume serial number on Windows.
    pub volume: u64,
    /// Inode number (`st_ino`) on Unix, or 64-bit NTFS/ReFS file index on Windows.
    pub index: u64,
    /// Metadata change timestamp: Unix `ctime` in nanoseconds since epoch, or Windows
    /// `FILE_BASIC_INFO::ChangeTime` in 100-nanosecond ticks since 1601, when available.
    pub change_time: Option<i128>,
}

/// Reads the identity and change time from an already-open [`File`] handle.
pub fn from_file(file: &File) -> Option<FileId> {
    imp::from_file(file)
}

/// Reads the identity and change time for `path` (following symbolic links) without blocking on
/// FIFOs, devices, or sockets.
pub fn from_path(path: &Path) -> Option<FileId> {
    imp::from_path(path)
}

/// Queries the non-blocking [`PathIdentity`] of `path` (following symbolic links).
pub fn path_identity(path: &Path) -> Option<PathIdentity> {
    imp::path_identity(path)
}

#[cfg(unix)]
mod imp {
    use std::fs::{self, File, Metadata};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::{FileId, PathIdentity};

    fn from_meta(meta: &Metadata) -> FileId {
        let change_time = i128::from(meta.ctime())
            .saturating_mul(1_000_000_000)
            .saturating_add(i128::from(meta.ctime_nsec()));
        FileId {
            volume: meta.dev(),
            index: meta.ino(),
            change_time: Some(change_time),
        }
    }

    pub(super) fn from_file(file: &File) -> Option<FileId> {
        file.metadata().ok().map(|meta| from_meta(&meta))
    }

    pub(super) fn from_path(path: &Path) -> Option<FileId> {
        // `stat(2)` inspects metadata without opening the file, so it never blocks on FIFOs or
        // triggers device opens.
        fs::metadata(path).ok().map(|meta| from_meta(&meta))
    }

    pub(super) fn path_identity(path: &Path) -> Option<PathIdentity> {
        fs::metadata(path).ok().map(|meta| PathIdentity {
            volume: meta.dev(),
            index: meta.ino(),
        })
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    use super::{FileId, PathIdentity};

    struct MetadataHandle(HANDLE);

    impl Drop for MetadataHandle {
        fn drop(&mut self) {
            if self.0 != INVALID_HANDLE_VALUE && !self.0.is_invalid() {
                // SAFETY: `self.0` is an owned Win32 handle obtained from `CreateFileW`.
                #[allow(unsafe_code)]
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    /// Opens a metadata-only handle (`dwDesiredAccess = 0`) with `FILE_FLAG_BACKUP_SEMANTICS` so
    /// directories and files can be queried without opening a data stream or triggering sharing
    /// conflicts.
    fn open_metadata_handle(path: &Path) -> Option<MetadataHandle> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is NUL-terminated for the duration of the call; `dwDesiredAccess = 0`
        // requests metadata query only without reading or writing the underlying object.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .ok()?;
        if handle == INVALID_HANDLE_VALUE || handle.is_invalid() {
            return None;
        }
        Some(MetadataHandle(handle))
    }

    fn from_raw_handle(handle: HANDLE) -> Option<FileId> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `handle` is valid for the duration of the call and `info` is properly aligned.
        #[allow(unsafe_code)]
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) }.is_ok();
        if !ok {
            return None;
        }
        let volume = u64::from(info.dwVolumeSerialNumber);
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);

        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: `handle` is valid and `basic` is properly aligned with matching byte length.
        #[allow(unsafe_code)]
        let change_time = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&raw mut basic).cast(),
                u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap_or(0),
            )
        }
        .ok()
        .map(|()| i128::from(basic.ChangeTime));

        Some(FileId {
            volume,
            index,
            change_time,
        })
    }

    pub(super) fn from_file(file: &File) -> Option<FileId> {
        from_raw_handle(HANDLE(file.as_raw_handle()))
    }

    pub(super) fn from_path(path: &Path) -> Option<FileId> {
        let handle = open_metadata_handle(path)?;
        from_raw_handle(handle.0)
    }

    pub(super) fn path_identity(path: &Path) -> Option<PathIdentity> {
        let handle = open_metadata_handle(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `handle.0` is valid for the duration of the call and `info` is properly aligned.
        #[allow(unsafe_code)]
        let ok = unsafe { GetFileInformationByHandle(handle.0, &mut info) }.is_ok();
        if !ok {
            return None;
        }
        let volume = u64::from(info.dwVolumeSerialNumber);
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        Some(PathIdentity { volume, index })
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::File;
    use std::path::Path;

    use super::{FileId, PathIdentity};

    pub(super) fn from_file(_file: &File) -> Option<FileId> {
        None
    }

    pub(super) fn from_path(_path: &Path) -> Option<FileId> {
        None
    }

    pub(super) fn path_identity(_path: &Path) -> Option<PathIdentity> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_same_file_and_changes_on_rename_replacement() {
        let dir = tempfile::tempdir().ok();
        let Some(dir) = dir else {
            return;
        };
        let target = dir.path().join("photo.png");
        let replacement = dir.path().join("replacement.png");
        assert!(std::fs::write(&target, b"12345678").is_ok());
        assert!(std::fs::write(&replacement, b"87654321").is_ok());

        let f1 = File::open(&target).ok();
        let id1 = f1.as_ref().and_then(from_file);
        let id1_path = from_path(&target);
        assert!(id1.is_some());
        assert_eq!(id1, id1_path);
        assert_eq!(
            path_identity(&target),
            id1.map(|id| PathIdentity {
                volume: id.volume,
                index: id.index,
            })
        );
        drop(f1);

        assert!(std::fs::rename(&replacement, &target).is_ok());
        let id2 = from_path(&target);
        assert!(id2.is_some());
        assert_ne!(
            id1.map(|i| (i.volume, i.index)),
            id2.map(|i| (i.volume, i.index))
        );
    }
}
