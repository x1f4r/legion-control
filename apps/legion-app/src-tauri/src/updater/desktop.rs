//! Desktop install-kind detection, safe bundle unpacking, copy-while-hashing staging, and detached
//! swap-and-rollback supervisors for Linux AppImage, Linux `.deb`, macOS `.app`, and Windows NSIS.
//!
//! Every detached helper script is a fixed constant receiving paths strictly through environment
//! variables (zero shell interpolation) and runs with a cleared, minimal system environment so
//! another local user cannot hijack supervisor tools or relaunched app variables through a stale
//! AppImage mount path. Desktop installs always keep the previous copy until the new build starts
//! once and writes its startup marker, restoring the previous copy automatically if the new build
//! crashes or times out.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use legion_update::InstallKind;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

use crate::backend::{AppError, ErrorCode};

/// Maximum uncompressed bytes allowed when extracting a macOS `app-tar` archive (512 MiB).
pub const MAX_EXTRACTED_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// Maximum number of archive entries accepted when extracting a macOS `app-tar` archive.
pub const MAX_EXTRACTED_ENTRIES: usize = 20_000;

/// Maximum symlink resolution hops allowed when checking multi-hop symlink chains inside an archive.
const MAX_SYMLINK_CHAIN_DEPTH: usize = 40;

/// Default seconds the detached desktop watchdog waits for the new build to confirm startup.
pub const DEFAULT_STARTUP_WAIT_SECS: u64 = 30;

/// Grace seconds the detached desktop watchdog keeps polling for the ready marker if the launched
/// PID exits non-zero before the marker appears (for example if a wrapper exited after handing off
/// to the real app process). Clean exit code 0 keeps polling up to the full `wait_secs`.
pub const DEFAULT_EXIT_GRACE_SECS: u64 = 8;

/// Maximum bytes retained in the captured first-start log (`new-start.log`) beside update state.
pub const MAX_CAPTURED_START_LOG_BYTES: usize = 32 * 1024;

/// Maximum characters surfaced in Technical details from captured first-start output.
const MAX_STARTUP_DETAIL_CHARS: usize = 1024;

/// Filename beside update state holding the capped first-start stdout/stderr of a new build.
pub const NEW_START_LOG_FILENAME: &str = "new-start.log";

/// Filename beside update state holding the process list captured at the moment of a rollback.
pub const ROLLBACK_PS_FILENAME: &str = "rollback-ps.txt";

/// JSON lock metadata file persisted in the updates directory across staging, supervisor swap, and
/// first start of the new copy.
pub const UPDATE_LOCK_FILENAME: &str = "update.lock";

/// Kernel-locked file (`File::try_lock`) held by the staging process while creating/updating
/// `update.lock` and staging the replacement so two processes can never stage simultaneously.
pub const UPDATE_STAGE_LOCK_FILENAME: &str = "update-stage.lock";

/// Kernel-locked file held by the Windows PowerShell supervisor (`FileShare.None`) for its entire
/// lifetime so any second instance can reliably detect a live supervisor without PID guessing.
pub const UPDATE_SUPERVISOR_LOCK_FILENAME: &str = "update-supervisor.lock";

/// Flag file written by a user-initiated second launch during a headless (`verify_only`) update so
/// the supervisor opens the updated app window as soon as the swap finishes.
pub const REOPEN_REQUESTED_FILENAME: &str = "reopen-requested";

/// Environment variable set exclusively by the detached supervisor on the newly launched copy so
/// that first start is allowed to proceed while `update.lock` is still held.
pub const SUPERVISED_LAUNCH_ENV: &str = "LEGION_UPDATE_SUPERVISED_LAUNCH";

/// Maximum age in seconds before an orphaned `update.lock` is treated as stale and cleaned up.
pub const UPDATE_LOCK_MAX_AGE_SECS: u64 = 180;

/// Hand-over grace in seconds on Windows while PowerShell cold-starts before opening
/// `update-supervisor.lock`.
#[cfg(windows)]
const WINDOWS_SUPERVISOR_HANDOVER_SECS: u64 = 15;

/// Default seconds the Unix supervisor waits before swap for all processes running from the
/// install being replaced to exit before deferring cleanly without changing anything.
pub const DEFAULT_PRE_SWAP_WAIT_SECS: u64 = 15;

/// Default rollback file-in-use retry attempts (20 × 250ms = 5s).
pub const DEFAULT_ROLLBACK_RETRIES: u64 = 20;

/// D-Bus well-known name registered by `tauri-plugin-single-instance` on Linux.
#[cfg(target_os = "linux")]
const LINUX_SINGLE_INSTANCE_DBUS_NAME: &str = "com.x1f4r.legion.SingleInstance";

/// Unix socket path created by `tauri-plugin-single-instance` on macOS.
#[cfg(target_os = "macos")]
const MACOS_SINGLE_INSTANCE_SOCKET: &str = "/tmp/com_x1f4r_legion_si.sock";

/// Metadata stored inside `<updates_dir>/update.lock` for the lifetime of an in-progress update
/// (stage -> supervisor hand-over -> swap -> first-start verification / rollback).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateLockState {
    /// PID of the app process that acquired the lock and staged the update.
    pub owner_pid: u32,
    /// PID of the detached supervisor once spawned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supervisor_pid: Option<u32>,
    /// Target update sequence being installed.
    pub target_sequence: u64,
    /// Lifecycle stage: `"staging"` or `"supervising"`.
    pub stage: String,
    /// True when the update was triggered headlessly on window close (`LEGION_UPDATE_VERIFY_ONLY=1`).
    #[serde(default)]
    pub verify_only: bool,
    /// Unix epoch seconds when this lock was acquired or handed over.
    pub started_unix: u64,
}

/// Exclusive update-staging guard held while verifying, staging, and spawning the detached
/// supervisor. Dropping the guard without calling [`Self::hand_over_to_supervisor`] automatically
/// removes `update.lock` and releases the OS stage lock.
pub struct UpdateLockGuard {
    lock_path: PathBuf,
    state: UpdateLockState,
    _stage_file: File,
    handed_over: bool,
}

impl UpdateLockGuard {
    /// Hands ownership of `update.lock` over to the newly spawned supervisor `supervisor_pid` so
    /// dropping this guard keeps `update.lock` alive until the supervisor finishes.
    pub fn hand_over_to_supervisor(mut self, supervisor_pid: u32) -> Result<(), AppError> {
        self.state.supervisor_pid = Some(supervisor_pid);
        self.state.stage = "supervising".to_owned();
        self.state.started_unix = now_unix_u64();
        write_update_lock_file(&self.lock_path, &self.state)?;
        self.handed_over = true;
        Ok(())
    }
}

impl Drop for UpdateLockGuard {
    fn drop(&mut self) {
        if !self.handed_over {
            let _ = fs::remove_file(&self.lock_path);
        }
    }
}

fn now_unix_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Returns `true` when this process was launched directly by the detached update supervisor for
/// first-start verification (allowing it to start while `update.lock` is held).
pub fn is_supervised_launch() -> bool {
    std::env::var_os(SUPERVISED_LAUNCH_ENV).is_some_and(|v| !v.is_empty())
}

/// Computes the desktop `<updates_dir>` early at startup (matching Tauri's `app_data_dir()` on
/// Linux/macOS and `app_local_data_dir()` on Windows for identifier `com.x1f4r.legion`) so
/// `lib::run` can check `update.lock` BEFORE registering `tauri-plugin-single-instance`.
pub fn early_desktop_updates_dir() -> Option<PathBuf> {
    const IDENTIFIER: &str = "com.x1f4r.legion";
    #[cfg(target_os = "linux")]
    {
        let base = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".local").join("share"))
            })?;
        Some(base.join(IDENTIFIER).join("updates"))
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty())?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(IDENTIFIER)
                .join("updates"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)?;
        Some(base.join(IDENTIFIER).join("updates"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// Writes `<updates_dir>/reopen-requested` so a headless (`verify_only`) update supervisor opens
/// the updated app window as soon as the swap completes.
pub fn request_reopen_after_update(updates_dir: &Path) {
    let _ = ensure_private_dir(updates_dir);
    let _ = fs::write(updates_dir.join(REOPEN_REQUESTED_FILENAME), b"1\n");
}

#[cfg(unix)]
fn is_unix_pid_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let Some(rpid) = rustix::process::Pid::from_raw(raw) else {
        return false;
    };
    match rustix::process::test_kill_process(rpid) {
        Ok(()) => true,
        Err(err) => err == rustix::io::Errno::PERM,
    }
}

#[cfg(windows)]
fn is_windows_file_locked(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => matches!(file.try_lock(), Err(fs::TryLockError::WouldBlock)),
        Err(err) => err.raw_os_error() == Some(32) || err.kind() == io::ErrorKind::PermissionDenied,
    }
}

fn write_update_lock_file(lock_path: &Path, state: &UpdateLockState) -> Result<(), AppError> {
    let parent = lock_path.parent().ok_or_else(|| {
        AppError::new(
            ErrorCode::Internal,
            format!("Lock path {} has no parent folder.", lock_path.display()),
        )
    })?;
    ensure_private_dir(parent)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Could not serialize update lock: {err}"),
        )
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create temp lock file: {err}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tmp
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600));
    }
    tmp.write_all(&bytes)
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not write update lock: {err}"),
            )
        })?;
    let mut pending = tmp;
    for attempt in 0..5u8 {
        match pending.persist(lock_path) {
            Ok(_) => return Ok(()),
            Err(err) => {
                if attempt < 4 {
                    pending = err.file;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!("Could not persist update lock: {}", err.error),
                ));
            }
        }
    }
    Ok(())
}

/// Inspects `<updates_dir>/update.lock` (plus OS stage/supervisor locks) without acquiring staging
/// ownership. Returns `Some(state)` when an update is actively staging or being supervised, or
/// `None` (cleaning up any stale lock file) when no update is running.
pub fn inspect_active_update_lock(updates_dir: &Path) -> Option<UpdateLockState> {
    let lock_path = updates_dir.join(UPDATE_LOCK_FILENAME);
    let stage_path = updates_dir.join(UPDATE_STAGE_LOCK_FILENAME);

    // 1. Check whether another process holds the kernel stage lock right now.
    if stage_path.is_file()
        && let Ok(file) = OpenOptions::new().read(true).write(true).open(&stage_path)
    {
        match file.try_lock() {
            Err(fs::TryLockError::WouldBlock) => {
                if let Ok(bytes) = fs::read(&lock_path)
                    && let Ok(state) = serde_json::from_slice::<UpdateLockState>(&bytes)
                {
                    return Some(state);
                }
                return Some(UpdateLockState {
                    owner_pid: 0,
                    supervisor_pid: None,
                    target_sequence: 0,
                    stage: "staging".to_owned(),
                    verify_only: false,
                    started_unix: now_unix_u64(),
                });
            }
            Ok(()) => {
                drop(file);
            }
            Err(_) => {}
        }
    }

    inspect_supervisor_lock_without_stage_probe(updates_dir)
}

fn inspect_supervisor_lock_without_stage_probe(updates_dir: &Path) -> Option<UpdateLockState> {
    let lock_path = updates_dir.join(UPDATE_LOCK_FILENAME);

    #[cfg(windows)]
    {
        let sup_lock_path = updates_dir.join(UPDATE_SUPERVISOR_LOCK_FILENAME);
        if is_windows_file_locked(&sup_lock_path) {
            if let Ok(bytes) = fs::read(&lock_path)
                && let Ok(state) = serde_json::from_slice::<UpdateLockState>(&bytes)
            {
                return Some(state);
            }
            return Some(UpdateLockState {
                owner_pid: 0,
                supervisor_pid: None,
                target_sequence: 0,
                stage: "supervising".to_owned(),
                verify_only: false,
                started_unix: now_unix_u64(),
            });
        }
    }

    let bytes = match fs::read(&lock_path) {
        Ok(b) => b,
        Err(_) => return None,
    };
    let state = match serde_json::from_slice::<UpdateLockState>(&bytes) {
        Ok(s) => s,
        Err(_) => {
            // Treat a freshly created unreadable file (< 5s) as active; otherwise clean stale junk.
            let recently_modified = fs::metadata(&lock_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|d| d.as_secs() < 5);
            if recently_modified {
                return Some(UpdateLockState {
                    owner_pid: 0,
                    supervisor_pid: None,
                    target_sequence: 0,
                    stage: "staging".to_owned(),
                    verify_only: false,
                    started_unix: now_unix_u64(),
                });
            }
            let _ = fs::remove_file(&lock_path);
            return None;
        }
    };

    let age = now_unix_u64().saturating_sub(state.started_unix);
    if age >= UPDATE_LOCK_MAX_AGE_SECS {
        let _ = fs::remove_file(&lock_path);
        return None;
    }

    #[cfg(unix)]
    {
        let alive = match state.stage.as_str() {
            "supervising" => state.supervisor_pid.is_some_and(is_unix_pid_alive),
            _ => is_unix_pid_alive(state.owner_pid),
        };
        if alive {
            Some(state)
        } else {
            let _ = fs::remove_file(&lock_path);
            None
        }
    }
    #[cfg(windows)]
    {
        // On Windows, `update-stage.lock` (checked by caller) and `update-supervisor.lock` (checked
        // above) are kernel-enforced. Keep a short hand-over window while PowerShell cold-starts
        // before it opens `update-supervisor.lock`.
        if state.stage == "supervising" && age < WINDOWS_SUPERVISOR_HANDOVER_SECS {
            Some(state)
        } else {
            let _ = fs::remove_file(&lock_path);
            None
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = state;
        None
    }
}

/// Atomically acquires the exclusive update lock in `updates_dir` so only one process may stage,
/// hand off to a supervisor, swap, and verify first start. Returns `Ok(None)` immediately when
/// another instance or supervisor already holds an active update lock.
pub fn try_acquire_update_lock(
    updates_dir: &Path,
    target_sequence: u64,
    verify_only: bool,
) -> Result<Option<UpdateLockGuard>, AppError> {
    ensure_private_dir(updates_dir)?;
    let stage_path = updates_dir.join(UPDATE_STAGE_LOCK_FILENAME);
    let mut opts = OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let stage_file = opts.open(&stage_path).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not open update stage lock: {err}"),
        )
    })?;
    match stage_file.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(None),
        Err(err) => {
            return Err(AppError::new(
                ErrorCode::Failed,
                format!("Could not acquire update stage lock: {err}"),
            ));
        }
    }

    // While holding `stage_file`, verify no live supervisor already owns `update.lock`.
    if inspect_supervisor_lock_without_stage_probe(updates_dir).is_some() {
        return Ok(None);
    }

    let lock_path = updates_dir.join(UPDATE_LOCK_FILENAME);
    let state = UpdateLockState {
        owner_pid: std::process::id(),
        supervisor_pid: None,
        target_sequence,
        stage: "staging".to_owned(),
        verify_only,
        started_unix: now_unix_u64(),
    };
    write_update_lock_file(&lock_path, &state)?;
    Ok(Some(UpdateLockGuard {
        lock_path,
        state,
        _stage_file: stage_file,
        handed_over: false,
    }))
}

/// Fixed minimal system `PATH` passed to the Unix swap supervisor shell itself.
#[cfg(unix)]
const SUPERVISOR_MINIMAL_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";

/// Fallback sanitized `PATH` passed to the relaunched desktop app if the user had no safe entries.
#[cfg(unix)]
const RELAUNCH_FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// Detected installation mechanism on the current desktop host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopInstall {
    /// Running from a Linux AppImage (`$APPIMAGE` points to the executable image).
    LinuxAppImage {
        /// Full path to the running `.AppImage` file.
        appimage_path: PathBuf,
    },
    /// Running from a Debian/Ubuntu `.deb` package (never self-replaces).
    LinuxDeb {
        /// Installed Debian package name reported by `dpkg-query`.
        package: String,
    },
    /// Running from a pacman package on Arch/CachyOS (never self-replaces).
    LinuxPacman {
        /// Installed pacman package name.
        package: String,
    },
    /// Running from a macOS `.app` bundle (`<dir>/Legion.app`).
    MacosAppBundle {
        /// Root `.app` bundle directory.
        bundle_dir: PathBuf,
        /// Relative executable path inside `bundle_dir` (for example `Contents/MacOS/legion-app`).
        executable_rel: PathBuf,
    },
    /// Running from a Windows NSIS installation directory.
    WindowsNsis {
        /// Installation directory holding the app binary.
        install_dir: PathBuf,
        /// Full executable path to relaunch after installation.
        exe_path: PathBuf,
    },
    /// Running from an unpackaged or development build (checks updates, does not self-replace).
    Standalone {
        /// Expected artifact kind for update checking/display.
        check_kind: InstallKind,
        /// Plain explanation of why in-place replacement is disabled for this launch.
        reason: String,
    },
}

impl DesktopInstall {
    /// Returns the `legion-update` [`InstallKind`] to look up in an update set's manifest.
    pub fn artifact_kind(&self) -> InstallKind {
        match self {
            Self::LinuxAppImage { .. } => InstallKind::AppImage,
            Self::LinuxDeb { .. } => InstallKind::Deb,
            Self::LinuxPacman { .. } => InstallKind::AppImage,
            Self::MacosAppBundle { .. } => InstallKind::AppTar,
            Self::WindowsNsis { .. } => InstallKind::Nsis,
            Self::Standalone { check_kind, .. } => *check_kind,
        }
    }

    /// Returns a short lowercase label for diagnostics and Technical details.
    pub fn label(&self) -> &'static str {
        match self {
            Self::LinuxAppImage { .. } => "appimage",
            Self::LinuxDeb { .. } => "deb",
            Self::LinuxPacman { .. } => "pacman",
            Self::MacosAppBundle { .. } => "app-tar",
            Self::WindowsNsis { .. } => "nsis",
            Self::Standalone { .. } => "standalone",
        }
    }

    /// Whether this installation supports automatic in-place replacement.
    pub fn can_self_install(&self) -> bool {
        matches!(
            self,
            Self::LinuxAppImage { .. } | Self::MacosAppBundle { .. } | Self::WindowsNsis { .. }
        )
    }

    /// Whether the updater should download the selected artifact (true for self-installers and
    /// `.deb`, false for `pacman` and `standalone` which cannot use a downloaded bundle).
    pub fn should_download_artifact(&self) -> bool {
        matches!(
            self,
            Self::LinuxAppImage { .. }
                | Self::LinuxDeb { .. }
                | Self::MacosAppBundle { .. }
                | Self::WindowsNsis { .. }
        )
    }

    /// Formats the manual install command for package-managed installs (such as `.deb`).
    pub fn manual_command_for(&self, verified_artifact_path: Option<&Path>) -> Option<String> {
        match self {
            Self::LinuxDeb { .. } => {
                let path = verified_artifact_path?;
                Some(format!("sudo apt-get install {}", quote_posix_arg(path)))
            }
            Self::LinuxPacman { package } => Some(format!(
                "Update the {package} package with your package manager."
            )),
            _ => None,
        }
    }
}

/// Ensures `dir` exists and is private to the current user (`0o700` on Unix).
pub fn ensure_private_dir(dir: &Path) -> Result<(), AppError> {
    fs::create_dir_all(dir).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create private folder {}: {err}", dir.display()),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not restrict permissions on {}: {err}", dir.display()),
            )
        })?;
    }
    Ok(())
}

/// Streams `src` into a temporary file beside `dst` while computing SHA-256 and verifying the exact
/// expected byte length and hex digest, then atomically persists `dst` with `unix_mode`.
///
/// Guarantees that every downstream installer works exclusively from the exact bytes verified,
/// closing TOCTOU gaps between verification and execution.
pub fn copy_and_verify_sha256(
    src: &Path,
    dst: &Path,
    expected_sha256: &str,
    expected_size: u64,
    unix_mode: u32,
) -> Result<(), AppError> {
    let src_meta = fs::symlink_metadata(src).map_err(|err| {
        AppError::new(
            ErrorCode::NotFound,
            format!("Could not inspect {}: {err}", src.display()),
        )
    })?;
    if !src_meta.file_type().is_file() {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!(
                "Refusing to stage {} because it is not a regular file.",
                src.display()
            ),
        ));
    }

    let parent = dst.parent().ok_or_else(|| {
        AppError::new(
            ErrorCode::Internal,
            format!("Destination path {} has no parent folder.", dst.display()),
        )
    })?;
    fs::create_dir_all(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not prepare staging folder {}: {err}",
                parent.display()
            ),
        )
    })?;

    let mut reader = fs::File::open(src).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not open {}: {err}", src.display()),
        )
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not create staging file in {}: {err}",
                parent.display()
            ),
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(unix_mode & 0o0777))
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not set permissions on staged file: {err}"),
                )
            })?;
    }
    #[cfg(not(unix))]
    let _ = unix_mode;

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut copied: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not read {}: {err}", src.display()),
            )
        })?;
        if n == 0 {
            break;
        }
        copied = copied.saturating_add(n as u64);
        if copied > expected_size {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Artifact {} grew beyond its signed size ({expected_size} bytes) while staging.",
                    src.display()
                ),
            ));
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n]).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not write staged file {}: {err}", dst.display()),
            )
        })?;
    }

    if copied != expected_size {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!(
                "Artifact {} size changed while staging: expected {expected_size} bytes, copied {copied} bytes.",
                src.display()
            ),
        ));
    }

    let mut actual_hex = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(&mut actual_hex, "{b:02x}");
    }
    if actual_hex != expected_sha256 {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!(
                "SHA-256 mismatch while staging {}: expected {expected_sha256}, got {actual_hex}.",
                src.display()
            ),
        ));
    }

    tmp.as_file().sync_all().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not flush staged file {}: {err}", dst.display()),
        )
    })?;
    tmp.persist(dst).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not finalize {}: {}", dst.display(), err.error),
        )
    })?;
    Ok(())
}

/// Detects how this desktop build was installed.
pub fn detect_desktop_install() -> DesktopInstall {
    #[cfg(target_os = "linux")]
    {
        detect_linux_install()
    }
    #[cfg(target_os = "macos")]
    {
        detect_macos_install()
    }
    #[cfg(target_os = "windows")]
    {
        detect_windows_install()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        DesktopInstall::Standalone {
            check_kind: InstallKind::AppImage,
            reason: "Automatic replacement is not supported on this platform.".to_owned(),
        }
    }
}

