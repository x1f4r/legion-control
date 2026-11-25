//! Resumable single-file streaming (`get` and `put`) with whole-file verification.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, classify_io};
use crate::hash::{Hash, hash_file};
use crate::manifest::Timestamp;
use crate::path::validate_target_filename;
use crate::platform;

/// Suffix appended to a target filename while a single-file [`put`] is in progress.
pub const PART_SUFFIX: &str = ".legion-part";

/// Fixed copy buffer size so large transfers run in constant memory.
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

/// Summary returned by [`put`] once the full file has been verified and moved into place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PutReport {
    /// Final total size of the file in bytes.
    pub size: u64,
    /// Bytes appended during this call (less than `size` when resumed).
    pub bytes_written: u64,
    /// Verified whole-file BLAKE3 digest.
    pub hash: Hash,
}

/// Computes the sibling partial file path (`<name>.legion-part`) for `target`.
pub fn part_path(target: &Path) -> Result<PathBuf> {
    let file_name = validate_target_filename(target)?;
    let mut name = file_name.to_os_string();
    name.push(PART_SUFFIX);
    Ok(match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    })
}

/// Returns the resumable byte offset for `target`: the partial file's current length, or `0` if no
/// partial file exists yet.
pub fn resume_offset(target: &Path) -> Result<u64> {
    let partial = part_path(target)?;
    match fs::symlink_metadata(&partial) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err(Error::Conflict {
                    path: partial,
                    reason: "partial path exists and is not a regular file".to_string(),
                });
            }
            Ok(meta.len())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(classify_io(&partial, err)),
    }
}

/// Open handle to a source file ready to stream a bounded byte range from a validated offset.
///
/// Splitting open from stream lets callers inspect [`Self::size`], [`Self::length`], and
/// [`Self::mtime`] (captured via `fstat` on the opened descriptor) and emit a response header
/// before writing raw file bytes onto a pipe.
#[derive(Debug)]
pub struct GetStream {
    path: PathBuf,
    file: File,
    size: u64,
    offset: u64,
    length: u64,
    mtime: Timestamp,
    file_id: Option<u64>,
}

/// Failure returned by [`GetStream::stream_to_checked`].
#[derive(Debug)]
pub enum StreamCheckedError<E> {
    /// Underlying file or transfer I/O error.
    Xfer(Error),
    /// Caller's periodic check aborted the stream before completion.
    Aborted(E),
}

impl GetStream {
    /// Opens `path`, verifies on the open handle that it is a regular file (without blocking on
    /// FIFOs) with `offset <= size`, and seeks to `offset`, streaming through the end of the file.
    pub fn open(path: &Path, offset: u64) -> Result<Self> {
        Self::open_range(path, offset, None)
    }

