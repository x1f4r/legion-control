//! Launching processes that outlive the caller, its terminal, and its SSH session.
//!
//! Each platform needs a different escape hatch so session teardown does not kill a detached job:
//!
//! - **Linux with a reachable `systemd --user` manager and lingering enabled:** a process started
//!   inside an SSH session remains in that session's cgroup (`session-N.scope`) even after
//!   `setsid()`, so `systemd-logind` with `KillUserProcesses=yes` terminates it when SSH
//!   disconnects. Wrapping the command in a transient
//!   `systemd-run --user --scope --quiet --collect --unit=legion-job-<name> --` moves it into the
//!   user manager's cgroup hierarchy while `exec`-ing the target in place. Because `logind` stops
//!   the user manager roughly ten seconds after the last session ends unless lingering is enabled
//!   for the account, lingering is checked first (`/var/lib/systemd/linger/<user>` or
//!   `loginctl show-user --property=Linger`); when lingering is off, launching falls back to
//!   `setsid` and records an actionable `loginctl enable-linger` warning in [`DetachHandle::warnings`].
//! - **Other Unix systems (macOS, containers, or Linux without a user manager):** starting the
//!   child in a new session via `setsid()` detaches it from the caller's controlling terminal and
//!   process group so `SIGHUP` on terminal or session close does not reach it.
//! - **Windows (direct path):** spawning with `CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW |
//!   CREATE_NEW_PROCESS_GROUP` creates a process in its own process group outside the caller's Job
//!   Object without opening visible console windows for console children (unlike `DETACHED_PROCESS`,
//!   which causes console programs spawned by the job to allocate visible console windows).
//! - **Windows (WMI fallback):** OpenSSH's `sshd` places each SSH session in a Job Object that
//!   forbids breakaway, causing `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB` to fail with
//!   `ERROR_ACCESS_DENIED`. Falling back to `Win32_Process.Create` via `Invoke-CimMethod` asks the
//!   WMI provider host (`wmiprvse.exe`), which lives in its own service session outside `sshd`'s
//!   job, to launch the process so it survives SSH disconnect.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::procid::{self, KillTreeReport, ProcessId};
use crate::shell::{
    self, CommandSpec, ResolvedCommand, encode_utf16le_base64, powershell_single_quote,
    windows_cmdline_escape,
};
use crate::{OsError, Result};

static SCOPE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
#[path = "detach/unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "detach/windows.rs"]
mod platform;

/// Maximum length of the sanitized caller-supplied prefix inside a Linux systemd scope unit name.
#[allow(dead_code)]
const MAX_SCOPE_PREFIX_LEN: usize = 48;

/// Maximum safe command-line length accepted by Windows `Win32_Process.Create`.
#[allow(dead_code)]
pub(crate) const WINDOWS_WMI_MAX_CMDLINE: usize = 32_000;

/// Configuration for [`spawn_detached`].
#[derive(Clone, Debug)]
pub struct DetachOptions {
    /// Command to start (exact argv or platform shell string).
    pub command: CommandSpec,
    /// Optional caller-chosen label used as a sanitized, length-limited prefix for the Linux
    /// transient systemd scope (`--unit=legion-job-<prefix>-<unique>`). A unique suffix is always
    /// appended so repeated launches with the same name never collide.
    pub name: Option<String>,
    /// Working directory for the detached process; inherits the caller's cwd when `None`.
    pub cwd: Option<PathBuf>,
    /// Extra environment variables overlaid on the inherited environment.
    pub env: BTreeMap<OsString, OsString>,
    /// File path to append standard output to; redirected to null when `None`.
    pub stdout_path: Option<PathBuf>,
    /// File path to append standard error to; redirected to null when `None`.
    pub stderr_path: Option<PathBuf>,
}

impl DetachOptions {
    /// Create detach options for `command` with null stdout/stderr and inherited cwd/env.
    pub fn new(command: CommandSpec) -> Self {
        Self {
            command,
            name: None,
            cwd: None,
            env: BTreeMap::new(),
            stdout_path: None,
            stderr_path: None,
        }
    }

    /// Set a caller-chosen job name used for the Linux systemd scope (`legion-job-<name>`).
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the working directory for the detached process.
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Set a single environment variable for the detached process.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Set multiple environment variables for the detached process.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        for (k, v) in vars {
            self.env.insert(k.into(), v.into());
        }
        self
    }

    /// Append standard output to `path`.
    pub fn stdout_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.stdout_path = Some(path.into());
        self
    }

    /// Append standard error to `path`.
    pub fn stderr_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.stderr_path = Some(path.into());
        self
    }

    /// Append both standard output and standard error to the same file `path`.
    pub fn output_path(mut self, path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        self.stdout_path = Some(p.clone());
        self.stderr_path = Some(p);
        self
    }
}

/// Persistent handle for a detached process, serializable for storage in a job record.
///
/// Later invocations (including in a different process) can deserialize this handle and pass it
/// to [`stop_detached`] to tear down the entire detached process tree or Linux systemd scope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DetachHandle {
    /// Process identity (`pid` and full-precision kernel start time) of the detached target.
    pub process: ProcessId,
    /// Transient Linux systemd user scope unit name (for example `legion-job-<name>.scope`) when
    /// the process was started inside a systemd user scope; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub systemd_unit: Option<String>,
    /// Non-fatal warnings produced while launching (for example when systemd user lingering is off).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Internal test seam controlling Linux systemd user-scope selection so integration tests can
/// exercise both the systemd-scope path and the `setsid` fallback paths on any host.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScopeProbeOverride {
    /// Probe the systemd user manager and lingering state normally.
    #[default]
    Auto,
    /// Behave as if no systemd user manager is reachable (forces direct `setsid`).
    ForceNoSystemd,
    /// Probe the systemd user manager normally, but treat lingering as disabled (forces `setsid`
    /// fallback with the lingering warning when a user manager is reachable).
    ForceLingerDisabled,
}

