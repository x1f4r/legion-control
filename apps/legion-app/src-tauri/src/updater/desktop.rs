//! Desktop install-kind detection, safe bundle unpacking, and detached swap-and-rollback
//! supervisors for Linux AppImage, Linux `.deb`, macOS `.app`, and Windows NSIS.
//!
//! Every detached helper script is a fixed constant receiving paths strictly through environment
//! variables (zero shell interpolation). Desktop installs always keep the previous copy until the
//! new build starts once and writes its startup marker, restoring the previous copy automatically
//! if the new build crashes or times out.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use legion_update::InstallKind;

use crate::backend::{AppError, ErrorCode};

/// Maximum uncompressed bytes allowed when extracting a macOS `app-tar` archive (512 MiB).
pub const MAX_EXTRACTED_BUNDLE_BYTES: u64 = 512 * 1024 * 1024;

/// Default seconds the detached desktop watchdog waits for the new build to confirm startup.
pub const DEFAULT_STARTUP_WAIT_SECS: u64 = 30;

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
    let out = Command::new("dpkg-query")
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
    let out = Command::new("pacman")
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
/// traversal, absolute paths, escaping symlinks, device nodes, and oversized archives. Returns the
/// single top-level `.app` bundle directory inside `dest_dir`.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn extract_macos_app_tar(tar_gz_path: &Path, dest_dir: &Path) -> Result<PathBuf, AppError> {
    use flate2::read::GzDecoder;
    use tar::{Archive, EntryType};

    fs::create_dir_all(dest_dir).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!(
                "Could not create macOS staging folder {}: {err}",
                dest_dir.display()
            ),
        )
    })?;
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
    archive.set_preserve_permissions(true);
    archive.set_overwrite(false);

    let mut total_bytes: u64 = 0;
    let entries = archive.entries().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not read update archive entries: {err}"),
        )
    })?;

    for entry_res in entries {
        let mut entry = entry_res.map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Corrupt entry in update archive: {err}"),
            )
        })?;
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

        let entry_type = entry.header().entry_type();
        let size = entry.header().size().map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!("Could not read archive entry size: {err}"),
            )
        })?;
        total_bytes = total_bytes.saturating_add(size);
        if total_bytes > MAX_EXTRACTED_BUNDLE_BYTES {
            return Err(AppError::new(
                ErrorCode::Failed,
                format!(
                    "Extracted macOS bundle exceeds the {MAX_EXTRACTED_BUNDLE_BYTES}-byte safety limit."
                ),
            ));
        }

        let target = canonical_root.join(&clean_rel);
        match entry_type {
            EntryType::Directory => {
                fs::create_dir_all(&target).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not create directory {}: {err}", clean_rel.display()),
                    )
                })?;
            }
            EntryType::Regular | EntryType::GNUSparse => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!("Could not create parent for {}: {err}", clean_rel.display()),
                        )
                    })?;
                }
                let mut out = fs::File::create(&target).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not extract {}: {err}", clean_rel.display()),
                    )
                })?;
                io::copy(&mut entry, &mut out).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not write extracted {}: {err}", clean_rel.display()),
                    )
                })?;
                #[cfg(unix)]
                if let Ok(mode) = entry.header().mode() {
                    use std::os::unix::fs::PermissionsExt;
                    let safe_mode = mode & 0o777;
                    let _ = fs::set_permissions(&target, fs::Permissions::from_mode(safe_mode));
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
                validate_tar_symlink(&clean_rel, &link_target)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).map_err(|err| {
                        AppError::new(
                            ErrorCode::Failed,
                            format!(
                                "Could not create parent for symlink {}: {err}",
                                clean_rel.display()
                            ),
                        )
                    })?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(&link_target, &target).map_err(|err| {
                    AppError::new(
                        ErrorCode::Failed,
                        format!("Could not create symlink {}: {err}", clean_rel.display()),
                    )
                })?;
                #[cfg(not(unix))]
                {
                    let _ = link_target;
                    return Err(AppError::new(
                        ErrorCode::Unsupported,
                        "macOS app-tar extraction requires a Unix host.",
                    ));
                }
            }
            EntryType::XGlobalHeader | EntryType::XHeader => {}
            other => {
                return Err(AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Refusing unsupported tar entry type {other:?} at {}.",
                        clean_rel.display()
                    ),
                ));
            }
        }
    }

    // Locate the single top-level `.app` bundle directory.
    let mut bundles = Vec::new();
    let entries = fs::read_dir(&canonical_root).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not inspect extracted macOS bundle: {err}"),
        )
    })?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() && p.extension().and_then(|e| e.to_str()) == Some("app") {
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
    if !bundle.join("Contents").join("MacOS").is_dir() {
        return Err(AppError::new(
            ErrorCode::Failed,
            format!(
                "Extracted bundle {} is missing Contents/MacOS.",
                bundle.display()
            ),
        ));
    }
    Ok(bundle)
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

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn validate_tar_symlink(entry_rel: &Path, target: &Path) -> Result<(), AppError> {
    if target.is_absolute() {
        return Err(AppError::new(
            ErrorCode::Denied,
            format!(
                "Refusing absolute symlink {} -> {} inside update archive.",
                entry_rel.display(),
                target.display()
            ),
        ));
    }
    let mut stack: Vec<std::ffi::OsString> = entry_rel
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
    for comp in target.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(part) => stack.push(part.to_os_string()),
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing escaping symlink {} -> {} inside update archive.",
                            entry_rel.display(),
                            target.display()
                        ),
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AppError::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing unsafe symlink {} -> {} inside update archive.",
                        entry_rel.display(),
                        target.display()
                    ),
                ));
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
    /// Target path to replace (`.AppImage` file on Linux, `.app` directory on macOS).
    pub target_path: PathBuf,
    /// Staged replacement path on the same filesystem as `target_path`.
    pub staged_new_path: PathBuf,
    /// Backup path on the same filesystem where the previous copy is held until startup succeeds.
    pub backup_path: PathBuf,
    /// Executable path to launch after swapping (`target_path` for AppImage,
    /// `<target_path>/Contents/MacOS/<bin>` for macOS).
    pub exec_path: PathBuf,
    /// Path the new build creates once its UI paints (`<updates_dir>/started-<new_seq>.ok`).
    pub ready_marker_path: PathBuf,
    /// Path where the watchdog writes rollback metadata if the new build fails to start.
    pub rollback_marker_path: PathBuf,
    /// JSON payload written to `rollback_marker_path` on rollback.
    pub rollback_json: String,
    /// Seconds to wait for `ready_marker_path` before restoring `backup_path`.
    pub wait_secs: u64,
    /// Clear AppImage mount variables (`APPIMAGE`, `APPDIR`, `ARGV0`, `OWD`) before launching.
    pub unset_appimage_env: bool,
}