#[cfg(target_os = "linux")]
fn detect_linux_install() -> DesktopInstall {
    if let Some(raw) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        let path = PathBuf::from(raw);
        if path.is_file() {
            return DesktopInstall::LinuxAppImage {
                appimage_path: path,
            };
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(pkg) = query_dpkg_owner(&exe) {
            return DesktopInstall::LinuxDeb { package: pkg };
        }
        if let Some(pkg) = query_pacman_owner(&exe) {
            return DesktopInstall::LinuxPacman { package: pkg };
        }
    }

    DesktopInstall::Standalone {
        check_kind: InstallKind::AppImage,
        reason: "This copy is not running as an AppImage or installed package, so it cannot replace itself.".to_owned(),
    }
}

#[cfg(target_os = "linux")]
fn query_dpkg_owner(exe: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/dpkg-query")
        .arg("-S")
        .arg(exe)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let pkg = text.lines().next()?.split(':').next()?.trim();
    if pkg.is_empty() {
        None
    } else {
        Some(pkg.to_owned())
    }
}

#[cfg(target_os = "linux")]
fn query_pacman_owner(exe: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/pacman")
        .args(["-Qoq"])
        .arg(exe)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pkg = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if pkg.is_empty() { None } else { Some(pkg) }
}

#[cfg(target_os = "macos")]
fn detect_macos_install() -> DesktopInstall {
    if let Ok(exe) = std::env::current_exe()
        && let Some((bundle_dir, executable_rel)) = find_macos_app_bundle(&exe)
    {
        return DesktopInstall::MacosAppBundle {
            bundle_dir,
            executable_rel,
        };
    }
    DesktopInstall::Standalone {
        check_kind: InstallKind::AppTar,
        reason: "This copy is not running from a macOS .app bundle, so it cannot replace itself."
            .to_owned(),
    }
}

/// Locates the enclosing `.app` bundle directory and relative executable path for a macOS binary
/// (`.../Legion.app/Contents/MacOS/<bin>`).
pub fn find_macos_app_bundle(exe: &Path) -> Option<(PathBuf, PathBuf)> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents_dir = macos_dir.parent()?;
    if contents_dir.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let bundle_dir = contents_dir.parent()?;
    if bundle_dir.extension()?.to_str()? != "app" {
        return None;
    }
    let rel = exe.strip_prefix(bundle_dir).ok()?.to_path_buf();
    Some((bundle_dir.to_path_buf(), rel))
}

#[cfg(target_os = "windows")]
fn detect_windows_install() -> DesktopInstall {
    if let Ok(exe) = std::env::current_exe()
        && let Some(install_dir) = exe.parent()
    {
        return DesktopInstall::WindowsNsis {
            install_dir: install_dir.to_path_buf(),
            exe_path: exe,
        };
    }
    DesktopInstall::Standalone {
        check_kind: InstallKind::Nsis,
        reason: "Could not locate the installed Windows application directory.".to_owned(),
    }
}

/// Safely extracts a verified macOS `.tar.gz` (`app-tar`) archive into `dest_dir`, refusing path
/// traversal, absolute paths, `GNUSparse`, hard links, device nodes, escaping or looping multi-hop
/// symlink chains, writing through symlink parents, special permission bits, excessive entry
/// counts, and oversized actual extracted byte counts.
///
/// Returns the single top-level `.app` bundle directory inside `dest_dir`.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn extract_macos_app_tar(tar_gz_path: &Path, dest_dir: &Path) -> Result<PathBuf, AppError> {
    extract_macos_app_tar_with_limit(
        tar_gz_path,
        dest_dir,
        MAX_EXTRACTED_BUNDLE_BYTES,
        MAX_EXTRACTED_ENTRIES,
    )
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn extract_macos_app_tar_with_limit(
    tar_gz_path: &Path,
    dest_dir: &Path,
    max_bytes: u64,
    max_entries: usize,
) -> Result<PathBuf, AppError> {
    use flate2::read::GzDecoder;
    use tar::{Archive, EntryType};

    ensure_private_dir(dest_dir)?;
    let canonical_root = fs::canonicalize(dest_dir).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not resolve staging folder {}: {err}",
                dest_dir.display()
            ),
        )
    })?;

    let file = fs::File::open(tar_gz_path).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not open update archive {}: {err}",
                tar_gz_path.display()
            ),
        )
    })?;
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    archive.set_preserve_permissions(false);
    archive.set_overwrite(false);

    let mut total_written: u64 = 0;
    let mut entry_count: usize = 0;
    let mut pending_symlinks: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    let entries = archive.entries().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not read update archive entries: {err}"),
        )
    })?;

    for entry_res in entries {
        entry_count += 1;
        if entry_count > max_entries {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("Update archive exceeds the {max_entries}-entry safety limit."),
            ));
        }

        let mut entry = entry_res.map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Corrupt entry in update archive: {err}"),
            )
        })?;
        let entry_type = entry.header().entry_type();
        if matches!(entry_type, EntryType::XGlobalHeader | EntryType::XHeader) {
            continue;
        }

        let rel_path = entry
            .path()
            .map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Invalid path in update archive: {err}"),
                )
            })?
            .into_owned();
        let clean_rel = validate_tar_rel_path(&rel_path)?;

        // Check no previously-recorded symlink sits on any ancestor of `clean_rel`.
        for ancestor in clean_rel.ancestors().skip(1) {
            if !ancestor.as_os_str().is_empty() && pending_symlinks.contains_key(ancestor) {
                return Err(AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing archive entry {} whose parent {} is a symlink.",
                        clean_rel.display(),
                        ancestor.display()
                    ),
                ));
            }
        }
        if pending_symlinks.contains_key(&clean_rel) {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!("Refusing duplicate archive path {}.", clean_rel.display()),
            ));
        }

        match entry_type {
            EntryType::Directory => {
                ensure_safe_dir_components(&canonical_root, &clean_rel)?;
            }
            EntryType::Regular => {
                let header_size = entry.header().size().map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not read archive entry size: {err}"),
                    )
                })?;
                if total_written.saturating_add(header_size) > max_bytes {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Extracted macOS bundle exceeds the {max_bytes}-byte safety limit."
                        ),
                    ));
                }

                if let Some(parent_rel) = clean_rel.parent()
                    && !parent_rel.as_os_str().is_empty()
                {
                    ensure_safe_dir_components(&canonical_root, parent_rel)?;
                }
                let target = canonical_root.join(&clean_rel);
                if fs::symlink_metadata(&target).is_ok() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing to overwrite existing archive path {}.",
                            clean_rel.display()
                        ),
                    ));
                }

                let mut opts = OpenOptions::new();
                opts.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    opts.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
                }
                let mut out = opts.open(&target).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not extract {}: {err}", clean_rel.display()),
                    )
                })?;

                // Bound actual bytes written during streaming (never trusting header size alone).
                let mut buf = [0u8; 64 * 1024];
                loop {
                    let n = entry.read(&mut buf).map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Could not read extracted {}: {err}", clean_rel.display()),
                        )
                    })?;
                    if n == 0 {
                        break;
                    }
                    total_written = total_written.saturating_add(n as u64);
                    if total_written > max_bytes {
                        return Err(AppError::new(
                            ErrorCode::Denied,
                            format!(
                                "Extracted macOS bundle exceeded the {max_bytes}-byte safety limit."
                            ),
                        ));
                    }
                    out.write_all(&buf[..n]).map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Could not write extracted {}: {err}", clean_rel.display()),
                        )
                    })?;
                }

                #[cfg(unix)]
                if let Ok(mode) = entry.header().mode() {
                    use std::os::unix::fs::PermissionsExt;
                    // Strip setuid (0o4000), setgid (0o2000), and sticky (0o1000) bits and set
                    // permissions on the open handle (fchmod) so no symlink can ever be followed.
                    let safe_mode = mode & 0o0777;
                    out.set_permissions(fs::Permissions::from_mode(safe_mode))
                        .map_err(|err| {
                            AppError::new(
                                ErrorCode::Failed,
                                format!(
                                    "Could not set permissions on {}: {err}",
                                    clean_rel.display()
                                ),
                            )
                        })?;
                }
            }
            EntryType::Symlink => {
                let link_target = entry
                    .link_name()
                    .map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Invalid symlink in archive: {err}"),
                        )
                    })?
                    .ok_or_else(|| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Symlink {} is missing its target.", clean_rel.display()),
                        )
                    })?
                    .into_owned();
                if link_target.is_absolute() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing absolute symlink {} -> {} inside update archive.",
                            clean_rel.display(),
                            link_target.display()
                        ),
                    ));
                }
                if fs::symlink_metadata(canonical_root.join(&clean_rel)).is_ok() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing symlink {} colliding with existing entry.",
                            clean_rel.display()
                        ),
                    ));
                }
                pending_symlinks.insert(clean_rel, link_target);
            }
            other => {
                return Err(AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing unsupported tar entry type {other:?} at {}.",
                        clean_rel.display()
                    ),
                ));
            }
        }
    }

    // Validate all symlink chains transitively (in any entry order) BEFORE creating any symlink on
    // disk, catching multi-hop escapes such as `b -> ..` followed or preceded by `c -> b/..` and
    // symlink loops.
    validate_all_symlink_chains(&pending_symlinks)?;

    #[cfg(unix)]
    for (clean_rel, link_target) in &pending_symlinks {
        if let Some(parent_rel) = clean_rel.parent()
            && !parent_rel.as_os_str().is_empty()
        {
            ensure_safe_dir_components(&canonical_root, parent_rel)?;
        }
        let target = canonical_root.join(clean_rel);
        if fs::symlink_metadata(&target).is_ok() {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Refusing symlink {} colliding with existing file or directory.",
                    clean_rel.display()
                ),
            ));
        }
        std::os::unix::fs::symlink(link_target, &target).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not create symlink {}: {err}", clean_rel.display()),
            )
        })?;
    }
    #[cfg(not(unix))]
    if !pending_symlinks.is_empty() {
        return Err(AppError::new(
            ErrorCode::Unsupported,
            "macOS app-tar extraction requires a Unix host.",
        ));
    }

    // Locate the single top-level real `.app` bundle directory (refusing symlinks).
    let mut bundles = Vec::new();
    let dir_entries = fs::read_dir(&canonical_root).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not inspect extracted macOS bundle: {err}"),
        )
    })?;
    for entry in dir_entries.flatten() {
        let p = entry.path();
        if let Ok(meta) = fs::symlink_metadata(&p)
            && meta.file_type().is_dir()
            && p.extension().and_then(|e| e.to_str()) == Some("app")
        {
            bundles.push(p);
        }
    }
    if bundles.len() != 1 {
        return Err(AppError::new(
            ErrorCode::Failed,
            format!(
                "Expected exactly one top-level .app bundle in update archive, found {}.",
                bundles.len()
            ),
        ));
    }
    let bundle = bundles.remove(0);
    let contents_dir = bundle.join("Contents");
    let macos_dir = contents_dir.join("MacOS");
    let contents_ok = fs::symlink_metadata(&contents_dir)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false);
    let macos_ok = fs::symlink_metadata(&macos_dir)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false);
    if !contents_ok || !macos_ok {
        return Err(AppError::new(
            ErrorCode::Failed,
            format!(
                "Extracted bundle {} is missing a real Contents/MacOS directory.",
                bundle.display()
            ),
        ));
    }
    Ok(bundle)
}

/// Creates each directory component under `root` one segment at a time while checking
/// `symlink_metadata` so extraction can never traverse through a symlinked directory.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn ensure_safe_dir_components(root: &Path, rel_dir: &Path) -> Result<(), AppError> {
    let mut cur = root.to_path_buf();
    for comp in rel_dir.components() {
        let Component::Normal(part) = comp else {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Refusing unsafe directory component in {}.",
                    rel_dir.display()
                ),
            ));
        };
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if !meta.file_type().is_dir() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing to traverse non-directory or symlink at {}.",
                            rel_dir.display()
                        ),
                    ));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cur).map_err(|e| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not create directory {}: {e}", rel_dir.display()),
                    )
                })?;
            }
            Err(err) => {
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!("Could not inspect {}: {err}", rel_dir.display()),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn validate_tar_rel_path(path: &Path) -> Result<PathBuf, AppError> {
    let mut clean = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => clean.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing unsafe path {} inside update archive.",
                        path.display()
                    ),
                ));
            }
        }
    }
    if clean.as_os_str().is_empty() {
        return Err(AppError::new(
            ErrorCode::Denied,
            "Refusing empty path inside update archive.",
        ));
    }
    Ok(clean)
}

/// Validates every symlink in `symlinks` against the `.app` bundle root by resolving multi-hop
/// symlink chains (forward or backward in archive order) up to [`MAX_SYMLINK_CHAIN_DEPTH`].
/// Refuses any top-level symlink, absolute target, symlink loop, or symlink chain that escapes
/// the top-level `.app` bundle directory (even one level up to the archive root or to a sibling).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn validate_all_symlink_chains(symlinks: &BTreeMap<PathBuf, PathBuf>) -> Result<(), AppError> {
    for (link_rel, raw_target) in symlinks {
        let mut stack: Vec<OsString> = link_rel
            .parent()
            .map(|p| {
                p.components()
                    .filter_map(|c| match c {
                        Component::Normal(part) => Some(part.to_os_string()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Every symlink in a macOS app-tar must live inside `<Bundle>.app/...`; refuse any
        // top-level symlink at the archive root.
        if stack.is_empty() {
            return Err(AppError::new(
                ErrorCode::Denied,
                format!(
                    "Refusing top-level symlink {} -> {} outside the .app bundle root.",
                    link_rel.display(),
                    raw_target.display()
                ),
            ));
        }
        let bundle_root = stack[0].clone();

        let mut remaining: Vec<Component<'_>> = raw_target.components().collect();
        let mut pending_targets: Vec<PathBuf> = Vec::new();
        let mut hops = 0usize;

        // Iterative work-queue resolution: push `raw_target` components, and whenever the current
        // resolved path matches another symlink in `symlinks`, expand that symlink's target in
        // place before continuing with the remaining path components.
        let mut work_queue: Vec<OsString> = Vec::new();
        for comp in remaining.drain(..) {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => work_queue.push(OsString::from("..")),
                Component::Normal(part) => work_queue.push(part.to_os_string()),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing absolute symlink {} -> {} inside update archive.",
                            link_rel.display(),
                            raw_target.display()
                        ),
                    ));
                }
            }
        }
        // Reverse so `pop()` yields components in path order.
        work_queue.reverse();

        while let Some(seg) = work_queue.pop() {
            if seg == OsStr::new("..") {
                // `stack[0]` is `<Bundle>.app`; popping it would escape one level above the `.app`
                // bundle root (for example into `/Applications` once swapped into place).
                if stack.len() <= 1 {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing escaping symlink chain starting at {} -> {} leaving bundle root.",
                            link_rel.display(),
                            raw_target.display()
                        ),
                    ));
                }
                stack.pop();
                continue;
            }
            stack.push(seg);
            if stack.first() != Some(&bundle_root) {
                return Err(AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing symlink chain starting at {} -> {} leaving bundle root.",
                        link_rel.display(),
                        raw_target.display()
                    ),
                ));
            }
            let current_rel: PathBuf = stack.iter().collect();
            if let Some(next_target) = symlinks.get(&current_rel) {
                hops += 1;
                if hops > MAX_SYMLINK_CHAIN_DEPTH {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing looping or overly deep symlink chain starting at {}.",
                            link_rel.display()
                        ),
                    ));
                }
                // Pop the symlink's own name so its target resolves relative to its parent dir.
                stack.pop();
                pending_targets.push(next_target.clone());
                let mut next_segs: Vec<OsString> = Vec::new();
                for comp in next_target.components() {
                    match comp {
                        Component::CurDir => {}
                        Component::ParentDir => next_segs.push(OsString::from("..")),
                        Component::Normal(part) => next_segs.push(part.to_os_string()),
                        Component::RootDir | Component::Prefix(_) => {
                            return Err(AppError::new(
                                ErrorCode::Denied,
                                format!(
                                    "Refusing absolute symlink target {} in chain from {}.",
                                    next_target.display(),
                                    link_rel.display()
                                ),
                            ));
                        }
                    }
                }
                for s in next_segs.into_iter().rev() {
                    work_queue.push(s);
                }
            }
        }
    }
    Ok(())
}

/// Parameters for launching the detached Unix atomic swap-and-rollback supervisor (Linux AppImage
/// and macOS `.app` bundle).
#[derive(Clone, Debug)]
pub struct UnixSwapPlan {
    /// PID of the currently running app to wait for before swapping.
    pub old_pid: u32,
    /// Optional parent PID when running under an AppImage extract-and-run wrapper runtime so the
    /// supervisor waits for the wrapper process to exit and clean up too.
    pub old_ppid: Option<u32>,
    /// Target path to replace (`.AppImage` file on Linux, `.app` directory on macOS).
    pub target_path: PathBuf,
    /// Staged replacement path on the same filesystem as `target_path`.
    pub staged_new_path: PathBuf,
    /// Backup path on the same filesystem where the previous copy is held until startup succeeds.
    pub backup_path: PathBuf,
    /// Executable path to launch after swapping (`target_path` for AppImage,
    /// `<target_path>/Contents/MacOS/<bin>` for macOS).
    pub exec_path: PathBuf,
    /// Path the new build creates once its startup is confirmed (`<updates_dir>/started-<new_seq>.ok`).
    pub ready_marker_path: PathBuf,
    /// Path where the watchdog writes rollback metadata if the new build fails to start.
    pub rollback_marker_path: PathBuf,
    /// JSON payload written to `rollback_marker_path` when the previous copy is restored cleanly.
    pub rollback_json: String,
    /// JSON payload written to `rollback_marker_path` when rollback cannot safely replace in-use
    /// files and keeps the new copy intact.
    pub rollback_kept_new_json: String,
    /// Path where the supervisor writes deferred metadata when it gives up cleanly before swapping
    /// because another instance is still running from the install (never counted as a failure).
    pub deferred_marker_path: Option<PathBuf>,
    /// JSON payload written to `deferred_marker_path` on clean pre-swap deferral.
    pub deferred_json: Option<String>,
    /// Target sequence string passed via `LEGION_UPDATE_SUPERVISED_LAUNCH` to the newly launched copy.
    pub target_sequence: u64,
    /// Path to `<updates_dir>/update.lock` removed by the supervisor when it exits.
    pub update_lock_path: Option<PathBuf>,
    /// Path to `<updates_dir>/reopen-requested` checked after a `verify_only` update finishes.
    pub reopen_marker_path: Option<PathBuf>,
    /// Optional diagnostic log file for supervisor stdout/stderr (`<updates_dir>/supervisor.log`).
    pub log_path: Option<PathBuf>,
    /// Optional size-capped log capturing the new copy's first-start stdout/stderr
    /// (`<updates_dir>/new-start.log`).
    pub new_start_log_path: Option<PathBuf>,
    /// Optional process-list snapshot captured at the exact moment of a rollback decision
    /// (`<updates_dir>/rollback-ps.txt`).
    pub rollback_ps_path: Option<PathBuf>,
    /// Optional Linux D-Bus single-instance name to wait to clear before launching the new copy.
    pub single_instance_dbus_name: Option<String>,
    /// Optional macOS Unix socket path to wait to clear before launching the new copy.
    pub single_instance_socket_path: Option<PathBuf>,
    /// Seconds to wait before swapping for all processes using the install to exit.
    pub pre_swap_wait_secs: u64,
    /// Seconds to wait for `ready_marker_path` before restoring `backup_path`.
    pub wait_secs: u64,
    /// Grace seconds to keep polling for `ready_marker_path` if the launched PID exits non-zero
    /// early (clamped to `wait_secs`).
    pub exit_grace_secs: u64,
    /// Bounded rollback retry count (250ms per retry) if another process holds target files open.
    pub rollback_retries: u64,
    /// When `true` (window-close install), launches the new build with `LEGION_UPDATE_VERIFY_ONLY=1`
    /// so it confirms startup and exits cleanly without opening a window, and does not relaunch a
    /// window on rollback (unless `reopen_marker_path` was created while updating).
    pub verify_only: bool,
}

/// Reads the captured first-start output (`new-start.log`, capped to 32 KiB) and any exit reason
/// recorded in `supervisor.log` so Technical details can explain why a desktop update rolled back.
pub fn read_captured_startup_detail(updates_dir: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Ok(sup_raw) = fs::read(updates_dir.join("supervisor.log")) {
        let sup_text = String::from_utf8_lossy(&sup_raw);
        let interesting = sup_text
            .lines()
            .rev()
            .find(|line| {
                line.contains("exited with")
                    || line.contains("NSIS installer exited")
                    || line.contains("supervisor exception")
            })
            .or_else(|| {
                sup_text.lines().rev().find(|line| {
                    line.contains("ready marker missing")
                        || line.contains("ready marker not observed")
                })
            })
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(line) = interesting {
            parts.push(line.to_owned());
        }
    }

    if let Ok(file) = fs::File::open(updates_dir.join(NEW_START_LOG_FILENAME)) {
        let mut buf = Vec::new();
        let _ = file
            .take((MAX_CAPTURED_START_LOG_BYTES + 1) as u64)
            .read_to_end(&mut buf);
        if buf.len() > MAX_CAPTURED_START_LOG_BYTES {
            let start = buf.len() - MAX_CAPTURED_START_LOG_BYTES;
            buf.drain(0..start);
        }
        let text = String::from_utf8_lossy(&buf);
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        if !lines.is_empty() {
            let tail_start = lines.len().saturating_sub(6);
            let excerpt = lines[tail_start..].join(" | ");
            if !excerpt.is_empty() {
                parts.push(excerpt);
            }
        }
    }

    if parts.is_empty() {
        return None;
    }
    let combined = parts.join(" — ");
    if combined.chars().count() <= MAX_STARTUP_DETAIL_CHARS {
        Some(combined)
    } else {
        let truncated: String = combined
            .chars()
            .take(MAX_STARTUP_DETAIL_CHARS.saturating_sub(1))
            .collect();
        Some(format!("{truncated}…"))
    }
}

/// Returns the parent PID on Linux ONLY when the parent process is the AppImage runtime binary
/// (`APPIMAGE_EXTRACT_AND_RUN=1` forks the child instead of `exec`ing in place). In FUSE mode the
/// runtime `exec`s `legion-app` in place, so our parent is the user's shell or desktop launcher and
/// must NOT be waited on.
#[cfg(target_os = "linux")]
fn detect_appimage_wrapper_ppid(appimage_path: &Path) -> Option<u32> {
    let ppid = rustix::process::getppid()?.as_raw_nonzero().get() as u32;
    if ppid <= 1 {
        return None;
    }
    let proc_exe = fs::canonicalize(format!("/proc/{ppid}/exe")).ok()?;
    let target_canon = fs::canonicalize(appimage_path).ok()?;
    if proc_exe == target_canon {
        Some(ppid)
    } else {
        None
    }
}

