//! Race-free, non-blocking opening of regular files for reading.
//!
//! Checking `stat(path)` before `File::open(path)` has a TOCTOU window: if a named pipe (FIFO) is
//! renamed into `path` between `stat` and `open`, `File::open` blocks forever waiting for a writer
//! and exhausts the node's blocking thread pool.
//!
//! [`open_regular_read`] eliminates that race on every platform:
//! - **Unix**: opens with `O_NONBLOCK | O_NOCTTY`, checks `fstat` (`File::metadata`) on the open
//!   file descriptor to reject directories, FIFOs, sockets, and devices, then clears `O_NONBLOCK`
//!   so subsequent reads behave normally.
//! - **Windows**: rejects non-disk handles (`GetFileType != FILE_TYPE_DISK`) and directories on
//!   the open handle itself.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

/// Why [`open_regular_read`] refused or failed to open a path.
#[derive(Debug)]
pub enum OpenRegularError {
    /// The path resolved to a directory.
    IsDirectory,
    /// The path resolved to a non-regular file (named pipe/FIFO, socket, or device node).
    NotRegularFile,
    /// Underlying I/O error (`NotFound`, `PermissionDenied`, etc.).
    Io(io::Error),
}

/// Safely opens `path` for reading without ever blocking on a FIFO or acquiring a controlling
/// terminal, verifying on the open handle that it refers to a regular file.
pub fn open_regular_read(path: &Path) -> std::result::Result<(File, Metadata), OpenRegularError> {
    platform::open_regular_read_impl(path)
}

#[cfg(unix)]
mod platform {
    use std::fs::{self, File, Metadata, OpenOptions};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::OpenRegularError;

    pub(super) fn open_regular_read_impl(
        path: &Path,
    ) -> std::result::Result<(File, Metadata), OpenRegularError> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open(path)
        {
            Ok(file) => file,
            Err(err) => {
                // Opening a Unix domain socket returns ENXIO (Linux) or EOPNOTSUPP (macOS/BSD).
                if matches!(err.raw_os_error(), Some(libc::ENXIO | libc::EOPNOTSUPP)) {
                    if let Ok(meta) = fs::metadata(path) {
                        if meta.is_dir() {
                            return Err(OpenRegularError::IsDirectory);
                        }
                        if !meta.is_file() {
                            return Err(OpenRegularError::NotRegularFile);
                        }
                    }
                    return Err(OpenRegularError::NotRegularFile);
                }
                return Err(OpenRegularError::Io(err));
            }
        };

        let meta = file.metadata().map_err(OpenRegularError::Io)?;
        if meta.is_dir() {
            return Err(OpenRegularError::IsDirectory);
        }
        if !meta.is_file() {
            return Err(OpenRegularError::NotRegularFile);
        }

        // Clear O_NONBLOCK now that `fstat` proved this descriptor is a regular file.
        clear_nonblock(&file).map_err(OpenRegularError::Io)?;
        Ok((file, meta))
    }

    #[allow(unsafe_code)]
    fn clear_nonblock(file: &File) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` is a valid open file descriptor borrowed from `file`.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let cleared = flags & !libc::O_NONBLOCK;
        if cleared != flags {
            // SAFETY: `fd` is valid and `cleared` is a valid flag mask returned by `F_GETFL`.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, cleared) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, Metadata, OpenOptions};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_TYPE_DISK, GetFileType,
    };

    use super::OpenRegularError;

    pub(super) fn open_regular_read_impl(
        path: &Path,
    ) -> std::result::Result<(File, Metadata), OpenRegularError> {
        let file = OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)
            .map_err(OpenRegularError::Io)?;

        // SAFETY: `file.as_raw_handle()` is a valid open Win32 file handle borrowed from `file`.
        #[allow(unsafe_code)]
        let file_type = unsafe { GetFileType(HANDLE(file.as_raw_handle())) };
        if file_type != FILE_TYPE_DISK {
            return Err(OpenRegularError::NotRegularFile);
        }

        let meta = file.metadata().map_err(OpenRegularError::Io)?;
        if meta.is_dir() {
            return Err(OpenRegularError::IsDirectory);
        }
        if !meta.is_file() {
            return Err(OpenRegularError::NotRegularFile);
        }

        Ok((file, meta))
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::fs::{File, Metadata};
    use std::path::Path;

    use super::OpenRegularError;

    pub(super) fn open_regular_read_impl(
        path: &Path,
    ) -> std::result::Result<(File, Metadata), OpenRegularError> {
        let file = File::open(path).map_err(OpenRegularError::Io)?;
        let meta = file.metadata().map_err(OpenRegularError::Io)?;
        if meta.is_dir() {
            return Err(OpenRegularError::IsDirectory);
        }
        if !meta.is_file() {
            return Err(OpenRegularError::NotRegularFile);
        }
        Ok((file, meta))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Read as _;

    use super::*;

    #[test]
    fn opens_regular_file_and_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("hello.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let (mut file, meta) = open_regular_read(&file_path).unwrap();
        assert_eq!(meta.len(), 11);
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "hello world");

        assert!(matches!(
            open_regular_read(dir.path()),
            Err(OpenRegularError::IsDirectory)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_fifo_socket_and_device_without_blocking() {
        use std::os::unix::net::UnixListener;
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.fifo");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        assert!(matches!(
            open_regular_read(&fifo),
            Err(OpenRegularError::NotRegularFile)
        ));

        let sock = dir.path().join("sock.unix");
        let _listener = UnixListener::bind(&sock).unwrap();
        assert!(matches!(
            open_regular_read(&sock),
            Err(OpenRegularError::NotRegularFile)
        ));

        assert!(matches!(
            open_regular_read(Path::new("/dev/zero")),
            Err(OpenRegularError::NotRegularFile)
        ));
    }
}
