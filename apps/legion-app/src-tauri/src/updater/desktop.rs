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
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_update::InstallKind;
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

    #[cfg(not(unix))]
    if !pending_symlinks.is_empty() {
        return Err(AppError::new(
            ErrorCode::Unsupported,
            "macOS app-tar extraction requires a Unix host.",
        ));
    }
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

/// Validates every symlink in `symlinks` against the archive root by resolving multi-hop symlink
/// chains (forward or backward in archive order) up to [`MAX_SYMLINK_CHAIN_DEPTH`]. Refuses any
/// symlink chain that escapes the archive root, uses an absolute target, or loops.
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
                if stack.pop().is_none() {
                    return Err(AppError::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing escaping symlink chain starting at {} -> {} inside update archive.",
                            link_rel.display(),
                            raw_target.display()
                        ),
                    ));
                }
                continue;
            }
            stack.push(seg);
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
    /// JSON payload written to `rollback_marker_path` on rollback.
    pub rollback_json: String,
    /// Optional diagnostic log file for supervisor stdout/stderr (`<updates_dir>/supervisor.log`).
    pub log_path: Option<PathBuf>,
    /// Seconds to wait for `ready_marker_path` before restoring `backup_path`.
    pub wait_secs: u64,
    /// When `true` (window-close install), launches the new build with `LEGION_UPDATE_VERIFY_ONLY=1`
    /// so it confirms startup and exits cleanly without opening a window, and does not relaunch a
    /// window on rollback.
    pub verify_only: bool,
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
        "LEGION_HOME",
        #[cfg(debug_assertions)]
        "LEGION_APP_READY_FILE",
        #[cfg(debug_assertions)]
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

printf 'supervisor: start old_pid=%s verify_only=%s target=%s\n' "$LEGION_OLD_PID" "${LEGION_VERIFY_ONLY:-0}" "$LEGION_TARGET"

# 1. Wait up to 15s for the previous process to exit cleanly.
i=0
while [ "$i" -lt 60 ]; do
  if ! kill -0 "$LEGION_OLD_PID" 2>/dev/null; then
    break
  fi
  "$SLEEP" 0.25 2>/dev/null || "$SLEEP" 1
  i=$((i + 1))
done
printf 'supervisor: old_pid=%s exited after %s ticks\n' "$LEGION_OLD_PID" "$i"

"$RM" -f -- "$LEGION_READY_MARKER"
"$RM" -rf -- "$LEGION_BACKUP"

# 2. Move current install aside and put the verified new copy into place.
if ! "$MV" -f -- "$LEGION_TARGET" "$LEGION_BACKUP"; then
  printf 'supervisor: failed to move target to backup\n'
  "$RM" -rf -- "$LEGION_NEW"
  exit 1
fi

if ! "$MV" -f -- "$LEGION_NEW" "$LEGION_TARGET"; then
  printf 'supervisor: failed to move staged new copy into target; restoring backup\n'
  "$MV" -f -- "$LEGION_BACKUP" "$LEGION_TARGET" || true
  exit 1
fi
printf 'supervisor: swapped new copy into target\n'

launch_target() {
  verify_flag="$1"
  exec_bin="$LEGION_EXEC"
  (
    PATH="${LEGION_RELAUNCH_PATH:-/usr/local/bin:/usr/bin:/bin}"
    export PATH
    if [ "$verify_flag" = "1" ]; then
      LEGION_UPDATE_VERIFY_ONLY=1
      export LEGION_UPDATE_VERIFY_ONLY
    else
      unset LEGION_UPDATE_VERIFY_ONLY
    fi
    unset LEGION_OLD_PID LEGION_TARGET LEGION_NEW LEGION_BACKUP LEGION_EXEC \
      LEGION_READY_MARKER LEGION_ROLLBACK_FILE LEGION_ROLLBACK_JSON \
      LEGION_WAIT_SECS LEGION_RELAUNCH_PATH LEGION_VERIFY_ONLY \
      APPIMAGE APPDIR ARGV0 OWD LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT \
      GTK_DATA_PREFIX GTK_EXE_PREFIX GTK_PATH GTK_IM_MODULE_FILE \
      GDK_PIXBUF_MODULE_FILE GSETTINGS_SCHEMA_DIR GI_TYPELIB_PATH GIO_MODULE_DIR \
      GST_PLUGIN_SYSTEM_PATH_1_0 GST_PLUGIN_PATH_1_0 GST_PLUGIN_SCANNER_1_0 GST_PTP_HELPER_1_0
    exec "$exec_bin"
  ) >/dev/null 2>&1 &
  NEW_PID=$!
}

