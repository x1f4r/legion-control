//! Resumable single-file streaming (`get` and `put`) with whole-file verification.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use legion_os::atomic::RenameNoReplaceError;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, classify_io};
use crate::hash::{Hash, hash_file};
use crate::path::validate_target_filename;
use crate::platform;

/// Suffix appended to a target filename while a single-file [`put`] is in progress.
pub const PART_SUFFIX: &str = ".legion-part";

/// Sidecar suffix recording the expected `(size, hash)` of a resumable `.legion-part` file.
pub const PART_META_SUFFIX: &str = ".legion-part.meta";

/// Dedicated lockfile suffix coordinating exclusive access to a resumable `.legion-part` file.
///
/// Locking a dedicated file (never renamed) ensures the exclusive lock stays held across
/// `sync_all`, closing the data handle (required on Windows before rename), final hashing, and
/// atomic placement.
pub const PART_LOCK_SUFFIX: &str = ".legion-part.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PartialMeta {
    size: u64,
    hash: Hash,
}

/// Default age threshold after which abandoned `.legion-put-*` / `.legion-unpack-*` temporary
/// files are swept opportunistically.
pub const STALE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

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

/// Atomically places `temp` at `target` according to `if_exists`, never using hard links.
///
/// Under [`IfExists::Replace`], `temp` is renamed over `target`. Under [`IfExists::Fail`],
/// [`IfExists::Skip`], and [`IfExists::Rename`], placement first attempts the kernel's atomic
/// no-replace rename (`renameat2(RENAME_NOREPLACE)`, `renamex_np(RENAME_EXCL)`, or `MoveFileExW`
/// without `MOVEFILE_REPLACE_EXISTING`). When the filesystem does not support no-replace rename
/// (such as FAT, exFAT, some SMB/FUSE mounts), it falls back to atomically claiming the candidate
/// name with `O_CREAT | O_EXCL` (`CREATE_NEW`) and renaming the verified `temp` over the claim we
/// own, removing the claim if the rename fails. Two concurrent transfers racing for the same name
/// never overwrite each other and no temporary file ever shares an inode with a placed file.
pub(crate) fn place_file<F>(
    temp: &Path,
    target: &Path,
    if_exists: IfExists,
    check_target: F,
) -> Result<PlaceOutcome>
where
    F: FnMut(&Path) -> Result<()>,
{
    place_file_with_ops(
        temp,
        target,
        if_exists,
        check_target,
        platform::rename_no_replace,
        platform::create_new_private,
        platform::replace_file,
    )
}

fn place_file_with_ops<F, RN, CL, RP>(
    temp: &Path,
    target: &Path,
    if_exists: IfExists,
    mut check_target: F,
    mut rename_no_replace: RN,
    mut claim_new: CL,
    mut replace_over: RP,
) -> Result<PlaceOutcome>
where
    F: FnMut(&Path) -> Result<()>,
    RN: FnMut(&Path, &Path) -> std::result::Result<(), RenameNoReplaceError>,
    CL: FnMut(&Path) -> io::Result<File>,
    RP: FnMut(&Path, &Path) -> Result<()>,
{
    match if_exists {
        IfExists::Replace => {
            replace_over(temp, target)?;
            Ok(PlaceOutcome::Placed(target.to_path_buf()))
        }
        IfExists::Fail => {
            if place_one_no_replace(
                temp,
                target,
                &mut rename_no_replace,
                &mut claim_new,
                &mut replace_over,
            )? {
                Ok(PlaceOutcome::Placed(target.to_path_buf()))
            } else {
                Err(Error::Exists {
                    path: target.to_path_buf(),
                    source: None,
                })
            }
        }
        IfExists::Skip => {
            if place_one_no_replace(
                temp,
                target,
                &mut rename_no_replace,
                &mut claim_new,
                &mut replace_over,
            )? {
                Ok(PlaceOutcome::Placed(target.to_path_buf()))
            } else {
                Ok(PlaceOutcome::Skipped)
            }
        }
        IfExists::Rename => {
            for attempt in 1..=MAX_RENAME_ATTEMPTS {
                let candidate = if attempt == 1 {
                    target.to_path_buf()
                } else {
                    let cand = numbered_candidate(target, attempt);
                    check_target(&cand)?;
                    cand
                };
                if place_one_no_replace(
                    temp,
                    &candidate,
                    &mut rename_no_replace,
                    &mut claim_new,
                    &mut replace_over,
                )? {
                    return Ok(PlaceOutcome::Placed(candidate));
                }
            }
            Err(Error::Exists {
                path: target.to_path_buf(),
                source: None,
            })
        }
    }
}

