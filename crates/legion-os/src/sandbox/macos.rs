//! macOS Seatbelt sandboxing for `__thumb-worker` (`sandbox_init`) and `ffmpeg` (`sandbox-exec`).
//!
//! - **Decode worker (`__thumb-worker`):** calls `sandbox_init` in `libSystem` directly inside
//!   the worker process with a deny-by-default SBPL profile allowing only `sysctl-read` (while
//!   keeping already-open stdio descriptors, non-file memory allocation, and process exit).
//! - **`ffmpeg` video poster extraction:** runs `ffmpeg` through `/usr/bin/sandbox-exec` with a
//!   deny-by-default SBPL profile that denies all network access and process forks, allows
//!   executing only `ffmpeg`, reading only the single input video file + dynamic libraries, and
//!   writing only to the throwaway temp directory (and `/dev/null`).

#![allow(unsafe_code)]

use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use crate::sandbox::{SandboxReport, SandboxUnavailable};
use crate::{OsError, Result};

const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// Minimal deny-by-default Seatbelt profile for `__thumb-worker`.
///
/// Denies every filesystem open, socket creation, and process fork/exec while allowing reads and
/// writes on already-inherited stdio file descriptors, heap memory allocation, `sysctl-read`, and
/// self-signals (e.g. `SIGABRT`).
const WORKER_SBPL_PROFILE: &str =
    "(version 1)\n(deny default)\n(allow sysctl-read)\n(allow signal (target self))\n";

unsafe extern "C" {
    fn sandbox_init(
        profile: *const libc::c_char,
        flags: u64,
        errorbuf: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sandbox_free_error(errorbuf: *mut libc::c_char);
}

pub(super) fn apply_worker_sandbox() -> std::result::Result<SandboxReport, SandboxUnavailable> {
    let profile = CString::new(WORKER_SBPL_PROFILE).map_err(|err| SandboxUnavailable {
        platform: "macos".to_owned(),
        missing: "seatbelt profile".to_owned(),
        detail: format!("invalid SBPL profile: {err}"),
    })?;
    let mut errbuf: *mut libc::c_char = std::ptr::null_mut();
    // SAFETY: `profile` is NUL-terminated, `flags == 0` selects raw SBPL text, and `errbuf` is
    // freed with `sandbox_free_error` whenever non-null.
    let rc = unsafe { sandbox_init(profile.as_ptr(), 0, &mut errbuf) };
    if rc != 0 {
        let message = if errbuf.is_null() {
            format!("sandbox_init returned {rc}")
        } else {
            // SAFETY: `errbuf` is a NUL-terminated C string allocated by `sandbox_init`.
            let text = unsafe { CStr::from_ptr(errbuf) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: `errbuf` was returned by `sandbox_init` and is freed once here.
            unsafe { sandbox_free_error(errbuf) };
            text
        };
        return Err(SandboxUnavailable {
            platform: "macos".to_owned(),
            missing: "sandbox_init (Seatbelt)".to_owned(),
            detail: format!("macOS Seatbelt sandbox_init failed: {message}"),
        });
    }
    if !errbuf.is_null() {
        // SAFETY: free any diagnostic buffer returned by `sandbox_init`.
        unsafe { sandbox_free_error(errbuf) };
    }

    Ok(SandboxReport {
        available: true,
        complete: true,
        platform: "macos".to_owned(),
        mechanism: "seatbelt".to_owned(),
        detail:
            "macOS decode worker sandbox active (Seatbelt deny-default profile via sandbox_init)."
                .to_owned(),
        missing: None,
    })
}

fn escape_sbpl_string(path: &Path) -> Result<String> {
    let text = path.to_str().ok_or_else(|| OsError::InvalidArgument {
        detail: format!(
            "cannot build macOS sandbox profile for non-UTF-8 path {}",
            path.display()
        ),
    })?;
    if text.contains('\0') || text.contains('\n') || text.contains('\r') {
        return Err(OsError::InvalidArgument {
            detail: format!(
                "cannot build macOS sandbox profile for control-character path {}",
                path.display()
            ),
        });
    }
    let mut out = String::with_capacity(text.len() + 4);
    for ch in text.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    Ok(out)
}

fn canonical_and_given(path: &Path) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();
    set.insert(path.to_path_buf());
    if let Ok(canon) = fs::canonicalize(path) {
        set.insert(canon);
    }
    set.into_iter().collect()
}

fn build_ffmpeg_sbpl_profile(
    ffmpeg_bin: &Path,
    input_file: Option<&Path>,
    temp_dir: &Path,
) -> Result<String> {
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (deny network*)\n\
         (deny process-fork)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow signal (target self))\n\
         (allow file-read-metadata)\n\
         (allow file-read* (literal \"/dev/null\") (literal \"/dev/urandom\") (literal \"/dev/random\") (literal \"/dev/dtracehelper\"))\n\
         (allow file-write* (literal \"/dev/null\"))\n\
         (allow file-read* (subpath \"/System/Library\") (subpath \"/System/Volumes/Preboot/Cryptexes\") (subpath \"/System/Cryptexes\") (subpath \"/private/var/db/dyld\") (subpath \"/usr/lib\") (subpath \"/Library/Frameworks\"))\n\
         (allow file-read* (subpath \"/opt/homebrew/lib\") (subpath \"/opt/homebrew/Cellar\") (subpath \"/opt/homebrew/opt\"))\n\
         (allow file-read* (subpath \"/usr/local/lib\") (subpath \"/usr/local/Cellar\") (subpath \"/usr/local/opt\"))\n\
         (allow file-map-executable (subpath \"/System/Library\") (subpath \"/System/Volumes/Preboot/Cryptexes\") (subpath \"/System/Cryptexes\") (subpath \"/usr/lib\") (subpath \"/Library/Frameworks\") (subpath \"/opt/homebrew\") (subpath \"/usr/local\"))\n",
    );

    // Allow executing, reading, and mapping ONLY the resolved `ffmpeg` binary (plus its prefix lib dir).
    for bin in canonical_and_given(ffmpeg_bin) {
        let esc = escape_sbpl_string(&bin)?;
        profile.push_str(&format!("(allow process-exec (literal \"{esc}\"))\n"));
        profile.push_str(&format!("(allow file-read* (literal \"{esc}\"))\n"));
        profile.push_str(&format!(
            "(allow file-map-executable (literal \"{esc}\"))\n"
        ));
        if let Some(prefix) = bin.parent().and_then(Path::parent) {
            let lib = prefix.join("lib");
            if lib.is_dir() {
                let lib_esc = escape_sbpl_string(&lib)?;
                profile.push_str(&format!("(allow file-read* (subpath \"{lib_esc}\"))\n"));
                profile.push_str(&format!(
                    "(allow file-map-executable (subpath \"{lib_esc}\"))\n"
                ));
            }
        }
    }

    // Allow reading ONLY the single input video file (when opened by path).
    if let Some(input) = input_file {
        for p in canonical_and_given(input) {
            let esc = escape_sbpl_string(&p)?;
            profile.push_str(&format!("(allow file-read* (literal \"{esc}\"))\n"));
        }
    }

    // Allow reading and writing ONLY inside the dedicated throwaway temp directory.
    for td in canonical_and_given(temp_dir) {
        let esc = escape_sbpl_string(&td)?;
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath \"{esc}\"))\n"
        ));
    }

    Ok(profile)
}

