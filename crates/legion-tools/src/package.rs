//! Detecting whether the running `legion` binary is owned by a system package manager.
//!
//! System packages (`.deb` and Arch `PKGBUILD`) install `/usr/bin/legion` alongside a small marker
//! file at `/usr/lib/legion/managed-by` naming the package manager (`deb` or `pacman`). Reading
//! that marker through the executable's canonical path lets `meta.update` and `fleet.install`
//! refuse in-place replacement of packaged binaries while leaving standalone installs (such as
//! `~/.local/bin/legion`) untouched, even when both exist on the same machine.

use std::fs;
use std::path::{Path, PathBuf};

use legion_core::proto::{ErrorBody, ErrorCode};
use serde_json::json;

/// Relative path under the installation prefix where system packages write their ownership marker.
pub const MANAGED_BY_REL_PATH: &str = "lib/legion/managed-by";

/// Package manager that owns an installed `legion` binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PackageManager {
    /// Debian or Ubuntu `.deb` package managed with `apt` / `dpkg`.
    Deb,
    /// Arch Linux package managed with `pacman`.
    Pacman,
    /// Another package manager declared in the marker file.
    Other(String),
}

impl PackageManager {
    /// Parses the content of a `managed-by` marker file.
    ///
    /// Uses the first non-empty line, trimmed and lowercased. Empty files or values containing
    /// whitespace or unexpected characters are ignored.
    pub fn parse(raw: &str) -> Option<Self> {
        let token = raw
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())?
            .to_ascii_lowercase();
        if !token.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        }) {
            return None;
        }
        match token.as_str() {
            "deb" => Some(Self::Deb),
            "pacman" => Some(Self::Pacman),
            _ => Some(Self::Other(token)),
        }
    }

    /// Stable machine-readable identifier (`deb`, `pacman`, or the raw lowercase marker value).
    pub fn as_str(&self) -> &str {
        match self {
            Self::Deb => "deb",
            Self::Pacman => "pacman",
            Self::Other(name) => name.as_str(),
        }
    }

    /// Human-readable label shown in error messages and `meta.doctor`.
    pub fn display_name(&self) -> &str {
        match self {
            Self::Deb => "deb (apt)",
            Self::Pacman => "pacman",
            Self::Other(name) => name.as_str(),
        }
    }

    /// Actionable upgrade command shown in error hints.
    pub fn upgrade_hint(&self) -> String {
        match self {
            Self::Deb => "Upgrade the package with apt instead, for example: sudo apt-get install ./legion_<version>_<arch>.deb (or sudo apt update && sudo apt upgrade if installed from a repository).".to_owned(),
            Self::Pacman => "Upgrade the package with pacman instead, for example: sudo pacman -Syu legion (or rebuild and run sudo pacman -U <package>.pkg.tar.zst).".to_owned(),
            Self::Other(name) => format!(
                "Upgrade legion through {name} instead of replacing the binary in place."
            ),
        }
    }
}

/// Returns the `<prefix>/lib/legion/managed-by` path corresponding to `exe`'s real path, when `exe`
/// lives inside a `bin` directory.
pub fn marker_path_for_exe(exe: &Path) -> Option<PathBuf> {
    let real = dunce::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let bin_dir = real.parent()?;
    if bin_dir.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return None;
    }
    let prefix = bin_dir.parent()?;
    Some(prefix.join("lib").join("legion").join("managed-by"))
}

/// Detects whether `exe` is managed by a system package by reading its prefix's `managed-by`
/// marker through the executable's canonical path.
pub fn detect_package_manager(exe: &Path) -> Option<PackageManager> {
    let marker = marker_path_for_exe(exe)?;
    let contents = fs::read_to_string(marker).ok()?;
    PackageManager::parse(&contents)
}

/// Returns how `exe` was installed: `"deb"`, `"pacman"`, another marker identifier, or
/// `"standalone"` when no package marker exists.
pub fn install_method(exe: Option<&Path>) -> String {
    exe.and_then(detect_package_manager)
        .map_or_else(|| "standalone".to_owned(), |m| m.as_str().to_owned())
}

/// Builds the stable refusal returned by `meta.update` when the running binary is owned by a
/// package manager.
pub fn managed_update_error(manager: &PackageManager, exe: &Path) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        format!(
            "Cannot self-update {}: this legion installation is managed by {}.",
            exe.display(),
            manager.display_name()
        ),
    )
    .with_hint(manager.upgrade_hint())
    .with_details(json!({
        "managed_by": manager.as_str(),
        "path": exe.display().to_string(),
    }))
}