/// Attempts to place `temp` at `candidate` without replacing an existing entry.
///
/// Returns `Ok(true)` when placed, `Ok(false)` when `candidate` already existed (untouched), or
/// `Err` on any I/O failure (removing any newly created claim before returning).
fn place_one_no_replace<RN, CL, RP>(
    temp: &Path,
    candidate: &Path,
    rename_no_replace: &mut RN,
    claim_new: &mut CL,
    replace_over: &mut RP,
) -> Result<bool>
where
    RN: FnMut(&Path, &Path) -> std::result::Result<(), RenameNoReplaceError>,
    CL: FnMut(&Path) -> io::Result<File>,
    RP: FnMut(&Path, &Path) -> Result<()>,
{
    match rename_no_replace(temp, candidate) {
        Ok(()) => Ok(true),
        Err(RenameNoReplaceError::AlreadyExists) => Ok(false),
        Err(RenameNoReplaceError::Io(err)) => Err(classify_io(candidate, err)),
        Err(RenameNoReplaceError::Unsupported) => {
            // Claim `candidate` atomically with `O_CREAT | O_EXCL` (`CREATE_NEW`), then rename
            // `temp` over the claim we own. Never touches any pre-existing file at `candidate`.
            match claim_new(candidate) {
                Ok(claim_file) => {
                    drop(claim_file);
                    if let Err(err) = replace_over(temp, candidate) {
                        let _ = fs::remove_file(candidate);
                        return Err(err);
                    }
                    Ok(true)
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                Err(err) => Err(classify_io(candidate, err)),
            }
        }
    }
}

/// An opened single-file receive session holding either an exclusive lock on the canonical
/// resumable partial (identified by its `.legion-part.meta` `(size, hash)`) or a private unique
/// ephemeral partial so concurrent transfers to the same target path never see each other's bytes.
#[derive(Debug)]
pub struct PutSession {
    target: PathBuf,
    partial: PathBuf,
    meta_path: Option<PathBuf>,
    lock_path: Option<PathBuf>,
    part_lock: Option<File>,
    file: Option<File>,
    size: u64,
    expected_hash: Hash,
    offset: u64,
    if_exists: IfExists,
    /// `true` when this session writes to a private unique sibling partial file that is removed if
    /// dropped before committing.
    ephemeral: bool,
    /// `true` once the session has committed or explicitly cleaned up its partial file.
    committed: bool,
}

impl Drop for PutSession {
    fn drop(&mut self) {
        self.file.take();
        if self.ephemeral && !self.committed {
            let _ = fs::remove_file(&self.partial);
        }
        self.part_lock.take();
    }
}

impl PutSession {
    /// Prepares to receive a file of `size` bytes with BLAKE3 digest `expected_hash` at `target`
    /// under `if_exists`.
    ///
    /// Acquires an exclusive lock on `<name>.legion-part.lock` when free. Resumes from
    /// `<name>.legion-part` ONLY when `<name>.legion-part.meta` records the exact same `(size,
    /// expected_hash)` and the partial is a single-linked regular file no longer than `size`.
    /// When another concurrent transfer holds the lock (or an unrelated partial exists under a
    /// non-replacing `if_exists` mode), opens a private unique sibling partial at offset 0 so
    /// concurrent transfers never share or corrupt each other's bytes.
    pub fn open(
        target: &Path,
        size: u64,
        expected_hash: Hash,
        if_exists: IfExists,
    ) -> Result<Self> {
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

        if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = sweep_stale_temps(parent, STALE_TEMP_AGE);
        }

        let canonical_partial = part_path(target)?;
        let meta_path = part_meta_path(target)?;
        let lock_path = part_lock_path(target)?;
        match try_open_locked_partial(
            &canonical_partial,
            &meta_path,
            &lock_path,
            size,
            expected_hash,
            if_exists,
        )? {
            Some((part_lock, file, offset)) => Ok(Self {
                target: target.to_path_buf(),
                partial: canonical_partial,
                meta_path: Some(meta_path),
                lock_path: Some(lock_path),
                part_lock: Some(part_lock),
                file: Some(file),
                size,
                expected_hash,
                offset,
                if_exists,
                ephemeral: false,
                committed: false,
            }),
            None => {
                let (partial, file) = create_unique_partial(target)?;
                Ok(Self {
                    target: target.to_path_buf(),
                    partial,
                    meta_path: None,
                    lock_path: None,
                    part_lock: None,
                    file: Some(file),
                    size,
                    expected_hash,
                    offset: 0,
                    if_exists,
                    ephemeral: true,
                    committed: false,
                })
            }
        }
    }

    /// Byte offset the sender should stream from (`0` for a fresh or concurrent transfer, or the
    /// existing matching partial file's length when resuming).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    fn release_canonical_lock(&mut self, remove_meta: bool) {
        if remove_meta && let Some(ref meta_path) = self.meta_path {
            let _ = fs::remove_file(meta_path);
        }
        #[cfg(unix)]
        {
            // Unlink the lock file while still holding the exclusive lock so no new opener can
            // collide on the same inode before cleanup completes.
            if let Some(ref lock_path) = self.lock_path {
                let _ = fs::remove_file(lock_path);
            }
            self.part_lock.take();
        }
        #[cfg(not(unix))]
        {
            self.part_lock.take();
            if let Some(ref lock_path) = self.lock_path {
                let _ = fs::remove_file(lock_path);
            }
        }
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
        if expected_hash != self.expected_hash {
            return Err(Error::Conflict {
                path: self.target.clone(),
                reason: "expected hash passed to finish does not match session hash".to_string(),
            });
        }
        let mut file = self.file.take().ok_or_else(|| Error::Conflict {
            path: self.partial.clone(),
            reason: "partial file handle was already closed".to_string(),
        })?;
        let remaining = self.size - self.offset;
        let mut buf = vec![0u8; STREAM_BUFFER_SIZE];
        let mut written = 0u64;

        while written < remaining {
            let to_read = usize::try_from(remaining - written)
                .unwrap_or(buf.len())
                .min(buf.len());
            let n = match reader.read(&mut buf[..to_read]) {
                Ok(0) => {
                    let _ = file.flush();
                    drop(file);
                    return Err(Error::Io {
                        path: Some(self.partial.clone()),
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
                    let _ = file.flush();
                    drop(file);
                    return Err(Error::Io {
                        path: None,
                        source: err,
                    });
                }
            };
            if let Err(err) = file.write_all(&buf[..n]) {
                drop(file);
                return Err(classify_io(&self.partial, err));
            }
            written += n as u64;
        }

        file.flush().map_err(|e| classify_io(&self.partial, e))?;
        file.sync_all().map_err(|e| classify_io(&self.partial, e))?;
        // Close the data handle (required before rename on Windows) while keeping `self.part_lock`
        // held until after hash verification and atomic placement finish.
        drop(file);

        let actual_hash = hash_file(&self.partial)?;
        if actual_hash != expected_hash {
            let _ = fs::remove_file(&self.partial);
            self.release_canonical_lock(true);
            self.committed = true;
            return Err(Error::HashMismatch {
                path: self.target.clone(),
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
        if let Err(err) = platform::set_mode(&self.partial, final_mode) {
            let _ = fs::remove_file(&self.partial);
            self.release_canonical_lock(true);
            self.committed = true;
            return Err(err);
        }

        let outcome = match place_file(&self.partial, &self.target, self.if_exists, check_target) {
            Ok(outcome) => outcome,
            Err(err) => {
                if self.ephemeral
                    || matches!(
                        err,
                        Error::Exists { .. }
                            | Error::Conflict { .. }
                            | Error::PermissionDenied { .. }
                    )
                {
                    let _ = fs::remove_file(&self.partial);
                    self.release_canonical_lock(true);
                    self.committed = true;
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
                self.release_canonical_lock(true);
                self.committed = true;
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
                self.release_canonical_lock(true);
                self.committed = true;
                Ok(PutOutcome {
                    report,
                    path: None,
                    skipped: true,
                })
            }
        }
    }
}

/// Removes abandoned temporary files (`.legion-put-*.legion-part`, `.legion-unpack-*.tmp`,
/// `.legion-link-*.tmp`, `.legion-move-probe-*.tmp`) in `dir` whose modification time is older
/// than `max_age` and that are not locked by a live transfer.
pub fn sweep_stale_temps(dir: &Path, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name_os = entry.file_name();
        let Some(name) = name_os.to_str() else {
            continue;
        };
        let is_sweepable = (name.starts_with(".legion-put-") && name.ends_with(PART_SUFFIX))
            || ((name.starts_with(".legion-unpack-")
                || name.starts_with(".legion-link-")
                || name.starts_with(".legion-move-probe-"))
                && name.ends_with(".tmp"));
        if !is_sweepable {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < max_age {
            continue;
        }
        // Only remove if no active transfer holds a lock on the file.
        let lockable = match OpenOptions::new().write(true).open(&path) {
            Ok(file) => match file.try_lock() {
                Ok(()) => {
                    drop(file);
                    true
                }
                Err(_) => false,
            },
            Err(_) => false,
        };
        if lockable && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn write_partial_meta(meta_path: &Path, size: u64, hash: Hash) -> Result<()> {
    let text = format!("{size} {hash}\n");
    legion_os::atomic::write_atomic(
        meta_path,
        text.as_bytes(),
        legion_os::atomic::WriteOptions::with_mode(0o600),
    )
    .map_err(|err| Error::Io {
        path: Some(meta_path.to_path_buf()),
        source: io::Error::other(err),
    })
}

fn read_partial_meta(meta_path: &Path) -> Option<PartialMeta> {
    let text = fs::read_to_string(meta_path).ok()?;
    let mut parts = text.split_whitespace();
    let size: u64 = parts.next()?.parse().ok()?;
    let hash: Hash = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(PartialMeta { size, hash })
}

fn try_lock_canonical_path(lock_path: &Path) -> Result<Option<File>> {
    let lock_file = match platform::create_new_private(lock_path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(lock_path).map_err(|e| classify_io(lock_path, e))?;
            if !meta.file_type().is_file() {
                return Ok(None);
            }
            match OpenOptions::new().write(true).open(lock_path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(err) => return Err(classify_io(lock_path, err)),
            }
        }
        Err(err) => return Err(classify_io(lock_path, err)),
    };

    match lock_file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(err)) => return Err(classify_io(lock_path, err)),
    }

    // Verify the locked inode is still the live entry at `lock_path` so an opener that raced with
    // another session's unlink never proceeds on an unlinked lock inode.
    let same_live_inode = platform::open_file_identity(&lock_file)
        .ok()
        .zip(platform::symlink_file_identity(lock_path).ok())
        .is_some_and(|(a, b)| a.same_file(&b));
    if !same_live_inode {
        return Ok(None);
    }

    Ok(Some(lock_file))
}

fn try_open_locked_partial(
    partial: &Path,
    meta_path: &Path,
    lock_path: &Path,
    size: u64,
    expected_hash: Hash,
    if_exists: IfExists,
) -> Result<Option<(File, File, u64)>> {
    let Some(part_lock) = try_lock_canonical_path(lock_path)? else {
        return Ok(None);
    };

    match fs::symlink_metadata(partial) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err(Error::Conflict {
                    path: partial.to_path_buf(),
                    reason: "partial path exists and is not a regular file".to_string(),
                });
            }
            // Never resume into or truncate a partial that shares an inode with another link.
            if platform::symlink_file_identity(partial).is_ok_and(|id| id.nlink > 1) {
                return Ok(None);
            }
            let recorded = read_partial_meta(meta_path);
            let same_source = recorded
                == Some(PartialMeta {
                    size,
                    hash: expected_hash,
                });
            let existing_len = meta.len();
            if same_source && existing_len <= size {
                let mut file = OpenOptions::new()
                    .write(true)
                    .open(partial)
                    .map_err(|e| classify_io(partial, e))?;
                if platform::open_file_identity(&file).is_ok_and(|id| id.nlink > 1) {
                    return Ok(None);
                }
                file.seek(SeekFrom::Start(existing_len))
                    .map_err(|e| classify_io(partial, e))?;
                return Ok(Some((part_lock, file, existing_len)));
            }
            // Existing partial belongs to a different transfer (missing or mismatched sidecar) or
            // exceeds `size`. Under non-replacing modes, leave it alone and use a private partial;
            // under Replace, reset it cleanly for this transfer.
            if if_exists != IfExists::Replace {
                return Ok(None);
            }
            let mut file = OpenOptions::new()
                .write(true)
                .open(partial)
                .map_err(|e| classify_io(partial, e))?;
            if platform::open_file_identity(&file).is_ok_and(|id| id.nlink > 1) {
                return Ok(None);
            }
            file.set_len(0).map_err(|e| classify_io(partial, e))?;
            file.seek(SeekFrom::Start(0))
                .map_err(|e| classify_io(partial, e))?;
            platform::set_mode(partial, 0o600)?;
            write_partial_meta(meta_path, size, expected_hash)?;
            Ok(Some((part_lock, file, 0)))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let file =
                platform::create_new_private(partial).map_err(|e| classify_io(partial, e))?;
            platform::set_mode(partial, 0o600)?;
            write_partial_meta(meta_path, size, expected_hash)?;
            Ok(Some((part_lock, file, 0)))
        }
        Err(err) => Err(classify_io(partial, err)),
    }
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
            Ok(file) => {
                // Hold an exclusive lock on the ephemeral partial so opportunistic sweeps skip it.
                let _ = file.try_lock();
                return Ok((candidate, file));
            }
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

/// Computes the sibling partial metadata path (`<name>.legion-part.meta`) for `target`.
pub fn part_meta_path(target: &Path) -> Result<PathBuf> {
    let file_name = validate_target_filename(target)?;
    let mut name = file_name.to_os_string();
    name.push(PART_META_SUFFIX);
    Ok(match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    })
}

/// Computes the sibling partial lock path (`<name>.legion-part.lock`) for `target`.
pub fn part_lock_path(target: &Path) -> Result<PathBuf> {
    let file_name = validate_target_filename(target)?;
    let mut name = file_name.to_os_string();
    name.push(PART_LOCK_SUFFIX);
    Ok(match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(name),
        _ => PathBuf::from(name),
    })
}

