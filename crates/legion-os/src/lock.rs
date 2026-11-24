//! Advisory kernel file locks built on `std::fs::File` locking primitives.
//!
//! Locks are owned by the operating system kernel (`flock` on Unix, `LockFileEx` on Windows).
//! Because the kernel releases a lock automatically when its file descriptor closes or its
//! owning process dies, a lock is never broken by age and the lock file itself is never deleted
//! (unlinking a held lock file would split callers across two distinct inodes).

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{OsError, Result};

const INITIAL_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Whether an advisory kernel file lock is exclusive or shared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    /// Exclusive writer lock; conflicts with all other shared and exclusive locks.
    Exclusive,
    /// Shared reader lock; compatible with other shared locks, conflicts with exclusive locks.
    Shared,
}

/// RAII guard holding an advisory kernel lock on a file.
///
/// Dropping the guard unlocks and closes the file descriptor without deleting the underlying
/// lock file. If the holding process terminates abruptly, the kernel releases the lock.
#[derive(Debug)]
pub struct FileLock {
    file: Option<File>,
    path: PathBuf,
    mode: LockMode,
}

impl FileLock {
    /// Attempt to acquire a lock immediately without blocking.
    ///
    /// Returns [`OsError::WouldBlock`] if another owner currently holds a conflicting lock.
    pub fn try_acquire(path: impl AsRef<Path>, mode: LockMode) -> Result<Self> {
        let path = path.as_ref();
        let file = open_lock_file(path, mode)?;
        let attempt = match mode {
            LockMode::Exclusive => file.try_lock(),
            LockMode::Shared => file.try_lock_shared(),
        };
        match attempt {
            Ok(()) => Ok(Self {
                file: Some(file),
                path: path.to_path_buf(),
                mode,
            }),
            Err(TryLockError::WouldBlock) => Err(OsError::WouldBlock {
                path: path.to_path_buf(),
            }),
            Err(TryLockError::Error(err)) => Err(OsError::from_io(
                format!("failed to lock {}", path.display()),
                err,
            )),
        }
    }

    /// Acquire a lock, blocking the current thread until it becomes available.
    pub fn acquire(path: impl AsRef<Path>, mode: LockMode) -> Result<Self> {
        let path = path.as_ref();
        let file = open_lock_file(path, mode)?;
        let res = match mode {
            LockMode::Exclusive => file.lock(),
            LockMode::Shared => file.lock_shared(),
        };
        res.map_err(|err| OsError::from_io(format!("failed to lock {}", path.display()), err))?;
        Ok(Self {
            file: Some(file),
            path: path.to_path_buf(),
            mode,
        })
    }

    /// Acquire a lock, blocking up to `timeout` before failing with [`OsError::TimedOut`],
    /// creating or tightening the lock file to owner-only permissions (`0o600`) on Unix.
    pub fn acquire_private_timeout(
        path: impl AsRef<Path>,
        mode: LockMode,
        timeout: Duration,
    ) -> Result<Self> {
        let path = path.as_ref();
        let file = open_lock_file_inner(path, mode, true)?;
        Self::lock_open_file_timeout(file, path, mode, timeout)
    }

    /// Acquire a lock, blocking up to `timeout` before failing with [`OsError::TimedOut`].
    ///
    /// Uses non-blocking attempts with short backoff so timing out never leaves an orphan
    /// blocking thread holding a pending kernel wait.
    pub fn acquire_timeout(
        path: impl AsRef<Path>,
        mode: LockMode,
        timeout: Duration,
    ) -> Result<Self> {
        let path = path.as_ref();
        let file = open_lock_file(path, mode)?;
        Self::lock_open_file_timeout(file, path, mode, timeout)
    }

    fn lock_open_file_timeout(
        file: File,
        path: &Path,
        mode: LockMode,
        timeout: Duration,
    ) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let mut pause = INITIAL_POLL_INTERVAL;

