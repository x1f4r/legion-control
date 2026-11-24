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
//! - **Windows (direct path):** spawning with `CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS |
//!   CREATE_NEW_PROCESS_GROUP` creates a console-less process in its own process group outside the
//!   caller's Job Object so closing the caller's console or job leaves the child running.
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

/// Maximum safe command-line length accepted by Windows `Win32_Process.Create`.
#[allow(dead_code)]
pub(crate) const WINDOWS_WMI_MAX_CMDLINE: usize = 32_000;

/// Configuration for [`spawn_detached`].
#[derive(Clone, Debug)]
pub struct DetachOptions {
    /// Command to start (exact argv or platform shell string).
    pub command: CommandSpec,
    /// Optional caller-chosen identifier used to name the Linux transient systemd scope
    /// (`--unit=legion-job-<name>`). Must contain only ASCII letters, digits, `-`, `_`, or `.`.
    /// When omitted, a unique name is generated automatically.
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

/// Start a detached process that outlives the caller, its terminal, and its SSH session.
///
/// Standard input is always connected to null; standard output and standard error are appended
/// to the configured files (or connected to null when omitted). Returns a serializable
/// [`DetachHandle`] carrying the target's [`ProcessId`], its Linux systemd scope unit name when
/// a scope was used, and any non-fatal launch warnings.
pub async fn spawn_detached(options: DetachOptions) -> Result<DetachHandle> {
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

    spawn_detached_impl(&resolved, &options).await
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
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(OsError::InvalidArgument {
            detail: format!(
                "detached job name {name:?} must be non-empty and contain only ASCII letters, digits, '-', '_', or '.'"
            ),
        });
    }
    Ok(())
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
#[allow(dead_code)]
pub(crate) fn resolve_scope_unit_name(name: Option<&str>) -> Result<(String, String)> {
    validate_scope_name(name)?;
    let stem = match name {
        Some(raw) => format!("legion-job-{raw}"),
        None => {
            let pid = std::process::id();
            let seq = SCOPE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            format!("legion-job-{pid}-{seq}-{nanos}")
        }
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

#[cfg(any(unix, windows))]
async fn spawn_detached_impl(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<DetachHandle> {
    platform::spawn_detached(resolved, options).await
}

#[cfg(not(any(unix, windows)))]
async fn spawn_detached_impl(
    _resolved: &ResolvedCommand,
    _options: &DetachOptions,
) -> Result<DetachHandle> {
    Err(OsError::Unsupported {
        detail: "detached process launching is not supported on this platform".into(),
    })
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
/// The launcher starts the target via `System.Diagnostics.Process` with stdin closed and raw
/// stdout/stderr streams pumped unbuffered into append-mode `FileStream`s, avoiding PowerShell
/// 5.1's UTF-16 `>>` redirection mangling and keeping the WMI process alive until the target exits.
#[allow(dead_code)]
pub(crate) fn build_wmi_launcher_script(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env_remove: &[String],
    env: &[(String, String)],
    stdout_path: Option<&str>,
    stderr_path: Option<&str>,
) -> String {
    let escaped_args: String = args
        .iter()
        .map(|arg| windows_cmdline_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");

    let mut lines = vec![
        "$ErrorActionPreference = 'Stop'".to_owned(),
        "$psi = New-Object System.Diagnostics.ProcessStartInfo".to_owned(),
        format!("$psi.FileName = {}", powershell_single_quote(program)),
        format!(
            "$psi.Arguments = {}",
            powershell_single_quote(&escaped_args)
        ),
    ];

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

    lines.push(match stdout_path {
        Some(path) => format!(
            "$outStream = New-Object System.IO.FileStream({}, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete), 1, [System.IO.FileOptions]::WriteThrough)",
            powershell_single_quote(path)
        ),
        None => "$outStream = [System.IO.Stream]::Null".to_owned(),
    });
    lines.push(match stderr_path {
        Some(path) => format!(
            "$errStream = New-Object System.IO.FileStream({}, [System.IO.FileMode]::Append, [System.IO.FileAccess]::Write, ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete), 1, [System.IO.FileOptions]::WriteThrough)",
            powershell_single_quote(path)
        ),
        None => "$errStream = [System.IO.Stream]::Null".to_owned(),
    });

    lines.extend([
        "try {".to_owned(),
        "  $proc = [System.Diagnostics.Process]::Start($psi)".to_owned(),
        "  $proc.StandardInput.Close()".to_owned(),
        "  $outTask = $proc.StandardOutput.BaseStream.CopyToAsync($outStream, 4096)".to_owned(),
        "  $errTask = $proc.StandardError.BaseStream.CopyToAsync($errStream, 4096)".to_owned(),
        "  $proc.WaitForExit()".to_owned(),
        "  [System.Threading.Tasks.Task]::WaitAll([System.Threading.Tasks.Task[]]@($outTask, $errTask))".to_owned(),
        "  exit $proc.ExitCode".to_owned(),
        "} finally {".to_owned(),
        "  if ($null -ne $outStream) { $outStream.Dispose() }".to_owned(),
        "  if ($null -ne $errStream) { $errStream.Dispose() }".to_owned(),
        "}".to_owned(),
    ]);

    lines.join("\n")
}

/// Build the outer PowerShell script that invokes `Win32_Process.Create` via `Invoke-CimMethod`
/// and prints the created PID to stdout.
#[allow(dead_code)]
pub(crate) fn build_wmi_create_script(launcher_script: &str, cwd: Option<&str>) -> Result<String> {
    let encoded = encode_utf16le_base64(launcher_script);
    let command_line = format!(
        "powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}"
    );
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
            powershell_single_quote(&command_line)
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
        );
        assert!(launcher.contains(r"'C:\Users\me\O''Brien\worker.exe'"));
        assert!(launcher.contains(r#"--flag "value with spaces""#));
        assert!(launcher.contains("$psi.EnvironmentVariables.Remove('PSModulePath')"));
        assert!(launcher.contains("$psi.EnvironmentVariables['LEGION_KEY'] = 'it''s ok'"));
        assert!(launcher.contains("[System.IO.FileMode]::Append"));
        assert!(launcher.contains("[System.Threading.Tasks.Task[]]@($outTask, $errTask)"));

        let cim = build_wmi_create_script(&launcher, Some(r"C:\Users\me\work")).unwrap();
        assert!(cim.contains("Invoke-CimMethod -ClassName Win32_Process -MethodName Create"));
        assert!(cim.contains("$cimArgs['CurrentDirectory'] = 'C:\\Users\\me\\work'"));
        assert!(!cim.contains("$args ="));
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
    fn scope_unit_name_validates_caller_names() {
        let (stem, full) = resolve_scope_unit_name(Some("job_42.run-1")).unwrap();
        assert_eq!(stem, "legion-job-job_42.run-1");
        assert_eq!(full, "legion-job-job_42.run-1.scope");

        assert!(matches!(
            resolve_scope_unit_name(Some("")),
            Err(OsError::InvalidArgument { .. })
        ));
        assert!(matches!(
            resolve_scope_unit_name(Some("bad/name")),
            Err(OsError::InvalidArgument { .. })
        ));
        assert!(matches!(
            resolve_scope_unit_name(Some("has space")),
            Err(OsError::InvalidArgument { .. })
        ));
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
