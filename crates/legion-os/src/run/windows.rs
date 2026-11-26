//! Windows Job Object containment and hard process/job memory limits for [`super::run`].
//!
//! Each child is assigned to a dedicated Windows Job Object with `KILL_ON_JOB_CLOSE`. When a
//! `memory_limit` is requested, the child is created suspended (`CREATE_SUSPENDED`), assigned to a
//! nested Job Object with `JOB_OBJECT_LIMIT_PROCESS_MEMORY` and `JOB_OBJECT_LIMIT_JOB_MEMORY`
//! sized with the same headroom rule (`memory_limit + rlimit_as_headroom`) before any child code
//! executes, and only then resumed.

use std::fs::File;
use std::io::{Read, Write};
use std::mem::size_of;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use tokio_util::sync::CancellationToken;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_JOB_MEMORY,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, GetExitCodeProcess, INFINITE, OpenProcess,
    OpenThread, PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread, SetEvent,
    THREAD_SUSPEND_RESUME, TerminateProcess, WaitForMultipleObjects, WaitForSingleObject,
};

use super::{
    BoundedCapture, CapturedOutput, ChunkCallback, OutputChunk, OutputStream,
    POST_KILL_DRAIN_TIMEOUT, RunOptions, RunOutput, drain_pipes, finalize_run_output,
    wait_deadline, watch_memory_limit,
};
use crate::sandbox::{self, SandboxPolicy};
use crate::shell::ResolvedCommand;
use crate::{OsError, Result};

const POST_KILL_WAIT: Duration = Duration::from_secs(2);

/// Holds any nested memory-limit Job Object handle alive for the lifetime of the child process.
pub(super) struct TreeDropGuard {
    mem_job: Option<HANDLE>,
}

// SAFETY: Win32 Job Object handles are kernel handles safe to send across threads and closed once
// in `Drop`.
#[allow(unsafe_code)]
unsafe impl Send for TreeDropGuard {}

impl Drop for TreeDropGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.mem_job.take() {
            // SAFETY: `handle` is an owned Win32 Job Object handle created by `CreateJobObjectW`.
            #[allow(unsafe_code)]
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

impl TreeDropGuard {
    pub(super) fn new(
        child_pid: Option<u32>,
        memory_limit: Option<u64>,
        memory_headroom: u64,
    ) -> Self {
        let mem_job = child_pid.zip(memory_limit).and_then(|(pid, limit)| {
            let job = attach_memory_job(pid, limit.saturating_add(memory_headroom));
            // Always resume the suspended child after attempting Job Object attachment so it
            // begins executing with the limit already active (or falls back to the RSS watchdog).
            // If resuming fails (for example ToolHelp snapshot or `OpenThread` fails), kill the
            // suspended child immediately rather than leaving it frozen until the timeout.
            if !resume_process_threads(pid) {
                terminate_pid(pid);
            }
            job
        });
        Self { mem_job }
    }

    pub(super) fn disarm(&mut self) {
        // Keep `mem_job` until `Drop` (which runs right before `child` is dropped after exit).
    }
}

fn attach_memory_job(pid: u32, commit_limit: u64) -> Option<HANDLE> {
    let limit = usize::try_from(commit_limit).unwrap_or(usize::MAX);
    // SAFETY: creates an anonymous Job Object with no name or custom security descriptor.
    #[allow(unsafe_code)]
    let job = unsafe { CreateJobObjectW(None, None) }.ok()?;
    if job.is_invalid() {
        return None;
    }

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY;
    info.ProcessMemoryLimit = limit;
    info.JobMemoryLimit = limit;

    // SAFETY: `job` is a valid owned Job Object handle and `info` is properly aligned with exact size.
    #[allow(unsafe_code)]
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
        )
    }
    .is_ok();
    if !configured {
        // SAFETY: `job` is an owned handle we are discarding on error.
        #[allow(unsafe_code)]
        unsafe {
            let _ = CloseHandle(job);
        }
        return None;
    }

    // SAFETY: opens the direct child PID with quota+terminate rights to assign it to `job`.
    #[allow(unsafe_code)]
    let proc_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) };
    let Ok(proc_handle) = proc_handle else {
        // SAFETY: `job` is an owned handle we are discarding on error.
        #[allow(unsafe_code)]
        unsafe {
            let _ = CloseHandle(job);
        }
        return None;
    };

    // SAFETY: both `job` and `proc_handle` are valid Win32 handles.
    #[allow(unsafe_code)]
    let assigned = unsafe { AssignProcessToJobObject(job, proc_handle) }.is_ok();
    // SAFETY: `proc_handle` is no longer needed after assignment.
    #[allow(unsafe_code)]
    unsafe {
        let _ = CloseHandle(proc_handle);
    }

    if !assigned {
        // SAFETY: `job` is an owned handle we are discarding on error.
        #[allow(unsafe_code)]
        unsafe {
            let _ = CloseHandle(job);
        }
        return None;
    }
    Some(job)
}