/// Start a detached process that outlives the caller, its terminal, and its SSH session.
///
/// Standard input is always connected to null; standard output and standard error are appended
/// to the configured files (or connected to null when omitted). Returns a serializable
/// [`DetachHandle`] carrying the target's [`ProcessId`], its Linux systemd scope unit name when
/// a scope was used, and any non-fatal launch warnings.
pub async fn spawn_detached(options: DetachOptions) -> Result<DetachHandle> {
    spawn_detached_for_test(options, ScopeProbeOverride::Auto).await
}

/// Test-only entry point for [`spawn_detached`] that allows forcing the Linux scope-selection
/// branch without mutating process-wide environment variables.
#[doc(hidden)]
pub async fn spawn_detached_for_test(
    options: DetachOptions,
    scope_override: ScopeProbeOverride,
) -> Result<DetachHandle> {
    let resolved = shell::resolve(&options.command)?;
    validate_scope_name(options.name.as_deref())?;

    if let Some(ref cwd) = options.cwd {
        let meta = tokio::fs::metadata(cwd)
            .await
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => OsError::NotFound {
                    detail: format!("working directory not found: {}", cwd.display()),
                },
                _ => OsError::from_io(
                    format!("cannot access working directory {}", cwd.display()),
                    err,
                ),
            })?;
        if !meta.is_dir() {
            return Err(OsError::InvalidArgument {
                detail: format!("working directory is not a directory: {}", cwd.display()),
            });
        }
    }

    spawn_detached_impl(&resolved, &options, scope_override).await
}

/// Return `true` while any process started under `handle` is still running—not just the root.
///
/// Strictly read-only and non-blocking on async worker threads (designed to be cheap when listing
/// many jobs):
/// - **Linux with a systemd user scope (`handle.systemd_unit`):** checks the root process first,
///   then reads the scope's cgroup (`cgroup.procs` / `cgroup.events`) directly under `/sys/fs/cgroup`
///   when available, falling back to `systemctl --user show` on non-standard cgroup layouts.
/// - **Other Unix (or `setsid` fallback):** returns `true` if `handle.process` is alive or if any
///   live process still has `handle.process.pid` as its session ID or process group ID and started
///   at or after `handle.process.start_time` (the same rule [`procid::kill_tree`] uses).
/// - **Windows:** returns `true` if `handle.process` is alive or if a verified read-only process
///   tree walk finds any live descendant whose handle confirms `parent_pid` and creation time.
pub async fn detached_active(handle: &DetachHandle) -> Result<bool> {
    if handle.systemd_unit.is_some() {
        return detached_active_impl(handle).await;
    }
    procid::tree_is_active(&handle.process).await
}

/// Stop a detached process and its entire tree using the boundary recorded in `handle`.
///
/// When `handle.systemd_unit` names a Linux transient systemd user scope, termination goes through
/// `systemctl --user kill`/`stop` on that scope so even descendants that called `setsid` are
/// stopped inside the cgroup. Otherwise falls back to [`procid::kill_tree`]. Returns only once
/// every process is gone or reports any survivors.
pub async fn stop_detached(handle: &DetachHandle, grace: Duration) -> Result<KillTreeReport> {
    if handle.systemd_unit.is_some() {
        return stop_detached_impl(handle, grace).await;
    }
    procid::kill_tree(&handle.process, grace).await
}

/// Validate an optional caller-supplied job name for systemd scope naming.
pub(crate) fn validate_scope_name(name: Option<&str>) -> Result<()> {
    let Some(name) = name else {
        return Ok(());
    };
    if name.trim().is_empty() {
        return Err(OsError::InvalidArgument {
            detail: "detached job name must not be empty".into(),
        });
    }
    Ok(())
}

