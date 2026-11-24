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
//! invoke WMI `Win32_Process.Create` directly via COM (`IWbemLocator` -> `ROOT\CIMV2`) so the
//! out-of-process WMI provider host (`wmiprvse.exe`) in its own service session spawns the
//! launcher without paying for an outer PowerShell 5.1 + `CimCmdlets` startup. Transient errors
//! that provably did not start a process are retried with bounded backoff, falling back to
//! `Invoke-CimMethod` and finally to a non-breakaway direct launch (with a warning) only when WMI
//! provably never started anything, so the same process is never launched twice.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::atomic::{WriteOptions, write_atomic};
use crate::procid::{self, KillTreeReport, ProcessId};
use crate::shell::{ResolvedCommand, encode_utf16le_base64, windows_cmdline_escape};
use crate::win_proc::{self, DirectSpawnError, WmiCreateError};
use crate::{OsError, Result};

use super::{
    DETACH_MARKER_ENV, DetachHandle, DetachOptions, InFlightGuard, LaunchBudget,
    WINDOWS_WMI_LAUNCH_DEADLINE, WINDOWS_WMI_MAX_CMDLINE, WINDOWS_WMI_SINGLE_CALL_CAP,
    WMI_FALLBACK_WARNING, WmiCimResult, build_windows_direct_launch,
    build_wmi_create_script_for_cmdline, build_wmi_launcher_script, generate_detach_marker,
    merge_recheck_survivors, merge_windows_env, parse_wmi_cim_output, parse_wmi_handoff,
    reconcile_wmi_identity, resolve_windows_job_name, resolve_windows_powershell_exe,
    resolve_wmi_program, validate_wmi_batch_target, wmi_breakaway_return_value_rejected,
    wmi_in_flight_error, wmi_unavailable_error,
};

const WMI_MAX_ATTEMPTS: usize = 3;
const WMI_RETRY_BACKOFFS: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(750)];
const HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(25);
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(25);
const JOB_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

static WMI_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
static WMI_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

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
        Ok((process, root_start_key, windows_job)) => {
            // The detached child is already running and may still be reading a spilled `.ps1`
            // wrapper (which deletes itself as its first action); disarm caller-side removal.
            resolved.disarm_temp_script();
            Ok(DetachHandle {
                process,
                root_start_key,
                systemd_unit: None,
                windows_job,
                marker: Some(marker),
                warnings: Vec::new(),
            })
        }
        Err(DirectSpawnError::BreakawayRefused) => {
            let direct_fallback = DirectFallbackLaunch {
                program_display: &program_display,
                application_name: application_name.as_deref(),
                command_line: &command_line,
                env_remove: &combined_env_remove_os,
                env_overrides: &direct_env_overrides,
                job_name: &job_name,
            };
            spawn_via_wmi(
                resolved,
                options,
                marker,
                system_root.as_deref(),
                &direct_fallback,
            )
            .await
        }
        Err(DirectSpawnError::Other(err)) => Err(err),
    }
}

struct DirectFallbackLaunch<'a> {
    program_display: &'a str,
    application_name: Option<&'a str>,
    command_line: &'a str,
    env_remove: &'a [OsString],
    env_overrides: &'a BTreeMap<OsString, OsString>,
    job_name: &'a str,
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
    let mut report = stop_detached_primary(handle, grace).await?;

    // Re-check using the exact same marker rule as `detached_active` so `stop_detached` never
    // reports `all_gone: true` while a marked descendant (for example on the WMI path or after a
    // fallback) is still running.
    if let Some(ref marker) = handle.marker {
        let mut remaining = procid::find_marked_live_processes(marker).await?;
        if !remaining.is_empty() {
            let followup = procid::kill_tree_with_marker_and_start_key(
                &handle.process,
                handle.root_start_key,
                Some(marker),
                grace,
            )
            .await?;
            for id in followup.unverified_survivors {
                if !report
                    .unverified_survivors
                    .iter()
                    .any(|existing| existing.pid == id.pid)
                {
                    report.unverified_survivors.push(id);
                }
            }
            remaining = procid::find_marked_live_processes(marker).await?;
        }
        if procid::is_alive(&handle.process)
            && !remaining.iter().any(|id| id.pid == handle.process.pid)
        {
            remaining.push(handle.process);
        }
        if !report.survivors.is_empty() {
            let prior = std::mem::take(&mut report.survivors);
            report.survivors = tokio::task::spawn_blocking(move || {
                prior.into_iter().filter(procid::is_alive).collect()
            })
            .await
            .map_err(|err| OsError::Platform {
                detail: format!("stop_detached survivor recheck task failed: {err}"),
            })?;
        }
        merge_recheck_survivors(&mut report, remaining);
    }

    Ok(report)
}

