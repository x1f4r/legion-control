//! Resumable single-file streaming (`get` and `put`) with whole-file verification.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use legion_os::atomic::{FileClaimIdentity, RenameNoReplaceError};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, classify_io};
use crate::hash::{Hash, hash_file};
use crate::path::validate_target_filename;
use crate::platform;

/// Suffix appended to a target filename while a single-file [`put`] is in progress.
pub const PART_SUFFIX: &str = ".legion-part";

/// Maximum retry attempts when a state-dir lock file is swept between `create_new` and `open` or
/// fails the live-inode check.
const LOCK_RETRY_LIMIT: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct PartialMeta {
    size: u64,
    hash: Hash,
}

/// Default state directory used when callers invoke [`PutSession::open`] or [`resume_offset`]
/// without specifying a node state directory (primarily tests).
pub fn default_xfer_state_dir() -> PathBuf {
    std::env::temp_dir().join("legion-xfer-state")
}

/// Computes a deterministic 64-hex-character key for `target` by canonicalizing its existing
/// parent directory (so aliases like `dir/../dir/a.txt` hash identically) while preserving the
/// exact final filename bytes.
fn target_state_key(target: &Path) -> String {
    let normalized = match (target.parent(), target.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            let resolved_parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
            resolved_parent.join(name)
        }
        _ => fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf()),
    };
    blake3::hash(normalized.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string()
}

/// Returns the state-directory metadata path (`<state_dir>/xfer/part/<key>.meta`) for `target`.
pub fn part_meta_state_path(state_dir: &Path, target: &Path) -> PathBuf {
    state_dir
        .join("xfer")
        .join("part")
        .join(format!("{}.meta", target_state_key(target)))
}

/// Returns the state-directory lock path (`<state_dir>/xfer/part/<key>.lock`) for `target`.
pub fn part_lock_state_path(state_dir: &Path, target: &Path) -> PathBuf {
    state_dir
        .join("xfer")
        .join("part")
        .join(format!("{}.lock", target_state_key(target)))
}

/// Returns the state-directory exclusive move-lock path (`<state_dir>/xfer/move-lock/<key>.lock`)
/// for `target`, used to serialize crossing moves of the same source/destination files.
pub fn move_lock_state_path(state_dir: &Path, target: &Path) -> PathBuf {
    state_dir
        .join("xfer")
        .join("move-lock")
        .join(format!("{}.lock", target_state_key(target)))
}

fn claim_state_paths(state_dir: &Path, target: &Path) -> (PathBuf, PathBuf) {
    let key = target_state_key(target);
    let dir = state_dir.join("xfer").join("claims");
    (
        dir.join(format!("{key}.lock")),
        dir.join(format!("{key}.claim")),
    )
}

/// Default age threshold after which abandoned `.legion-put-*` / `.legion-unpack-*` temporary
/// files and state-directory sidecars are swept opportunistically.
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
/// (such as FAT, exFAT, some SMB/FUSE mounts), it falls back to recording a claim in `state_dir`,
/// atomically claiming the candidate name with `O_CREAT | O_EXCL` (`CREATE_NEW`), and renaming the
/// verified `temp` over the claim we own. A stale empty claim left by a crash between the two
/// steps is recognized from `state_dir` and cleaned up on the next attempt.
pub(crate) fn place_file_with_state<F>(
    state_dir: &Path,
    temp: &Path,
    target: &Path,
    if_exists: IfExists,
    check_target: F,
) -> Result<PlaceOutcome>
where
    F: FnMut(&Path) -> Result<()>,
{
    place_file_with_ops(
        state_dir,
        temp,
        target,
        if_exists,
        check_target,
        platform::rename_no_replace,
        platform::create_new_private,
        platform::replace_file,
    )
}

#[allow(clippy::too_many_arguments)]
fn place_file_with_ops<F, RN, CL, RP>(
    state_dir: &Path,
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
                state_dir,
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
                state_dir,
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
                    state_dir,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimRecord {
    /// Legacy pending or v1 `(dev, ino)` marker without `ctime`/`btime`; never authorizes
    /// unlinking a file because it cannot guard against inode reuse.
    LegacyUnverified,
    /// Empty claim file created by Legion with this exact `(dev, ino, ctime, btime)` identity.
    Claimed(FileClaimIdentity),
}

fn format_claim_identity(id: &FileClaimIdentity) -> String {
    let btime_s = id
        .btime_secs
        .map_or_else(|| "-".to_owned(), |v| v.to_string());
    let btime_ns = id
        .btime_nanos
        .map_or_else(|| "-".to_owned(), |v| v.to_string());
    format!(
        "{} {} {} {} {} {}",
        id.dev, id.ino, id.ctime_secs, id.ctime_nanos, btime_s, btime_ns
    )
}

fn parse_claim_identity<'a>(parts: &mut impl Iterator<Item = &'a str>) -> Option<FileClaimIdentity> {
    let dev: u64 = parts.next()?.parse().ok()?;
    let ino: u64 = parts.next()?.parse().ok()?;
    let ctime_secs: i64 = parts.next()?.parse().ok()?;
    let ctime_nanos: u32 = parts.next()?.parse().ok()?;
    let btime_secs = match parts.next()? {
        "-" => None,
        s => Some(s.parse().ok()?),
    };
    let btime_nanos = match parts.next()? {
        "-" => None,
        s => Some(s.parse().ok()?),
    };
    if btime_secs.is_some() != btime_nanos.is_some() {
        return None;
    }
    Some(FileClaimIdentity {
        dev,
        ino,
        nlink: 1,
        ctime_secs,
        ctime_nanos,
        btime_secs,
        btime_nanos,
    })
}

fn write_claim_record(path: &Path, record: ClaimRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let text = match record {
        ClaimRecord::LegacyUnverified => return Ok(()),
        ClaimRecord::Claimed(id) => format!("claimed2 {}\n", format_claim_identity(&id)),
    };
    legion_os::atomic::write_atomic(
        path,
        text.as_bytes(),
        legion_os::atomic::WriteOptions::with_mode(0o600),
    )
    .map_err(|err| Error::Io {
        path: Some(path.to_path_buf()),
        source: io::Error::other(err),
    })
}