/// Sanitize a caller-supplied job name into a bounded ASCII systemd unit-name prefix.
#[allow(dead_code)]
pub(crate) fn sanitize_scope_prefix(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_SCOPE_PREFIX_LEN));
    let mut prev_dash = false;
    for ch in raw.chars() {
        if out.len() >= MAX_SCOPE_PREFIX_LEN {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with(['-', '_']) {
        out.pop();
    }
    out
}

/// Parse the `populated` flag (`0` or `1`) from a Linux cgroup v2 `cgroup.events` file.
#[allow(dead_code)]
pub(crate) fn parse_cgroup_events_populated(contents: &str) -> Option<bool> {
    for line in contents.lines() {
        if let Some(rest) = line.trim().strip_prefix("populated ") {
            match rest.trim() {
                "1" => return Some(true),
                "0" => return Some(false),
                _ => {}
            }
        }
    }
    None
}

/// Return `true` when any cgroup path in `/proc/<pid>/cgroup` ends with `/<full_unit>`
/// (tolerating the ` (deleted)` suffix Linux appends after `--collect` removes an empty scope
/// for an already-exited unreaped child).
#[allow(dead_code)]
pub(crate) fn cgroup_matches_unit(contents: &str, full_unit: &str) -> bool {
    let suffix = format!("/{full_unit}");
    for line in contents.lines() {
        let mut parts = line.splitn(3, ':');
        let Some(path_raw) = parts.nth(2) else {
            continue;
        };
        let trimmed = path_raw.trim();
        let clean = trimmed.strip_suffix(" (deleted)").unwrap_or(trimmed);
        if clean.ends_with(&suffix) {
            return true;
        }
    }
    false
}

/// Warning recorded on [`DetachHandle::warnings`] when Linux `systemd --user` is reachable but
/// lingering is disabled for the current account.
#[allow(dead_code)]
pub(crate) const LINGER_DISABLED_WARNING: &str = "systemd user lingering is disabled for this account, so the process was detached with setsid instead of a systemd user scope and may be stopped on logout; run `loginctl enable-linger` to keep user scopes alive across sessions";

/// Return `true` when `linger_dir/<username>` exists as a file.
#[allow(dead_code)]
pub(crate) fn linger_file_exists(linger_dir: &Path, username: &str) -> bool {
    !username.is_empty()
        && !username.contains('/')
        && !username.contains('\\')
        && username != "."
        && username != ".."
        && linger_dir.join(username).is_file()
}

/// Parse `loginctl show-user --property=Linger` output (`Linger=yes`, `Linger=no`, `yes`, or `no`).
#[allow(dead_code)]
pub(crate) fn parse_loginctl_linger(output: &str) -> Option<bool> {
    for line in output.lines() {
        let trimmed = line.trim();
        let value = trimmed
            .strip_prefix("Linger=")
            .unwrap_or(trimmed)
            .trim()
            .to_ascii_lowercase();
        match value.as_str() {
            "yes" | "true" | "1" => return Some(true),
            "no" | "false" | "0" => return Some(false),
            _ => {}
        }
    }
    None
}

/// Build the `(unit_flag_value, full_scope_unit)` pair for `systemd-run --user --scope --unit=...`.
///
/// Always appends a unique suffix so concurrent or repeated launches with the same caller label
/// never collide on an active or failed unit name.
#[allow(dead_code)]
pub(crate) fn resolve_scope_unit_name(name: Option<&str>) -> Result<(String, String)> {
    validate_scope_name(name)?;
    let pid = std::process::id();
    let seq = SCOPE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let unique = format!("{pid}-{seq}-{nanos:x}");

    let prefix = name.map(sanitize_scope_prefix).unwrap_or_default();
    let stem = if prefix.is_empty() {
        format!("legion-job-{unique}")
    } else {
        format!("legion-job-{prefix}-{unique}")
    };
    let full = format!("{stem}.scope");
    Ok((stem, full))
}

/// Open append-mode [`Stdio`] handles for stdout and stderr, sharing a single open file
/// description when both target the same path.
#[allow(dead_code)]
pub(crate) fn open_append_stdio(
    stdout_path: Option<&Path>,
    stderr_path: Option<&Path>,
) -> Result<(Stdio, Stdio)> {
    match (stdout_path, stderr_path) {
        (Some(out), Some(err)) if out == err => {
            let file = open_append_file(out)?;
            let cloned = file.try_clone().map_err(|io_err| {
                OsError::from_io(
                    format!("cannot duplicate handle for {}", out.display()),
                    io_err,
                )
            })?;
            Ok((Stdio::from(file), Stdio::from(cloned)))
        }
        (out_opt, err_opt) => {
            let stdout = match out_opt {
                Some(path) => Stdio::from(open_append_file(path)?),
                None => Stdio::null(),
            };
            let stderr = match err_opt {
                Some(path) => Stdio::from(open_append_file(path)?),
                None => Stdio::null(),
            };
            Ok((stdout, stderr))
        }
    }
}

fn open_append_file(path: &Path) -> Result<std::fs::File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| OsError::from_io(format!("cannot open log file {}", path.display()), err))
}

#[cfg(unix)]
async fn spawn_detached_impl(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
    scope_override: ScopeProbeOverride,
) -> Result<DetachHandle> {
    platform::spawn_detached(resolved, options, scope_override).await
}

#[cfg(windows)]
async fn spawn_detached_impl(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
    _scope_override: ScopeProbeOverride,
) -> Result<DetachHandle> {
    platform::spawn_detached(resolved, options).await
}

#[cfg(not(any(unix, windows)))]
async fn spawn_detached_impl(
    _resolved: &ResolvedCommand,
    _options: &DetachOptions,
    _scope_override: ScopeProbeOverride,
) -> Result<DetachHandle> {
    Err(OsError::Unsupported {
        detail: "detached process launching is not supported on this platform".into(),
    })
}

#[cfg(unix)]
async fn detached_active_impl(handle: &DetachHandle) -> Result<bool> {
    platform::detached_active(handle).await
}

#[cfg(not(unix))]
async fn detached_active_impl(handle: &DetachHandle) -> Result<bool> {
    procid::tree_is_active(&handle.process).await
}

#[cfg(unix)]
async fn stop_detached_impl(handle: &DetachHandle, grace: Duration) -> Result<KillTreeReport> {
    platform::stop_detached(handle, grace).await
}

#[cfg(not(unix))]
async fn stop_detached_impl(handle: &DetachHandle, grace: Duration) -> Result<KillTreeReport> {
    procid::kill_tree(&handle.process, grace).await
}