# 3. Start the new build and wait for its ready marker.
verify_mode="${LEGION_VERIFY_ONLY:-0}"
launch_target "$verify_mode"
printf 'supervisor: launched new_pid=%s exec=%s\n' "$NEW_PID" "$LEGION_EXEC"
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
  new_stat="$("$PS" -o stat= -p "$NEW_PID" 2>/dev/null || true)"
  case "$new_stat" in
    ""|*Z*) break ;;
  esac
  "$SLEEP" 0.25 2>/dev/null || "$SLEEP" 1
  j=$((j + 1))
done

# One final check after process exit (in verify_only mode the new binary writes the marker and exits 0).
if [ -f "$LEGION_READY_MARKER" ]; then
  started=1
fi

if [ "$started" -eq 1 ]; then
  printf 'supervisor: ready marker observed at %s; removing backup\n' "$LEGION_READY_MARKER"
  "$RM" -rf -- "$LEGION_BACKUP"
  exit 0
fi

# 4. New build failed to start: stop it, record rollback, restore previous copy, and relaunch if interactive.
printf 'supervisor: ready marker missing after %s ticks; rolling back\n' "$j"
kill "$NEW_PID" 2>/dev/null || true
"$SLEEP" 1
kill -9 "$NEW_PID" 2>/dev/null || true
printf '%s\n' "$LEGION_ROLLBACK_JSON" > "$LEGION_ROLLBACK_FILE"
"$RM" -rf -- "$LEGION_TARGET"
"$MV" -f -- "$LEGION_BACKUP" "$LEGION_TARGET"
if [ "$verify_mode" != "1" ]; then
  launch_target 0
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
    wait_secs: u64,
    verify_only: bool,
) -> Result<(), AppError> {
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

    spawn_unix_swap_supervisor(&UnixSwapPlan {
        old_pid: pid,
        target_path: appimage_path.to_path_buf(),
        staged_new_path: staged_new,
        backup_path,
        exec_path: appimage_path.to_path_buf(),
        ready_marker_path: ready_marker_path.to_path_buf(),
        rollback_marker_path: rollback_marker_path.to_path_buf(),
        rollback_json,
        log_path: ready_marker_path.parent().map(|d| d.join("supervisor.log")),
        wait_secs,
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
    wait_secs: u64,
    verify_only: bool,
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
    spawn_unix_swap_supervisor(&UnixSwapPlan {
        old_pid: pid,
        target_path: bundle_dir.to_path_buf(),
        staged_new_path: staged_bundle,
        backup_path,
        exec_path,
        ready_marker_path: ready_marker_path.to_path_buf(),
        rollback_marker_path: rollback_marker_path.to_path_buf(),
        rollback_json,
        log_path: ready_marker_path.parent().map(|d| d.join("supervisor.log")),
        wait_secs,
        verify_only,
    })
}

/// Spawns the detached POSIX `/bin/sh` swap supervisor for [`UnixSwapPlan`] using the current
/// process environment (sanitized).
#[cfg(unix)]
pub fn spawn_unix_swap_supervisor(plan: &UnixSwapPlan) -> Result<(), AppError> {
    let sanitized = build_sanitized_unix_env(std::env::vars_os(), None);
    spawn_unix_swap_supervisor_with_env(plan, &sanitized)
}

/// Spawns the detached POSIX `/bin/sh` swap supervisor with an explicitly pre-sanitized
/// environment (used directly by security tests with planted `$APPDIR` fixtures).
#[cfg(unix)]
pub fn spawn_unix_swap_supervisor_with_env(
    plan: &UnixSwapPlan,
    sanitized: &SanitizedUnixEnv,
) -> Result<(), AppError> {
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
        .env("LEGION_WAIT_SECS", plan.wait_secs.to_string())
        .env(
            "LEGION_VERIFY_ONLY",
            if plan.verify_only { "1" } else { "0" },
        );

    cmd.spawn().map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not start update helper process: {err}"),
        )
    })?;
    Ok(())
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

