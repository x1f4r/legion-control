//! Atomic file creation, replacement, and streaming writes.
//!
//! Files are always written to a sibling temporary file in the destination directory, flushed
//! and synced to storage, and then atomically renamed over the target. On Windows, renames
//! over an existing file can transiently fail while search indexers, antivirus scanners, or
//! concurrent readers hold a handle, so [`replace_file`] retries the rename with short backoff
//! rather than ever deleting the destination first.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncWriteExt};

use crate::{OsError, Result};

#[cfg(unix)]
#[path = "atomic/unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "atomic/windows.rs"]
mod platform;

/// How long [`replace_file`] keeps retrying transient Windows sharing violations.
#[allow(dead_code)]
pub(crate) const WINDOWS_REPLACE_TIMEOUT: Duration = Duration::from_secs(3);

/// How long [`replace_with_backup`] keeps retrying transient Windows sharing violations before
/// returning the error to the caller for deferral.
#[allow(dead_code)]
pub(crate) const WINDOWS_REPLACE_BACKUP_TIMEOUT: Duration = Duration::from_millis(500);

/// Initial backoff pause between Windows rename retries.
#[allow(dead_code)]
pub(crate) const WINDOWS_INITIAL_BACKOFF: Duration = Duration::from_millis(10);

/// Maximum backoff pause between Windows rename retries.
#[allow(dead_code)]
pub(crate) const WINDOWS_MAX_BACKOFF: Duration = Duration::from_millis(100);

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Options controlling atomic file writes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WriteOptions {
    /// Unix permission bits (for example `0o600` or `0o644`) to apply when creating a new file.
    ///
    /// When the target file already exists, its existing permissions are preserved instead unless
    /// [`WriteOptions::force_mode`] is `true`, so atomic rewrites never widen or alter permissions
    /// unexpectedly. Ignored on Windows.
    pub mode: Option<u32>,
    /// When `true` and [`WriteOptions::mode`] is `Some`, apply `mode` on Unix even when replacing
    /// an existing file (used for security-sensitive files such as `authorized_keys` that must
    /// stay `0o600`). Ignored on Windows.
    pub force_mode: bool,
    /// When `true`, create the target exclusively and fail with [`OsError::AlreadyExists`] if it
    /// already exists instead of replacing it.
    ///
    /// Commits atomically via the kernel's exclusive rename (`renameat2(RENAME_NOREPLACE)` on
    /// Linux, `renamex_np(RENAME_EXCL)` on macOS, `MoveFileExW` without
    /// `MOVEFILE_REPLACE_EXISTING` on Windows), falling back to `hard_link` + unlink only when the
    /// exclusive rename is unsupported by the filesystem. If neither atomic primitive is
    /// supported, returns [`OsError::Unsupported`] rather than falling back to a replacing rename.
    pub no_replace: bool,
}

impl WriteOptions {
    /// Create options requesting a specific Unix mode for newly created files.
    pub fn with_mode(mode: u32) -> Self {
        Self {
            mode: Some(mode),
            force_mode: false,
            no_replace: false,
        }
    }

    /// Create options requesting exclusive file creation (`no_replace = true`).
    pub fn create_new() -> Self {
        Self {
            mode: None,
            force_mode: false,
            no_replace: true,
        }
    }

    /// Set the Unix permission mode applied to newly created files.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Apply [`WriteOptions::mode`] on Unix even when replacing an existing file.
    pub fn force_mode(mut self, force_mode: bool) -> Self {
        self.force_mode = force_mode;
        self
    }

    /// Require that the target path does not already exist.
    pub fn no_replace(mut self, no_replace: bool) -> Self {
        self.no_replace = no_replace;
        self
    }
}

/// Write `bytes` to `path` atomically.
///
/// The data is written to a temporary file in the same directory, flushed and synced to disk,
/// and renamed over `path`. If any step fails, the temporary file is removed and `path` is
/// left untouched.
pub fn write_atomic(path: impl AsRef<Path>, bytes: &[u8], options: WriteOptions) -> Result<()> {
    let mut cursor = io::Cursor::new(bytes);
    write_atomic_from_reader_prepared(path, &mut cursor, options, |_| Ok(()))?;
    Ok(())
}

