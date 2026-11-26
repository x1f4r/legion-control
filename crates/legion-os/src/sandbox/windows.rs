//! Windows AppContainer + Job Object sandboxing (with Low-Integrity Restricted Token fallback)
//! for `__thumb-worker` and `ffmpeg`.
//!
//! - **Primary sandbox:** zero-capability AppContainer (`CreateAppContainerProfile` /
//!   `DeriveAppContainerSidFromAppContainerName` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`)
//!   with explicit `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (the three stdio handles only) inside a
//!   Windows Job Object enforcing `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
//!   `JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 1`, `JOB_OBJECT_UILIMIT_ALL`, and optional process/job
//!   commit memory limits. Blocks user file reads, writes, network sockets, and child spawns.
//! - **Windows-only fallback:** if AppContainer profile creation or process launch fails on a
//!   host, falls back to `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE | LUA_TOKEN)` at Low
//!   Integrity (`S-1-16-4096`) via `CreateProcessAsUserW` inside the exact same Job Object.
//!   Blocks writes and child spawns; [`verify_worker_sandbox`] reports `complete = false` so
//!   callers and `meta.doctor` see honestly that AppContainer was not active.

#![allow(unsafe_code)]

use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ALREADY_EXISTS, GENERIC_READ,
    HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSidToSidW, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW,
    GetSecurityInfo, REVOKE_ACCESS, SE_FILE_OBJECT, SE_WINDOW_OBJECT, SetEntriesInAclW,
    SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, AclSizeInformation,
    CreateRestrictedToken, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE, EqualSid, FreeSid,
    GetAce, GetAclInformation, GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount,
    GetTokenInformation, LUA_TOKEN, NO_INHERITANCE, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY, TokenIntegrityLevel, TokenIsAppContainer,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOB_OBJECT_UILIMIT, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CloseWindowStation, DESKTOP_CONTROL_FLAGS, GetProcessWindowStation,
    GetThreadDesktop, GetUserObjectInformationW, OpenDesktopW, OpenWindowStationW, UOI_NAME,
};
use windows::Win32::System::SystemServices::{JOB_OBJECT_UILIMIT_ALL, SE_GROUP_INTEGRITY};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    GetCurrentThreadId, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, ResumeThread,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::sandbox::{SandboxPolicy, SandboxReport, SandboxUnavailable};
use crate::shell;
use crate::{OsError, Result};

const APPCONTAINER_NAME: &str = "Legion.DecodeWorker";
const APPCONTAINER_DISPLAY_NAME: &str = "Legion Decode Worker";
const APPCONTAINER_DESCRIPTION: &str = "Sandboxed media decode worker for Legion";
const LOW_INTEGRITY_SID_STR: &str = "S-1-16-4096";
const ALL_APP_PACKAGES_SID_STR: &str = "S-1-15-2-1";
const SECURITY_MANDATORY_LOW_RID: u32 = 0x1000;
const AC_FALLBACK_REASON_ENV: &str = "LEGION_WIN_AC_FALLBACK_REASON";

// Standard Win32 access bits for Window Station and Desktop DACL management in non-interactive
// sessions (e.g. Windows OpenSSH `sshd`, where `Service-0x0-<LUID>$\Default` omits
// `ALL APPLICATION PACKAGES` and `user32.dll` fails initialization with `0xC0000142` unless our
// specific AppContainer SID has read/connect access).
const WIN_READ_CONTROL: u32 = 0x0002_0000;
const WIN_WRITE_DAC: u32 = 0x0004_0000;
const WINSTA_ENUMDESKTOPS: u32 = 0x0001;
const WINSTA_READATTRIBUTES: u32 = 0x0002;
const WINSTA_ACCESSGLOBALATOMS: u32 = 0x0020;
const WINSTA_ENUMERATE: u32 = 0x0100;
/// Minimal Window Station rights needed for `user32.dll` initialization inside AppContainer
/// (omits clipboard, read-screen, desktop creation, write-attributes, and exit-windows; Job Object
/// `JOB_OBJECT_UILIMIT_ALL` additionally blocks all UI interaction before the child thread resumes).
const WINSTA_MIN_ACCESS: u32 = WIN_READ_CONTROL
    | WINSTA_ENUMDESKTOPS
    | WINSTA_READATTRIBUTES
    | WINSTA_ACCESSGLOBALATOMS
    | WINSTA_ENUMERATE;
const DESKTOP_READOBJECTS: u32 = 0x0001;
const DESKTOP_ENUMERATE: u32 = 0x0040;
/// Minimal Desktop rights needed for `user32.dll` initialization inside AppContainer (read-only
/// connect; omits create-window, create-menu, hooks, journal, write-objects, and switch-desktop).
const DESKTOP_MIN_ACCESS: u32 = WIN_READ_CONTROL | DESKTOP_READOBJECTS | DESKTOP_ENUMERATE;
const WIN_GENERIC_ALL: u32 = 0x1000_0000;
const ACE_FLAG_INHERIT_ONLY: u8 = 0x08;
const ACE_TYPE_ACCESS_ALLOWED: u8 = 0;

/// Serializes inheritable stdio handle duplication across sandboxed `CreateProcess*` calls.
static SANDBOX_SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Cached AppContainer SID bytes for [`APPCONTAINER_NAME`].
static APPCONTAINER_SID_CACHE: OnceLock<std::result::Result<Vec<u8>, String>> = OnceLock::new();

/// Paths whose DACL has already been granted RX for our AppContainer SID in this process.
static APPCONTAINER_ACL_CACHE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// Refcounted process-wide guard tracking temporary Window Station / Desktop ACEs added for
/// non-interactive sessions (such as `sshd` Session 0 `Service-0x0-<LUID>$\Default`).
struct WinstaGrantState {
    refcount: usize,
    sid_bytes: Vec<u8>,
    granted_winsta: Option<Vec<u16>>,
    granted_desk: Option<Vec<u16>>,
}

static WINSTA_GRANT_STATE: Mutex<WinstaGrantState> = Mutex::new(WinstaGrantState {
    refcount: 0,
    sid_bytes: Vec::new(),
    granted_winsta: None,
    granted_desk: None,
});

/// RAII token holding a temporary Window Station / Desktop ACE grant for our specific AppContainer
/// SID while one or more AppContainer child processes are alive in a non-interactive session.
///
/// On interactive desktops (`WinSta0\Default`), Windows already grants `ALL APPLICATION PACKAGES`
/// (`S-1-15-2-1`) access, so no DACL is modified and [`WindowStationGrant::acquire`] returns
/// `None`. When the last live grant drops, our SID's ACE is revoked via `REVOKE_ACCESS`.
pub(crate) struct WindowStationGrant;