/// Wraps an `ffmpeg` invocation inside `/usr/bin/sandbox-exec` so macOS enforces network,
/// filesystem, and process-fork isolation before `ffmpeg` starts. Fails closed if
/// `/usr/bin/sandbox-exec` is unavailable.
pub(crate) fn wrap_ffmpeg_command(
    ffmpeg_bin: &Path,
    input_file: Option<&Path>,
    temp_dir: &Path,
    original_args: &[OsString],
) -> Result<(OsString, Vec<OsString>)> {
    let wrapper = Path::new(SANDBOX_EXEC_PATH);
    if !wrapper.is_file() {
        return Err(OsError::Unsupported {
            detail: format!("macOS ffmpeg sandbox unavailable: {SANDBOX_EXEC_PATH} does not exist"),
        });
    }
    let profile = build_ffmpeg_sbpl_profile(ffmpeg_bin, input_file, temp_dir)?;
    let mut wrapped_args = Vec::with_capacity(original_args.len() + 4);
    wrapped_args.push(OsString::from("-p"));
    wrapped_args.push(OsString::from(profile));
    wrapped_args.push(OsString::from("--"));
    wrapped_args.push(ffmpeg_bin.as_os_str().to_os_string());
    wrapped_args.extend(original_args.iter().cloned());
    Ok((OsString::from(SANDBOX_EXEC_PATH), wrapped_args))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sbpl_escape_handles_quotes_and_backslashes_and_rejects_newlines() {
        let escaped = escape_sbpl_string(Path::new("/tmp/a\"b\\c.mp4")).expect("escape");
        assert_eq!(escaped, "/tmp/a\\\"b\\\\c.mp4");
        assert!(escape_sbpl_string(Path::new("/tmp/bad\nline.mp4")).is_err());
    }

    #[test]
    fn ffmpeg_sbpl_profile_denies_network_and_forks_and_pins_input_and_temp() {
        let profile = build_ffmpeg_sbpl_profile(
            Path::new("/opt/homebrew/bin/ffmpeg"),
            Some(Path::new("/tmp/sample.mp4")),
            Path::new("/tmp/work"),
        )
        .expect("profile");
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(deny process-fork)"));
        assert!(profile.contains("(allow process-exec (literal \"/opt/homebrew/bin/ffmpeg\"))"));
        assert!(profile.contains("(allow file-read* (literal \"/tmp/sample.mp4\"))"));
        assert!(profile.contains("(allow file-read* file-write* (subpath \"/tmp/work\"))"));
    }
}
