//! Minimal Win32 process-handle FFI layer for full-precision identity checks and race-free
//! termination.
//!
//! All Windows `unsafe` FFI calls in `legion-os` live in this module. Holding an open `HANDLE`
//! pins the OS PID against reuse between `GetProcessTimes` verification and `TerminateProcess`.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemTimeOfDayInformation,
};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, FILETIME,
    HANDLE, STILL_ACTIVE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_BASIC_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
};

use crate::procid::ProcessId;
use crate::{OsError, Result};

/// RAII wrapper that closes a Win32 `HANDLE` on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` is an owned, valid Win32 handle obtained from `OpenProcess` or
            // `CreateToolhelp32Snapshot`, and `Drop` closes it exactly once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// Parent/child PID pair from a ToolHelp32 process snapshot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessSnapshotEntry {
    pub(crate) pid: u32,
    pub(crate) parent_pid: u32,
}

/// Outcome of an atomic open-verify-terminate attempt against one [`ProcessId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminateOutcome {
    /// Verified full-precision creation time matched and `TerminateProcess` succeeded.
    Terminated,
    /// The process no longer exists or has already exited.
    AlreadyGone,
    /// A different process with a different creation time now holds the PID.
    IdentityMismatch,
    /// Access was denied when opening or terminating the process.
    AccessDenied,
}

/// An open Win32 process handle paired with its verified identity and parent PID read directly
/// from the handle.
///
/// Holding this value keeps the OS PID pinned so the kernel cannot reuse it while subsequent
/// tree-walk snapshots and termination waves are in progress.
pub(crate) struct PinnedProcess {
    handle: OwnedHandle,
    pub(crate) id: ProcessId,
    pub(crate) parent_pid: u32,
    can_terminate: bool,
}

impl PinnedProcess {
    /// Return `true` while this pinned process has not yet exited.
    pub(crate) fn is_running(&self) -> bool {
        match query_times_raw(self.handle.0, &format!("pid {}", self.id.pid)) {
            Ok((_, exit_ft)) => is_still_running(self.handle.0, exit_ft),
            Err(_) => false,
        }
    }

    /// Terminate this pinned process through its already-verified handle.
    pub(crate) fn terminate(&self) -> TerminateOutcome {
        if !self.is_running() {
            return TerminateOutcome::AlreadyGone;
        }
        if !self.can_terminate {
            return TerminateOutcome::AccessDenied;
        }

        // SAFETY: `self.handle.0` is a valid handle opened with `PROCESS_TERMINATE` whose
        // identity was verified while this handle pinned the PID against reuse.
        match unsafe { TerminateProcess(self.handle.0, 1) } {
            Ok(()) => TerminateOutcome::Terminated,
            Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                if !self.is_running() {
                    TerminateOutcome::AlreadyGone
                } else {
                    TerminateOutcome::AccessDenied
                }
            }
            Err(_) => TerminateOutcome::AccessDenied,
        }
    }
}

/// Result of attempting to open and pin one candidate PID during a process-tree walk.
pub(crate) enum OpenPinnedOutcome {
    /// Handle opened and creation time plus parent PID read from the pinned handle.
    Pinned(PinnedProcess),
    /// The PID does not exist or has already exited.
    Gone,
    /// Access was denied even for `PROCESS_QUERY_LIMITED_INFORMATION`.
    AccessDenied,
    /// Querying times or parent PID failed for this single process.
    Failed,
}