impl WindowStationGrant {
    fn acquire(sid_bytes: &[u8]) -> Option<Self> {
        let mut state = WINSTA_GRANT_STATE.lock().unwrap_or_else(|p| p.into_inner());
        if state.refcount > 0 {
            state.refcount += 1;
            return Some(Self);
        }

        // SAFETY: `GetProcessWindowStation` and `GetThreadDesktop` return OS-owned handles for the
        // current process/thread that must not be closed.
        let Ok(cur_ws) = (unsafe { GetProcessWindowStation() }) else {
            return None;
        };
        if cur_ws.is_invalid() {
            return None;
        }
        // SAFETY: `GetCurrentThreadId` and `GetThreadDesktop` take plain integer arguments.
        let Ok(cur_dk) = (unsafe { GetThreadDesktop(GetCurrentThreadId()) }) else {
            return None;
        };
        if cur_dk.is_invalid() {
            return None;
        }

        let ws_name = query_user_object_name(HANDLE(cur_ws.0));
        let dk_name = query_user_object_name(HANDLE(cur_dk.0));

        let ws_modified = grant_winsta_if_needed(ws_name.as_deref(), HANDLE(cur_ws.0), sid_bytes);
        let dk_modified = grant_desktop_if_needed(dk_name.as_deref(), HANDLE(cur_dk.0), sid_bytes);

        if !ws_modified && !dk_modified {
            return None;
        }

        state.sid_bytes = sid_bytes.to_vec();
        state.granted_winsta = if ws_modified { ws_name } else { None };
        state.granted_desk = if dk_modified { dk_name } else { None };
        state.refcount = 1;
        Some(Self)
    }
}

impl Drop for WindowStationGrant {
    fn drop(&mut self) {
        let mut state = WINSTA_GRANT_STATE.lock().unwrap_or_else(|p| p.into_inner());
        if state.refcount == 0 {
            return;
        }
        state.refcount -= 1;
        if state.refcount > 0 {
            return;
        }
        let sid_bytes = std::mem::take(&mut state.sid_bytes);
        let ws_name = state.granted_winsta.take();
        let dk_name = state.granted_desk.take();

        // Keep `WINSTA_GRANT_STATE` locked across revocation so a concurrent `acquire()` cannot
        // observe the soon-to-be-revoked ACE and skip granting for its own child.
        if !sid_bytes.is_empty() {
            if let Some(name) = ws_name {
                revoke_winsta(&name, &sid_bytes);
            }
            if let Some(name) = dk_name {
                revoke_desktop(&name, &sid_bytes);
            }
        }
    }
}

fn query_user_object_name(handle: HANDLE) -> Option<Vec<u16>> {
    let mut needed = 0u32;
    // SAFETY: size query with `None` output buffer.
    let _ = unsafe { GetUserObjectInformationW(handle, UOI_NAME, None, 0, Some(&mut needed)) };
    if needed < 2 {
        return None;
    }
    let mut wide = vec![0u16; (needed as usize).div_ceil(2) + 1];
    // SAFETY: `wide` holds at least `needed` writable bytes.
    unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(wide.as_mut_ptr().cast::<c_void>()),
            needed,
            Some(&mut needed),
        )
    }
    .ok()?;
    if let Some(pos) = wide.iter().position(|&c| c == 0) {
        wide.truncate(pos + 1);
    } else {
        wide.push(0);
    }
    (wide.len() > 1).then_some(wide)
}

fn dacl_already_allows(dacl: *const ACL, sid_bytes: &[u8], required_mask: u32) -> bool {
    if dacl.is_null() {
        // A NULL DACL grants unrestricted access to everyone; do not replace it.
        return true;
    }
    let all_ac_w = str_to_wide_null(ALL_APP_PACKAGES_SID_STR);
    let mut all_ac_sid = PSID::default();
    // SAFETY: `all_ac_w` is NUL-terminated; freed with `LocalFree` before return.
    let has_all_ac =
        unsafe { ConvertStringSidToSidW(PCWSTR::from_raw(all_ac_w.as_ptr()), &mut all_ac_sid) }
            .is_ok()
            && !all_ac_sid.0.is_null();

    let our_psid = PSID(sid_bytes.as_ptr().cast_mut().cast::<c_void>());
    let mut size_info = ACL_SIZE_INFORMATION::default();
    let mut granted_mask = 0u32;

    // SAFETY: `dacl` is a valid Win32 ACL pointer returned by `GetSecurityInfo`.
    let got_info = unsafe {
        GetAclInformation(
            dacl,
            (&raw mut size_info).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .is_ok();

    if got_info {
        for idx in 0..size_info.AceCount {
            let mut ace_ptr: *mut c_void = std::ptr::null_mut();
            // SAFETY: `idx < size_info.AceCount` for `dacl`.
            if unsafe { GetAce(dacl, idx, &mut ace_ptr) }.is_err() || ace_ptr.is_null() {
                continue;
            }
            // SAFETY: every ACE begins with `ACE_HEADER`.
            let header = unsafe { &*ace_ptr.cast::<ACE_HEADER>() };
            if header.AceType != ACE_TYPE_ACCESS_ALLOWED
                || (header.AceFlags & ACE_FLAG_INHERIT_ONLY) != 0
            {
                continue;
            }
            // SAFETY: `AceType == ACCESS_ALLOWED_ACE_TYPE` confirms `ACCESS_ALLOWED_ACE` layout.
            let allowed = unsafe { &*ace_ptr.cast::<ACCESS_ALLOWED_ACE>() };
            let ace_sid = PSID((&raw const allowed.SidStart).cast_mut().cast::<c_void>());
            // SAFETY: `ace_sid`, `our_psid`, and `all_ac_sid` are valid Win32 SIDs.
            let matches_sid = unsafe { EqualSid(ace_sid, our_psid) }.is_ok()
                || (has_all_ac && unsafe { EqualSid(ace_sid, all_ac_sid) }.is_ok());
            if matches_sid {
                granted_mask |= allowed.Mask;
            }
        }
    }

    if has_all_ac {
        // SAFETY: `all_ac_sid` was allocated by `ConvertStringSidToSidW` and freed via `LocalFree`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(all_ac_sid.0)));
        }
    }

    (granted_mask & required_mask) == required_mask || (granted_mask & WIN_GENERIC_ALL) != 0
}

fn grant_window_object_ace_if_needed(handle: HANDLE, sid_bytes: &[u8], access_mask: u32) -> bool {
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();

    // SAFETY: `handle` is a valid Window Station or Desktop handle; `sd` is freed below.
    let get_err = unsafe {
        GetSecurityInfo(
            handle,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            Some(&mut sd),
        )
    };
    if get_err.0 != 0 {
        return false;
    }

    let mut modified = false;
    if !dacl_already_allows(old_dacl, sid_bytes, access_mask) {
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access_mask,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: Default::default(),
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR::from_raw(sid_bytes.as_ptr().cast_mut().cast::<u16>()),
            },
        };
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `ea` references live `sid_bytes` and `old_dacl` stays live until `sd` is freed.
        let merge_err = unsafe { SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl) };
        if merge_err.0 == 0 && !new_dacl.is_null() {
            // SAFETY: `handle` has `WRITE_DAC` and `new_dacl` was built by `SetEntriesInAclW`.
            let set_err = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(new_dacl),
                    None,
                )
            };
            modified = set_err.0 == 0;
            // SAFETY: `new_dacl` was allocated by `SetEntriesInAclW`.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(new_dacl.cast::<c_void>())));
            }
        }
    }

    if !sd.0.is_null() {
        // SAFETY: `sd` was allocated by `GetSecurityInfo`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
    modified
}