fn read_claim_record(path: &Path) -> Option<ClaimRecord> {
    let text = fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    match parts.next()? {
        "pending" | "claimed" => Some(ClaimRecord::LegacyUnverified),
        "claimed2" => parse_claim_identity(&mut parts).map(ClaimRecord::Claimed),
        _ => None,
    }
}

/// Checks whether `candidate` is an abandoned 0-byte claim left by a crashed claim-then-rename
/// fallback (recorded under `state_dir` and whose claim lock is free). Removes the stale empty
/// claim file and its record, returning `true` if a stale claim was cleaned up.
///
/// Never removes a non-empty file, a file with `nlink != 1`, a file whose
/// `(dev, ino, ctime, btime)` identity does not match the recorded claim, or a legacy record.
pub fn reclaim_stale_claim(state_dir: &Path, candidate: &Path) -> bool {
    let (lock_path, claim_path) = claim_state_paths(state_dir, candidate);
    if fs::symlink_metadata(&claim_path).is_err() {
        return false;
    }
    let Ok(Some(_lock)) = try_lock_state_file(&lock_path) else {
        return false;
    };
    let reclaimed = reclaim_stale_claim_locked(&claim_path, candidate);
    let _ = fs::remove_file(&lock_path);
    reclaimed
}

fn reclaim_stale_claim_locked(claim_path: &Path, candidate: &Path) -> bool {
    let Some(record) = read_claim_record(claim_path) else {
        let _ = fs::remove_file(claim_path);
        return false;
    };
    let Ok(meta) = fs::symlink_metadata(candidate) else {
        let _ = fs::remove_file(claim_path);
        return false;
    };
    if !meta.file_type().is_file() || meta.len() != 0 {
        let _ = fs::remove_file(claim_path);
        return false;
    }
    let matches = match record {
        ClaimRecord::Claimed(recorded) => platform::symlink_claim_identity(candidate)
            .is_ok_and(|id| id.nlink == 1 && id.matches_unmodified(&recorded)),
        // Never delete a file whose full `(dev, ino, ctime, btime)` identity was not recorded.
        ClaimRecord::LegacyUnverified => false,
    };
    let _ = fs::remove_file(claim_path);
    if matches {
        fs::remove_file(candidate).is_ok()
    } else {
        false
    }
}

/// Attempts to place `temp` at `candidate` without replacing an existing entry.
///
/// Returns `Ok(true)` when placed, `Ok(false)` when `candidate` already existed (untouched), or
/// `Err` on any I/O failure (removing any newly created claim before returning).
fn place_one_no_replace<RN, CL, RP>(
    state_dir: &Path,
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
        Err(RenameNoReplaceError::AlreadyExists) => {
            if reclaim_stale_claim(state_dir, candidate) {
                match rename_no_replace(temp, candidate) {
                    Ok(()) => return Ok(true),
                    Err(RenameNoReplaceError::AlreadyExists) => return Ok(false),
                    Err(RenameNoReplaceError::Io(err)) => return Err(classify_io(candidate, err)),
                    Err(RenameNoReplaceError::Unsupported) => {}
                }
            } else {
                return Ok(false);
            }
            place_via_claim(state_dir, temp, candidate, claim_new, replace_over)
        }
        Err(RenameNoReplaceError::Io(err)) => Err(classify_io(candidate, err)),
        Err(RenameNoReplaceError::Unsupported) => {
            place_via_claim(state_dir, temp, candidate, claim_new, replace_over)
        }
    }
}

fn place_via_claim<CL, RP>(
    state_dir: &Path,
    temp: &Path,
    candidate: &Path,
    claim_new: &mut CL,
    replace_over: &mut RP,
) -> Result<bool>
where
    CL: FnMut(&Path) -> io::Result<File>,
    RP: FnMut(&Path, &Path) -> Result<()>,
{
    let (lock_path, claim_path) = claim_state_paths(state_dir, candidate);
    let Some(_claim_lock) = try_lock_state_file(&lock_path)? else {
        return Ok(false);
    };
    if fs::symlink_metadata(&claim_path).is_ok() {
        reclaim_stale_claim_locked(&claim_path, candidate);
    }
    let res = match claim_new(candidate) {
        Ok(claim_file) => {
            if let Ok(id) = platform::open_claim_identity(&claim_file) {
                let _ = write_claim_record(&claim_path, ClaimRecord::Claimed(id));
            }
            drop(claim_file);
            if let Err(err) = replace_over(temp, candidate) {
                let _ = fs::remove_file(candidate);
                let _ = fs::remove_file(&claim_path);
                Err(err)
            } else {
                let _ = fs::remove_file(&claim_path);
                Ok(true)
            }
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(classify_io(candidate, err)),
    };
    let _ = fs::remove_file(&lock_path);
    res
}

/// Atomically moves `src` to `dst` via the exclusive-create claim path (`O_CREAT | O_EXCL` +
/// recorded claim identity -> replace own claim), failing with `AlreadyExists` if `dst` is taken.
pub(crate) fn place_one_via_claim(state_dir: &Path, src: &Path, dst: &Path) -> io::Result<()> {
    match place_via_claim(
        state_dir,
        src,
        dst,
        &mut platform::create_new_private,
        &mut platform::replace_file,
    ) {
        Ok(true) => Ok(()),
        Ok(false) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "destination already exists",
        )),
        Err(Error::Io { source, .. }) => Err(source),
        Err(other) => Err(io::Error::other(other.to_string())),
    }
}