/// Read `InheritedFromUniqueProcessId` directly from an open handle with at least
/// `PROCESS_QUERY_LIMITED_INFORMATION`.
fn query_parent_pid_raw(handle: HANDLE) -> Result<u32> {
    let mut info = PROCESS_BASIC_INFORMATION::default();
    let mut returned = 0u32;
    let len = size_of::<PROCESS_BASIC_INFORMATION>() as u32;

    // SAFETY: `handle` is a valid process handle with at least `PROCESS_QUERY_LIMITED_INFORMATION`
    // (sufficient for `ProcessBasicInformation` on Windows Vista+), `info` has exact size `len`,
    // and `returned` is a valid mutable `u32`.
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            ProcessBasicInformation,
            (&mut info as *mut PROCESS_BASIC_INFORMATION).cast::<c_void>(),
            len,
            &mut returned,
        )
    };
    status.ok().map_err(|err| OsError::Platform {
        detail: format!("NtQueryInformationProcess(ProcessBasicInformation) failed: {err}"),
    })?;
    u32::try_from(info.InheritedFromUniqueProcessId).map_err(|_| OsError::Platform {
        detail: format!(
            "InheritedFromUniqueProcessId {} exceeds u32 range",
            info.InheritedFromUniqueProcessId
        ),
    })
}

/// Open `pid` (requesting `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`, falling back to
/// `PROCESS_QUERY_LIMITED_INFORMATION` if termination is denied) and read both its creation time
/// and its `parent_pid` directly from the held handle.
pub(crate) fn open_pinned(pid: u32, include_exited: bool) -> OpenPinnedOutcome {
    let terminate_rights = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
    // SAFETY: `OpenProcess` takes plain integer arguments and any returned handle is immediately
    // wrapped in `OwnedHandle`.
    let (handle, can_terminate) = match unsafe { OpenProcess(terminate_rights, false, pid) } {
        Ok(h) => (OwnedHandle(h), true),
        Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            return OpenPinnedOutcome::Gone;
        }
        Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            // Fall back to query-only access so we can still verify identity, walk children, and
            // record the process as a survivor if it cannot be terminated.
            // SAFETY: Same plain-integer `OpenProcess` contract with query-only rights.
            match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
                Ok(h) => (OwnedHandle(h), false),
                Err(err2) if err2.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
                    return OpenPinnedOutcome::Gone;
                }
                Err(err2) if err2.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                    return OpenPinnedOutcome::AccessDenied;
                }
                Err(_) => return OpenPinnedOutcome::Failed,
            }
        }
        Err(_) => return OpenPinnedOutcome::Failed,
    };

    let Ok((creation, exit_ft)) = query_times_raw(handle.0, &format!("pid {pid}")) else {
        return OpenPinnedOutcome::Failed;
    };
    if !include_exited && !is_still_running(handle.0, exit_ft) {
        return OpenPinnedOutcome::Gone;
    }
    let Ok(parent_pid) = query_parent_pid_raw(handle.0) else {
        return OpenPinnedOutcome::Failed;
    };

    OpenPinnedOutcome::Pinned(PinnedProcess {
        handle,
        id: ProcessId::new(pid, creation),
        parent_pid,
        can_terminate,
    })
}

#[inline]
fn filetime_to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// Read `(creation_filetime, exit_filetime)` from a valid Win32 process handle.
fn query_times_raw(handle: HANDLE, context: &str) -> Result<(u64, u64)> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    // SAFETY: `handle` is a valid process handle with at least `PROCESS_QUERY_LIMITED_INFORMATION`
    // and all four output pointers refer to initialized stack `FILETIME` structs.
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }.map_err(
        |err| OsError::Platform {
            detail: format!("GetProcessTimes failed for {context}: {err}"),
        },
    )?;

    let creation_ticks = filetime_to_u64(creation);
    if creation_ticks == 0 {
        return Err(OsError::Platform {
            detail: format!("GetProcessTimes returned zero creation time for {context}"),
        });
    }
    Ok((creation_ticks, filetime_to_u64(exit)))
}

fn is_still_running(handle: HANDLE, exit_filetime: u64) -> bool {
    if exit_filetime != 0 {
        return false;
    }
    let mut exit_code = 0u32;
    // SAFETY: `handle` is a valid process handle with query access and `exit_code` is a valid
    // mutable `u32`.
    let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) }.is_ok();
    ok && exit_code == STILL_ACTIVE.0 as u32
}