/// Write `bytes` to `path` atomically, running `prepare` on the closed, synced sibling temporary
/// file immediately before renaming it into place.
///
/// Used when platform-specific metadata (such as a Windows ACL) must be applied to the staged file
/// before it becomes visible at `path`. If `prepare` fails, the temporary file is removed and
/// `path` is left untouched.
pub fn write_atomic_prepared<F>(
    path: impl AsRef<Path>,
    bytes: &[u8],
    options: WriteOptions,
    prepare: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let mut cursor = io::Cursor::new(bytes);
    write_atomic_from_reader_prepared(path, &mut cursor, options, prepare)?;
    Ok(())
}

/// Stream all bytes from `reader` into `path` atomically, returning the number of bytes written.
///
/// If reading, writing, syncing, or renaming fails, the temporary file is removed and the
/// target path is never left partially written.
pub fn write_atomic_from_reader<R: Read>(
    path: impl AsRef<Path>,
    reader: &mut R,
    options: WriteOptions,
) -> Result<u64> {
    write_atomic_from_reader_prepared(path, reader, options, |_| Ok(()))
}

fn write_atomic_from_reader_prepared<R: Read, F>(
    path: impl AsRef<Path>,
    reader: &mut R,
    options: WriteOptions,
    prepare: F,
) -> Result<u64>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let target = path.as_ref();
    let (parent, existing_meta) = inspect_target(target, options.no_replace)?;
    let (mut file, mut guard) =
        create_sibling_temp(parent, target, existing_meta.as_ref(), options)?;

    let written = io::copy(reader, &mut file).map_err(|err| {
        OsError::from_io(
            format!("failed writing temporary file for {}", target.display()),
            err,
        )
    })?;
    file.flush().map_err(|err| {
        OsError::from_io(
            format!("failed flushing temporary file for {}", target.display()),
            err,
        )
    })?;
    file.sync_all().map_err(|err| {
        OsError::from_io(
            format!("failed syncing temporary file for {}", target.display()),
            err,
        )
    })?;

    apply_final_permissions(&file, existing_meta.as_ref(), options, target)?;

    // Drop the open file handle before running `prepare` or committing so Windows does not reject
    // ACL updates or rename/link.
    drop(file);

    prepare(&guard.path)?;

    if options.no_replace {
        commit_exclusive(&mut guard, target)?;
    } else {
        replace_file(&guard.path, target)?;
        guard.disarm();
    }
    sync_parent_dir(parent);
    Ok(written)
}

/// Stream all bytes from an async `reader` into `path` atomically, returning the bytes written.
///
/// Dropping or cancelling the returned future mid-stream removes the temporary sibling file
/// without modifying `path`.
pub async fn write_atomic_async<R>(
    path: impl AsRef<Path>,
    reader: &mut R,
    options: WriteOptions,
) -> Result<u64>
where
    R: AsyncRead + Unpin,
{
    let target = path.as_ref();
    let (parent, existing_meta) = inspect_target(target, options.no_replace)?;
    let (std_file, mut guard) =
        create_sibling_temp(parent, target, existing_meta.as_ref(), options)?;

    let mut async_file = tokio::fs::File::from_std(std_file);
    let written = tokio::io::copy(reader, &mut async_file)
        .await
        .map_err(|err| {
            OsError::from_io(
                format!("failed writing temporary file for {}", target.display()),
                err,
            )
        })?;
    async_file.flush().await.map_err(|err| {
        OsError::from_io(
            format!("failed flushing temporary file for {}", target.display()),
            err,
        )
    })?;
    async_file.sync_all().await.map_err(|err| {
        OsError::from_io(
            format!("failed syncing temporary file for {}", target.display()),
            err,
        )
    })?;

    let std_file = async_file.into_std().await;
    apply_final_permissions(&std_file, existing_meta.as_ref(), options, target)?;
    drop(std_file);

    if options.no_replace {
        commit_exclusive_async(&mut guard, target).await?;
    } else {
        replace_file_async(&guard.path, target).await?;
        guard.disarm();
    }

    let parent_owned = parent.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || sync_parent_dir(&parent_owned)).await;
    Ok(written)
}

/// Atomically replace `to` with `from`.
///
/// On Windows, retries `PermissionDenied` and sharing/lock violations with short backoff for up
/// to three seconds so transient holds by antivirus scanners or indexers do not fail writes.
/// Never deletes `to` before renaming.
pub fn replace_file(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    replace_file_impl(from.as_ref(), to.as_ref())
}

/// Async variant of [`replace_file`] that yields to the Tokio timer between Windows retries.
pub async fn replace_file_async(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    replace_file_async_impl(from.as_ref(), to.as_ref()).await
}

