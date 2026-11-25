//! Resumable single-file streaming (`get` and `put`) with whole-file verification.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, classify_io};
use crate::hash::{Hash, hash_file};
use crate::path::validate_target_filename;
use crate::platform;

/// Suffix appended to a target filename while a single-file [`put`] is in progress.
pub const PART_SUFFIX: &str = ".legion-part";

/// Fixed copy buffer size so large transfers run in constant memory.
const STREAM_BUFFER_SIZE: usize = 64 * 1024;

/// Upper bound on numbering attempts for [`IfExists::Rename`] (`name (2).ext`, `name (3).ext`, …).
pub const MAX_RENAME_ATTEMPTS: u32 = 10_000;

/// Policy when a destination entry already exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IfExists {
    /// Overwrite the destination entry atomically (default).
    #[default]
    Replace,
    /// Refuse with [`Error::Exists`] without touching the destination.
    Fail,
    /// Leave the existing destination entry untouched and skip writing.
    Skip,
    /// Write beside the existing entry as `name (2).ext`, counting up atomically.
    Rename,
}

/// Result of atomically placing a completed temporary file at a destination path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceOutcome {
    /// The file was placed at the returned path (which may differ from the requested path under
    /// [`IfExists::Rename`]).
    Placed(PathBuf),
    /// The destination already existed and [`IfExists::Skip`] left it untouched.
    Skipped,
}

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

/// Detailed result returned by [`PutSession::finish`], including the chosen destination path and
/// whether the file was skipped under [`IfExists::Skip`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// Byte counts and verified hash of the streamed file.
    pub report: PutReport,
    /// Actual destination path written, or `None` when [`Self::skipped`] is `true`.
    pub path: Option<PathBuf>,
    /// `true` when the destination appeared concurrently and [`IfExists::Skip`] left it intact.
    pub skipped: bool,
}

/// Builds the `n`-th numbered sibling candidate for `target` (`n = 2` produces `name (2).ext`).
///
/// A leading dot with no other dot (such as `.bashrc`) is treated as a stem without an extension
/// so `.bashrc` becomes `.bashrc (2)`.
pub fn numbered_candidate(target: &Path, n: u32) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let candidate_name = numbered_filename(file_name, n);
    match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(candidate_name),
        _ => PathBuf::from(candidate_name),
    }
}

/// Formats `name` with a ` (n)` suffix before its last extension (when any).
pub(crate) fn numbered_filename(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() => {
            let (stem, ext) = name.split_at(dot);
            format!("{stem} ({n}){ext}")
        }
        _ => format!("{name} ({n})"),
    }
}