/// Fixed POSIX `/bin/sh` supervisor script for Linux AppImage and macOS `.app` atomic swap with
/// automatic rollback. Receives all paths strictly through environment variables.
const UNIX_SWAP_SUPERVISOR_SCRIPT: &str = r#"
set -u

# 1. Wait up to 15s for the previous process to exit cleanly.
i=0
while [ "$i" -lt 60 ]; do
  if ! kill -0 "$LEGION_OLD_PID" 2>/dev/null; then
    break
  fi
  sleep 0.25 2>/dev/null || sleep 1
  i=$((i + 1))
done

rm -f -- "$LEGION_READY_MARKER"
rm -rf -- "$LEGION_BACKUP"

# 2. Move current install aside and put the verified new copy into place.
if ! mv -f -- "$LEGION_TARGET" "$LEGION_BACKUP"; then
  rm -rf -- "$LEGION_NEW"
  exit 1
fi

if ! mv -f -- "$LEGION_NEW" "$LEGION_TARGET"; then
  mv -f -- "$LEGION_BACKUP" "$LEGION_TARGET" || true
  exit 1
fi

launch_target() {
  if [ "${LEGION_UNSET_APPIMAGE:-0}" = "1" ]; then
    ( unset APPIMAGE APPDIR ARGV0 OWD; exec "$LEGION_EXEC" ) >/dev/null 2>&1 &
  else
    "$LEGION_EXEC" >/dev/null 2>&1 &
  fi
  NEW_PID=$!
}