async fn stop_detached_primary(handle: &DetachHandle, grace: Duration) -> Result<KillTreeReport> {
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

    procid::kill_tree_with_marker_and_start_key(
        &handle.process,
        handle.root_start_key,
        handle.marker.as_deref(),
        grace,
    )
    .await
}

async fn spawn_via_wmi(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
    marker: String,
    system_root: Option<&str>,
    direct_fallback: &DirectFallbackLaunch<'_>,
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

    let powershell_exe = resolve_windows_powershell_exe(system_root);
    let powershell_escaped = windows_cmdline_escape(&powershell_exe);

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
        "{powershell_escaped} -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {encoded}"
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
            "{powershell_escaped} -NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File {}",
            windows_cmdline_escape(&script_str)
        )
    };

    let budget = LaunchBudget::from_now(WINDOWS_WMI_LAUNCH_DEADLINE);

    let reported = match create_wmi_wrapper_with_retry(
        &command_line,
        &cwd_str,
        &handoff_path,
        &powershell_exe,
        budget,
    )
    .await?
    {
        WmiCreateOutcome::Started(wrapper_pid) => {
            let handoff_timeout = budget.remaining().ok_or_else(|| OsError::TimedOut {
                detail: format!(
                    "WMI detached launch exceeded its {}s overall deadline before the launcher reported target PID; check system load and inspect `job.status` before retrying",
                    WINDOWS_WMI_LAUNCH_DEADLINE.as_secs()
                ),
            })?;
            wait_for_handoff(&handoff_path, Some(wrapper_pid), handoff_timeout).await?
        }
        WmiCreateOutcome::HandoffReady => {
            let handoff_timeout = budget.remaining().unwrap_or(Duration::from_secs(1));
            wait_for_handoff(&handoff_path, None, handoff_timeout).await?
        }
        WmiCreateOutcome::MaybeStarted(cause) => {
            // The WMI provider host may have created the process before failing or timing out.
            // Wait for the remaining overall hand-off budget before concluding failure, and never
            // retry or fall back.
            if let Ok(contents) = fs::read_to_string(&handoff_path)
                && !contents.trim().is_empty()
            {
                parse_wmi_handoff(&contents)?
            } else if let Some(handoff_timeout) = budget.remaining() {
                match wait_for_handoff(&handoff_path, None, handoff_timeout).await {
                    Ok(id) => id,
                    Err(_) => return Err(cause),
                }
            } else {
                return Err(cause);
            }
        }
        WmiCreateOutcome::Unavailable(wmi_reason) => {
            if !options.allow_in_job_fallback {
                return Err(wmi_unavailable_error(&wmi_reason));
            }
            let (process, root_start_key, windows_job) = win_proc::spawn_detached_in_current_job(
                direct_fallback.program_display,
                direct_fallback.application_name,
                direct_fallback.command_line,
                options.cwd.as_deref(),
                direct_fallback.env_remove,
                direct_fallback.env_overrides,
                options.stdout_path.as_deref(),
                options.stderr_path.as_deref(),
                Some(direct_fallback.job_name),
            )
            .map_err(|direct_err| OsError::Platform {
                detail: format!(
                    "WMI detached launch unavailable ({wmi_reason}) and opt-in in-job fallback failed ({direct_err}); ensure the Windows Management Instrumentation (Winmgmt) service is running and check permissions"
                ),
            })?;
            resolved.disarm_temp_script();
            return Ok(DetachHandle {
                process,
                root_start_key,
                systemd_unit: None,
                windows_job,
                marker: Some(marker),
                warnings: vec![format!("{WMI_FALLBACK_WARNING} ({wmi_reason})")],
            });
        }
    };
    resolved.disarm_temp_script();

    // Prefer querying Win32 GetProcessTimes directly in UTC for the target PID (and capture its
    // monotonic kernel ProcessStartKey at launch time), accepting the queried identity only when
    // it matches `reported` exactly or differs by exactly +/- 1 hour (DST ambiguity in .NET's
    // Process.StartTime); reject any other mismatch as a recycled PID.
    let (process, root_start_key) =
        match win_proc::query_process_identity_with_start_key(reported.pid, true) {
            Ok(Some((queried, start_key))) => {
                (reconcile_wmi_identity(reported, Some(queried))?, start_key)
            }
            Ok(None) | Err(_) => (reported, None),
        };

    Ok(DetachHandle {
        process,
        root_start_key,
        systemd_unit: None,
        windows_job: None,
        marker: Some(marker),
        warnings: Vec::new(),
    })
}