/// Sanitized environment prepared for the detached Unix swap supervisor and relaunched desktop app.
#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct SanitizedUnixEnv {
    /// Minimal safe session variables for the relaunched app (no `PATH`, no `LD_*`, no `APPDIR`).
    pub session_vars: BTreeMap<OsString, OsString>,
    /// Sanitized `PATH` to restore inside the relaunched app (never used by the supervisor shell).
    pub relaunch_path: OsString,
}

/// Strips every AppImage mount variable, dynamic-linker override, shell-injection variable, and
/// `$APPDIR`/temporary-directory entry from `raw_env` so neither the swap supervisor nor the
/// relaunched desktop app inherits hijacked paths or libraries.
#[cfg(unix)]
pub fn build_sanitized_unix_env<I>(raw_env: I, explicit_appdir: Option<&Path>) -> SanitizedUnixEnv
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let raw_map: BTreeMap<OsString, OsString> = raw_env.into_iter().collect();

    let mut forbidden_prefixes: Vec<PathBuf> = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/var/tmp"),
        PathBuf::from("/dev/shm"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/tmp"),
    ];
    let sys_tmp = std::env::temp_dir();
    forbidden_prefixes.push(sys_tmp.clone());
    if let Ok(c) = fs::canonicalize(&sys_tmp) {
        forbidden_prefixes.push(c);
    }
    if let Some(appdir) = explicit_appdir
        .map(Path::to_path_buf)
        .or_else(|| raw_map.get(OsStr::new("APPDIR")).map(PathBuf::from))
        && !appdir.as_os_str().is_empty()
    {
        forbidden_prefixes.push(appdir.clone());
        if let Ok(c) = fs::canonicalize(&appdir) {
            forbidden_prefixes.push(c);
        }
    }

    let is_forbidden_entry = |entry: &Path| -> bool {
        if !entry.is_absolute() {
            return true;
        }
        let s = entry.to_string_lossy();
        if s.contains("/.mount_") || s.contains("/appimage_extracted_") {
            return true;
        }
        forbidden_prefixes.iter().any(|p| entry.starts_with(p))
    };

    let filter_colon_list = |val: &OsStr| -> Option<OsString> {
        let kept: Vec<PathBuf> = std::env::split_paths(val)
            .filter(|p| !p.as_os_str().is_empty() && !is_forbidden_entry(p))
            .collect();
        if kept.is_empty() {
            None
        } else {
            std::env::join_paths(kept).ok()
        }
    };

    let relaunch_path = raw_map
        .get(OsStr::new("PATH"))
        .and_then(|v| filter_colon_list(v))
        .unwrap_or_else(|| OsString::from(RELAUNCH_FALLBACK_PATH));

    const ALLOWED_EXACT: &[&str] = &[
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "LANGUAGE",
        "TERM",
        "COLORTERM",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "XDG_SESSION_CLASS",
        "XDG_SESSION_DESKTOP",
        "XDG_SEAT",
        "XDG_VTNR",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSH_AUTH_SOCK",
        "TZ",
        "DESKTOP_SESSION",
        "GDMSESSION",
        "QT_QPA_PLATFORM",
        "MOZ_ENABLE_WAYLAND",
        "GTK_IM_MODULE",
        "QT_IM_MODULE",
        "QT4_IM_MODULE",
        "CLUTTER_IM_MODULE",
        "SDL_IM_MODULE",
        "GLFW_IM_MODULE",
        "XMODIFIERS",
        "INPUT_METHOD",
        "IMSETTINGS_MODULE",
        "QT_IM_MODULES",
        "IBUS_ADDRESS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
        "all_proxy",
        "LEGION_HOME",
        "LEGION_APP_READY_FILE",
        "LEGION_APP_ALLOW_LOOPBACK_ENDPOINTS",
    ];
    const FILTERED_PATH_LISTS: &[&str] =
        &["XDG_DATA_DIRS", "XDG_CONFIG_DIRS", "MANPATH", "INFOPATH"];

    let mut session_vars = BTreeMap::new();
    for (k, v) in &raw_map {
        let Some(key) = k.to_str() else {
            continue;
        };
        if FILTERED_PATH_LISTS.contains(&key) {
            if let Some(filtered) = filter_colon_list(v) {
                session_vars.insert(k.clone(), filtered);
            }
            continue;
        }
        let allowed =
            ALLOWED_EXACT.contains(&key) || key.starts_with("LC_") || key.starts_with("WEBKIT_");
        if allowed {
            // Even for allowed single-path vars (e.g. XDG_DATA_HOME), drop if pointing inside APPDIR.
            if key.starts_with("XDG_") && key.ends_with("_HOME") && is_forbidden_entry(Path::new(v))
            {
                continue;
            }
            session_vars.insert(k.clone(), v.clone());
        } else if key == "APPIMAGE_EXTRACT_AND_RUN"
            && (v == OsStr::new("1") || v.to_string_lossy().eq_ignore_ascii_case("true"))
        {
            session_vars.insert(k.clone(), OsString::from("1"));
        }
    }

    SanitizedUnixEnv {
        session_vars,
        relaunch_path,
    }
}

/// Fixed POSIX `/bin/sh` supervisor script for Linux AppImage and macOS `.app` atomic swap with
/// automatic rollback.
///
/// Runs with a cleared environment (`PATH=/usr/sbin:/usr/bin:/sbin:/bin`), resolves every external
/// utility (`mv`, `rm`, `ps`, `sleep`) exclusively from fixed system directories, restores the
/// pre-sanitized `LEGION_RELAUNCH_PATH` only inside the relaunched child, and strips all supervisor
/// helper variables before `exec`.
#[cfg(unix)]
const UNIX_SWAP_SUPERVISOR_SCRIPT: &str = r#"
set -u
PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH

find_sys_bin() {
  for d in /usr/bin /bin /usr/sbin /sbin; do
    if [ -x "$d/$1" ] && [ ! -d "$d/$1" ]; then
      printf '%s\n' "$d/$1"
      return 0
    fi
  done
  return 1
}

MV="$(find_sys_bin mv)" || exit 1
RM="$(find_sys_bin rm)" || exit 1
PS="$(find_sys_bin ps)" || exit 1
SLEEP="$(find_sys_bin sleep)" || exit 1
TAIL="$(find_sys_bin tail 2>/dev/null || true)"
DBUS_SEND="$(find_sys_bin dbus-send 2>/dev/null || true)"
READLINK="$(find_sys_bin readlink 2>/dev/null || true)"

release_update_lock() {
  if [ -n "${LEGION_UPDATE_LOCK:-}" ]; then
    "$RM" -f -- "$LEGION_UPDATE_LOCK" 2>/dev/null || true
  fi
}
trap release_update_lock EXIT

collect_children() {
  parent="$1"
  ps_snap="$("$PS" -eo pid=,ppid= 2>/dev/null || true)"
  out=""
  if [ -n "$ps_snap" ]; then
    set -- $ps_snap
    while [ "$#" -ge 2 ]; do
      cpid="$1"
      cppid="$2"
      shift 2
      case "$cpid" in ''|*[!0-9]*) continue ;; esac
      if [ "$cppid" = "$parent" ] && [ "$cpid" != "$$" ]; then
        out="$out $cpid"
      fi
    done
  fi
  printf '%s\n' "$out"
}