/// Builds the stable refusal returned by `fleet.install` when the remote machine's `legion` binary
/// is owned by a package manager.
pub fn managed_fleet_install_error(
    manager: &PackageManager,
    remote_path: &str,
    machine: &str,
) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        format!(
            "Cannot update legion on {machine} with fleet.install: {remote_path} is managed by {}.",
            manager.display_name()
        ),
    )
    .with_hint(manager.upgrade_hint())
    .with_machine(machine.to_owned())
    .with_details(json!({
        "managed_by": manager.as_str(),
        "path": remote_path,
    }))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_known_and_custom_markers_and_rejects_invalid_values() {
        assert_eq!(PackageManager::parse("deb\n"), Some(PackageManager::Deb));
        assert_eq!(
            PackageManager::parse("  PACMAN \r\n"),
            Some(PackageManager::Pacman)
        );
        assert_eq!(
            PackageManager::parse("\n\nrpm\nignored second line\n"),
            Some(PackageManager::Other("rpm".to_owned()))
        );
        assert_eq!(PackageManager::parse(""), None);
        assert_eq!(PackageManager::parse("   \n\t "), None);
        assert_eq!(PackageManager::parse("deb pacman"), None);
        assert_eq!(PackageManager::parse("../bad"), None);
    }

    #[test]
    fn detects_marker_relative_to_bin_prefix_and_ignores_standalone() {
        let dir = tempdir().unwrap();
        let usr_bin = dir.path().join("usr").join("bin");
        let usr_lib = dir.path().join("usr").join("lib").join("legion");
        fs::create_dir_all(&usr_bin).unwrap();
        fs::create_dir_all(&usr_lib).unwrap();
        let exe = usr_bin.join("legion");
        fs::write(&exe, b"bin").unwrap();

        // Without marker, installation is standalone.
        assert_eq!(detect_package_manager(&exe), None);
        assert_eq!(install_method(Some(&exe)), "standalone");

        // With `deb` marker, detected as deb.
        fs::write(usr_lib.join("managed-by"), "deb\n").unwrap();
        assert_eq!(detect_package_manager(&exe), Some(PackageManager::Deb));
        assert_eq!(install_method(Some(&exe)), "deb");

        // With `pacman` marker, detected as pacman.
        fs::write(usr_lib.join("managed-by"), "pacman\n").unwrap();
        assert_eq!(detect_package_manager(&exe), Some(PackageManager::Pacman));
        assert_eq!(install_method(Some(&exe)), "pacman");

        // A sibling user install under a different prefix is not affected.
        let local_bin = dir
            .path()
            .join("home")
            .join("me")
            .join(".local")
            .join("bin");
        fs::create_dir_all(&local_bin).unwrap();
        let local_exe = local_bin.join("legion");
        fs::write(&local_exe, b"bin").unwrap();
        assert_eq!(detect_package_manager(&local_exe), None);
        assert_eq!(install_method(Some(&local_exe)), "standalone");
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlink_to_detect_packaged_binary() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let usr_bin = dir.path().join("usr").join("bin");
        let usr_lib = dir.path().join("usr").join("lib").join("legion");
        fs::create_dir_all(&usr_bin).unwrap();
        fs::create_dir_all(&usr_lib).unwrap();
        let real_exe = usr_bin.join("legion");
        fs::write(&real_exe, b"bin").unwrap();
        fs::write(usr_lib.join("managed-by"), "deb\n").unwrap();

        let alias_dir = dir.path().join("alias");
        fs::create_dir_all(&alias_dir).unwrap();
        let link = alias_dir.join("legion");
        symlink(&real_exe, &link).unwrap();

        assert_eq!(detect_package_manager(&link), Some(PackageManager::Deb));
    }

    #[test]
    fn error_messages_and_hints_name_exact_package_manager_commands() {
        let deb_err = managed_update_error(&PackageManager::Deb, Path::new("/usr/bin/legion"));
        assert_eq!(deb_err.code, ErrorCode::Unsupported);
        assert!(deb_err.message.contains("deb (apt)"));
        assert!(deb_err.hint.as_deref().unwrap().contains("apt-get install"));
        assert_eq!(deb_err.details.as_ref().unwrap()["managed_by"], "deb");

        let pacman_err =
            managed_fleet_install_error(&PackageManager::Pacman, "/usr/bin/legion", "tower");
        assert_eq!(pacman_err.code, ErrorCode::Unsupported);
        assert_eq!(pacman_err.machine.as_deref(), Some("tower"));
        assert!(pacman_err.message.contains("pacman"));
        assert!(pacman_err.hint.as_deref().unwrap().contains("pacman"));
        assert_eq!(pacman_err.details.as_ref().unwrap()["managed_by"], "pacman");
    }
}
