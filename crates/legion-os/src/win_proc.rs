//! Minimal Win32 process-handle FFI layer for full-precision identity checks and race-free
//! termination.
//!
//! All Windows `unsafe` FFI calls in `legion-os` live in this module. Holding an open `HANDLE`
//! pins the OS PID against reuse between `GetProcessTimes` verification and `TerminateProcess`.

#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString, c_void};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemTimeOfDayInformation,
};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_HANDLE_OPTIONS, DUPLICATE_SAME_ACCESS, DuplicateHandle,
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, FILETIME,
    GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, STILL_ACTIVE,
};
use windows::Win32::Security::{EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, MOVE_FILE_FLAGS, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING, SYNCHRONIZE,
};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOBOBJECT_BASIC_PROCESS_ID_LIST,
    JobObjectBasicProcessIdList, OpenJobObjectW, QueryInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQueryEx,
};
use windows::Win32::System::SystemServices::{JOB_OBJECT_QUERY, JOB_OBJECT_TERMINATE};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, IsProcessCritical, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcess, OpenProcessToken, PEB, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROCESS_BASIC_INFORMATION, PROCESS_INFORMATION, PROCESS_NAME_FORMAT, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ,
    QueryFullProcessImageNameW, RTL_USER_PROCESS_PARAMETERS, ResumeThread, STARTF_USESTDHANDLES,
    STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::procid::{EnvMarkerCheck, ProcessId, windows_env_utf16_has_marker};
use crate::{OsError, Result};

/// Serializes direct Windows detach spawns while temporary inheritable stdio duplicates exist
/// across `CreateProcessW`.
///
/// Base log and `NUL` handles are opened non-inheritable; only short-lived duplicates created
/// inside this lock are marked inheritable for `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` and closed
/// immediately after `CreateProcessW` returns, before releasing the lock. (`std`'s own process
/// spawn mutex is private, so this both serializes crate-internal spawns and shrinks the
/// inheritable window against external `std::process::Command` spawns to the single syscall.)
static DIRECT_SPAWN_LOCK: Mutex<()> = Mutex::new(());

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

/// Open `pid` with `PROCESS_QUERY_LIMITED_INFORMATION` only (strictly read-only) and read both
/// its creation time and its `parent_pid` directly from the held handle.
pub(crate) fn open_pinned_query_only(pid: u32, include_exited: bool) -> OpenPinnedOutcome {
    // SAFETY: `OpenProcess` takes plain integer arguments and any returned handle is immediately
    // wrapped in `OwnedHandle`.
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => OwnedHandle(h),
        Err(err) if err.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            return OpenPinnedOutcome::Gone;
        }
        Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            return OpenPinnedOutcome::AccessDenied;
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
        can_terminate: false,
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

/// Error returned by [`spawn_detached_direct`], distinguishing a Job Object breakaway denial
/// (which triggers the WMI fallback) from all other errors.
pub(crate) enum DirectSpawnError {
    /// `CreateProcessW` failed with `ERROR_ACCESS_DENIED` because the caller's Job Object forbids
    /// `CREATE_BREAKAWAY_FROM_JOB`.
    BreakawayRefused,
    /// Any other failure opening log files or starting the process.
    Other(OsError),
}

struct ProcThreadAttrList {
    _storage: Vec<usize>,
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl Drop for ProcThreadAttrList {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was initialized by `InitializeProcThreadAttributeList` into
        // `self._storage` and is deleted exactly once before `_storage` is freed.
        unsafe { DeleteProcThreadAttributeList(self.ptr) };
    }
}