/// Atomically places `temp` at `target` according to `if_exists`.
///
/// Under [`IfExists::Replace`], `temp` is renamed over `target`. Under [`IfExists::Fail`],
/// [`IfExists::Skip`], and [`IfExists::Rename`], placement uses [`fs::hard_link`] so two concurrent
/// transfers racing for the same destination name never overwrite each other; `temp` is unlinked on
/// successful placement, and left for the caller to clean up on [`PlaceOutcome::Skipped`] or error.
pub(crate) fn place_file<F>(
    temp: &Path,
    target: &Path,
    if_exists: IfExists,
    mut check_target: F,
) -> Result<PlaceOutcome>
where
    F: FnMut(&Path) -> Result<()>,
{
    match if_exists {
        IfExists::Replace => {
            platform::replace_file(temp, target)?;
            Ok(PlaceOutcome::Placed(target.to_path_buf()))
        }
        IfExists::Fail => match fs::hard_link(temp, target) {
            Ok(()) => {
                let _ = fs::remove_file(temp);
                Ok(PlaceOutcome::Placed(target.to_path_buf()))
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(Error::Exists {
                path: target.to_path_buf(),
                source: Some(err),
            }),
            Err(err) => Err(classify_io(target, err)),
        },
        IfExists::Skip => match fs::hard_link(temp, target) {
            Ok(()) => {
                let _ = fs::remove_file(temp);
                Ok(PlaceOutcome::Placed(target.to_path_buf()))
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(PlaceOutcome::Skipped),
            Err(err) => Err(classify_io(target, err)),
        },
        IfExists::Rename => {
            for attempt in 1..=MAX_RENAME_ATTEMPTS {
                let candidate = if attempt == 1 {
                    target.to_path_buf()
                } else {
                    let cand = numbered_candidate(target, attempt);
                    check_target(&cand)?;
                    cand
                };
                match fs::hard_link(temp, &candidate) {
                    Ok(()) => {
                        let _ = fs::remove_file(temp);
                        return Ok(PlaceOutcome::Placed(candidate));
                    }
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(err) => return Err(classify_io(&candidate, err)),
                }
            }
            Err(Error::Exists {
                path: target.to_path_buf(),
                source: None,
            })
        }
    }
}

/// An opened single-file receive session holding an exclusive lock on its partial file so
/// concurrent transfers to the same target path never corrupt each other's partial data.
#[derive(Debug)]
pub struct PutSession {
    target: PathBuf,
    partial: PathBuf,
    file: File,
    size: u64,
    offset: u64,
    if_exists: IfExists,
    /// `true` when another transfer held the canonical `.legion-part` lock and this session fell
    /// back to a private unique sibling partial file that should be removed if interrupted.
    ephemeral: bool,
}

impl PutSession {
    /// Prepares to receive a file of `size` bytes at `target` under `if_exists`.
    ///
    /// Locks `<name>.legion-part` exclusively when free so a single interrupted transfer resumes
    /// from its length on disk; if another concurrent transfer holds that lock, opens a unique
    /// sibling partial file at offset 0 so both transfers race cleanly to commit atomically.
    pub fn open(target: &Path, size: u64, if_exists: IfExists) -> Result<Self> {
        if let Ok(meta) = fs::symlink_metadata(target) {
            if if_exists == IfExists::Fail {
                return Err(Error::Exists {
                    path: target.to_path_buf(),
                    source: None,
                });
            }
            if if_exists == IfExists::Replace && meta.file_type().is_dir() {
                return Err(Error::Conflict {
                    path: target.to_path_buf(),
                    reason: "target path is a directory".to_string(),
                });
            }
        }

        let canonical_partial = part_path(target)?;
        match try_open_locked_partial(&canonical_partial, size)? {
            Some((file, offset)) => Ok(Self {
                target: target.to_path_buf(),
                partial: canonical_partial,
                file,
                size,
                offset,
                if_exists,
                ephemeral: false,
            }),
            None => {
                let (partial, file) = create_unique_partial(target)?;
                Ok(Self {
                    target: target.to_path_buf(),
                    partial,
                    file,
                    size,
                    offset: 0,
                    if_exists,
                    ephemeral: true,
                })
            }
        }
    }

    /// Byte offset the sender should stream from (`0` for a fresh or concurrent transfer, or the
    /// existing partial file's length when resuming).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Reads the remaining `size - offset` bytes from `reader`, verifies `expected_hash`, applies
    /// `mode`, and atomically commits according to `if_exists`.
    pub fn finish<R, F>(
        mut self,
        expected_hash: Hash,
        mut reader: R,
        mode: Option<u32>,
        check_target: F,
    ) -> Result<PutOutcome>
    where
        R: Read,
        F: FnMut(&Path) -> Result<()>,
    {
        let remaining = self.size - self.offset;
        let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
        let mut written = 0u64;

        while written < remaining {
            let to_read = usize::try_from(remaining - written)
                .unwrap_or(buf.len())
                .min(buf.len());
            let n = match reader.read(&mut buf[..to_read]) {
                Ok(0) => {
                    let _ = self.file.flush();
                    drop(self.file);
                    if self.ephemeral {
                        let _ = fs::remove_file(&self.partial);
                    }
                    return Err(Error::Io {
                        path: Some(self.partial),
                        source: io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "stream ended after {} of {} bytes",
                                self.offset.saturating_add(written),
                                self.size
                            ),
                        ),
                    });
                }
                Ok(n) => n,
                Err(err) => {
                    let _ = self.file.flush();
                    drop(self.file);
                    if self.ephemeral {
                        let _ = fs::remove_file(&self.partial);
                    }
                    return Err(Error::Io {
                        path: None,
                        source: err,
                    });
                }
            };
            if let Err(err) = self.file.write_all(&buf[..n]) {
                drop(self.file);
                if self.ephemeral {
                    let _ = fs::remove_file(&self.partial);
                }
                return Err(classify_io(&self.partial, err));
            }
            written += n as u64;
        }

        self.file
            .flush()
            .map_err(|e| classify_io(&self.partial, e))?;
        self.file
            .sync_all()
            .map_err(|e| classify_io(&self.partial, e))?;
        drop(self.file);

        let actual_hash = hash_file(&self.partial)?;
        if actual_hash != expected_hash {
            let _ = fs::remove_file(&self.partial);
            return Err(Error::HashMismatch {
                path: self.target,
                expected: expected_hash,
                actual: actual_hash,
            });
        }

        let final_mode = match mode {
            Some(m) => m & 0o777,
            None => fs::symlink_metadata(&self.target)
                .ok()
                .filter(|meta| meta.file_type().is_file())
                .and_then(|meta| platform::entry_mode(&meta))
                .map(|m| m & 0o777)
                .unwrap_or(0o644),
        };
        platform::set_mode(&self.partial, final_mode)?;

        let outcome = match place_file(&self.partial, &self.target, self.if_exists, check_target) {
            Ok(outcome) => outcome,
            Err(err) => {
                if self.ephemeral || matches!(err, Error::Exists { .. }) {
                    let _ = fs::remove_file(&self.partial);
                }
                return Err(err);
            }
        };

        let report = PutReport {
            size: self.size,
            bytes_written: written,
            hash: actual_hash,
        };

        match outcome {
            PlaceOutcome::Placed(final_path) => {
                let parent = match final_path.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p,
                    _ => Path::new("."),
                };
                platform::sync_dir(parent)?;
                Ok(PutOutcome {
                    report,
                    path: Some(final_path),
                    skipped: false,
                })
            }
            PlaceOutcome::Skipped => {
                let _ = fs::remove_file(&self.partial);
                Ok(PutOutcome {
                    report,
                    path: None,
                    skipped: true,
                })
            }
        }
    }
}