/// Build the PowerShell launcher script executed by WMI `Win32_Process.Create`.
///
/// The launcher starts the target via `System.Diagnostics.Process` with stdin closed, applies
/// the merged caller environment (since WMI spawns under the service environment rather than the
/// caller's), writes the target's real PID and 100-ns `FILETIME` creation timestamp atomically to
/// `handoff_path`, and pumps raw stdout/stderr streams unbuffered into append-mode `FileStream`s.
/// When `stdout_path` and `stderr_path` target the same file, a single synchronized `FileStream`
/// is shared by both streams so concurrent writes never overwrite each other.
#[allow(dead_code, clippy::too_many_arguments)]
pub(crate) fn build_wmi_launcher_script(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env_remove: &[String],
    env: &[(String, String)],
    stdout_path: Option<&str>,
    stderr_path: Option<&str>,
    handoff_path: &str,
    self_script_path: Option<&str>,
) -> String {
    let escaped_args: String = args
        .iter()
        .map(|arg| windows_cmdline_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let shared_output = match (stdout_path, stderr_path) {
        (Some(out), Some(err)) => same_windows_path(out, err),
        _ => false,
    };

    let mut lines = vec!["$ErrorActionPreference = 'Stop'".to_owned()];
    if let Some(script_path) = self_script_path {
        // PowerShell parses the full `.ps1` before running line 1, so deleting the temporary
        // launcher script immediately avoids leaving it behind even if the target runs for days
        // or is terminated abruptly.
        lines.push(format!(
            "Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue",
            powershell_single_quote(script_path)
        ));
    }
    lines.extend([
        format!("$handoffPath = {}", powershell_single_quote(handoff_path)),
        "$handoffTmp = $handoffPath + '.tmp'".to_owned(),
        "$psi = New-Object System.Diagnostics.ProcessStartInfo".to_owned(),
        format!("$psi.FileName = {}", powershell_single_quote(program)),
        format!(
            "$psi.Arguments = {}",
            powershell_single_quote(&escaped_args)
        ),
    ]);

    if let Some(dir) = cwd {
        lines.push(format!(
            "$psi.WorkingDirectory = {}",
            powershell_single_quote(dir)
        ));
    }

    lines.extend([
        "$psi.UseShellExecute = $false".to_owned(),
        "$psi.CreateNoWindow = $true".to_owned(),
        "$psi.RedirectStandardInput = $true".to_owned(),
        "$psi.RedirectStandardOutput = $true".to_owned(),
        "$psi.RedirectStandardError = $true".to_owned(),
    ]);

    for key in env_remove {
        lines.push(format!(
            "$psi.EnvironmentVariables.Remove({})",
            powershell_single_quote(key)
        ));
    }
    for (key, value) in env {
        lines.push(format!(
            "$psi.EnvironmentVariables[{}] = {}",
            powershell_single_quote(key),
            powershell_single_quote(value)
        ));
    }

    lines.extend([
        "$outStream = $null".to_owned(),
        "$errStream = $null".to_owned(),
        "$proc = $null".to_owned(),
        "try {".to_owned(),
    ]);

    if shared_output {
        if let Some(path) = stdout_path {
            lines.push(format!(
                "  $rawShared = New-Object System.IO.FileStream({}, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete), 1, [System.IO.FileOptions]::WriteThrough)",
                powershell_single_quote(path)
            ));
            lines.push("  $outStream = [System.IO.Stream]::Synchronized($rawShared)".to_owned());
            lines.push("  $errStream = $outStream".to_owned());
        }
    } else {
        lines.push(match stdout_path {
            Some(path) => format!(
                "  $outStream = New-Object System.IO.FileStream({}, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete), 1, [System.IO.FileOptions]::WriteThrough)",
                powershell_single_quote(path)
            ),
            None => "  $outStream = [System.IO.Stream]::Null".to_owned(),
        });
        lines.push(match stderr_path {
            Some(path) => format!(
                "  $errStream = New-Object System.IO.FileStream({}, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete), 1, [System.IO.FileOptions]::WriteThrough)",
                powershell_single_quote(path)
            ),
            None => "  $errStream = [System.IO.Stream]::Null".to_owned(),
        });
    }

    lines.extend([
        "  $proc = [System.Diagnostics.Process]::Start($psi)".to_owned(),
        "  $proc.StandardInput.Close()".to_owned(),
        "  [System.IO.File]::WriteAllText($handoffTmp, (\"{0}:{1}\" -f $proc.Id, $proc.StartTime.ToFileTimeUtc()))".to_owned(),
        "  [System.IO.File]::Move($handoffTmp, $handoffPath)".to_owned(),
        "  $outTask = $proc.StandardOutput.BaseStream.CopyToAsync($outStream, 4096)".to_owned(),
        "  $errTask = $proc.StandardError.BaseStream.CopyToAsync($errStream, 4096)".to_owned(),
        "  $proc.WaitForExit()".to_owned(),
        "  [System.Threading.Tasks.Task]::WaitAll([System.Threading.Tasks.Task[]]@($outTask, $errTask))".to_owned(),
        "  exit $proc.ExitCode".to_owned(),
        "} catch {".to_owned(),
        "  if ($null -eq $proc) {".to_owned(),
        "    try {".to_owned(),
        "      [System.IO.File]::WriteAllText($handoffTmp, ('ERR:' + $_.Exception.Message))".to_owned(),
        "      [System.IO.File]::Move($handoffTmp, $handoffPath)".to_owned(),
        "    } catch {}".to_owned(),
        "  }".to_owned(),
        "  exit 1".to_owned(),
        "} finally {".to_owned(),
        "  if ($null -ne $outStream) { $outStream.Dispose() }".to_owned(),
    ]);

    if !shared_output {
        lines.push("  if ($null -ne $errStream) { $errStream.Dispose() }".to_owned());
    }
    if let Some(script_path) = self_script_path {
        lines.push(format!(
            "  Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue",
            powershell_single_quote(script_path)
        ));
    }
    lines.push("}".to_owned());

    lines.join("\n")
}

/// Compare two Windows file paths case-insensitively, treating `/` and `\` as equivalent.
#[allow(dead_code)]
pub(crate) fn same_windows_path(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(b)
}

/// Merge the caller's environment with `env_remove` exclusions and `overrides` using Windows
/// case-insensitive variable name semantics.
#[allow(dead_code)]
pub(crate) fn merge_windows_env(
    inherited: impl IntoIterator<Item = (String, String)>,
    env_remove: &[String],
    overrides: &[(String, String)],
) -> Vec<(String, String)> {
    let removed_lower: Vec<String> = env_remove.iter().map(|k| k.to_ascii_lowercase()).collect();
    let mut merged: BTreeMap<String, (String, String)> = BTreeMap::new();

    for (key, value) in inherited {
        let lower = key.to_ascii_lowercase();
        if removed_lower.contains(&lower) {
            continue;
        }
        merged.insert(lower, (key, value));
    }
    for (key, value) in overrides {
        let lower = key.to_ascii_lowercase();
        merged.insert(lower, (key.clone(), value.clone()));
    }
    merged.into_values().collect()
}

/// Parse the target identity handoff file written by the WMI launcher (`<pid>:<filetime_ticks>`
/// or `ERR:<message>`).
#[allow(dead_code)]
pub(crate) fn parse_wmi_handoff(contents: &str) -> Result<ProcessId> {
    let trimmed = contents.trim();
    if let Some(err_msg) = trimmed.strip_prefix("ERR:") {
        return Err(OsError::Platform {
            detail: format!("WMI detached target failed to start: {}", err_msg.trim()),
        });
    }
    let Some((pid_str, start_str)) = trimmed.split_once(':') else {
        return Err(OsError::Platform {
            detail: format!("malformed WMI handoff payload: {trimmed:?}"),
        });
    };
    let pid = pid_str
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&p| p > 0)
        .ok_or_else(|| OsError::Platform {
            detail: format!("invalid target PID in WMI handoff: {pid_str:?}"),
        })?;
    let start_time = start_str
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|&t| t > 0)
        .ok_or_else(|| OsError::Platform {
            detail: format!("invalid creation time in WMI handoff: {start_str:?}"),
        })?;
    Ok(ProcessId::new(pid, start_time))
}