/// Atomically rename `from` to `to`, refusing to replace an existing entry at `to`.
///
/// Uses the kernel's exclusive rename (`renameat2(RENAME_NOREPLACE)` on Linux,
/// `renamex_np(RENAME_EXCL)` on macOS, `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on
/// Windows), falling back to `hard_link` + unlink only when the filesystem reports that
/// exclusive rename is unsupported. Returns [`OsError::AlreadyExists`] if `to` already exists
/// and [`OsError::Unsupported`] if neither atomic primitive is supported.
pub fn rename_no_replace(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();
    commit_exclusive_with_ops(
        || rename_exclusive_impl(from, to),
        || fs::hard_link(from, to),
        || {
            let _ = fs::remove_file(from);
        },
        from,
        to,
    )
}

/// Atomically exchange two existing filesystem entries `a` and `b`.
///
/// Uses `renameat2(RENAME_EXCHANGE)` on Linux and `renamex_np(RENAME_SWAP)` on macOS. Returns
/// [`OsError::Unsupported`] on filesystems or operating systems without atomic swap support so
/// callers can fall back to [`replace_with_backup`] or claim-then-rename.
pub fn exchange_files(a: impl AsRef<Path>, b: impl AsRef<Path>) -> Result<()> {
    exchange_files_impl(a.as_ref(), b.as_ref())
}

/// Atomically replace existing `target` with `staged`, moving the previous contents of `target`
/// to `backup` in one OS operation.
///
/// Uses `ReplaceFileW` on Windows (retrying transient sharing violations) and returns
/// [`OsError::Unsupported`] on Unix where callers use [`exchange_files`] or claim-then-rename.
pub fn replace_with_backup(
    staged: impl AsRef<Path>,
    target: impl AsRef<Path>,
    backup: impl AsRef<Path>,
) -> Result<()> {
    replace_with_backup_impl(staged.as_ref(), target.as_ref(), backup.as_ref())
}

/// Returns a local filesystem change fingerprint for `path` / `meta` to detect same-size,
/// same-mtime overwrites or renames without re-hashing unchanged files.
///
/// On Unix, hashes `(dev, ino, ctime, ctime_nsec)`. On Windows, queries NTFS `FileBasicInfo`
/// (`ChangeTime`) and `FileIdInfo` (`VolumeSerialNumber`, `FileId`) via
/// `GetFileInformationByHandleEx` (falling back to `GetFileInformationByHandle` on volumes without
/// 128-bit file IDs).
pub fn file_change_id(path: impl AsRef<Path>, meta: &Metadata) -> Option<u64> {
    file_change_id_impl(path.as_ref(), meta)
}