fn revoke_window_object_ace(handle: HANDLE, sid_bytes: &[u8]) {
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();

    // SAFETY: `handle` is a valid Window Station or Desktop handle; `sd` is freed below.
    let get_err = unsafe {
        GetSecurityInfo(
            handle,
            SE_WINDOW_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            Some(&mut sd),
        )
    };
    if get_err.0 == 0 && !old_dacl.is_null() {
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: 0,
            grfAccessMode: REVOKE_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: Default::default(),
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR::from_raw(sid_bytes.as_ptr().cast_mut().cast::<u16>()),
            },
        };
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: `ea` references live `sid_bytes` and `old_dacl` stays live until `sd` is freed.
        let merge_err = unsafe { SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl) };
        if merge_err.0 == 0 && !new_dacl.is_null() {
            // SAFETY: `handle` has `WRITE_DAC` and `new_dacl` was built by `SetEntriesInAclW`.
            let _ = unsafe {
                SetSecurityInfo(
                    handle,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(new_dacl),
                    None,
                )
            };
            // SAFETY: `new_dacl` was allocated by `SetEntriesInAclW`.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(new_dacl.cast::<c_void>())));
            }
        }
    }
    if !sd.0.is_null() {
        // SAFETY: `sd` was allocated by `GetSecurityInfo`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
}

fn grant_winsta_if_needed(name: Option<&[u16]>, fallback: HANDLE, sid_bytes: &[u8]) -> bool {
    if let Some(wide) = name
        // SAFETY: `wide` is NUL-terminated UTF-16; returned handle is closed below.
        && let Ok(opened) = unsafe {
            OpenWindowStationW(PCWSTR::from_raw(wide.as_ptr()), false, WIN_READ_CONTROL | WIN_WRITE_DAC)
        }
        && !opened.is_invalid()
    {
        let modified =
            grant_window_object_ace_if_needed(HANDLE(opened.0), sid_bytes, WINSTA_MIN_ACCESS);
        // SAFETY: `opened` was returned by `OpenWindowStationW`.
        unsafe {
            let _ = CloseWindowStation(opened);
        }
        return modified;
    }
    grant_window_object_ace_if_needed(fallback, sid_bytes, WINSTA_MIN_ACCESS)
}

fn grant_desktop_if_needed(name: Option<&[u16]>, fallback: HANDLE, sid_bytes: &[u8]) -> bool {
    if let Some(wide) = name
        // SAFETY: `wide` is NUL-terminated UTF-16; returned handle is closed below.
        && let Ok(opened) = unsafe {
            OpenDesktopW(
                PCWSTR::from_raw(wide.as_ptr()),
                DESKTOP_CONTROL_FLAGS(0),
                false,
                WIN_READ_CONTROL | WIN_WRITE_DAC,
            )
        }
        && !opened.is_invalid()
    {
        let modified =
            grant_window_object_ace_if_needed(HANDLE(opened.0), sid_bytes, DESKTOP_MIN_ACCESS);
        // SAFETY: `opened` was returned by `OpenDesktopW`.
        unsafe {
            let _ = CloseDesktop(opened);
        }
        return modified;
    }
    grant_window_object_ace_if_needed(fallback, sid_bytes, DESKTOP_MIN_ACCESS)
}

fn revoke_winsta(name: &[u16], sid_bytes: &[u8]) {
    // SAFETY: `name` is NUL-terminated UTF-16; returned handle is closed below.
    if let Ok(opened) = unsafe {
        OpenWindowStationW(
            PCWSTR::from_raw(name.as_ptr()),
            false,
            WIN_READ_CONTROL | WIN_WRITE_DAC,
        )
    } && !opened.is_invalid()
    {
        revoke_window_object_ace(HANDLE(opened.0), sid_bytes);
        // SAFETY: `opened` was returned by `OpenWindowStationW`.
        unsafe {
            let _ = CloseWindowStation(opened);
        }
        return;
    }
    // SAFETY: fallback to OS-owned process window station handle (must not be closed).
    if let Ok(cur) = unsafe { GetProcessWindowStation() }
        && !cur.is_invalid()
    {
        revoke_window_object_ace(HANDLE(cur.0), sid_bytes);
    }
}

fn revoke_desktop(name: &[u16], sid_bytes: &[u8]) {
    // SAFETY: `name` is NUL-terminated UTF-16; returned handle is closed below.
    if let Ok(opened) = unsafe {
        OpenDesktopW(
            PCWSTR::from_raw(name.as_ptr()),
            DESKTOP_CONTROL_FLAGS(0),
            false,
            WIN_READ_CONTROL | WIN_WRITE_DAC,
        )
    } && !opened.is_invalid()
    {
        revoke_window_object_ace(HANDLE(opened.0), sid_bytes);
        // SAFETY: `opened` was returned by `OpenDesktopW`.
        unsafe {
            let _ = CloseDesktop(opened);
        }
        return;
    }
    // SAFETY: fallback to OS-owned thread desktop handle (must not be closed).
    if let Ok(cur) = unsafe { GetThreadDesktop(GetCurrentThreadId()) }
        && !cur.is_invalid()
    {
        revoke_window_object_ace(HANDLE(cur.0), sid_bytes);
    }
}

/// RAII wrapper for an owned Win32 `HANDLE`.
pub(crate) struct OwnedWinHandle(pub(crate) HANDLE);

// SAFETY: Win32 kernel handles (`HANDLE`) are thread-safe to transfer across threads and closed
// once on `Drop`.
unsafe impl Send for OwnedWinHandle {}
// SAFETY: Shared references only read the raw handle value for Wait/Terminate/Query APIs.
unsafe impl Sync for OwnedWinHandle {}

impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            // SAFETY: `self.0` is an owned Win32 kernel handle closed once on drop.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

