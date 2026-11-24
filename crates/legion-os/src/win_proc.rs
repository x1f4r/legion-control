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
use std::path::Path;
use std::sync::Mutex;

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, SystemTimeOfDayInformation,
};
use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessBasicInformation};
use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, FILETIME, GENERIC_READ,
    GENERIC_WRITE, HANDLE, STILL_ACTIVE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_APPEND_DATA, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, MOVE_FILE_FLAGS, MoveFileExW, OPEN_ALWAYS, OPEN_EXISTING, SYNCHRONIZE,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, IsProcessCritical, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcess, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_BASIC_INFORMATION, PROCESS_INFORMATION,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::procid::ProcessId;
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

/// Spawn a detached Windows process via `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
/// restricted strictly to short-lived inheritable duplicates of the three standard handles
/// (`stdin` null, `stdout` and `stderr` log files or null).
///
/// The base log and `NUL` handles are opened non-inheritable before acquiring `DIRECT_SPAWN_LOCK`;
/// inheritable duplicates exist only inside `DIRECT_SPAWN_LOCK` across `CreateProcessW` and are
/// closed immediately afterwards so they cannot leak into other spawns.
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
) -> std::result::Result<ProcessId, DirectSpawnError> {
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

    let flags = CREATE_BREAKAWAY_FROM_JOB
        | CREATE_NO_WINDOW
        | CREATE_NEW_PROCESS_GROUP
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT;

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

        let h_in = stdin_dup.0;
        let h_out = stdout_dup.0;
        let h_err = stderr_dup.as_ref().map_or(h_out, |h| h.0);

        let mut handle_list = Vec::with_capacity(3);
        for h in [h_in, h_out, h_err] {
            if !handle_list.contains(&h) {
                handle_list.push(h);
            }
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

    let _thread_handle = OwnedHandle(pi.hThread);
    let proc_handle = OwnedHandle(pi.hProcess);
    let (creation, _) = query_times_raw(proc_handle.0, "detached child process")
        .map_err(DirectSpawnError::Other)?;
    Ok(ProcessId::new(pi.dwProcessId, creation))
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