/// RAII guard that unlinks the temporary sibling file unless [`TempFileGuard::disarm`] is called.
struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn inspect_target(target: &Path, no_replace: bool) -> Result<(&Path, Option<Metadata>)> {
    let parent = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    if no_replace {
        // Check symlink_metadata so even a dangling symlink counts as an existing target and is
        // refused before creating a temporary file.
        match fs::symlink_metadata(target) {
            Ok(_) => {
                return Err(OsError::AlreadyExists {
                    path: target.to_path_buf(),
                });
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok((parent, None)),
            Err(err) => {
                return Err(OsError::from_io(
                    format!("cannot inspect {}", target.display()),
                    err,
                ));
            }
        }
    }
    let existing_meta = match fs::metadata(target) {
        Ok(meta) => {
            if meta.is_dir() {
                return Err(OsError::InvalidArgument {
                    detail: format!("target path is a directory: {}", target.display()),
                });
            }
            Some(meta)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(OsError::from_io(
                format!("cannot inspect {}", target.display()),
                err,
            ));
        }
    };
    Ok((parent, existing_meta))
}

/// Outcome of the platform's kernel exclusive-rename primitive.
pub(crate) enum ExclusiveRenameError {
    /// Target path already exists.
    AlreadyExists,
    /// Kernel or filesystem does not support exclusive rename; hard-link fallback may be tried.
    Unsupported,
    /// Any other I/O or permission error (never triggers a fallback).
    Io(io::Error),
}

/// Atomically publish `guard.path` at `target` only if `target` does not yet exist.
///
/// Prefers the kernel's exclusive rename (`renameat2(RENAME_NOREPLACE)`, `renamex_np(RENAME_EXCL)`,
/// or `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`), falls back to `hard_link` + unlink ONLY
/// when exclusive rename is unsupported, and returns [`OsError::Unsupported`] rather than ever
/// replacing an existing file when neither atomic mechanism is available.
fn commit_exclusive(guard: &mut TempFileGuard, target: &Path) -> Result<()> {
    let from = guard.path.clone();
    commit_exclusive_with_ops(
        || rename_exclusive_impl(&from, target),
        || fs::hard_link(&from, target),
        || {
            let _ = fs::remove_file(&from);
        },
        &from,
        target,
    )?;
    guard.disarm();
    Ok(())
}

async fn commit_exclusive_async(guard: &mut TempFileGuard, target: &Path) -> Result<()> {
    let from = guard.path.clone();
    match rename_exclusive_async_impl(&from, target).await {
        Ok(()) => {
            guard.disarm();
            Ok(())
        }
        Err(ExclusiveRenameError::AlreadyExists) => Err(OsError::AlreadyExists {
            path: target.to_path_buf(),
        }),
        Err(ExclusiveRenameError::Io(err)) => Err(OsError::from_io(
            format!("cannot rename {} to {}", from.display(), target.display()),
            err,
        )),
        Err(ExclusiveRenameError::Unsupported) => {
            commit_via_hard_link(
                || fs::hard_link(&from, target),
                || {
                    let _ = fs::remove_file(&from);
                },
                &from,
                target,
            )?;
            guard.disarm();
            Ok(())
        }
    }
}

fn commit_exclusive_with_ops<FR, FL, FU>(
    rename_exclusive: FR,
    hard_link: FL,
    unlink_temp: FU,
    from: &Path,
    target: &Path,
) -> Result<()>
where
    FR: FnOnce() -> std::result::Result<(), ExclusiveRenameError>,
    FL: FnOnce() -> io::Result<()>,
    FU: FnOnce(),
{
    match rename_exclusive() {
        Ok(()) => Ok(()),
        Err(ExclusiveRenameError::AlreadyExists) => Err(OsError::AlreadyExists {
            path: target.to_path_buf(),
        }),
        Err(ExclusiveRenameError::Io(err)) => Err(OsError::from_io(
            format!("cannot rename {} to {}", from.display(), target.display()),
            err,
        )),
        Err(ExclusiveRenameError::Unsupported) => {
            commit_via_hard_link(hard_link, unlink_temp, from, target)
        }
    }
}

fn commit_via_hard_link<FL, FU>(
    hard_link: FL,
    unlink_temp: FU,
    from: &Path,
    target: &Path,
) -> Result<()>
where
    FL: FnOnce() -> io::Result<()>,
    FU: FnOnce(),
{
    match hard_link() {
        Ok(()) => {
            unlink_temp();
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(OsError::AlreadyExists {
            path: target.to_path_buf(),
        }),
        Err(err) if is_hard_link_unsupported(&err) => Err(OsError::Unsupported {
            detail: format!(
                "filesystem does not support atomic exclusive file creation for {}: {err}",
                target.display()
            ),
        }),
        Err(err) => Err(OsError::from_io(
            format!(
                "cannot link temporary file {} to {}",
                from.display(),
                target.display()
            ),
            err,
        )),
    }
}

fn is_hard_link_unsupported(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        // Only true lack-of-support errnos (never EPERM/EDQUOT, which are real failures).
        #[allow(unreachable_patterns)]
        let unsupported = matches!(
            err.raw_os_error(),
            Some(libc::EOPNOTSUPP | libc::ENOTSUP | libc::ENOSYS | libc::EXDEV | libc::EMLINK)
        );
        unsupported
    }
    #[cfg(windows)]
    {
        // ERROR_INVALID_FUNCTION (1), ERROR_NOT_SAME_DEVICE (17), ERROR_NOT_SUPPORTED (50).
        matches!(err.raw_os_error(), Some(1 | 17 | 50))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = err;
        false
    }
}

#[cfg(any(unix, windows))]
fn rename_exclusive_impl(from: &Path, to: &Path) -> std::result::Result<(), ExclusiveRenameError> {
    platform::rename_exclusive(from, to)
}

#[cfg(not(any(unix, windows)))]
fn rename_exclusive_impl(
    _from: &Path,
    _to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    Err(ExclusiveRenameError::Unsupported)
}

#[cfg(any(unix, windows))]
async fn rename_exclusive_async_impl(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    platform::rename_exclusive_async(from, to).await
}

#[cfg(not(any(unix, windows)))]
async fn rename_exclusive_async_impl(
    from: &Path,
    to: &Path,
) -> std::result::Result<(), ExclusiveRenameError> {
    rename_exclusive_impl(from, to)
}

#[cfg(any(unix, windows))]
fn exchange_files_impl(a: &Path, b: &Path) -> Result<()> {
    platform::exchange_files(a, b)
}

#[cfg(not(any(unix, windows)))]
fn exchange_files_impl(_a: &Path, _b: &Path) -> Result<()> {
    Err(OsError::Unsupported {
        detail: "atomic file exchange is not supported on this platform".to_string(),
    })
}

#[cfg(any(unix, windows))]
fn replace_with_backup_impl(staged: &Path, target: &Path, backup: &Path) -> Result<()> {
    platform::replace_with_backup(staged, target, backup)
}

#[cfg(not(any(unix, windows)))]
fn replace_with_backup_impl(_staged: &Path, _target: &Path, _backup: &Path) -> Result<()> {
    Err(OsError::Unsupported {
        detail: "replace_with_backup is not supported on this platform".to_string(),
    })
}

#[cfg(any(unix, windows))]
fn file_change_id_impl(path: &Path, meta: &Metadata) -> Option<u64> {
    platform::file_change_id(path, meta)
}

#[cfg(not(any(unix, windows)))]
fn file_change_id_impl(_path: &Path, _meta: &Metadata) -> Option<u64> {
    None
}

fn create_sibling_temp(
    parent: &Path,
    target: &Path,
    existing_meta: Option<&Metadata>,
    options: WriteOptions,
) -> Result<(File, TempFileGuard)> {
    let base_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("file");
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);

    // When explicit permissions will be set (either preserving an existing file's mode or
    // applying options.mode), open the temp file restrictively first so there is no window where
    // contents are exposed wider than requested.
    let restrictive = existing_meta.is_some() || options.mode.is_some();

    for _ in 0..64 {
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{base_name}.legion-tmp-{pid}-{seq}-{nanos}"));
        let mut open = OpenOptions::new();
        open.write(true).create_new(true);
        configure_temp_open(&mut open, restrictive);

        match open.open(&candidate) {
            Ok(file) => return Ok((file, TempFileGuard::new(candidate))),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(OsError::from_io(
                    format!("cannot create temporary file in {}", parent.display()),
                    err,
                ));
            }
        }
    }

    Err(OsError::Platform {
        detail: format!(
            "failed to allocate a unique temporary file name in {}",
            parent.display()
        ),
    })
}