/// Resumes the primary thread(s) of a child created with `CREATE_SUSPENDED` after its memory-limit
/// Job Object has been attached. Returns `true` only if at least one thread of `pid` was
/// successfully resumed.
fn resume_process_threads(pid: u32) -> bool {
    // SAFETY: `CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)` takes plain integer arguments and
    // returns an owned snapshot handle closed before returning.
    #[allow(unsafe_code)]
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        return false;
    };
    if snap.is_invalid() {
        return false;
    }

    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).unwrap_or(0),
        ..Default::default()
    };

    let mut resumed_any = false;
    let mut failed_any = false;
    // SAFETY: `snap` is a valid ToolHelp32 snapshot handle and `entry.dwSize` is initialized.
    #[allow(unsafe_code)]
    let mut has_next = unsafe { Thread32First(snap, &mut entry) }.is_ok();
    while has_next {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: opens the suspended thread owned by our child PID to resume it and closes the handle.
            #[allow(unsafe_code)]
            match unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) } {
                Ok(thread) => {
                    #[allow(unsafe_code)]
                    let prev_count = unsafe {
                        let rc = ResumeThread(thread);
                        let _ = CloseHandle(thread);
                        rc
                    };
                    if prev_count != u32::MAX {
                        resumed_any = true;
                    } else {
                        failed_any = true;
                    }
                }
                Err(_) => {
                    failed_any = true;
                }
            }
        }
        // SAFETY: `snap` is valid and `entry.dwSize` remains set.
        #[allow(unsafe_code)]
        {
            has_next = unsafe { Thread32Next(snap, &mut entry) }.is_ok();
        }
    }

    // SAFETY: `snap` is an owned ToolHelp32 handle.
    #[allow(unsafe_code)]
    unsafe {
        let _ = CloseHandle(snap);
    }
    resumed_any && !failed_any
}

fn terminate_pid(pid: u32) {
    // SAFETY: opens the suspended child PID with terminate right only, terminates it, and closes
    // the handle immediately.
    #[allow(unsafe_code)]
    if let Ok(proc_handle) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        #[allow(unsafe_code)]
        unsafe {
            let _ = TerminateProcess(proc_handle, 1);
            let _ = CloseHandle(proc_handle);
        }
    }
}

pub(super) struct PreSpawnSandbox;

impl PreSpawnSandbox {
    pub(super) fn release(self) {}
}

pub(super) fn prepare_sandbox(
    _policy: Option<&SandboxPolicy>,
    _resolved: &mut ResolvedCommand,
) -> Result<PreSpawnSandbox> {
    Ok(PreSpawnSandbox)
}

pub(super) fn configure_pre_exec(
    _cmd: &mut tokio::process::Command,
    _memory_limit: Option<u64>,
    _rlimit_as_headroom: u64,
    _pre_spawn: &PreSpawnSandbox,
) {
    // Windows enforces memory limits via the suspended-create + Job Object path in `TreeDropGuard::new`
    // and sandboxing via `run_sandboxed`.
}