/// An opened single-file receive session holding either an exclusive lock in the node's state
/// directory for the canonical resumable partial (identified by its state-directory metadata
/// `(size, hash)`) or a private unique ephemeral partial so concurrent transfers to the same
/// target path never see each other's bytes.
#[derive(Debug)]
pub struct PutSession {
    state_dir: PathBuf,
    target: PathBuf,
    partial: PathBuf,
    meta_path: Option<PathBuf>,
    part_lock: Option<File>,
    file: Option<File>,
    size: u64,
    expected_hash: Hash,
    offset: u64,
    if_exists: IfExists,
    /// `true` when this session writes to a private unique sibling partial file that is removed if
    /// dropped before committing.
    ephemeral: bool,
    /// Ownership record under `<state_dir>/xfer/temps/` for ephemeral partial files so an abrupt
    /// kill leaves an unlocked ownership record swept immediately on the next transfer.
    owned_temp: Option<OwnedTempGuard>,
    /// `true` once the session has committed or explicitly cleaned up its partial file.
    committed: bool,
}

impl Drop for PutSession {
    fn drop(&mut self) {
        self.file.take();
        if self.ephemeral && !self.committed {
            let _ = fs::remove_file(&self.partial);
        }
        self.owned_temp.take();
        self.part_lock.take();
    }
}

impl PutSession {
    /// Prepares to receive a file of `size` bytes with BLAKE3 digest `expected_hash` at `target`
    /// under `if_exists`, storing lock, partial metadata, and claim records under the default
    /// transfer state directory.
    pub fn open(
        target: &Path,
        size: u64,
        expected_hash: Hash,
        if_exists: IfExists,
    ) -> Result<Self> {
        Self::open_with_state(
            &default_xfer_state_dir(),
            target,
            size,
            expected_hash,
            if_exists,
        )
    }