struct ProcThreadAttrList {
    _storage: Vec<usize>,
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl Drop for ProcThreadAttrList {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` was initialized by `InitializeProcThreadAttributeList` into
        // `self._storage` and is deleted once before `_storage` drops.
        unsafe { DeleteProcThreadAttributeList(self.ptr) };
    }
}

fn to_wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn str_to_wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_sid_bytes(psid: PSID) -> std::result::Result<Vec<u8>, String> {
    if psid.0.is_null() {
        return Err("kernel returned null SID pointer".to_owned());
    }
    // SAFETY: `psid` is a valid non-null Win32 SID returned by the OS.
    let len = unsafe { GetLengthSid(psid) } as usize;
    if len == 0 {
        return Err("GetLengthSid returned 0".to_owned());
    }
    let mut out = vec![0u8; len];
    // SAFETY: `psid.0` points to `len` readable bytes and `out` has `len` writable bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(psid.0.cast::<u8>(), out.as_mut_ptr(), len);
    }
    Ok(out)
}

fn ensure_appcontainer_sid() -> std::result::Result<&'static [u8], &'static str> {
    let cached = APPCONTAINER_SID_CACHE.get_or_init(|| {
        let name = str_to_wide_null(APPCONTAINER_NAME);
        let display = str_to_wide_null(APPCONTAINER_DISPLAY_NAME);
        let desc = str_to_wide_null(APPCONTAINER_DESCRIPTION);

        // SAFETY: all wide strings are NUL-terminated; zero capabilities (`None`).
        let created = unsafe {
            CreateAppContainerProfile(
                PCWSTR::from_raw(name.as_ptr()),
                PCWSTR::from_raw(display.as_ptr()),
                PCWSTR::from_raw(desc.as_ptr()),
                None,
            )
        };
        let psid = match created {
            Ok(sid) => sid,
            Err(err) if err.code() == ERROR_ALREADY_EXISTS.to_hresult() => {
                // SAFETY: `name` is NUL-terminated.
                unsafe {
                    DeriveAppContainerSidFromAppContainerName(PCWSTR::from_raw(name.as_ptr()))
                }
                .map_err(|derive_err| {
                    format!("DeriveAppContainerSidFromAppContainerName failed: {derive_err}")
                })?
            }
            Err(err) => {
                return Err(format!("CreateAppContainerProfile failed: {err}"));
            }
        };
        let bytes = copy_sid_bytes(psid);
        // SAFETY: `psid` was allocated by `CreateAppContainerProfile` or
        // `DeriveAppContainerSidFromAppContainerName` and must be freed with `FreeSid`.
        unsafe {
            let _ = FreeSid(psid);
        }
        bytes
    });
    match cached {
        Ok(bytes) => Ok(bytes.as_slice()),
        Err(msg) => Err(msg.as_str()),
    }
}

/// Best-effort grant of `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE` on `path` to our specific
/// AppContainer SID so executables inside user/workspace directories (outside `C:\Program Files`)
/// can be mapped by the AppContainer loader.
fn ensure_appcontainer_rx_acl(path: &Path, sid_bytes: &[u8]) {
    let cache = APPCONTAINER_ACL_CACHE.get_or_init(|| Mutex::new(HashSet::new()));
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    {
        let guard = cache.lock().unwrap_or_else(|p| p.into_inner());
        if guard.contains(&key) {
            return;
        }
    }

    let wide = to_wide_null(path.as_os_str());
    let mut old_dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();

    // SAFETY: `wide` is NUL-terminated; `sd` is freed with `LocalFree` when non-null.
    let get_err = unsafe {
        GetNamedSecurityInfoW(
            PCWSTR::from_raw(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old_dacl),
            None,
            &mut sd,
        )
    };
    if get_err.0 != 0 {
        return;
    }

    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: (FILE_GENERIC_READ | FILE_GENERIC_EXECUTE).0,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: Default::default(),
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            // `ptstrName` carries the raw `PSID` pointer when `TrusteeForm == TRUSTEE_IS_SID`.
            ptstrName: PWSTR::from_raw(sid_bytes.as_ptr().cast_mut().cast::<u16>()),
        },
    };

    let mut new_dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `ea` references `sid_bytes` (live across this call) and `old_dacl` is valid while
    // `sd` stays live.
    let merge_err = unsafe { SetEntriesInAclW(Some(&[ea]), Some(old_dacl), &mut new_dacl) };
    if merge_err.0 == 0 && !new_dacl.is_null() {
        // SAFETY: `wide` is NUL-terminated and `new_dacl` is a valid ACL built by `SetEntriesInAclW`.
        let set_err = unsafe {
            SetNamedSecurityInfoW(
                PCWSTR::from_raw(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(new_dacl),
                None,
            )
        };
        if set_err.0 == 0 {
            let mut guard = cache.lock().unwrap_or_else(|p| p.into_inner());
            guard.insert(key);
        }
        // SAFETY: `new_dacl` was allocated by `SetEntriesInAclW` and is freed via `LocalFree`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(new_dacl.cast::<c_void>())));
        }
    }

    if !sd.0.is_null() {
        // SAFETY: `sd` was allocated by `GetNamedSecurityInfoW` and is freed via `LocalFree`.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
    }
}

fn ensure_ffmpeg_binary_and_dlls_rx_acl(ffmpeg_bin: &Path, sid_bytes: &[u8]) {
    ensure_appcontainer_rx_acl(ffmpeg_bin, sid_bytes);
    if let Some(dir) = ffmpeg_bin.parent()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("dll"))
            {
                ensure_appcontainer_rx_acl(&path, sid_bytes);
            }
        }
    }
}

fn create_sandbox_job(
    memory_limit: Option<u64>,
    rlimit_as_headroom: u64,
) -> Result<OwnedWinHandle> {
    // SAFETY: creates an anonymous Job Object with default security attributes.
    let job = unsafe { CreateJobObjectW(None, None) }.map_err(|err| OsError::Platform {
        detail: format!("CreateJobObjectW for sandbox failed: {err}"),
    })?;
    let owned = OwnedWinHandle(job);

    let mut ext = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    ext.BasicLimitInformation.ActiveProcessLimit = 1;
    if let Some(limit) = memory_limit {
        let commit =
            usize::try_from(limit.saturating_add(rlimit_as_headroom)).unwrap_or(usize::MAX);
        flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_JOB_MEMORY;
        ext.ProcessMemoryLimit = commit;
        ext.JobMemoryLimit = commit;
    }
    ext.BasicLimitInformation.LimitFlags = flags;

    // SAFETY: `owned.0` is a valid Job Object handle and `ext` has exact size.
    unsafe {
        SetInformationJobObject(
            owned.0,
            JobObjectExtendedLimitInformation,
            (&raw const ext).cast::<c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("SetInformationJobObject(ExtendedLimit) failed: {err}"),
    })?;

    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_OBJECT_UILIMIT(JOB_OBJECT_UILIMIT_ALL),
    };
    // SAFETY: `owned.0` is a valid Job Object handle and `ui` has exact size.
    unsafe {
        SetInformationJobObject(
            owned.0,
            JobObjectBasicUIRestrictions,
            (&raw const ui).cast::<c_void>(),
            size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("SetInformationJobObject(BasicUIRestrictions) failed: {err}"),
    })?;

    Ok(owned)
}

fn create_non_inheritable_pipe() -> Result<(OwnedWinHandle, OwnedWinHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: BOOL(0),
    };
    let mut read_h = HANDLE::default();
    let mut write_h = HANDLE::default();
    // SAFETY: `sa` has `bInheritHandle = FALSE` and both out-pointers are valid stack handles.
    unsafe { CreatePipe(&mut read_h, &mut write_h, Some(&sa), 0) }.map_err(|err| {
        OsError::Platform {
            detail: format!("CreatePipe failed: {err}"),
        }
    })?;
    Ok((OwnedWinHandle(read_h), OwnedWinHandle(write_h)))
}

fn open_non_inheritable_nul(access: u32) -> Result<OwnedWinHandle> {
    let nul = str_to_wide_null("NUL");
    // SAFETY: `nul` is NUL-terminated UTF-16 and `None` security attributes mark non-inheritable.
    let handle = unsafe {
        CreateFileW(
            PCWSTR::from_raw(nul.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("CreateFileW(NUL) failed: {err}"),
    })?;
    Ok(OwnedWinHandle(handle))
}

fn duplicate_inheritable(source: HANDLE) -> Result<OwnedWinHandle> {
    let mut dup = HANDLE::default();
    // SAFETY: duplicates `source` inside the current process with `bInheritHandle = true`.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, source, me, &mut dup, 0, true, DUPLICATE_SAME_ACCESS)
    }
    .map_err(|err| OsError::Platform {
        detail: format!("DuplicateHandle(inheritable) failed: {err}"),
    })?;
    Ok(OwnedWinHandle(dup))
}

fn build_appcontainer_env_block(
    env_clear: bool,
    env_remove: &[OsString],
    overrides: &BTreeMap<OsString, OsString>,
) -> Vec<u16> {
    // Windows `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES` runs
    // `BasepAppContainerEnvironmentExtension`, which fails with `ERROR_ENVVAR_NOT_FOUND` (203 /
    // `0x800700CB`) when an explicit `lpEnvironment` block omits required profile/system variables
    // (`LOCALAPPDATA`, `TEMP`, `TMP`, `USERPROFILE`, `SystemRoot`, `SystemDrive`, `WINDIR`).
    // Windows itself rewrites `LOCALAPPDATA`/`TEMP`/`TMP` to the isolated AppContainer storage
    // path inside the child, and zero-capability AppContainer tokens cannot access the host user's
    // profile directory.
    let mut ac_overrides = overrides.clone();
    let has_ci = |map: &BTreeMap<OsString, OsString>, name: &str| {
        map.keys()
            .any(|k| k.to_string_lossy().eq_ignore_ascii_case(name))
    };
    let sys_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .unwrap_or_else(|| OsString::from(r"C:\Windows"));
    let sys_drive = std::env::var_os("SystemDrive").unwrap_or_else(|| OsString::from("C:"));
    let fallback_tmp = PathBuf::from(&sys_root).join("Temp").into_os_string();
    for (name, fallback) in [
        ("SystemRoot", sys_root.clone()),
        ("WINDIR", sys_root.clone()),
        ("SystemDrive", sys_drive),
        (
            "USERPROFILE",
            std::env::var_os("USERPROFILE").unwrap_or_else(|| sys_root.clone()),
        ),
        (
            "APPDATA",
            std::env::var_os("APPDATA").unwrap_or_else(|| fallback_tmp.clone()),
        ),
        (
            "LOCALAPPDATA",
            std::env::var_os("LOCALAPPDATA").unwrap_or_else(|| fallback_tmp.clone()),
        ),
        (
            "TEMP",
            std::env::var_os("TEMP").unwrap_or_else(|| fallback_tmp.clone()),
        ),
        ("TMP", std::env::var_os("TMP").unwrap_or(fallback_tmp)),
    ] {
        if !has_ci(&ac_overrides, name) {
            ac_overrides.insert(OsString::from(name), fallback);
        }
    }
    build_unicode_env_block(env_clear, env_remove, &ac_overrides)
}

fn build_unicode_env_block(
    env_clear: bool,
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

    let mut merged: BTreeMap<Vec<u16>, (Vec<u16>, Vec<u16>)> = BTreeMap::new();
    if !env_clear {
        let removed: Vec<Vec<u16>> = env_remove.iter().map(|k| normalize_key(k)).collect();
        for (k, v) in std::env::vars_os() {
            let norm = normalize_key(&k);
            if removed.contains(&norm) {
                continue;
            }
            merged.insert(norm, (k.encode_wide().collect(), v.encode_wide().collect()));
        }
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

fn build_command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(shell::windows_cmdline_escape(&program.to_string_lossy()));
    for arg in args {
        parts.push(shell::windows_cmdline_escape(&arg.to_string_lossy()));
    }
    str_to_wide_null(&parts.join(" "))
}

fn resolve_executable_path(program: &OsStr) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.is_absolute() || p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let name = program.to_str()?;
    let exts = shell::parse_pathext(std::env::var_os("PATHEXT").as_deref());
    shell::find_executable_in_paths(name, std::env::var_os("PATH").as_deref(), &exts)
}

fn system_root_dir() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
}

fn init_attr_list(count: u32) -> Result<ProcThreadAttrList> {
    let mut size = 0usize;
    // SAFETY: size query with `None` buffer.
    let _ = unsafe { InitializeProcThreadAttributeList(None, count, None, &mut size) };
    if size == 0 {
        return Err(OsError::Platform {
            detail: "InitializeProcThreadAttributeList returned zero size".into(),
        });
    }
    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
    let ptr = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast::<c_void>());
    // SAFETY: `storage` is pointer-aligned with at least `size` bytes.
    unsafe { InitializeProcThreadAttributeList(Some(ptr), count, None, &mut size) }.map_err(
        |err| OsError::Platform {
            detail: format!("InitializeProcThreadAttributeList failed: {err}"),
        },
    )?;
    Ok(ProcThreadAttrList {
        _storage: storage,
        ptr,
    })
}

fn create_low_il_restricted_token() -> Result<OwnedWinHandle> {
    let mut current_token = HANDLE::default();
    // SAFETY: opens current process token with duplicate/assign/query/adjust rights.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY | TOKEN_ADJUST_DEFAULT,
            &mut current_token,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("OpenProcessToken failed: {err}"),
    })?;
    let current_token = OwnedWinHandle(current_token);

    let mut restricted = HANDLE::default();
    // SAFETY: creates a stripped LUA restricted primary token from `current_token`.
    unsafe {
        CreateRestrictedToken(
            current_token.0,
            DISABLE_MAX_PRIVILEGE | LUA_TOKEN,
            None,
            None,
            None,
            &mut restricted,
        )
    }
    .map_err(|err| OsError::Platform {
        detail: format!("CreateRestrictedToken failed: {err}"),
    })?;
    let restricted = OwnedWinHandle(restricted);

    let low_sid_w = str_to_wide_null(LOW_INTEGRITY_SID_STR);
    let mut low_sid = PSID::default();
    // SAFETY: `low_sid_w` is NUL-terminated; `low_sid` is freed with `LocalFree` below.
    unsafe { ConvertStringSidToSidW(PCWSTR::from_raw(low_sid_w.as_ptr()), &mut low_sid) }.map_err(
        |err| OsError::Platform {
            detail: format!("ConvertStringSidToSidW(Low IL) failed: {err}"),
        },
    )?;

    // SAFETY: `low_sid` is valid until `LocalFree` below.
    let sid_len = unsafe { GetLengthSid(low_sid) } as usize;
    let label_len = size_of::<TOKEN_MANDATORY_LABEL>();
    let total_len = label_len + sid_len;
    let mut buf = vec![0usize; total_len.div_ceil(size_of::<usize>())];
    // SAFETY: `buf` is pointer-aligned with room for `TOKEN_MANDATORY_LABEL` followed by `sid_len`
    // bytes for the inline SID copy, so `SetTokenInformation` reads strictly within `buf`.
    let set_res = unsafe {
        let base = buf.as_mut_ptr().cast::<u8>();
        let sid_dst = base.add(label_len);
        std::ptr::copy_nonoverlapping(low_sid.0.cast::<u8>(), sid_dst, sid_len);
        let tml = TOKEN_MANDATORY_LABEL {
            Label: SID_AND_ATTRIBUTES {
                Sid: PSID(sid_dst.cast::<c_void>()),
                Attributes: SE_GROUP_INTEGRITY as u32,
            },
        };
        std::ptr::write(base.cast::<TOKEN_MANDATORY_LABEL>(), tml);
        SetTokenInformation(
            restricted.0,
            TokenIntegrityLevel,
            base.cast::<c_void>(),
            total_len as u32,
        )
    };
    // SAFETY: `low_sid` was allocated by `ConvertStringSidToSidW` and must be freed via `LocalFree`.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(low_sid.0)));
    }
    set_res.map_err(|err| OsError::Platform {
        detail: format!("SetTokenInformation(TokenIntegrityLevel=Low) failed: {err}"),
    })?;

    Ok(restricted)
}

/// Active sandboxed Windows child process with owned process/job handles and stdio pipes.
pub(crate) struct SandboxedProcess {
    pub(crate) pid: u32,
    pub(crate) process: OwnedWinHandle,
    pub(crate) job: OwnedWinHandle,
    pub(crate) winsta_grant: Option<WindowStationGrant>,
    pub(crate) stdin_writer: Option<File>,
    pub(crate) stdout_reader: File,
    pub(crate) stderr_reader: File,
}

/// Spawns `program` + `args` inside a Windows sandbox matching `policy`.
///
/// - For [`SandboxPolicy::Worker`] and `stdin`-fed [`SandboxPolicy::Ffmpeg`] (`input_file: None`),
///   attempts zero-capability AppContainer + Job Object first and falls back to a Low-Integrity
///   Restricted Token + Job Object only if AppContainer launch fails.
/// - For path-based [`SandboxPolicy::Ffmpeg`] (`input_file: Some(_)`), launches inside a
///   Low-Integrity Restricted Token + Job Object so `ffmpeg` can still read the named input file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_sandboxed(
    policy: &SandboxPolicy,
    program: &OsStr,
    args: &[OsString],
    cwd: Option<&Path>,
    env_clear: bool,
    env_remove: &[OsString],
    env_overrides: &BTreeMap<OsString, OsString>,
    stdin_file: Option<File>,
    has_stdin_bytes: bool,
    memory_limit: Option<u64>,
    rlimit_as_headroom: u64,
) -> Result<SandboxedProcess> {
    let resolved_exe = resolve_executable_path(program);
    let app_path_for_acl = match policy {
        SandboxPolicy::Ffmpeg { ffmpeg_bin, .. } => Some(ffmpeg_bin.as_path()),
        SandboxPolicy::Worker => resolved_exe.as_deref(),
    };
    let app_wide = resolved_exe.as_deref().map(|p| to_wide_null(p.as_os_str()));
    let cmdline_base = build_command_line(program, args);
    let ac_env_block = build_appcontainer_env_block(env_clear, env_remove, env_overrides);
    let env_block = build_unicode_env_block(env_clear, env_remove, env_overrides);

    // Prepare stdio handles (parent ends stay non-inheritable; child ends are duplicated
    // inheritable strictly inside `SANDBOX_SPAWN_LOCK`).
    let (stdin_parent, stdin_child_base, _stdin_file_keepalive) = if let Some(file) = stdin_file {
        let raw = HANDLE(file.as_raw_handle());
        (
            None,
            OwnedWinHandle(duplicate_non_inheritable(raw)?),
            Some(file),
        )
    } else if has_stdin_bytes {
        let (r, w) = create_non_inheritable_pipe()?;
        // SAFETY: `w.0` is an owned pipe write handle transferred exclusively to `File`.
        let w_raw = w.0;
        std::mem::forget(w);
        let file = unsafe { File::from_raw_handle(w_raw.0) };
        (Some(file), r, None)
    } else {
        (None, open_non_inheritable_nul(GENERIC_READ.0)?, None)
    };

    let (stdout_read_h, stdout_child_base) = create_non_inheritable_pipe()?;
    let (stderr_read_h, stderr_child_base) = create_non_inheritable_pipe()?;

    // SAFETY: `stdout_read_h` / `stderr_read_h` are owned pipe read handles transferred to `File`.
    let stdout_raw = stdout_read_h.0;
    let stderr_raw = stderr_read_h.0;
    std::mem::forget(stdout_read_h);
    std::mem::forget(stderr_read_h);
    let stdout_reader = unsafe { File::from_raw_handle(stdout_raw.0) };
    let stderr_reader = unsafe { File::from_raw_handle(stderr_raw.0) };

    let try_appcontainer = match policy {
        SandboxPolicy::Worker => true,
        SandboxPolicy::Ffmpeg { input_file, .. } => input_file.is_none(),
    };

    let mut ac_error: Option<String> = None;
    if try_appcontainer {
        match ensure_appcontainer_sid() {
            Ok(sid_bytes) => {
                if let Some(exe) = app_path_for_acl {
                    match policy {
                        SandboxPolicy::Ffmpeg { .. } => {
                            ensure_ffmpeg_binary_and_dlls_rx_acl(exe, sid_bytes);
                        }
                        SandboxPolicy::Worker => {
                            ensure_appcontainer_rx_acl(exe, sid_bytes);
                        }
                    }
                }
                let ac_cwd = system_root_dir();
                let winsta_grant = WindowStationGrant::acquire(sid_bytes);
                match launch_with_appcontainer(
                    sid_bytes,
                    app_wide.as_deref(),
                    &cmdline_base,
                    &ac_cwd,
                    &ac_env_block,
                    stdin_child_base.0,
                    stdout_child_base.0,
                    stderr_child_base.0,
                    memory_limit,
                    rlimit_as_headroom,
                ) {
                    Ok((pid, process, job)) => {
                        return Ok(SandboxedProcess {
                            pid,
                            process,
                            job,
                            winsta_grant,
                            stdin_writer: stdin_parent,
                            stdout_reader,
                            stderr_reader,
                        });
                    }
                    Err(err) => {
                        drop(winsta_grant);
                        ac_error = Some(err.to_string());
                    }
                }
            }
            Err(err) => {
                ac_error = Some(err.to_owned());
            }
        }
    }

    // Windows-only fallback: Restricted Token + Low Integrity + Job Object.
    let fallback_env_block = if let Some(ref ac) = ac_error {
        let mut fallback_overrides = env_overrides.clone();
        fallback_overrides.insert(OsString::from(AC_FALLBACK_REASON_ENV), OsString::from(ac));
        build_unicode_env_block(env_clear, env_remove, &fallback_overrides)
    } else {
        env_block
    };
    let fallback_cwd = cwd.map(Path::to_path_buf).unwrap_or_else(system_root_dir);
    match launch_with_restricted_token(
        app_wide.as_deref(),
        &cmdline_base,
        &fallback_cwd,
        &fallback_env_block,
        stdin_child_base.0,
        stdout_child_base.0,
        stderr_child_base.0,
        memory_limit,
        rlimit_as_headroom,
    ) {
        Ok((pid, process, job)) => Ok(SandboxedProcess {
            pid,
            process,
            job,
            winsta_grant: None,
            stdin_writer: stdin_parent,
            stdout_reader,
            stderr_reader,
        }),
        Err(fallback_err) => Err(OsError::Platform {
            detail: match ac_error {
                Some(ac) => format!(
                    "Windows sandbox launch failed (AppContainer: {ac}; Low-IL restricted token fallback: {fallback_err})"
                ),
                None => {
                    format!("Windows Low-IL restricted token sandbox launch failed: {fallback_err}")
                }
            },
        }),
    }
}

fn duplicate_non_inheritable(source: HANDLE) -> Result<HANDLE> {
    let mut dup = HANDLE::default();
    // SAFETY: duplicates `source` inside the current process as non-inheritable.
    unsafe {
        let me = GetCurrentProcess();
        DuplicateHandle(me, source, me, &mut dup, 0, false, DUPLICATE_SAME_ACCESS)
    }
    .map_err(|err| OsError::Platform {
        detail: format!("DuplicateHandle(non-inheritable) failed: {err}"),
    })?;
    Ok(dup)
}

#[allow(clippy::too_many_arguments)]
fn launch_with_appcontainer(
    sid_bytes: &[u8],
    app_wide: Option<&[u16]>,
    cmdline_template: &[u16],
    cwd: &Path,
    env_block: &[u16],
    stdin_base: HANDLE,
    stdout_base: HANDLE,
    stderr_base: HANDLE,
    memory_limit: Option<u64>,
    rlimit_as_headroom: u64,
) -> Result<(u32, OwnedWinHandle, OwnedWinHandle)> {
    let job = create_sandbox_job(memory_limit, rlimit_as_headroom)?;
    let attr_list = init_attr_list(2)?;
    let mut cmdline_wide = cmdline_template.to_vec();
    let cwd_wide = to_wide_null(cwd.as_os_str());
    let app_pcwstr = app_wide.map_or_else(PCWSTR::null, |b| PCWSTR::from_raw(b.as_ptr()));

    let mut sec_caps = SECURITY_CAPABILITIES {
        AppContainerSid: PSID(sid_bytes.as_ptr().cast_mut().cast::<c_void>()),
        Capabilities: std::ptr::null_mut(),
        CapabilityCount: 0,
        Reserved: 0,
    };

    let mut pi = PROCESS_INFORMATION::default();
    let create_res = {
        let _guard = SANDBOX_SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let stdin_dup = duplicate_inheritable(stdin_base)?;
        let stdout_dup = duplicate_inheritable(stdout_base)?;
        let stderr_dup = duplicate_inheritable(stderr_base)?;
        let handle_list = [stdin_dup.0, stdout_dup.0, stderr_dup.0];

        // SAFETY: `attr_list.ptr` has capacity 2; `handle_list` and `sec_caps` stay live across
        // `CreateProcessW`.
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
        .map_err(|err| OsError::Platform {
            detail: format!("UpdateProcThreadAttribute(HANDLE_LIST) failed: {err}"),
        })?;

        // SAFETY: `sec_caps` points to `sid_bytes` (live across `CreateProcessW`) with 0 caps.
        unsafe {
            UpdateProcThreadAttribute(
                attr_list.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                Some((&raw mut sec_caps).cast::<c_void>()),
                size_of::<SECURITY_CAPABILITIES>(),
                None,
                None,
            )
        }
        .map_err(|err| OsError::Platform {
            detail: format!("UpdateProcThreadAttribute(SECURITY_CAPABILITIES) failed: {err}"),
        })?;

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin_dup.0;
        si.StartupInfo.hStdOutput = stdout_dup.0;
        si.StartupInfo.hStdError = stderr_dup.0;
        si.lpAttributeList = attr_list.ptr;

        let flags = CREATE_NO_WINDOW
            | CREATE_SUSPENDED
            | CREATE_UNICODE_ENVIRONMENT
            | EXTENDED_STARTUPINFO_PRESENT;

        // SAFETY: all buffers and `attr_list` remain live across `CreateProcessW`.
        let res = unsafe {
            CreateProcessW(
                app_pcwstr,
                Some(PWSTR::from_raw(cmdline_wide.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(env_block.as_ptr().cast::<c_void>()),
                PCWSTR::from_raw(cwd_wide.as_ptr()),
                &si.StartupInfo,
                &mut pi,
            )
        };
        drop(attr_list);
        drop(stderr_dup);
        drop(stdout_dup);
        drop(stdin_dup);
        res
    };

    create_res.map_err(|err| OsError::Platform {
        detail: format!("CreateProcessW(AppContainer) failed: {err}"),
    })?;

    attach_job_and_resume(pi, job)
}

#[allow(clippy::too_many_arguments)]
fn launch_with_restricted_token(
    app_wide: Option<&[u16]>,
    cmdline_template: &[u16],
    cwd: &Path,
    env_block: &[u16],
    stdin_base: HANDLE,
    stdout_base: HANDLE,
    stderr_base: HANDLE,
    memory_limit: Option<u64>,
    rlimit_as_headroom: u64,
) -> Result<(u32, OwnedWinHandle, OwnedWinHandle)> {
    let token = create_low_il_restricted_token()?;
    let job = create_sandbox_job(memory_limit, rlimit_as_headroom)?;
    let attr_list = init_attr_list(1)?;
    let mut cmdline_wide = cmdline_template.to_vec();
    let cwd_wide = to_wide_null(cwd.as_os_str());
    let app_pcwstr = app_wide.map_or_else(PCWSTR::null, |b| PCWSTR::from_raw(b.as_ptr()));

    let mut pi = PROCESS_INFORMATION::default();
    let create_res = {
        let _guard = SANDBOX_SPAWN_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let stdin_dup = duplicate_inheritable(stdin_base)?;
        let stdout_dup = duplicate_inheritable(stdout_base)?;
        let stderr_dup = duplicate_inheritable(stderr_base)?;
        let handle_list = [stdin_dup.0, stdout_dup.0, stderr_dup.0];

        // SAFETY: `attr_list.ptr` has capacity 1 and `handle_list` stays live across spawn.
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
        .map_err(|err| OsError::Platform {
            detail: format!("UpdateProcThreadAttribute(HANDLE_LIST) failed: {err}"),
        })?;

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = stdin_dup.0;
        si.StartupInfo.hStdOutput = stdout_dup.0;
        si.StartupInfo.hStdError = stderr_dup.0;
        si.lpAttributeList = attr_list.ptr;

        let flags = CREATE_NO_WINDOW
            | CREATE_SUSPENDED
            | CREATE_UNICODE_ENVIRONMENT
            | EXTENDED_STARTUPINFO_PRESENT;

        // SAFETY: `token.0` is an owned primary restricted token and all pointers remain live.
        let res = unsafe {
            CreateProcessAsUserW(
                Some(token.0),
                app_pcwstr,
                Some(PWSTR::from_raw(cmdline_wide.as_mut_ptr())),
                None,
                None,
                true,
                flags,
                Some(env_block.as_ptr().cast::<c_void>()),
                PCWSTR::from_raw(cwd_wide.as_ptr()),
                &si.StartupInfo,
                &mut pi,
            )
        };
        drop(attr_list);
        drop(stderr_dup);
        drop(stdout_dup);
        drop(stdin_dup);
        res
    };

    create_res.map_err(|err| OsError::Platform {
        detail: format!("CreateProcessAsUserW(Low-IL restricted token) failed: {err}"),
    })?;

    attach_job_and_resume(pi, job)
}

fn attach_job_and_resume(
    pi: PROCESS_INFORMATION,
    job: OwnedWinHandle,
) -> Result<(u32, OwnedWinHandle, OwnedWinHandle)> {
    let proc_handle = OwnedWinHandle(pi.hProcess);
    let thread_handle = OwnedWinHandle(pi.hThread);

    // SAFETY: `job.0` and `proc_handle.0` are valid owned Win32 handles.
    if let Err(err) = unsafe { AssignProcessToJobObject(job.0, proc_handle.0) } {
        // SAFETY: kill the suspended child before returning so it never leaks.
        let _ = unsafe { TerminateProcess(proc_handle.0, 1) };
        return Err(OsError::Platform {
            detail: format!("AssignProcessToJobObject failed for sandboxed child: {err}"),
        });
    }

    // SAFETY: `thread_handle.0` is the suspended primary thread of the newly assigned process.
    let prev = unsafe { ResumeThread(thread_handle.0) };
    if prev == u32::MAX {
        // SAFETY: kill the suspended child immediately on resume failure.
        let _ = unsafe { TerminateProcess(proc_handle.0, 1) };
        return Err(OsError::Platform {
            detail: "ResumeThread failed for sandboxed child".into(),
        });
    }

    Ok((pi.dwProcessId, proc_handle, job))
}

/// Verifies from inside `__thumb-worker` on Windows that the current process is actually running
/// inside an AppContainer + Job Object (or Low-Integrity Restricted Token + Job Object fallback).
///
/// Fails closed if the worker was started outside Legion's sandboxed Windows spawner.
pub(super) fn verify_worker_sandbox() -> std::result::Result<SandboxReport, SandboxUnavailable> {
    let mut in_job = BOOL(0);
    // SAFETY: queries whether the current process belongs to any Job Object.
    let job_ok = unsafe { IsProcessInJob(GetCurrentProcess(), None, &mut in_job) }.is_ok()
        && in_job.as_bool();

    let mut token_h = HANDLE::default();
    // SAFETY: opens the current process token read-only for sandbox verification.
    if let Err(err) = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token_h) } {
        return Err(SandboxUnavailable {
            platform: "windows".to_owned(),
            missing: "process token inspection".to_owned(),
            detail: format!("OpenProcessToken(TOKEN_QUERY) failed: {err}"),
        });
    }
    let token = OwnedWinHandle(token_h);

    let mut is_ac: u32 = 0;
    let mut ret_len: u32 = 0;
    // SAFETY: `is_ac` is a 4-byte output buffer matching `TokenIsAppContainer`.
    let ac_ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenIsAppContainer,
            Some((&raw mut is_ac).cast::<c_void>()),
            size_of::<u32>() as u32,
            &mut ret_len,
        )
    }
    .is_ok()
        && is_ac != 0;

    if ac_ok && job_ok {
        return Ok(SandboxReport {
            available: true,
            complete: true,
            platform: "windows".to_owned(),
            mechanism: "appcontainer+job".to_owned(),
            detail: "Windows decode worker sandbox active (zero-capability AppContainer + single-process UI-restricted Job Object)."
                .to_owned(),
            missing: None,
        });
    }

    // Check whether the Windows-only Low-Integrity Restricted Token + Job Object fallback is active.
    let integrity_rid = query_token_integrity_rid(token.0);
    if job_ok && integrity_rid.is_some_and(|rid| rid <= SECURITY_MANDATORY_LOW_RID) {
        let ac_reason = std::env::var(AC_FALLBACK_REASON_ENV)
            .ok()
            .filter(|s| !s.trim().is_empty());
        let missing = match ac_reason {
            Some(ref reason) => format!("AppContainer isolation ({reason})"),
            None => {
                "AppContainer isolation (fell back to Low-Integrity restricted token)".to_owned()
            }
        };
        let detail = match ac_reason {
            Some(ref reason) => format!(
                "Windows decode worker running in Low-Integrity restricted token + Job Object fallback ({reason}; writes and child spawns blocked, user file reads and network still permitted)."
            ),
            None => "Windows decode worker running in Low-Integrity restricted token + Job Object fallback (writes and child spawns blocked; user file reads and network are still permitted).".to_owned(),
        };
        return Ok(SandboxReport {
            available: true,
            complete: false,
            platform: "windows".to_owned(),
            mechanism: "restricted-token+job".to_owned(),
            detail,
            missing: Some(missing),
        });
    }

    let missing = if !job_ok && !ac_ok {
        "AppContainer token and Job Object".to_owned()
    } else if !job_ok {
        "Job Object".to_owned()
    } else {
        "AppContainer or Low-Integrity restricted token".to_owned()
    };

    Err(SandboxUnavailable {
        platform: "windows".to_owned(),
        missing,
        detail: format!(
            "Windows decode worker is not running inside a verified sandbox (in_job={job_ok}, appcontainer={ac_ok}, integrity_rid={integrity_rid:?})"
        ),
    })
}

fn query_token_integrity_rid(token: HANDLE) -> Option<u32> {
    let mut needed = 0u32;
    // SAFETY: size query for `TokenIntegrityLevel`.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed) };
    if needed < size_of::<TOKEN_MANDATORY_LABEL>() as u32 {
        return None;
    }
    let mut buf = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
    // SAFETY: `buf` is pointer-aligned with at least `needed` bytes.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    // SAFETY: `buf` starts with an initialized `TOKEN_MANDATORY_LABEL`.
    let label = unsafe { &*buf.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let sid = label.Label.Sid;
    if sid.0.is_null() {
        return None;
    }
    // SAFETY: `sid` is a valid SID inside `buf`; reads sub-authority count and final RID.
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    if count == 0 {
        return None;
    }
    // SAFETY: `count - 1` is in bounds for `GetSidSubAuthority`.
    let rid = unsafe { *GetSidSubAuthority(sid, u32::from(count - 1)) };
    Some(rid)
}