fn to_wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Atomically rename `from` to `to` without `MOVEFILE_REPLACE_EXISTING` so Windows fails with
/// `ERROR_ALREADY_EXISTS` / `ERROR_FILE_EXISTS` if `to` already exists.
pub(crate) fn move_file_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let from_wide = to_wide_null(from.as_os_str());
    let to_wide = to_wide_null(to.as_os_str());
    // SAFETY: Both pointers reference NUL-terminated UTF-16 buffers that remain valid for the
    // duration of the `MoveFileExW` call.
    match unsafe {
        MoveFileExW(
            PCWSTR::from_raw(from_wide.as_ptr()),
            PCWSTR::from_raw(to_wide.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    } {
        Ok(()) => Ok(()),
        Err(_) => Err(std::io::Error::last_os_error()),
    }
}

fn open_non_inheritable_nul(access: u32) -> Result<OwnedHandle> {
    let nul_wide = to_wide_null(OsStr::new("NUL"));
    // SAFETY: `nul_wide` is NUL-terminated UTF-16; `None` security attributes mark the handle
    // non-inheritable so concurrent spawns on other threads can never inherit it.
    let handle = unsafe {
        CreateFileW(
            PCWSTR::from_raw(nul_wide.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("cannot open NUL device: {err}"),
    })?;
    Ok(OwnedHandle(handle))
}

fn open_non_inheritable_append(path: &Path) -> Result<OwnedHandle> {
    let path_wide = to_wide_null(path.as_os_str());
    // SAFETY: `path_wide` is NUL-terminated UTF-16; `None` security attributes mark the handle
    // non-inheritable until a short-lived duplicate is created under `DIRECT_SPAWN_LOCK`.
    let handle = unsafe {
        CreateFileW(
            PCWSTR::from_raw(path_wide.as_ptr()),
            (FILE_APPEND_DATA | SYNCHRONIZE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("cannot open log file {}: {err}", path.display()),
    })?;
    Ok(OwnedHandle(handle))
}

fn duplicate_inheritable(source: HANDLE) -> Result<OwnedHandle> {
    let mut dup = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns the current-process pseudo-handle, `source` is a valid
    // open handle owned by this process, and `dup` is immediately wrapped in `OwnedHandle`.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, source, me, &mut dup, 0, true, DUPLICATE_SAME_ACCESS)
    }
    .map_err(|err| OsError::Platform {
        detail: format!("DuplicateHandle for inheritable stdio failed: {err}"),
    })?;
    Ok(OwnedHandle(dup))
}

fn build_unicode_env_block(
    env_remove: &[OsString],
    overrides: &BTreeMap<OsString, OsString>,
) -> Vec<u16> {
    let normalize_key = |k: &OsStr| -> Vec<u16> {
        k.encode_wide()
            .map(|u| {
                if (b'a' as u16..=b'z' as u16).contains(&u) {
                    u - 32
                } else {
                    u
                }
            })
            .collect()
    };

    let removed: Vec<Vec<u16>> = env_remove.iter().map(|k| normalize_key(k)).collect();
    let mut merged: BTreeMap<Vec<u16>, (Vec<u16>, Vec<u16>)> = BTreeMap::new();

    for (k, v) in std::env::vars_os() {
        let norm = normalize_key(&k);
        if removed.contains(&norm) {
            continue;
        }
        merged.insert(norm, (k.encode_wide().collect(), v.encode_wide().collect()));
    }
    for (k, v) in overrides {
        let norm = normalize_key(k);
        merged.insert(norm, (k.encode_wide().collect(), v.encode_wide().collect()));
    }

    let mut block = Vec::new();
    for (k_wide, v_wide) in merged.into_values() {
        block.extend_from_slice(&k_wide);
        block.push(b'=' as u16);
        block.extend_from_slice(&v_wide);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

fn duplicate_inheritable_with_access(source: HANDLE, desired_access: u32) -> Result<OwnedHandle> {
    let mut dup = HANDLE::default();
    // SAFETY: `GetCurrentProcess` returns the current-process pseudo-handle, `source` is a valid
    // open handle owned by this process, and `dup` is immediately wrapped in `OwnedHandle`.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(
            me,
            source,
            me,
            &mut dup,
            desired_access,
            true,
            DUPLICATE_HANDLE_OPTIONS(0),
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("DuplicateHandle for inheritable job handle failed: {err}"),
    })?;
    Ok(OwnedHandle(dup))
}

fn create_named_job(job_name: &str) -> Option<OwnedHandle> {
    let wide = to_wide_null(OsStr::new(job_name));
    // SAFETY: `wide` is NUL-terminated UTF-16 and `None` uses default non-inheritable security
    // attributes; returned handle is immediately wrapped in `OwnedHandle`.
    let handle = unsafe { CreateJobObjectW(None, PCWSTR::from_raw(wide.as_ptr())) }.ok()?;
    let owned = OwnedHandle(handle);
    // Refuse to attach to a pre-existing job object if the name somehow already existed.
    // SAFETY: `GetLastError` reads thread-local error state set by `CreateJobObjectW`.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return None;
    }
    Some(owned)
}

/// Spawn a detached Windows process via `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
/// restricted strictly to short-lived inheritable duplicates of the three standard handles
/// (`stdin` null, `stdout` and `stderr` log files or null) plus, when `job_name` is supplied and a
/// named Job Object is created, one read-only `JOB_OBJECT_QUERY` keepalive handle to that job so
/// its name remains openable via `OpenJobObjectW` across process exits.
///
/// The base log, `NUL`, and Job Object handles are opened non-inheritable before acquiring
/// `DIRECT_SPAWN_LOCK`; inheritable duplicates exist only inside `DIRECT_SPAWN_LOCK` across
/// `CreateProcessW` and are closed immediately afterwards so they cannot leak into other spawns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_detached_direct(
    program_display: &str,
    application_name: Option<&str>,
    command_line: &str,
    cwd: Option<&Path>,
    env_remove: &[OsString],
    env_overrides: &BTreeMap<OsString, OsString>,
    stdout_path: Option<&Path>,
    stderr_path: Option<&Path>,
    job_name: Option<&str>,
) -> std::result::Result<(ProcessId, Option<String>), DirectSpawnError> {
    // Open base stdio handles non-inheritable before taking the spawn lock.
    let stdin_base = open_non_inheritable_nul(GENERIC_READ.0).map_err(DirectSpawnError::Other)?;

    let shared_logs = match (stdout_path, stderr_path) {
        (Some(out), Some(err)) => {
            crate::detach::same_windows_path(&out.to_string_lossy(), &err.to_string_lossy())
        }
        _ => false,
    };

    let stdout_base = match stdout_path {
        Some(path) => open_non_inheritable_append(path).map_err(DirectSpawnError::Other)?,
        None => open_non_inheritable_nul(GENERIC_WRITE.0).map_err(DirectSpawnError::Other)?,
    };
    let stderr_base = if shared_logs || (stdout_path.is_none() && stderr_path.is_none()) {
        None
    } else {
        Some(match stderr_path {
            Some(path) => open_non_inheritable_append(path).map_err(DirectSpawnError::Other)?,
            None => open_non_inheritable_nul(GENERIC_WRITE.0).map_err(DirectSpawnError::Other)?,
        })
    };

    // Create the non-inheritable named Job Object before taking the spawn lock. If creation fails,
    // fall back cleanly to the `LEGION_DETACH_ID` environment-marker rule without failing launch.
    let job_base = job_name.and_then(|name| create_named_job(name).map(|h| (h, name.to_owned())));

    let app_wide = application_name.map(|app| to_wide_null(OsStr::new(app)));
    let app_pcwstr = match app_wide {
        Some(ref buf) => PCWSTR::from_raw(buf.as_ptr()),
        None => PCWSTR::null(),
    };
    let mut cmdline_wide: Vec<u16> = command_line
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let env_block = build_unicode_env_block(env_remove, env_overrides);
    let cwd_wide = cwd.map(|dir| to_wide_null(dir.as_os_str()));
    let cwd_pcwstr = match cwd_wide {
        Some(ref buf) => PCWSTR::from_raw(buf.as_ptr()),
        None => PCWSTR::null(),
    };

    let mut attr_size = 0usize;
    // SAFETY: Querying the required attribute-list size passes `None` and a valid mutable `usize`.
    let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut attr_size) };
    if attr_size == 0 {
        return Err(DirectSpawnError::Other(OsError::Platform {
            detail: "InitializeProcThreadAttributeList returned zero buffer size".into(),
        }));
    }

    let mut storage = vec![0usize; attr_size.div_ceil(size_of::<usize>())];
    let attr_ptr = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast::<c_void>());
    // SAFETY: `storage` is pointer-aligned with at least `attr_size` bytes.
    unsafe { InitializeProcThreadAttributeList(Some(attr_ptr), 1, None, &mut attr_size) }.map_err(
        |err| {
            DirectSpawnError::Other(OsError::Platform {
                detail: format!("InitializeProcThreadAttributeList failed: {err}"),
            })
        },
    )?;
    let attr_list = ProcThreadAttrList {
        _storage: storage,
        ptr: attr_ptr,
    };

    let mut flags = CREATE_BREAKAWAY_FROM_JOB
        | CREATE_NO_WINDOW
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT;
    if job_base.is_some() {
        // Start suspended so we assign the child into the named Job Object before it can spawn
        // any descendants or exit.
        flags |= CREATE_SUSPENDED;
    }

    let mut pi = PROCESS_INFORMATION::default();
    let create_res = {
        let _spawn_guard = DIRECT_SPAWN_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let stdin_dup = duplicate_inheritable(stdin_base.0).map_err(DirectSpawnError::Other)?;
        let stdout_dup = duplicate_inheritable(stdout_base.0).map_err(DirectSpawnError::Other)?;
        let stderr_dup = match stderr_base {
            Some(ref base) => Some(duplicate_inheritable(base.0).map_err(DirectSpawnError::Other)?),
            None => None,
        };
        let job_keepalive_dup = match job_base {
            Some((ref job, _)) => duplicate_inheritable_with_access(job.0, JOB_OBJECT_QUERY).ok(),
            None => None,
        };

        let h_in = stdin_dup.0;
        let h_out = stdout_dup.0;
        let h_err = stderr_dup.as_ref().map_or(h_out, |h| h.0);

        let mut handle_list = Vec::with_capacity(4);
        for h in [h_in, h_out, h_err] {
            if !handle_list.contains(&h) {
                handle_list.push(h);
            }
        }
        if let Some(ref job_dup) = job_keepalive_dup
            && !handle_list.contains(&job_dup.0)
        {
            handle_list.push(job_dup.0);
        }

        // SAFETY: `attr_list.ptr` is initialized for 1 attribute and `handle_list` stays alive
        // through the `CreateProcessW` call below.
        unsafe {
            UpdateProcThreadAttribute(
                attr_list.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handle_list.as_ptr().cast::<c_void>()),
                handle_list.len() * size_of::<HANDLE>(),
                None,
                None,
            )
        }
        .map_err(|err| {
            DirectSpawnError::Other(OsError::Platform {
                detail: format!("UpdateProcThreadAttribute(HANDLE_LIST) failed: {err}"),
            })
        })?;

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = h_in;
        si.StartupInfo.hStdOutput = h_out;
        si.StartupInfo.hStdError = h_err;
        si.lpAttributeList = attr_list.ptr;

        // SAFETY: All pointers (`app_wide`, `cmdline_wide`, `env_block`, `cwd_wide`, `si`,
        // `attr_list`, `handle_list`) remain live and properly aligned/terminated across
        // `CreateProcessW`, and `si.StartupInfo.cb == size_of::<STARTUPINFOEXW>()` matches
        // `EXTENDED_STARTUPINFO_PRESENT`.
        let res = unsafe {
            CreateProcessW(
                app_pcwstr,
                Some(PWSTR::from_raw(cmdline_wide.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(env_block.as_ptr().cast::<c_void>()),
                cwd_pcwstr,
                &si.StartupInfo,
                &mut pi,
            )
        };

        // Close all inheritable duplicates before releasing `DIRECT_SPAWN_LOCK`.
        drop(attr_list);
        drop(handle_list);
        drop(job_keepalive_dup);
        drop(stderr_dup);
        drop(stdout_dup);
        drop(stdin_dup);
        res
    };

    drop(stderr_base);
    drop(stdout_base);
    drop(stdin_base);

    if let Err(err) = create_res {
        if err.code() == ERROR_ACCESS_DENIED.to_hresult() {
            return Err(DirectSpawnError::BreakawayRefused);
        }
        if err.code() == ERROR_FILE_NOT_FOUND.to_hresult()
            || err.code() == ERROR_PATH_NOT_FOUND.to_hresult()
        {
            return Err(DirectSpawnError::Other(OsError::ProgramNotFound {
                program: program_display.to_owned(),
            }));
        }
        return Err(DirectSpawnError::Other(OsError::Platform {
            detail: format!("CreateProcessW failed for `{program_display}`: {err}"),
        }));
    }

    let thread_handle = OwnedHandle(pi.hThread);
    let proc_handle = OwnedHandle(pi.hProcess);

    let assigned_job_name = if let Some((job_handle, name)) = job_base {
        // SAFETY: `job_handle.0` and `proc_handle.0` are valid open handles.
        let assigned = unsafe { AssignProcessToJobObject(job_handle.0, proc_handle.0) }.is_ok();
        // SAFETY: `thread_handle.0` is the suspended primary thread from `CreateProcessW`.
        let resume_rc = unsafe { ResumeThread(thread_handle.0) };
        if resume_rc == u32::MAX {
            // SAFETY: Avoid leaking a permanently suspended child process if resume fails.
            let _ = unsafe { TerminateProcess(proc_handle.0, 1) };
            return Err(DirectSpawnError::Other(OsError::Platform {
                detail: format!("ResumeThread failed for `{program_display}`"),
            }));
        }
        if assigned { Some(name) } else { None }
    } else {
        None
    };

    let (creation, _) = query_times_raw(proc_handle.0, "detached child process")
        .map_err(DirectSpawnError::Other)?;
    Ok((ProcessId::new(pi.dwProcessId, creation), assigned_job_name))
}