/// Runs a command inside a Windows OS sandbox (AppContainer + Job Object primary, Low-Integrity
/// Restricted Token + Job Object fallback) with identical timeout, cancellation, RSS watchdog,
/// and bounded output capture semantics to [`super::run`].
pub(super) async fn run_sandboxed(
    options: RunOptions,
    resolved: ResolvedCommand,
    policy: SandboxPolicy,
    started: Instant,
) -> Result<RunOutput> {
    let stdin_file = options.stdin_file;
    let has_stdin_bytes = stdin_file.is_none() && options.stdin.is_some();
    let mut env_remove = resolved.env_remove.clone();
    env_remove.extend(options.env_remove.iter().cloned());

    let sandbox::windows::SandboxedProcess {
        pid,
        process,
        job,
        winsta_grant,
        stdin_writer,
        stdout_reader,
        stderr_reader,
    } = sandbox::windows::spawn_sandboxed(
        &policy,
        &resolved.program,
        &resolved.args,
        options.cwd.as_deref(),
        options.env_clear,
        &env_remove,
        &options.env,
        stdin_file,
        has_stdin_bytes,
        options.memory_limit,
        options.rlimit_as_headroom,
    )?;

    struct Handles {
        process: sandbox::windows::OwnedWinHandle,
        job: sandbox::windows::OwnedWinHandle,
        _winsta_grant: Option<sandbox::windows::WindowStationGrant>,
    }
    impl Handles {
        fn terminate_job(&self) {
            // SAFETY: `self.job.0` is an owned Win32 Job Object handle.
            #[allow(unsafe_code)]
            let _ =
                unsafe { windows::Win32::System::JobObjects::TerminateJobObject(self.job.0, 1) };
        }
    }
    let handles = Arc::new(Handles {
        process,
        job,
        _winsta_grant: winsta_grant,
    });

    // Ensure the sandbox Job Object is terminated even if this future is dropped early.
    struct JobDropGuard(Arc<Handles>);
    impl Drop for JobDropGuard {
        fn drop(&mut self) {
            self.0.terminate_job();
        }
    }
    let _job_guard = JobDropGuard(Arc::clone(&handles));

    // Move the sole parent write end of stdin into the writer task so dropping it closes the pipe
    // and delivers EOF to the child immediately after writing `bytes`.
    let stdin_task = match (stdin_writer, options.stdin) {
        (Some(mut pipe), Some(bytes)) => Some(tokio::task::spawn_blocking(move || {
            let _ = pipe.write_all(&bytes);
            let _ = pipe.flush();
        })),
        _ => None,
    };

    let drain_stop = CancellationToken::new();
    let _drain_guard = drain_stop.clone().drop_guard();

    let mut stdout_task = Some(tokio::task::spawn_blocking({
        let limit = options.capture_limit;
        let on_output = options.on_output.clone();
        let stop = drain_stop.clone();
        let raw_stdout = options.raw_stdout;
        move || {
            read_blocking_stream(
                stdout_reader,
                limit,
                OutputStream::Stdout,
                on_output,
                stop,
                raw_stdout,
            )
        }
    }));
    let mut stderr_task = Some(tokio::task::spawn_blocking({
        let limit = options.capture_limit;
        let on_output = options.on_output.clone();
        let stop = drain_stop.clone();
        move || {
            read_blocking_stream(
                stderr_reader,
                limit,
                OutputStream::Stderr,
                on_output,
                stop,
                false,
            )
        }
    }));

    // Create a manual-reset Win32 Event to wake the process waiter immediately if timeout,
    // cancellation, or RSS watchdog fires before the child exits.
    // SAFETY: anonymous manual-reset event initialized unsignaled.
    #[allow(unsafe_code)]
    let cancel_event = Arc::new(sandbox::windows::OwnedWinHandle(
        unsafe { CreateEventW(None, true, false, None) }.map_err(|err| OsError::Platform {
            detail: format!("CreateEventW failed for sandbox wait: {err}"),
        })?,
    ));
    struct EventDropGuard(Arc<sandbox::windows::OwnedWinHandle>);
    impl Drop for EventDropGuard {
        fn drop(&mut self) {
            // SAFETY: signaling the event wakes any waiting blocking thread so it exits promptly.
            #[allow(unsafe_code)]
            let _ = unsafe { SetEvent(self.0.0) };
        }
    }
    let _event_guard = EventDropGuard(Arc::clone(&cancel_event));

    let mut wait_task = tokio::task::spawn_blocking({
        let proc_wait = Arc::clone(&handles);
        let cancel_ev = Arc::clone(&cancel_event);
        move || -> Option<i32> {
            let wait_handles = [proc_wait.process.0, cancel_ev.0];
            // SAFETY: both `proc_wait.process.0` and `cancel_ev.0` stay alive via `Arc` for the
            // full duration of `WaitForMultipleObjects`.
            #[allow(unsafe_code)]
            let waited = unsafe { WaitForMultipleObjects(&wait_handles, false, INFINITE) };
            if waited == WAIT_OBJECT_0 {
                let mut code = 0u32;
                // SAFETY: `proc_wait.process.0` is an owned handle to the exited process.
                #[allow(unsafe_code)]
                if unsafe { GetExitCodeProcess(proc_wait.process.0, &mut code) }.is_ok() {
                    return Some(code as i32);
                }
            }
            None
        }
    });

    let deadline = options.timeout.map(|d| tokio::time::Instant::now() + d);
    let peak_rss = Arc::new(AtomicU64::new(0));
    let mut exit_code: Option<i32> = None;
    let mut wait_done = false;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut memory_watchdog_killed = false;
    let mut cpu_time: Option<Duration> = None;

    tokio::select! {
        res = &mut wait_task => {
            wait_done = true;
            exit_code = res.ok().flatten();
        }
        () = wait_deadline(deadline) => {
            timed_out = true;
            cpu_time = crate::mem_limit::process_cpu_time(pid);
        }
        () = options.cancel.cancelled() => {
            cancelled = true;
        }
        () = watch_memory_limit(Some(pid), options.memory_limit, options.rss_poll_interval, Arc::clone(&peak_rss)) => {
            memory_watchdog_killed = true;
            cpu_time = crate::mem_limit::process_cpu_time(pid);
        }
    }

    // Wake wait_task (if still running) and terminate the Job Object so all handles close.
    // SAFETY: `cancel_event.0` is a valid owned Event handle.
    #[allow(unsafe_code)]
    let _ = unsafe { SetEvent(cancel_event.0) };
    handles.terminate_job();

    if exit_code.is_none() {
        let proc_wait = Arc::clone(&handles);
        let wait_ms = POST_KILL_DRAIN_TIMEOUT.as_millis().min(2000) as u32;
        exit_code = tokio::task::spawn_blocking(move || {
            // SAFETY: `proc_wait.process.0` is an owned process handle.
            #[allow(unsafe_code)]
            let _ = unsafe { WaitForSingleObject(proc_wait.process.0, wait_ms) };
            let mut code = 0u32;
            // SAFETY: queries exit code after wait.
            #[allow(unsafe_code)]
            if unsafe { GetExitCodeProcess(proc_wait.process.0, &mut code) }.is_ok() {
                Some(code as i32)
            } else {
                None
            }
        })
        .await
        .ok()
        .flatten();
    }

    if !wait_done {
        let _ = wait_task.await;
    }

    let mut stdout_captured: Option<(CapturedOutput, Vec<u8>)> = None;
    let mut stderr_captured: Option<(CapturedOutput, Vec<u8>)> = None;

    let _ = tokio::time::timeout(
        POST_KILL_DRAIN_TIMEOUT,
        drain_pipes(
            &mut stdout_task,
            &mut stderr_task,
            &mut stdout_captured,
            &mut stderr_captured,
        ),
    )
    .await;

    drain_stop.cancel();
    drain_pipes(
        &mut stdout_task,
        &mut stderr_task,
        &mut stdout_captured,
        &mut stderr_captured,
    )
    .await;

    if let Some(handle) = stdin_task {
        handle.abort();
    }

    let (stdout, stdout_bytes) = stdout_captured.unwrap_or_default();
    let (stderr, _) = stderr_captured.unwrap_or_default();

    Ok(finalize_run_output(
        exit_code,
        None,
        stdout,
        stdout_bytes,
        stderr,
        timed_out,
        cancelled,
        memory_watchdog_killed,
        options.memory_limit,
        peak_rss.load(Ordering::Relaxed),
        cpu_time,
        started.elapsed(),
    ))
}

