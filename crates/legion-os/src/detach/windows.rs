//! Windows detached process launcher with WMI fallback.
//!
//! Primary path: `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW |
//! CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT` and a
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` restricted strictly to the three freshly opened standard
//! handles (`stdin` null, `stdout` and `stderr` log files or null). This places the process
//! outside the caller's Job Object without opening visible console windows for console children
//! (unlike `DETACHED_PROCESS`) and prevents any other inheritable handle in the parent process
//! (such as parent stdout/stderr pipes) from leaking into the detached job.
//!
//! Fallback path: when the caller runs inside a Job Object that forbids breakaway (OpenSSH's
//! `sshd` session job does), `CreateProcessW` returns `ERROR_ACCESS_DENIED`. In that case we
//! launch through WMI (`Win32_Process.Create` via `Invoke-CimMethod`), which asks the out-of-process
//! WMI provider host (`wmiprvse.exe`) in its own service session to spawn the launcher. Because
//! WMI creates the process from `wmiprvse.exe` rather than from our process, the detached target
//! inherits **zero** handles from the caller process (its only open standard handles are the
//! redirected pipes connected to the launcher's freshly opened append `FileStream`s).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::atomic::{WriteOptions, write_atomic};
use crate::procid::ProcessId;
use crate::shell::{ResolvedCommand, encode_utf16le_base64, windows_cmdline_escape};
use crate::win_proc::{self, DirectSpawnError};
use crate::{OsError, Result};

use super::{
    DetachHandle, DetachOptions, WINDOWS_WMI_MAX_CMDLINE, build_wmi_create_script_for_cmdline,
    build_wmi_launcher_script, merge_windows_env, parse_wmi_handoff, reconcile_wmi_identity,
    resolve_wmi_program, validate_wmi_batch_target,
};

const WMI_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(25);

static WMI_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Removes temporary handoff and fallback launcher files whenever `spawn_via_wmi` exits (on
/// every error path as well as after handoff completes).
struct HandoffCleanup {
    path: PathBuf,
    tmp_path: PathBuf,
    script_path: PathBuf,
}

impl Drop for HandoffCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.tmp_path);
        let _ = fs::remove_file(&self.script_path);
    }
}

pub(super) async fn spawn_detached(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<DetachHandle> {
    let base_cwd = match options.cwd {
        Some(ref dir) => absolutize(dir)?,
        None => std::env::current_dir()
            .map_err(|err| OsError::from_io("cannot determine current directory", err))?,
    };

    let program_display = resolved.program_display();
    let args_vec: Vec<String> = resolved
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let env_remove_str: Vec<String> = resolved
        .env_remove
        .iter()
        .map(|key| key.to_string_lossy().into_owned())
        .collect();
    let overrides_str: Vec<(String, String)> = options
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let inherited_str = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    let merged_env = merge_windows_env(inherited_str, &env_remove_str, &overrides_str);

    let resolved_program = resolve_wmi_program(&program_display, &base_cwd, &merged_env);
    let command_line = build_direct_command_line(&resolved_program, &args_vec)?;

    match win_proc::spawn_detached_direct(
        &program_display,
        &command_line,
        options.cwd.as_deref(),
        &resolved.env_remove,
        &options.env,
        options.stdout_path.as_deref(),
        options.stderr_path.as_deref(),
    ) {
        Ok(process) => {
            // The detached child is already running and may still be reading a spilled `.ps1`
            // wrapper (which deletes itself as its first action); disarm caller-side removal.
            resolved.disarm_temp_script();
            Ok(DetachHandle {
                process,
                systemd_unit: None,
                warnings: Vec::new(),
            })
        }
        Err(DirectSpawnError::BreakawayRefused) => spawn_via_wmi(resolved, options).await,
        Err(DirectSpawnError::Other(err)) => Err(err),
    }
}

fn build_direct_command_line(program: &str, args: &[String]) -> Result<String> {
    let trimmed = program.trim().trim_end_matches(['.', ' ']);
    let lower = trimmed.to_ascii_lowercase();
    let is_batch = lower.ends_with(".cmd") || lower.ends_with(".bat");
    if is_batch {
        validate_wmi_batch_target(program, args)?;
        let mut inner = windows_cmdline_escape(program);
        for arg in args {
            inner.push(' ');
            inner.push_str(&windows_cmdline_escape(arg));
        }
        return Ok(format!("cmd.exe /d /s /c \"{inner}\""));
    }

    let mut line = windows_cmdline_escape(program);
    for arg in args {
        line.push(' ');
        line.push_str(&windows_cmdline_escape(arg));
    }
    Ok(line)
}

async fn spawn_via_wmi(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<DetachHandle> {
    let base_cwd = match options.cwd {
        Some(ref dir) => absolutize(dir)?,
        None => std::env::current_dir()
            .map_err(|err| OsError::from_io("cannot determine current directory", err))?,
    };
    let cwd_str = base_cwd.to_string_lossy().into_owned();

    let stdout_abs = match options.stdout_path {
        Some(ref path) => Some(absolutize(path)?.to_string_lossy().into_owned()),
        None => None,
    };
    let stderr_abs = match options.stderr_path {
        Some(ref path) => Some(absolutize(path)?.to_string_lossy().into_owned()),
        None => None,
    };

    let (handoff_path, script_fallback_path) = allocate_wmi_temp_paths();
    let handoff_str = handoff_path.to_string_lossy().into_owned();
    let _handoff_cleanup = HandoffCleanup {
        tmp_path: PathBuf::from(format!("{handoff_str}.tmp")),
        path: handoff_path.clone(),
        script_path: script_fallback_path.clone(),
    };

    let raw_program = resolved.program_display();
    let args_vec: Vec<String> = resolved
        .args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let env_remove: Vec<String> = resolved
        .env_remove
        .iter()
        .map(|key| key.to_string_lossy().into_owned())
        .collect();
    let overrides: Vec<(String, String)> = options
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    // Use `vars_os()` rather than `vars()` so unpaired UTF-16 surrogates in unrelated inherited
    // environment variables never panic.
    let inherited_env = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    let merged_env = merge_windows_env(inherited_env, &env_remove, &overrides);

    // Resolve bare program names using the caller's forwarded PATH + PATHEXT (before building the
    // launcher, because WMI's `Process::Start` searches the service PATH rather than `$psi`'s env)
    // and refuse `.cmd`/`.bat` targets with cmd.exe metacharacters in their arguments.
    let program_str = resolve_wmi_program(&raw_program, &base_cwd, &merged_env);
    validate_wmi_batch_target(&program_str, &args_vec)?;

    let launcher_script = build_wmi_launcher_script(
        &program_str,
        &args_vec,
        Some(&cwd_str),
        &env_remove,
        &merged_env,
        stdout_abs.as_deref(),
        stderr_abs.as_deref(),
        &handoff_str,
        None,
    );

    let encoded = encode_utf16le_base64(&launcher_script);
    let encoded_cmdline = format!(
        "powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}"
    );

    // Prefer -EncodedCommand when it fits within Win32_Process.Create's 32 KB command-line cap.
    // If a large inherited environment pushes the encoded script past the limit, write a UTF-8
    // with BOM temporary `.ps1` file (which the launcher removes in its `finally` block).
    let command_line = if encoded_cmdline.len() <= WINDOWS_WMI_MAX_CMDLINE {
        encoded_cmdline
    } else {
        let script_str = script_fallback_path.to_string_lossy().into_owned();
        let file_script = build_wmi_launcher_script(
            &program_str,
            &args_vec,
            Some(&cwd_str),
            &env_remove,
            &merged_env,
            stdout_abs.as_deref(),
            stderr_abs.as_deref(),
            &handoff_str,
            Some(&script_str),
        );
        // Write a UTF-8 BOM so Windows PowerShell 5.1 parses the `.ps1` file as UTF-8.
        let mut bom_bytes = Vec::with_capacity(3 + file_script.len());
        bom_bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        bom_bytes.extend_from_slice(file_script.as_bytes());
        write_atomic(&script_fallback_path, &bom_bytes, WriteOptions::default())?;
        format!(
            "powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File {}",
            windows_cmdline_escape(&script_str)
        )
    };

    let cim_script = build_wmi_create_script_for_cmdline(&command_line, Some(&cwd_str))?;

    // Invoke Windows PowerShell with PSModulePath cleared so a PowerShell 7 parent environment
    // cannot break CimCmdlets / Invoke-CimMethod.
    let mut cim = tokio::process::Command::new("powershell.exe");
    cim.args(["-NoProfile", "-NonInteractive", "-Command", &cim_script])
        .env_remove("PSModulePath")
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(WMI_LAUNCH_TIMEOUT, cim.output())
        .await
        .map_err(|_| OsError::TimedOut {
            detail: "WMI Win32_Process.Create did not respond before its deadline".into(),
        })?
        .map_err(|err| OsError::from_spawn_io("powershell.exe", err))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(OsError::Platform {
            detail: if stderr.is_empty() {
                format!(
                    "WMI detached launch failed with exit status {}",
                    output.status
                )
            } else {
                format!("WMI detached launch failed: {stderr}")
            },
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let wrapper_pid = stdout
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&pid| pid > 0)
        .ok_or_else(|| OsError::Platform {
            detail: format!(
                "WMI detached launch did not return a valid wrapper PID: {:?}",
                stdout.trim()
            ),
        })?;

    // Wait for the launcher to start the real target process and write its PID + creation time.
    let reported = wait_for_handoff(&handoff_path, wrapper_pid, WMI_LAUNCH_TIMEOUT).await?;
    resolved.disarm_temp_script();

    // Prefer querying Win32 GetProcessTimes directly in UTC for the target PID, accepting the
    // queried identity only when it matches `reported` exactly or differs by exactly +/- 1 hour
    // (DST ambiguity in .NET's Process.StartTime); reject any other mismatch as a recycled PID.
    let process = match win_proc::query_process_identity(reported.pid, true) {
        Ok(queried) => reconcile_wmi_identity(reported, queried)?,
        Err(_) => reported,
    };

    Ok(DetachHandle {
        process,
        systemd_unit: None,
        warnings: Vec::new(),
    })
}

async fn wait_for_handoff(
    handoff_path: &Path,
    wrapper_pid: u32,
    timeout: Duration,
) -> Result<ProcessId> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(handoff_path)
            && !contents.trim().is_empty()
        {
            return parse_wmi_handoff(&contents);
        }

        // If the WMI wrapper exited without producing a handoff file, fail promptly rather than
        // waiting out the full timeout.
        let wrapper_gone = matches!(
            win_proc::query_process_identity(wrapper_pid, false),
            Ok(None)
        );
        if wrapper_gone {
            if let Ok(contents) = fs::read_to_string(handoff_path)
                && !contents.trim().is_empty()
            {
                return parse_wmi_handoff(&contents);
            }
            return Err(OsError::Platform {
                detail: format!(
                    "WMI detached launcher (pid {wrapper_pid}) exited without reporting target identity"
                ),
            });
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(OsError::TimedOut {
                detail: "timed out waiting for WMI detached launcher to report target PID".into(),
            });
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(HANDOFF_POLL_INTERVAL.min(remaining)).await;
    }
}

fn allocate_wmi_temp_paths() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let seq = WMI_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let stem = format!("legion-wmi-{pid}-{seq}-{nanos}");
    (
        dir.join(format!("{stem}.pid")),
        dir.join(format!("{stem}.ps1")),
    )
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|err| OsError::from_io("cannot determine current directory", err))?;
    Ok(cwd.join(path))
}