/// Query all PIDs currently assigned to the named Job Object `job_name`.
///
/// Returns `Ok(None)` when the named job does not exist (for example after every process in the
/// job has exited and closed its handle) or cannot be opened.
pub(crate) fn query_job_active_pids(job_name: &str) -> Result<Option<Vec<u32>>> {
    let wide = to_wide_null(OsStr::new(job_name));
    // SAFETY: `wide` is NUL-terminated UTF-16; returned handle is wrapped in `OwnedHandle`.
    let job =
        match unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, false, PCWSTR::from_raw(wide.as_ptr())) } {
            Ok(h) => OwnedHandle(h),
            Err(err)
                if err.code() == ERROR_FILE_NOT_FOUND.to_hresult()
                    || err.code() == ERROR_ACCESS_DENIED.to_hresult() =>
            {
                return Ok(None);
            }
            Err(_) => return Ok(None),
        };
    query_job_pids_raw(job.0).map(Some)
}

/// Terminate every process inside the named Job Object `job_name` via `TerminateJobObject` and
/// return the live [`ProcessId`] snapshot captured just before termination so the caller can wait
/// for them to settle.
///
/// Returns `Ok(None)` when the named job does not exist or cannot be opened (caller should fall
/// back to root + marker inspection).
pub(crate) fn terminate_named_job(job_name: &str) -> Result<Option<Vec<ProcessId>>> {
    let wide = to_wide_null(OsStr::new(job_name));
    let access = JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY;
    // SAFETY: `wide` is NUL-terminated UTF-16; returned handle is wrapped in `OwnedHandle`.
    let job = match unsafe { OpenJobObjectW(access, false, PCWSTR::from_raw(wide.as_ptr())) } {
        Ok(h) => OwnedHandle(h),
        Err(_) => return Ok(None),
    };

    let own_pid = std::process::id();
    let pids = query_job_pids_raw(job.0)?;
    let mut tracked = Vec::with_capacity(pids.len());
    for pid in pids {
        if pid == 0 || pid == own_pid {
            continue;
        }
        if let Ok(Some(id)) = query_process_identity(pid, false) {
            tracked.push(id);
        }
    }

    // SAFETY: `job.0` was opened with `JOB_OBJECT_TERMINATE`.
    unsafe { TerminateJobObject(job.0, 1) }.map_err(|err| OsError::Platform {
        detail: format!("TerminateJobObject failed for `{job_name}`: {err}"),
    })?;
    Ok(Some(tracked))
}