/// Layout of Windows `SYSTEM_TIMEOFDAY_INFORMATION` (48 bytes on all supported Windows versions).
#[repr(C)]
#[derive(Default)]
struct SystemTimeOfDayInfo {
    boot_time: i64,
    current_time: i64,
    time_zone_bias: i64,
    time_zone_id: u32,
    reserved: u32,
    boot_time_bias: u64,
    sleep_time_bias: u64,
}

/// Query the clock-step-invariant Windows boot timestamp (`BootTime - BootTimeBias`).
///
/// Whenever `NtSetSystemTime` adjusts the system wall clock (NTP or dual-boot RTC jump), the
/// kernel shifts both `KeBootTime` and `KeBootTimeBias` by the exact same delta, so subtracting
/// `boot_time_bias` recovers the original boot timestamp unchanged across wall-clock steps of
/// any magnitude.
pub(crate) fn query_unbiased_boot_time() -> Result<u64> {
    let mut info = SystemTimeOfDayInfo::default();
    let mut returned = 0u32;
    let buf_len = size_of::<SystemTimeOfDayInfo>() as u32;

    // SAFETY: `info` is an initialized 48-byte `SystemTimeOfDayInfo` struct matching the Windows
    // `SYSTEM_TIMEOFDAY_INFORMATION` ABI, `buf_len` is its exact byte size, and `returned` is a
    // valid mutable `u32`.
    let status = unsafe {
        NtQuerySystemInformation(
            SystemTimeOfDayInformation,
            (&mut info as *mut SystemTimeOfDayInfo).cast::<c_void>(),
            buf_len,
            &mut returned,
        )
    };
    if let Err(err) = status.ok() {
        return Err(OsError::Platform {
            detail: format!("NtQuerySystemInformation(SystemTimeOfDayInformation) failed: {err}"),
        });
    }
    if (returned as usize) < size_of::<SystemTimeOfDayInfo>() || info.boot_time <= 0 {
        return Err(OsError::Platform {
            detail: format!(
                "NtQuerySystemInformation(SystemTimeOfDayInformation) returned incomplete data (bytes={returned}, boot_time={})",
                info.boot_time
            ),
        });
    }

    let unbiased = crate::procid::unbiased_windows_boot_ticks(info.boot_time, info.boot_time_bias);
    if unbiased == 0 {
        return Err(OsError::Platform {
            detail: "NtQuerySystemInformation produced zero unbiased boot time".into(),
        });
    }
    Ok(unbiased)
}

/// Read the exact 100-ns `FILETIME` creation timestamp from a borrowed process handle (for example
/// `std::process::Child::as_handle()`).
pub(crate) fn creation_time_from_handle(handle: BorrowedHandle<'_>) -> Result<u64> {
    let raw = HANDLE(handle.as_raw_handle());
    let (creation, _exit) = query_times_raw(raw, "child process handle")?;
    Ok(creation)
}

/// Query full-precision [`ProcessId`] (`pid` + 100-ns `FILETIME` creation time) for `pid`.
///
/// Returns `Ok(None)` when no process with `pid` exists or when it has already exited and
/// `include_exited` is `false`.
pub(crate) fn query_process_identity(pid: u32, include_exited: bool) -> Result<Option<ProcessId>> {
    // SAFETY: `OpenProcess` takes only plain integer flags and returns an owned handle or an error.
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => OwnedHandle(h),
        Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(None),
        Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            return Err(OsError::PermissionDenied {
                detail: format!("cannot query creation time for process {pid}"),
                source: None,
            });
        }
        Err(err) => {
            return Err(OsError::Platform {
                detail: format!("OpenProcess failed for pid {pid}: {err}"),
            });
        }
    };

    let (creation, exit_ft) = query_times_raw(handle.0, &format!("pid {pid}"))?;
    if !include_exited && !is_still_running(handle.0, exit_ft) {
        return Ok(None);
    }
    Ok(Some(ProcessId::new(pid, creation)))
}