# 3. Start the new build and wait for its ready marker.
launch_target
wait_limit="${LEGION_WAIT_SECS:-30}"
ticks=$((wait_limit * 4))
j=0
started=0
while [ "$j" -lt "$ticks" ]; do
  if [ -f "$LEGION_READY_MARKER" ]; then
    started=1
    break
  fi
  if ! kill -0 "$NEW_PID" 2>/dev/null; then
    break
  fi
  sleep 0.25 2>/dev/null || sleep 1
  j=$((j + 1))
done

if [ "$started" -eq 1 ]; then
  rm -rf -- "$LEGION_BACKUP"
  exit 0
fi

# 4. New build failed to start: kill it, record rollback, restore previous copy, and relaunch.
kill "$NEW_PID" 2>/dev/null || true
sleep 1
kill -9 "$NEW_PID" 2>/dev/null || true
printf '%s\n' "$LEGION_ROLLBACK_JSON" > "$LEGION_ROLLBACK_FILE"
rm -rf -- "$LEGION_TARGET"
mv -f -- "$LEGION_BACKUP" "$LEGION_TARGET"
launch_target
exit 1
"#;

/// Stages a verified Linux AppImage beside `appimage_path`, sets executable mode `0o755`, and
/// spawns the detached swap-and-rollback supervisor.
#[cfg(unix)]
pub fn stage_and_spawn_linux_appimage(
    verified_download: &Path,
    appimage_path: &Path,
    ready_marker_path: &Path,
    rollback_marker_path: &Path,
    rollback_json: String,
    wait_secs: u64,
) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;

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

    // Copy onto the same filesystem as `appimage_path` so the final rename is atomic.
    {
        let mut src = fs::File::open(verified_download).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!(
                    "Could not open verified AppImage {}: {err}",
                    verified_download.display()
                ),
            )
        })?;
        let mut dst = fs::File::create(&staged_new).map_err(|err| {
            AppError::new(
                ErrorCode::Failed,
                format!(
                    "Could not stage new AppImage in {} (check folder permissions): {err}",
                    parent.display()
                ),
            )
        })?;
        if let Err(err) = io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all()) {
            let _ = fs::remove_file(&staged_new);
            return Err(AppError::new(
                ErrorCode::Failed,
                format!("Could not copy new AppImage into place: {err}"),
            ));
        }
    }
    if let Err(err) = fs::set_permissions(&staged_new, fs::Permissions::from_mode(0o755)) {
        let _ = fs::remove_file(&staged_new);
        return Err(AppError::new(
            ErrorCode::Failed,
            format!("Could not mark staged AppImage executable: {err}"),
        ));
    }

    spawn_unix_swap_supervisor(&UnixSwapPlan {
        old_pid: pid,
        target_path: appimage_path.to_path_buf(),
        staged_new_path: staged_new,
        backup_path,
        exec_path: appimage_path.to_path_buf(),
        ready_marker_path: ready_marker_path.to_path_buf(),
        rollback_marker_path: rollback_marker_path.to_path_buf(),
        rollback_json,
        wait_secs,
        unset_appimage_env: true,
    })
}