    /// Opens `path` as a regular file (without blocking on FIFOs), validates `offset <= size`,
    /// clamps `length` (when given) to `size - offset`, and seeks to `offset`.
    ///
    /// Returns [`Error::BadOffset`] when `offset > size` and [`Error::Conflict`] when `path` is
    /// not a regular file. A `length` of `Some(0)` with `offset <= size` succeeds and streams zero
    /// bytes, allowing callers to inspect `size` and `mtime` without transferring file data.
    pub fn open_range(path: &Path, offset: u64, length: Option<u64>) -> Result<Self> {
        let (mut file, meta) =
            legion_os::open_regular::open_regular_read(path).map_err(|err| match err {
                legion_os::open_regular::OpenRegularError::IsDirectory
                | legion_os::open_regular::OpenRegularError::NotRegularFile => Error::Conflict {
                    path: path.to_path_buf(),
                    reason: "source path is not a regular file".to_string(),
                },
                legion_os::open_regular::OpenRegularError::Io(io_err) => classify_io(path, io_err),
            })?;

        let size = meta.len();
        if offset > size {
            return Err(Error::BadOffset {
                path: path.to_path_buf(),
                requested: offset,
                actual: size,
            });
        }

        let available = size - offset;
        let actual_length = match length {
            Some(requested) => requested.min(available),
            None => available,
        };
        let mtime = Timestamp::from_metadata(&meta);
        let file_id = platform::file_id(&meta);

        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| classify_io(path, e))?;
        }

        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
            offset,
            length: actual_length,
            mtime,
            file_id,
        })
    }

    /// Returns the file's total byte length captured via `fstat` when opened.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the validated byte offset streaming will start from.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the exact number of bytes [`Self::stream_to`] will emit.
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Returns the file's modification time captured via `fstat` when opened.
    pub fn mtime(&self) -> Timestamp {
        self.mtime
    }

    /// Returns a platform file identity (derived from device and inode on Unix) when available.
    pub fn file_id(&self) -> Option<u64> {
        self.file_id
    }

    /// Returns the exact number of bytes [`Self::stream_to`] will emit.
    pub fn remaining(&self) -> u64 {
        self.length
    }

    /// Streams exactly [`Self::length`] bytes into `writer` and returns the byte count written.
    ///
    /// If the file grows after opening, reads stop at `length` so framed protocols stay aligned. If
    /// the file shrinks before `length` bytes are read, returns [`Error::Conflict`].
    pub fn stream_to<W: Write>(self, writer: W) -> Result<u64> {
        match self.stream_to_checked(writer, |_| Ok::<(), std::convert::Infallible>(())) {
            Ok(written) => Ok(written),
            Err(StreamCheckedError::Xfer(err)) => Err(err),
            Err(StreamCheckedError::Aborted(never)) => match never {},
        }
    }

    /// Streams [`Self::length`] bytes into `writer`, invoking `check(bytes_written)` before each
    /// chunk after the first so callers can re-verify authorization or cancellation during long
    /// reads and stop streaming immediately on failure.
    pub fn stream_to_checked<W, E, F>(
        self,
        mut writer: W,
        mut check: F,
    ) -> std::result::Result<u64, StreamCheckedError<E>>
    where
        W: Write,
        F: FnMut(u64) -> std::result::Result<(), E>,
    {
        let remaining = self.length;
        if remaining == 0 {
            writer
                .flush()
                .map_err(|e| StreamCheckedError::Xfer(e.into()))?;
            return Ok(0);
        }
        let mut limited = self.file.take(remaining);
        let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
        let mut written = 0u64;

        loop {
            if written > 0 {
                check(written).map_err(StreamCheckedError::Aborted)?;
            }
            let n = limited
                .read(&mut buf)
                .map_err(|e| StreamCheckedError::Xfer(classify_io(&self.path, e)))?;
            if n == 0 {
                break;
            }
            writer
                .write_all(&buf[..n])
                .map_err(|e| StreamCheckedError::Xfer(e.into()))?;
            written += n as u64;
        }
        writer
            .flush()
            .map_err(|e| StreamCheckedError::Xfer(e.into()))?;

        if written < remaining {
            return Err(StreamCheckedError::Xfer(Error::Conflict {
                path: self.path,
                reason: format!(
                    "file shrank while streaming: expected {remaining} bytes from offset {}, read {written}",
                    self.offset
                ),
            }));
        }

        Ok(written)
    }
}

/// Opens `path` at `offset` for two-phase size inspection and bounded streaming to the end.
pub fn open_get(path: &Path, offset: u64) -> Result<GetStream> {
    GetStream::open(path, offset)
}

/// Opens `path` at `offset` for up to `length` bytes (or to the end when `None`).
pub fn open_get_range(path: &Path, offset: u64, length: Option<u64>) -> Result<GetStream> {
    GetStream::open_range(path, offset, length)
}

/// Streams the bytes of `path` starting at `offset` into `writer` and returns the total file size.
///
/// Fails with [`Error::BadOffset`] when `offset` exceeds the file's length.
pub fn get<W: Write>(path: &Path, offset: u64, writer: W) -> Result<u64> {
    let handle = open_get(path, offset)?;
    let size = handle.size();
    handle.stream_to(writer)?;
    Ok(size)
}