        loop {
            let attempt = match mode {
                LockMode::Exclusive => file.try_lock(),
                LockMode::Shared => file.try_lock_shared(),
            };
            match attempt {
                Ok(()) => {
                    return Ok(Self {
                        file: Some(file),
                        path: path.to_path_buf(),
                        mode,
                    });
                }
                Err(TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(OsError::TimedOut {
                            detail: format!(
                                "lock {} was still held after {} ms",
                                path.display(),
                                timeout.as_millis()
                            ),
                        });
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    std::thread::sleep(pause.min(remaining));
                    pause = (pause * 2).min(MAX_POLL_INTERVAL);
                }
                Err(TryLockError::Error(err)) => {
                    return Err(OsError::from_io(
                        format!("failed to lock {}", path.display()),
                        err,
                    ));
                }
            }
        }
    }

    /// Asynchronously acquire a lock, optionally bounded by `timeout`.
    ///
    /// When `timeout` is `Some`, polls non-blockingly using Tokio's timer so dropping the future
    /// cancels cleanly without leaving a background thread blocked on the lock. When `timeout` is
    /// `None`, delegates the blocking kernel wait to Tokio's blocking pool.
    pub async fn acquire_async(
        path: impl AsRef<Path>,
        mode: LockMode,
        timeout: Option<Duration>,
    ) -> Result<Self> {
        let path_owned = path.as_ref().to_path_buf();
        let Some(timeout) = timeout else {
            return tokio::task::spawn_blocking(move || Self::acquire(&path_owned, mode))
                .await
                .map_err(|err| OsError::Platform {
                    detail: format!("blocking lock task failed: {err}"),
                })?;
        };

        let file = open_lock_file(&path_owned, mode)?;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut pause = INITIAL_POLL_INTERVAL;

        loop {
            let attempt = match mode {
                LockMode::Exclusive => file.try_lock(),
                LockMode::Shared => file.try_lock_shared(),
            };
            match attempt {
                Ok(()) => {
                    return Ok(Self {
                        file: Some(file),
                        path: path_owned,
                        mode,
                    });
                }
                Err(TryLockError::WouldBlock) => {
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(OsError::TimedOut {
                            detail: format!(
                                "lock {} was still held after {} ms",
                                path_owned.display(),
                                timeout.as_millis()
                            ),
                        });
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    tokio::time::sleep(pause.min(remaining)).await;
                    pause = (pause * 2).min(MAX_POLL_INTERVAL);
                }
                Err(TryLockError::Error(err)) => {
                    return Err(OsError::from_io(
                        format!("failed to lock {}", path_owned.display()),
                        err,
                    ));
                }
            }
        }
    }

    /// Acquire an exclusive lock, blocking until it is available.
    pub fn lock_exclusive(path: impl AsRef<Path>) -> Result<Self> {
        Self::acquire(path, LockMode::Exclusive)
    }

    /// Attempt to acquire an exclusive lock without blocking.
    pub fn try_lock_exclusive(path: impl AsRef<Path>) -> Result<Self> {
        Self::try_acquire(path, LockMode::Exclusive)
    }

    /// Acquire an exclusive lock, waiting at most `timeout`.
    pub fn lock_exclusive_timeout(path: impl AsRef<Path>, timeout: Duration) -> Result<Self> {
        Self::acquire_timeout(path, LockMode::Exclusive, timeout)
    }

    /// Acquire a shared lock, blocking until it is available.
    pub fn lock_shared(path: impl AsRef<Path>) -> Result<Self> {
        Self::acquire(path, LockMode::Shared)
    }

    /// Attempt to acquire a shared lock without blocking.
    pub fn try_lock_shared(path: impl AsRef<Path>) -> Result<Self> {
        Self::try_acquire(path, LockMode::Shared)
    }

    /// Acquire a shared lock, waiting at most `timeout`.
    pub fn lock_shared_timeout(path: impl AsRef<Path>, timeout: Duration) -> Result<Self> {
        Self::acquire_timeout(path, LockMode::Shared, timeout)
    }

    /// Path of the lock file held by this guard.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Mode (`Exclusive` or `Shared`) of this held lock.
    pub fn mode(&self) -> LockMode {
        self.mode
    }

    /// Release the lock explicitly, surfacing any unlock error to the caller.
    ///
    /// The lock file itself is kept on disk.
    pub fn unlock(mut self) -> Result<()> {
        if let Some(file) = self.file.take() {
            file.unlock().map_err(|err| {
                OsError::from_io(format!("failed to unlock {}", self.path.display()), err)
            })?;
        }
        Ok(())
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
    }
}