    /// Prepares to receive a file of `size` bytes with BLAKE3 digest `expected_hash` at `target`
    /// under `if_exists`, storing lock, partial metadata, and claim records under `state_dir` so
    /// only the partial file itself lives beside `target`.
    pub fn open_with_state(
        state_dir: &Path,
        target: &Path,
        size: u64,
        expected_hash: Hash,
        if_exists: IfExists,
    ) -> Result<Self> {
        reclaim_stale_claim(state_dir, target);
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

        // Only sweep abandoned temporaries that carry an explicit ownership record under
        // `state_dir`. Never delete files in user folders by filename pattern and age alone.
        let _ = sweep_abandoned_temps(state_dir, PROBE_SWEEP_AGE);
        let _ = sweep_stale_state(state_dir, STALE_TEMP_AGE);

        let canonical_partial = part_path(target)?;
        let meta_path = part_meta_state_path(state_dir, target);
        let lock_path = part_lock_state_path(state_dir, target);
        match try_open_locked_partial(
            &canonical_partial,
            &meta_path,
            &lock_path,
            size,
            expected_hash,
            if_exists,
        )? {
            Some((part_lock, file, offset)) => Ok(Self {
                state_dir: state_dir.to_path_buf(),
                target: target.to_path_buf(),
                partial: canonical_partial,
                meta_path: Some(meta_path),
                part_lock: Some(part_lock),
                file: Some(file),
                size,
                expected_hash,
                offset,
                if_exists,
                ephemeral: false,
                owned_temp: None,
                committed: false,
            }),
            None => {
                let (partial, file) = create_unique_partial(target)?;
                let owned_temp = register_owned_temp(state_dir, &partial, &file);
                Ok(Self {
                    state_dir: state_dir.to_path_buf(),
                    target: target.to_path_buf(),
                    partial,
                    meta_path: None,
                    part_lock: None,
                    file: Some(file),
                    size,
                    expected_hash,
                    offset: 0,
                    if_exists,
                    ephemeral: true,
                    owned_temp,
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
        // Leave the state-directory lock file in place (swept opportunistically by age when idle)
        // so concurrent openers never race with lock-file unlinking.
        self.part_lock.take();
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
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
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

        let outcome = match place_file_with_state(
            &self.state_dir,
            &self.partial,
            &self.target,
            self.if_exists,
            check_target,
        ) {
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

/// Default age threshold after which unheld probe marker records under `<state_dir>/xfer/probes/`
/// are swept opportunistically.
pub const PROBE_SWEEP_AGE: Duration = Duration::from_secs(30);

/// Default short age threshold after which unlocked lock files in `move-lock/` and orphaned lock
/// files in `renames/` are swept.
const SHORT_LOCK_SWEEP_AGE: Duration = Duration::from_secs(30);

/// Exclusive flock-backed ownership record under `<state_dir>/xfer/temps/` for an ephemeral Legion
/// temporary file (`.legion-unpack-*.tmp`, `.legion-put-*.legion-part`, held
/// `.legion-move-probe-*.tmp`).
///
/// While the owning transfer is alive, the kernel holds the exclusive flock on `owner_path`. If
/// the process is killed (`SIGKILL`), the kernel releases the flock immediately, allowing the very
/// next transfer on this node to sweep the abandoned temporary after verifying its full
/// `(dev, ino, ctime, btime)` identity.
#[derive(Debug)]
pub struct OwnedTempGuard {
    owner_path: PathBuf,
    lock_file: Option<File>,
}

impl Drop for OwnedTempGuard {
    fn drop(&mut self) {
        if let Some(lock_file) = self.lock_file.take() {
            let _ = fs::remove_file(&self.owner_path);
            drop(lock_file);
        }
    }
}

fn owned_temp_state_path(state_dir: &Path, temp_path: &Path) -> PathBuf {
    let key = target_state_key(temp_path);
    state_dir
        .join("xfer")
        .join("temps")
        .join(format!("{key}.owner"))
}

fn probe_marker_state_path(state_dir: &Path, marker_path: &Path) -> PathBuf {
    let key = target_state_key(marker_path);
    state_dir
        .join("xfer")
        .join("probes")
        .join(format!("{key}.probe"))
}

/// Registers an ephemeral temporary file under `<state_dir>/xfer/temps/`, binding it to its
/// `(dev, ino, ctime, btime)` identity and holding an exclusive flock for the lifetime of the
/// returned guard.
pub fn register_owned_temp(
    state_dir: &Path,
    temp_path: &Path,
    file: &File,
) -> Option<OwnedTempGuard> {
    let id = platform::open_claim_identity(file).ok()?;
    let owner_path = owned_temp_state_path(state_dir, temp_path);
    let mut lock_file = try_lock_state_file(&owner_path).ok()??;
    let payload = format!(
        "v2 {}\n{}\n",
        format_claim_identity(&id),
        temp_path.to_string_lossy()
    );
    if lock_file.set_len(0).is_ok()
        && lock_file.seek(SeekFrom::Start(0)).is_ok()
        && lock_file.write_all(payload.as_bytes()).is_ok()
    {
        let _ = lock_file.flush();
        Some(OwnedTempGuard {
            owner_path,
            lock_file: Some(lock_file),
        })
    } else {
        let _ = fs::remove_file(&owner_path);
        None
    }
}

/// Records an unheld `.legion-move-probe-*.tmp` marker under `<state_dir>/xfer/probes/` bound to
/// its `(dev, ino, ctime, btime)` identity so a crashed probe is swept promptly on a subsequent
/// transfer.
pub fn record_probe_marker(state_dir: &Path, marker_path: &Path, file: &File) {
    let Ok(id) = platform::open_claim_identity(file) else {
        return;
    };
    let record_path = probe_marker_state_path(state_dir, marker_path);
    if let Some(parent) = record_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let now_secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let text = format!(
        "v2 {} {now_secs}\n{}\n",
        format_claim_identity(&id),
        marker_path.to_string_lossy()
    );
    let _ = legion_os::atomic::write_atomic(
        &record_path,
        text.as_bytes(),
        legion_os::atomic::WriteOptions::with_mode(0o600),
    );
}

/// Removes the probe marker tracking record created by [`record_probe_marker`].
pub fn forget_probe_marker(state_dir: &Path, marker_path: &Path) {
    let _ = fs::remove_file(probe_marker_state_path(state_dir, marker_path));
}

/// Sweeps every abandoned temporary file whose ownership record under `<state_dir>/xfer/temps/` is
/// unlocked (its owner died or was killed) and every stale unheld probe marker under
/// `<state_dir>/xfer/probes/` older than `probe_max_age`.
///
/// Only unlinks a target when its filename satisfies [`platform::is_sweepable_temp_name`] AND its
/// on-disk `(dev, ino, nlink == 1, ctime, btime)` matches the recorded identity. Files without a
/// matching ownership record (including leftovers from older versions) are never touched.
pub fn sweep_abandoned_temps(state_dir: &Path, probe_max_age: Duration) -> usize {
    let mut removed = 0usize;

    // 1. Flock-tracked owned temporaries (`.legion-unpack-*`, `.legion-put-*`, held probe markers):
    //    unlocked means the owning process is gone, so sweep immediately regardless of age.
    let temps_dir = state_dir.join("xfer").join("temps");
    if let Ok(entries) = fs::read_dir(&temps_dir) {
        for entry in entries.flatten() {
            let owner_path = entry.path();
            if owner_path.extension().and_then(|s| s.to_str()) != Some("owner") {
                continue;
            }
            let Ok(mut lock_file) = OpenOptions::new().read(true).write(true).open(&owner_path)
            else {
                continue;
            };
            if lock_file.try_lock().is_err() {
                continue;
            }
            let mut text = String::new();
            if lock_file.read_to_string(&mut text).is_ok() {
                let mut lines = text.lines();
                if let (Some(header), Some(path_line)) = (lines.next(), lines.next()) {
                    let mut parts = header.split_whitespace();
                    if parts.next() == Some("v2")
                        && let Some(recorded) = parse_claim_identity(&mut parts)
                    {
                        let temp_path = Path::new(path_line);
                        let file_name = temp_path.file_name().and_then(|s| s.to_str());
                        let name_ok = file_name.is_some_and(platform::is_sweepable_temp_name);
                        let is_probe =
                            file_name.is_some_and(|n| n.starts_with(".legion-move-probe-"));
                        let meta_ok = fs::symlink_metadata(temp_path).is_ok_and(|m| {
                            m.file_type().is_file() && (!is_probe || m.len() == 0)
                        });
                        let id_matches =
                            platform::symlink_claim_identity(temp_path).is_ok_and(|id| {
                                id.nlink == 1
                                    && if is_probe {
                                        id.matches_unmodified(&recorded)
                                    } else {
                                        id.matches_written_temp(&recorded)
                                    }
                            });
                        if name_ok && meta_ok && id_matches && fs::remove_file(temp_path).is_ok() {
                            removed += 1;
                        }
                    }
                }
            }
            crate::rename::unlink_held_lock(&owner_path, lock_file);
        }
    }

    // 2. Unheld probe markers older than `probe_max_age`: must be 0-byte, sweepable, and match
    //    exact `(dev, ino, nlink == 1, ctime, btime)` before unlinking.
    let probes_dir = state_dir.join("xfer").join("probes");
    if let Ok(entries) = fs::read_dir(&probes_dir) {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let max_secs = probe_max_age.as_secs();
        for entry in entries.flatten() {
            let record_path = entry.path();
            if record_path.extension().and_then(|s| s.to_str()) != Some("probe") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&record_path) else {
                let _ = fs::remove_file(&record_path);
                continue;
            };
            let mut lines = text.lines();
            let (Some(header), Some(path_line)) = (lines.next(), lines.next()) else {
                let _ = fs::remove_file(&record_path);
                continue;
            };
            let mut parts = header.split_whitespace();
            let Some("v2") = parts.next() else {
                let _ = fs::remove_file(&record_path);
                continue;
            };
            let Some(recorded) = parse_claim_identity(&mut parts) else {
                let _ = fs::remove_file(&record_path);
                continue;
            };
            let Some(Ok(created_secs)) = parts.next().map(str::parse::<u64>) else {
                let _ = fs::remove_file(&record_path);
                continue;
            };
            if now_secs.saturating_sub(created_secs) < max_secs {
                continue;
            }
            let marker_path = Path::new(path_line);
            let name_ok = marker_path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(platform::is_sweepable_temp_name);
            let meta_ok = fs::symlink_metadata(marker_path)
                .is_ok_and(|m| m.file_type().is_file() && m.len() == 0);
            let id_matches = platform::symlink_claim_identity(marker_path)
                .is_ok_and(|id| id.nlink == 1 && id.matches_unmodified(&recorded));
            if name_ok && meta_ok && id_matches && fs::remove_file(marker_path).is_ok() {
                removed += 1;
            }
            let _ = fs::remove_file(&record_path);
        }
    }

    removed
}

/// Removes abandoned lock, partial metadata, claim, and orphaned rename lock files under
/// `<state_dir>/xfer/` whose modification time is older than the applicable threshold and that are
/// not currently locked.
pub fn sweep_stale_state(state_dir: &Path, max_age: Duration) -> usize {
    let base = state_dir.join("xfer");
    let now = SystemTime::now();
    let mut removed = 0usize;
    let short_age = max_age.min(SHORT_LOCK_SWEEP_AGE);
    for (sub, threshold) in [
        ("part", max_age),
        ("claims", max_age),
        ("move-lock", short_age),
        ("renames", short_age),
    ] {
        let dir = base.join(sub);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if sub == "move-lock"
                && path.extension().and_then(|s| s.to_str()) == Some("meta")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && dir.join(format!("{stem}.lock")).exists()
            {
                continue;
            }
            if sub == "renames" {
                // Never delete pending `.json` rename intent records; only sweep orphaned `.lock`
                // files whose matching `.json` intent no longer exists.
                if path.extension().and_then(|s| s.to_str()) != Some("lock") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    && dir.join(format!("{stem}.json")).exists()
                {
                    continue;
                }
            }
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
            if age < threshold {
                continue;
            }
            let lockable = match OpenOptions::new().write(true).open(&path) {
                Ok(file) => match file.try_lock() {
                    Ok(()) => {
                        if sub == "move-lock"
                            && path.extension().and_then(|s| s.to_str()) == Some("lock")
                        {
                            let _ = fs::remove_file(path.with_extension("meta"));
                        }
                        #[cfg(unix)]
                        let ok = {
                            let ok = fs::remove_file(&path).is_ok();
                            drop(file);
                            ok
                        };
                        #[cfg(not(unix))]
                        let ok = {
                            drop(file);
                            fs::remove_file(&path).is_ok()
                        };
                        ok
                    }
                    Err(_) => false,
                },
                Err(_) => false,
            };
            if lockable {
                removed += 1;
            }
        }
    }
    removed
}

/// Writes the partial metadata record (`{size} {hash}\n`) for `target` under `state_dir`.
pub fn write_partial_meta_in_state(
    state_dir: &Path,
    target: &Path,
    size: u64,
    hash: Hash,
) -> Result<()> {
    let meta_path = part_meta_state_path(state_dir, target);
    write_partial_meta(&meta_path, size, hash)
}

fn write_partial_meta(meta_path: &Path, size: u64, hash: Hash) -> Result<()> {
    if let Some(parent) = meta_path.parent() {
        fs::create_dir_all(parent).map_err(|e| classify_io(parent, e))?;
    }
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

/// Non-blocking exclusive lock acquisition on a state-directory lock file.
///
/// Retries cleanly if another process or opportunistic sweep unlinks the lock file between
/// `create_new` and `symlink_metadata`/`open`, or if the locked file descriptor no longer matches
/// the live directory entry. Returns `Ok(Some(file))` when acquired, `Ok(None)` when another live
/// holder owns the lock (`WouldBlock`), or `Err` on unexpected I/O errors.
pub fn try_lock_state_file(lock_path: &Path) -> Result<Option<File>> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| classify_io(parent, e))?;
    }
    for _ in 0..LOCK_RETRY_LIMIT {
        let lock_file = match platform::create_new_private(lock_path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let meta = match fs::symlink_metadata(lock_path) {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(classify_io(lock_path, e)),
                };
                if !meta.file_type().is_file() {
                    return Ok(None);
                }
                match OpenOptions::new().write(true).open(lock_path) {
                    Ok(file) => file,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(classify_io(lock_path, e)),
                }
            }
            Err(err) => return Err(classify_io(lock_path, err)),
        };

        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(err)) => return Err(classify_io(lock_path, err)),
        }

        // Verify the locked inode is still the live entry at `lock_path` so an opener that raced
        // with an age sweep never proceeds on an unlinked lock inode.
        let same_live_inode = platform::open_file_identity(&lock_file)
            .ok()
            .zip(platform::symlink_file_identity(lock_path).ok())
            .is_some_and(|(a, b)| a.same_file(&b));
        if !same_live_inode {
            continue;
        }

        return Ok(Some(lock_file));
    }
    Ok(None)
}