fn try_open_locked_partial(partial: &Path, size: u64) -> Result<Option<(File, u64)>> {
    let mut file = match platform::create_new_private(partial) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(partial).map_err(|e| classify_io(partial, e))?;
            if !meta.file_type().is_file() {
                return Err(Error::Conflict {
                    path: partial.to_path_buf(),
                    reason: "partial path exists and is not a regular file".to_string(),
                });
            }
            OpenOptions::new()
                .write(true)
                .open(partial)
                .map_err(|e| classify_io(partial, e))?
        }
        Err(err) => return Err(classify_io(partial, err)),
    };

    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(err)) => return Err(classify_io(partial, err)),
    }

    let meta = file.metadata().map_err(|e| classify_io(partial, e))?;
    let mut offset = meta.len();
    if offset > size {
        file.set_len(0).map_err(|e| classify_io(partial, e))?;
        offset = 0;
    }
    if offset == 0 {
        platform::set_mode(partial, 0o600)?;
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| classify_io(partial, e))?;
    Ok(Some((file, offset)))
}

fn create_unique_partial(target: &Path) -> Result<(PathBuf, File)> {
    let parent = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let pid = std::process::id();
    loop {
        let seq = platform::next_temp_id();
        let candidate = parent.join(format!(".legion-put-{pid}-{seq}{PART_SUFFIX}"));
        match platform::create_new_private(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(classify_io(&candidate, err)),
        }
    }
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

/// Open handle to a source file ready to stream from a validated byte offset.
///
/// Splitting open from stream lets callers inspect [`Self::size`] (captured via `fstat` on the
/// opened descriptor) and emit a response header before writing raw file bytes onto a pipe.
#[derive(Debug)]
pub struct GetStream {
    path: PathBuf,
    file: File,
    size: u64,
    offset: u64,
}

impl GetStream {
    /// Opens `path`, verifies it is a regular file with `offset <= size`, and seeks to `offset`.
    pub fn open(path: &Path, offset: u64) -> Result<Self> {
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

        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
            offset,
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

    /// Returns the exact number of bytes [`Self::stream_to`] will emit (`size - offset`).
    pub fn remaining(&self) -> u64 {
        self.size - self.offset
    }

    /// Streams exactly `size - offset` bytes into `writer` and returns the byte count written.
    ///
    /// If the file grows after opening, reads stop at `size` so framed protocols stay aligned. If
    /// the file shrinks before `size - offset` bytes are read, returns [`Error::Conflict`].
    pub fn stream_to<W: Write>(self, mut writer: W) -> Result<u64> {
        let remaining = self.remaining();
        let mut limited = self.file.take(remaining);
        let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
        let mut written = 0u64;

        loop {
            let n = limited
                .read(&mut buf)
                .map_err(|e| classify_io(&self.path, e))?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            written += n as u64;
        }
        writer.flush()?;

        if written < remaining {
            return Err(Error::Conflict {
                path: self.path,
                reason: format!(
                    "file shrank while streaming: expected {remaining} bytes from offset {}, read {written}",
                    self.offset
                ),
            });
        }

        Ok(written)
    }
}

/// Opens `path` at `offset` for two-phase size inspection and bounded streaming.
pub fn open_get(path: &Path, offset: u64) -> Result<GetStream> {
    GetStream::open(path, offset)
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

    #[test]
    fn numbered_candidate_formats_extensions_and_dotfiles() {
        assert_eq!(
            numbered_candidate(Path::new("/dir/Holiday.mp4"), 2),
            PathBuf::from("/dir/Holiday (2).mp4")
        );
        assert_eq!(
            numbered_candidate(Path::new("/dir/README"), 3),
            PathBuf::from("/dir/README (3)")
        );
        assert_eq!(
            numbered_candidate(Path::new("/dir/.bashrc"), 2),
            PathBuf::from("/dir/.bashrc (2)")
        );
        assert_eq!(
            numbered_candidate(Path::new("/dir/.config.toml"), 2),
            PathBuf::from("/dir/.config (2).toml")
        );
        assert_eq!(
            numbered_candidate(Path::new("archive.tar.gz"), 4),
            PathBuf::from("archive.tar (4).gz")
        );
    }

    #[test]
    fn put_session_enforces_if_exists_modes_and_handles_concurrent_race() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("Holiday.mp4");
        let existing = b"original";
        fs::write(&dst, existing).unwrap();

        // Fail: refuses immediately without overwriting.
        let err = PutSession::open(&dst, 3, IfExists::Fail).unwrap_err();
        assert!(matches!(err, Error::Exists { .. }), "{err:?}");
        assert_eq!(fs::read(&dst).unwrap(), existing);

        // Skip: leaves existing file untouched.
        let payload = b"new bytes";
        let session = PutSession::open(&dst, payload.len() as u64, IfExists::Skip).unwrap();
        let outcome = session
            .finish(hash_bytes(payload), Cursor::new(payload), None, |_| Ok(()))
            .unwrap();
        assert!(outcome.skipped);
        assert_eq!(outcome.path, None);
        assert_eq!(fs::read(&dst).unwrap(), existing);

        // Rename: writes `Holiday (2).mp4`, then `Holiday (3).mp4`.
        let session = PutSession::open(&dst, payload.len() as u64, IfExists::Rename).unwrap();
        let outcome = session
            .finish(hash_bytes(payload), Cursor::new(payload), None, |_| Ok(()))
            .unwrap();
        assert!(!outcome.skipped);
        assert_eq!(outcome.path, Some(dir.path().join("Holiday (2).mp4")));
        assert_eq!(
            fs::read(dir.path().join("Holiday (2).mp4")).unwrap(),
            payload
        );

        // Race: 4 threads concurrently write to the same target under Rename.
        let race_dst = dir.path().join("race.bin");
        let barrier = Arc::new(Barrier::new(4));
        let mut handles = Vec::new();
        for i in 0u8..4 {
            let race_dst = race_dst.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let bytes = vec![b'A' + i; 16 * 1024];
                let hash = hash_bytes(&bytes);
                barrier.wait();
                let session =
                    PutSession::open(&race_dst, bytes.len() as u64, IfExists::Rename).unwrap();
                let out = session
                    .finish(hash, Cursor::new(&bytes), None, |_| Ok(()))
                    .unwrap();
                (out.path.unwrap(), bytes)
            }));
        }
        let mut paths = std::collections::BTreeSet::new();
        for handle in handles {
            let (path, expected_bytes) = handle.join().unwrap();
            assert_eq!(fs::read(&path).unwrap(), expected_bytes);
            assert!(
                paths.insert(path),
                "two racing transfers claimed the same path"
            );
        }
        assert_eq!(paths.len(), 4);
        assert!(paths.contains(&dir.path().join("race.bin")));
        assert!(paths.contains(&dir.path().join("race (2).bin")));
        assert!(paths.contains(&dir.path().join("race (3).bin")));
        assert!(paths.contains(&dir.path().join("race (4).bin")));

        // Race under Fail: exactly one wins and the other gets Error::Exists.
        let fail_dst = dir.path().join("exclusive.bin");
        let barrier = Arc::new(Barrier::new(2));
        let mut results = Vec::new();
        for i in 0u8..2 {
            let fail_dst = fail_dst.clone();
            let barrier = Arc::clone(&barrier);
            results.push(std::thread::spawn(move || {
                let bytes = vec![b'X' + i; 16 * 1024];
                let hash = hash_bytes(&bytes);
                let session =
                    PutSession::open(&fail_dst, bytes.len() as u64, IfExists::Fail).unwrap();
                barrier.wait();
                session.finish(hash, Cursor::new(&bytes), None, |_| Ok(()))
            }));
        }
        let mut ok_count = 0;
        let mut exists_count = 0;
        for h in results {
            match h.join().unwrap() {
                Ok(_) => ok_count += 1,
                Err(Error::Exists { .. }) => exists_count += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!((ok_count, exists_count), (1, 1));
    }
}