/// Returns the resumable byte offset for `target`: the partial file's current length, or `0` if no
/// safe single-linked regular partial file exists yet or another transfer currently holds its lock.
pub fn resume_offset(target: &Path) -> Result<u64> {
    let partial = part_path(target)?;
    let lock_path = part_lock_path(target)?;
    if let Ok(lock_file) = OpenOptions::new().write(true).open(&lock_path)
        && matches!(lock_file.try_lock(), Err(TryLockError::WouldBlock))
    {
        return Ok(0);
    }
    match fs::symlink_metadata(&partial) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err(Error::Conflict {
                    path: partial,
                    reason: "partial path exists and is not a regular file".to_string(),
                });
            }
            if platform::symlink_file_identity(&partial).is_ok_and(|id| id.nlink > 1) {
                return Ok(0);
            }
            Ok(meta.len())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(classify_io(&partial, err)),
    }
}

/// Returns the resumable byte offset for `target` ONLY when `<name>.legion-part` is unlocked,
/// single-linked, no longer than `expected_size`, and `<name>.legion-part.meta` records the exact
/// same `expected_size` and `expected_hash` (when given). Returns `0` otherwise.
pub fn resume_offset_for(
    target: &Path,
    expected_size: u64,
    expected_hash: Option<Hash>,
) -> Result<u64> {
    let offset = resume_offset(target)?;
    if offset == 0 || offset > expected_size {
        return Ok(0);
    }
    let meta_path = part_meta_path(target)?;
    let Some(recorded) = read_partial_meta(&meta_path) else {
        return Ok(0);
    };
    if recorded.size != expected_size {
        return Ok(0);
    }
    if let Some(hash) = expected_hash
        && recorded.hash != hash
    {
        return Ok(0);
    }
    Ok(offset)
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
                if platform::symlink_file_identity(partial).is_ok_and(|id| id.nlink > 1) {
                    return Err(Error::Conflict {
                        path: partial.to_path_buf(),
                        reason: "partial file has multiple hard links and cannot be resumed safely"
                            .to_string(),
                    });
                }
                let actual = meta.len();
                if actual > size {
                    // A stale partial file larger than the whole incoming file cannot be a valid
                    // prefix of this transfer; truncate and start cleanly from offset 0.
                    let file = OpenOptions::new()
                        .write(true)
                        .open(partial)
                        .map_err(|e| classify_io(partial, e))?;
                    if platform::open_file_identity(&file).is_ok_and(|id| id.nlink > 1) {
                        return Err(Error::Conflict {
                            path: partial.to_path_buf(),
                            reason:
                                "partial file has multiple hard links and cannot be resumed safely"
                                    .to_string(),
                        });
                    }
                    file.set_len(0).map_err(|e| classify_io(partial, e))?;
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
                if platform::open_file_identity(&file).is_ok_and(|id| id.nlink > 1) {
                    return Err(Error::Conflict {
                        path: partial.to_path_buf(),
                        reason: "partial file has multiple hard links and cannot be resumed safely"
                            .to_string(),
                    });
                }
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
        if platform::symlink_file_identity(partial).is_ok_and(|id| id.nlink > 1) {
            return Err(Error::Conflict {
                path: partial.to_path_buf(),
                reason: "partial file has multiple hard links and cannot be resumed safely"
                    .to_string(),
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
        if platform::open_file_identity(&file).is_ok_and(|id| id.nlink > 1) {
            return Err(Error::Conflict {
                path: partial.to_path_buf(),
                reason: "partial file has multiple hard links and cannot be resumed safely"
                    .to_string(),
            });
        }
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
        let err = PutSession::open(&dst, 3, hash_bytes(b"abc"), IfExists::Fail).unwrap_err();
        assert!(matches!(err, Error::Exists { .. }), "{err:?}");
        assert_eq!(fs::read(&dst).unwrap(), existing);

        // Skip: leaves existing file untouched.
        let payload = b"new bytes";
        let session = PutSession::open(
            &dst,
            payload.len() as u64,
            hash_bytes(payload),
            IfExists::Skip,
        )
        .unwrap();
        let outcome = session
            .finish(hash_bytes(payload), Cursor::new(payload), None, |_| Ok(()))
            .unwrap();
        assert!(outcome.skipped);
        assert_eq!(outcome.path, None);
        assert_eq!(fs::read(&dst).unwrap(), existing);

        // Rename: writes `Holiday (2).mp4`, then `Holiday (3).mp4`.
        let session = PutSession::open(
            &dst,
            payload.len() as u64,
            hash_bytes(payload),
            IfExists::Rename,
        )
        .unwrap();
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
                    PutSession::open(&race_dst, bytes.len() as u64, hash, IfExists::Rename)
                        .unwrap();
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
                    PutSession::open(&fail_dst, bytes.len() as u64, hash, IfExists::Fail).unwrap();
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

    #[test]
    fn claim_then_rename_fallback_works_without_hard_links_and_cleans_up_on_kill() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("photo.jpg");
        fs::write(&existing, b"original-200kb-content").unwrap();

        // Simulate a filesystem (FAT / exFAT / SMB) that supports neither `rename_no_replace`
        // nor hard links: placement uses `O_CREAT|O_EXCL` claim + rename exclusively.
        let tmp_fail = dir.path().join(".tmp-fail");
        fs::write(&tmp_fail, b"new-fail").unwrap();
        let err = place_file_with_ops(
            &tmp_fail,
            &existing,
            IfExists::Fail,
            |_| Ok(()),
            |_, _| Err(RenameNoReplaceError::Unsupported),
            platform::create_new_private,
            platform::replace_file,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Exists { .. }), "{err:?}");
        assert_eq!(fs::read(&existing).unwrap(), b"original-200kb-content");
        let _ = fs::remove_file(&tmp_fail);

        let tmp_skip = dir.path().join(".tmp-skip");
        fs::write(&tmp_skip, b"new-skip").unwrap();
        let skipped = place_file_with_ops(
            &tmp_skip,
            &existing,
            IfExists::Skip,
            |_| Ok(()),
            |_, _| Err(RenameNoReplaceError::Unsupported),
            platform::create_new_private,
            platform::replace_file,
        )
        .unwrap();
        assert_eq!(skipped, PlaceOutcome::Skipped);
        assert_eq!(fs::read(&existing).unwrap(), b"original-200kb-content");
        let _ = fs::remove_file(&tmp_skip);

        // Simulate failure / crash right between claim (`create_new`) and rename: the claim we
        // created must be removed and the pre-existing file untouched.
        let fresh = dir.path().join("fresh.bin");
        let tmp_crash = dir.path().join(".tmp-crash");
        fs::write(&tmp_crash, b"fresh-data").unwrap();
        let err = place_file_with_ops(
            &tmp_crash,
            &fresh,
            IfExists::Fail,
            |_| Ok(()),
            |_, _| Err(RenameNoReplaceError::Unsupported),
            platform::create_new_private,
            |_, to| {
                // Verify the claim exists before rename fails.
                assert!(to.exists());
                Err(Error::Io {
                    path: Some(to.to_path_buf()),
                    source: io::Error::other("simulated kill between claim and rename"),
                })
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::Io { .. }), "{err:?}");
        assert!(!fresh.exists(), "failed claim must be cleaned up");
        assert!(tmp_crash.exists(), "partial stays intact for retry");

        // Concurrent Rename race under claim-then-rename fallback (no `rename_no_replace`, no
        // hard links): 4 racing threads all land at distinct numbered files with distinct inodes.
        let race_target = dir.path().join("exfat.bin");
        let barrier = Arc::new(Barrier::new(4));
        let mut threads = Vec::new();
        for i in 0u8..4 {
            let race_target = race_target.clone();
            let barrier = Arc::clone(&barrier);
            let tmp = dir.path().join(format!(".tmp-race-{i}"));
            let payload = vec![b'0' + i; 4096];
            fs::write(&tmp, &payload).unwrap();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let placed = place_file_with_ops(
                    &tmp,
                    &race_target,
                    IfExists::Rename,
                    |_| Ok(()),
                    |_, _| Err(RenameNoReplaceError::Unsupported),
                    platform::create_new_private,
                    platform::replace_file,
                )
                .unwrap();
                match placed {
                    PlaceOutcome::Placed(path) => (path, payload),
                    PlaceOutcome::Skipped => panic!("Rename must not skip"),
                }
            }));
        }
        let mut seen_paths = std::collections::BTreeSet::new();
        let mut seen_inodes = std::collections::BTreeSet::new();
        for t in threads {
            let (path, expected) = t.join().unwrap();
            assert_eq!(fs::read(&path).unwrap(), expected);
            assert!(seen_paths.insert(path.clone()));
            if let Ok(id) = platform::symlink_file_identity(&path) {
                assert_eq!(id.nlink, 1, "placed file must have nlink=1");
                assert!(seen_inodes.insert((id.dev, id.ino)));
            }
        }
        assert_eq!(seen_paths.len(), 4);
    }

    #[test]
    fn never_resumes_into_hard_linked_partial_or_destroys_original() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x.bin");
        let original = vec![0xAAu8; 200 * 1024];
        fs::write(&target, &original).unwrap();

        // Simulate a stale partial sharing an inode with the placed target (nlink == 2).
        let partial = part_path(&target).unwrap();
        fs::hard_link(&target, &partial).unwrap();
        assert_eq!(platform::symlink_file_identity(&partial).unwrap().nlink, 2);

        // `resume_offset` must refuse to treat a multi-linked partial as resumable.
        assert_eq!(resume_offset(&target).unwrap(), 0);

        // Legacy `put` refuses to open or truncate a multi-linked partial.
        let smaller = vec![0xBBu8; 50 * 1024];
        let err = put(
            &target,
            smaller.len() as u64,
            hash_bytes(&smaller),
            0,
            Cursor::new(&smaller),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }), "{err:?}");
        assert_eq!(
            fs::read(&target).unwrap(),
            original,
            "original must never be truncated or modified"
        );

        // `PutSession::open` under Rename starts fresh in an ephemeral partial at offset 0 and
        // writes `x (2).bin` on a new inode without touching `x.bin`.
        let session = PutSession::open(
            &target,
            smaller.len() as u64,
            hash_bytes(&smaller),
            IfExists::Rename,
        )
        .unwrap();
        assert_eq!(session.offset(), 0);
        let outcome = session
            .finish(
                hash_bytes(&smaller),
                Cursor::new(&smaller),
                None,
                |_| Ok(()),
            )
            .unwrap();
        let placed = outcome.path.unwrap();
        assert_eq!(placed, dir.path().join("x (2).bin"));
        assert_eq!(fs::read(&target).unwrap(), original);
        assert_eq!(fs::read(&placed).unwrap(), smaller);
        let id_orig = platform::symlink_file_identity(&target).unwrap();
        let id_new = platform::symlink_file_identity(&placed).unwrap();
        assert!(
            !id_orig.same_file(&id_new),
            "x.bin and x (2).bin must have distinct inodes"
        );
    }

    #[test]
    fn concurrent_puts_never_share_partial_bytes_and_resume_checks_source_identity() {
        let dir = tempfile::tempdir().unwrap();
        let bytes_a = vec![b'A'; 64 * 1024];
        let bytes_b = vec![b'B'; 64 * 1024];
        let hash_a = hash_bytes(&bytes_a);
        let hash_b = hash_bytes(&bytes_b);

        // Order 1: Put A opens (holds canonical lock), Put B opens while A is open (gets private
        // ephemeral partial at offset 0), Put B finishes FIRST (takes `ord1.bin`), then Put A
        // finishes SECOND (takes `ord1 (2).bin`). Neither sees the other's bytes!
        let ord1 = dir.path().join("ord1.bin");
        let sess_a =
            PutSession::open(&ord1, bytes_a.len() as u64, hash_a, IfExists::Rename).unwrap();
        assert_eq!(sess_a.offset(), 0);
        assert_eq!(
            resume_offset_for(&ord1, bytes_a.len() as u64, Some(hash_a)).unwrap(),
            0,
            "locked partial must not advertise resume while active"
        );
        let sess_b =
            PutSession::open(&ord1, bytes_b.len() as u64, hash_b, IfExists::Rename).unwrap();
        assert_eq!(sess_b.offset(), 0);
        let out_b = sess_b
            .finish(hash_b, Cursor::new(&bytes_b), None, |_| Ok(()))
            .unwrap();
        let out_a = sess_a
            .finish(hash_a, Cursor::new(&bytes_a), None, |_| Ok(()))
            .unwrap();
        assert_eq!(out_b.path, Some(dir.path().join("ord1.bin")));
        assert_eq!(out_a.path, Some(dir.path().join("ord1 (2).bin")));
        assert_eq!(fs::read(dir.path().join("ord1.bin")).unwrap(), bytes_b);
        assert_eq!(fs::read(dir.path().join("ord1 (2).bin")).unwrap(), bytes_a);
        assert!(!part_path(&ord1).unwrap().exists());
        assert!(!part_meta_path(&ord1).unwrap().exists());
        assert!(!part_lock_path(&ord1).unwrap().exists());

        // Order 2: Put A opens, Put B opens while A is open, Put A finishes FIRST (takes
        // `ord2.bin`), then Put B finishes SECOND (takes `ord2 (2).bin`).
        let ord2 = dir.path().join("ord2.bin");
        let sess_a =
            PutSession::open(&ord2, bytes_a.len() as u64, hash_a, IfExists::Rename).unwrap();
        let sess_b =
            PutSession::open(&ord2, bytes_b.len() as u64, hash_b, IfExists::Rename).unwrap();
        let out_a = sess_a
            .finish(hash_a, Cursor::new(&bytes_a), None, |_| Ok(()))
            .unwrap();
        let out_b = sess_b
            .finish(hash_b, Cursor::new(&bytes_b), None, |_| Ok(()))
            .unwrap();
        assert_eq!(out_a.path, Some(dir.path().join("ord2.bin")));
        assert_eq!(out_b.path, Some(dir.path().join("ord2 (2).bin")));
        assert_eq!(fs::read(dir.path().join("ord2.bin")).unwrap(), bytes_a);
        assert_eq!(fs::read(dir.path().join("ord2 (2).bin")).unwrap(), bytes_b);

        // Identity check on resume: Put A is interrupted halfway (leaving `.legion-part` +
        // `.legion-part.meta` for `(size, hash_a)`). Put B with SAME size but DIFFERENT `hash_b`
        // MUST NOT resume Put A's partial, succeeds at offset 0, and leaves Put A's partial intact
        // so Put A still resumes from 32 KiB afterwards!
        let id_dst = dir.path().join("identity.bin");
        let half = 32 * 1024;
        let sess_a1 =
            PutSession::open(&id_dst, bytes_a.len() as u64, hash_a, IfExists::Rename).unwrap();
        let err = sess_a1
            .finish(hash_a, Cursor::new(&bytes_a[..half]), None, |_| Ok(()))
            .unwrap_err();
        assert!(matches!(err, Error::Io { .. }));
        assert_eq!(
            resume_offset_for(&id_dst, bytes_a.len() as u64, Some(hash_a)).unwrap(),
            half as u64
        );
        assert_eq!(
            resume_offset_for(&id_dst, bytes_b.len() as u64, Some(hash_b)).unwrap(),
            0
        );

        let sess_b =
            PutSession::open(&id_dst, bytes_b.len() as u64, hash_b, IfExists::Rename).unwrap();
        assert_eq!(
            sess_b.offset(),
            0,
            "different source hash must never resume another transfer's partial"
        );
        let out_b = sess_b
            .finish(hash_b, Cursor::new(&bytes_b), None, |_| Ok(()))
            .unwrap();
        assert_eq!(out_b.path, Some(id_dst.clone()));
        assert_eq!(fs::read(&id_dst).unwrap(), bytes_b);

        // Now Put A resumes from 32 KiB and places `identity (2).bin` cleanly.
        let sess_a2 =
            PutSession::open(&id_dst, bytes_a.len() as u64, hash_a, IfExists::Rename).unwrap();
        assert_eq!(sess_a2.offset(), half as u64);
        let out_a = sess_a2
            .finish(hash_a, Cursor::new(&bytes_a[half..]), None, |_| Ok(()))
            .unwrap();
        assert_eq!(out_a.path, Some(dir.path().join("identity (2).bin")));
        assert_eq!(
            fs::read(dir.path().join("identity (2).bin")).unwrap(),
            bytes_a
        );
    }

    #[test]
    fn sweeps_stale_put_temporaries_by_age_and_preserves_locked_ones() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join(".legion-put-123-0.legion-part");
        let fresh = dir.path().join(".legion-put-123-1.legion-part");
        let locked = dir.path().join(".legion-put-123-2.legion-part");
        let user_file = dir.path().join("keep.txt");
        fs::write(&stale, b"old").unwrap();
        fs::write(&fresh, b"new").unwrap();
        fs::write(&locked, b"active").unwrap();
        fs::write(&user_file, b"user").unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        let old_times = fs::FileTimes::new().set_modified(old_time);
        fs::set_times(&stale, old_times).unwrap();
        fs::set_times(&locked, old_times).unwrap();

        let held = OpenOptions::new().write(true).open(&locked).unwrap();
        held.try_lock().unwrap();

        let removed = sweep_stale_temps(dir.path(), STALE_TEMP_AGE);
        assert_eq!(removed, 1);
        assert!(!stale.exists(), "stale unlocked temp must be removed");
        assert!(fresh.exists(), "recent temp must remain");
        assert!(locked.exists(), "actively locked temp must remain");
        assert!(user_file.exists(), "unrelated user file must remain");
        drop(held);
    }
}