fn try_open_locked_partial(
    partial: &Path,
    meta_path: &Path,
    lock_path: &Path,
    size: u64,
    expected_hash: Hash,
    if_exists: IfExists,
) -> Result<Option<(File, File, u64)>> {
    let Some(part_lock) = try_lock_state_file(lock_path)? else {
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
            // Existing partial belongs to a different transfer (missing or mismatched metadata) or
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

/// Returns the resumable byte offset for `target` using the default transfer state directory.
pub fn resume_offset(target: &Path) -> Result<u64> {
    resume_offset_with_state(&default_xfer_state_dir(), target)
}

/// Returns the resumable byte offset for `target`: the partial file's current length, or `0` if no
/// safe single-linked regular partial file exists yet or another transfer currently holds its lock
/// in `state_dir`.
pub fn resume_offset_with_state(state_dir: &Path, target: &Path) -> Result<u64> {
    let partial = part_path(target)?;
    let lock_path = part_lock_state_path(state_dir, target);
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

/// Returns the resumable byte offset for `target` using the default transfer state directory,
/// checking the recorded `(expected_size, expected_hash)`.
pub fn resume_offset_for(
    target: &Path,
    expected_size: u64,
    expected_hash: Option<Hash>,
) -> Result<u64> {
    resume_offset_for_with_state(
        &default_xfer_state_dir(),
        target,
        expected_size,
        expected_hash,
    )
}

/// Returns the resumable byte offset for `target` ONLY when `<name>.legion-part` is unlocked,
/// single-linked, no longer than `expected_size`, and its state-directory metadata under
/// `state_dir` records the exact same `expected_size` and `expected_hash` (when given). Returns
/// `0` otherwise.
pub fn resume_offset_for_with_state(
    state_dir: &Path,
    target: &Path,
    expected_size: u64,
    expected_hash: Option<Hash>,
) -> Result<u64> {
    let offset = resume_offset_with_state(state_dir, target)?;
    if offset == 0 || offset > expected_size {
        return Ok(0);
    }
    let meta_path = part_meta_state_path(state_dir, target);
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
            let n = match limited.read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(classify_io(&self.path, e)),
            };
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            written += n as u64;
        }
        loop {
            match writer.flush() {
                Ok(()) => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }

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
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
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
        let state = tempfile::tempdir().unwrap();
        let existing = dir.path().join("photo.jpg");
        fs::write(&existing, b"original-200kb-content").unwrap();

        // Simulate a filesystem (FAT / exFAT / SMB) that supports neither `rename_no_replace`
        // nor hard links: placement uses `O_CREAT|O_EXCL` claim + rename exclusively.
        let tmp_fail = dir.path().join(".tmp-fail");
        fs::write(&tmp_fail, b"new-fail").unwrap();
        let err = place_file_with_ops(
            state.path(),
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
            state.path(),
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
            state.path(),
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

        // Simulate a hard SIGKILL crash between `create_new` and `replace_over` that leaves BOTH
        // the 0-byte claim file at `crashed.bin` AND its `.claim` record in `state_dir` (lock
        // released by OS on process death). Subsequent `IfExists::Fail` and `IfExists::Skip` must
        // recognize and remove the stale claim instead of failing or skipping forever, while a
        // genuine user-created 0-byte file with NO claim record is NEVER removed!
        for mode in [IfExists::Fail, IfExists::Skip] {
            let crashed = dir.path().join(format!("crashed-{mode:?}.bin"));
            let claim_file = platform::create_new_private(&crashed).unwrap();
            let id = platform::open_claim_identity(&claim_file).unwrap();
            drop(claim_file);
            let (_, claim_path) = claim_state_paths(state.path(), &crashed);
            write_claim_record(&claim_path, ClaimRecord::Claimed(id)).unwrap();

            let payload = b"recovered-after-crash";
            let hash = crate::hash::hash_bytes(payload);
            let session = PutSession::open_with_state(
                state.path(),
                &crashed,
                payload.len() as u64,
                hash,
                mode,
            )
            .unwrap();
            let out = session
                .finish(hash, io::Cursor::new(payload), None, |_| Ok(()))
                .unwrap();
            assert!(!out.skipped, "stale claim must be reclaimed under {mode:?}");
            assert_eq!(out.path, Some(crashed.clone()));
            assert_eq!(fs::read(&crashed).unwrap(), payload);
        }

        // A genuine user 0-byte file (no claim record in state_dir) must NEVER be removed by Fail.
        let real_empty = dir.path().join("real-empty.txt");
        fs::write(&real_empty, b"").unwrap();
        let hash = crate::hash::hash_bytes(b"overwrite");
        let err = PutSession::open_with_state(state.path(), &real_empty, 9, hash, IfExists::Fail)
            .unwrap_err();
        assert!(matches!(err, Error::Exists { .. }), "{err:?}");
        assert!(real_empty.exists());

        // Concurrent Rename race under claim-then-rename fallback (no `rename_no_replace`, no
        // hard links): 4 racing threads all land at distinct numbered files with distinct inodes.
        let race_target = dir.path().join("exfat.bin");
        let barrier = Arc::new(Barrier::new(4));
        let mut threads = Vec::new();
        for i in 0u8..4 {
            let race_target = race_target.clone();
            let state_path = state.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            let tmp = dir.path().join(format!(".tmp-race-{i}"));
            let payload = vec![b'0' + i; 4096];
            fs::write(&tmp, &payload).unwrap();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let placed = place_file_with_ops(
                    &state_path,
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
        // No `.lock` or `.meta` sidecars are ever created in the user's folder.
        let user_entries: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            user_entries
                .iter()
                .all(|n| !n.ends_with(".lock") && !n.ends_with(".meta")),
            "sidecars must live in state_dir, not user dir: {user_entries:?}"
        );

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
    fn unowned_legion_named_user_files_are_never_deleted_by_age() {
        let user_dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let thesis_put = user_dir.path().join(".legion-put-thesis.legion-part");
        let old_unpack = user_dir.path().join(".legion-unpack-old-1.tmp");
        fs::write(&thesis_put, b"two-day-old user notes").unwrap();
        fs::write(&old_unpack, b"legacy leftover").unwrap();

        let old_time = SystemTime::now() - Duration::from_secs(48 * 60 * 60);
        let old_times = fs::FileTimes::new().set_modified(old_time);
        fs::set_times(&thesis_put, old_times).unwrap();
        fs::set_times(&old_unpack, old_times).unwrap();

        // Opening a PutSession in the same folder and sweeping abandoned temps must NEVER touch
        // files without an ownership record in `state_dir`.
        let target = user_dir.path().join("other.bin");
        let payload = b"fresh-upload";
        let hash = crate::hash::hash_bytes(payload);
        let sess = PutSession::open_with_state(
            state_dir.path(),
            &target,
            payload.len() as u64,
            hash,
            IfExists::Replace,
        )
        .unwrap();
        sess.finish(hash, io::Cursor::new(payload), None, |_| Ok(()))
            .unwrap();
        assert_eq!(sweep_abandoned_temps(state_dir.path(), Duration::ZERO), 0);
        assert_eq!(
            fs::read(&thesis_put).unwrap(),
            b"two-day-old user notes",
            "unowned .legion-put-* file must never be deleted by name and age"
        );
        assert_eq!(
            fs::read(&old_unpack).unwrap(),
            b"legacy leftover",
            "unowned .legion-unpack-* file must never be deleted by name and age"
        );
    }

    #[test]
    fn every_legion_created_user_folder_name_is_on_the_authoritative_list() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("notes.txt");
        let part_name = part_path(&target)
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (ephemeral_path, ephemeral_file) = create_unique_partial(&target).unwrap();
        drop(ephemeral_file);
        let ephemeral_name = ephemeral_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let _ = fs::remove_file(&ephemeral_path);

        let all_internal_names = [
            part_name,
            ephemeral_name,
            ".legion-unpack-42-0.tmp".to_owned(),
            ".legion-link-42-1.tmp".to_owned(),
            ".legion-CaseProbe-42-2.tmp".to_owned(),
            ".legion-NormProbe-e\u{0301}-42-3.tmp".to_owned(),
            ".legion-move-probe-42-abcd-4.tmp".to_owned(),
            ".notes.txt.legion-tmp-42-5-999".to_owned(),
        ];
        for name in &all_internal_names {
            assert!(
                platform::is_legion_internal_name(name),
                "internal name {name:?} must be recognized by is_legion_internal_name"
            );
        }

        // Visible recovery intermediates for two-step case renames and normal user files MUST NOT
        // be hidden as internal names.
        for visible in [
            "notes.txt",
            ".gitignore",
            "legion-renaming-01234567-Report.txt",
            "report.legion-part.bak",
        ] {
            assert!(
                !platform::is_legion_internal_name(visible),
                "visible name {visible:?} must NOT be hidden as internal"
            );
        }
    }

    #[test]
    fn interrupted_put_leaves_only_partial_in_user_folder_and_sidecars_in_state_dir() {
        let user_dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let target = user_dir.path().join("clip.bin");
        let payload = vec![b'X'; 32 * 1024];
        let hash = crate::hash::hash_bytes(&payload);

        let sess = PutSession::open_with_state(
            state_dir.path(),
            &target,
            payload.len() as u64,
            hash,
            IfExists::Replace,
        )
        .unwrap();
        let err = sess
            .finish(hash, io::Cursor::new(&payload[..8192]), None, |_| Ok(()))
            .unwrap_err();
        assert!(matches!(err, Error::Io { .. }));

        // User folder holds ONLY `clip.bin.legion-part` — zero `.lock` or `.meta` files!
        let user_entries: Vec<String> = fs::read_dir(user_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(user_entries, vec!["clip.bin.legion-part"]);

        // State dir holds the lock and metadata, and resume works across restart.
        assert!(part_meta_state_path(state_dir.path(), &target).exists());
        assert!(part_lock_state_path(state_dir.path(), &target).exists());
        assert_eq!(
            resume_offset_for_with_state(
                state_dir.path(),
                &target,
                payload.len() as u64,
                Some(hash),
            )
            .unwrap(),
            8192
        );

        // Age-sweeping state dir removes old unlocked sidecars cleanly.
        let lock_path = part_lock_state_path(state_dir.path(), &target);
        let old = fs::FileTimes::new()
            .set_modified(SystemTime::now() - Duration::from_secs(48 * 60 * 60));
        fs::set_times(&lock_path, old).unwrap();
        assert!(sweep_stale_state(state_dir.path(), STALE_TEMP_AGE) >= 1);
    }

    #[test]
    fn try_lock_state_file_retries_cleanly_when_lock_file_is_unlinked_under_contention() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let state_dir = tempfile::tempdir().unwrap();
        let lock_path = state_dir.path().join("xfer/part/race.lock");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_sweeper = Arc::clone(&stop);
        let lock_sweeper = lock_path.clone();

        // Simulate an aggressive sweeper unlinking the lock file while workers lock/unlock it.
        let sweeper = std::thread::spawn(move || {
            while !stop_sweeper.load(Ordering::Relaxed) {
                let _ = fs::remove_file(&lock_sweeper);
                std::thread::yield_now();
            }
        });

        for _ in 0..200 {
            let res = try_lock_state_file(&lock_path);
            assert!(
                res.is_ok(),
                "try_lock_state_file must retry on NotFound instead of failing: {res:?}"
            );
        }
        stop.store(true, Ordering::Relaxed);
        sweeper.join().unwrap();
    }

    #[test]
    fn claim_reclamation_requires_matching_file_identity_and_never_deletes_user_empty_file() {
        let state_dir = tempfile::tempdir().unwrap();
        let user_dir = tempfile::tempdir().unwrap();
        let user_empty = user_dir.path().join("notes.txt");
        fs::write(&user_empty, b"").unwrap();
        let (claim_lock_path, claim_path) = claim_state_paths(state_dir.path(), &user_empty);
        fs::create_dir_all(claim_path.parent().unwrap()).unwrap();

        // 1. Legacy `pending` and v1 `claimed <dev> <ino>` records (without ctime/btime) must
        //    NEVER delete an existing 0-byte user file.
        let real_id = platform::symlink_claim_identity(&user_empty).unwrap();
        for legacy in [
            "pending 12345\n".to_owned(),
            format!("claimed {} {}\n", real_id.dev, real_id.ino),
        ] {
            fs::write(&claim_path, &legacy).unwrap();
            reclaim_stale_claim(state_dir.path(), &user_empty);
            assert!(
                user_empty.exists(),
                "legacy claim record {legacy:?} must never delete a user's empty file"
            );
            assert!(!claim_path.exists());
        }

        // 2. Mismatched `ino`, mismatched `ctime`, or mismatched `btime` must NEVER delete the
        //    user's empty file (defends against inode reuse).
        let mut wrong_ino = real_id;
        wrong_ino.ino = real_id.ino.wrapping_add(99_999);
        let mut wrong_ctime = real_id;
        wrong_ctime.ctime_secs = real_id.ctime_secs.wrapping_sub(5);
        let mut wrong_btime = real_id;
        wrong_btime.btime_secs = Some(real_id.btime_secs.unwrap_or(0).wrapping_sub(5));
        wrong_btime.btime_nanos = Some(0);
        for bad in [wrong_ino, wrong_ctime, wrong_btime] {
            write_claim_record(&claim_path, ClaimRecord::Claimed(bad)).unwrap();
            reclaim_stale_claim(state_dir.path(), &user_empty);
            assert!(
                user_empty.exists(),
                "mismatched claim identity {bad:?} must never delete a user's empty file"
            );
            assert!(!claim_path.exists());
        }

        // 3. A `claimed2` record whose full `(dev, ino, ctime, btime)` matches a 0-byte
        //    single-link claim file DOES reclaim it and cleans up both `.claim` and `.lock`.
        write_claim_record(&claim_path, ClaimRecord::Claimed(real_id)).unwrap();
        reclaim_stale_claim(state_dir.path(), &user_empty);
        assert!(
            !user_empty.exists(),
            "matching (dev, ino, ctime, btime) 0-byte claim file should be reclaimed"
        );
        assert!(!claim_path.exists());
        assert!(!claim_lock_path.exists());
    }

    #[test]
    fn sweep_abandoned_temps_and_orphaned_rename_locks_cleans_dead_artifacts_only() {
        let state_dir = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();

        // 1. Abandoned `.legion-unpack-*.tmp` and `.legion-put-*` with unlocked v2 ownership
        //    records (as left after SIGKILL, including after bytes were written) are swept
        //    immediately regardless of age.
        let dead_unpack = dir.path().join(".legion-unpack-999-1.tmp");
        let dead_put = dir.path().join(".legion-put-999-2.legion-part");
        let mut dead_unpack_f = File::create(&dead_unpack).unwrap();
        let dead_put_f = File::create(&dead_put).unwrap();
        let id_u = platform::open_claim_identity(&dead_unpack_f).unwrap();
        let id_p = platform::open_claim_identity(&dead_put_f).unwrap();
        // Writing to an owned temp after creation updates ctime; sweep must still recognize it via
        // matching `(dev, ino, btime)` and non-decreasing `ctime`.
        dead_unpack_f.write_all(b"partial-bytes").unwrap();
        drop(dead_unpack_f);
        drop(dead_put_f);

        let temps_dir = state_dir.path().join("xfer").join("temps");
        fs::create_dir_all(&temps_dir).unwrap();
        fs::write(
            owned_temp_state_path(state_dir.path(), &dead_unpack),
            format!(
                "v2 {}\n{}\n",
                format_claim_identity(&id_u),
                dead_unpack.display()
            ),
        )
        .unwrap();
        fs::write(
            owned_temp_state_path(state_dir.path(), &dead_put),
            format!(
                "v2 {}\n{}\n",
                format_claim_identity(&id_p),
                dead_put.display()
            ),
        )
        .unwrap();

        // 2. A live `.legion-unpack-*.tmp` whose `OwnedTempGuard` is still held must NOT be swept.
        let live_unpack = dir.path().join(".legion-unpack-999-3.tmp");
        let live_f = File::create(&live_unpack).unwrap();
        let _live_guard = register_owned_temp(state_dir.path(), &live_unpack, &live_f).unwrap();
        drop(live_f);

        // 3. A user file whose path was spoofed into an ownership record, or a temp whose recorded
        //    `ctime`/`btime` does not match, must NEVER be deleted.
        let user_file = dir.path().join("precious.txt");
        let user_f = File::create(&user_file).unwrap();
        let id_user = platform::open_claim_identity(&user_f).unwrap();
        drop(user_f);
        fs::write(
            owned_temp_state_path(state_dir.path(), &user_file),
            format!(
                "v2 {}\n{}\n",
                format_claim_identity(&id_user),
                user_file.display()
            ),
        )
        .unwrap();

        let reused_probe = dir.path().join(".legion-move-probe-999-reused-1.tmp");
        let reused_f = File::create(&reused_probe).unwrap();
        let mut bad_probe_id = platform::open_claim_identity(&reused_f).unwrap();
        bad_probe_id.ctime_secs = bad_probe_id.ctime_secs.wrapping_sub(10);
        drop(reused_f);
        fs::write(
            owned_temp_state_path(state_dir.path(), &reused_probe),
            format!(
                "v2 {}\n{}\n",
                format_claim_identity(&bad_probe_id),
                reused_probe.display()
            ),
        )
        .unwrap();

        // 4. An unheld `.legion-move-probe-*.tmp` marker recorded via `record_probe_marker` is
        //    swept once its age exceeds `probe_max_age`.
        let dead_probe = dir.path().join(".legion-move-probe-999-abc-1.tmp");
        let probe_f = File::create(&dead_probe).unwrap();
        record_probe_marker(state_dir.path(), &dead_probe, &probe_f);
        drop(probe_f);

        let removed = sweep_abandoned_temps(state_dir.path(), Duration::ZERO);
        assert_eq!(removed, 3);
        assert!(!dead_unpack.exists());
        assert!(!dead_put.exists());
        assert!(!dead_probe.exists());
        assert!(live_unpack.exists(), "live locked temp must not be swept");
        assert!(user_file.exists(), "user file must never be swept");
        assert!(
            reused_probe.exists(),
            "probe with mismatched ctime must never be swept"
        );

        // 5. Orphaned `xfer/renames/*.lock` (with no matching `.json` intent) is swept by both
        //    `recover_pending_renames` and `sweep_stale_state`, while a live `.json` + `.lock`
        //    pair is preserved until recovered.
        let renames_dir = crate::rename::pending_renames_dir(state_dir.path());
        fs::create_dir_all(&renames_dir).unwrap();
        let orphan_lock = renames_dir.join("orphan.lock");
        fs::write(&orphan_lock, b"").unwrap();
        let old = fs::FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(120));
        fs::set_times(&orphan_lock, old).unwrap();
        assert_eq!(sweep_stale_state(state_dir.path(), STALE_TEMP_AGE), 1);
        assert!(!orphan_lock.exists());

        // Also test immediate orphan lock sweep inside `recover_pending_renames`.
        fs::write(&orphan_lock, b"").unwrap();
        crate::rename::recover_pending_renames(state_dir.path(), None);
        assert!(!orphan_lock.exists());
    }
}