/// One hour expressed in Windows 100-ns `FILETIME` ticks, used to tolerate exact DST-transition
/// ambiguity between `.NET` `Process.StartTime.ToFileTimeUtc()` and Win32 `GetProcessTimes`.
#[allow(dead_code)]
const ONE_HOUR_FILETIME_TICKS: u64 = 3_600 * 10_000_000;

/// Reconcile the target identity reported by the WMI launcher with a direct UTC `GetProcessTimes`
/// lookup.
///
/// Accepts `queried` only when it matches `reported` exactly or differs by exactly one hour
/// (the local-time DST fold ambiguity in `.NET`'s `Process.StartTime`); any other discrepancy
/// fails instead of silently accepting a recycled PID. Returns `reported` when `queried` is
/// `None` (the target already exited before our query ran).
#[allow(dead_code)]
pub(crate) fn reconcile_wmi_identity(
    reported: ProcessId,
    queried: Option<ProcessId>,
) -> Result<ProcessId> {
    let Some(exact) = queried else {
        return Ok(reported);
    };
    if exact.pid == reported.pid
        && (exact.start_time == reported.start_time
            || exact.start_time.abs_diff(reported.start_time) == ONE_HOUR_FILETIME_TICKS)
    {
        return Ok(exact);
    }
    Err(OsError::Platform {
        detail: format!(
            "WMI detached target identity mismatch for pid {} (reported start_time={}, queried pid={}, start_time={})",
            reported.pid, reported.start_time, exact.pid, exact.start_time
        ),
    })
}

/// Resolve a bare or relative executable name against `cwd` and the caller's forwarded `PATH` +
/// `PATHEXT` entries in `merged_env` before building the WMI launcher script.
///
/// Because `wmiprvse.exe` runs under the service environment rather than the caller's, `.NET`
/// `Process::Start` would otherwise search the service `PATH` instead of the caller's.
#[allow(dead_code)]
pub(crate) fn resolve_wmi_program(
    program: &str,
    cwd: &Path,
    merged_env: &[(String, String)],
) -> String {
    let is_absolute = program.starts_with(['\\', '/'])
        || (program.len() >= 3
            && program.as_bytes()[0].is_ascii_alphabetic()
            && program.as_bytes()[1] == b':'
            && matches!(program.as_bytes()[2], b'\\' | b'/'));
    if is_absolute {
        return program.to_owned();
    }

    let pathext_val = merged_env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATHEXT"))
        .map(|(_, v)| std::ffi::OsStr::new(v.as_str()));
    let exts = shell::parse_pathext(pathext_val);

    let has_separator = program.contains(['\\', '/']);
    if has_separator {
        let base = cwd.join(program);
        if let Some(found) = probe_executable_with_exts(&base, &exts) {
            return found.to_string_lossy().into_owned();
        }
        return base.to_string_lossy().into_owned();
    }

    if let Some((_, path_val)) = merged_env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
    {
        for dir in split_windows_path_entries(path_val) {
            let candidate = Path::new(dir).join(program);
            if let Some(found) = probe_executable_with_exts(&candidate, &exts) {
                return found.to_string_lossy().into_owned();
            }
        }
    }

    let cwd_candidate = cwd.join(program);
    if let Some(found) = probe_executable_with_exts(&cwd_candidate, &exts) {
        return found.to_string_lossy().into_owned();
    }

    program.to_owned()
}

#[allow(dead_code)]
fn split_windows_path_entries(path_var: &str) -> impl Iterator<Item = &str> {
    path_var
        .split(';')
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty())
}

#[allow(dead_code)]
fn probe_executable_with_exts(base: &Path, exts: &[String]) -> Option<PathBuf> {
    let has_ext = base.extension().is_some();
    if (has_ext || exts.is_empty()) && base.is_file() {
        return Some(resolve_on_disk_filename(base));
    }
    if !has_ext {
        let raw = base.to_string_lossy();
        for ext in exts {
            let candidate = PathBuf::from(format!("{raw}{ext}"));
            if candidate.is_file() {
                return Some(resolve_on_disk_filename(&candidate));
            }
            // Support case-sensitive filesystems (for example cross-platform unit tests on Linux)
            // by also checking the lowercase extension when `PATHEXT` contains uppercase entries.
            let lower_ext = ext.to_ascii_lowercase();
            if lower_ext != *ext {
                let lower_candidate = PathBuf::from(format!("{raw}{lower_ext}"));
                if lower_candidate.is_file() {
                    return Some(resolve_on_disk_filename(&lower_candidate));
                }
            }
        }
    }
    None
}

/// Replace the final path component with its actual on-disk spelling when the parent directory is
/// readable, avoiding uppercase `PATHEXT` suffixes (such as `mytool.EXE`) on case-insensitive
/// filesystems without introducing Windows `\\?\` verbatim prefixes from `canonicalize`.
#[allow(dead_code)]
fn resolve_on_disk_filename(path: &Path) -> PathBuf {
    let (Some(parent), Some(target_name)) = (path.parent(), path.file_name()) else {
        return path.to_path_buf();
    };
    let target_str = target_name.to_string_lossy();
    let mut ci_match = None;
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            if entry_name == target_name {
                return parent.join(entry_name);
            }
            if ci_match.is_none()
                && entry_name
                    .to_string_lossy()
                    .eq_ignore_ascii_case(&target_str)
            {
                ci_match = Some(entry_name);
            }
        }
    }
    match ci_match {
        Some(name) => parent.join(name),
        None => path.to_path_buf(),
    }
}