/// Encodes a PowerShell script as UTF-16LE Base64 for `powershell.exe -EncodedCommand` so
/// embedded newlines and quotes are never mangled by Windows command-line parsing.
pub fn encode_powershell_command(script: &str) -> String {
    let mut utf16le = Vec::with_capacity(script.len() * 2);
    for unit in script.encode_utf16() {
        utf16le.extend_from_slice(&unit.to_le_bytes());
    }
    BASE64.encode(utf16le)
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

/// Fixed PowerShell supervisor script for Windows NSIS silent upgrade with automatic rollback.
///
/// Passed to `powershell.exe -EncodedCommand` (UTF-16LE Base64) so embedded newlines and quotes
/// are never mangled by `CreateProcessW` / PowerShell `-Command` parsing. Receives all paths
/// strictly through environment variables, uses `-LiteralPath` on every filesystem cmdlet, invokes
/// the NSIS installer directly via `ProcessStartInfo` (`UseShellExecute = $false`) with
/// `/S /UPDATE /D=<install_dir>` (`/D=` last and unquoted), writes progress lines to stdout
/// (redirected by the parent to `supervisor.log`), and restores the previous copy on any installer
/// or startup failure.
pub const WINDOWS_NSIS_SUPERVISOR_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
function Write-Log([string]$msg) {
  $line = "$(Get-Date -Format 'yyyy-MM-ddTHH:mm:ss.fffZ') $msg"
  [Console]::Out.WriteLine($line)
  [Console]::Out.Flush()
}

function Start-Direct([string]$filePath, [string]$arguments, [bool]$noWindow) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $filePath
  if ($arguments) { $psi.Arguments = $arguments }
  $psi.UseShellExecute = $false
  $psi.CreateNoWindow = $noWindow
  return [System.Diagnostics.Process]::Start($psi)
}

$oldPid = [int]$env:LEGION_OLD_PID
$verifyOnly = ($env:LEGION_VERIFY_ONLY -eq '1')
Write-Log "supervisor start oldPid=$oldPid verifyOnly=$verifyOnly installer=$($env:LEGION_INSTALLER) installDir=$($env:LEGION_INSTALL_DIR) exe=$($env:LEGION_EXE)"

for ($i = 0; $i -lt 80; $i++) {
  if (-not (Get-Process -Id $oldPid -ErrorAction SilentlyContinue)) { break }
  Start-Sleep -Milliseconds 250
}
if (Get-Process -Id $oldPid -ErrorAction SilentlyContinue) {
  Write-Log "oldPid $oldPid still alive after 20s; forcing stop"
  Stop-Process -Id $oldPid -Force -ErrorAction SilentlyContinue
  Start-Sleep -Milliseconds 500
} else {
  Write-Log "oldPid $oldPid exited cleanly"
  Start-Sleep -Milliseconds 250
}

if (Test-Path -LiteralPath $env:LEGION_READY_MARKER) {
  Remove-Item -LiteralPath $env:LEGION_READY_MARKER -Force -ErrorAction SilentlyContinue
}