#[cfg(any(unix, windows))]
fn configure_temp_open(open: &mut OpenOptions, restrictive: bool) {
    platform::configure_temp_open(open, restrictive);
}

#[cfg(not(any(unix, windows)))]
fn configure_temp_open(_open: &mut OpenOptions, _restrictive: bool) {}

#[cfg(any(unix, windows))]
fn apply_final_permissions(
    file: &File,
    existing_meta: Option<&Metadata>,
    options: WriteOptions,
    target: &Path,
) -> Result<()> {
    platform::apply_final_permissions(file, existing_meta, options, target)
}

#[cfg(not(any(unix, windows)))]
fn apply_final_permissions(
    _file: &File,
    _existing_meta: Option<&Metadata>,
    _options: WriteOptions,
    _target: &Path,
) -> Result<()> {
    Ok(())
}

#[cfg(any(unix, windows))]
fn replace_file_impl(from: &Path, to: &Path) -> Result<()> {
    platform::replace_file(from, to)
}

#[cfg(not(any(unix, windows)))]
fn replace_file_impl(from: &Path, to: &Path) -> Result<()> {
    fs::rename(from, to).map_err(|err| {
        OsError::from_io(
            format!("cannot rename {} to {}", from.display(), to.display()),
            err,
        )
    })
}

#[cfg(any(unix, windows))]
async fn replace_file_async_impl(from: &Path, to: &Path) -> Result<()> {
    platform::replace_file_async(from, to).await
}

#[cfg(not(any(unix, windows)))]
async fn replace_file_async_impl(from: &Path, to: &Path) -> Result<()> {
    replace_file_impl(from, to)
}

#[cfg(any(unix, windows))]
fn sync_parent_dir(parent: &Path) {
    platform::sync_parent_dir(parent);
}