/// Extracts a verified macOS `app-tar` beside `bundle_dir` and spawns the detached swap-and-rollback
/// supervisor.
#[cfg(unix)]
pub fn stage_and_spawn_macos_bundle(
    verified_tar_gz: &Path,
    bundle_dir: &Path,
    executable_rel: &Path,
    ready_marker_path: &Path,
    rollback_marker_path: &Path,
    rollback_json: String,
    wait_secs: u64,
) -> Result<(), AppError> {
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
    let extract_root = parent.join(format!(".{bundle_name}.extract-{pid}"));
    let staged_bundle = parent.join(format!(".{bundle_name}.new-{pid}"));
    let backup_path = parent.join(format!(".{bundle_name}.prev"));

    let _ = fs::remove_dir_all(&extract_root);
    let _ = fs::remove_dir_all(&staged_bundle);

    let extracted_bundle = match extract_macos_app_tar(verified_tar_gz, &extract_root) {
        Ok(b) => b,
        Err(err) => {
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
    spawn_unix_swap_supervisor(&UnixSwapPlan {
        old_pid: pid,
        target_path: bundle_dir.to_path_buf(),
        staged_new_path: staged_bundle,
        backup_path,
        exec_path,
        ready_marker_path: ready_marker_path.to_path_buf(),
        rollback_marker_path: rollback_marker_path.to_path_buf(),
        rollback_json,
        wait_secs,
        unset_appimage_env: false,
    })
}

/// Spawns the detached POSIX `/bin/sh` swap supervisor for [`UnixSwapPlan`].
#[cfg(unix)]
pub fn spawn_unix_swap_supervisor(plan: &UnixSwapPlan) -> Result<(), AppError> {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", UNIX_SWAP_SUPERVISOR_SCRIPT])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("LEGION_OLD_PID", plan.old_pid.to_string())
        .env("LEGION_TARGET", &plan.target_path)
        .env("LEGION_NEW", &plan.staged_new_path)
        .env("LEGION_BACKUP", &plan.backup_path)
        .env("LEGION_EXEC", &plan.exec_path)
        .env("LEGION_READY_MARKER", &plan.ready_marker_path)
        .env("LEGION_ROLLBACK_FILE", &plan.rollback_marker_path)
        .env("LEGION_ROLLBACK_JSON", &plan.rollback_json)
        .env("LEGION_WAIT_SECS", plan.wait_secs.to_string())
        .env(
            "LEGION_UNSET_APPIMAGE",
            if plan.unset_appimage_env { "1" } else { "0" },
        );

    cmd.spawn().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not start update helper process: {err}"),
        )
    })?;
    Ok(())
}

/// Fixed PowerShell supervisor script for Windows NSIS silent upgrade with automatic rollback.
/// Receives all paths strictly through environment variables.
pub const WINDOWS_NSIS_SUPERVISOR_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$oldPid = [int]$env:LEGION_OLD_PID
for ($i = 0; $i -lt 60; $i++) {
  if (-not (Get-Process -Id $oldPid -ErrorAction SilentlyContinue)) { break }
  Start-Sleep -Milliseconds 250
}

if (Test-Path $env:LEGION_READY_MARKER) {
  Remove-Item -LiteralPath $env:LEGION_READY_MARKER -Force -ErrorAction SilentlyContinue
}

function Restore-Previous {
  Set-Content -LiteralPath $env:LEGION_ROLLBACK_FILE -Value $env:LEGION_ROLLBACK_JSON -Encoding UTF8
  if (Test-Path $env:LEGION_BACKUP_DIR) {
    Copy-Item -Path (Join-Path $env:LEGION_BACKUP_DIR '*') -Destination $env:LEGION_INSTALL_DIR -Recurse -Force
  }
  if (Test-Path $env:LEGION_EXE) {
    Start-Process -FilePath $env:LEGION_EXE
  }
}