fn query_job_pids_raw(job: HANDLE) -> Result<Vec<u32>> {
    let own_pid = std::process::id();
    let mut capacity: usize = 64;

    for _ in 0..5 {
        let header_words =
            size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>().div_ceil(size_of::<usize>());
        let total_words = header_words.saturating_add(capacity);
        let mut storage = vec![0usize; total_words];
        let byte_len = (total_words * size_of::<usize>()) as u32;

        // SAFETY: `storage` is `usize`-aligned and sized for `JOBOBJECT_BASIC_PROCESS_ID_LIST`
        // plus `capacity` trailing `usize` PID slots.
        let res = unsafe {
            QueryInformationJobObject(
                Some(job),
                JobObjectBasicProcessIdList,
                storage.as_mut_ptr().cast::<c_void>(),
                byte_len,
                None,
            )
        };

        // SAFETY: `storage` starts with a properly aligned `JOBOBJECT_BASIC_PROCESS_ID_LIST`.
        let list_ptr = storage.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
        let (assigned, in_list) = unsafe {
            (
                (*list_ptr).NumberOfAssignedProcesses as usize,
                (*list_ptr).NumberOfProcessIdsInList as usize,
            )
        };

        match res {
            Ok(()) => {
                // SAFETY: `QueryInformationJobObject` populated `in_list` entries starting at
                // `ProcessIdList` within `storage`.
                let base_pid_ptr =
                    unsafe { std::ptr::addr_of!((*list_ptr).ProcessIdList) as *const usize };
                let max_slots = (total_words * size_of::<usize>()
                    - (base_pid_ptr as usize - storage.as_ptr() as usize))
                    / size_of::<usize>();
                let count = in_list.min(max_slots);
                let mut out = Vec::with_capacity(count);
                for idx in 0..count {
                    // SAFETY: `idx < count <= max_slots` stays strictly inside `storage`.
                    let raw = unsafe { *base_pid_ptr.add(idx) };
                    if let Ok(pid) = u32::try_from(raw)
                        && pid != 0
                        && pid != own_pid
                    {
                        out.push(pid);
                    }
                }
                return Ok(out);
            }
            Err(err) if err.code() == ERROR_MORE_DATA.to_hresult() => {
                capacity = assigned.max(capacity.saturating_mul(2)).saturating_add(16);
            }
            Err(err) => {
                return Err(OsError::Platform {
                    detail: format!("QueryInformationJobObject(BasicProcessIdList) failed: {err}"),
                });
            }
        }
    }

    Err(OsError::Platform {
        detail: "QueryInformationJobObject(BasicProcessIdList) exceeded retry limit".into(),
    })
}