fn open_lock_file(path: &Path, mode: LockMode) -> Result<File> {
    open_lock_file_inner(path, mode, false)
}

fn open_lock_file_inner(path: &Path, mode: LockMode, private: bool) -> Result<File> {
    // Open read+write without truncating so both Unix flock and Windows LockFileEx work for
    // exclusive and shared modes while leaving the lock file intact.
    let mut open = OpenOptions::new();
    open.read(true).write(true).create(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let file = match open.open(path) {
        Ok(file) => file,
        Err(err) if mode == LockMode::Shared && err.kind() == io::ErrorKind::PermissionDenied => {
            OpenOptions::new()
                .read(true)
                .open(path)
                .map_err(|read_err| {
                    OsError::from_io(
                        format!("cannot open lock file {}", path.display()),
                        read_err,
                    )
                })?
        }
        Err(err) => {
            return Err(OsError::from_io(
                format!("cannot open lock file {}", path.display()),
                err,
            ));
        }
    };
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = file.metadata()
            && meta.permissions().mode() & 0o077 != 0
        {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_lock_blocks_other_handles_and_keeps_file_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("node.lock");

        let guard = FileLock::try_lock_exclusive(&lock_path).unwrap();
        assert_eq!(guard.mode(), LockMode::Exclusive);
        assert_eq!(guard.path(), lock_path.as_path());

        let err = FileLock::try_lock_exclusive(&lock_path).unwrap_err();
        assert!(matches!(err, OsError::WouldBlock { .. }));

        let err = FileLock::try_lock_shared(&lock_path).unwrap_err();
        assert!(matches!(err, OsError::WouldBlock { .. }));

        let err =
            FileLock::lock_exclusive_timeout(&lock_path, Duration::from_millis(40)).unwrap_err();
        assert!(matches!(err, OsError::TimedOut { .. }));

        drop(guard);

        // Lock file is never deleted, and a new owner can immediately acquire it.
        assert!(lock_path.exists());
        let reacquired = FileLock::try_lock_exclusive(&lock_path).unwrap();
        reacquired.unlock().unwrap();
        assert!(lock_path.exists());
    }

    #[test]
    fn shared_locks_allow_concurrent_readers_and_block_writers() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("shared.lock");

        let r1 = FileLock::try_lock_shared(&lock_path).unwrap();
        let r2 = FileLock::try_lock_shared(&lock_path).unwrap();
        assert_eq!(r1.mode(), LockMode::Shared);
        assert_eq!(r2.mode(), LockMode::Shared);

        let err = FileLock::try_lock_exclusive(&lock_path).unwrap_err();
        assert!(matches!(err, OsError::WouldBlock { .. }));

        drop(r1);
        assert!(matches!(
            FileLock::try_lock_exclusive(&lock_path),
            Err(OsError::WouldBlock { .. })
        ));

        drop(r2);
        let w = FileLock::try_lock_exclusive(&lock_path).unwrap();
        drop(w);
    }

    #[tokio::test]
    async fn async_lock_waits_for_release_and_times_out_when_held() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("async.lock");

        let held = FileLock::try_lock_exclusive(&lock_path).unwrap();
        let err = FileLock::acquire_async(
            &lock_path,
            LockMode::Exclusive,
            Some(Duration::from_millis(40)),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OsError::TimedOut { .. }));

        let path_clone = lock_path.clone();
        let waiter = tokio::spawn(async move {
            FileLock::acquire_async(
                &path_clone,
                LockMode::Exclusive,
                Some(Duration::from_secs(2)),
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(held);

        let acquired = waiter.await.unwrap().unwrap();
        assert_eq!(acquired.mode(), LockMode::Exclusive);
    }
}
