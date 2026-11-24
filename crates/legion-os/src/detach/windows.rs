//! Windows detached process launcher with WMI fallback.
//!
//! Primary path: `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS |
//! CREATE_NEW_PROCESS_GROUP` starts the process outside the caller's Job Object with no console
//! attached and in its own console process group.
//!
//! Fallback path: when the caller runs inside a Job Object that forbids breakaway (OpenSSH's
//! `sshd` session job does), `CreateProcessW` returns `ERROR_ACCESS_DENIED`. In that case we
//! launch through WMI (`Win32_Process.Create` via `Invoke-CimMethod`), which creates the process
//! under the WMI service host outside the caller's job so it outlives the SSH session.

use std::io;
use std::os::windows::io::AsHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

use crate::procid::{self, ProcessId};
use crate::shell::ResolvedCommand;
use crate::win_proc;
use crate::{OsError, Result};

use super::{DetachOptions, build_wmi_create_script, build_wmi_launcher_script, open_append_stdio};

const WMI_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const ERROR_ACCESS_DENIED: i32 = 5;

pub(super) async fn spawn_detached(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<ProcessId> {
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
            (CREATE_BREAKAWAY_FROM_JOB | DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).0,
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
            Ok(ProcessId::new(pid, start_time))
        }
        Err(err) if is_breakaway_refused(&err) => spawn_via_wmi(resolved, options).await,
        Err(err) => Err(OsError::from_spawn_io(program_display, err)),
    }
}

fn is_breakaway_refused(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(ERROR_ACCESS_DENIED)
}

async fn spawn_via_wmi(resolved: &ResolvedCommand, options: &DetachOptions) -> Result<ProcessId> {
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
    let env_pairs: Vec<(String, String)> = options
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();

    let launcher_script = build_wmi_launcher_script(
        &program_str,
        &args_vec,
        Some(&cwd_str),
        &env_remove,
        &env_pairs,
        stdout_abs.as_deref(),
        stderr_abs.as_deref(),
    );
    let cim_script = build_wmi_create_script(&launcher_script, Some(&cwd_str))?;

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
    let pid = stdout
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&pid| pid > 0)
        .ok_or_else(|| OsError::Platform {
            detail: format!(
                "WMI detached launch did not return a valid PID: {:?}",
                stdout.trim()
            ),
        })?;

    procid::of_pid_including_zombie(pid)?.ok_or_else(|| OsError::Platform {
        detail: format!("could not read process creation time for detached pid {pid}"),
    })
}

fn absolutize(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|err| OsError::from_io("cannot determine current directory", err))?;
    Ok(cwd.join(path))
}
