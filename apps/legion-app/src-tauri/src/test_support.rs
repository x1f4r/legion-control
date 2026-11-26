//! Shared test helpers for real-binary and loopback `sshd` tests in `legion-app`.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Environment variable naming an explicit `legion` binary to test against.
pub(crate) const LEGION_BIN_ENV: &str = "LEGION_BIN";

/// Environment variable that turns missing real-binary / `sshd` prerequisites into test failures
/// instead of skips (set in Linux CI jobs).
pub(crate) const REQUIRE_REAL_BINARY_ENV: &str = "LEGION_REQUIRE_REAL_BINARY";

fn env_flag_enabled(value: Option<&OsStr>) -> bool {
    let Some(raw) = value.and_then(OsStr::to_str) else {
        return false;
    };
    let trimmed = raw.trim();
    !trimmed.is_empty()
        && !trimmed.eq_ignore_ascii_case("0")
        && !trimmed.eq_ignore_ascii_case("false")
        && !trimmed.eq_ignore_ascii_case("no")
}

/// Returns `true` when `LEGION_REQUIRE_REAL_BINARY` is set to a truthy value.
pub(crate) fn require_real_binary() -> bool {
    env_flag_enabled(std::env::var_os(REQUIRE_REAL_BINARY_ENV).as_deref())
}

/// Creates a temporary directory under a private user-owned base directory (preferring
/// `CARGO_TARGET_TMPDIR`, then `CARGO_TARGET_DIR/tmp`, then `<crate>/target/tmp`) instead of
/// `/tmp`, so OpenSSH `sshd` with `StrictModes yes` accepts `AuthorizedKeysFile` paths inside it
/// even when `TMPDIR` is unset and `/tmp` is `1777`.
pub(crate) fn strict_tempdir() -> tempfile::TempDir {
    let base = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_TARGET_TMPDIR").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR")
                .filter(|v| !v.is_empty())
                .map(|dir| PathBuf::from(dir).join("tmp"))
        })
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("tmp")
        });
    std::fs::create_dir_all(&base).unwrap_or_else(|err| {
        panic!(
            "failed to create strict temp base {}: {err}",
            base.display()
        )
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap_or_else(
            |err| {
                panic!(
                    "failed to chmod 0700 strict temp base {}: {err}",
                    base.display()
                )
            },
        );
    }
    let dir = tempfile::tempdir_in(&base).unwrap_or_else(|err| {
        panic!(
            "failed to create strict tempdir in {}: {err}",
            base.display()
        )
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|err| {
                panic!(
                    "failed to chmod 0700 strict tempdir {}: {err}",
                    dir.path().display()
                )
            });
    }
    dir
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn resolve_explicit_legion_bin(test_name: &str) -> Result<Option<PathBuf>, String> {
    let Some(raw) = std::env::var_os(LEGION_BIN_ENV) else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(raw);
    if !is_executable_file(&path) {
        return Err(format!(
            "{test_name}: {LEGION_BIN_ENV}={} does not point to an executable file",
            path.display()
        ));
    }
    let canonical = std::fs::canonicalize(&path).unwrap_or(path);
    Ok(Some(canonical))
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn find_sshd() -> Option<PathBuf> {
    find_tool("sshd").or_else(|| {
        ["/usr/sbin/sshd", "/sbin/sshd", "/usr/local/sbin/sshd"]
            .into_iter()
            .map(PathBuf::from)
            .find(|path| is_executable_file(path))
    })
}

/// Resolves `LEGION_BIN` for a real-binary test that does not need `sshd`.
///
/// Prints one status line stating which binary is running or why the test was skipped. Never falls
/// back to guessing `target/debug/legion` or `target/release/legion`. Panics when `LEGION_BIN` is
/// set to an invalid path, or when `LEGION_BIN` is unset and `LEGION_REQUIRE_REAL_BINARY=1`.
pub(crate) fn real_legion_bin(test_name: &str) -> Option<PathBuf> {
    let bin = resolve_explicit_legion_bin(test_name).unwrap_or_else(|err| panic!("{err}"));
    match bin {
        Some(path) => {
            eprintln!(
                "[real-binary] {test_name}: running with LEGION_BIN={}",
                path.display()
            );
            Some(path)
        }
        None => {
            if require_real_binary() {
                panic!(
                    "{test_name}: {LEGION_BIN_ENV} is not set (required because {REQUIRE_REAL_BINARY_ENV} is enabled; build `legion` and set {LEGION_BIN_ENV})"
                );
            }
            eprintln!("[real-binary] {test_name}: skipped ({LEGION_BIN_ENV} is not set)");
            None
        }
    }
}

/// Resolves `LEGION_BIN`, `sshd`, and `ssh-keygen` for a real-binary test backed by loopback
/// `sshd`.
///
/// Prints one status line stating which binary is running or why the test was skipped. Panics when
/// `LEGION_BIN` is invalid or when any prerequisite is missing while
/// `LEGION_REQUIRE_REAL_BINARY=1`.
#[cfg(unix)]
pub(crate) fn real_legion_bin_with_sshd(test_name: &str) -> Option<(PathBuf, PathBuf, PathBuf)> {
    let bin = resolve_explicit_legion_bin(test_name).unwrap_or_else(|err| panic!("{err}"));
    let Some(legion_bin) = bin else {
        if require_real_binary() {
            panic!(
                "{test_name}: {LEGION_BIN_ENV} is not set (required because {REQUIRE_REAL_BINARY_ENV} is enabled; build `legion` and set {LEGION_BIN_ENV})"
            );
        }
        eprintln!("[real-binary] {test_name}: skipped ({LEGION_BIN_ENV} is not set)");
        return None;
    };
    let (Some(sshd_bin), Some(keygen_bin)) = (find_sshd(), find_tool("ssh-keygen")) else {
        if require_real_binary() {
            panic!(
                "{test_name}: sshd or ssh-keygen unavailable (required because {REQUIRE_REAL_BINARY_ENV} is enabled)"
            );
        }
        eprintln!("[real-binary] {test_name}: skipped (sshd or ssh-keygen unavailable)");
        return None;
    };
    eprintln!(
        "[real-binary] {test_name}: running with LEGION_BIN={} (sshd={})",
        legion_bin.display(),
        sshd_bin.display()
    );
    Some((legion_bin, sshd_bin, keygen_bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_flag_parsing_handles_truthy_and_falsy_values() {
        assert!(!env_flag_enabled(None));
        assert!(!env_flag_enabled(Some(OsStr::new(""))));
        assert!(!env_flag_enabled(Some(OsStr::new("0"))));
        assert!(!env_flag_enabled(Some(OsStr::new("false"))));
        assert!(!env_flag_enabled(Some(OsStr::new("NO"))));
        assert!(env_flag_enabled(Some(OsStr::new("1"))));
        assert!(env_flag_enabled(Some(OsStr::new("true"))));
        assert!(env_flag_enabled(Some(OsStr::new("yes"))));
    }

    #[cfg(unix)]
    #[test]
    fn strict_tempdir_has_private_permissions_and_avoids_world_writable_tmp() {
        use std::os::unix::fs::PermissionsExt;

        let dir = strict_tempdir();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "strict_tempdir must be mode 0700");
        assert!(
            !dir.path().starts_with(Path::new("/tmp")),
            "strict_tempdir must not live under /tmp: {}",
            dir.path().display()
        );
    }
}