try {
  $inst = Start-Process -FilePath $env:LEGION_INSTALLER -ArgumentList ("/S /D=" + $env:LEGION_INSTALL_DIR) -Wait -PassThru
  if ($inst.ExitCode -ne 0) {
    Restore-Previous
    exit 1
  }
  $proc = Start-Process -FilePath $env:LEGION_EXE -PassThru
  $waitSecs = [int]$env:LEGION_WAIT_SECS
  if ($waitSecs -le 0) { $waitSecs = 30 }
  $started = $false
  for ($j = 0; $j -lt ($waitSecs * 4); $j++) {
    if (Test-Path $env:LEGION_READY_MARKER) {
      $started = $true
      break
    }
    if ($proc.HasExited) { break }
    Start-Sleep -Milliseconds 250
  }
  if ($started) {
    if (Test-Path $env:LEGION_BACKUP_DIR) {
      Remove-Item -LiteralPath $env:LEGION_BACKUP_DIR -Recurse -Force -ErrorAction SilentlyContinue
    }
    exit 0
  }
  if (-not $proc.HasExited) {
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  }
  Restore-Previous
  exit 1
} catch {
  Restore-Previous
  exit 1
}
"#;

/// Creates a backup copy of the current Windows installation directory and spawns the detached
/// PowerShell NSIS silent-update and rollback supervisor.
#[cfg(windows)]
pub fn stage_and_spawn_windows_nsis(
    verified_installer: &Path,
    install_dir: &Path,
    exe_path: &Path,
    backup_dir: &Path,
    ready_marker_path: &Path,
    rollback_marker_path: &Path,
    rollback_json: String,
    wait_secs: u64,
) -> Result<(), AppError> {
    use std::os::windows::process::CommandExt;

    let dir_str = install_dir.to_string_lossy();
    if dir_str.contains('"') || dir_str.contains('\r') || dir_str.contains('\n') {
        return Err(AppError::new(
            ErrorCode::Failed,
            "Installation directory path contains unsupported characters.",
        ));
    }

    let _ = fs::remove_dir_all(backup_dir);
    copy_dir_bounded(install_dir, backup_dir, 0)?;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        WINDOWS_NSIS_SUPERVISOR_SCRIPT,
    ])
    .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .env("LEGION_OLD_PID", std::process::id().to_string())
    .env("LEGION_INSTALLER", verified_installer)
    .env("LEGION_INSTALL_DIR", install_dir)
    .env("LEGION_EXE", exe_path)
    .env("LEGION_BACKUP_DIR", backup_dir)
    .env("LEGION_READY_MARKER", ready_marker_path)
    .env("LEGION_ROLLBACK_FILE", rollback_marker_path)
    .env("LEGION_ROLLBACK_JSON", rollback_json)
    .env("LEGION_WAIT_SECS", wait_secs.to_string());

    cmd.spawn().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not start Windows update helper: {err}"),
        )
    })?;
    Ok(())
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
        assert_eq!(deb.artifact_kind(), InstallKind::Deb);
        let cmd = deb
            .manual_command_for(Some(Path::new(
                "/home/me/.local/share/legion/legion_0.2.0.deb",
            )))
            .unwrap();
        assert_eq!(
            cmd,
            "sudo apt-get install /home/me/.local/share/legion/legion_0.2.0.deb"
        );

        let spaced = deb
            .manual_command_for(Some(Path::new("/home/me/my updates/legion 0.2.0.deb")))
            .unwrap();
        assert_eq!(
            spaced,
            "sudo apt-get install '/home/me/my updates/legion 0.2.0.deb'"
        );
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

    #[cfg(unix)]
    #[test]
    fn extract_macos_app_tar_accepts_valid_bundle_and_rejects_traversal_and_escaping_symlinks() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use tar::{Builder, Header};

        let dir = tempfile::tempdir().unwrap();

        // 1. Valid bundle with an internal relative symlink.
        let good_tar = dir.path().join("good.app.tar.gz");
        {
            let file = fs::File::create(&good_tar).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(enc);

            let bin_bytes = b"#!/bin/sh\necho ok\n";
            let mut h = Header::new_gnu();
            h.set_size(bin_bytes.len() as u64);
            h.set_mode(0o755);
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
        assert!(extracted.join("Contents/MacOS/legion-app").is_file());

        // 2. Escaping symlink is refused.
        let evil_symlink_tar = dir.path().join("evil-sym.app.tar.gz");
        {
            let file = fs::File::create(&evil_symlink_tar).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = Builder::new(enc);
            let mut link_h = Header::new_gnu();
            link_h.set_entry_type(tar::EntryType::Symlink);
            link_h.set_size(0);
            link_h.set_cksum();
            builder
                .append_link(
                    &mut link_h,
                    "Legion.app/Contents/MacOS/escape",
                    "../../../../etc/passwd",
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let err =
            extract_macos_app_tar(&evil_symlink_tar, &dir.path().join("out-evil")).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied, "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn unix_swap_supervisor_replaces_and_removes_backup_on_success_and_rolls_back_on_crash() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("Legion.AppImage");
        let staged_ok = dir.path().join(".Legion.AppImage.new-1");
        let backup = dir.path().join(".Legion.AppImage.prev");
        let ready_ok = dir.path().join("started-2.ok");
        let rollback_ok = dir.path().join("rollback-2.json");
        let ran_marker = dir.path().join("ran.txt");

        // Old version writes "v1" when launched.
        let v1_script = format!("#!/bin/sh\nprintf 'v1\\n' >> '{}'\n", ran_marker.display());
        fs::write(&target, v1_script).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        // Good new version writes ready marker and stays alive briefly.
        let v2_script = format!(
            "#!/bin/sh\nprintf 'v2\\n' >> '{}'\nprintf 'ok\\n' > '{}'\nsleep 2\n",
            ran_marker.display(),
            ready_ok.display()
        );
        fs::write(&staged_ok, v2_script).unwrap();
        fs::set_permissions(&staged_ok, fs::Permissions::from_mode(0o755)).unwrap();

        // Use an already-exited PID by spawning `exit 0` and reaping it so it is not a zombie.
        let mut exited_child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let exited_pid = exited_child.id();
        let _ = exited_child.wait();

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            target_path: target.clone(),
            staged_new_path: staged_ok,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_ok.clone(),
            rollback_marker_path: rollback_ok.clone(),
            rollback_json: "{\"sequence\":2}".to_owned(),
            wait_secs: 5,
            unset_appimage_env: true,
        })
        .unwrap();

        // Wait up to 8s for v2 ready marker and backup cleanup.
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if ready_ok.exists() && !backup.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(ready_ok.exists(), "expected v2 ready marker to be written");
        assert!(
            !backup.exists(),
            "expected .prev backup to be cleaned up after v2 started"
        );
        assert!(!rollback_ok.exists());
        assert!(fs::read_to_string(&target).unwrap().contains("v2"));

        // Now test ROLLBACK: stage a crashing v3 that exits 1 without writing ready_v3.
        let staged_bad = dir.path().join(".Legion.AppImage.new-3");
        let ready_bad = dir.path().join("started-3.ok");
        let rollback_bad = dir.path().join("rollback-3.json");
        fs::write(&staged_bad, "#!/bin/sh\nexit 1\n").unwrap();
        fs::set_permissions(&staged_bad, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_file(&ready_ok);

        spawn_unix_swap_supervisor(&UnixSwapPlan {
            old_pid: exited_pid,
            target_path: target.clone(),
            staged_new_path: staged_bad,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_bad,
            rollback_marker_path: rollback_bad.clone(),
            rollback_json: "{\"version\":\"0.3.0\",\"sequence\":3,\"reason\":\"crashed\"}"
                .to_owned(),
            wait_secs: 3,
            unset_appimage_env: true,
        })
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if rollback_bad.exists() && ready_ok.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            rollback_bad.exists(),
            "expected rollback marker to be written"
        );
        assert!(
            ready_ok.exists(),
            "expected previous working v2 copy to be restored and relaunched"
        );
        assert!(
            fs::read_to_string(&target).unwrap().contains("v2"),
            "expected target AppImage to be restored back to v2"
        );
        assert!(!backup.exists());
    }
}