/// Capture a snapshot of all `(pid, parent_pid)` entries currently in the Windows process table.
pub(crate) fn snapshot_processes() -> Result<Vec<ProcessSnapshotEntry>> {
    // SAFETY: `CreateToolhelp32Snapshot` with `TH32CS_SNAPPROCESS` takes plain integer arguments
    // and returns an owned snapshot handle wrapped immediately in `OwnedHandle`.
    let snapshot = OwnedHandle(
        unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(|err| {
            OsError::Platform {
                detail: format!("CreateToolhelp32Snapshot failed: {err}"),
            }
        })?,
    );

    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };

    // SAFETY: `snapshot.0` is a valid ToolHelp32 snapshot handle and `entry.dwSize` is initialized
    // to `size_of::<PROCESSENTRY32W>()` as required by `Process32FirstW`.
    if let Err(err) = unsafe { Process32FirstW(snapshot.0, &mut entry) } {
        if err.code() == ERROR_NO_MORE_FILES.to_hresult() {
            return Ok(Vec::new());
        }
        return Err(OsError::Platform {
            detail: format!("Process32FirstW failed: {err}"),
        });
    }

    let mut out = Vec::new();
    loop {
        out.push(ProcessSnapshotEntry {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
        });

        // SAFETY: `snapshot.0` is a valid snapshot handle and `entry` remains properly sized.
        match unsafe { Process32NextW(snapshot.0, &mut entry) } {
            Ok(()) => {}
            Err(err) if err.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(err) => {
                return Err(OsError::Platform {
                    detail: format!("Process32NextW failed: {err}"),
                });
            }
        }
    }

    Ok(out)
}

/// Open `identity.pid` with `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`, verify its
/// exact 100-ns `FILETIME` creation time while the held handle pins the PID against reuse, and
/// call `TerminateProcess` through that handle.
#[allow(dead_code)]
pub(crate) fn open_verify_terminate(identity: &ProcessId) -> Result<TerminateOutcome> {
    let desired = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION;
    // SAFETY: `OpenProcess` takes plain integer arguments and the returned handle is immediately
    // wrapped in `OwnedHandle`, pinning the PID until drop.
    let handle = match unsafe { OpenProcess(desired, false, identity.pid) } {
        Ok(h) => OwnedHandle(h),
        Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            return Ok(TerminateOutcome::AlreadyGone);
        }
        Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            return Ok(TerminateOutcome::AccessDenied);
        }
        Err(err) => {
            return Err(OsError::Platform {
                detail: format!("OpenProcess failed for pid {}: {err}", identity.pid),
            });
        }
    };

    let (creation, exit_ft) = query_times_raw(handle.0, &format!("pid {}", identity.pid))?;
    if !is_still_running(handle.0, exit_ft) {
        return Ok(TerminateOutcome::AlreadyGone);
    }
    if creation != identity.start_time {
        return Ok(TerminateOutcome::IdentityMismatch);
    }

    // SAFETY: `handle.0` is a valid handle opened with `PROCESS_TERMINATE` whose identity was
    // verified against `identity.start_time` while this handle pinned the PID.
    match unsafe { TerminateProcess(handle.0, 1) } {
        Ok(()) => Ok(TerminateOutcome::Terminated),
        Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            // `TerminateProcess` returns ERROR_ACCESS_DENIED if the process finished exiting
            // concurrently right before the call; re-check liveness on the pinned handle.
            let (_, post_exit_ft) = query_times_raw(handle.0, &format!("pid {}", identity.pid))
                .unwrap_or((creation, 0));
            if !is_still_running(handle.0, post_exit_ft) {
                Ok(TerminateOutcome::AlreadyGone)
            } else {
                Ok(TerminateOutcome::AccessDenied)
            }
        }
        Err(err) => Err(OsError::Platform {
            detail: format!("TerminateProcess failed for pid {}: {err}", identity.pid),
        }),
    }
}