/// Refuse `.cmd`/`.bat` targets on the Windows WMI path unless every argument is free of `cmd.exe`
/// metacharacters.
///
/// Launching a batch file through `CreateProcessW` implicitly routes the command line through
/// `cmd.exe /c`, where characters like `&`, `|`, `<`, `>`, `^`, `%`, `!`, `"`, or newlines can
/// inject unintended commands despite MSVCRT quoting.
#[allow(dead_code)]
pub(crate) fn validate_wmi_batch_target(program: &str, args: &[String]) -> Result<()> {
    let trimmed = program.trim().trim_end_matches(['.', ' ']);
    let lower = trimmed.to_ascii_lowercase();
    let is_batch = lower.ends_with(".cmd") || lower.ends_with(".bat");
    if !is_batch {
        return Ok(());
    }

    for (idx, arg) in args.iter().enumerate() {
        if !is_safe_cmd_batch_arg(arg) {
            return Err(OsError::InvalidArgument {
                detail: format!(
                    "refusing to launch batch file {program:?} via WMI because argument {idx} contains cmd.exe metacharacters; invoke the underlying executable directly or use CommandSpec::Shell"
                ),
            });
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn is_safe_cmd_batch_arg(arg: &str) -> bool {
    if arg.ends_with('\\') {
        return false;
    }
    !arg.chars().any(|ch| {
        matches!(
            ch,
            '\0' | '\r'
                | '\n'
                | '&'
                | '|'
                | '<'
                | '>'
                | '^'
                | '%'
                | '!'
                | '"'
                | '('
                | ')'
                | ';'
                | ','
                | '='
        )
    })
}

/// Build the outer PowerShell script that invokes `Win32_Process.Create` via `Invoke-CimMethod`
/// for a pre-built `command_line` and prints the created wrapper PID to stdout.
#[allow(dead_code)]
pub(crate) fn build_wmi_create_script_for_cmdline(
    command_line: &str,
    cwd: Option<&str>,
) -> Result<String> {
    if command_line.len() > WINDOWS_WMI_MAX_CMDLINE {
        return Err(OsError::InvalidArgument {
            detail: format!(
                "detached WMI command line length ({}) exceeds Windows limit ({WINDOWS_WMI_MAX_CMDLINE})",
                command_line.len()
            ),
        });
    }

    let mut lines = vec![
        "$ErrorActionPreference = 'Stop'".to_owned(),
        format!(
            "$cimArgs = @{{ CommandLine = {} }}",
            powershell_single_quote(command_line)
        ),
    ];
    if let Some(dir) = cwd {
        lines.push(format!(
            "$cimArgs['CurrentDirectory'] = {}",
            powershell_single_quote(dir)
        ));
    }
    lines.extend([
        "$created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments $cimArgs"
            .to_owned(),
        "if ($created.ReturnValue -ne 0 -or $created.ProcessId -le 0) { throw \"Win32_Process.Create failed with ReturnValue=$($created.ReturnValue)\" }"
            .to_owned(),
        "[Console]::Out.Write([string]$created.ProcessId)".to_owned(),
    ]);
    Ok(lines.join("\n"))
}

/// Build the `-EncodedCommand` command line for a launcher script and wrap it in a WMI
/// `Invoke-CimMethod` creation script.
#[allow(dead_code)]
pub(crate) fn build_wmi_create_script(launcher_script: &str, cwd: Option<&str>) -> Result<String> {
    let encoded = encode_utf16le_base64(launcher_script);
    let command_line = format!(
        "powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}"
    );
    build_wmi_create_script_for_cmdline(&command_line, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmi_scripts_encode_arguments_env_and_log_paths_safely() {
        let launcher = build_wmi_launcher_script(
            r"C:\Users\me\O'Brien\worker.exe",
            &["--flag".into(), "value with spaces".into()],
            Some(r"C:\Users\me\work"),
            &["PSModulePath".into()],
            &[("LEGION_KEY".into(), "it's ok".into())],
            Some(r"C:\Users\me\out.log"),
            Some(r"C:\Users\me\err.log"),
            r"C:\Users\me\handoff.pid",
            None,
        );
        assert!(launcher.contains(r"'C:\Users\me\O''Brien\worker.exe'"));
        assert!(launcher.contains(r#"--flag "value with spaces""#));
        assert!(launcher.contains("$psi.EnvironmentVariables.Remove('PSModulePath')"));
        assert!(launcher.contains("$psi.EnvironmentVariables['LEGION_KEY'] = 'it''s ok'"));
        assert!(launcher.contains("[System.IO.FileMode]::Append"));
        assert!(launcher.contains("$proc.Id, $proc.StartTime.ToFileTimeUtc()"));
        assert!(launcher.contains("[System.Threading.Tasks.Task[]]@($outTask, $errTask)"));
        assert_eq!(
            launcher.matches("New-Object System.IO.FileStream").count(),
            2
        );

        let cim = build_wmi_create_script(&launcher, Some(r"C:\Users\me\work")).unwrap();
        assert!(cim.contains("Invoke-CimMethod -ClassName Win32_Process -MethodName Create"));
        assert!(cim.contains("$cimArgs['CurrentDirectory'] = 'C:\\Users\\me\\work'"));
        assert!(!cim.contains("$args ="));
    }

    #[test]
    fn wmi_launcher_shares_single_synchronized_stream_when_stdout_equals_stderr() {
        let launcher = build_wmi_launcher_script(
            r"C:\Users\me\worker.exe",
            &[],
            None,
            &[],
            &[],
            Some(r"C:\Users\me\combined.log"),
            Some("c:/users/me/COMBINED.LOG"),
            r"C:\Users\me\handoff.pid",
            Some(r"C:\Users\me\temp.ps1"),
        );
        assert_eq!(
            launcher.matches("New-Object System.IO.FileStream").count(),
            1
        );
        assert!(launcher.contains("[System.IO.Stream]::Synchronized($rawShared)"));
        assert!(launcher.contains("$errStream = $outStream"));
        assert!(!launcher.contains("$errStream.Dispose()"));
        let mut lines = launcher.lines();
        assert_eq!(lines.next(), Some("$ErrorActionPreference = 'Stop'"));
        assert_eq!(
            lines.next(),
            Some(
                "Remove-Item -LiteralPath 'C:\\Users\\me\\temp.ps1' -Force -ErrorAction SilentlyContinue"
            )
        );
    }

    #[test]
    fn wmi_identity_reconciliation_accepts_exact_and_dst_hour_only() {
        let reported = ProcessId::new(500, 133_700_000_000_000_000);
        // Exact match accepted.
        assert_eq!(
            reconcile_wmi_identity(reported, Some(reported)).unwrap(),
            reported
        );
        // Exact +1h and -1h DST shift accepted, returning the direct UTC queried identity.
        let plus_1h = ProcessId::new(500, reported.start_time + ONE_HOUR_FILETIME_TICKS);
        assert_eq!(
            reconcile_wmi_identity(reported, Some(plus_1h)).unwrap(),
            plus_1h
        );
        let minus_1h = ProcessId::new(500, reported.start_time - ONE_HOUR_FILETIME_TICKS);
        assert_eq!(
            reconcile_wmi_identity(reported, Some(minus_1h)).unwrap(),
            minus_1h
        );
        // Exited before query (`None`) falls back to reported.
        assert_eq!(reconcile_wmi_identity(reported, None).unwrap(), reported);
        // Arbitrary recycled PID start time must fail.
        let recycled = ProcessId::new(500, reported.start_time + 5_000_000);
        assert!(matches!(
            reconcile_wmi_identity(reported, Some(recycled)),
            Err(OsError::Platform { .. })
        ));
        let off_by_1h_plus_1 =
            ProcessId::new(500, reported.start_time + ONE_HOUR_FILETIME_TICKS + 1);
        assert!(matches!(
            reconcile_wmi_identity(reported, Some(off_by_1h_plus_1)),
            Err(OsError::Platform { .. })
        ));
    }

    #[test]
    fn wmi_program_resolution_and_batch_argument_validation() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("tools");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let exe_path = bin_dir.join("mytool.exe");
        let cmd_path = bin_dir.join("shim.cmd");
        std::fs::write(&exe_path, b"").unwrap();
        std::fs::write(&cmd_path, b"@echo off\r\n").unwrap();

        let merged_env = vec![
            ("Path".to_owned(), bin_dir.to_string_lossy().into_owned()),
            ("PATHEXT".to_owned(), ".EXE;.CMD;.BAT".to_owned()),
        ];

        let resolved_exe = resolve_wmi_program("mytool", dir.path(), &merged_env);
        assert!(
            same_windows_path(&resolved_exe, &exe_path.to_string_lossy()),
            "expected {}, got {resolved_exe}",
            exe_path.display()
        );
        assert_eq!(
            Path::new(&resolved_exe).file_name(),
            Some(std::ffi::OsStr::new("mytool.exe"))
        );

        let resolved_shim = resolve_wmi_program("shim", dir.path(), &merged_env);
        assert!(
            same_windows_path(&resolved_shim, &cmd_path.to_string_lossy()),
            "expected {}, got {resolved_shim}",
            cmd_path.display()
        );
        assert_eq!(
            Path::new(&resolved_shim).file_name(),
            Some(std::ffi::OsStr::new("shim.cmd"))
        );

        // Even when probed with uppercase `PATHEXT` casing, on-disk filename spelling is returned.
        assert_eq!(
            resolve_on_disk_filename(&bin_dir.join("MYTOOL.EXE")),
            exe_path
        );

        // Safe args to a `.cmd` file are allowed; cmd.exe metacharacters are refused.
        assert!(
            validate_wmi_batch_target(&resolved_shim, &["--flag".into(), "plain arg".into()])
                .is_ok()
        );
        for dangerous in [
            "a&calc",
            "a|b",
            "a>out",
            "%TEMP%",
            "!VAR!",
            "quote\"here",
            "line1\nline2",
            "caret^escape",
            "trailing\\",
        ] {
            assert!(
                matches!(
                    validate_wmi_batch_target(&resolved_shim, &[dangerous.to_owned()]),
                    Err(OsError::InvalidArgument { .. })
                ),
                "expected batch rejection for arg {dangerous:?}"
            );
        }

        // Non-batch executables allow arbitrary arguments without restriction.
        assert!(
            validate_wmi_batch_target(&exe_path.to_string_lossy(), &["a&b|c%d\"e".to_owned()])
                .is_ok()
        );
    }

    #[test]
    fn wmi_env_merge_and_handoff_parsing_handle_overrides_and_errors() {
        let merged = merge_windows_env(
            [
                ("Path".into(), r"C:\Windows".into()),
                (
                    "PSModulePath".into(),
                    r"C:\Program Files\PowerShell\7".into(),
                ),
                ("LEGION_HOME".into(), r"C:\Users\me\old".into()),
            ],
            &["psmodulepath".into()],
            &[
                ("legion_home".into(), r"C:\Users\me\new".into()),
                ("EXTRA".into(), "1".into()),
            ],
        );
        assert!(
            merged
                .iter()
                .all(|(k, _)| !k.eq_ignore_ascii_case("PSModulePath"))
        );
        assert!(
            merged
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("PATH") && v == r"C:\Windows")
        );
        assert!(
            merged
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case("LEGION_HOME") && v == r"C:\Users\me\new")
        );
        assert!(merged.iter().any(|(k, v)| k == "EXTRA" && v == "1"));

        let parsed = parse_wmi_handoff("4321:133700000000000001\n").unwrap();
        assert_eq!(parsed, ProcessId::new(4321, 133_700_000_000_000_001));

        let err = parse_wmi_handoff("ERR:The system cannot find the file specified").unwrap_err();
        assert!(matches!(err, OsError::Platform { .. }));
        assert!(parse_wmi_handoff("not-a-handoff").is_err());
        assert!(parse_wmi_handoff("0:12345").is_err());
    }

    #[test]
    fn wmi_create_script_rejects_oversized_payload() {
        let huge = "a".repeat(20_000);
        let err = build_wmi_create_script(&huge, None).unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));
    }

    #[test]
    fn detach_handle_serializes_cleanly_for_job_records() {
        let with_scope = DetachHandle {
            process: ProcessId::new(9001, 42_000),
            systemd_unit: Some("legion-job-build-1.scope".into()),
            warnings: Vec::new(),
        };
        let json = serde_json::to_string(&with_scope).unwrap();
        assert!(json.contains("\"systemd_unit\":\"legion-job-build-1.scope\""));
        assert!(!json.contains("\"warnings\""));
        let decoded: DetachHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, with_scope);

        let without_scope = DetachHandle {
            process: ProcessId::new(9002, 42_001),
            systemd_unit: None,
            warnings: vec!["lingering disabled".into()],
        };
        let json2 = serde_json::to_string(&without_scope).unwrap();
        assert!(!json2.contains("systemd_unit"));
        let decoded2: DetachHandle = serde_json::from_str(&json2).unwrap();
        assert_eq!(decoded2, without_scope);
    }

    #[test]
    fn scope_unit_name_sanitizes_limits_and_uniquifies_caller_names() {
        let (stem1, full1) = resolve_scope_unit_name(Some("job_42.run-1")).unwrap();
        let (stem2, full2) = resolve_scope_unit_name(Some("job_42.run-1")).unwrap();
        assert_ne!(stem1, stem2, "repeated names must produce distinct units");
        assert_ne!(full1, full2);
        assert!(stem1.starts_with("legion-job-job_42-run-1-"));
        assert!(full1.ends_with(".scope"));

        // Invalid path/space characters are sanitized into safe unit-name prefixes.
        let (san_stem, _) = resolve_scope_unit_name(Some("  my/bad name:v2!!  ")).unwrap();
        assert!(san_stem.starts_with("legion-job-my-bad-name-v2-"));
        assert!(
            san_stem
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        );

        // Overlong caller names are truncated to a bounded prefix before appending the suffix.
        let long = "a".repeat(200);
        let (long_stem, long_full) = resolve_scope_unit_name(Some(&long)).unwrap();
        assert!(long_full.len() < 128, "unit name too long: {long_full}");
        assert!(
            long_stem.starts_with(&format!("legion-job-{}-", "a".repeat(MAX_SCOPE_PREFIX_LEN)))
        );

        // Empty or whitespace-only caller names are rejected.
        assert!(matches!(
            resolve_scope_unit_name(Some("")),
            Err(OsError::InvalidArgument { .. })
        ));
        assert!(matches!(
            resolve_scope_unit_name(Some("   ")),
            Err(OsError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn cgroup_matches_unit_handles_live_deleted_and_non_matching_entries() {
        let unit = "legion-job-demo-123-0-abc.scope";
        assert!(cgroup_matches_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/legion-job-demo-123-0-abc.scope\n",
            unit
        ));
        assert!(cgroup_matches_unit(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/legion-job-demo-123-0-abc.scope (deleted)\n",
            unit
        ));
        assert!(cgroup_matches_unit(
            "2:cpu:/\n1:name=systemd:/user.slice/legion-job-demo-123-0-abc.scope\n",
            unit
        ));
        // Must require a leading `/` boundary so a different unit sharing the suffix does not match.
        assert!(!cgroup_matches_unit(
            "0::/user.slice/other-legion-job-demo-123-0-abc.scope\n",
            unit
        ));
        assert!(!cgroup_matches_unit(
            "0::/user.slice/user-1000.slice/session-2.scope\n",
            unit
        ));

        assert_eq!(
            parse_cgroup_events_populated("populated 1\nfrozen 0\n"),
            Some(true)
        );
        assert_eq!(
            parse_cgroup_events_populated("populated 0\nfrozen 0\n"),
            Some(false)
        );
        assert_eq!(parse_cgroup_events_populated("frozen 0\n"), None);
    }

    #[tokio::test]
    async fn detached_active_reports_false_for_absent_handle() {
        let absent = DetachHandle {
            process: ProcessId::new(i32::MAX as u32, 1),
            systemd_unit: None,
            warnings: Vec::new(),
        };
        assert!(!detached_active(&absent).await.unwrap());

        let absent_scope = DetachHandle {
            process: ProcessId::new(i32::MAX as u32, 1),
            systemd_unit: Some("legion-job-definitely-absent-0.scope".into()),
            warnings: Vec::new(),
        };
        assert!(!detached_active(&absent_scope).await.unwrap());
    }

    #[test]
    fn linger_checks_detect_enabled_and_disabled_states() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!linger_file_exists(dir.path(), "me"));
        std::fs::write(dir.path().join("me"), b"").unwrap();
        assert!(linger_file_exists(dir.path(), "me"));
        assert!(!linger_file_exists(dir.path(), "../me"));
        assert!(!linger_file_exists(dir.path(), ""));

        assert_eq!(parse_loginctl_linger("Linger=yes\n"), Some(true));
        assert_eq!(parse_loginctl_linger("Linger=no\n"), Some(false));
        assert_eq!(parse_loginctl_linger("yes\n"), Some(true));
        assert_eq!(parse_loginctl_linger("no\n"), Some(false));
        assert_eq!(parse_loginctl_linger(""), None);
        assert_eq!(parse_loginctl_linger("Linger=maybe"), None);

        assert!(LINGER_DISABLED_WARNING.contains("loginctl enable-linger"));
        assert!(LINGER_DISABLED_WARNING.contains("setsid"));
    }

    #[tokio::test]
    async fn spawn_detached_reports_missing_program_and_missing_cwd() {
        let missing = "legion-definitely-nonexistent-detached-9c8a7b";
        let err = spawn_detached(DetachOptions::new(CommandSpec::argv(
            missing,
            Vec::<&str>::new(),
        )))
        .await
        .unwrap_err();
        match err {
            OsError::ProgramNotFound { program } => assert_eq!(program, missing),
            other => panic!("expected ProgramNotFound, got {other:?}"),
        }

        let dir = tempfile::tempdir().unwrap();
        let err = spawn_detached(
            DetachOptions::new(CommandSpec::argv("unused", Vec::<&str>::new()))
                .cwd(dir.path().join("no-such-dir")),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OsError::NotFound { .. }));
    }
}