# Returns 0 if any live process (other than this supervisor $$) is running from $LEGION_TARGET.
# Checks /proc/<pid>/exe in a single pass on Linux and ps arg0/script prefix on macOS/POSIX
# without word-splitting paths that contain spaces or glob characters.
install_target_in_use() {
  target_real="$LEGION_TARGET"
  if [ -n "$READLINK" ]; then
    resolved="$("$READLINK" -f -- "$LEGION_TARGET" 2>/dev/null || true)"
    if [ -n "$resolved" ]; then
      target_real="$resolved"
    fi
  fi
  if [ -d /proc ] && [ -n "$READLINK" ]; then
    proc_exes="$("$READLINK" -f /proc/[0-9]*/exe 2>/dev/null || true)"
    if [ -n "$proc_exes" ]; then
      while IFS= read -r pexe; do
        case "$pexe" in
          "$target_real"|"$target_real"/*|"$LEGION_TARGET"|"$LEGION_TARGET"/*)
            return 0
            ;;
        esac
      done <<EOF
$proc_exes
EOF
    fi
  fi
  ps_args="$("$PS" -eo pid=,args= 2>/dev/null || true)"
  if [ -n "$ps_args" ]; then
    while IFS= read -r pline; do
      trimmed="${pline#"${pline%%[! 	]*}"}"
      [ -n "$trimmed" ] || continue
      pp="${trimmed%%[ 	]*}"
      case "$pp" in ''|*[!0-9]*) continue ;; esac
      if [ "$pp" = "$$" ]; then
        continue
      fi
      rest="${trimmed#"$pp"}"
      rest="${rest#"${rest%%[! 	]*}"}"
      [ -n "$rest" ] || continue
      case "$rest" in
        "$LEGION_TARGET"|"$LEGION_TARGET"/*|"$LEGION_TARGET "*|"$target_real"|"$target_real"/*|"$target_real "*)
          return 0
          ;;
        "sh $LEGION_TARGET"|"sh $LEGION_TARGET "*|"/bin/sh $LEGION_TARGET"|"/bin/sh $LEGION_TARGET "*|"/usr/bin/sh $LEGION_TARGET"|"/usr/bin/sh $LEGION_TARGET "*|"dash $LEGION_TARGET"|"dash $LEGION_TARGET "*|"/bin/dash $LEGION_TARGET"|"/bin/dash $LEGION_TARGET "*|"/usr/bin/dash $LEGION_TARGET"|"/usr/bin/dash $LEGION_TARGET "*|"bash $LEGION_TARGET"|"bash $LEGION_TARGET "*|"/bin/bash $LEGION_TARGET"|"/bin/bash $LEGION_TARGET "*|"/usr/bin/bash $LEGION_TARGET"|"/usr/bin/bash $LEGION_TARGET "*|"sh $target_real"|"sh $target_real "*|"/bin/sh $target_real"|"/bin/sh $target_real "*|"/usr/bin/sh $target_real"|"/usr/bin/sh $target_real "*|"dash $target_real"|"dash $target_real "*|"/bin/dash $target_real"|"/bin/dash $target_real "*|"/usr/bin/dash $target_real"|"/usr/bin/dash $target_real "*|"bash $target_real"|"bash $target_real "*|"/bin/bash $target_real"|"/bin/bash $target_real "*|"/usr/bin/bash $target_real"|"/usr/bin/bash $target_real "*)
          return 0
          ;;
      esac
    done <<EOF
$ps_args
EOF
  fi
  return 1
}

dbus_name_owned() {
  if [ -z "${LEGION_SINGLE_INSTANCE_DBUS:-}" ] || [ -z "$DBUS_SEND" ] || [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    return 1
  fi
  reply="$("$DBUS_SEND" --session --dest=org.freedesktop.DBus --type=method_call --print-reply \
    /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner "string:$LEGION_SINGLE_INSTANCE_DBUS" 2>/dev/null || true)"
  case "$reply" in
    *"boolean true"*) return 0 ;;
    *) return 1 ;;
  esac
}

cap_log_file() {
  log_file="$1"
  if [ -z "$log_file" ] || [ ! -f "$log_file" ]; then
    return 0
  fi
  if [ -n "$TAIL" ]; then
    tmp_cap="${log_file}.cap"
    if (umask 077 && "$TAIL" -c 32768 "$log_file" > "$tmp_cap" 2>/dev/null); then
      "$MV" -f -- "$tmp_cap" "$log_file" 2>/dev/null || "$RM" -f -- "$tmp_cap"
    else
      "$RM" -f -- "$tmp_cap"
    fi
  fi
}

capture_rollback_ps() {
  ps_file="${LEGION_ROLLBACK_PS_FILE:-}"
  if [ -z "$ps_file" ]; then
    return 0
  fi
  (umask 077 && { "$PS" -ef 2>/dev/null || "$PS" aux 2>/dev/null || true; } > "$ps_file") 2>/dev/null || true
}

old_children="$(collect_children "$LEGION_OLD_PID")"
printf 'supervisor: start old_pid=%s old_ppid=%s old_children=[%s] verify_only=%s target=%s\n' \
  "$LEGION_OLD_PID" "${LEGION_OLD_PPID:-}" "$old_children" "${LEGION_VERIFY_ONLY:-0}" "$LEGION_TARGET"

# 1. Wait up to LEGION_PRE_SWAP_WAIT_SECS for the previous process, its AppImage wrapper parent
# (if any), its helper children, its single-instance lock, AND any other process running from
# $LEGION_TARGET to exit completely. If another instance is still running when the bound expires,
# defer cleanly without changing the install and without counting as a failure.
pre_wait="${LEGION_PRE_SWAP_WAIT_SECS:-15}"
pre_ticks=$((pre_wait * 4))
i=0
still_up=0
while [ "$i" -lt "$pre_ticks" ]; do
  still_up=0
  if kill -0 "$LEGION_OLD_PID" 2>/dev/null; then
    still_up=1
  fi
  if [ -n "${LEGION_OLD_PPID:-}" ] && kill -0 "$LEGION_OLD_PPID" 2>/dev/null; then
    still_up=1
  fi
  for cpid in $old_children; do
    if kill -0 "$cpid" 2>/dev/null; then
      still_up=1
      break
    fi
  done
  if [ "$still_up" -eq 0 ]; then
    if install_target_in_use; then
      still_up=1
    elif dbus_name_owned; then
      still_up=1
    elif [ -n "${LEGION_SINGLE_INSTANCE_SOCK:-}" ] && [ -S "$LEGION_SINGLE_INSTANCE_SOCK" ]; then
      still_up=1
    fi
  fi
  if [ "$still_up" -eq 0 ]; then
    break
  fi
  "$SLEEP" 0.25 2>/dev/null || "$SLEEP" 1
  i=$((i + 1))
done
printf 'supervisor: pre-swap wait finished after %s ticks (still_up=%s)\n' "$i" "$still_up"

if [ "$still_up" -ne 0 ]; then
  printf 'supervisor: install still in use after %ss; deferring update cleanly without changing install\n' "$pre_wait"
  "$RM" -rf -- "$LEGION_NEW"
  if [ -n "${LEGION_DEFERRED_FILE:-}" ] && [ -n "${LEGION_DEFERRED_JSON:-}" ]; then
    printf '%s\n' "$LEGION_DEFERRED_JSON" > "$LEGION_DEFERRED_FILE" || true
  fi
  exit 0
fi

"$RM" -f -- "$LEGION_READY_MARKER"
"$RM" -rf -- "$LEGION_BACKUP"

# 2. Move current install aside and put the verified new copy into place.
if ! "$MV" -f -- "$LEGION_TARGET" "$LEGION_BACKUP"; then
  printf 'supervisor: failed to move target to backup\n'
  "$RM" -rf -- "$LEGION_NEW"
  if [ -n "${LEGION_DEFERRED_FILE:-}" ] && [ -n "${LEGION_DEFERRED_JSON:-}" ]; then
    printf '%s\n' "$LEGION_DEFERRED_JSON" > "$LEGION_DEFERRED_FILE" || true
  fi
  exit 0
fi

if ! "$MV" -f -- "$LEGION_NEW" "$LEGION_TARGET"; then
  printf 'supervisor: failed to move staged new copy into target; restoring backup\n'
  "$MV" -f -- "$LEGION_BACKUP" "$LEGION_TARGET" || true
  exit 1
fi
printf 'supervisor: swapped new copy into target\n'

launch_target() {
  verify_flag="$1"
  capture_log="$2"
  supervised_flag="$3"
  exec_bin="$LEGION_EXEC"
  if [ -n "$capture_log" ]; then
    "$RM" -f -- "$capture_log"
    (umask 077 && : > "$capture_log") 2>/dev/null || true
    (
      PATH="${LEGION_RELAUNCH_PATH:-/usr/local/bin:/usr/bin:/bin}"
      export PATH
      if [ "$verify_flag" = "1" ]; then
        LEGION_UPDATE_VERIFY_ONLY=1
        export LEGION_UPDATE_VERIFY_ONLY
      else
        unset LEGION_UPDATE_VERIFY_ONLY
      fi
      if [ -n "$supervised_flag" ]; then
        LEGION_UPDATE_SUPERVISED_LAUNCH="$supervised_flag"
        export LEGION_UPDATE_SUPERVISED_LAUNCH
      else
        unset LEGION_UPDATE_SUPERVISED_LAUNCH
      fi
      unset LEGION_OLD_PID LEGION_OLD_PPID LEGION_TARGET LEGION_NEW LEGION_BACKUP LEGION_EXEC \
        LEGION_READY_MARKER LEGION_ROLLBACK_FILE LEGION_ROLLBACK_JSON LEGION_ROLLBACK_KEPT_NEW_JSON \
        LEGION_DEFERRED_FILE LEGION_DEFERRED_JSON LEGION_TARGET_SEQUENCE LEGION_UPDATE_LOCK \
        LEGION_REOPEN_FILE LEGION_PRE_SWAP_WAIT_SECS LEGION_ROLLBACK_RETRIES \
        LEGION_WAIT_SECS LEGION_EXIT_GRACE_SECS LEGION_RELAUNCH_PATH LEGION_VERIFY_ONLY \
        LEGION_NEW_LOG LEGION_ROLLBACK_PS_FILE LEGION_SINGLE_INSTANCE_DBUS LEGION_SINGLE_INSTANCE_SOCK \
        APPIMAGE APPDIR ARGV0 OWD LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT \
        GTK_DATA_PREFIX GTK_EXE_PREFIX GTK_PATH GTK_IM_MODULE_FILE \
        GDK_PIXBUF_MODULE_FILE GSETTINGS_SCHEMA_DIR GI_TYPELIB_PATH GIO_MODULE_DIR \
        GST_PLUGIN_SYSTEM_PATH_1_0 GST_PLUGIN_PATH_1_0 GST_PLUGIN_SCANNER_1_0 GST_PTP_HELPER_1_0
      exec "$exec_bin"
    ) >"$capture_log" 2>&1 &
  else
    (
      PATH="${LEGION_RELAUNCH_PATH:-/usr/local/bin:/usr/bin:/bin}"
      export PATH
      if [ "$verify_flag" = "1" ]; then
        LEGION_UPDATE_VERIFY_ONLY=1
        export LEGION_UPDATE_VERIFY_ONLY
      else
        unset LEGION_UPDATE_VERIFY_ONLY
      fi
      if [ -n "$supervised_flag" ]; then
        LEGION_UPDATE_SUPERVISED_LAUNCH="$supervised_flag"
        export LEGION_UPDATE_SUPERVISED_LAUNCH
      else
        unset LEGION_UPDATE_SUPERVISED_LAUNCH
      fi
      unset LEGION_OLD_PID LEGION_OLD_PPID LEGION_TARGET LEGION_NEW LEGION_BACKUP LEGION_EXEC \
        LEGION_READY_MARKER LEGION_ROLLBACK_FILE LEGION_ROLLBACK_JSON LEGION_ROLLBACK_KEPT_NEW_JSON \
        LEGION_DEFERRED_FILE LEGION_DEFERRED_JSON LEGION_TARGET_SEQUENCE LEGION_UPDATE_LOCK \
        LEGION_REOPEN_FILE LEGION_PRE_SWAP_WAIT_SECS LEGION_ROLLBACK_RETRIES \
        LEGION_WAIT_SECS LEGION_EXIT_GRACE_SECS LEGION_RELAUNCH_PATH LEGION_VERIFY_ONLY \
        LEGION_NEW_LOG LEGION_ROLLBACK_PS_FILE LEGION_SINGLE_INSTANCE_DBUS LEGION_SINGLE_INSTANCE_SOCK \
        APPIMAGE APPDIR ARGV0 OWD LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT \
        GTK_DATA_PREFIX GTK_EXE_PREFIX GTK_PATH GTK_IM_MODULE_FILE \
        GDK_PIXBUF_MODULE_FILE GSETTINGS_SCHEMA_DIR GI_TYPELIB_PATH GIO_MODULE_DIR \
        GST_PLUGIN_SYSTEM_PATH_1_0 GST_PLUGIN_PATH_1_0 GST_PLUGIN_SCANNER_1_0 GST_PTP_HELPER_1_0
      exec "$exec_bin"
    ) >/dev/null 2>&1 &
  fi
  NEW_PID=$!
}

# 3. Start the new build (marked as supervised launch so update.lock permits it) and decide
# startup strictly by whether the ready marker appears within the wait window.
verify_mode="${LEGION_VERIFY_ONLY:-0}"
new_log="${LEGION_NEW_LOG:-}"
target_seq="${LEGION_TARGET_SEQUENCE:-1}"
if [ -n "${LEGION_ROLLBACK_PS_FILE:-}" ]; then
  "$RM" -f -- "$LEGION_ROLLBACK_PS_FILE"
fi
launch_target "$verify_mode" "$new_log" "$target_seq"
printf 'supervisor: launched new_pid=%s exec=%s log=%s\n' "$NEW_PID" "$LEGION_EXEC" "$new_log"
wait_limit="${LEGION_WAIT_SECS:-30}"
grace_limit="${LEGION_EXIT_GRACE_SECS:-8}"
max_ticks=$((wait_limit * 4))
grace_ticks=$((grace_limit * 4))
j=0
started=0
proc_done=0
while [ "$j" -lt "$max_ticks" ]; do
  if [ -f "$LEGION_READY_MARKER" ]; then
    started=1
    break
  fi
  if [ "$proc_done" -eq 0 ]; then
    exited_now=0
    if ! kill -0 "$NEW_PID" 2>/dev/null; then
      exited_now=1
    else
      new_stat="$("$PS" -o stat= -p "$NEW_PID" 2>/dev/null || true)"
      case "$new_stat" in
        *Z*) exited_now=1 ;;
      esac
    fi
    if [ "$exited_now" -eq 1 ]; then
      proc_done=1
      wait "$NEW_PID" 2>/dev/null
      new_rc=$?
      printf 'supervisor: new_pid=%s exited with rc=%s after %s ticks\n' "$NEW_PID" "$new_rc" "$j"
      # Never roll back immediately on a single PID exit: on exit 0 keep polling for the full
      # wait_limit (single-instance handover or wrapper fork); on non-zero exit keep polling for
      # grace_limit seconds in case a wrapper exited while another process finishes starting.
      if [ "$new_rc" -ne 0 ]; then
        grace_end=$((j + grace_ticks))
        if [ "$grace_end" -lt "$max_ticks" ]; then
          max_ticks="$grace_end"
        fi
      fi
    fi
  fi
  "$SLEEP" 0.25 2>/dev/null || "$SLEEP" 1
  j=$((j + 1))
done

# One final check after loop exit.
if [ -f "$LEGION_READY_MARKER" ]; then
  started=1
fi

if [ "$started" -eq 1 ]; then
  printf 'supervisor: ready marker observed at %s; removing backup\n' "$LEGION_READY_MARKER"
  "$RM" -rf -- "$LEGION_BACKUP"
  if [ -n "$new_log" ]; then
    "$RM" -f -- "$new_log"
  fi
  if [ -n "${LEGION_ROLLBACK_PS_FILE:-}" ]; then
    "$RM" -f -- "$LEGION_ROLLBACK_PS_FILE"
  fi
  release_update_lock
  if [ "$verify_mode" = "1" ] && [ -n "${LEGION_REOPEN_FILE:-}" ] && [ -f "$LEGION_REOPEN_FILE" ]; then
    "$RM" -f -- "$LEGION_REOPEN_FILE"
    printf 'supervisor: user reopened app during close update; launching updated copy interactively\n'
    launch_target 0 "" "" || true
  fi
  exit 0
fi

# 4. New build failed to confirm startup: capture live process list at the moment of rollback,
# stop the new process and its direct children, cap startup output, retry boundedly if another
# process holds the target open (never half-copying; keeping new copy intact if still busy),
# otherwise atomically swap backup back, record exact rollback state, release lock, and relaunch.
printf 'supervisor: ready marker missing after %s ticks; rolling back\n' "$j"
capture_rollback_ps
new_children="$(collect_children "$NEW_PID")"
kill "$NEW_PID" 2>/dev/null || true
for cpid in $new_children; do
  kill "$cpid" 2>/dev/null || true
done
"$SLEEP" 1
kill -9 "$NEW_PID" 2>/dev/null || true
for cpid in $new_children; do
  kill -9 "$cpid" 2>/dev/null || true
done
cap_log_file "$new_log"

rb_retries="${LEGION_ROLLBACK_RETRIES:-20}"
r=0
target_busy=0
while [ "$r" -lt "$rb_retries" ]; do
  if ! install_target_in_use; then
    target_busy=0
    break
  fi
  target_busy=1
  "$SLEEP" 0.25 2>/dev/null || "$SLEEP" 1
  r=$((r + 1))
done

restored=0
if [ "$target_busy" -eq 1 ]; then
  printf 'supervisor: rollback target still in use after %s retries; keeping new copy intact\n' "$rb_retries"
  printf '%s\n' "${LEGION_ROLLBACK_KEPT_NEW_JSON:-$LEGION_ROLLBACK_JSON}" > "$LEGION_ROLLBACK_FILE" || true
else
  failed_aside="${LEGION_TARGET}.failed.$$"
  "$RM" -rf -- "$failed_aside" 2>/dev/null || true
  if "$MV" -f -- "$LEGION_TARGET" "$failed_aside" 2>/dev/null; then
    if "$MV" -f -- "$LEGION_BACKUP" "$LEGION_TARGET" 2>/dev/null; then
      "$RM" -rf -- "$failed_aside" 2>/dev/null || true
      restored=1
      printf '%s\n' "$LEGION_ROLLBACK_JSON" > "$LEGION_ROLLBACK_FILE" || true
    else
      printf 'supervisor: failed to move backup back into target; restoring new copy intact\n'
      "$MV" -f -- "$failed_aside" "$LEGION_TARGET" 2>/dev/null || true
      printf '%s\n' "${LEGION_ROLLBACK_KEPT_NEW_JSON:-$LEGION_ROLLBACK_JSON}" > "$LEGION_ROLLBACK_FILE" || true
    fi
  else
    printf 'supervisor: could not move new target aside during rollback; keeping new copy intact\n'
    printf '%s\n' "${LEGION_ROLLBACK_KEPT_NEW_JSON:-$LEGION_ROLLBACK_JSON}" > "$LEGION_ROLLBACK_FILE" || true
  fi
fi

reopen_needed=0
if [ "$verify_mode" = "1" ] && [ -n "${LEGION_REOPEN_FILE:-}" ] && [ -f "$LEGION_REOPEN_FILE" ]; then
  "$RM" -f -- "$LEGION_REOPEN_FILE"
  reopen_needed=1
fi
release_update_lock

if [ "$restored" -eq 1 ]; then
  k=0
  while [ "$k" -lt 8 ]; do
    if ! dbus_name_owned && { [ -z "${LEGION_SINGLE_INSTANCE_SOCK:-}" ] || [ ! -S "$LEGION_SINGLE_INSTANCE_SOCK" ]; }; then
      break
    fi
    "$SLEEP" 0.25 2>/dev/null || "$SLEEP" 1
    k=$((k + 1))
  done
  if { [ "$verify_mode" != "1" ] || [ "$reopen_needed" -eq 1 ]; } && [ -x "$LEGION_EXEC" ]; then
    launch_target 0 "" "" || true
  fi
fi
exit 1
"#;

/// Stages a verified Linux AppImage beside `appimage_path` by copying-while-hashing the exact
/// verified bytes, sets executable mode `0o755`, and spawns the detached swap-and-rollback
/// supervisor.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn stage_and_spawn_linux_appimage(
    verified_download: &Path,
    expected_sha256: &str,
    expected_size: u64,
    appimage_path: &Path,
    ready_marker_path: &Path,
    rollback_marker_path: &Path,
    rollback_json: String,
    rollback_kept_new_json: String,
    deferred_marker_path: &Path,
    deferred_json: String,
    target_sequence: u64,
    wait_secs: u64,
    verify_only: bool,
) -> Result<u32, AppError> {
    let parent = appimage_path.parent().ok_or_else(|| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "AppImage path {} has no parent folder.",
                appimage_path.display()
            ),
        )
    })?;
    let file_name = appimage_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::Failed,
                format!(
                    "AppImage path {} has no file name.",
                    appimage_path.display()
                ),
            )
        })?;
    let pid = std::process::id();
    let staged_new = parent.join(format!(".{file_name}.new-{pid}"));
    let backup_path = parent.join(format!(".{file_name}.prev"));

    // Copy-while-hashing onto the same filesystem as `appimage_path` so the final rename is atomic
    // and installs the exact bytes verified.
    copy_and_verify_sha256(
        verified_download,
        &staged_new,
        expected_sha256,
        expected_size,
        0o755,
    )?;

    #[cfg(target_os = "linux")]
    let old_ppid = detect_appimage_wrapper_ppid(appimage_path);
    #[cfg(not(target_os = "linux"))]
    let old_ppid = None;
    #[cfg(target_os = "linux")]
    let single_instance_dbus_name = Some(LINUX_SINGLE_INSTANCE_DBUS_NAME.to_owned());
    #[cfg(not(target_os = "linux"))]
    let single_instance_dbus_name = None;

    let updates_dir = ready_marker_path.parent();
    spawn_unix_swap_supervisor(&UnixSwapPlan {
        old_pid: pid,
        old_ppid,
        target_path: appimage_path.to_path_buf(),
        staged_new_path: staged_new,
        backup_path,
        exec_path: appimage_path.to_path_buf(),
        ready_marker_path: ready_marker_path.to_path_buf(),
        rollback_marker_path: rollback_marker_path.to_path_buf(),
        rollback_json,
        rollback_kept_new_json,
        deferred_marker_path: Some(deferred_marker_path.to_path_buf()),
        deferred_json: Some(deferred_json),
        target_sequence,
        update_lock_path: updates_dir.map(|d| d.join(UPDATE_LOCK_FILENAME)),
        reopen_marker_path: updates_dir.map(|d| d.join(REOPEN_REQUESTED_FILENAME)),
        log_path: updates_dir.map(|d| d.join("supervisor.log")),
        new_start_log_path: updates_dir.map(|d| d.join(NEW_START_LOG_FILENAME)),
        rollback_ps_path: updates_dir.map(|d| d.join(ROLLBACK_PS_FILENAME)),
        single_instance_dbus_name,
        single_instance_socket_path: None,
        pre_swap_wait_secs: DEFAULT_PRE_SWAP_WAIT_SECS,
        wait_secs,
        exit_grace_secs: DEFAULT_EXIT_GRACE_SECS,
        rollback_retries: DEFAULT_ROLLBACK_RETRIES,
        verify_only,
    })
}

/// Copies-while-hashing a verified macOS `app-tar` into `verified_dir`, safely extracts it beside
/// `bundle_dir`, and spawns the detached swap-and-rollback supervisor.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn stage_and_spawn_macos_bundle(
    verified_tar_gz: &Path,
    expected_sha256: &str,
    expected_size: u64,
    verified_dir: &Path,
    bundle_dir: &Path,
    executable_rel: &Path,
    ready_marker_path: &Path,
    rollback_marker_path: &Path,
    rollback_json: String,
    rollback_kept_new_json: String,
    deferred_marker_path: &Path,
    deferred_json: String,
    target_sequence: u64,
    wait_secs: u64,
    verify_only: bool,
) -> Result<u32, AppError> {
    let parent = bundle_dir.parent().ok_or_else(|| {
        AppError::new(
            ErrorCode::Failed,
            format!("App bundle {} has no parent folder.", bundle_dir.display()),
        )
    })?;
    let bundle_name = bundle_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::Failed,
                format!("App bundle {} has no file name.", bundle_dir.display()),
            )
        })?;
    let pid = std::process::id();
    ensure_private_dir(verified_dir)?;
    let private_tar = verified_dir.join(format!("verified-{pid}.app.tar.gz"));
    copy_and_verify_sha256(
        verified_tar_gz,
        &private_tar,
        expected_sha256,
        expected_size,
        0o600,
    )?;

    let extract_root = parent.join(format!(".{bundle_name}.extract-{pid}"));
    let staged_bundle = parent.join(format!(".{bundle_name}.new-{pid}"));
    let backup_path = parent.join(format!(".{bundle_name}.prev"));

    let _ = fs::remove_dir_all(&extract_root);
    let _ = fs::remove_dir_all(&staged_bundle);

    let extracted_bundle = match extract_macos_app_tar(&private_tar, &extract_root) {
        Ok(b) => {
            let _ = fs::remove_file(&private_tar);
            b
        }
        Err(err) => {
            let _ = fs::remove_file(&private_tar);
            let _ = fs::remove_dir_all(&extract_root);
            return Err(err);
        }
    };
    if let Err(err) = fs::rename(&extracted_bundle, &staged_bundle) {
        let _ = fs::remove_dir_all(&extract_root);
        return Err(AppError::new(
            ErrorCode::Failed,
            format!("Could not stage extracted macOS bundle: {err}"),
        ));
    }
    let _ = fs::remove_dir_all(&extract_root);

    let exec_path = bundle_dir.join(executable_rel);
    #[cfg(target_os = "macos")]
    let single_instance_socket_path = Some(PathBuf::from(MACOS_SINGLE_INSTANCE_SOCKET));
    #[cfg(not(target_os = "macos"))]
    let single_instance_socket_path = None;
    let updates_dir = ready_marker_path.parent();
    spawn_unix_swap_supervisor(&UnixSwapPlan {
        old_pid: pid,
        old_ppid: None,
        target_path: bundle_dir.to_path_buf(),
        staged_new_path: staged_bundle,
        backup_path,
        exec_path,
        ready_marker_path: ready_marker_path.to_path_buf(),
        rollback_marker_path: rollback_marker_path.to_path_buf(),
        rollback_json,
        rollback_kept_new_json,
        deferred_marker_path: Some(deferred_marker_path.to_path_buf()),
        deferred_json: Some(deferred_json),
        target_sequence,
        update_lock_path: updates_dir.map(|d| d.join(UPDATE_LOCK_FILENAME)),
        reopen_marker_path: updates_dir.map(|d| d.join(REOPEN_REQUESTED_FILENAME)),
        log_path: updates_dir.map(|d| d.join("supervisor.log")),
        new_start_log_path: updates_dir.map(|d| d.join(NEW_START_LOG_FILENAME)),
        rollback_ps_path: updates_dir.map(|d| d.join(ROLLBACK_PS_FILENAME)),
        single_instance_dbus_name: None,
        single_instance_socket_path,
        pre_swap_wait_secs: DEFAULT_PRE_SWAP_WAIT_SECS,
        wait_secs,
        exit_grace_secs: DEFAULT_EXIT_GRACE_SECS,
        rollback_retries: DEFAULT_ROLLBACK_RETRIES,
        verify_only,
    })
}

/// Spawns the detached POSIX `/bin/sh` swap supervisor for [`UnixSwapPlan`] using the current
/// process environment (sanitized) and returns its PID.
#[cfg(unix)]
pub fn spawn_unix_swap_supervisor(plan: &UnixSwapPlan) -> Result<u32, AppError> {
    let sanitized = build_sanitized_unix_env(std::env::vars_os(), None);
    spawn_unix_swap_supervisor_with_env(plan, &sanitized)
}

/// Spawns the detached POSIX `/bin/sh` swap supervisor with an explicitly pre-sanitized
/// environment (used directly by security tests with planted `$APPDIR` fixtures) and returns its
/// PID.
#[cfg(unix)]
pub fn spawn_unix_swap_supervisor_with_env(
    plan: &UnixSwapPlan,
    sanitized: &SanitizedUnixEnv,
) -> Result<u32, AppError> {
    if let Some(script_diag) = plan
        .log_path
        .as_deref()
        .and_then(|p| p.parent().map(|d| d.join("supervisor-script.sh.txt")))
    {
        let _ = fs::write(&script_diag, UNIX_SWAP_SUPERVISOR_SCRIPT);
    }
    let (stdout_cfg, stderr_cfg) = open_supervisor_log_stdio(plan.log_path.as_deref());
    let mut cmd = Command::new("/bin/sh");
    cmd.env_clear()
        .args(["-c", UNIX_SWAP_SUPERVISOR_SCRIPT])
        .stdin(Stdio::null())
        .stdout(stdout_cfg)
        .stderr(stderr_cfg)
        .envs(&sanitized.session_vars)
        .env("PATH", SUPERVISOR_MINIMAL_PATH)
        .env("LEGION_RELAUNCH_PATH", &sanitized.relaunch_path)
        .env("LEGION_OLD_PID", plan.old_pid.to_string())
        .env("LEGION_TARGET", &plan.target_path)
        .env("LEGION_NEW", &plan.staged_new_path)
        .env("LEGION_BACKUP", &plan.backup_path)
        .env("LEGION_EXEC", &plan.exec_path)
        .env("LEGION_READY_MARKER", &plan.ready_marker_path)
        .env("LEGION_ROLLBACK_FILE", &plan.rollback_marker_path)
        .env("LEGION_ROLLBACK_JSON", &plan.rollback_json)
        .env(
            "LEGION_ROLLBACK_KEPT_NEW_JSON",
            &plan.rollback_kept_new_json,
        )
        .env("LEGION_TARGET_SEQUENCE", plan.target_sequence.to_string())
        .env(
            "LEGION_PRE_SWAP_WAIT_SECS",
            plan.pre_swap_wait_secs.to_string(),
        )
        .env("LEGION_WAIT_SECS", plan.wait_secs.to_string())
        .env("LEGION_EXIT_GRACE_SECS", plan.exit_grace_secs.to_string())
        .env("LEGION_ROLLBACK_RETRIES", plan.rollback_retries.to_string())
        .env(
            "LEGION_VERIFY_ONLY",
            if plan.verify_only { "1" } else { "0" },
        );
    if let Some(ppid) = plan.old_ppid {
        cmd.env("LEGION_OLD_PPID", ppid.to_string());
    }
    if let Some(deferred_path) = &plan.deferred_marker_path {
        cmd.env("LEGION_DEFERRED_FILE", deferred_path);
    }
    if let Some(deferred_json) = &plan.deferred_json {
        cmd.env("LEGION_DEFERRED_JSON", deferred_json);
    }
    if let Some(lock_path) = &plan.update_lock_path {
        cmd.env("LEGION_UPDATE_LOCK", lock_path);
    }
    if let Some(reopen_path) = &plan.reopen_marker_path {
        cmd.env("LEGION_REOPEN_FILE", reopen_path);
    }
    if let Some(new_log) = &plan.new_start_log_path {
        cmd.env("LEGION_NEW_LOG", new_log);
    }
    if let Some(ps_path) = &plan.rollback_ps_path {
        cmd.env("LEGION_ROLLBACK_PS_FILE", ps_path);
    }
    if let Some(dbus_name) = &plan.single_instance_dbus_name {
        cmd.env("LEGION_SINGLE_INSTANCE_DBUS", dbus_name);
    }
    if let Some(sock_path) = &plan.single_instance_socket_path {
        cmd.env("LEGION_SINGLE_INSTANCE_SOCK", sock_path);
    }

    let child = cmd.spawn().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not start update helper process: {err}"),
        )
    })?;
    Ok(child.id())
}

fn open_supervisor_log_stdio(log_path: Option<&Path>) -> (Stdio, Stdio) {
    let Some(path) = log_path else {
        return (Stdio::null(), Stdio::null());
    };
    if let Some(parent) = path.parent() {
        let _ = ensure_private_dir(parent);
    }
    let mut opts = OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(file) => match file.try_clone() {
            Ok(cloned) => (Stdio::from(file), Stdio::from(cloned)),
            Err(_) => (Stdio::from(file), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    }
}

/// Resolves Windows PowerShell via the full System32 path (`%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe`).
pub fn windows_system_powershell_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

/// Resolves a System32 executable on Windows (`%SystemRoot%\System32\<name>`).
#[cfg(windows)]
fn windows_system32_exe(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    root.join("System32").join(name)
}

/// Restricts a Windows file's DACL to owner-only (`*<current-user-SID>:F` plus
/// `NT AUTHORITY\SYSTEM` `*S-1-5-18:F`) with inheritance disabled via System32 `icacls.exe`.
#[cfg(windows)]
fn restrict_windows_file_to_owner(path: &Path) -> Result<(), AppError> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let whoami = windows_system32_exe("whoami.exe");
    let owner_grant = Command::new(&whoami)
        .args(["/user", "/fo", "csv", "/nh"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| {
            let line = String::from_utf8_lossy(&out.stdout);
            let sid = line
                .trim()
                .rsplit(',')
                .next()?
                .trim()
                .trim_matches('"')
                .trim();
            if sid.starts_with("S-1-")
                && sid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'S' | b'-'))
            {
                Some(format!("*{sid}:F"))
            } else {
                None
            }
        })
        .or_else(|| {
            let user = std::env::var("USERNAME").ok()?;
            let user = user.trim();
            if user.is_empty() {
                return None;
            }
            if let Ok(domain) = std::env::var("USERDOMAIN")
                && !domain.trim().is_empty()
            {
                Some(format!("{}\\{}:F", domain.trim(), user))
            } else {
                Some(format!("{user}:F"))
            }
        })
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::Failed,
                "Could not determine current Windows user for supervisor script permissions.",
            )
        })?;

    let icacls = windows_system32_exe("icacls.exe");
    let out = Command::new(&icacls)
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &owner_grant,
            "/grant:r",
            "*S-1-5-18:F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .output()
        .map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not run icacls on {}: {err}", path.display()),
            )
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not restrict permissions on {}: {} {}",
                path.display(),
                stdout.trim(),
                stderr.trim()
            ),
        ));
    }
    Ok(())
}

/// Writes [`WINDOWS_NSIS_SUPERVISOR_SCRIPT`] (with a UTF-8 BOM so Windows PowerShell 5.1 always
/// parses it as UTF-8) to a private owner-only `.ps1` file inside `updates_dir` so `powershell.exe`
/// runs it with `-File` and a short command line well below Windows's 32,767-character limit.
pub fn write_windows_supervisor_script(updates_dir: &Path, pid: u32) -> Result<PathBuf, AppError> {
    ensure_private_dir(updates_dir)?;
    let final_path = updates_dir.join(format!("supervisor-{pid}.ps1"));
    let tmp_path = updates_dir.join(format!(".supervisor-{pid}-{}.ps1.tmp", now_unix_u64()));
    let _ = fs::remove_file(&tmp_path);

    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp_path).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not create supervisor script {}: {err}",
                tmp_path.display()
            ),
        )
    })?;
    let write_res = file
        .write_all(&[0xEF, 0xBB, 0xBF])
        .and_then(|()| file.write_all(WINDOWS_NSIS_SUPERVISOR_SCRIPT.as_bytes()))
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not write supervisor script {}: {err}",
                tmp_path.display()
            ),
        ));
    }

    #[cfg(windows)]
    if let Err(err) = restrict_windows_file_to_owner(&tmp_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(err);
    }

    for attempt in 0..5u8 {
        match fs::rename(&tmp_path, &final_path) {
            Ok(()) => return Ok(final_path),
            Err(err) if attempt < 4 => {
                let _ = err;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Could not finalize supervisor script {}: {err}",
                        final_path.display()
                    ),
                ));
            }
        }
    }
    Ok(final_path)
}

/// Fixed PowerShell supervisor script for Windows NSIS silent upgrade with automatic rollback.
///
/// Written to an owner-only `.ps1` file in the updates directory and invoked via System32
/// `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File <path>` so the command
/// line stays short (well under Windows's 32,767-character `CreateProcessW` limit). Receives all
/// paths strictly through environment variables, uses `-LiteralPath` on every filesystem cmdlet,
/// invokes the NSIS installer directly via `ProcessStartInfo` (`UseShellExecute = $false`) with
/// `/S /UPDATE /D=<install_dir>` (`/D=` last and unquoted), writes progress lines to stdout
/// (redirected by the parent to `supervisor.log`), and restores the previous copy transactionally
/// on any installer or startup failure.
pub const WINDOWS_NSIS_SUPERVISOR_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$script:supLockStream = $null
function Write-Log([string]$msg) {
  try {
    $line = "$(Get-Date -Format 'yyyy-MM-ddTHH:mm:ss.fffZ') $msg"
    [Console]::Out.WriteLine($line)
    [Console]::Out.Flush()
  } catch {}
}

function Release-UpdateLocks {
  if ($script:supLockStream) {
    try { $script:supLockStream.Dispose() } catch {}
    $script:supLockStream = $null
  }
  if ($env:LEGION_SUPERVISOR_LOCK -and (Test-Path -LiteralPath $env:LEGION_SUPERVISOR_LOCK)) {
    Remove-Item -LiteralPath $env:LEGION_SUPERVISOR_LOCK -Force -ErrorAction SilentlyContinue
  }
  if ($env:LEGION_UPDATE_LOCK -and (Test-Path -LiteralPath $env:LEGION_UPDATE_LOCK)) {
    Remove-Item -LiteralPath $env:LEGION_UPDATE_LOCK -Force -ErrorAction SilentlyContinue
  }
}

if ($env:LEGION_SUPERVISOR_LOCK) {
  try {
    $script:supLockStream = [System.IO.File]::Open(
      $env:LEGION_SUPERVISOR_LOCK,
      [System.IO.FileMode]::OpenOrCreate,
      [System.IO.FileAccess]::ReadWrite,
      [System.IO.FileShare]::None
    )
  } catch {
    Write-Log "failed to hold supervisor lock $($env:LEGION_SUPERVISOR_LOCK): $($_.Exception.Message)"
  }
}

function Start-Direct([string]$filePath, [string]$arguments, [bool]$noWindow) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $filePath
  if ($arguments) { $psi.Arguments = $arguments }
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $noWindow
  return [System.Diagnostics.Process]::Start($psi)
}

function Get-InstallDirProcesses([string]$installDir) {
  if (-not $installDir) { return @() }
  $prefix = $installDir.TrimEnd('\') + '\'
  $found = New-Object System.Collections.Generic.List[object]
  foreach ($p in (Get-Process -ErrorAction SilentlyContinue)) {
    if ($p.Id -eq $PID) { continue }
    $pPath = ''
    try { $pPath = $p.Path } catch {}
    if ($pPath -and $pPath.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
      $found.Add($p)
    }
  }
  return $found.ToArray()
}

function Get-LegionWebViewProcesses {
  $found = New-Object System.Collections.Generic.List[object]
  try {
    $searcher = New-Object System.Management.ManagementObjectSearcher("SELECT ProcessId, CommandLine FROM Win32_Process WHERE Name = 'msedgewebview2.exe'")
    foreach ($p in $searcher.Get()) {
      $cl = [string]$p['CommandLine']
      if ($cl -and $cl -match 'com\.x1f4r\.legion') {
        $found.Add($p)
      }
    }
  } catch {}
  return $found.ToArray()
}

function Capture-RollbackPs {
  if (-not $env:LEGION_ROLLBACK_PS_FILE) { return }
  try {
    $lines = New-Object System.Collections.Generic.List[string]
    foreach ($p in (Get-Process -ErrorAction SilentlyContinue | Sort-Object Id)) {
      $pPath = ''
      try { $pPath = $p.Path } catch {}
      $lines.Add("pid=$($p.Id) name=$($p.ProcessName) path=$pPath")
    }
    Set-Content -LiteralPath $env:LEGION_ROLLBACK_PS_FILE -Value ($lines -join "`n") -Encoding UTF8
  } catch {
    Write-Log "failed to write rollback process list: $($_.Exception.Message)"
  }
}

function Save-CappedNewStartLog([string]$outLog, [string]$errLog) {
  if (-not $env:LEGION_NEW_LOG) { return }
  try {
    $chunks = New-Object System.Collections.Generic.List[string]
    foreach ($src in @($outLog, $errLog)) {
      if ($src -and (Test-Path -LiteralPath $src)) {
        $raw = Get-Content -LiteralPath $src -Raw -ErrorAction SilentlyContinue
        if ($raw) { $chunks.Add($raw) }
      }
    }
    $combined = ($chunks -join "`n")
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($combined)
    if ($bytes.Length -gt 32768) {
      $combined = [System.Text.Encoding]::UTF8.GetString($bytes, $bytes.Length - 32768, 32768)
    }
    Set-Content -LiteralPath $env:LEGION_NEW_LOG -Value $combined -Encoding UTF8
  } catch {
    Write-Log "failed to save capped startup log: $($_.Exception.Message)"
  }
  foreach ($src in @($outLog, $errLog)) {
    if ($src -and (Test-Path -LiteralPath $src)) {
      Remove-Item -LiteralPath $src -Force -ErrorAction SilentlyContinue
    }
  }
}

function Test-FileUnlocked([string]$filePath) {
  if (-not (Test-Path -LiteralPath $filePath)) { return $true }
  $stream = $null
  try {
    $stream = [System.IO.File]::Open(
      $filePath,
      [System.IO.FileMode]::Open,
      [System.IO.FileAccess]::ReadWrite,
      [System.IO.FileShare]::None
    )
    return $true
  } catch {
    return $false
  } finally {
    if ($stream) {
      try { $stream.Dispose() } catch {}
    }
  }
}

function Write-RollbackNotice([string]$installState, [string]$extraDetail) {
  $payload = if ($installState -eq 'new_kept_files_in_use' -and $env:LEGION_ROLLBACK_KEPT_NEW_JSON) {
    $env:LEGION_ROLLBACK_KEPT_NEW_JSON
  } else {
    $env:LEGION_ROLLBACK_JSON
  }
  try {
    $obj = $payload | ConvertFrom-Json
    $obj | Add-Member -NotePropertyName 'install_state' -NotePropertyValue $installState -Force
    if ($extraDetail) {
      $obj | Add-Member -NotePropertyName 'detail' -NotePropertyValue $extraDetail -Force
    }
    $payload = $obj | ConvertTo-Json -Compress
  } catch {}
  try {
    [System.IO.File]::WriteAllText($env:LEGION_ROLLBACK_FILE, $payload, (New-Object System.Text.UTF8Encoding($false)))
  } catch {
    Write-Log "failed to write rollback notice: $($_.Exception.Message)"
  }
}

$oldPid = [int]$env:LEGION_OLD_PID
$verifyOnly = ($env:LEGION_VERIFY_ONLY -eq '1')
$preWaitSecs = [int]$env:LEGION_PRE_SWAP_WAIT_SECS
if ($preWaitSecs -le 0) { $preWaitSecs = 20 }
$rollbackRetries = [int]$env:LEGION_ROLLBACK_RETRIES
if ($rollbackRetries -le 0) { $rollbackRetries = 20 }

$script:restoreAttempted = $false
function Restore-Previous([string]$why) {
  if ($script:restoreAttempted) { return }
  $script:restoreAttempted = $true
  Write-Log "restoring previous install: $why"

  $hasBackup = $false
  try {
    $hasBackup = [bool]($env:LEGION_BACKUP_DIR -and (Test-Path -LiteralPath $env:LEGION_BACKUP_DIR))
  } catch {
    Write-Log "failed to inspect backup dir: $($_.Exception.Message)"
  }

  if (-not $hasBackup) {
    Write-Log "backup dir missing: $($env:LEGION_BACKUP_DIR); keeping new install intact"
    Write-RollbackNotice 'new_kept_backup_missing' "Backup folder missing; kept new install in place."
    return
  }

  $backupRoot = (Get-Item -LiteralPath $env:LEGION_BACKUP_DIR).FullName.TrimEnd('\')
  $backupFiles = @()
  try {
    $backupFiles = @(Get-ChildItem -LiteralPath $backupRoot -Recurse -File -Force)
  } catch {
    Write-Log "failed to list backup dir: $($_.Exception.Message)"
    Write-RollbackNotice 'new_kept_files_in_use' "Could not enumerate backup folder: $($_.Exception.Message)"
    return
  }

  # Phase 1: Bounded retry waiting until no install-dir process is running and EVERY file that
  # would be overwritten can be opened exclusively. Never touch a single file while another process
  # holds any target file open so the install directory can never end up half-copied.
  $canRestore = $false
  $busyReason = ''
  for ($r = 0; $r -lt $rollbackRetries; $r++) {
    $busyReason = ''
    $dirProcs = @(Get-InstallDirProcesses $env:LEGION_INSTALL_DIR)
    if ($dirProcs.Count -gt 0) {
      $busyReason = "install-dir process pid=$($dirProcs[0].Id) ($($dirProcs[0].ProcessName)) still running"
    } else {
      foreach ($bf in $backupFiles) {
        $rel = $bf.FullName.Substring($backupRoot.Length).TrimStart('\')
        $dst = Join-Path $env:LEGION_INSTALL_DIR $rel
        if (-not (Test-FileUnlocked $dst)) {
          $busyReason = "file '$rel' is in use by another process"
          break
        }
      }
    }
    if (-not $busyReason) {
      $canRestore = $true
      break
    }
    Start-Sleep -Milliseconds 250
  }

  if (-not $canRestore) {
    Write-Log "rollback could not acquire exclusive access after $rollbackRetries attempts ($busyReason); keeping new install intact"
    Write-RollbackNotice 'new_kept_files_in_use' "Could not restore previous install because $busyReason; kept new install intact."
    return
  }

  if ($env:LEGION_INSTALLER -and (Test-Path -LiteralPath $env:LEGION_INSTALLER)) {
    Remove-Item -LiteralPath $env:LEGION_INSTALLER -Force -ErrorAction SilentlyContinue
  }

  # Phase 2: Transactional swap with undo — move all existing target files into a staging snapshot
  # first, then copy backup files in. If any step fails mid-way, remove newly created files and
  # restore all staged new files back so the install directory is never left half-copied.
  $stageNew = "$backupRoot.new-snap"
  if (Test-Path -LiteralPath $stageNew) {
    Remove-Item -LiteralPath $stageNew -Recurse -Force -ErrorAction SilentlyContinue
  }
  $moved = New-Object System.Collections.Generic.List[object]
  $copied = New-Object System.Collections.Generic.List[string]
  $swapFailed = $false
  $swapError = ''

  foreach ($bf in $backupFiles) {
    $rel = $bf.FullName.Substring($backupRoot.Length).TrimStart('\')
    $dst = Join-Path $env:LEGION_INSTALL_DIR $rel
    if (Test-Path -LiteralPath $dst) {
      $stagedDst = Join-Path $stageNew $rel
      $stagedParent = Split-Path -LiteralPath $stagedDst -Parent
      try {
        if ($stagedParent -and -not (Test-Path -LiteralPath $stagedParent)) {
          [System.IO.Directory]::CreateDirectory($stagedParent) | Out-Null
        }
        Move-Item -LiteralPath $dst -Destination $stagedDst -Force
        $moved.Add(@{ Src = $dst; Staged = $stagedDst })
      } catch {
        $swapFailed = $true
        $swapError = "could not move '$rel' aside: $($_.Exception.Message)"
        break
      }
    }
  }

  if (-not $swapFailed) {
    foreach ($bf in $backupFiles) {
      $rel = $bf.FullName.Substring($backupRoot.Length).TrimStart('\')
      $dst = Join-Path $env:LEGION_INSTALL_DIR $rel
      $dstParent = Split-Path -LiteralPath $dst -Parent
      try {
        if ($dstParent -and -not (Test-Path -LiteralPath $dstParent)) {
          [System.IO.Directory]::CreateDirectory($dstParent) | Out-Null
        }
        Copy-Item -LiteralPath $bf.FullName -Destination $dst -Force
        $copied.Add($dst)
      } catch {
        $swapFailed = $true
        $swapError = "could not restore '$rel': $($_.Exception.Message)"
        break
      }
    }
  }

  if ($swapFailed) {
    Write-Log "transactional rollback aborted ($swapError); undoing partial changes so new install stays intact"
    foreach ($c in $copied) {
      if (Test-Path -LiteralPath $c) {
        Remove-Item -LiteralPath $c -Force -ErrorAction SilentlyContinue
      }
    }
    foreach ($m in $moved) {
      try {
        if (Test-Path -LiteralPath $m.Staged) {
          Move-Item -LiteralPath $m.Staged -Destination $m.Src -Force
        }
      } catch {
        Write-Log "failed to undo staged file $($m.Src): $($_.Exception.Message)"
      }
    }
    if (Test-Path -LiteralPath $stageNew) {
      Remove-Item -LiteralPath $stageNew -Recurse -Force -ErrorAction SilentlyContinue
    }
    Write-RollbackNotice 'new_kept_files_in_use' "Rollback could not replace install files ($swapError); kept new install intact."
    return
  }

  if (Test-Path -LiteralPath $stageNew) {
    Remove-Item -LiteralPath $stageNew -Recurse -Force -ErrorAction SilentlyContinue
  }
  Write-Log "restored previous install cleanly ($($backupFiles.Count) files)"
  Write-RollbackNotice 'restored_previous' ''

  $reopenNeeded = $false
  if ($verifyOnly -and $env:LEGION_REOPEN_FILE -and (Test-Path -LiteralPath $env:LEGION_REOPEN_FILE)) {
    Remove-Item -LiteralPath $env:LEGION_REOPEN_FILE -Force -ErrorAction SilentlyContinue
    $reopenNeeded = $true
  }
  Release-UpdateLocks

  if (-not $verifyOnly -or $reopenNeeded) {
    try {
      if (Test-Path -LiteralPath $env:LEGION_EXE) {
        Remove-Item Env:LEGION_UPDATE_VERIFY_ONLY -ErrorAction SilentlyContinue
        Remove-Item Env:LEGION_UPDATE_SUPERVISED_LAUNCH -ErrorAction SilentlyContinue
        if ($env:LEGION_RELAUNCH_PATH) { $env:PATH = $env:LEGION_RELAUNCH_PATH }
        Write-Log "relaunching restored executable after rollback: $($env:LEGION_EXE)"
        Start-Direct $env:LEGION_EXE '' $false | Out-Null
      } else {
        Write-Log "cannot relaunch after rollback: executable missing at $($env:LEGION_EXE)"
      }
    } catch {
      Write-Log "failed to relaunch executable after rollback: $($_.Exception.Message)"
    }
  }
}

try {
  Write-Log "supervisor start oldPid=$oldPid verifyOnly=$verifyOnly installer=$($env:LEGION_INSTALLER) installDir=$($env:LEGION_INSTALL_DIR) exe=$($env:LEGION_EXE)"

  # 1. Wait up to $preWaitSecs for the old process, any other process running from $installDir,
  # and any Legion WebView2 helpers to exit. If still in use when the bound expires, defer cleanly
  # without touching the install or counting toward the two-failures rule.
  $preTicks = $preWaitSecs * 4
  $stillUp = $false
  for ($i = 0; $i -lt $preTicks; $i++) {
    $oldAlive = [bool](Get-Process -Id $oldPid -ErrorAction SilentlyContinue)
    $dirProcs = @(Get-InstallDirProcesses $env:LEGION_INSTALL_DIR)
    $wvProcs = if (-not $oldAlive -and $dirProcs.Count -eq 0) { @(Get-LegionWebViewProcesses) } else { @() }
    if (-not $oldAlive -and $dirProcs.Count -eq 0 -and $wvProcs.Count -eq 0) {
      $stillUp = $false
      break
    }
    $stillUp = $true
    Start-Sleep -Milliseconds 250
  }
  if ($stillUp) {
    $oldAlive = [bool](Get-Process -Id $oldPid -ErrorAction SilentlyContinue)
    $dirProcs = @(Get-InstallDirProcesses $env:LEGION_INSTALL_DIR)
    $wvProcs = @(Get-LegionWebViewProcesses)
    Write-Log "install still in use after ${preWaitSecs}s (oldAlive=$oldAlive, dirProcs=$($dirProcs.Count), webviewProcs=$($wvProcs.Count)); deferring update cleanly without changing install"
    if ($env:LEGION_INSTALLER -and (Test-Path -LiteralPath $env:LEGION_INSTALLER)) {
      Remove-Item -LiteralPath $env:LEGION_INSTALLER -Force -ErrorAction SilentlyContinue
    }
    if ($env:LEGION_BACKUP_DIR -and (Test-Path -LiteralPath $env:LEGION_BACKUP_DIR)) {
      Remove-Item -LiteralPath $env:LEGION_BACKUP_DIR -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($env:LEGION_DEFERRED_FILE -and $env:LEGION_DEFERRED_JSON) {
      [System.IO.File]::WriteAllText($env:LEGION_DEFERRED_FILE, $env:LEGION_DEFERRED_JSON, (New-Object System.Text.UTF8Encoding($false)))
    }
    exit 0
  }
  Write-Log "old instance and helpers exited cleanly"
  Start-Sleep -Milliseconds 250

  if (Test-Path -LiteralPath $env:LEGION_READY_MARKER) {
    Remove-Item -LiteralPath $env:LEGION_READY_MARKER -Force -ErrorAction SilentlyContinue
  }
  if ($env:LEGION_ROLLBACK_PS_FILE -and (Test-Path -LiteralPath $env:LEGION_ROLLBACK_PS_FILE)) {
    Remove-Item -LiteralPath $env:LEGION_ROLLBACK_PS_FILE -Force -ErrorAction SilentlyContinue
  }

  $nsisArgs = "/S /UPDATE /D=" + $env:LEGION_INSTALL_DIR
  Write-Log "running NSIS installer with args: $nsisArgs"
  $inst = Start-Direct $env:LEGION_INSTALLER $nsisArgs $true
  $inst.WaitForExit()
  $instCode = $inst.ExitCode
  Write-Log "NSIS installer exited with code $instCode"
  if ($instCode -ne 0) {
    Restore-Previous "NSIS exited with $instCode"
    exit 1
  }
  if ($env:LEGION_RELAUNCH_PATH) {
    $env:PATH = $env:LEGION_RELAUNCH_PATH
  }
  if ($verifyOnly) {
    $env:LEGION_UPDATE_VERIFY_ONLY = '1'
  } else {
    Remove-Item Env:LEGION_UPDATE_VERIFY_ONLY -ErrorAction SilentlyContinue
  }
  $env:LEGION_UPDATE_SUPERVISED_LAUNCH = if ($env:LEGION_TARGET_SEQUENCE) { $env:LEGION_TARGET_SEQUENCE } else { '1' }

  $outLog = $null
  $errLog = $null
  if ($env:LEGION_NEW_LOG) {
    $outLog = "$($env:LEGION_NEW_LOG).stdout"
    $errLog = "$($env:LEGION_NEW_LOG).stderr"
    foreach ($f in @($env:LEGION_NEW_LOG, $outLog, $errLog)) {
      if (Test-Path -LiteralPath $f) {
        Remove-Item -LiteralPath $f -Force -ErrorAction SilentlyContinue
      }
    }
  }
  Write-Log "launching updated app: $($env:LEGION_EXE)"
  if ($outLog -and $errLog) {
    $proc = Start-Process -FilePath $env:LEGION_EXE -RedirectStandardOutput $outLog -RedirectStandardError $errLog -PassThru
  } else {
    $proc = Start-Direct $env:LEGION_EXE '' $false
  }
  Write-Log "launched updated app pid=$($proc.Id)"
  $waitSecs = [int]$env:LEGION_WAIT_SECS
  if ($waitSecs -le 0) { $waitSecs = 30 }
  $graceSecs = [int]$env:LEGION_EXIT_GRACE_SECS
  if ($graceSecs -le 0) { $graceSecs = 8 }
  $maxTicks = $waitSecs * 4
  $graceTicks = $graceSecs * 4
  $started = $false
  $exitedLogged = $false
  for ($j = 0; $j -lt $maxTicks; $j++) {
    if (Test-Path -LiteralPath $env:LEGION_READY_MARKER) {
      $started = $true
      break
    }
    if ($proc.HasExited) {
      if (-not $exitedLogged) {
        $exitedLogged = $true
        Write-Log "updated app pid=$($proc.Id) exited with code $($proc.ExitCode) after $j ticks"
        if ($proc.ExitCode -ne 0) {
          $graceEnd = $j + $graceTicks
          if ($graceEnd -lt $maxTicks) {
            $maxTicks = $graceEnd
          }
        }
      }
    }
    Start-Sleep -Milliseconds 250
  }
  if (Test-Path -LiteralPath $env:LEGION_READY_MARKER) {
    $started = $true
  }
  if ($started) {
    Write-Log "ready marker observed at $($env:LEGION_READY_MARKER); cleaning up backup"
    if (Test-Path -LiteralPath $env:LEGION_BACKUP_DIR) {
      Remove-Item -LiteralPath $env:LEGION_BACKUP_DIR -Recurse -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $env:LEGION_INSTALLER) {
      Remove-Item -LiteralPath $env:LEGION_INSTALLER -Force -ErrorAction SilentlyContinue
    }
    if ($env:LEGION_ROLLBACK_PS_FILE -and (Test-Path -LiteralPath $env:LEGION_ROLLBACK_PS_FILE)) {
      Remove-Item -LiteralPath $env:LEGION_ROLLBACK_PS_FILE -Force -ErrorAction SilentlyContinue
    }
    $reopenNeeded = $false
    if ($verifyOnly -and $env:LEGION_REOPEN_FILE -and (Test-Path -LiteralPath $env:LEGION_REOPEN_FILE)) {
      Remove-Item -LiteralPath $env:LEGION_REOPEN_FILE -Force -ErrorAction SilentlyContinue
      $reopenNeeded = $true
    }
    Release-UpdateLocks
    if ($reopenNeeded -and (Test-Path -LiteralPath $env:LEGION_EXE)) {
      Remove-Item Env:LEGION_UPDATE_VERIFY_ONLY -ErrorAction SilentlyContinue
      Remove-Item Env:LEGION_UPDATE_SUPERVISED_LAUNCH -ErrorAction SilentlyContinue
      Write-Log "user reopened app during close update; launching updated copy interactively"
      Start-Direct $env:LEGION_EXE '' $false | Out-Null
    }
    exit 0
  }
  Capture-RollbackPs
  if (-not $proc.HasExited) {
    Write-Log "ready marker not observed after $j ticks; stopping pid=$($proc.Id)"
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  }
  Start-Sleep -Milliseconds 500
  Save-CappedNewStartLog $outLog $errLog
  Restore-Previous "ready marker not observed"
  exit 1
} catch {
  Write-Log "supervisor exception: $($_.Exception.Message)"
  Restore-Previous $_.Exception.Message
  exit 1
} finally {
  Release-UpdateLocks
  if ($env:LEGION_SUPERVISOR_SCRIPT -and (Test-Path -LiteralPath $env:LEGION_SUPERVISOR_SCRIPT)) {
    Remove-Item -LiteralPath $env:LEGION_SUPERVISOR_SCRIPT -Force -ErrorAction SilentlyContinue
  }
}
"#;

/// Fixed allow-list of harmless Windows session, OS, and proxy variables preserved when spawning
/// the detached PowerShell supervisor and relaunching Legion.
pub const WIN_ALLOWED_VARS: &[&str] = &[
    "SystemRoot",
    "WINDIR",
    "SystemDrive",
    "COMSPEC",
    "PATHEXT",
    "LOCALAPPDATA",
    "APPDATA",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "USERNAME",
    "USERDOMAIN",
    "COMPUTERNAME",
    "PUBLIC",
    "ALLUSERSPROFILE",
    "TEMP",
    "TMP",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "ProgramData",
    "CommonProgramFiles",
    "CommonProgramFiles(x86)",
    "CommonProgramW6432",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "OS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "LEGION_HOME",
    "LEGION_APP_READY_FILE",
    "LEGION_APP_ALLOW_LOOPBACK_ENDPOINTS",
];

/// Filters `raw_env` to the fixed allow-list of harmless Windows variables (plus `WEBVIEW2_*`) so
/// the relaunched app keeps standard Windows and proxy variables while stripping hostile variables.
pub fn build_sanitized_windows_env<I>(raw_env: I) -> BTreeMap<OsString, OsString>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let mut kept = BTreeMap::new();
    for (k, v) in raw_env {
        if let Some(key) = k.to_str()
            && (WIN_ALLOWED_VARS
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(key))
                || key.to_ascii_uppercase().starts_with("WEBVIEW2_"))
        {
            kept.insert(k, v);
        }
    }
    kept
}

/// Copies-while-hashing the verified NSIS installer into `verified_dir`, backs up the current
/// Windows installation directory, writes the supervisor script to an owner-only `.ps1` file in
/// the updates directory, and spawns detached System32 PowerShell via `-File` with job breakaway
/// (`CREATE_BREAKAWAY_FROM_JOB`, falling back if forbidden) and a cleaned environment.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub fn stage_and_spawn_windows_nsis(
    verified_installer: &Path,
    expected_sha256: &str,
    expected_size: u64,
    verified_dir: &Path,
    install_dir: &Path,
    exe_path: &Path,
    backup_dir: &Path,
    ready_marker_path: &Path,
    rollback_marker_path: &Path,
    rollback_json: String,
    rollback_kept_new_json: String,
    deferred_marker_path: &Path,
    deferred_json: String,
    target_sequence: u64,
    wait_secs: u64,
    verify_only: bool,
) -> Result<u32, AppError> {
    use std::os::windows::process::CommandExt;

    let dir_str = install_dir.to_string_lossy();
    if dir_str.contains('"') || dir_str.contains('\r') || dir_str.contains('\n') {
        return Err(AppError::new(
            ErrorCode::Failed,
            "Installation directory path contains unsupported characters.",
        ));
    }

    let pid = std::process::id();
    ensure_private_dir(verified_dir)?;
    let staged_installer = verified_dir.join(format!("verified-{pid}-setup.exe"));
    copy_and_verify_sha256(
        verified_installer,
        &staged_installer,
        expected_sha256,
        expected_size,
        0o600,
    )?;

    let _ = fs::remove_dir_all(backup_dir);
    copy_dir_bounded(install_dir, backup_dir, 0)?;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let ps_path = windows_system_powershell_path();
    let sys_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .unwrap_or_else(|| OsString::from(r"C:\Windows"));
    let sys_root_path = PathBuf::from(&sys_root);
    let clean_path = std::env::join_paths([
        sys_root_path.join("System32"),
        sys_root_path.clone(),
        sys_root_path.join("System32").join("Wbem"),
        sys_root_path
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0"),
    ])
    .unwrap_or_else(|_| OsString::from(r"C:\Windows\System32;C:\Windows"));

    // Preserve non-temp user PATH entries for the relaunched app (while keeping the supervisor
    // shell itself strictly on System32 PATH).
    let sys_tmp = std::env::temp_dir();
    let sys_tmp_lower = sys_tmp.to_string_lossy().to_ascii_lowercase();
    let relaunch_path = std::env::var_os("PATH")
        .and_then(|raw| {
            let kept: Vec<PathBuf> = std::env::split_paths(&raw)
                .filter(|p| {
                    p.is_absolute()
                        && !p
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .starts_with(&sys_tmp_lower)
                })
                .collect();
            if kept.is_empty() {
                None
            } else {
                std::env::join_paths(kept).ok()
            }
        })
        .unwrap_or_else(|| clean_path.clone());

    let sanitized_win_env = build_sanitized_windows_env(std::env::vars_os());
    let updates_dir = ready_marker_path.parent();
    let script_dir = updates_dir.unwrap_or(verified_dir);
    let script_path = write_windows_supervisor_script(script_dir, pid)?;
    let log_path = updates_dir.map(|d| d.join("supervisor.log"));
    let new_start_log_path = updates_dir.map(|d| d.join(NEW_START_LOG_FILENAME));
    let rollback_ps_path = updates_dir.map(|d| d.join(ROLLBACK_PS_FILENAME));
    let update_lock_path = updates_dir.map(|d| d.join(UPDATE_LOCK_FILENAME));
    let sup_lock_path = updates_dir.map(|d| d.join(UPDATE_SUPERVISOR_LOCK_FILENAME));
    let reopen_path = updates_dir.map(|d| d.join(REOPEN_REQUESTED_FILENAME));
    if let Some(lp) = &log_path {
        let _ = fs::remove_file(lp);
        if let Some(dir) = lp.parent() {
            let _ = fs::write(
                dir.join("supervisor-script.ps1.txt"),
                WINDOWS_NSIS_SUPERVISOR_SCRIPT,
            );
        }
    }

    let build_cmd = |flags: u32| {
        let (stdout_cfg, stderr_cfg) = open_supervisor_log_stdio(log_path.as_deref());
        let mut cmd = Command::new(&ps_path);
        cmd.env_clear();
        cmd.envs(&sanitized_win_env);
        cmd.env("PATH", &clean_path)
            .env("LEGION_RELAUNCH_PATH", &relaunch_path)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script_path)
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(stdout_cfg)
            .stderr(stderr_cfg)
            .env("LEGION_SUPERVISOR_SCRIPT", &script_path)
            .env("LEGION_OLD_PID", pid.to_string())
            .env("LEGION_INSTALLER", &staged_installer)
            .env("LEGION_INSTALL_DIR", install_dir)
            .env("LEGION_EXE", exe_path)
            .env("LEGION_BACKUP_DIR", backup_dir)
            .env("LEGION_READY_MARKER", ready_marker_path)
            .env("LEGION_ROLLBACK_FILE", rollback_marker_path)
            .env("LEGION_ROLLBACK_JSON", &rollback_json)
            .env("LEGION_ROLLBACK_KEPT_NEW_JSON", &rollback_kept_new_json)
            .env("LEGION_DEFERRED_FILE", deferred_marker_path)
            .env("LEGION_DEFERRED_JSON", &deferred_json)
            .env("LEGION_TARGET_SEQUENCE", target_sequence.to_string())
            .env("LEGION_PRE_SWAP_WAIT_SECS", "20")
            .env("LEGION_WAIT_SECS", wait_secs.to_string())
            .env(
                "LEGION_EXIT_GRACE_SECS",
                DEFAULT_EXIT_GRACE_SECS.to_string(),
            )
            .env(
                "LEGION_ROLLBACK_RETRIES",
                DEFAULT_ROLLBACK_RETRIES.to_string(),
            )
            .env("LEGION_VERIFY_ONLY", if verify_only { "1" } else { "0" });
        if let Some(lp) = &log_path {
            cmd.env("LEGION_SUPERVISOR_LOG", lp);
        }
        if let Some(nl) = &new_start_log_path {
            cmd.env("LEGION_NEW_LOG", nl);
        }
        if let Some(ps) = &rollback_ps_path {
            cmd.env("LEGION_ROLLBACK_PS_FILE", ps);
        }
        if let Some(ul) = &update_lock_path {
            cmd.env("LEGION_UPDATE_LOCK", ul);
        }
        if let Some(sl) = &sup_lock_path {
            cmd.env("LEGION_SUPERVISOR_LOCK", sl);
        }
        if let Some(rp) = &reopen_path {
            cmd.env("LEGION_REOPEN_FILE", rp);
        }
        cmd
    };

    // Match `legion-os::detach`: `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` (never combined
    // with `DETACHED_PROCESS`, which is mutually exclusive with `CREATE_NO_WINDOW`). Try breaking
    // out of the parent's Windows Job Object first so exiting the app never kills the supervisor
    // or NSIS installer; fall back without breakaway if the job forbids it (`ERROR_ACCESS_DENIED`).
    let base_flags = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    match build_cmd(base_flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(child) => Ok(child.id()),
        Err(err)
            if err.kind() == io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(5) =>
        {
            match build_cmd(base_flags).spawn() {
                Ok(child) => Ok(child.id()),
                Err(err2) => {
                    let _ = fs::remove_file(&script_path);
                    Err(AppError::new(
                        ErrorCode::Failed,
                        format!("Could not start Windows update helper: {err2}"),
                    ))
                }
            }
        }
        Err(err) => {
            let _ = fs::remove_file(&script_path);
            Err(AppError::new(
                ErrorCode::Failed,
                format!("Could not start Windows update helper: {err}"),
            ))
        }
    }
}

/// Copies a directory tree up to a bounded depth (used for Windows install backup before NSIS).
pub fn copy_dir_bounded(src: &Path, dst: &Path, depth: usize) -> Result<(), AppError> {
    if depth > 8 {
        return Err(AppError::new(
            ErrorCode::Failed,
            "Installation directory nesting exceeded the backup depth limit.",
        ));
    }
    fs::create_dir_all(dst).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create backup folder {}: {err}", dst.display()),
        )
    })?;
    let entries = fs::read_dir(src).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not read installation folder {}: {err}",
                src.display()
            ),
        )
    })?;
    for entry in entries.flatten() {
        let meta = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if meta.is_file() {
            fs::copy(&from, &to).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not back up {}: {err}", from.display()),
                )
            })?;
        } else if meta.is_dir() {
            copy_dir_bounded(&from, &to, depth + 1)?;
        }
    }
    Ok(())
}

/// Quotes a filesystem path safely for display in a POSIX shell command (such as `sudo apt-get install`).
pub fn quote_posix_arg(path: &Path) -> String {
    let s = path.to_string_lossy();
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b'+'))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn deb_never_self_installs_and_formats_exact_command() {
        let deb = DesktopInstall::LinuxDeb {
            package: "legion-app".to_owned(),
        };
        assert!(!deb.can_self_install());
        assert!(deb.should_download_artifact());
        assert_eq!(deb.artifact_kind(), InstallKind::Deb);
        let cmd = deb
            .manual_command_for(Some(Path::new(
                "/home/me/.local/share/legion/updates/verified/legion_0.2.0.deb",
            )))
            .unwrap();
        assert_eq!(
            cmd,
            "sudo apt-get install /home/me/.local/share/legion/updates/verified/legion_0.2.0.deb"
        );

        let spaced = deb
            .manual_command_for(Some(Path::new("/home/me/my updates/legion 0.2.0.deb")))
            .unwrap();
        assert_eq!(
            spaced,
            "sudo apt-get install '/home/me/my updates/legion 0.2.0.deb'"
        );

        let pacman = DesktopInstall::LinuxPacman {
            package: "legion-app".to_owned(),
        };
        assert!(!pacman.can_self_install());
        assert!(!pacman.should_download_artifact());

        let standalone = DesktopInstall::Standalone {
            check_kind: InstallKind::AppImage,
            reason: "standalone".to_owned(),
        };
        assert!(!standalone.can_self_install());
        assert!(!standalone.should_download_artifact());
    }

    #[test]
    fn find_macos_app_bundle_recognizes_bundle_and_rejects_plain_binaries() {
        let (bundle, rel) = find_macos_app_bundle(Path::new(
            "/Applications/Legion.app/Contents/MacOS/legion-app",
        ))
        .unwrap();
        assert_eq!(bundle, Path::new("/Applications/Legion.app"));
        assert_eq!(rel, Path::new("Contents/MacOS/legion-app"));

        assert!(find_macos_app_bundle(Path::new("/usr/local/bin/legion-app")).is_none());
    }

    #[test]
    fn copy_and_verify_sha256_accepts_match_and_refuses_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("verified/dst.bin");
        let bytes = b"verified-installer-payload";
        fs::write(&src, bytes).unwrap();
        let digest = legion_update::sha256_bytes(bytes);

        copy_and_verify_sha256(&src, &dst, &digest, bytes.len() as u64, 0o600).unwrap();
        assert_eq!(fs::read(&dst).unwrap(), bytes);

        let wrong_hash = copy_and_verify_sha256(
            &src,
            &dir.path().join("bad-hash.bin"),
            &"0".repeat(64),
            bytes.len() as u64,
            0o600,
        )
        .unwrap_err();
        assert_eq!(wrong_hash.code, ErrorCode::Denied);
        assert!(!dir.path().join("bad-hash.bin").exists());

        let wrong_size = copy_and_verify_sha256(
            &src,
            &dir.path().join("bad-size.bin"),
            &digest,
            (bytes.len() - 1) as u64,
            0o600,
        )
        .unwrap_err();
        assert_eq!(wrong_size.code, ErrorCode::Denied);
        assert!(!dir.path().join("bad-size.bin").exists());
    }

    #[test]
    fn windows_supervisor_uses_literal_path_and_system32_powershell() {
        let ps = windows_system_powershell_path();
        let ps_str = ps.to_string_lossy().to_ascii_lowercase();
        assert!(ps_str.contains("system32"));
        assert!(ps_str.ends_with("powershell.exe"));

        // Supervisor script must be written to an owner-only .ps1 file (with UTF-8 BOM for
        // Windows PowerShell 5.1) so launching via `-File` stays well under Windows's 32,767-char
        // CreateProcessW command-line limit regardless of script length.
        let dir = tempfile::tempdir().unwrap();
        let script_path = write_windows_supervisor_script(dir.path(), 4242).unwrap();
        assert!(script_path.is_file());
        let raw_bytes = fs::read(&script_path).unwrap();
        assert!(
            raw_bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
            "Windows .ps1 script must carry UTF-8 BOM for PowerShell 5.1"
        );
        assert_eq!(
            std::str::from_utf8(&raw_bytes[3..]).unwrap(),
            WINDOWS_NSIS_SUPERVISOR_SCRIPT
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("$env:LEGION_SUPERVISOR_SCRIPT"),
            "Windows supervisor must clean up its own script file in finally"
        );

        // NSIS silent update must pass /S /UPDATE /D= with /D= last and unquoted.
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("\"/S /UPDATE /D=\" + $env:LEGION_INSTALL_DIR"),
            "Windows NSIS supervisor must pass /S /UPDATE /D= unquoted and last"
        );

        // Every Test-Path, Remove-Item, Set-Content, Get-ChildItem, Copy-Item, Move-Item, and
        // Split-Path in the script must use -LiteralPath (never wildcard-expanding -Path or bare
        // positional path).
        for line in WINDOWS_NSIS_SUPERVISOR_SCRIPT.lines() {
            let trimmed = line.trim();
            if trimmed.contains("Test-Path ") {
                assert!(trimmed.contains("Test-Path -LiteralPath "), "{trimmed}");
            }
            if trimmed.contains("Remove-Item ") && !trimmed.contains("Env:") {
                assert!(trimmed.contains("Remove-Item -LiteralPath "), "{trimmed}");
            }
            if trimmed.contains("Set-Content ") {
                assert!(trimmed.contains("Set-Content -LiteralPath "), "{trimmed}");
            }
            if trimmed.contains("Get-ChildItem ") {
                assert!(trimmed.contains("Get-ChildItem -LiteralPath "), "{trimmed}");
            }
            if trimmed.contains("Copy-Item ") {
                assert!(trimmed.contains("Copy-Item -LiteralPath "), "{trimmed}");
            }
            if trimmed.contains("Move-Item ") {
                assert!(trimmed.contains("Move-Item -LiteralPath "), "{trimmed}");
            }
            if trimmed.contains("Split-Path ") {
                assert!(trimmed.contains("Split-Path -LiteralPath "), "{trimmed}");
            }
        }

        // Restore-Previous must guard every step individually, probe file locks before touching
        // any file, undo any mid-way move/copy failure so the install directory can never end up
        // half-copied, and prevent double-execution from the outer catch block.
        let restore_start = WINDOWS_NSIS_SUPERVISOR_SCRIPT
            .find("function Restore-Previous")
            .unwrap();
        let restore_end = WINDOWS_NSIS_SUPERVISOR_SCRIPT[restore_start..]
            .find("\ntry {")
            .unwrap()
            + restore_start;
        let restore_body = &WINDOWS_NSIS_SUPERVISOR_SCRIPT[restore_start..restore_end];
        assert!(
            restore_body.contains("$script:restoreAttempted"),
            "Restore-Previous must guard against double invocation"
        );
        assert!(
            restore_body.contains("Test-FileUnlocked")
                && restore_body.contains("new_kept_files_in_use")
                && restore_body.contains("restored_previous"),
            "Restore-Previous must retry file locks and record exact install_state:\n{restore_body}"
        );
        assert!(
            restore_body.matches("try {").count() >= 4,
            "Restore-Previous must isolate list, transactional swap, notice write, and relaunch in separate try/catch guards:\n{restore_body}"
        );
        assert!(
            restore_body.matches("} catch {").count() >= 4,
            "Restore-Previous must catch failures per step:\n{restore_body}"
        );
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("if ($proc.ExitCode -ne 0)"),
            "Windows supervisor must keep polling for ready marker after clean exit code 0 (close-then-reopen race)"
        );
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("$graceEnd = $j + $graceTicks"),
            "Windows supervisor must wait a grace period on non-zero exit instead of rolling back immediately"
        );
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("Get-InstallDirProcesses")
                && WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("Get-LegionWebViewProcesses"),
            "Windows supervisor must wait for lingering install-dir and Legion WebView2 processes to exit"
        );
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("Release-UpdateLocks")
                && WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("$env:LEGION_DEFERRED_FILE")
                && WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("LEGION_UPDATE_SUPERVISED_LAUNCH")
                && WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("$env:LEGION_REOPEN_FILE"),
            "Windows supervisor must hold/release update locks, defer cleanly when busy, mark supervised launches, and honor reopen-requested"
        );
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("Capture-RollbackPs")
                && WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("$env:LEGION_ROLLBACK_PS_FILE"),
            "Windows supervisor must snapshot processes at rollback decision"
        );
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("Save-CappedNewStartLog")
                && WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("$env:LEGION_NEW_LOG"),
            "Windows supervisor must capture size-capped first-start output"
        );
    }

    #[test]
    fn windows_sanitized_env_preserves_pathext_programw6432_and_proxies_while_stripping_hostile_vars()
     {
        let raw = vec![
            (OsString::from("SystemRoot"), OsString::from(r"C:\Windows")),
            (
                OsString::from("PATHEXT"),
                OsString::from(".COM;.EXE;.BAT;.CMD"),
            ),
            (
                OsString::from("ProgramW6432"),
                OsString::from(r"C:\Program Files"),
            ),
            (
                OsString::from("CommonProgramW6432"),
                OsString::from(r"C:\Program Files\Common Files"),
            ),
            (
                OsString::from("CommonProgramFiles(x86)"),
                OsString::from(r"C:\Program Files (x86)\Common Files"),
            ),
            (
                OsString::from("HTTPS_PROXY"),
                OsString::from("http://proxy.example:8080"),
            ),
            (
                OsString::from("no_proxy"),
                OsString::from("localhost,127.0.0.1"),
            ),
            (
                OsString::from("WEBVIEW2_USER_DATA_FOLDER"),
                OsString::from(r"C:\Users\me\AppData\Local\Legion"),
            ),
            (
                OsString::from("PSModulePath"),
                OsString::from(r"C:\evil\modules"),
            ),
            (OsString::from("EVIL_VAR"), OsString::from("injected")),
        ];
        let kept = build_sanitized_windows_env(raw);
        assert_eq!(
            kept.get(OsStr::new("PATHEXT")),
            Some(&OsString::from(".COM;.EXE;.BAT;.CMD"))
        );
        assert_eq!(
            kept.get(OsStr::new("ProgramW6432")),
            Some(&OsString::from(r"C:\Program Files"))
        );
        assert_eq!(
            kept.get(OsStr::new("CommonProgramW6432")),
            Some(&OsString::from(r"C:\Program Files\Common Files"))
        );
        assert_eq!(
            kept.get(OsStr::new("CommonProgramFiles(x86)")),
            Some(&OsString::from(r"C:\Program Files (x86)\Common Files"))
        );
        assert_eq!(
            kept.get(OsStr::new("HTTPS_PROXY")),
            Some(&OsString::from("http://proxy.example:8080"))
        );
        assert_eq!(
            kept.get(OsStr::new("no_proxy")),
            Some(&OsString::from("localhost,127.0.0.1"))
        );
        assert!(kept.contains_key(OsStr::new("WEBVIEW2_USER_DATA_FOLDER")));
        assert!(!kept.contains_key(OsStr::new("PSModulePath")));
        assert!(!kept.contains_key(OsStr::new("EVIL_VAR")));
    }

    #[cfg(unix)]
    #[test]
    fn extract_macos_app_tar_hardening_covers_symlink_chains_parents_hardlinks_sparse_and_setuid() {
        use std::os::unix::fs::PermissionsExt;

        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tar::{Builder, Header};

        let dir = tempfile::tempdir().unwrap();

        // 1. Valid bundle with setuid/setgid/sticky bits -> stripped to 0o0755 on extraction.
        let good_tar = dir.path().join("good.app.tar.gz");
        {
            let file = fs::File::create(&good_tar).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(enc);

            let bin_bytes = b"#!/bin/sh\necho ok\n";
            let mut h = Header::new_gnu();
            h.set_size(bin_bytes.len() as u64);
            h.set_mode(0o6755); // setuid + setgid!
            h.set_cksum();
            builder
                .append_data(
                    &mut h,
                    "Legion.app/Contents/MacOS/legion-app",
                    &bin_bytes[..],
                )
                .unwrap();

            let mut link_h = Header::new_gnu();
            link_h.set_entry_type(tar::EntryType::Symlink);
            link_h.set_size(0);
            link_h.set_cksum();
            builder
                .append_link(
                    &mut link_h,
                    "Legion.app/Contents/MacOS/current",
                    "legion-app",
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let dest_good = dir.path().join("out-good");
        let extracted = extract_macos_app_tar(&good_tar, &dest_good).unwrap();
        assert!(extracted.ends_with("Legion.app"));
        let bin_mode = fs::metadata(extracted.join("Contents/MacOS/legion-app"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(bin_mode, 0o0755, "setuid/setgid bits must be stripped");

        // 2. Multi-hop escaping symlink chain (`b -> ..`, then `c -> b/..`, AND reversed order).
        for (label, first, second) in [
            (
                "forward-chain",
                ("Legion.app/b", ".."),
                ("Legion.app/c", "b/.."),
            ),
            (
                "reverse-chain",
                ("Legion.app/c", "b/.."),
                ("Legion.app/b", ".."),
            ),
        ] {
            let tar_path = dir.path().join(format!("{label}.app.tar.gz"));
            {
                let file = fs::File::create(&tar_path).unwrap();
                let enc = GzEncoder::new(file, Compression::default());
                let mut builder = Builder::new(enc);
                for (name, target) in [first, second] {
                    let mut lh = Header::new_gnu();
                    lh.set_entry_type(tar::EntryType::Symlink);
                    lh.set_size(0);
                    lh.set_cksum();
                    builder.append_link(&mut lh, name, target).unwrap();
                }
                builder.into_inner().unwrap().finish().unwrap();
            }
            let err = extract_macos_app_tar(&tar_path, &dir.path().join(format!("out-{label}")))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{label}: {err:?}");
        }

        // 3. Symlink parent write (`a -> .` then `a/evil.txt`) must be refused.
        let sym_parent_tar = dir.path().join("sym-parent.app.tar.gz");
        {
            let file = fs::File::create(&sym_parent_tar).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(enc);
            let mut lh = Header::new_gnu();
            lh.set_entry_type(tar::EntryType::Symlink);
            lh.set_size(0);
            lh.set_cksum();
            builder
                .append_link(&mut lh, "Legion.app/linkdir", "Contents")
                .unwrap();

            let payload = b"escaped";
            let mut fh = Header::new_gnu();
            fh.set_size(payload.len() as u64);
            fh.set_mode(0o644);
            fh.set_cksum();
            builder
                .append_data(&mut fh, "Legion.app/linkdir/pwned.txt", &payload[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let sp_err =
            extract_macos_app_tar(&sym_parent_tar, &dir.path().join("out-sym-parent")).unwrap_err();
        assert_eq!(sp_err.code, ErrorCode::Denied, "{sp_err:?}");

        // 4. Hard links (`EntryType::Link`) must be refused.
        let hardlink_tar = dir.path().join("hardlink.app.tar.gz");
        {
            let file = fs::File::create(&hardlink_tar).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(enc);
            let mut hl = Header::new_gnu();
            hl.set_entry_type(tar::EntryType::Link);
            hl.set_size(0);
            hl.set_cksum();
            builder
                .append_link(&mut hl, "Legion.app/hl", "Legion.app/Contents/MacOS/legion")
                .unwrap();
            builder.finish().unwrap();
        }
        let hl_err = extract_macos_app_tar(&hardlink_tar, &dir.path().join("out-hl")).unwrap_err();
        assert_eq!(hl_err.code, ErrorCode::Denied, "{hl_err:?}");

        // 5. GNUSparse entries must be refused.
        let sparse_tar = dir.path().join("sparse.app.tar.gz");
        {
            let file = fs::File::create(&sparse_tar).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(enc);
            let mut sh = Header::new_gnu();
            sh.set_entry_type(tar::EntryType::GNUSparse);
            sh.set_size(0);
            sh.as_mut_bytes()[483..495].copy_from_slice(b"00000000000\0");
            sh.set_cksum();
            builder
                .append_data(&mut sh, "Legion.app/sparse.bin", &[][..])
                .unwrap();
            builder.finish().unwrap();
        }
        let sp_type_err =
            extract_macos_app_tar(&sparse_tar, &dir.path().join("out-sparse")).unwrap_err();
        assert_eq!(sp_type_err.code, ErrorCode::Denied, "{sp_type_err:?}");

        // 6. Byte limit enforcement on actual extracted bytes.
        let small_limit_err = extract_macos_app_tar_with_limit(
            &good_tar,
            &dir.path().join("out-small-limit"),
            8,
            MAX_EXTRACTED_ENTRIES,
        )
        .unwrap_err();
        assert_eq!(small_limit_err.code, ErrorCode::Denied);

        // 7. Symlinks that escape one level above the `.app` bundle root (pointing to the archive
        //    root or a sibling entry, which would escape into `/Applications` once swapped) must
        //    be refused, while symlinks staying inside the `.app` bundle remain valid.
        for (label, link_name, link_target) in [
            (
                "escape-one-above-contents",
                "Legion.app/Contents/escape",
                "../..",
            ),
            ("escape-one-above-bundle", "Legion.app/escape", ".."),
            (
                "escape-to-sibling",
                "Legion.app/Contents/sibling",
                "../../Other",
            ),
            ("top-level-symlink", "escape", "Legion.app"),
        ] {
            let tar_path = dir.path().join(format!("{label}.app.tar.gz"));
            {
                let file = fs::File::create(&tar_path).unwrap();
                let enc = GzEncoder::new(file, Compression::default());
                let mut builder = Builder::new(enc);
                let mut lh = Header::new_gnu();
                lh.set_entry_type(tar::EntryType::Symlink);
                lh.set_size(0);
                lh.set_cksum();
                builder
                    .append_link(&mut lh, link_name, link_target)
                    .unwrap();
                builder.into_inner().unwrap().finish().unwrap();
            }
            let err = extract_macos_app_tar(&tar_path, &dir.path().join(format!("out-{label}")))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::Denied, "{label}: {err:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_ignores_planted_appdir_tools_and_strips_appdir_env() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let fake_appdir = dir.path().join("mount_legion_fake");
        let fake_bin = fake_appdir.join("usr/bin");
        let fake_lib = fake_appdir.join("usr/lib");
        let fake_share = fake_appdir.join("usr/share");
        fs::create_dir_all(&fake_bin).unwrap();
        fs::create_dir_all(&fake_lib).unwrap();
        fs::create_dir_all(&fake_share).unwrap();

        let pwn_marker = dir.path().join("pwned.txt");
        // Plant trojan mv, rm, ps, sleep, and legion inside fake $APPDIR/usr/bin!
        for tool in ["mv", "rm", "ps", "sleep", "legion"] {
            let tool_path = fake_bin.join(tool);
            fs::write(
                &tool_path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' '{tool}' >> '{}'\nexit 1\n",
                    pwn_marker.display()
                ),
            )
            .unwrap();
            fs::set_permissions(&tool_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let poisoned_env = vec![
            (
                OsString::from("APPDIR"),
                fake_appdir.as_os_str().to_os_string(),
            ),
            (
                OsString::from("APPIMAGE"),
                OsString::from("/home/me/Legion.AppImage"),
            ),
            (
                OsString::from("ARGV0"),
                OsString::from("/home/me/Legion.AppImage"),
            ),
            (OsString::from("OWD"), OsString::from("/home/me")),
            (
                OsString::from("PATH"),
                OsString::from(format!(
                    "{}:/usr/local/bin:/usr/bin:/bin",
                    fake_bin.display()
                )),
            ),
            (
                OsString::from("LD_LIBRARY_PATH"),
                fake_lib.as_os_str().to_os_string(),
            ),
            (OsString::from("LD_PRELOAD"), OsString::from("/tmp/evil.so")),
            (
                OsString::from("XDG_DATA_DIRS"),
                OsString::from(format!(
                    "{}:/usr/local/share:/usr/share",
                    fake_share.display()
                )),
            ),
            (
                OsString::from("GTK_PATH"),
                fake_appdir
                    .join("usr/lib/gtk-3.0")
                    .as_os_str()
                    .to_os_string(),
            ),
            (OsString::from("HOME"), OsString::from("/home/me")),
            (OsString::from("DISPLAY"), OsString::from(":0")),
            (OsString::from("GTK_IM_MODULE"), OsString::from("fcitx")),
            (OsString::from("QT_IM_MODULE"), OsString::from("fcitx")),
            (OsString::from("XMODIFIERS"), OsString::from("@im=fcitx")),
            (
                OsString::from("GTK_IM_MODULE_FILE"),
                fake_appdir
                    .join("usr/lib/gtk-3.0/immodules.cache")
                    .as_os_str()
                    .to_os_string(),
            ),
            (
                OsString::from("HTTPS_PROXY"),
                OsString::from("http://proxy.example:3128"),
            ),
            (
                OsString::from("http_proxy"),
                OsString::from("http://proxy.example:3128"),
            ),
            (OsString::from("NO_PROXY"), OsString::from("localhost")),
            (OsString::from("BASH_ENV"), OsString::from("/tmp/evil.sh")),
        ];

        let sanitized = build_sanitized_unix_env(poisoned_env, Some(&fake_appdir));
        let relaunch_path_str = sanitized.relaunch_path.to_string_lossy();
        assert!(
            !relaunch_path_str.contains(&*fake_appdir.to_string_lossy()),
            "relaunch PATH leaked APPDIR: {relaunch_path_str}"
        );
        assert!(!sanitized.session_vars.contains_key(OsStr::new("APPDIR")));
        assert!(!sanitized.session_vars.contains_key(OsStr::new("APPIMAGE")));
        assert!(
            !sanitized
                .session_vars
                .contains_key(OsStr::new("LD_LIBRARY_PATH"))
        );
        assert!(
            !sanitized
                .session_vars
                .contains_key(OsStr::new("LD_PRELOAD"))
        );
        assert!(!sanitized.session_vars.contains_key(OsStr::new("GTK_PATH")));
        assert!(
            !sanitized
                .session_vars
                .contains_key(OsStr::new("GTK_IM_MODULE_FILE"))
        );
        assert!(!sanitized.session_vars.contains_key(OsStr::new("BASH_ENV")));
        // Harmless input-method and proxy variables must survive sanitization and relaunch.
        assert_eq!(
            sanitized.session_vars.get(OsStr::new("GTK_IM_MODULE")),
            Some(&OsString::from("fcitx"))
        );
        assert_eq!(
            sanitized.session_vars.get(OsStr::new("QT_IM_MODULE")),
            Some(&OsString::from("fcitx"))
        );
        assert_eq!(
            sanitized.session_vars.get(OsStr::new("XMODIFIERS")),
            Some(&OsString::from("@im=fcitx"))
        );
        assert_eq!(
            sanitized.session_vars.get(OsStr::new("HTTPS_PROXY")),
            Some(&OsString::from("http://proxy.example:3128"))
        );
        assert_eq!(
            sanitized.session_vars.get(OsStr::new("http_proxy")),
            Some(&OsString::from("http://proxy.example:3128"))
        );
        assert_eq!(
            sanitized.session_vars.get(OsStr::new("NO_PROXY")),
            Some(&OsString::from("localhost"))
        );
        let xdg = sanitized
            .session_vars
            .get(OsStr::new("XDG_DATA_DIRS"))
            .unwrap()
            .to_string_lossy();
        assert!(!xdg.contains(&*fake_appdir.to_string_lossy()));
        assert_eq!(xdg, "/usr/local/share:/usr/share");

        // Also confirm an APPDIR located outside `/tmp` (e.g. under `$HOME/.local`) is stripped
        // from PATH and session variables just like `/tmp/.mount_*`.
        let non_tmp_appdir = PathBuf::from("/home/me/.local/fake_appdir");
        let non_tmp_env = vec![
            (
                OsString::from("APPDIR"),
                non_tmp_appdir.as_os_str().to_os_string(),
            ),
            (
                OsString::from("PATH"),
                OsString::from("/home/me/.local/fake_appdir/usr/bin:/usr/bin:/bin"),
            ),
            (
                OsString::from("XDG_DATA_DIRS"),
                OsString::from("/home/me/.local/fake_appdir/usr/share:/usr/share"),
            ),
            (OsString::from("GTK_IM_MODULE"), OsString::from("ibus")),
        ];
        let non_tmp_sanitized = build_sanitized_unix_env(non_tmp_env, None);
        assert!(
            !non_tmp_sanitized
                .relaunch_path
                .to_string_lossy()
                .contains("/home/me/.local/fake_appdir"),
            "non-/tmp APPDIR leaked into relaunch PATH: {:?}",
            non_tmp_sanitized.relaunch_path
        );
        assert!(
            !non_tmp_sanitized
                .session_vars
                .contains_key(OsStr::new("APPDIR"))
        );
        assert_eq!(
            non_tmp_sanitized
                .session_vars
                .get(OsStr::new("XDG_DATA_DIRS")),
            Some(&OsString::from("/usr/share"))
        );
        assert_eq!(
            non_tmp_sanitized
                .session_vars
                .get(OsStr::new("GTK_IM_MODULE")),
            Some(&OsString::from("ibus"))
        );

        // Now run the real swap supervisor with the sanitized env and capture the relaunched
        // process's complete environment to prove zero planted tools ran and zero APPDIR/LD vars
        // survived into the relaunched binary.
        let target = dir.path().join("Legion.AppImage");
        let staged = dir.path().join(".Legion.AppImage.new-1");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-2.ok");
        let rollback = dir.path().join("rollback-2.json");
        let env_dump = dir.path().join("relaunched-env.txt");

        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            &staged,
            format!(
                "#!/bin/sh\nenv > '{}'\nprintf 'ok\\n' > '{}'\n",
                env_dump.display(),
                ready.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor_with_env(
            &UnixSwapPlan {
                old_pid: exited_pid,
                old_ppid: None,
                target_path: target.clone(),
                staged_new_path: staged,
                backup_path: backup.clone(),
                exec_path: target,
                ready_marker_path: ready.clone(),
                rollback_marker_path: rollback,
                rollback_json: "{}".to_owned(),
                rollback_kept_new_json: "{}".to_owned(),
                deferred_marker_path: None,
                deferred_json: None,
                target_sequence: 2,
                update_lock_path: None,
                reopen_marker_path: None,
                log_path: None,
                new_start_log_path: None,
                rollback_ps_path: None,
                single_instance_dbus_name: None,
                single_instance_socket_path: None,
                pre_swap_wait_secs: 5,
                wait_secs: 5,
                exit_grace_secs: 2,
                rollback_retries: 4,
                verify_only: false,
            },
            &sanitized,
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if ready.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            ready.exists(),
            "expected relaunched binary to write ready marker"
        );
        assert!(!backup.exists(), "expected .prev backup to be deleted");
        assert!(
            !pwn_marker.exists(),
            "planted APPDIR tools must NEVER be executed: {:?}",
            fs::read_to_string(&pwn_marker)
        );
        let dumped = fs::read_to_string(&env_dump).unwrap();
        assert!(
            !dumped.contains(&*fake_appdir.to_string_lossy()),
            "relaunched env must not contain APPDIR paths:\n{dumped}"
        );
        for forbidden in [
            "APPDIR=",
            "APPIMAGE=",
            "LD_LIBRARY_PATH=",
            "LD_PRELOAD=",
            "GTK_IM_MODULE_FILE=",
            "BASH_ENV=",
            "LEGION_RELAUNCH_PATH=",
            "LEGION_TARGET=",
        ] {
            assert!(
                !dumped.contains(forbidden),
                "relaunched env leaked {forbidden}:\n{dumped}"
            );
        }
        for preserved in [
            "GTK_IM_MODULE=fcitx",
            "QT_IM_MODULE=fcitx",
            "XMODIFIERS=@im=fcitx",
            "HTTPS_PROXY=http://proxy.example:3128",
            "http_proxy=http://proxy.example:3128",
            "NO_PROXY=localhost",
        ] {
            assert!(
                dumped.contains(preserved),
                "relaunched env lost {preserved}:\n{dumped}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_close_then_reopen_race_does_not_roll_back() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged = dir.path().join(".Legion.AppImage.new-2");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-2.ok");
        let rollback = dir.path().join("rollback-2.json");

        fs::write(&target, "#!/bin/sh\necho v1\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        // Simulate a close-then-reopen race where the verify-only launch exits 0 immediately
        // (for instance after handing off to a concurrently reopened instance of the new build)
        // while the reopened instance finishes painting and writes `started-2.ok` 600ms later.
        let v2_handover = format!(
            "#!/bin/sh\n(sleep 0.6; printf 'ok\\n' > '{}') &\nexit 0\n",
            ready.display()
        );
        fs::write(&staged, v2_handover).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready.clone(),
            rollback_marker_path: rollback.clone(),
            rollback_json: "{\"sequence\":2}".to_owned(),
            rollback_kept_new_json: "{\"sequence\":2}".to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 2,
            update_lock_path: None,
            reopen_marker_path: None,
            log_path: None,
            new_start_log_path: None,
            rollback_ps_path: None,
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 5,
            wait_secs: 5,
            exit_grace_secs: 2,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if ready.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready.exists(), "expected ready marker to be written");
        assert!(
            !rollback.exists(),
            "close-then-reopen race must NOT write a rollback file"
        );
        assert!(
            !backup.exists(),
            "supervisor should clean up .prev backup once ready marker arrives"
        );
        assert!(
            fs::read_to_string(&target).unwrap().contains("sleep 0.6"),
            "new v2 binary should stay installed rather than rolling back to v1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_replaces_and_rolls_back_and_supports_verify_only_on_close() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged_ok = dir.path().join(".Legion.AppImage.new-1");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready_ok = dir.path().join("started-2.ok");
        let rollback_ok = dir.path().join("rollback-2.json");
        let ran_marker = dir.path().join("ran.txt");

        let v1_script = format!("#!/bin/sh\nprintf 'v1\\n' >> '{}'\n", ran_marker.display());
        fs::write(&target, v1_script).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        // Good v2 in verify_only mode: verifies LEGION_UPDATE_VERIFY_ONLY=1, writes ready marker,
        // and exits 0 immediately without staying open.
        let v2_script = format!(
            "#!/bin/sh\nprintf 'v2:%s\\n' \"${{LEGION_UPDATE_VERIFY_ONLY:-0}}\" >> '{}'\nprintf 'ok\\n' > '{}'\nexit 0\n",
            ran_marker.display(),
            ready_ok.display()
        );
        fs::write(&staged_ok, v2_script).unwrap();
        fs::set_permissions(&staged_ok, fs::Permissions::from_mode(0o755)).unwrap();

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged_ok,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_ok.clone(),
            rollback_marker_path: rollback_ok.clone(),
            rollback_json: "{\"sequence\":2}".to_owned(),
            rollback_kept_new_json: "{\"sequence\":2}".to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 2,
            update_lock_path: None,
            reopen_marker_path: None,
            log_path: None,
            new_start_log_path: None,
            rollback_ps_path: None,
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 5,
            wait_secs: 5,
            exit_grace_secs: 2,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if ready_ok.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready_ok.exists(), "expected v2 verify_only ready marker");
        assert!(!backup.exists(), "expected .prev backup removed");
        assert!(!rollback_ok.exists());
        assert!(fs::read_to_string(&ran_marker).unwrap().contains("v2:1"));

        // Now test interactive ROLLBACK: crashing v3 exits 1 without writing ready_bad -> v2 restored and relaunched.
        let staged_bad = dir.path().join(".Legion.AppImage.new-3");
        let ready_bad = dir.path().join("started-3.ok");
        let rollback_bad = dir.path().join("rollback-3.json");
        fs::write(&staged_bad, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&staged_bad, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(&ready_ok);

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged_bad,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_bad,
            rollback_marker_path: rollback_bad.clone(),
            rollback_json: "{\"version\":\"0.3.0\",\"sequence\":3,\"reason\":\"crashed\"}"
                .to_owned(),
            rollback_kept_new_json:
                "{\"version\":\"0.3.0\",\"sequence\":3,\"reason\":\"crashed\",\"install_state\":\"new_kept_files_in_use\"}"
                    .to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 3,
            update_lock_path: None,
            reopen_marker_path: None,
            log_path: None,
            new_start_log_path: None,
            rollback_ps_path: None,
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 5,
            wait_secs: 3,
            exit_grace_secs: 1,
            rollback_retries: 4,
            verify_only: false,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if rollback_bad.exists() && ready_ok.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(rollback_bad.exists());
        assert!(ready_ok.exists());
        assert!(fs::read_to_string(&target).unwrap().contains("v2:"));
        assert!(!backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_waits_grace_period_on_nonzero_exit_and_captures_capped_log_and_ps() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged_handover = dir.path().join(".Legion.AppImage.new-handover");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-10.ok");
        let rollback = dir.path().join("rollback-10.json");
        let sup_log = dir.path().join("supervisor.log");
        let new_log = dir.path().join(NEW_START_LOG_FILENAME);
        let rollback_ps = dir.path().join(ROLLBACK_PS_FILENAME);

        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        // 1. Even if the launched PID exits non-zero (rc=42) immediately, the supervisor waits the
        //    grace period; a handover child writing the ready marker 0.8s later succeeds cleanly
        //    without rolling back and cleans up new-start.log.
        fs::write(
            &staged_handover,
            format!(
                "#!/bin/sh\necho 'handover starting' >&2\n(sleep 0.8; printf 'ok\\n' > '{}') &\nexit 42\n",
                ready.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&staged_handover, fs::Permissions::from_mode(0o755)).unwrap();

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged_handover,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready.clone(),
            rollback_marker_path: rollback.clone(),
            rollback_json: "{\"sequence\":10}".to_owned(),
            rollback_kept_new_json: "{\"sequence\":10}".to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 10,
            update_lock_path: None,
            reopen_marker_path: None,
            log_path: Some(sup_log.clone()),
            new_start_log_path: Some(new_log.clone()),
            rollback_ps_path: Some(rollback_ps.clone()),
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 5,
            wait_secs: 6,
            exit_grace_secs: 3,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if ready.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            ready.exists(),
            "handover during non-zero exit grace period must succeed"
        );
        assert!(
            !rollback.exists(),
            "must not roll back when marker appears during grace period"
        );
        assert!(
            !new_log.exists(),
            "new-start.log should be cleaned up on success"
        );
        assert!(
            !rollback_ps.exists(),
            "rollback-ps.txt should not exist on success"
        );

        // 2. When a build truly crashes and prints > 64 KiB before exiting without writing the
        //    ready marker, the supervisor waits the grace period, captures rollback-ps.txt, caps
        //    new-start.log to <= 32 KiB, and read_captured_startup_detail surfaces the failure.
        let staged_crash = dir.path().join(".Legion.AppImage.new-crash");
        let ready_crash = dir.path().join("started-11.ok");
        let rollback_crash = dir.path().join("rollback-11.json");
        fs::write(
            &staged_crash,
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 800 ]; do\n  printf 'noisy startup line %04d 0123456789abcdefghijklmnopqrstuvwxyz0123456789\\n' \"$i\"\n  i=$((i + 1))\ndone\nprintf 'fatal: missing shared library libbroken.so.1\\n' >&2\nexit 127\n",
        )
        .unwrap();
        fs::set_permissions(&staged_crash, fs::Permissions::from_mode(0o755)).unwrap();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged_crash,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_crash,
            rollback_marker_path: rollback_crash.clone(),
            rollback_json: "{\"version\":\"0.4.0\",\"sequence\":11,\"reason\":\"crashed\"}"
                .to_owned(),
            rollback_kept_new_json:
                "{\"version\":\"0.4.0\",\"sequence\":11,\"reason\":\"crashed\",\"install_state\":\"new_kept_files_in_use\"}"
                    .to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 11,
            update_lock_path: None,
            reopen_marker_path: None,
            log_path: Some(sup_log.clone()),
            new_start_log_path: Some(new_log.clone()),
            rollback_ps_path: Some(rollback_ps.clone()),
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 5,
            wait_secs: 6,
            exit_grace_secs: 1,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if rollback_crash.exists() && rollback_ps.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            rollback_crash.exists(),
            "expected rollback marker after crash"
        );
        assert!(
            rollback_ps.exists(),
            "expected rollback-ps.txt captured at rollback"
        );
        assert!(
            !fs::read_to_string(&rollback_ps).unwrap().trim().is_empty(),
            "rollback-ps.txt must contain process snapshot"
        );
        let new_log_meta = fs::metadata(&new_log).unwrap();
        assert!(
            new_log_meta.len() > 0 && new_log_meta.len() <= MAX_CAPTURED_START_LOG_BYTES as u64,
            "new-start.log must be non-empty and capped to <= 32 KiB, got {} bytes",
            new_log_meta.len()
        );
        let detail = read_captured_startup_detail(dir.path()).unwrap();
        assert!(
            detail.contains("rc=127") && detail.contains("libbroken.so.1"),
            "captured startup detail should surface exit status and stderr tail: {detail}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_waits_for_old_children_and_wrapper_parent_before_swap() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged = dir.path().join(".Legion.AppImage.new-wait");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-12.ok");
        let rollback = dir.path().join("rollback-12.json");
        let child_gone_marker = dir.path().join("old-child-gone.txt");
        let new_started_check = dir.path().join("new-saw-child-gone.txt");

        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        // New binary asserts that the old child already finished before the new copy launched!
        fs::write(
            &staged,
            format!(
                "#!/bin/sh\nif [ -f '{}' ]; then printf 'clean\\n' > '{}'; fi\nprintf 'ok\\n' > '{}'\n",
                child_gone_marker.display(),
                new_started_check.display(),
                ready.display(),
            ),
        )
        .unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();

        // Simulate old process + lingering helper child (e.g., WebKitNetworkProcess) that exits
        // 0.8s after old_pid exits, plus a lingering AppImage wrapper parent (`old_ppid`).
        let mut old_proc = Command::new("/bin/sh")
            .args([
                "-c",
                &format!(
                    "(sleep 0.9; printf 'done\\n' > '{}') &\nsleep 0.25\nexit 0\n",
                    child_gone_marker.display()
                ),
            ])
            .spawn()
            .unwrap();
        let old_pid = old_proc.id();
        // Give the shell a moment to spawn its background child before spawning the supervisor so
        // `collect_children "$LEGION_OLD_PID"` sees it while old_pid is still running.
        std::thread::sleep(Duration::from_millis(80));

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready.clone(),
            rollback_marker_path: rollback.clone(),
            rollback_json: "{\"sequence\":12}".to_owned(),
            rollback_kept_new_json: "{\"sequence\":12}".to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 12,
            update_lock_path: None,
            reopen_marker_path: None,
            log_path: Some(dir.path().join("supervisor.log")),
            new_start_log_path: Some(dir.path().join(NEW_START_LOG_FILENAME)),
            rollback_ps_path: Some(dir.path().join(ROLLBACK_PS_FILENAME)),
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 6,
            wait_secs: 6,
            exit_grace_secs: 2,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        let _ = old_proc.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if ready.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ready.exists(), "expected new build ready marker");
        assert!(!rollback.exists(), "expected no rollback");
        assert!(
            new_started_check.exists(),
            "supervisor must wait until old helper children exit before launching new build"
        );
    }

    #[test]
    fn update_lock_prevents_concurrent_staging_and_recovers_stale_lock() {
        let dir = tempfile::tempdir().unwrap();
        let updates_dir = dir.path().join("updates");

        // 1. Acquiring the lock excludes a second acquirer and exposes active metadata.
        let guard = try_acquire_update_lock(&updates_dir, 202611250210, true)
            .unwrap()
            .expect("first lock acquisition must succeed");
        assert!(
            try_acquire_update_lock(&updates_dir, 202611250210, false)
                .unwrap()
                .is_none(),
            "second instance must not acquire the update lock while staging is active"
        );
        let active = inspect_active_update_lock(&updates_dir).expect("active lock must be visible");
        assert_eq!(active.target_sequence, 202611250210);
        assert!(active.verify_only);
        assert_eq!(active.stage, "staging");

        // 2. Dropping without hand-over releases both locks cleanly.
        drop(guard);
        assert!(
            inspect_active_update_lock(&updates_dir).is_none(),
            "dropping lock guard without hand-over must remove update.lock"
        );

        // 3. Hand-over to a live supervisor PID keeps update.lock active until removed or stale.
        let guard2 = try_acquire_update_lock(&updates_dir, 202611250211, false)
            .unwrap()
            .expect("re-acquiring after clean drop must succeed");
        guard2.hand_over_to_supervisor(std::process::id()).unwrap();
        let handed =
            inspect_active_update_lock(&updates_dir).expect("handed-over lock must stay active");
        assert_eq!(handed.stage, "supervising");
        assert_eq!(handed.target_sequence, 202611250211);
        assert!(
            try_acquire_update_lock(&updates_dir, 202611250211, false)
                .unwrap()
                .is_none(),
            "second instance must not stage while supervisor holds update.lock"
        );

        // 4. A stale lock past UPDATE_LOCK_MAX_AGE_SECS is cleaned automatically so a crashed
        //    supervisor can never wedge future updates.
        let stale = UpdateLockState {
            owner_pid: 999_999,
            supervisor_pid: Some(999_998),
            target_sequence: 202611250211,
            stage: "supervising".to_owned(),
            verify_only: false,
            started_unix: now_unix_u64().saturating_sub(UPDATE_LOCK_MAX_AGE_SECS + 10),
        };
        write_update_lock_file(&updates_dir.join(UPDATE_LOCK_FILENAME), &stale).unwrap();
        assert!(
            inspect_active_update_lock(&updates_dir).is_none(),
            "expired stale lock must be cleaned automatically"
        );
        let recovered = try_acquire_update_lock(&updates_dir, 202611250212, false)
            .unwrap()
            .expect("must acquire lock after stale lock cleanup");
        drop(recovered);
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_defers_cleanly_when_another_instance_holds_install() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged = dir.path().join(".Legion.AppImage.new-defer");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-13.ok");
        let rollback = dir.path().join("rollback-13.json");
        let deferred = dir.path().join("deferred-13.json");
        let lock_file = dir.path().join(UPDATE_LOCK_FILENAME);

        fs::write(&target, "#!/bin/sh\nsleep 30\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&staged, "#!/bin/sh\necho new-v2\n").unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&lock_file, "{}").unwrap();

        // Spawn a second running process executing $LEGION_TARGET so the install is actively in use.
        let mut busy_child = Command::new(&target).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(120));

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged.clone(),
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready.clone(),
            rollback_marker_path: rollback.clone(),
            rollback_json: "{\"sequence\":13}".to_owned(),
            rollback_kept_new_json: "{\"sequence\":13}".to_owned(),
            deferred_marker_path: Some(deferred.clone()),
            deferred_json: Some("{\"sequence\":13,\"reason\":\"busy\"}".to_owned()),
            target_sequence: 13,
            update_lock_path: Some(lock_file.clone()),
            reopen_marker_path: None,
            log_path: Some(dir.path().join("supervisor.log")),
            new_start_log_path: None,
            rollback_ps_path: None,
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 1,
            wait_secs: 4,
            exit_grace_secs: 1,
            rollback_retries: 4,
            verify_only: false,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if deferred.exists() && !lock_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = busy_child.kill();
        let _ = busy_child.wait();

        assert!(
            deferred.exists(),
            "supervisor must write deferred marker when another instance holds the install"
        );
        assert!(
            !rollback.exists(),
            "deferring pre-swap must NOT write a rollback failure marker"
        );
        assert!(!ready.exists(), "deferred update must not launch new copy");
        assert!(
            !staged.exists(),
            "staged new file must be cleaned up on defer"
        );
        assert!(
            !backup.exists(),
            "target must never be moved aside on defer"
        );
        assert!(!lock_file.exists(), "update.lock must be released on defer");
        assert!(
            fs::read_to_string(&target).unwrap().contains("sleep 30"),
            "original install must remain untouched when deferred"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_rollback_keeps_new_intact_when_other_process_holds_target_open() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged = dir.path().join(".Legion.AppImage.new-busy-rb");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-14.ok");
        let rollback = dir.path().join("rollback-14.json");
        let lock_file = dir.path().join(UPDATE_LOCK_FILENAME);
        let launched_flag = dir.path().join("new-launched.txt");

        fs::write(&target, "#!/bin/sh\necho old-v1\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        // New v2 script marks that it started, then exits 1 without writing ready marker.
        // While it fails, this test spawns an independent process holding $LEGION_TARGET open so
        // rollback retries hit busy and MUST leave the new copy intact (never half-replaced) with
        // install_state == "new_kept_files_in_use".
        let v2_body = format!(
            "#!/bin/sh\nif [ \"${{HOLD_OPEN:-0}}\" = \"1\" ]; then\n  sleep 30\n  exit 0\nfi\nprintf 'started\\n' > '{}'\nexit 1\n",
            launched_flag.display()
        );
        fs::write(&staged, &v2_body).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&lock_file, "{}").unwrap();

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready,
            rollback_marker_path: rollback.clone(),
            rollback_json:
                "{\"version\":\"0.4.0\",\"sequence\":14,\"reason\":\"failed\",\"install_state\":\"restored_previous\"}"
                    .to_owned(),
            rollback_kept_new_json:
                "{\"version\":\"0.4.0\",\"sequence\":14,\"reason\":\"failed\",\"install_state\":\"new_kept_files_in_use\"}"
                    .to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 14,
            update_lock_path: Some(lock_file.clone()),
            reopen_marker_path: None,
            log_path: Some(dir.path().join("supervisor.log")),
            new_start_log_path: Some(dir.path().join(NEW_START_LOG_FILENAME)),
            rollback_ps_path: Some(dir.path().join(ROLLBACK_PS_FILENAME)),
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 4,
            wait_secs: 4,
            exit_grace_secs: 1,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        // As soon as the swap completes and v2 launches, open a second process executing $target.
        let swap_deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < swap_deadline && !launched_flag.exists() {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(launched_flag.exists(), "expected v2 to launch after swap");
        let mut holder = Command::new(&target).env("HOLD_OPEN", "1").spawn().unwrap();

        let rb_deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < rb_deadline {
            if rollback.exists() && !lock_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = holder.kill();
        let _ = holder.wait();

        assert!(rollback.exists(), "expected rollback notice to be written");
        assert!(!lock_file.exists(), "update.lock must be released");
        let rb_text = fs::read_to_string(&rollback).unwrap();
        assert!(
            rb_text.contains("new_kept_files_in_use"),
            "rollback notice must record new_kept_files_in_use: {rb_text}"
        );
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            v2_body,
            "new copy must stay completely intact when target is held open during rollback"
        );
        assert!(
            backup.exists(),
            "previous backup must stay intact when rollback kept the new copy"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_reopen_requested_launches_window_after_verify_only() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged = dir.path().join(".Legion.AppImage.new-reopen");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready = dir.path().join("started-15.ok");
        let rollback = dir.path().join("rollback-15.json");
        let lock_file = dir.path().join(UPDATE_LOCK_FILENAME);
        let reopen_file = dir.path().join(REOPEN_REQUESTED_FILENAME);
        let modes_log = dir.path().join("launch-modes.txt");

        fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            &staged,
            format!(
                "#!/bin/sh\nprintf 'verify=%s supervised=%s\\n' \"${{LEGION_UPDATE_VERIFY_ONLY:-0}}\" \"${{LEGION_UPDATE_SUPERVISED_LAUNCH:-none}}\" >> '{}'\nprintf 'ok\\n' > '{}'\nexit 0\n",
                modes_log.display(),
                ready.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&lock_file, "{}").unwrap();
        // Simulate a user reopening Legion while the headless close-update supervisor is running.
        request_reopen_after_update(dir.path());
        assert!(reopen_file.exists());

        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            old_ppid: None,
            target_path: target.clone(),
            staged_new_path: staged,
            backup_path: backup.clone(),
            exec_path: target,
            ready_marker_path: ready.clone(),
            rollback_marker_path: rollback,
            rollback_json: "{\"sequence\":15}".to_owned(),
            rollback_kept_new_json: "{\"sequence\":15}".to_owned(),
            deferred_marker_path: None,
            deferred_json: None,
            target_sequence: 15,
            update_lock_path: Some(lock_file.clone()),
            reopen_marker_path: Some(reopen_file.clone()),
            log_path: Some(dir.path().join("supervisor.log")),
            new_start_log_path: None,
            rollback_ps_path: None,
            single_instance_dbus_name: None,
            single_instance_socket_path: None,
            pre_swap_wait_secs: 4,
            wait_secs: 5,
            exit_grace_secs: 1,
            rollback_retries: 4,
            verify_only: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Ok(text) = fs::read_to_string(&modes_log)
                && text.lines().count() >= 2
                && !lock_file.exists()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let text = fs::read_to_string(&modes_log).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec!["verify=1 supervised=15", "verify=0 supervised=none"],
            "supervisor must run verify_only first with supervised env, then open interactive window when reopen-requested was touched"
        );
        assert!(!reopen_file.exists(), "reopen-requested must be consumed");
        assert!(!lock_file.exists(), "update.lock must be released");
    }

    #[cfg(windows)]
    #[test]
    fn windows_supervisor_rollback_retries_and_never_half_copies_when_exe_locked() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let install_dir = dir.path().join("install");
        let backup_dir = dir.path().join("backup-1");
        let updates_dir = dir.path().join("updates");
        fs::create_dir_all(&install_dir).unwrap();
        fs::create_dir_all(&backup_dir).unwrap();
        fs::create_dir_all(&updates_dir).unwrap();

        let exe_path = install_dir.join("legion-app.exe");
        let dll_path = install_dir.join("helper.dll");
        fs::write(&exe_path, b"new-v2-exe").unwrap();
        fs::write(&dll_path, b"new-v2-dll").unwrap();
        fs::write(backup_dir.join("legion-app.exe"), b"old-v1-exe").unwrap();
        fs::write(backup_dir.join("helper.dll"), b"old-v1-dll").unwrap();

        // Use System32 where.exe as a failing stand-in for NSIS (`/S /UPDATE /D=...` exits non-zero
        // cleanly without reading stdin), triggering Restore-Previous directly.
        let sys_root = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("WINDIR"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let fake_installer = updates_dir.join("failing-installer.exe");
        fs::copy(sys_root.join("System32").join("where.exe"), &fake_installer).unwrap();

        let mut exited = Command::new(sys_root.join("System32").join("cmd.exe"))
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();
        let dead_pid = exited.id();
        let _ = exited.wait();

        let ready_marker = updates_dir.join("started-2.ok");
        let rollback_marker = updates_dir.join("rollback-2.json");
        let deferred_marker = updates_dir.join("deferred-2.json");
        let update_lock = updates_dir.join(UPDATE_LOCK_FILENAME);
        let sup_lock = updates_dir.join(UPDATE_SUPERVISOR_LOCK_FILENAME);
        let ps_path = windows_system_powershell_path();

        // Part 1: Hold `install\legion-app.exe` open with exclusive share_mode(0). The supervisor
        // MUST retry boundedly, NOT throw an unhandled exception, leave BOTH `legion-app.exe` and
        // `helper.dll` completely intact as v2 (ZERO half-copy), write `new_kept_files_in_use`,
        // and release its locks and script file.
        let script1 = write_windows_supervisor_script(&updates_dir, 1001).unwrap();
        fs::write(&update_lock, "{}").unwrap();
        let held_exe = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&exe_path)
            .unwrap();

        let out1 = Command::new(&ps_path)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script1)
            .env("LEGION_SUPERVISOR_SCRIPT", &script1)
            .env("LEGION_OLD_PID", dead_pid.to_string())
            .env("LEGION_INSTALLER", &fake_installer)
            .env("LEGION_INSTALL_DIR", &install_dir)
            .env("LEGION_EXE", &exe_path)
            .env("LEGION_BACKUP_DIR", &backup_dir)
            .env("LEGION_READY_MARKER", &ready_marker)
            .env("LEGION_ROLLBACK_FILE", &rollback_marker)
            .env(
                "LEGION_ROLLBACK_JSON",
                r#"{"version":"0.2.0","sequence":2,"reason":"failed","install_state":"restored_previous"}"#,
            )
            .env(
                "LEGION_ROLLBACK_KEPT_NEW_JSON",
                r#"{"version":"0.2.0","sequence":2,"reason":"failed","install_state":"new_kept_files_in_use"}"#,
            )
            .env("LEGION_DEFERRED_FILE", &deferred_marker)
            .env("LEGION_DEFERRED_JSON", r#"{"sequence":2}"#)
            .env("LEGION_TARGET_SEQUENCE", "2")
            .env("LEGION_PRE_SWAP_WAIT_SECS", "2")
            .env("LEGION_WAIT_SECS", "4")
            .env("LEGION_EXIT_GRACE_SECS", "1")
            .env("LEGION_ROLLBACK_RETRIES", "4")
            .env("LEGION_VERIFY_ONLY", "1")
            .env("LEGION_UPDATE_LOCK", &update_lock)
            .env("LEGION_SUPERVISOR_LOCK", &sup_lock)
            .output()
            .unwrap();

        drop(held_exe);

        let log1 = String::from_utf8_lossy(&out1.stdout);
        assert!(
            !log1.contains("supervisor exception:"),
            "rollback must not throw an unhandled exception when exe is locked:\n{log1}\nstderr:\n{}",
            String::from_utf8_lossy(&out1.stderr)
        );
        assert_eq!(
            fs::read(&exe_path).unwrap(),
            b"new-v2-exe",
            "locked exe must stay intact at v2"
        );
        assert_eq!(
            fs::read(&dll_path).unwrap(),
            b"new-v2-dll",
            "sibling helper.dll must NOT be overwritten when exe is locked (zero half-copy!)"
        );
        let rb1_text = fs::read_to_string(&rollback_marker).unwrap();
        assert!(
            rb1_text.contains("new_kept_files_in_use"),
            "rollback notice must record new_kept_files_in_use: {rb1_text}"
        );
        assert!(!update_lock.exists(), "update.lock must be released");
        assert!(
            !sup_lock.exists(),
            "update-supervisor.lock must be released"
        );
        assert!(
            !script1.exists(),
            "supervisor .ps1 script must be cleaned up"
        );

        // Part 2: With the file lock released, rollback cleanly restores ALL files to v1 and
        // records `restored_previous`.
        fs::copy(sys_root.join("System32").join("where.exe"), &fake_installer).unwrap();
        let _ = fs::remove_file(&rollback_marker);
        let script2 = write_windows_supervisor_script(&updates_dir, 1002).unwrap();
        fs::write(&update_lock, "{}").unwrap();

        let out2 = Command::new(&ps_path)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script2)
            .env("LEGION_SUPERVISOR_SCRIPT", &script2)
            .env("LEGION_OLD_PID", dead_pid.to_string())
            .env("LEGION_INSTALLER", &fake_installer)
            .env("LEGION_INSTALL_DIR", &install_dir)
            .env("LEGION_EXE", &exe_path)
            .env("LEGION_BACKUP_DIR", &backup_dir)
            .env("LEGION_READY_MARKER", &ready_marker)
            .env("LEGION_ROLLBACK_FILE", &rollback_marker)
            .env(
                "LEGION_ROLLBACK_JSON",
                r#"{"version":"0.2.0","sequence":2,"reason":"failed","install_state":"restored_previous"}"#,
            )
            .env(
                "LEGION_ROLLBACK_KEPT_NEW_JSON",
                r#"{"version":"0.2.0","sequence":2,"reason":"failed","install_state":"new_kept_files_in_use"}"#,
            )
            .env("LEGION_DEFERRED_FILE", &deferred_marker)
            .env("LEGION_DEFERRED_JSON", r#"{"sequence":2}"#)
            .env("LEGION_TARGET_SEQUENCE", "2")
            .env("LEGION_PRE_SWAP_WAIT_SECS", "2")
            .env("LEGION_WAIT_SECS", "4")
            .env("LEGION_EXIT_GRACE_SECS", "1")
            .env("LEGION_ROLLBACK_RETRIES", "4")
            .env("LEGION_VERIFY_ONLY", "1")
            .env("LEGION_UPDATE_LOCK", &update_lock)
            .env("LEGION_SUPERVISOR_LOCK", &sup_lock)
            .output()
            .unwrap();

        let log2 = String::from_utf8_lossy(&out2.stdout);
        assert!(
            !log2.contains("supervisor exception:"),
            "clean rollback must not throw:\n{log2}\nstderr:\n{}",
            String::from_utf8_lossy(&out2.stderr)
        );
        assert_eq!(
            fs::read(&exe_path).unwrap(),
            b"old-v1-exe",
            "unlocked exe must be restored to v1"
        );
        assert_eq!(
            fs::read(&dll_path).unwrap(),
            b"old-v1-dll",
            "sibling helper.dll must be restored to v1"
        );
        let rb2_text = fs::read_to_string(&rollback_marker).unwrap();
        assert!(
            rb2_text.contains("restored_previous"),
            "rollback notice must record restored_previous: {rb2_text}"
        );
        assert!(!update_lock.exists(), "update.lock must be released");
        assert!(
            !sup_lock.exists(),
            "update-supervisor.lock must be released"
        );
        assert!(
            !script2.exists(),
            "supervisor .ps1 script must be cleaned up"
        );
    }
}
