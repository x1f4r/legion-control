//! Windows detached process launcher with named Job Object boundary and WMI fallback.
//!
//! Primary path: `CreateProcessW` with `CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW |
//! CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED` and a
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` restricted strictly to the three freshly opened standard
//! handles (`stdin` null, `stdout` and `stderr` log files or null) plus one read-only
//! `JOB_OBJECT_QUERY` keepalive handle for the newly created named Job Object
//! (`Local\legion-job-<marker>`). The child is assigned to the Job Object before `ResumeThread` so
//! the job bounds the entire detached tree and [`detached_active`] / [`stop_detached`] can query
//! and terminate it directly without risking PID-reuse mistakes; no caller file or pipe handles
//! leak into the child.
//!
//! Fallback path: when the caller runs inside a Job Object that forbids breakaway (OpenSSH's
//! `sshd` session job does), `CreateProcessW` returns `ERROR_ACCESS_DENIED`. In that case we
//! launch through WMI (`Win32_Process.Create` via `Invoke-CimMethod`), which asks the out-of-process
//! WMI provider host (`wmiprvse.exe`) in its own service session to spawn the launcher. Because
//! WMI creates the process from `wmiprvse.exe` rather than from our process, the detached target
//! inherits **zero** handles from the caller process (its only open standard handles are the
//! redirected pipes connected to the launcher's freshly opened append `FileStream`s), and
//! `LEGION_DETACH_ID=<marker>` in its environment provides orphan proof after root exit.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::atomic::{WriteOptions, write_atomic};
use crate::procid::{self, KillTreeReport, ProcessId};
use crate::shell::{ResolvedCommand, encode_utf16le_base64, windows_cmdline_escape};
use crate::win_proc::{self, DirectSpawnError};
use crate::{OsError, Result};

use super::{
    DETACH_MARKER_ENV, DetachHandle, DetachOptions, WINDOWS_WMI_MAX_CMDLINE,
    build_windows_direct_launch, build_wmi_create_script_for_cmdline, build_wmi_launcher_script,
    generate_detach_marker, merge_windows_env, parse_wmi_handoff, reconcile_wmi_identity,
    resolve_windows_job_name, resolve_wmi_program, validate_wmi_batch_target,
};

const WMI_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(25);
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(100);
const JOB_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

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
    let mut combined_env_remove_os = resolved.env_remove.clone();
    combined_env_remove_os.extend(options.env_remove.iter().cloned());
    let env_remove_str: Vec<String> = combined_env_remove_os
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

    let marker = generate_detach_marker();
    let job_name = resolve_windows_job_name(&marker);

    let mut direct_env_overrides = options.env.clone();
    direct_env_overrides
        .retain(|k, _| !k.to_string_lossy().eq_ignore_ascii_case(DETACH_MARKER_ENV));
    direct_env_overrides.insert(OsString::from(DETACH_MARKER_ENV), OsString::from(&marker));

    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .and_then(|v| v.into_string().ok());
    let resolved_program = resolve_wmi_program(&program_display, &base_cwd, &merged_env);
    let (application_name, command_line) =
        build_windows_direct_launch(&resolved_program, &args_vec, system_root.as_deref())?;

    match win_proc::spawn_detached_direct(
        &program_display,
        application_name.as_deref(),
        &command_line,
        options.cwd.as_deref(),
        &combined_env_remove_os,
        &direct_env_overrides,
        options.stdout_path.as_deref(),
        options.stderr_path.as_deref(),
        Some(&job_name),
    ) {
        Ok((process, windows_job)) => {
            // The detached child is already running and may still be reading a spilled `.ps1`
            // wrapper (which deletes itself as its first action); disarm caller-side removal.
            resolved.disarm_temp_script();
            Ok(DetachHandle {
                process,
                systemd_unit: None,
                windows_job,
                marker: Some(marker),
                warnings: Vec::new(),
            })
        }
        Err(DirectSpawnError::BreakawayRefused) => spawn_via_wmi(resolved, options, marker).await,
        Err(DirectSpawnError::Other(err)) => Err(err),
    }
}

pub(super) async fn detached_active(handle: &DetachHandle) -> Result<bool> {
    if let Some(ref job_name) = handle.windows_job {
        let job_owned = job_name.clone();
        let root = handle.process;
        let job_check = tokio::task::spawn_blocking(move || -> Result<Option<bool>> {
            if procid::is_alive(&root) {
                return Ok(Some(true));
            }
            let Some(pids) = win_proc::query_job_active_pids(&job_owned)? else {
                return Ok(None);
            };
            for pid in pids {
                if !matches!(procid::of_pid(pid), Ok(None)) {
                    return Ok(Some(true));
                }
            }
            Ok(Some(false))
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("detached_active job worker task failed: {err}"),
        })??;

        if let Some(active) = job_check {
            return Ok(active);
        }
    }

    procid::tree_is_active_with_marker(&handle.process, handle.marker.as_deref()).await
}

pub(super) async fn stop_detached(
    handle: &DetachHandle,
    grace: Duration,
) -> Result<KillTreeReport> {
    if let Some(ref job_name) = handle.windows_job {
        let job_owned = job_name.clone();
        let root = handle.process;
        let terminated = tokio::task::spawn_blocking(move || {
            let Some(mut tracked) = win_proc::terminate_named_job(&job_owned)? else {
                return Ok(None);
            };
            if procid::is_alive(&root) && !tracked.iter().any(|id| id.pid == root.pid) {
                tracked.push(root);
            }
            Ok::<_, OsError>(Some(tracked))
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("stop_detached job worker task failed: {err}"),
        })??;

        if let Some(tracked) = terminated {
            let settle = JOB_SETTLE_TIMEOUT.max(grace.min(Duration::from_secs(5)));
            let deadline = Instant::now() + settle;
            loop {
                let job_poll = job_name.clone();
                let probe = tracked.clone();
                let still_running = tokio::task::spawn_blocking(move || -> bool {
                    if probe.iter().any(procid::is_alive) {
                        return true;
                    }
                    if let Ok(Some(pids)) = win_proc::query_job_active_pids(&job_poll) {
                        return pids
                            .into_iter()
                            .any(|pid| matches!(procid::of_pid(pid), Ok(Some(_))));
                    }
                    false
                })
                .await
                .map_err(|err| OsError::Platform {
                    detail: format!("stop_detached job settle task failed: {err}"),
                })?;

                if !still_running {
                    return Ok(KillTreeReport {
                        all_gone: true,
                        survivors: Vec::new(),
                        unverified_survivors: Vec::new(),
                    });
                }
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(JOB_POLL_INTERVAL.min(remaining)).await;
            }

            let job_final = job_name.clone();
            let survivors = tokio::task::spawn_blocking(move || {
                let mut left: Vec<ProcessId> =
                    tracked.into_iter().filter(procid::is_alive).collect();
                if let Ok(Some(pids)) = win_proc::query_job_active_pids(&job_final) {
                    for pid in pids {
                        if let Ok(Some(id)) = procid::of_pid(pid)
                            && !left.iter().any(|existing| existing.pid == id.pid)
                        {
                            left.push(id);
                        }
                    }
                }
                left.sort_by_key(|id| id.pid);
                left
            })
            .await
            .map_err(|err| OsError::Platform {
                detail: format!("stop_detached job survivor check failed: {err}"),
            })?;

            return Ok(KillTreeReport {
                all_gone: survivors.is_empty(),
                survivors,
                unverified_survivors: Vec::new(),
            });
        }
    }

    procid::kill_tree_with_marker(&handle.process, handle.marker.as_deref(), grace).await
}

async fn spawn_via_wmi(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
    marker: String,
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
        .chain(options.env_remove.iter())
        .map(|key| key.to_string_lossy().into_owned())
        .collect();
    let mut overrides: Vec<(String, String)> = options
        .env
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    overrides.push((DETACH_MARKER_ENV.to_owned(), marker.clone()));
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
        windows_job: None,
        marker: Some(marker),
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