#[cfg(not(any(unix, windows)))]
fn sync_parent_dir(_parent: &Path) {}

/// Return `true` when `err` represents a transient Windows sharing or permission conflict that
/// warrants retrying an atomic rename.
#[allow(dead_code)]
pub(crate) fn is_retryable_windows_replace_error(err: &io::Error) -> bool {
    // Win32 codes: ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32),
    // ERROR_LOCK_VIOLATION (33), ERROR_BUSY (170), ERROR_DELETE_PENDING (303).
    matches!(
        err.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::ResourceBusy
    ) || matches!(err.raw_os_error(), Some(5 | 32 | 33 | 170 | 303))
}

/// Return `true` when `err` from `ReplaceFileW` is safe to retry in place without hiding a
/// partial-failure state.
///
/// Retries sharing/access errors and `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175, where Windows
/// guarantees neither file has moved). Never retries `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (1176)
/// or `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (1177), because on 1177 the target has already been
/// moved to the backup path and retrying would fail with `ERROR_FILE_NOT_FOUND` and mask the
/// recovery state.
#[allow(dead_code)]
pub(crate) fn is_retryable_windows_replace_with_backup_error(err: &io::Error) -> bool {
    if matches!(err.raw_os_error(), Some(1176 | 1177)) {
        return false;
    }
    is_retryable_windows_replace_error(err) || err.raw_os_error() == Some(1175)
}

/// Retry `op` on retryable errors with capped exponential backoff until `timeout` elapses.
#[allow(dead_code)]
pub(crate) fn retry_sync_with_backoff<F, P>(
    mut op: F,
    mut is_retryable: P,
    timeout: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
    P: FnMut(&io::Error) -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut pause = initial_backoff;

    loop {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) => {
                let now = Instant::now();
                if !is_retryable(&err) || now >= deadline {
                    return Err(err);
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(pause.min(remaining));
                pause = (pause * 2).min(max_backoff);
            }
        }
    }
}

