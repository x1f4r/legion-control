//! Resumable single-file streaming (`get` and `put`) with whole-file verification.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, classify_io};
use crate::hash::{Hash, hash_file};
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
    let file_name = target.file_name().ok_or_else(|| Error::Conflict {
        path: target.to_path_buf(),
        reason: "target path has no file name".to_string(),
    })?;
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

/// Streams the bytes of `path` starting at `offset` into `writer` and returns the total file size.
///
/// Fails with [`Error::BadOffset`] when `offset` exceeds the file's length.
pub fn get<W: Write>(path: &Path, offset: u64, mut writer: W) -> Result<u64> {
    let mut file = File::open(path).map_err(|e| classify_io(path, e))?;
    let meta = file.metadata().map_err(|e| classify_io(path, e))?;
    if !meta.is_file() {
        return Err(Error::Conflict {
            path: path.to_path_buf(),
            reason: "source path is not a regular file".to_string(),
        });
    }

    let size = meta.len();
    if offset > size {
        return Err(Error::BadOffset {
            path: path.to_path_buf(),
            requested: offset,
            actual: size,
        });
    }

    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| classify_io(path, e))?;
    }

    let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
    loop {
        let n = file.read(&mut buf).map_err(|e| classify_io(path, e))?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
    }
    writer.flush()?;

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

    if let Some(m) = mode {
        platform::set_mode(&partial, m & 0o7777)?;
    }

    platform::replace_file(&partial, path)?;

    Ok(PutReport {
        size,
        bytes_written: written,
        hash: actual_hash,
    })
}

fn open_partial_at_offset(target: &Path, partial: &Path, size: u64, offset: u64) -> Result<File> {
    if offset == 0 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(partial)
        {
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
                if actual != 0 {
                    return Err(Error::BadOffset {
                        path: target.to_path_buf(),
                        requested: 0,
                        actual,
                    });
                }
                OpenOptions::new()
                    .write(true)
                    .open(partial)
                    .map_err(|e| classify_io(partial, e))
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
}