fn read_blocking_stream(
    mut file: File,
    limit: usize,
    stream: OutputStream,
    on_output: Option<ChunkCallback>,
    stop: CancellationToken,
    keep_raw: bool,
) -> (CapturedOutput, Vec<u8>) {
    let mut capture = BoundedCapture::new(limit);
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];

    while !stop.is_cancelled() {
        match file.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if keep_raw {
                    let room = limit.saturating_sub(raw.len());
                    if room > 0 {
                        raw.extend_from_slice(&chunk[..room.min(chunk.len())]);
                    }
                    capture.total_bytes = capture.total_bytes.saturating_add(chunk.len() as u64);
                    if chunk.len() > room {
                        capture.truncated = true;
                    }
                } else {
                    capture.push(chunk);
                }
                if let Some(ref cb) = on_output {
                    cb(OutputChunk {
                        stream,
                        bytes: chunk.to_vec(),
                    });
                }
            }
        }
    }

    (capture.finish(), raw)
}

pub(super) fn configure_containment(wrapped: &mut CommandWrap, suspend_for_memory_job: bool) {
    let flags = if suspend_for_memory_job {
        CREATE_NO_WINDOW | CREATE_SUSPENDED
    } else {
        CREATE_NO_WINDOW
    };
    wrapped
        .wrap(CreationFlags(flags))
        .wrap(JobObject)
        .wrap(KillOnDrop);
}

pub(super) fn extract_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), None)
}

pub(super) async fn cleanup_after_direct_exit(
    child: &mut Box<dyn ChildWrapper>,
    _child_pid: Option<u32>,
    _grace: Duration,
) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(POST_KILL_WAIT, child.wait()).await;
}

pub(super) async fn terminate_tree(
    child: &mut Box<dyn ChildWrapper>,
    _child_pid: Option<u32>,
    grace: Duration,
) -> Option<ExitStatus> {
    let _ = child.start_kill();
    let wait_budget = POST_KILL_WAIT.max(grace.min(Duration::from_secs(5)));
    tokio::time::timeout(wait_budget, child.wait())
        .await
        .ok()
        .and_then(std::result::Result::ok)
}
