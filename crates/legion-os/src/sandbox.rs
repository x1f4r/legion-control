//! OS-level sandboxing for untrusted media decode workers and external `ffmpeg` extraction.
//!
//! All platform-specific sandbox FFI (`prctl`, `seccomp`, `landlock`, macOS `sandbox_init` /
//! `sandbox-exec`, Windows AppContainer / Restricted Token + Job Objects) lives under this module
//! so higher-level crates stay `unsafe_code = "forbid"` and platform-agnostic.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(windows)]
pub(crate) mod windows;

/// Environment variable that allows debug builds ONLY to decode without an OS sandbox.
///
/// Release builds (`!cfg!(debug_assertions)`) ignore this variable completely so production
/// binaries always fail closed when the OS sandbox is unavailable.
pub const DEV_UNSANDBOXED_ENV: &str = "LEGION_ALLOW_UNSANDBOXED_DECODE";

/// Returns `true` only in debug builds when [`DEV_UNSANDBOXED_ENV`] is explicitly set to `1` or
/// `true`. Always returns `false` in release builds.
pub fn dev_unsandboxed_override_active() -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }
    std::env::var_os(DEV_UNSANDBOXED_ENV)
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Summary of the OS sandbox state for the decode worker on this host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SandboxReport {
    /// `true` when an OS-level worker sandbox was installed or verified.
    pub available: bool,
    /// `true` when the full isolation guarantee holds (no filesystem open, no network sockets,
    /// no child process creation). `false` when unavailable or running under a reduced Windows
    /// fallback (Restricted Token + Low Integrity + Job Object).
    pub complete: bool,
    /// Short platform identifier (`"linux"`, `"macos"`, `"windows"`, ...).
    pub platform: String,
    /// Active sandbox mechanism (e.g. `"seccomp-bpf+landlock"`, `"seccomp-bpf"`, `"seatbelt"`,
    /// `"appcontainer+job"`, `"restricted-token+job"`, or `"none"`).
    pub mechanism: String,
    /// One-sentence human-readable summary suitable for `meta.doctor` and diagnostics.
    pub detail: String,
    /// What capability is missing when `!available` or `!complete`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

/// Typed failure returned when the current platform or host kernel cannot enforce the decode
/// worker sandbox.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SandboxUnavailable {
    /// Short platform identifier (`"linux"`, `"macos"`, `"windows"`, ...).
    pub platform: String,
    /// Specific kernel or OS feature that was unavailable or refused.
    pub missing: String,
    /// Human-readable explanation of why the sandbox could not be applied.
    pub detail: String,
}

impl SandboxUnavailable {
    /// Converts this unavailability error into a structured [`SandboxReport`].
    pub fn into_report(self) -> SandboxReport {
        SandboxReport {
            available: false,
            complete: false,
            platform: self.platform,
            mechanism: "none".to_owned(),
            detail: self.detail,
            missing: Some(self.missing),
        }
    }
}

impl fmt::Display for SandboxUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "decode worker sandbox unavailable on {} (missing {}): {}",
            self.platform, self.missing, self.detail
        )
    }
}

impl std::error::Error for SandboxUnavailable {}

/// Containment policy applied to a child process launched through [`crate::run::run`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxPolicy {
    /// Legion's internal `__thumb-worker` subprocess.
    ///
    /// - On Linux and macOS, the worker installs its own strict sandbox before reading byte 0 of
    ///   untrusted input (via [`apply_worker_sandbox`]).
    /// - On Windows, the parent launches the worker inside a zero-capability AppContainer and a
    ///   single-process Job Object (falling back on Windows-only to a Low-Integrity Restricted
    ///   Token + Job Object if AppContainer cannot start).
    Worker,
    /// External `ffmpeg` video poster extraction subprocess.
    ///
    /// Blocks network access on every platform and restricts filesystem access to the single
    /// input video (when opened by path), the single throwaway temp directory, and the libraries
    /// `ffmpeg` needs to run.
    Ffmpeg {
        /// Resolved path to the `ffmpeg` executable.
        ffmpeg_bin: PathBuf,
        /// The single input video file path, or `None` when the video is passed on `stdin` (`fd:`).
        input_file: Option<PathBuf>,
        /// Dedicated per-invocation throwaway temporary directory.
        temp_dir: PathBuf,
    },
}

/// Irreversibly enters (Linux/macOS) or verifies (Windows) the decode worker OS sandbox in the
/// current process.
///
/// Must be called inside `__thumb-worker` after startup setup (RSS baseline sampling and `stdin`
/// handle duplication/metadata) and strictly **before** reading byte 0 of untrusted media input.
pub fn apply_worker_sandbox() -> std::result::Result<SandboxReport, SandboxUnavailable> {
    #[cfg(target_os = "linux")]
    {
        linux::apply_worker_sandbox()
    }
    #[cfg(target_os = "macos")]
    {
        macos::apply_worker_sandbox()
    }
    #[cfg(windows)]
    {
        windows::verify_worker_sandbox()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        unsupported::apply_worker_sandbox()
    }
}

/// Prepares platform pre-exec state on Linux for a [`SandboxPolicy::Ffmpeg`] launch.
#[cfg(target_os = "linux")]
pub(crate) use linux::{PreparedFfmpegSandbox, prepare_ffmpeg_sandbox};

/// Wraps an `ffmpeg` invocation inside macOS `/usr/bin/sandbox-exec`.
#[cfg(target_os = "macos")]
pub(crate) use macos::wrap_ffmpeg_command;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_unavailable_round_trips_into_report() {
        let err = SandboxUnavailable {
            platform: "linux".to_owned(),
            missing: "seccomp-bpf".to_owned(),
            detail: "kernel returned ENOSYS".to_owned(),
        };
        let report = err.into_report();
        assert!(!report.available);
        assert!(!report.complete);
        assert_eq!(report.platform, "linux");
        assert_eq!(report.mechanism, "none");
        assert_eq!(report.missing.as_deref(), Some("seccomp-bpf"));
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_builds_never_honor_dev_unsandboxed_override() {
        assert!(!dev_unsandboxed_override_active());
    }
}