function Restore-Previous([string]$why) {
  Write-Log "restoring previous install: $why"
  Set-Content -LiteralPath $env:LEGION_ROLLBACK_FILE -Value $env:LEGION_ROLLBACK_JSON -Encoding UTF8
  if (Test-Path -LiteralPath $env:LEGION_BACKUP_DIR) {
    Get-ChildItem -LiteralPath $env:LEGION_BACKUP_DIR -Force | ForEach-Object {
      Copy-Item -LiteralPath $_.FullName -Destination $env:LEGION_INSTALL_DIR -Recurse -Force
    }
  }
  if ((-not $verifyOnly) -and (Test-Path -LiteralPath $env:LEGION_EXE)) {
    Remove-Item Env:LEGION_UPDATE_VERIFY_ONLY -ErrorAction SilentlyContinue
    if ($env:LEGION_RELAUNCH_PATH) { $env:PATH = $env:LEGION_RELAUNCH_PATH }
    Start-Direct $env:LEGION_EXE '' $false | Out-Null
  }
}

try {
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
  Write-Log "launching updated app: $($env:LEGION_EXE)"
  $proc = Start-Direct $env:LEGION_EXE '' $false
  Write-Log "launched updated app pid=$($proc.Id)"
  $waitSecs = [int]$env:LEGION_WAIT_SECS
  if ($waitSecs -le 0) { $waitSecs = 30 }
  $started = $false
  for ($j = 0; $j -lt ($waitSecs * 4); $j++) {
    if (Test-Path -LiteralPath $env:LEGION_READY_MARKER) {
      $started = $true
      break
    }
    if ($proc.HasExited) {
      Write-Log "updated app exited early with code $($proc.ExitCode)"
      break
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
    exit 0
  }
  if (-not $proc.HasExited) {
    Write-Log "ready marker not observed after ${waitSecs}s; stopping pid=$($proc.Id)"
    Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
  }
  Restore-Previous "ready marker not observed"
  exit 1
} catch {
  Write-Log "supervisor exception: $($_.Exception.Message)"
  Restore-Previous $_.Exception.Message
  exit 1
}
"#;

/// Copies-while-hashing the verified NSIS installer into `verified_dir`, backs up the current
/// Windows installation directory, and spawns the detached System32 PowerShell supervisor via
/// `-EncodedCommand` with job breakaway (`CREATE_BREAKAWAY_FROM_JOB`, falling back if forbidden)
/// and a cleaned environment.
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
    wait_secs: u64,
    verify_only: bool,
) -> Result<(), AppError> {
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

    const WIN_ALLOWED_VARS: &[&str] = &[
        "SystemRoot",
        "WINDIR",
        "SystemDrive",
        "COMSPEC",
        "LOCALAPPDATA",
        "APPDATA",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "TEMP",
        "TMP",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "CommonProgramFiles",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
        "LEGION_HOME",
        #[cfg(debug_assertions)]
        "LEGION_APP_READY_FILE",
        #[cfg(debug_assertions)]
        "LEGION_APP_ALLOW_LOOPBACK_ENDPOINTS",
    ];

    let encoded_script = encode_powershell_command(WINDOWS_NSIS_SUPERVISOR_SCRIPT);
    let log_path = ready_marker_path.parent().map(|d| d.join("supervisor.log"));
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
        for (k, v) in std::env::vars_os() {
            if let Some(key) = k.to_str()
                && (WIN_ALLOWED_VARS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(key))
                    || key.to_ascii_uppercase().starts_with("WEBVIEW2_"))
            {
                cmd.env(&k, &v);
            }
        }
        cmd.env("PATH", &clean_path)
            .env("LEGION_RELAUNCH_PATH", &relaunch_path)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                &encoded_script,
            ])
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(stdout_cfg)
            .stderr(stderr_cfg)
            .env("LEGION_OLD_PID", pid.to_string())
            .env("LEGION_INSTALLER", &staged_installer)
            .env("LEGION_INSTALL_DIR", install_dir)
            .env("LEGION_EXE", exe_path)
            .env("LEGION_BACKUP_DIR", backup_dir)
            .env("LEGION_READY_MARKER", ready_marker_path)
            .env("LEGION_ROLLBACK_FILE", rollback_marker_path)
            .env("LEGION_ROLLBACK_JSON", &rollback_json)
            .env("LEGION_WAIT_SECS", wait_secs.to_string())
            .env("LEGION_VERIFY_ONLY", if verify_only { "1" } else { "0" });
        if let Some(lp) = &log_path {
            cmd.env("LEGION_SUPERVISOR_LOG", lp);
        }
        cmd
    };

    // Match `legion-os::detach`: `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` (never combined
    // with `DETACHED_PROCESS`, which is mutually exclusive with `CREATE_NO_WINDOW`). Try breaking
    // out of the parent's Windows Job Object first so exiting the app never kills the supervisor
    // or NSIS installer; fall back without breakaway if the job forbids it (`ERROR_ACCESS_DENIED`).
    let base_flags = CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    match build_cmd(base_flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Ok(_) => Ok(()),
        Err(err)
            if err.kind() == io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(5) =>
        {
            build_cmd(base_flags).spawn().map_err(|err2| {
                AppError::new(
                    ErrorCode::Failed,
                    format!("Could not start Windows update helper: {err2}"),
                )
            })?;
            Ok(())
        }
        Err(err) => Err(AppError::new(
            ErrorCode::Failed,
            format!("Could not start Windows update helper: {err}"),
        )),
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

        // EncodedCommand round-trips cleanly as UTF-16LE Base64.
        let encoded = encode_powershell_command(WINDOWS_NSIS_SUPERVISOR_SCRIPT);
        let decoded_bytes = BASE64.decode(&encoded).unwrap();
        assert_eq!(decoded_bytes.len() % 2, 0);
        let units: Vec<u16> = decoded_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            WINDOWS_NSIS_SUPERVISOR_SCRIPT
        );

        // NSIS silent update must pass /S /UPDATE /D= with /D= last and unquoted.
        assert!(
            WINDOWS_NSIS_SUPERVISOR_SCRIPT.contains("\"/S /UPDATE /D=\" + $env:LEGION_INSTALL_DIR"),
            "Windows NSIS supervisor must pass /S /UPDATE /D= unquoted and last"
        );

        // Every Test-Path, Remove-Item, Set-Content, Get-ChildItem, and Copy-Item in the script
        // must use -LiteralPath (never wildcard-expanding -Path or bare positional path).
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
        }
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
        let xdg = sanitized
            .session_vars
            .get(OsStr::new("XDG_DATA_DIRS"))
            .unwrap()
            .to_string_lossy();
        assert!(!xdg.contains(&*fake_appdir.to_string_lossy()));
        assert_eq!(xdg, "/usr/local/share:/usr/share");

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
                target_path: target.clone(),
                staged_new_path: staged,
                backup_path: backup.clone(),
                exec_path: target,
                ready_marker_path: ready.clone(),
                rollback_marker_path: rollback,
                rollback_json: "{}".to_owned(),
                log_path: None,
                wait_secs: 5,
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
            "LEGION_RELAUNCH_PATH=",
            "LEGION_TARGET=",
        ] {
            assert!(
                !dumped.contains(forbidden),
                "relaunched env leaked {forbidden}:\n{dumped}"
            );
        }
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
            target_path: target.clone(),
            staged_new_path: staged_ok,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_ok.clone(),
            rollback_marker_path: rollback_ok.clone(),
            rollback_json: "{\"sequence\":2}".to_owned(),
            log_path: None,
            wait_secs: 5,
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
            target_path: target.clone(),
            staged_new_path: staged_bad,
            backup_path: backup.clone(),
            exec_path: target.clone(),
            ready_marker_path: ready_bad,
            rollback_marker_path: rollback_bad.clone(),
            rollback_json: "{\"version\":\"0.3.0\",\"sequence\":3,\"reason\":\"crashed\"}"
                .to_owned(),
            log_path: None,
            wait_secs: 3,
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
}