/// Async counterpart of [`retry_sync_with_backoff`] using Tokio sleep between attempts.
#[allow(dead_code)]
pub(crate) async fn retry_async_with_backoff<F, P>(
    mut op: F,
    mut is_retryable: P,
    timeout: Duration,
    initial_backoff: Duration,
    max_backoff: Duration,
) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
    P: FnMut(&io::Error) -> bool,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut pause = initial_backoff;

    loop {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) => {
                let now = tokio::time::Instant::now();
                if !is_retryable(&err) || now >= deadline {
                    return Err(err);
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(pause.min(remaining)).await;
                pause = (pause * 2).min(max_backoff);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::ReadBuf;

    use super::*;

    struct FailingReader {
        prefix: &'static [u8],
        sent: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.sent {
                self.sent = true;
                let n = self.prefix.len().min(buf.len());
                buf[..n].copy_from_slice(&self.prefix[..n]);
                return Ok(n);
            }
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "simulated mid-stream failure",
            ))
        }
    }

    struct PendingAfterPrefixAsyncReader {
        prefix: &'static [u8],
        sent: bool,
    }

    impl AsyncRead for PendingAfterPrefixAsyncReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if !self.sent {
                self.sent = true;
                let n = self.prefix.len().min(buf.remaining());
                buf.put_slice(&self.prefix[..n]);
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        }
    }

    fn list_dir_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_atomic_creates_and_replaces_without_leftover_temps() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.toml");

        write_atomic(&target, b"first version\n", WriteOptions::default()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first version\n");
        assert_eq!(list_dir_names(dir.path()), vec!["config.toml"]);

        write_atomic(&target, b"second version\n", WriteOptions::default()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second version\n");
        assert_eq!(list_dir_names(dir.path()), vec!["config.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_sets_mode_for_new_file_and_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("secret.txt");

        write_atomic(&target, b"v1", WriteOptions::with_mode(0o600)).unwrap();
        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Overwriting with a wider mode in options must preserve the existing target's 0o600 mode.
        write_atomic(&target, b"v2", WriteOptions::with_mode(0o644)).unwrap();
        let mode_after = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode_after, 0o600);
        assert_eq!(fs::read(&target).unwrap(), b"v2");

        // force_mode tightens an existing permissive file atomically before renaming it in place.
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic(
            &target,
            b"v3",
            WriteOptions::with_mode(0o600).force_mode(true),
        )
        .unwrap();
        let tightened = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(tightened, 0o600);
        assert_eq!(fs::read(&target).unwrap(), b"v3");
    }

    #[test]
    fn write_atomic_prepared_runs_before_replace_and_cleans_up_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("auth");
        write_atomic(&target, b"before", WriteOptions::default()).unwrap();

        let err = write_atomic_prepared(&target, b"after", WriteOptions::default(), |temp| {
            assert_ne!(temp, target.as_path());
            assert_eq!(fs::read(temp).unwrap(), b"after");
            Err(OsError::Platform {
                detail: "acl failed".to_owned(),
            })
        })
        .unwrap_err();
        assert!(matches!(err, OsError::Platform { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"before");
        assert_eq!(list_dir_names(dir.path()), vec!["auth"]);

        let mut saw_temp = false;
        write_atomic_prepared(&target, b"after", WriteOptions::default(), |temp| {
            saw_temp = temp != target.as_path() && temp.exists();
            Ok(())
        })
        .unwrap();
        assert!(saw_temp);
        assert_eq!(fs::read(&target).unwrap(), b"after");
        assert_eq!(list_dir_names(dir.path()), vec!["auth"]);
    }

    #[test]
    fn failed_sync_stream_leaves_target_intact_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("state.json");
        write_atomic(&target, b"{\"ok\":true}", WriteOptions::default()).unwrap();

        let mut reader = FailingReader {
            prefix: b"partial data",
            sent: false,
        };
        let err =
            write_atomic_from_reader(&target, &mut reader, WriteOptions::default()).unwrap_err();
        assert!(matches!(err, OsError::Io { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"{\"ok\":true}");
        assert_eq!(list_dir_names(dir.path()), vec!["state.json"]);
    }

    #[tokio::test]
    async fn cancelled_async_stream_leaves_no_partial_or_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("streamed.bin");
        write_atomic(&target, b"original", WriteOptions::default()).unwrap();

        let mut reader = PendingAfterPrefixAsyncReader {
            prefix: b"half-written",
            sent: false,
        };
        let timed_out = tokio::time::timeout(
            Duration::from_millis(40),
            write_atomic_async(&target, &mut reader, WriteOptions::default()),
        )
        .await;
        assert!(timed_out.is_err());

        // Original content is untouched and no sibling temp file remains.
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(list_dir_names(dir.path()), vec!["streamed.bin"]);
    }

    #[tokio::test]
    async fn write_atomic_async_streams_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("async.txt");
        let payload = b"streamed asynchronously";
        let mut cursor = io::Cursor::new(payload.as_slice());

        let n = write_atomic_async(&target, &mut cursor, WriteOptions::default())
            .await
            .unwrap();
        assert_eq!(n, payload.len() as u64);
        assert_eq!(fs::read(&target).unwrap(), payload);
        assert_eq!(list_dir_names(dir.path()), vec!["async.txt"]);
    }

    #[test]
    fn retry_backoff_recovers_from_transient_permission_denied() {
        let mut attempts = 0usize;
        retry_sync_with_backoff(
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "held by scanner",
                    ))
                } else {
                    Ok(())
                }
            },
            is_retryable_windows_replace_error,
            Duration::from_millis(500),
            Duration::from_millis(5),
            Duration::from_millis(20),
        )
        .unwrap();
        assert_eq!(attempts, 3);
    }

    #[test]
    fn retry_backoff_does_not_retry_non_sharing_errors() {
        let mut attempts = 0usize;
        let err = retry_sync_with_backoff(
            || {
                attempts += 1;
                Err(io::Error::new(io::ErrorKind::NotFound, "missing source"))
            },
            is_retryable_windows_replace_error,
            Duration::from_secs(1),
            Duration::from_millis(5),
            Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn replace_with_backup_retry_predicate_respects_replacefilew_partial_codes() {
        // Safe pre-move errors retry: sharing (32), lock (33), busy (170), delete-pending (303),
        // and ERROR_UNABLE_TO_REMOVE_REPLACED (1175).
        for code in [5, 32, 33, 170, 303, 1175] {
            let err = io::Error::from_raw_os_error(code);
            assert!(
                is_retryable_windows_replace_with_backup_error(&err),
                "expected Win32 code {code} to be retryable before target moves"
            );
        }
        // Partial-move states MUST NOT retry: ERROR_UNABLE_TO_MOVE_REPLACEMENT (1176) and
        // ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 (1177) must surface directly for caller recovery.
        for code in [1176, 1177] {
            let err = io::Error::from_raw_os_error(code);
            assert!(
                !is_retryable_windows_replace_with_backup_error(&err),
                "Win32 code {code} must not be blindly retried"
            );
        }
    }

    #[test]
    fn no_replace_creates_new_file_and_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("exclusive.json");

        write_atomic(&target, b"winner", WriteOptions::create_new()).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"winner");
        assert_eq!(list_dir_names(dir.path()), vec!["exclusive.json"]);

        let err = write_atomic(&target, b"loser", WriteOptions::create_new()).unwrap_err();
        assert!(matches!(err, OsError::AlreadyExists { ref path } if path == &target));
        assert_eq!(fs::read(&target).unwrap(), b"winner");
        assert_eq!(list_dir_names(dir.path()), vec!["exclusive.json"]);
    }

    #[tokio::test]
    async fn no_replace_async_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("exclusive-async.txt");
        let mut c1 = io::Cursor::new(b"first".as_slice());
        write_atomic_async(&target, &mut c1, WriteOptions::create_new())
            .await
            .unwrap();

        let mut c2 = io::Cursor::new(b"second".as_slice());
        let err = write_atomic_async(&target, &mut c2, WriteOptions::create_new())
            .await
            .unwrap_err();
        assert!(matches!(err, OsError::AlreadyExists { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"first");
        assert_eq!(list_dir_names(dir.path()), vec!["exclusive-async.txt"]);
    }

    #[test]
    fn no_replace_never_falls_back_to_overwrite_on_unsupported_or_error() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("tmp");
        let target = dir.path().join("target");
        fs::write(&from, b"new").unwrap();
        fs::write(&target, b"existing").unwrap();

        // 1. EPERM/EDQUOT on exclusive rename must return an error without trying hard_link.
        let mut link_called = false;
        let err = commit_exclusive_with_ops(
            || {
                Err(ExclusiveRenameError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "eperm",
                )))
            },
            || {
                link_called = true;
                Ok(())
            },
            || {},
            &from,
            &target,
        )
        .unwrap_err();
        assert!(!link_called);
        assert!(matches!(err, OsError::PermissionDenied { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"existing");

        // 2. Unsupported exclusive rename + unsupported hard_link returns OsError::Unsupported
        //    and leaves target untouched (never falls back to replacing rename).
        let err = commit_exclusive_with_ops(
            || Err(ExclusiveRenameError::Unsupported),
            || Err(io::Error::new(io::ErrorKind::Unsupported, "no hard_link")),
            || {},
            &from,
            &target,
        )
        .unwrap_err();
        assert!(matches!(err, OsError::Unsupported { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"existing");

        // 3. Unsupported exclusive rename falls back to hard_link when supported, unlinking temp.
        let mut unlinked = false;
        commit_exclusive_with_ops(
            || Err(ExclusiveRenameError::Unsupported),
            || Ok(()),
            || unlinked = true,
            &from,
            &target,
        )
        .unwrap();
        assert!(unlinked);

        // 4. Hard-link EPERM/EDQUOT is surfaced as PermissionDenied/Io, never misclassified as
        //    unsupported or silently overwritten.
        let err = commit_exclusive_with_ops(
            || Err(ExclusiveRenameError::Unsupported),
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "eperm on link",
                ))
            },
            || {},
            &from,
            &target,
        )
        .unwrap_err();
        assert!(matches!(err, OsError::PermissionDenied { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"existing");
    }

    #[test]
    fn no_replace_allows_exactly_one_concurrent_creator() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("race.txt");
        let threads = 8usize;
        let barrier = Arc::new(Barrier::new(threads));

        let handles: Vec<_> = (0..threads)
            .map(|idx| {
                let target = target.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    write_atomic(
                        &target,
                        format!("writer-{idx}").as_bytes(),
                        WriteOptions::create_new(),
                    )
                })
            })
            .collect();

        let mut successes = 0usize;
        let mut already_exists = 0usize;
        for h in handles {
            match h.join().unwrap() {
                Ok(()) => successes += 1,
                Err(OsError::AlreadyExists { .. }) => already_exists += 1,
                Err(other) => panic!("unexpected error in exclusive create race: {other:?}"),
            }
        }
        assert_eq!(successes, 1);
        assert_eq!(already_exists, threads - 1);
        assert_eq!(list_dir_names(dir.path()), vec!["race.txt"]);
    }
}