enum WmiCreateOutcome {
    Started(u32),
    HandoffReady,
    MaybeStarted(OsError),
    Unavailable(String),
}

fn handoff_file_ready(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| !contents.trim().is_empty())
}

enum NativeWmiCallError {
    /// Failed before invoking `Win32_Process.Create` (budget already expired, earlier WMI call
    /// still blocked in-flight, or worker thread could not be spawned).
    NotDispatched(OsError),
    /// Timed out waiting for an invoked COM WMI call to return; the process may still start late.
    TimedOut(OsError),
}

async fn run_native_wmi_create_once(
    command_line: String,
    cwd: String,
    budget: LaunchBudget,
) -> std::result::Result<std::result::Result<u32, WmiCreateError>, NativeWmiCallError> {
    let Some(call_timeout) = budget.call_budget(WINDOWS_WMI_SINGLE_CALL_CAP) else {
        return Err(NativeWmiCallError::NotDispatched(OsError::TimedOut {
            detail: format!(
                "WMI Win32_Process.Create exceeded the {}s overall launch deadline; check system load and the Windows Management Instrumentation (Winmgmt) service, and inspect `job.status` before retrying",
                WINDOWS_WMI_LAUNCH_DEADLINE.as_secs()
            ),
        }));
    };

    // Fail fast if a previous WMI worker thread is still stuck in DCOM rather than piling up
    // blocked OS threads. The guard lives inside the worker thread until `wmi_create_process`
    // actually returns, even if this caller times out first.
    let Some(in_flight) = InFlightGuard::try_acquire(&WMI_IN_FLIGHT) else {
        return Err(NativeWmiCallError::NotDispatched(wmi_in_flight_error()));
    };

    let (tx, rx) = tokio::sync::oneshot::channel();
    if let Err(err) = std::thread::Builder::new()
        .name("legion-wmi-create".to_owned())
        .spawn(move || {
            let _guard = in_flight;
            let res = win_proc::wmi_create_process(&command_line, Some(&cwd));
            let _ = tx.send(res);
        })
    {
        return Err(NativeWmiCallError::NotDispatched(OsError::from_io(
            "cannot spawn WMI worker thread",
            err,
        )));
    }

    match tokio::time::timeout(call_timeout, rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(_)) => Ok(Err(WmiCreateError::MaybeStarted {
            detail: "WMI worker thread exited before returning a result".into(),
        })),
        Err(_) => Err(NativeWmiCallError::TimedOut(OsError::TimedOut {
            detail: format!(
                "WMI Win32_Process.Create did not respond within {}s; check system load and the Windows Management Instrumentation (Winmgmt) service, and inspect `job.status` before retrying",
                call_timeout.as_secs()
            ),
        })),
    }
}

