//! Shared helpers for real-binary and throwaway `sshd` integration tests in `legion-link`.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Environment variable naming an explicit `legion` binary to test against.
pub const LEGION_BIN_ENV: &str = "LEGION_BIN";

/// Environment variable that turns missing real-binary / `sshd` prerequisites into test failures
/// instead of skips (set in Linux CI jobs).
pub const REQUIRE_REAL_BINARY_ENV: &str = "LEGION_REQUIRE_REAL_BINARY";

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
pub fn require_real_binary() -> bool {
    env_flag_enabled(env::var_os(REQUIRE_REAL_BINARY_ENV).as_deref())
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub fn find_tool(name: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

pub fn find_sshd() -> Option<PathBuf> {
    find_tool("sshd").or_else(|| {
        ["/usr/sbin/sshd", "/sbin/sshd", "/usr/local/sbin/sshd"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| is_executable_file(p))
    })
}

pub fn current_user() -> Option<String> {
    let output = Command::new("id")
        .arg("-un")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// Creates a temporary directory under a private user-owned base directory (preferring
/// `CARGO_TARGET_TMPDIR`, then `CARGO_TARGET_DIR/tmp`, then `<workspace>/target/tmp`) instead of
/// `/tmp`, so OpenSSH `sshd` with `StrictModes yes` accepts `AuthorizedKeysFile` paths inside it
/// even when `TMPDIR` is unset and `/tmp` is `1777`.
pub fn strict_tempdir() -> TempDir {
    let base = option_env!("CARGO_TARGET_TMPDIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("CARGO_TARGET_TMPDIR").map(PathBuf::from))
        .or_else(|| {
            env::var_os("CARGO_TARGET_DIR")
                .filter(|v| !v.is_empty())
                .map(|dir| PathBuf::from(dir).join("tmp"))
        })
        .or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .map(|root| root.join("target").join("tmp"))
        });

    let dir = if let Some(base_path) = base {
        fs::create_dir_all(&base_path).unwrap();
        #[cfg(unix)]
        {
            let _ = fs::set_permissions(&base_path, fs::Permissions::from_mode(0o700));
        }
        tempfile::tempdir_in(&base_path).unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700));
    }
    dir
}

pub fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

fn resolve_explicit_legion_bin(test_name: &str) -> Result<Option<PathBuf>, String> {
    let Some(raw) = env::var_os(LEGION_BIN_ENV) else {
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
    let canonical = fs::canonicalize(&path).unwrap_or(path);
    Ok(Some(canonical))
}

/// Resolves `LEGION_BIN`, `sshd`, and `ssh-keygen` for a real-binary `sshd` test.
///
/// Prints one status line stating which binary is running or why the test was skipped. Never falls
/// back to guessing `target/debug/legion` or `target/release/legion`. Panics when `LEGION_BIN` is
/// invalid or when any prerequisite is missing while `LEGION_REQUIRE_REAL_BINARY=1`.
pub fn real_legion_bin_with_sshd(test_name: &str) -> Option<PathBuf> {
    if !cfg!(unix) {
        if require_real_binary() {
            panic!(
                "{test_name}: unprivileged loopback sshd requires Unix (required because {REQUIRE_REAL_BINARY_ENV} is enabled)"
            );
        }
        eprintln!("[real-binary] {test_name}: skipped (loopback sshd requires Unix)");
        return None;
    }
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
    let (Some(sshd_bin), Some(_keygen_bin)) = (find_sshd(), find_tool("ssh-keygen")) else {
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
    Some(legion_bin)
}
