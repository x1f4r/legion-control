//! Windows Job Object containment and hard process/job memory limits for [`super::run`].
//!
//! Each child is assigned to a dedicated Windows Job Object with `KILL_ON_JOB_CLOSE`. When a
//! `memory_limit` is requested, the child is created suspended (`CREATE_SUSPENDED`), assigned to a
//! nested Job Object with `JOB_OBJECT_LIMIT_PROCESS_MEMORY` and `JOB_OBJECT_LIMIT_JOB_MEMORY`
//! sized with the same headroom rule (`memory_limit + rlimit_as_headroom`) before any child code
//! executes, and only then resumed.

use std::mem::size_of;
use std::process::ExitStatus;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
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
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenProcess, OpenThread, PROCESS_SET_QUOTA,
    PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME, TerminateProcess,
};

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

pub(super) fn configure_pre_exec_memory_limit(
    _cmd: &mut tokio::process::Command,
    _memory_limit: Option<u64>,
    _rlimit_as_headroom: u64,
) {
    // Windows enforces memory limits via the suspended-create + Job Object path in `TreeDropGuard::new`.
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