/// Receives a file into a sibling partial file (`<name>.legion-part`), verifies the whole file's
/// BLAKE3 hash, and atomically renames it over `path`.
///
/// `offset` must equal the partial file's current length on disk; otherwise [`Error::BadOffset`]
/// returns the actual length so the caller can retry from there. Reads exactly `size - offset`
/// bytes from `reader` without waiting for EOF so bidirectional pipes cannot deadlock. On an
/// interrupted stream the partial file is kept for resume; on a hash mismatch the partial file is
/// deleted and [`Error::HashMismatch`] is returned.
pub fn put<R: Read>(
    path: &Path,
    size: u64,
    expected_hash: Hash,
    offset: u64,
    mut reader: R,
    mode: Option<u32>,
) -> Result<PutReport> {
    if let Ok(meta) = fs::symlink_metadata(path)
        && meta.file_type().is_dir()
    {
        return Err(Error::Conflict {
            path: path.to_path_buf(),
            reason: "target path is a directory".to_string(),
        });
    }

    let partial = part_path(path)?;
    let mut file = open_partial_at_offset(path, &partial, size, offset)?;

    let remaining = size - offset;
    let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
    let mut written = 0u64;

    while written < remaining {
        let to_read = usize::try_from(remaining - written)
            .unwrap_or(buf.len())
            .min(buf.len());
        let n = match reader.read(&mut buf[..to_read]) {
            Ok(0) => {
                let _ = file.flush();
                return Err(Error::Io {
                    path: Some(partial),
                    source: io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "stream ended after {} of {size} bytes",
                            offset.saturating_add(written)
                        ),
                    ),
                });
            }
            Ok(n) => n,
            Err(err) => {
                let _ = file.flush();
                return Err(Error::Io {
                    path: None,
                    source: err,
                });
            }
        };
        file.write_all(&buf[..n])
            .map_err(|e| classify_io(&partial, e))?;
        written += n as u64;
    }

    file.flush().map_err(|e| classify_io(&partial, e))?;
    file.sync_all().map_err(|e| classify_io(&partial, e))?;
    drop(file);

    let actual_hash = hash_file(&partial)?;
    if actual_hash != expected_hash {
        let _ = fs::remove_file(&partial);
        return Err(Error::HashMismatch {
            path: path.to_path_buf(),
            expected: expected_hash,
            actual: actual_hash,
        });
    }

    let final_mode = match mode {
        Some(m) => m & 0o777,
        None => fs::symlink_metadata(path)
            .ok()
            .filter(|meta| meta.file_type().is_file())
            .and_then(|meta| platform::entry_mode(&meta))
            .map(|m| m & 0o777)
            .unwrap_or(0o644),
    };
    platform::set_mode(&partial, final_mode)?;

    platform::replace_file(&partial, path)?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    platform::sync_dir(parent)?;

    Ok(PutReport {
        size,
        bytes_written: written,
        hash: actual_hash,
    })
}