/// Extension of `windows::Win32::System::Threading::RTL_USER_PROCESS_PARAMETERS` exposing the
/// immediately-following `Environment` pointer (`0x80` on 64-bit, `0x48` on 32-bit Windows).
#[repr(C)]
struct RtlUserProcessParametersWithEnv {
    base: RTL_USER_PROCESS_PARAMETERS,
    environment: *mut c_void,
}

/// Inspect `pid`'s PEB environment block via `ReadProcessMemory` to check for `key=expected`.
///
/// Returns [`EnvMarkerCheck::Unverifiable`] whenever the process cannot be opened with
/// `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` or its PEB/environment memory cannot be read.
pub(crate) fn check_process_env_marker(pid: u32, key: &str, expected: &str) -> EnvMarkerCheck {
    const MAX_ENV_BYTES: usize = 1024 * 1024;

    // SAFETY: `OpenProcess` takes plain integer flags and any handle is wrapped in `OwnedHandle`.
    let handle =
        match unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) } {
            Ok(h) => OwnedHandle(h),
            Err(_) => return EnvMarkerCheck::Unverifiable,
        };

    let mut pbi = PROCESS_BASIC_INFORMATION::default();
    let mut returned = 0u32;
    // SAFETY: `handle.0` has `PROCESS_QUERY_INFORMATION`; `pbi` matches `ProcessBasicInformation`.
    let status = unsafe {
        NtQueryInformationProcess(
            handle.0,
            ProcessBasicInformation,
            (&mut pbi as *mut PROCESS_BASIC_INFORMATION).cast::<c_void>(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut returned,
        )
    };
    if status.is_err() || pbi.PebBaseAddress.is_null() {
        return EnvMarkerCheck::Unverifiable;
    }

    let mut peb: PEB = unsafe { std::mem::zeroed() };
    let mut read_bytes = 0usize;
    // SAFETY: Reading `size_of::<PEB>()` bytes from the remote `PebBaseAddress` into stack `peb`.
    if unsafe {
        ReadProcessMemory(
            handle.0,
            pbi.PebBaseAddress.cast::<c_void>(),
            (&mut peb as *mut PEB).cast::<c_void>(),
            size_of::<PEB>(),
            Some(&mut read_bytes),
        )
    }
    .is_err()
        || read_bytes < size_of::<PEB>()
        || peb.ProcessParameters.is_null()
    {
        return EnvMarkerCheck::Unverifiable;
    }

    let mut params: RtlUserProcessParametersWithEnv = unsafe { std::mem::zeroed() };
    read_bytes = 0;
    // SAFETY: Reading `RtlUserProcessParametersWithEnv` from remote `peb.ProcessParameters`.
    if unsafe {
        ReadProcessMemory(
            handle.0,
            peb.ProcessParameters.cast::<c_void>(),
            (&mut params as *mut RtlUserProcessParametersWithEnv).cast::<c_void>(),
            size_of::<RtlUserProcessParametersWithEnv>(),
            Some(&mut read_bytes),
        )
    }
    .is_err()
        || read_bytes < size_of::<RtlUserProcessParametersWithEnv>()
        || params.environment.is_null()
    {
        return EnvMarkerCheck::Unverifiable;
    }

    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `VirtualQueryEx` queries memory protection and region bounds for `params.environment`.
    if unsafe {
        VirtualQueryEx(
            handle.0,
            Some(params.environment.cast_const()),
            &mut mbi,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    } == 0
    {
        return EnvMarkerCheck::Unverifiable;
    }
    if mbi.State != MEM_COMMIT || (mbi.Protect.0 & (PAGE_NOACCESS.0 | PAGE_GUARD.0)) != 0 {
        return EnvMarkerCheck::Unverifiable;
    }

    let env_addr = params.environment as usize;
    let base_addr = mbi.BaseAddress as usize;
    if env_addr < base_addr {
        return EnvMarkerCheck::Unverifiable;
    }
    let offset = env_addr - base_addr;
    if offset >= mbi.RegionSize {
        return EnvMarkerCheck::Unverifiable;
    }
    let available = mbi.RegionSize - offset;
    let truncated = available > MAX_ENV_BYTES;
    let to_read = available.min(MAX_ENV_BYTES) & !1;
    if to_read < 2 {
        return EnvMarkerCheck::Unverifiable;
    }

    let mut units = vec![0u16; to_read / 2];
    let mut env_bytes_read = 0usize;
    // SAFETY: `units` has exact byte length `to_read`, aligned to `u16`.
    if unsafe {
        ReadProcessMemory(
            handle.0,
            params.environment.cast_const(),
            units.as_mut_ptr().cast::<c_void>(),
            to_read,
            Some(&mut env_bytes_read),
        )
    }
    .is_err()
        || env_bytes_read < 2
    {
        return EnvMarkerCheck::Unverifiable;
    }

    units.truncate(env_bytes_read / 2);
    windows_env_utf16_has_marker(&units, key, expected, truncated || env_bytes_read < to_read)
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

/// Check whether a verified Windows process is marked critical by the kernel (`IsProcessCritical`)
/// or belongs to the core Windows session executable set (`csrss`, `wininit`, `smss`, `lsass`,
/// `services`, `winlogon`).
pub(crate) fn check_protected_system_process(identity: &ProcessId) -> Option<&'static str> {
    // SAFETY: `OpenProcess` takes plain integer arguments and the returned handle is immediately
    // wrapped in `OwnedHandle`.
    let handle =
        match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, identity.pid) } {
            Ok(h) => OwnedHandle(h),
            Err(err) if err.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                // Protected Process Light (PPL) system processes such as csrss/smss/lsass can refuse
                // OpenProcess for unprivileged callers; fall back to ToolHelp32's image name.
                if let Some(exe_name) = snapshot_exe_name(identity.pid)
                    && crate::procid::is_windows_core_session_image(&exe_name)
                {
                    return Some("Windows core session process");
                }
                return None;
            }
            Err(_) => return None,
        };

    let Ok((creation, exit_ft)) = query_times_raw(handle.0, &format!("pid {}", identity.pid))
    else {
        return None;
    };
    if !is_still_running(handle.0, exit_ft) || creation != identity.start_time {
        return None;
    }

    let mut critical = BOOL::default();
    // SAFETY: `handle.0` has `PROCESS_QUERY_LIMITED_INFORMATION` (the minimum right required by
    // `IsProcessCritical`) and `critical` is a valid mutable `BOOL`.
    if unsafe { IsProcessCritical(handle.0, &mut critical) }.is_ok() && critical.as_bool() {
        return Some("Windows critical system process");
    }

    if let Some(image_path) = query_image_name_raw(handle.0)
        && crate::procid::is_windows_core_session_image(&image_path)
    {
        return Some("Windows core session process");
    }

    None
}

