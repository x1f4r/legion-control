//! Windows detached process launcher with WMI fallback.
//!
//! Primary path: `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW |
//! CREATE_NEW_PROCESS_GROUP` starts the process outside the caller's Job Object in its own console
//! process group without allowing child console programs to open visible console windows (unlike
//! `DETACHED_PROCESS`, which causes console children to allocate new visible windows).
//!
//! Fallback path: when the caller runs inside a Job Object that forbids breakaway (OpenSSH's
//! `sshd` session job does), `CreateProcessW` returns `ERROR_ACCESS_DENIED`. In that case we
//! launch through WMI (`Win32_Process.Create` via `Invoke-CimMethod`), which creates the process
//! under the WMI service host outside the caller's job so it outlives the SSH session. Because
//! WMI launches under the service environment and returns the wrapper's PID, the launcher script
//! explicitly forwards the merged caller environment and writes the real target's PID and creation
//! timestamp to a handoff file read back by the caller.

use std::fs;
use std::io;
use std::os::windows::io::AsHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
};

use crate::atomic::{WriteOptions, write_atomic};
use crate::procid::ProcessId;
use crate::shell::{ResolvedCommand, encode_utf16le_base64, windows_cmdline_escape};
use crate::win_proc;
use crate::{OsError, Result};

use super::{
    DetachHandle, DetachOptions, WINDOWS_WMI_MAX_CMDLINE, build_wmi_create_script_for_cmdline,
    build_wmi_launcher_script, merge_windows_env, open_append_stdio, parse_wmi_handoff,
};

const WMI_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(25);
const ERROR_ACCESS_DENIED: i32 = 5;

static WMI_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Removes temporary handoff files when `spawn_via_wmi` exits.
struct HandoffCleanup {
    path: PathBuf,
    tmp_path: PathBuf,
}

impl Drop for HandoffCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(&self.tmp_path);
    }
}

pub(super) async fn spawn_detached(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<DetachHandle> {
    let (stdout_stdio, stderr_stdio) = open_append_stdio(
        options.stdout_path.as_deref(),
        options.stderr_path.as_deref(),
    )?;

    let program_display = resolved.program_display();
    let mut cmd = resolved.to_std_command();
    for (key, value) in &options.env {
        cmd.env(key, value);
    }
    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
        .stdout(stdout_stdio)
        .stderr(stderr_stdio)
        .creation_flags(
            (CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).0,
        );

    let spawn_result = cmd.spawn();
    // Release the command (and its appended log handles) before falling back to WMI.
    drop(cmd);

    match spawn_result {
        Ok(child) => {
            let pid = child.id();
            // Read the exact 100-ns FILETIME creation timestamp directly from the process handle we
            // already hold; this stays valid even if the child exits immediately and never guesses.
            let start_time = win_proc::creation_time_from_handle(child.as_handle())?;
            // Dropping std::process::Child on Windows closes our process handle without killing
            // the detached child.
            drop(child);
            Ok(DetachHandle {
                process: ProcessId::new(pid, start_time),
                systemd_unit: None,
                warnings: Vec::new(),
            })
        }
        Err(err) if is_breakaway_refused(&err) => spawn_via_wmi(resolved, options).await,
        Err(err) => Err(OsError::from_spawn_io(program_display, err)),
    }
}

fn is_breakaway_refused(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(ERROR_ACCESS_DENIED)
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
    };

    let program_str = resolved.program_display();
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
    let merged_env = merge_windows_env(std::env::vars(), &env_remove, &overrides);

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

    // Prefer querying Win32 GetProcessTimes directly in UTC for the target PID (avoiding any
    // local-time DST ambiguity in .NET's Process.StartTime), falling back to the launcher's
    // reported FILETIME only if the target already exited before we queried it.
    let process = match win_proc::query_process_identity(reported.pid, true) {
        Ok(Some(exact)) => exact,
        _ => reported,
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