fn open_partial_at_offset(target: &Path, partial: &Path, size: u64, offset: u64) -> Result<File> {
    if offset == 0 {
        match platform::create_new_private(partial) {
            Ok(file) => Ok(file),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let meta = fs::symlink_metadata(partial).map_err(|e| classify_io(partial, e))?;
                if !meta.file_type().is_file() {
                    return Err(Error::Conflict {
                        path: partial.to_path_buf(),
                        reason: "partial path exists and is not a regular file".to_string(),
                    });
                }
                let actual = meta.len();
                if actual > size {
                    // A stale partial file larger than the whole incoming file cannot be a valid
                    // prefix of this transfer; truncate and start cleanly from offset 0.
                    let file = OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(partial)
                        .map_err(|e| classify_io(partial, e))?;
                    platform::set_mode(partial, 0o600)?;
                    return Ok(file);
                }
                if actual != 0 {
                    return Err(Error::BadOffset {
                        path: target.to_path_buf(),
                        requested: 0,
                        actual,
                    });
                }
                let file = OpenOptions::new()
                    .write(true)
                    .open(partial)
                    .map_err(|e| classify_io(partial, e))?;
                platform::set_mode(partial, 0o600)?;
                Ok(file)
            }
            Err(err) => Err(classify_io(partial, err)),
        }
    } else {
        let meta = match fs::symlink_metadata(partial) {
            Ok(m) => m,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(Error::BadOffset {
                    path: target.to_path_buf(),
                    requested: offset,
                    actual: 0,
                });
            }
            Err(err) => return Err(classify_io(partial, err)),
        };
        if !meta.file_type().is_file() {
            return Err(Error::Conflict {
                path: partial.to_path_buf(),
                reason: "partial path exists and is not a regular file".to_string(),
            });
        }
        let actual = meta.len();
        if actual != offset {
            return Err(Error::BadOffset {
                path: target.to_path_buf(),
                requested: offset,
                actual,
            });
        }
        if offset > size {
            return Err(Error::BadOffset {
                path: target.to_path_buf(),
                requested: offset,
                actual: size,
            });
        }
        let mut file = OpenOptions::new()
            .write(true)
            .open(partial)
            .map_err(|e| classify_io(partial, e))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| classify_io(partial, e))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::hash::hash_bytes;

    #[test]
    fn get_and_put_full_and_resumed_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.bin");
        let dst = dir.path().join("dest.bin");

        let data: Vec<u8> = (0..(300 * 1024 + 37))
            .map(|i| ((i * 73 + 11) % 256) as u8)
            .collect();
        let expected_hash = hash_bytes(&data);
        fs::write(&src, &data).unwrap();

        assert_eq!(resume_offset(&dst).unwrap(), 0);

        // Simulate a connection drop after 111_111 bytes.
        let cut = 111_111usize;
        let err = put(
            &dst,
            data.len() as u64,
            expected_hash,
            0,
            Cursor::new(&data[..cut]),
            Some(0o640),
        )
        .unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(!dst.exists());
        assert_eq!(resume_offset(&dst).unwrap(), cut as u64);

        // Passing a wrong offset reports the actual partial file length.
        let bad_zero = put(
            &dst,
            data.len() as u64,
            expected_hash,
            0,
            Cursor::new(&data),
            Some(0o640),
        )
        .unwrap_err();
        assert!(
            matches!(
                bad_zero,
                Error::BadOffset {
                    requested: 0,
                    actual,
                    ..
                } if actual == cut as u64
            ),
            "{bad_zero:?}"
        );

        let bad_other = put(
            &dst,
            data.len() as u64,
            expected_hash,
            500,
            Cursor::new(&data[500..]),
            Some(0o640),
        )
        .unwrap_err();
        assert!(
            matches!(
                bad_other,
                Error::BadOffset {
                    requested: 500,
                    actual,
                    ..
                } if actual == cut as u64
            ),
            "{bad_other:?}"
        );

        // Fetch only the remaining bytes using `get` from the resumable offset.
        let offset = resume_offset(&dst).unwrap();
        let mut tail = Vec::new();
        let total_size = get(&src, offset, &mut tail).unwrap();
        assert_eq!(total_size, data.len() as u64);
        assert_eq!(tail.len(), data.len() - cut);

        let report = put(
            &dst,
            total_size,
            expected_hash,
            offset,
            Cursor::new(&tail),
            Some(0o640),
        )
        .unwrap();
        assert_eq!(report.size, data.len() as u64);
        assert_eq!(report.bytes_written, tail.len() as u64);
        assert_eq!(report.hash, expected_hash);
        assert_eq!(fs::read(&dst).unwrap(), data);
        assert_eq!(resume_offset(&dst).unwrap(), 0);
        assert!(!part_path(&dst).unwrap().exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640);
        }
    }

    #[test]
    fn hash_mismatch_deletes_partial_and_preserves_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("target.bin");
        fs::write(&dst, b"original").unwrap();

        let payload = b"corrupted payload";
        let wrong_hash = hash_bytes(b"different expected content");

        let err = put(
            &dst,
            payload.len() as u64,
            wrong_hash,
            0,
            Cursor::new(payload),
            None,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::HashMismatch {
                    expected,
                    actual,
                    ..
                } if expected == wrong_hash && actual == hash_bytes(payload)
            ),
            "{err:?}"
        );
        assert!(!part_path(&dst).unwrap().exists());
        assert_eq!(fs::read(&dst).unwrap(), b"original");
    }

    #[test]
    fn get_rejects_offset_past_eof_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("small.txt");
        fs::write(&file, b"hello").unwrap();

        let mut out = Vec::new();
        let err = get(&file, 10, &mut out).unwrap_err();
        assert!(
            matches!(
                err,
                Error::BadOffset {
                    requested: 10,
                    actual: 5,
                    ..
                }
            ),
            "{err:?}"
        );

        let missing = dir.path().join("nope.txt");
        let err = get(&missing, 0, &mut out).unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn put_handles_empty_file_and_stops_at_declared_size() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.bin");
        let empty_hash = hash_bytes(b"");

        let report = put(&empty, 0, empty_hash, 0, Cursor::new(b""), None).unwrap();
        assert_eq!(report.size, 0);
        assert_eq!(report.bytes_written, 0);
        assert_eq!(fs::read(&empty).unwrap(), b"");

        // Extra trailing bytes in the stream must not be consumed (pipes stay open for replies).
        let framed = dir.path().join("framed.bin");
        let mut cursor = Cursor::new(b"helloTRAILING".as_slice());
        let hello_hash = hash_bytes(b"hello");
        put(&framed, 5, hello_hash, 0, &mut cursor, None).unwrap();
        assert_eq!(fs::read(&framed).unwrap(), b"hello");
        assert_eq!(cursor.position(), 5);
    }

    #[test]
    fn put_rejects_colon_in_target_filename() {
        let dir = tempfile::tempdir().unwrap();
        let ads = dir.path().join("file.txt:stream");
        let err = put(&ads, 0, hash_bytes(b""), 0, Cursor::new(b""), None).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchiveEntry { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn put_strips_special_permission_bits() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("tool.bin");
        let payload = b"binary";
        put(
            &dst,
            payload.len() as u64,
            hash_bytes(payload),
            0,
            Cursor::new(payload),
            Some(0o6755),
        )
        .unwrap();

        let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn put_syncs_parent_directory_after_rename() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("durable.bin");
        let before = platform::dir_sync_calls();
        let data = b"durable bytes";
        put(
            &dst,
            data.len() as u64,
            hash_bytes(data),
            0,
            Cursor::new(data),
            None,
        )
        .unwrap();
        assert!(platform::dir_sync_calls() > before);
    }

    #[test]
    fn open_get_caps_growing_file_and_errors_on_shrinking_file() {
        let dir = tempfile::tempdir().unwrap();
        let growing = dir.path().join("grow.bin");
        fs::write(&growing, vec![b'X'; 100]).unwrap();

        let handle = open_get(&growing, 20).unwrap();
        assert_eq!(handle.size(), 100);
        assert_eq!(handle.offset(), 20);
        assert_eq!(handle.remaining(), 80);

        // Append extra bytes after opening; stream must still emit exactly 80 bytes.
        let mut appender = OpenOptions::new().append(true).open(&growing).unwrap();
        appender.write_all(&[b'Y'; 50]).unwrap();
        drop(appender);

        let mut out = Vec::new();
        let written = handle.stream_to(&mut out).unwrap();
        assert_eq!(written, 80);
        assert_eq!(out, vec![b'X'; 80]);

        // Truncate after opening; stream must fail with Conflict.
        let shrinking = dir.path().join("shrink.bin");
        fs::write(&shrinking, vec![b'Z'; 100]).unwrap();
        let handle = open_get(&shrinking, 10).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&shrinking)
            .unwrap()
            .set_len(40)
            .unwrap();

        let mut sink = Vec::new();
        let err = handle.stream_to(&mut sink).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn open_get_range_slices_clamps_handles_zero_and_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("range.bin");
        let data: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        fs::write(&path, &data).unwrap();
        let expected_mtime = Timestamp::from_metadata(&fs::metadata(&path).unwrap());

        // Bounded middle slice.
        let stream = open_get_range(&path, 100, Some(250)).unwrap();
        assert_eq!(stream.size(), 1024);
        assert_eq!(stream.offset(), 100);
        assert_eq!(stream.length(), 250);
        assert_eq!(stream.remaining(), 250);
        assert_eq!(stream.mtime(), expected_mtime);
        let mut out = Vec::new();
        assert_eq!(stream.stream_to(&mut out).unwrap(), 250);
        assert_eq!(out, &data[100..350]);

        // Length extending past EOF clamps to available bytes.
        let stream = open_get_range(&path, 900, Some(500)).unwrap();
        assert_eq!(stream.size(), 1024);
        assert_eq!(stream.offset(), 900);
        assert_eq!(stream.length(), 124);
        let mut out = Vec::new();
        assert_eq!(stream.stream_to(&mut out).unwrap(), 124);
        assert_eq!(out, &data[900..]);

        // Length 0 succeeds without transferring bytes, even at offset == size.
        let stream = open_get_range(&path, 0, Some(0)).unwrap();
        assert_eq!(stream.size(), 1024);
        assert_eq!(stream.length(), 0);
        let mut out = Vec::new();
        assert_eq!(stream.stream_to(&mut out).unwrap(), 0);
        assert!(out.is_empty());

        let stream = open_get_range(&path, 1024, Some(10)).unwrap();
        assert_eq!(stream.offset(), 1024);
        assert_eq!(stream.length(), 0);
        let mut out = Vec::new();
        assert_eq!(stream.stream_to(&mut out).unwrap(), 0);

        // Offset past EOF returns BadOffset even when length is 0.
        let err = open_get_range(&path, 1025, Some(0)).unwrap_err();
        assert!(
            matches!(
                err,
                Error::BadOffset {
                    requested: 1025,
                    actual: 1024,
                    ..
                }
            ),
            "{err:?}"
        );

        // Shrinking inside the requested range fails with Conflict.
        let stream = open_get_range(&path, 100, Some(300)).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(200)
            .unwrap();
        let mut sink = Vec::new();
        let err = stream.stream_to(&mut sink).unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }), "{err:?}");
    }

    #[test]
    fn put_cleans_up_oversized_stale_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("small.bin");
        let stale = part_path(&dst).unwrap();
        fs::write(&stale, vec![b'X'; 500]).unwrap();

        let payload = b"fresh 10!!";
        put(
            &dst,
            payload.len() as u64,
            hash_bytes(payload),
            0,
            Cursor::new(payload),
            None,
        )
        .unwrap();

        assert_eq!(fs::read(&dst).unwrap(), payload);
        assert!(!stale.exists());
    }

    #[cfg(unix)]
    #[test]
    fn put_creates_private_partial_and_resolves_final_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let new_file = dir.path().join("fresh.bin");
        let payload = b"secret while in flight";
        let hash = hash_bytes(payload);

        // Interrupted put leaves a 0o600 partial file.
        let _ = put(
            &new_file,
            payload.len() as u64,
            hash,
            0,
            Cursor::new(&payload[..6]),
            None,
        );
        let partial = part_path(&new_file).unwrap();
        let part_mode = fs::metadata(&partial).unwrap().permissions().mode() & 0o777;
        assert_eq!(part_mode, 0o600);

        // Completing without an explicit mode defaults to 0o644 for a new file.
        put(
            &new_file,
            payload.len() as u64,
            hash,
            6,
            Cursor::new(&payload[6..]),
            None,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&new_file).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // Replacing an existing file with mode=None preserves its existing mode.
        let existing = dir.path().join("script.sh");
        fs::write(&existing, b"old").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o750)).unwrap();
        put(
            &existing,
            payload.len() as u64,
            hash,
            0,
            Cursor::new(payload),
            None,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o750
        );

        // Explicit mode overrides the existing target's mode.
        put(
            &existing,
            payload.len() as u64,
            hash,
            0,
            Cursor::new(payload),
            Some(0o640),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[cfg(unix)]
    #[test]
    fn put_with_no_mode_over_symlink_defaults_to_0644_not_0777() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        fs::write(&real, b"real").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let payload = b"replacement";
        put(
            &link,
            payload.len() as u64,
            hash_bytes(payload),
            0,
            Cursor::new(payload),
            None,
        )
        .unwrap();

        let meta = fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.permissions().mode() & 0o777, 0o644);
        assert_eq!(fs::read(&real).unwrap(), b"real");
    }
}