fn query_image_name_raw(handle: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` has `PROCESS_QUERY_LIMITED_INFORMATION`, `buf` has capacity `len` UTF-16
    // code units, and `QueryFullProcessImageNameW` writes at most `len` units and updates `len`.
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR::from_raw(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    let slice = buf.get(..(len as usize))?;
    Some(String::from_utf16_lossy(slice))
}

/// Snapshot of a pinned Windows process used to verify whether it is a same-account `sshd`
/// session ancestor or target.
pub(crate) struct WindowsProcSshdInfo {
    pub(crate) id: ProcessId,
    pub(crate) image_path: String,
    pub(crate) same_account: bool,
    pub(crate) cmdline: Option<String>,
}

fn read_token_user_buf(process_handle: HANDLE) -> Option<Vec<usize>> {
    let mut token = HANDLE::default();
    // SAFETY: `process_handle` is a valid process handle and `token` is immediately wrapped in
    // `OwnedHandle`.
    unsafe { OpenProcessToken(process_handle, TOKEN_QUERY, &mut token) }.ok()?;
    let token = OwnedHandle(token);

    let mut needed: u32 = 0;
    // SAFETY: Standard Win32 buffer-size query with null output pointer.
    let first = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
    if let Err(err) = first
        && err.code() != ERROR_INSUFFICIENT_BUFFER.to_hresult()
    {
        return None;
    }
    if needed == 0 {
        return None;
    }
    let words = (needed as usize).div_ceil(size_of::<usize>());
    let mut buf = vec![0usize; words];
    // SAFETY: `buf` is `usize`-aligned with at least `needed` bytes of writable storage.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    Some(buf)
}

fn same_user_sid_as_current(target_handle: HANDLE) -> bool {
    // SAFETY: `GetCurrentProcess` returns the current process pseudo-handle.
    let Some(my_buf) = read_token_user_buf(unsafe { GetCurrentProcess() }) else {
        return false;
    };
    let Some(target_buf) = read_token_user_buf(target_handle) else {
        return false;
    };
    let my_sid = unsafe { (*my_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let target_sid = unsafe { (*target_buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    if my_sid.0.is_null() || target_sid.0.is_null() {
        return false;
    }
    // SAFETY: Both SIDs live inside `my_buf` and `target_buf` for the duration of `EqualSid`.
    unsafe { EqualSid(my_sid, target_sid) }.is_ok()
}

fn read_process_cmdline(pid: u32) -> Option<String> {
    const MAX_CMDLINE_BYTES: usize = 64 * 1024;
    // SAFETY: `OpenProcess` takes plain integer flags and returns an owned handle.
    let handle = OwnedHandle(
        unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }.ok()?,
    );
    let mut pbi = PROCESS_BASIC_INFORMATION::default();
    let mut returned = 0u32;
    // SAFETY: `handle.0` has `PROCESS_QUERY_INFORMATION`; `pbi` matches `ProcessBasicInformation`.
    let status = unsafe {
        NtQueryInformationProcess(
            handle.0,
            ProcessBasicInformation,
            (&mut pbi as *mut PROCESS_BASIC_INFORMATION).cast::<c_void>(),
            size_of::<PROCESS_BASIC_INFORMATION>() as u32,
            &mut returned,
        )
    };
    if status.is_err() || pbi.PebBaseAddress.is_null() {
        return None;
    }
    let mut peb: PEB = unsafe { std::mem::zeroed() };
    let mut read_bytes = 0usize;
    // SAFETY: Reading `PEB` from remote `PebBaseAddress`.
    if unsafe {
        ReadProcessMemory(
            handle.0,
            pbi.PebBaseAddress.cast::<c_void>(),
            (&mut peb as *mut PEB).cast::<c_void>(),
            size_of::<PEB>(),
            Some(&mut read_bytes),
        )
    }
    .is_err()
        || read_bytes < size_of::<PEB>()
        || peb.ProcessParameters.is_null()
    {
        return None;
    }
    let mut params: RTL_USER_PROCESS_PARAMETERS = unsafe { std::mem::zeroed() };
    read_bytes = 0;
    // SAFETY: Reading `RTL_USER_PROCESS_PARAMETERS` from remote `peb.ProcessParameters`.
    if unsafe {
        ReadProcessMemory(
            handle.0,
            peb.ProcessParameters.cast::<c_void>(),
            (&mut params as *mut RTL_USER_PROCESS_PARAMETERS).cast::<c_void>(),
            size_of::<RTL_USER_PROCESS_PARAMETERS>(),
            Some(&mut read_bytes),
        )
    }
    .is_err()
        || read_bytes < size_of::<RTL_USER_PROCESS_PARAMETERS>()
        || params.CommandLine.Buffer.is_null()
    {
        return None;
    }
    let byte_len = usize::from(params.CommandLine.Length).min(MAX_CMDLINE_BYTES) & !1;
    if byte_len == 0 {
        return Some(String::new());
    }
    let mut units = vec![0u16; byte_len / 2];
    read_bytes = 0;
    // SAFETY: `units` has exact byte capacity `byte_len`, aligned to `u16`.
    if unsafe {
        ReadProcessMemory(
            handle.0,
            params.CommandLine.Buffer.0.cast::<c_void>(),
            units.as_mut_ptr().cast::<c_void>(),
            byte_len,
            Some(&mut read_bytes),
        )
    }
    .is_err()
        || read_bytes < 2
    {
        return None;
    }
    units.truncate(read_bytes / 2);
    let trimmed_len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..trimmed_len]))
}

/// Query the full executable image path of live process `identity` after verifying its exact
/// `pid` and `start_time` through a held query-only handle.
pub(crate) fn query_verified_image_path(identity: &ProcessId) -> Option<PathBuf> {
    let OpenPinnedOutcome::Pinned(pinned) = open_pinned_query_only(identity.pid, false) else {
        return None;
    };
    if !identity.matches(&pinned.id) {
        return None;
    }
    query_image_name_raw(pinned.handle.0).map(PathBuf::from)
}

/// Inspect live process `pid` through a held query-only handle to verify its identity, parent PID,
/// image path, user SID equality with the current process, and command line.
pub(crate) fn inspect_process_for_sshd(pid: u32) -> Option<WindowsProcSshdInfo> {
    let OpenPinnedOutcome::Pinned(pinned) = open_pinned_query_only(pid, false) else {
        return None;
    };
    let image_path = query_image_name_raw(pinned.handle.0)?;
    let same_account = same_user_sid_as_current(pinned.handle.0);
    let cmdline = if same_account {
        read_process_cmdline(pid)
    } else {
        None
    };
    Some(WindowsProcSshdInfo {
        id: pinned.id,
        image_path,
        same_account,
        cmdline,
    })
}

fn snapshot_exe_name(target_pid: u32) -> Option<String> {
    // SAFETY: `CreateToolhelp32Snapshot` takes plain integer arguments and returns an owned handle.
    let snapshot = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.ok()?);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snapshot.0` is valid and `entry.dwSize` is initialized to `size_of::<PROCESSENTRY32W>()`.
    unsafe { Process32FirstW(snapshot.0, &mut entry) }.ok()?;
    loop {
        if entry.th32ProcessID == target_pid {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            return Some(String::from_utf16_lossy(&entry.szExeFile[..end]));
        }
        // SAFETY: `snapshot.0` is valid and `entry` remains properly sized.
        if unsafe { Process32NextW(snapshot.0, &mut entry) }.is_err() {
            break;
        }
    }
    None
}

/// Open `identity.pid` with `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`, verify its
/// exact 100-ns `FILETIME` creation time while the held handle pins the PID against reuse, and
/// call `TerminateProcess` through that handle.
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
