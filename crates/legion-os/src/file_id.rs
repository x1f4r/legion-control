//! Stable per-file identity and metadata change timestamp (`ctime` / NTFS `ChangeTime`).
//!
//! Used by caches and TOCTOU checks to tell two distinct files apart even when they live at the
//! same path with identical byte length and modification time.

use std::fs::File;
use std::path::Path;

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

/// Reads the identity and change time for `path` (following symbolic links).
pub fn from_path(path: &Path) -> Option<FileId> {
    imp::from_path(path)
}

#[cfg(unix)]
mod imp {
    use std::fs::{self, File, Metadata};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use super::FileId;

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
        fs::metadata(path).ok().map(|meta| from_meta(&meta))
    }
}

#[cfg(windows)]
mod imp {
    use std::fs::File;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx,
    };

    use super::FileId;

    pub(super) fn from_file(file: &File) -> Option<FileId> {
        let handle = HANDLE(file.as_raw_handle());
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `handle` is borrowed from a live `std::fs::File` for the duration of the call,
        // and `info` is a properly aligned, mutable `BY_HANDLE_FILE_INFORMATION` output buffer.
        #[allow(unsafe_code)]
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) }.is_ok();
        if !ok {
            return None;
        }
        let volume = u64::from(info.dwVolumeSerialNumber);
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);

        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: `handle` is borrowed from a live `std::fs::File`, `basic` is a properly aligned
        // `FILE_BASIC_INFO` output buffer, and its byte length matches `size_of::<FILE_BASIC_INFO>()`.
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

    pub(super) fn from_path(path: &Path) -> Option<FileId> {
        File::open(path).ok().and_then(|file| from_file(&file))
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::File;
    use std::path::Path;

    use super::FileId;

    pub(super) fn from_file(_file: &File) -> Option<FileId> {
        None
    }

    pub(super) fn from_path(_path: &Path) -> Option<FileId> {
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