async fn create_wmi_wrapper_with_retry(
    command_line: &str,
    cwd: &str,
    handoff_path: &Path,
    powershell_exe: &str,
    budget: LaunchBudget,
) -> Result<WmiCreateOutcome> {
    let mut last_com_err = String::new();
    for attempt in 0..WMI_MAX_ATTEMPTS {
        if attempt > 0 && handoff_file_ready(handoff_path) {
            return Ok(WmiCreateOutcome::HandoffReady);
        }

        match run_native_wmi_create_once(command_line.to_owned(), cwd.to_owned(), budget).await {
            Ok(Ok(wrapper_pid)) => return Ok(WmiCreateOutcome::Started(wrapper_pid)),
            Ok(Err(WmiCreateError::NotStarted { transient, detail })) => {
                last_com_err = detail;
                if transient && attempt + 1 < WMI_MAX_ATTEMPTS {
                    let desired = WMI_RETRY_BACKOFFS
                        .get(attempt)
                        .copied()
                        .unwrap_or(Duration::from_millis(750));
                    let Some(backoff) = budget.backoff(desired) else {
                        break;
                    };
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                break;
            }
            Ok(Err(WmiCreateError::MaybeStarted { detail })) => {
                return Ok(WmiCreateOutcome::MaybeStarted(OsError::Platform {
                    detail: format!(
                        "{detail}; verify whether the process started (for example via `job.status`) and ensure the Windows Management Instrumentation (Winmgmt) service is running before retrying"
                    ),
                }));
            }
            Err(NativeWmiCallError::NotDispatched(err)) => {
                return Err(err);
            }
            Err(NativeWmiCallError::TimedOut(timeout_err)) => {
                return Ok(WmiCreateOutcome::MaybeStarted(timeout_err));
            }
        }
    }

    // Native COM WMI provably did not start a process. Try `Invoke-CimMethod` once in case COM
    // security in this process prevented local WMI access, unless the handoff file already
    // appeared or the overall launch budget is exhausted.
    if handoff_file_ready(handoff_path) {
        return Ok(WmiCreateOutcome::HandoffReady);
    }

    for breakaway in [true, false] {
        let Some(call_timeout) = budget.call_budget(WINDOWS_WMI_SINGLE_CALL_CAP) else {
            return Err(OsError::TimedOut {
                detail: format!(
                    "WMI detached launch exceeded its {}s overall deadline (after COM WMI error: {last_com_err}); check system load and the Windows Management Instrumentation (Winmgmt) service",
                    WINDOWS_WMI_LAUNCH_DEADLINE.as_secs()
                ),
            });
        };

        let cim_script = build_wmi_create_script_for_cmdline(command_line, Some(cwd), breakaway)?;
        let mut cim = tokio::process::Command::new(powershell_exe);
        cim.args(["-NoProfile", "-NonInteractive", "-Command", &cim_script])
            .env_remove("PSModulePath")
            .creation_flags(CREATE_NO_WINDOW.0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = match tokio::time::timeout(call_timeout, cim.output()).await {
            Ok(Ok(out)) => out,
            Ok(Err(err)) => {
                // `powershell.exe` itself could not be spawned, so `Invoke-CimMethod` never ran.
                return Ok(WmiCreateOutcome::Unavailable(format!(
                    "COM WMI failed ({last_com_err}); PowerShell CIM spawn failed ({err})"
                )));
            }
            Err(_) => {
                return Ok(WmiCreateOutcome::MaybeStarted(OsError::TimedOut {
                    detail: format!(
                        "WMI Win32_Process.Create via PowerShell did not respond within {}s (after COM WMI error: {last_com_err}); check system load and the Windows Management Instrumentation (Winmgmt) service, and inspect `job.status` before retrying",
                        call_timeout.as_secs()
                    ),
                }));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        match parse_wmi_cim_output(&stdout, &stderr, output.status.success()) {
            WmiCimResult::Started(wrapper_pid) => {
                return Ok(WmiCreateOutcome::Started(wrapper_pid));
            }
            WmiCimResult::NotStarted {
                return_value,
                detail,
            } => {
                if breakaway && wmi_breakaway_return_value_rejected(return_value) {
                    continue;
                }
                return Ok(WmiCreateOutcome::Unavailable(format!(
                    "COM WMI failed ({last_com_err}); {detail}"
                )));
            }
            WmiCimResult::MaybeStarted { detail } => {
                return Ok(WmiCreateOutcome::MaybeStarted(OsError::Platform {
                    detail: format!(
                        "{detail} (after COM WMI error: {last_com_err}); verify whether the process started (for example via `job.status`) and ensure the Windows Management Instrumentation (Winmgmt) service is running before retrying"
                    ),
                }));
            }
        }
    }

    Ok(WmiCreateOutcome::Unavailable(format!(
        "COM WMI failed ({last_com_err}); Invoke-CimMethod failed"
    )))
}

async fn wait_for_handoff(
    handoff_path: &Path,
    wrapper_pid: Option<u32>,
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
        if let Some(pid) = wrapper_pid {
            let wrapper_gone = matches!(win_proc::query_process_identity(pid, false), Ok(None));
            if wrapper_gone {
                if let Ok(contents) = fs::read_to_string(handoff_path)
                    && !contents.trim().is_empty()
                {
                    return parse_wmi_handoff(&contents);
                }
                return Err(OsError::Platform {
                    detail: format!(
                        "WMI detached launcher (pid {pid}) exited without reporting target identity"
                    ),
                });
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(OsError::TimedOut {
                detail: format!(
                    "timed out after {}s waiting for WMI detached launcher to report target PID; check system load and inspect `job.status` before retrying",
                    timeout.as_secs()
                ),
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
