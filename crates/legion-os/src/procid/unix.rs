//! Unix process identity, boot identity, and process-tree termination.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, getpgid, getpgrp, getsid};
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::{OsError, Result};

use super::{
    EnvMarkerCheck, KillTreeReport, ProcessId, SignalOutcome, child_not_older_than_parent, is_alive,
};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ANCESTOR_HOPS: usize = 256;
const MAX_SSHD_ANCESTOR_HOPS: usize = 4;

/// Backward clock-step tolerance for exact `ProcessId.start_time` parent-before-child checks:
/// Linux `/proc/<pid>/stat` jiffies are monotonic (zero tolerance); macOS `proc_bsdinfo` uses
/// wall-clock microseconds and can step backward across NTP or dual-boot RTC adjustments.
#[cfg(target_os = "linux")]
const EXACT_START_BACKWARD_TOLERANCE: u64 = 0;
#[cfg(target_os = "macos")]
const EXACT_START_BACKWARD_TOLERANCE: u64 = super::WALL_CLOCK_BACKWARD_TOLERANCE_MICROS;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const EXACT_START_BACKWARD_TOLERANCE: u64 = super::WALL_CLOCK_BACKWARD_TOLERANCE_SECS;

/// Tolerance for second-resolution `sysinfo` start times in BFS parent checks.
#[cfg(target_os = "linux")]
const SYSINFO_START_BACKWARD_TOLERANCE_SECS: u64 = 1;
#[cfg(not(target_os = "linux"))]
const SYSINFO_START_BACKWARD_TOLERANCE_SECS: u64 = super::WALL_CLOCK_BACKWARD_TOLERANCE_SECS;

pub(super) fn is_protected(identity: &ProcessId) -> Option<&'static str> {
    if identity.pid == 1 {
        return Some("init process (pid 1)");
    }
    if is_ancestor_of_current(identity) {
        return Some("ancestor of current process");
    }
    None
}

#[cfg(target_os = "linux")]
pub(super) fn self_and_ancestors() -> HashSet<u32> {
    let mut seen = HashSet::new();
    let own = std::process::id();
    seen.insert(own);
    let Some((mut parent_pid, mut child_start)) = linux_parent_and_start(own) else {
        return seen;
    };
    for _ in 0..MAX_ANCESTOR_HOPS {
        if parent_pid <= 1 || !seen.insert(parent_pid) {
            break;
        }
        match linux_parent_and_start(parent_pid) {
            Some((next_ppid, parent_start)) => {
                if parent_start > child_start {
                    seen.remove(&parent_pid);
                    break;
                }
                child_start = parent_start;
                parent_pid = next_ppid;
            }
            None => break,
        }
    }
    seen
}

#[cfg(target_os = "macos")]
pub(super) fn self_and_ancestors() -> HashSet<u32> {
    super::macos_self_and_ancestors(
        std::process::id(),
        MAX_ANCESTOR_HOPS,
        macos_proc::parent_and_start,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn self_and_ancestors() -> HashSet<u32> {
    let mut seen = HashSet::new();
    let own = std::process::id();
    seen.insert(own);
    let Ok(mut system) = System::new() else {
        return seen;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut cur = sysinfo::Pid::from_u32(own);
    for _ in 0..MAX_ANCESTOR_HOPS {
        let Some(parent) = system.process(cur).and_then(|p| p.parent()) else {
            break;
        };
        let parent_pid = parent.as_u32();
        if parent_pid <= 1 || !seen.insert(parent_pid) {
            break;
        }
        cur = parent;
    }
    seen
}

#[cfg(target_os = "linux")]
fn linux_parent_and_start(pid: u32) -> Option<(u32, u64)> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (state, ppid, start_ticks) = super::parse_linux_proc_stat_full(&contents)?;
    if matches!(state, 'Z' | 'X' | 'x') {
        return None;
    }
    Some((ppid, start_ticks))
}

#[cfg(target_os = "linux")]
fn is_ancestor_of_current(identity: &ProcessId) -> bool {
    let Some((mut parent_pid, mut child_start)) = linux_parent_and_start(std::process::id()) else {
        return false;
    };
    let mut seen = HashSet::new();
    seen.insert(std::process::id());

    for _ in 0..MAX_ANCESTOR_HOPS {
        if parent_pid <= 1 || !seen.insert(parent_pid) {
            break;
        }
        match linux_parent_and_start(parent_pid) {
            Some((next_ppid, parent_start)) => {
                // A genuine ancestor cannot have started after its child.
                if parent_start > child_start {
                    break;
                }
                if parent_pid == identity.pid && parent_start == identity.start_time {
                    return true;
                }
                child_start = parent_start;
                parent_pid = next_ppid;
            }
            None => {
                // Under Linux `hidepid`, reading a privileged ancestor's `/proc/<pid>/stat` can
                // fail even though our own `ppid` revealed it; err on the side of protecting it.
                return parent_pid == identity.pid;
            }
        }
    }
    false
}

#[cfg(target_os = "macos")]
fn is_ancestor_of_current(identity: &ProcessId) -> bool {
    super::macos_is_ancestor_of_current(
        std::process::id(),
        MAX_ANCESTOR_HOPS,
        identity,
        macos_proc::parent_and_start,
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_ancestor_of_current(identity: &ProcessId) -> bool {
    let Ok(mut system) = System::new() else {
        return false;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let mut cur = sysinfo::Pid::from_u32(std::process::id());
    let mut seen = HashSet::new();
    seen.insert(cur.as_u32());

    for _ in 0..MAX_ANCESTOR_HOPS {
        let Some(parent) = system.process(cur).and_then(|p| p.parent()) else {
            break;
        };
        let parent_pid = parent.as_u32();
        if parent_pid <= 1 || !seen.insert(parent_pid) {
            break;
        }
        if parent_pid == identity.pid {
            return match super::of_pid(parent_pid) {
                Ok(Some(observed)) => identity.matches(&observed),
                _ => true,
            };
        }
        cur = parent;
    }
    false
}

#[cfg(target_os = "linux")]
fn linux_same_account_sshd(pid: u32, expected_start: Option<u64>) -> Option<ProcessId> {
    if pid <= 1 || pid == std::process::id() {
        return None;
    }
    let (_ppid, start_ticks) = linux_parent_and_start(pid)?;
    if let Some(expected) = expected_start
        && start_ticks != expected
    {
        return None;
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let (ruid, euid) = super::parse_linux_proc_status_uids(&status)?;
    let my_euid = nix::unistd::Uid::effective().as_raw();
    if ruid != my_euid || euid != my_euid {
        return None;
    }
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    let cmdline_bytes = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv = super::parse_linux_proc_cmdline(&cmdline_bytes);
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    if !super::is_sshd_session_process(comm.trim(), &argv_refs, false) {
        return None;
    }
    Some(ProcessId::new(pid, start_ticks))
}

#[cfg(target_os = "linux")]
pub(super) fn sshd_session_ancestor() -> Option<ProcessId> {
    let own_pid = std::process::id();
    let (mut parent_pid, mut child_start) = linux_parent_and_start(own_pid)?;
    let mut seen = HashSet::new();
    seen.insert(own_pid);

    for _ in 0..MAX_SSHD_ANCESTOR_HOPS {
        if parent_pid <= 1 || !seen.insert(parent_pid) {
            break;
        }
        let (next_ppid, parent_start) = linux_parent_and_start(parent_pid)?;
        if parent_start > child_start {
            break;
        }
        if let Some(sshd_id) = linux_same_account_sshd(parent_pid, Some(parent_start)) {
            return Some(sshd_id);
        }
        let comm = std::fs::read_to_string(format!("/proc/{parent_pid}/comm")).ok()?;
        if !super::is_shell_wrapper_comm(comm.trim()) {
            break;
        }
        child_start = parent_start;
        parent_pid = next_ppid;
    }
    None
}

#[cfg(target_os = "linux")]
pub(super) fn verify_same_account_sshd(identity: &ProcessId) -> bool {
    linux_same_account_sshd(identity.pid, Some(identity.start_time)).is_some()
}

#[cfg(target_os = "linux")]
pub(super) fn executable_of(identity: &ProcessId) -> Option<PathBuf> {
    let observed = of_pid(identity.pid, false).ok()??;
    if !identity.matches(&observed) {
        return None;
    }
    std::fs::read_link(format!("/proc/{}/exe", identity.pid)).ok()
}

#[cfg(target_os = "macos")]
fn macos_same_account_sshd(pid: u32, expected_start: Option<u64>) -> Option<ProcessId> {
    if pid <= 1 || pid == std::process::id() {
        return None;
    }
    let (_ppid, start_us, ruid, euid, comm) = macos_proc::bsd_details(pid)?;
    if let Some(expected) = expected_start
        && start_us != expected
    {
        return None;
    }
    let my_euid = nix::unistd::Uid::effective().as_raw();
    if ruid != my_euid || euid != my_euid {
        return None;
    }
    let argv = macos_proc::query_argv(pid)?;
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    if !super::is_sshd_session_process(&comm, &argv_refs, false) {
        return None;
    }
    Some(ProcessId::new(pid, start_us))
}

#[cfg(target_os = "macos")]
pub(super) fn sshd_session_ancestor() -> Option<ProcessId> {
    let own_pid = std::process::id();
    let (mut parent_pid, mut child_start) = macos_proc::parent_and_start(own_pid)?;
    let mut seen = HashSet::new();
    seen.insert(own_pid);

    for _ in 0..MAX_SSHD_ANCESTOR_HOPS {
        if parent_pid <= 1 || !seen.insert(parent_pid) {
            break;
        }
        let (next_ppid, parent_start, _ruid, _euid, comm) = macos_proc::bsd_details(parent_pid)?;
        if !child_not_older_than_parent(child_start, parent_start, EXACT_START_BACKWARD_TOLERANCE) {
            break;
        }
        if let Some(sshd_id) = macos_same_account_sshd(parent_pid, Some(parent_start)) {
            return Some(sshd_id);
        }
        if !super::is_shell_wrapper_comm(&comm) {
            break;
        }
        child_start = parent_start;
        parent_pid = next_ppid;
    }
    None
}

#[cfg(target_os = "macos")]
pub(super) fn verify_same_account_sshd(identity: &ProcessId) -> bool {
    macos_same_account_sshd(identity.pid, Some(identity.start_time)).is_some()
}

#[cfg(target_os = "macos")]
pub(super) fn executable_of(identity: &ProcessId) -> Option<PathBuf> {
    let observed = of_pid(identity.pid, false).ok()??;
    if !identity.matches(&observed) {
        return None;
    }
    macos_proc::query_executable_path(identity.pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn sshd_session_ancestor() -> Option<ProcessId> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn verify_same_account_sshd(_identity: &ProcessId) -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn executable_of(_identity: &ProcessId) -> Option<PathBuf> {
    None
}

#[cfg(target_os = "linux")]
pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
    use std::fs;
    use std::io;

    // On Linux `/proc/<tid>/stat` reads like a process even for non-main threads, so verify that
    // `pid` is actually a thread-group leader (`Tgid == pid` in `/proc/<pid>/status`) before
    // accepting it as a process identity.
    let status_path = format!("/proc/{pid}/status");
    let status = match fs::read_to_string(&status_path) {
        Ok(text) => text,
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.raw_os_error() == Some(libc_esrch()) =>
        {
            return Ok(None);
        }
        Err(err) => {
            return Err(OsError::from_io(
                format!("failed reading {status_path}"),
                err,
            ));
        }
    };
    if let Some(tgid) = super::parse_linux_proc_status_tgid(&status)
        && tgid != pid
    {
        return Err(OsError::InvalidArgument {
            detail: format!("id {pid} is a thread of process {tgid}, not a process"),
        });
    }

    let stat_path = format!("/proc/{pid}/stat");
    let contents = match fs::read_to_string(&stat_path) {
        Ok(text) => text,
        Err(err)
            if err.kind() == io::ErrorKind::NotFound
                || err.raw_os_error() == Some(libc_esrch()) =>
        {
            return Ok(None);
        }
        Err(err) => {
            return Err(OsError::from_io(format!("failed reading {stat_path}"), err));
        }
    };

    let Some((state, start_ticks)) = super::parse_linux_proc_stat(&contents) else {
        return Ok(None);
    };
    if !include_zombie && matches!(state, 'Z' | 'X' | 'x') {
        return Ok(None);
    }
    Ok(Some(ProcessId::new(pid, start_ticks)))
}

#[cfg(target_os = "linux")]
const fn libc_esrch() -> i32 {
    Errno::ESRCH as i32
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos_proc {
    use std::mem::{offset_of, size_of, zeroed};

    use crate::procid::{
        MACOS_SZOMB, MacosProcSample, ProcessId, macos_start_time_us, resolve_macos_pid_identity,
    };
    use crate::{OsError, Result};

    /// Minimal ABI-matching layout for Darwin 64-bit `struct kinfo_proc` (`<sys/sysctl.h>` +
    /// `<sys/proc.h>`, 648 bytes on both `aarch64-apple-darwin` and `x86_64-apple-darwin`).
    ///
    /// Verified field offsets:
    /// - `0..16`: `kp_proc.p_un.__p_starttime` (`libc::timeval`: `tv_sec: i64`, `tv_usec: i32`)
    /// - `36`: `kp_proc.p_stat` (`libc::c_char`; `SZOMB == 5`)
    /// - `40..44`: `kp_proc.p_pid` (`libc::pid_t`)
    #[repr(C)]
    struct KinfoProc {
        p_starttime: libc::timeval,
        _pad_to_stat: [u8; 20],
        p_stat: libc::c_char,
        _pad_to_pid: [u8; 3],
        p_pid: libc::pid_t,
        _tail: [u8; 648 - 44],
    }

    const _: () = {
        assert!(size_of::<KinfoProc>() == 648);
        assert!(offset_of!(KinfoProc, p_starttime) == 0);
        assert!(offset_of!(KinfoProc, p_stat) == 36);
        assert!(offset_of!(KinfoProc, p_pid) == 40);
    };

    pub(super) fn query_bsd_info(raw_pid: i32) -> Option<MacosProcSample> {
        // SAFETY: `libc::proc_bsdinfo` is a plain C struct safe to zero-initialize; `proc_pidinfo`
        // receives its exact byte size `expected`, writes at most `expected` bytes into `info`,
        // and we accept the result only when the return value equals `expected`.
        let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
        let expected = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(
                raw_pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                expected,
            )
        };
        if written != expected {
            return None;
        }
        let start_us = macos_start_time_us(info.pbi_start_tvsec, info.pbi_start_tvusec)?;
        Some(MacosProcSample {
            status: info.pbi_status,
            start_us,
        })
    }

    pub(super) fn query_kinfo(raw_pid: i32) -> Option<MacosProcSample> {
        let mut mib: [libc::c_int; 4] = [
            libc::CTL_KERN,
            libc::KERN_PROC,
            libc::KERN_PROC_PID,
            raw_pid,
        ];
        // SAFETY: `KinfoProc` is a plain `#[repr(C)]` struct matching Darwin's 648-byte
        // `struct kinfo_proc` ABI (checked by compile-time size/offset assertions above) and is
        // safe to zero-initialize. `size` is initialized to `size_of::<KinfoProc>()` so `sysctl`
        // writes at most `size_of::<KinfoProc>()` bytes into `kp`. We validate both the `rc == 0`
        // return code and that the kernel wrote a full struct (`size >= size_of::<KinfoProc>()`;
        // `sysctl` returns `rc == 0, size == 0` for a nonexistent PID).
        let mut kp: KinfoProc = unsafe { zeroed() };
        let mut size: libc::size_t = size_of::<KinfoProc>();
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                (&mut kp as *mut KinfoProc).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || size < size_of::<KinfoProc>() || kp.p_pid != raw_pid {
            return None;
        }
        let tv_sec = u64::try_from(kp.p_starttime.tv_sec).ok()?;
        let tv_usec = u64::try_from(kp.p_starttime.tv_usec).ok()?;
        let start_us = macos_start_time_us(tv_sec, tv_usec)?;
        let status = u32::from(kp.p_stat as u8);
        Some(MacosProcSample { status, start_us })
    }

    pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
        let Ok(raw_pid) = i32::try_from(pid) else {
            return Err(OsError::InvalidArgument {
                detail: format!("pid {pid} exceeds Unix pid_t range"),
            });
        };

        let bsd_info = query_bsd_info(raw_pid);
        let kinfo = if bsd_info.is_none() {
            query_kinfo(raw_pid)
        } else {
            None
        };
        Ok(resolve_macos_pid_identity(
            pid,
            include_zombie,
            bsd_info,
            kinfo,
        ))
    }

    pub(super) fn parent_and_start(pid: u32) -> Option<(u32, u64)> {
        let raw_pid = i32::try_from(pid).ok()?;
        // SAFETY: `libc::proc_bsdinfo` is a plain C struct safe to zero-initialize; `proc_pidinfo`
        // writes at most `size_of::<libc::proc_bsdinfo>()` bytes into `info`.
        let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
        let expected = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(
                raw_pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                expected,
            )
        };
        if written != expected || info.pbi_status == MACOS_SZOMB {
            return None;
        }
        let start_us = macos_start_time_us(info.pbi_start_tvsec, info.pbi_start_tvusec)?;
        Some((info.pbi_ppid, start_us))
    }

    pub(super) fn bsd_details(pid: u32) -> Option<(u32, u64, u32, u32, String)> {
        let raw_pid = i32::try_from(pid).ok()?;
        // SAFETY: `libc::proc_bsdinfo` is a plain C struct safe to zero-initialize; `proc_pidinfo`
        // writes at most `size_of::<libc::proc_bsdinfo>()` bytes into `info`.
        let mut info: libc::proc_bsdinfo = unsafe { zeroed() };
        let expected = size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(
                raw_pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                expected,
            )
        };
        if written != expected || info.pbi_status == MACOS_SZOMB {
            return None;
        }
        let start_us = macos_start_time_us(info.pbi_start_tvsec, info.pbi_start_tvusec)?;
        let name_bytes = info.pbi_name.map(|c| c as u8);
        let comm_bytes = info.pbi_comm.map(|c| c as u8);
        let comm = crate::procid::parse_macos_bsd_comm(&name_bytes, &comm_bytes);
        Some((info.pbi_ppid, start_us, info.pbi_ruid, info.pbi_uid, comm))
    }

    pub(super) fn query_executable_path(pid: u32) -> Option<std::path::PathBuf> {
        const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
        let raw_pid = libc::c_int::try_from(pid).ok()?;
        let mut buf = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
        // SAFETY: `buf` holds `PROC_PIDPATHINFO_MAXSIZE` writable bytes and `proc_pidpath` returns
        // the non-negative byte length written (excluding NUL) or <= 0 on failure.
        let written = unsafe {
            libc::proc_pidpath(
                raw_pid,
                buf.as_mut_ptr().cast(),
                PROC_PIDPATHINFO_MAXSIZE as u32,
            )
        };
        crate::procid::parse_macos_proc_path(&buf, written)
    }

    pub(super) fn query_argv(pid: u32) -> Option<Vec<String>> {
        let raw_pid = libc::c_int::try_from(pid).ok()?;
        let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw_pid];

        for _ in 0..2 {
            let mut size: libc::size_t = 0;
            // SAFETY: Querying buffer size with a valid 3-element MIB and null output pointer.
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as libc::c_uint,
                    std::ptr::null_mut(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
                || size == 0
            {
                return None;
            }
            let mut buf = vec![0u8; size];
            // SAFETY: `buf` has exact byte capacity `size` and remains alive across `sysctl`.
            let rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as libc::c_uint,
                    buf.as_mut_ptr().cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if rc == 0 {
                buf.truncate(size);
                return crate::procid::parse_macos_procargs2_argv(&buf);
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
                return None;
            }
        }
        None
    }

    pub(super) fn check_env_marker(
        pid: u32,
        key: &str,
        expected: &str,
    ) -> crate::procid::EnvMarkerCheck {
        use crate::procid::EnvMarkerCheck;

        let Ok(raw_pid) = libc::c_int::try_from(pid) else {
            return EnvMarkerCheck::Unverifiable;
        };
        let mut mib: [libc::c_int; 3] = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw_pid];

        for _ in 0..2 {
            let mut size: libc::size_t = 0;
            // SAFETY: Querying the required `KERN_PROCARGS2` buffer size passes a valid 3-element
            // MIB, null output pointer, and valid mutable `size`.
            if unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as libc::c_uint,
                    std::ptr::null_mut(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            } != 0
                || size == 0
            {
                return EnvMarkerCheck::Unverifiable;
            }

            let mut buf = vec![0u8; size];
            // SAFETY: `buf` has exact byte capacity `size` and remains alive across `sysctl`.
            let rc = unsafe {
                libc::sysctl(
                    mib.as_mut_ptr(),
                    mib.len() as libc::c_uint,
                    buf.as_mut_ptr().cast(),
                    &mut size,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if rc == 0 {
                buf.truncate(size);
                return match crate::procid::parse_macos_procargs2_env_has_marker(
                    &buf, key, expected,
                ) {
                    Some(true) => EnvMarkerCheck::Matched,
                    Some(false) => EnvMarkerCheck::NotMatched,
                    None => EnvMarkerCheck::Unverifiable,
                };
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
                return EnvMarkerCheck::Unverifiable;
            }
        }
        EnvMarkerCheck::Unverifiable
    }
}

#[cfg(target_os = "macos")]
pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
    macos_proc::of_pid(pid, include_zombie)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
    let mut system = System::new().map_err(|err| OsError::Platform {
        detail: format!("failed to initialize system process view: {err}"),
    })?;
    let sys_pid = sysinfo::Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    let Some(proc_info) = system.process(sys_pid) else {
        return Ok(None);
    };
    if !include_zombie
        && matches!(
            proc_info.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        )
    {
        return Ok(None);
    }
    let start_time = proc_info.start_time();
    if start_time == 0 {
        return Ok(None);
    }
    Ok(Some(ProcessId::new(pid, start_time)))
}

#[cfg(target_os = "linux")]
pub(super) fn boot_id() -> Result<String> {
    const PATH: &str = "/proc/sys/kernel/random/boot_id";
    let raw = std::fs::read_to_string(PATH)
        .map_err(|err| OsError::from_io(format!("failed reading {PATH}"), err))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OsError::Platform {
            detail: format!("{PATH} was empty"),
        });
    }
    Ok(trimmed.to_owned())
}

#[cfg(target_os = "macos")]
pub(super) fn boot_id() -> Result<String> {
    // kern.bootsessionuuid is a kernel-generated UUID immutable across NTP/wall-clock steps.
    let mut cmd = std::process::Command::new("/usr/sbin/sysctl");
    cmd.args(["-n", "kern.bootsessionuuid"]);
    let raw =
        super::run_command_stdout_with_timeout(cmd, Duration::from_secs(5), "/usr/sbin/sysctl")?;
    super::parse_macos_boot_id(&raw)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn boot_id() -> Result<String> {
    let secs = System::boot_time().map_err(|err| OsError::Platform {
        detail: format!("failed to read system boot time: {err}"),
    })?;
    Ok(format!("boot-sec-{secs}"))
}

#[cfg(target_os = "linux")]
fn check_env_marker(pid: u32, key: &str, expected: &str) -> EnvMarkerCheck {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(bytes) => {
            if super::environ_bytes_has_marker(&bytes, key, expected) {
                EnvMarkerCheck::Matched
            } else {
                EnvMarkerCheck::NotMatched
            }
        }
        Err(_) => EnvMarkerCheck::Unverifiable,
    }
}

#[cfg(target_os = "macos")]
fn check_env_marker(pid: u32, key: &str, expected: &str) -> EnvMarkerCheck {
    macos_proc::check_env_marker(pid, key, expected)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_env_marker(_pid: u32, _key: &str, _expected: &str) -> EnvMarkerCheck {
    EnvMarkerCheck::Unverifiable
}

pub(super) async fn tree_is_active(identity: &ProcessId, marker: Option<&str>) -> Result<bool> {
    if i32::try_from(identity.pid).is_err() {
        return Ok(false);
    }
    let identity = *identity;
    let marker_owned = marker.map(str::to_owned);
    let own_pid = std::process::id();

    tokio::task::spawn_blocking(move || -> Result<bool> {
        // Fast path: check root identity directly first (single /proc/<pid>/stat or proc_pidinfo).
        if let Some(observed) = super::of_pid(identity.pid)?
            && identity.matches(&observed)
        {
            return Ok(true);
        }

        // Once the root itself is gone, treat another process as part of the tree ONLY if it
        // carries the detach environment marker proving ownership across PID reuse.
        let Some(ref marker_val) = marker_owned else {
            return Ok(false);
        };

        Ok(has_live_marked_process(marker_val, own_pid))
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("tree_is_active worker task failed: {err}"),
    })?
}

pub(super) async fn find_marked_live_processes(marker: &str) -> Result<Vec<ProcessId>> {
    let marker_owned = marker.to_owned();
    let own_pid = std::process::id();
    tokio::task::spawn_blocking(move || collect_live_marked_processes(&marker_owned, own_pid))
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("find_marked_live_processes worker task failed: {err}"),
        })
}

#[cfg(target_os = "linux")]
fn collect_live_marked_processes(marker: &str, own_pid: u32) -> Vec<ProcessId> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == own_pid {
            continue;
        }
        if check_env_marker(pid, crate::detach::DETACH_MARKER_ENV, marker)
            == EnvMarkerCheck::Matched
            && let Ok(Some(id)) = super::of_pid(pid)
        {
            found.push(id);
        }
    }
    found.sort_by_key(|id| id.pid);
    found
}

#[cfg(not(target_os = "linux"))]
fn collect_live_marked_processes(marker: &str, own_pid: u32) -> Vec<ProcessId> {
    let Ok(mut system) = System::new() else {
        return Vec::new();
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let mut found = Vec::new();
    for (sys_pid, proc_info) in system.processes() {
        if matches!(
            proc_info.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) {
            continue;
        }
        let pid = sys_pid.as_u32();
        if pid == 0 || pid == own_pid {
            continue;
        }
        if check_env_marker(pid, crate::detach::DETACH_MARKER_ENV, marker)
            == EnvMarkerCheck::Matched
            && let Ok(Some(id)) = super::of_pid(pid)
        {
            found.push(id);
        }
    }
    found.sort_by_key(|id| id.pid);
    found
}

#[cfg(target_os = "linux")]
fn has_live_marked_process(marker: &str, own_pid: u32) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        if pid == 0 || pid == own_pid {
            continue;
        }
        if check_env_marker(pid, crate::detach::DETACH_MARKER_ENV, marker)
            == EnvMarkerCheck::Matched
            && matches!(super::of_pid(pid), Ok(Some(_)))
        {
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn has_live_marked_process(marker: &str, own_pid: u32) -> bool {
    let Ok(mut system) = System::new() else {
        return false;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    for (sys_pid, proc_info) in system.processes() {
        if matches!(
            proc_info.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) {
            continue;
        }
        let pid = sys_pid.as_u32();
        if pid == 0 || pid == own_pid {
            continue;
        }
        if check_env_marker(pid, crate::detach::DETACH_MARKER_ENV, marker)
            == EnvMarkerCheck::Matched
            && matches!(super::of_pid(pid), Ok(Some(_)))
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux_pidfd {
    use std::io;

    use nix::errno::Errno;
    use nix::sys::signal::Signal;

    use crate::procid::ProcessId;

    /// Owned Linux `pidfd` pinning a specific kernel `struct pid` instance so `pidfd_send_signal`
    /// can never hit a recycled PID even after the target exits and is reaped.
    pub(super) struct PidFd(libc::c_int);

    impl Drop for PidFd {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an owned file descriptor returned by `SYS_pidfd_open` and is
            // closed exactly once on drop.
            let _ = unsafe { libc::close(self.0) };
        }
    }

    pub(super) enum VerifiedOpen {
        Pinned(PidFd),
        GoneOrMismatch,
        /// `pidfd_open` returned `EINVAL` (e.g. non-leader thread ID) or `of_pid` saw `Tgid != pid`.
        NotAProcess,
        /// `pidfd_open` failed with an unexpected kernel/limit/permission error; never fall back
        /// to bare-PID `kill()`.
        Refused(Errno),
        /// Kernel does not implement `pidfd_open` (`ENOSYS`): the sole case where a verified
        /// bare-PID fallback is permitted.
        Unsupported,
    }

    impl PidFd {
        pub(super) fn send_signal(&self, sig: Signal) -> std::result::Result<(), Errno> {
            self.send_raw(sig as libc::c_int)
        }

        pub(super) fn send_raw(&self, sig: libc::c_int) -> std::result::Result<(), Errno> {
            // SAFETY: `self.0` is a valid pidfd, `info` is null (kernel fills default siginfo),
            // and `flags` is 0 as required by `pidfd_send_signal(2)`.
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0,
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0 as libc::c_uint,
                )
            };
            if rc == 0 {
                Ok(())
            } else {
                let raw = io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EINVAL);
                Err(Errno::from_raw(raw))
            }
        }
    }

    /// Open a `pidfd` for `identity.pid` and verify `identity.start_time` WHILE holding the fd.
    ///
    /// Once verified, `PidFd::send_signal` is immune to PID reuse: if the original process exits
    /// and another process reuses its PID number, the pidfd still refers to the exited instance
    /// and `pidfd_send_signal` returns `ESRCH`.
    ///
    /// `EINVAL` (returned by the kernel for non-thread-group-leader thread IDs) is treated as
    /// [`VerifiedOpen::NotAProcess`], never as a fallback to `kill()`. Only `ENOSYS` permits a
    /// bare-PID fallback.
    pub(super) fn open_verified(identity: &ProcessId) -> VerifiedOpen {
        let Ok(raw_pid) = libc::c_int::try_from(identity.pid) else {
            return VerifiedOpen::GoneOrMismatch;
        };
        // SAFETY: `SYS_pidfd_open` takes a plain `pid_t` and flags (0); the kernel automatically
        // sets `O_CLOEXEC` on the returned file descriptor.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw_pid, 0 as libc::c_uint) };
        if fd < 0 {
            let raw_err = io::Error::last_os_error().raw_os_error();
            return match raw_err {
                Some(libc::ESRCH | libc::ENOENT) => match crate::procid::of_pid(identity.pid) {
                    Err(crate::OsError::InvalidArgument { .. }) => VerifiedOpen::NotAProcess,
                    _ => VerifiedOpen::GoneOrMismatch,
                },
                Some(libc::EINVAL) => VerifiedOpen::NotAProcess,
                Some(libc::ENOSYS) => VerifiedOpen::Unsupported,
                Some(code) => VerifiedOpen::Refused(Errno::from_raw(code)),
                None => VerifiedOpen::Refused(Errno::EINVAL),
            };
        }

        let pidfd = PidFd(fd as libc::c_int);
        match crate::procid::of_pid(identity.pid) {
            Ok(Some(observed)) if identity.matches(&observed) => VerifiedOpen::Pinned(pidfd),
            Err(crate::OsError::InvalidArgument { .. }) => VerifiedOpen::NotAProcess,
            Err(crate::OsError::PermissionDenied { .. }) => VerifiedOpen::Refused(Errno::EPERM),
            Ok(Some(_)) | Ok(None) | Err(_) => VerifiedOpen::GoneOrMismatch,
        }
    }
}

fn signal_verified_once(identity: &ProcessId, sig: Signal) -> std::result::Result<bool, Errno> {
    #[cfg(target_os = "linux")]
    match linux_pidfd::open_verified(identity) {
        linux_pidfd::VerifiedOpen::Pinned(pidfd) => {
            return match pidfd.send_signal(sig) {
                Ok(()) => Ok(false),
                Err(Errno::ESRCH) => Ok(true),
                Err(err) => Err(err),
            };
        }
        linux_pidfd::VerifiedOpen::GoneOrMismatch => return Ok(true),
        linux_pidfd::VerifiedOpen::NotAProcess => return Err(Errno::EINVAL),
        linux_pidfd::VerifiedOpen::Refused(err) => return Err(err),
        linux_pidfd::VerifiedOpen::Unsupported => {}
    }

    if !is_alive(identity) {
        return Ok(true);
    }
    let Ok(raw) = i32::try_from(identity.pid) else {
        return Ok(true);
    };
    match kill(Pid::from_raw(raw), sig) {
        Ok(()) => Ok(false),
        Err(Errno::ESRCH) => Ok(true),
        Err(err) => Err(err),
    }
}

pub(super) async fn signal_process(identity: &ProcessId, force: bool) -> Result<SignalOutcome> {
    let raw_pid = i32::try_from(identity.pid).map_err(|_| OsError::InvalidArgument {
        detail: format!("pid {} exceeds Unix pid_t range", identity.pid),
    })?;
    let nix_pid = Pid::from_raw(raw_pid);
    let identity = *identity;
    let sig = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };

    tokio::task::spawn_blocking(move || -> Result<SignalOutcome> {
        #[cfg(target_os = "linux")]
        match linux_pidfd::open_verified(&identity) {
            linux_pidfd::VerifiedOpen::Pinned(pidfd) => {
                return match pidfd.send_signal(sig) {
                    Ok(()) => Ok(SignalOutcome::Signaled),
                    Err(Errno::ESRCH) => Ok(SignalOutcome::AlreadyGone),
                    Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                        detail: format!("permission denied signaling process {}", identity.pid),
                        source: None,
                    }),
                    Err(err) => Err(OsError::Platform {
                        detail: format!("failed signaling process {}: {err}", identity.pid),
                    }),
                };
            }
            linux_pidfd::VerifiedOpen::GoneOrMismatch => {
                return match super::of_pid(identity.pid)? {
                    Some(_) => Ok(SignalOutcome::IdentityMismatch),
                    None => Ok(SignalOutcome::AlreadyGone),
                };
            }
            linux_pidfd::VerifiedOpen::NotAProcess => {
                return Err(OsError::InvalidArgument {
                    detail: format!("id {} is a thread, not a process", identity.pid),
                });
            }
            linux_pidfd::VerifiedOpen::Refused(Errno::EPERM | Errno::EACCES) => {
                return Err(OsError::PermissionDenied {
                    detail: format!("permission denied opening process {}", identity.pid),
                    source: None,
                });
            }
            linux_pidfd::VerifiedOpen::Refused(err) => {
                return Err(OsError::Platform {
                    detail: format!("pidfd_open failed for process {}: {err}", identity.pid),
                });
            }
            linux_pidfd::VerifiedOpen::Unsupported => {}
        }

        match super::of_pid(identity.pid)? {
            None => return Ok(SignalOutcome::AlreadyGone),
            Some(observed) if !identity.matches(&observed) => {
                return Ok(SignalOutcome::IdentityMismatch);
            }
            Some(_) => {}
        }
        match kill(nix_pid, sig) {
            Ok(()) => Ok(SignalOutcome::Signaled),
            Err(Errno::ESRCH) => Ok(SignalOutcome::AlreadyGone),
            Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                detail: format!("permission denied signaling process {}", identity.pid),
                source: None,
            }),
            Err(err) => Err(OsError::Platform {
                detail: format!("failed signaling process {}: {err}", identity.pid),
            }),
        }
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("signal_process worker task failed: {err}"),
    })?
}

pub(super) async fn kill_process(
    identity: &ProcessId,
    force: bool,
    grace: Duration,
) -> Result<bool> {
    let raw_pid = i32::try_from(identity.pid).map_err(|_| OsError::InvalidArgument {
        detail: format!("pid {} exceeds Unix pid_t range", identity.pid),
    })?;
    let nix_pid = Pid::from_raw(raw_pid);
    let identity = *identity;

    let initial_sig = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };

    #[cfg(target_os = "linux")]
    let (already_gone, pinned_pidfd): (bool, Option<Arc<linux_pidfd::PidFd>>) =
        tokio::task::spawn_blocking(move || -> Result<(bool, Option<Arc<linux_pidfd::PidFd>>)> {
            match linux_pidfd::open_verified(&identity) {
                linux_pidfd::VerifiedOpen::Pinned(pidfd) => {
                    let pidfd = Arc::new(pidfd);
                    match pidfd.send_signal(initial_sig) {
                        Ok(()) => Ok((false, Some(pidfd))),
                        Err(Errno::ESRCH) => Ok((true, None)),
                        Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                            detail: format!("permission denied signaling process {}", identity.pid),
                            source: None,
                        }),
                        Err(err) => Err(OsError::Platform {
                            detail: format!("failed signaling process {}: {err}", identity.pid),
                        }),
                    }
                }
                linux_pidfd::VerifiedOpen::GoneOrMismatch => Ok((true, None)),
                linux_pidfd::VerifiedOpen::NotAProcess => Err(OsError::InvalidArgument {
                    detail: format!("id {} is a thread, not a process", identity.pid),
                }),
                linux_pidfd::VerifiedOpen::Refused(Errno::EPERM | Errno::EACCES) => {
                    Err(OsError::PermissionDenied {
                        detail: format!("permission denied opening process {}", identity.pid),
                        source: None,
                    })
                }
                linux_pidfd::VerifiedOpen::Refused(err) => Err(OsError::Platform {
                    detail: format!("pidfd_open failed for process {}: {err}", identity.pid),
                }),
                linux_pidfd::VerifiedOpen::Unsupported => {
                    match super::of_pid(identity.pid)? {
                        Some(observed) if identity.matches(&observed) => {}
                        _ => return Ok((true, None)),
                    }
                    match kill(nix_pid, initial_sig) {
                        Ok(()) => Ok((false, None)),
                        Err(Errno::ESRCH) => Ok((true, None)),
                        Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                            detail: format!("permission denied signaling process {}", identity.pid),
                            source: None,
                        }),
                        Err(err) => Err(OsError::Platform {
                            detail: format!("failed signaling process {}: {err}", identity.pid),
                        }),
                    }
                }
            }
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_process worker task failed: {err}"),
        })??;

    #[cfg(not(target_os = "linux"))]
    let already_gone = tokio::task::spawn_blocking(move || -> Result<bool> {
        if !is_alive(&identity) {
            return Ok(true);
        }
        match kill(nix_pid, initial_sig) {
            Ok(()) => Ok(false),
            // On macOS, `kill` can return `ESRCH` while a process is in `P_WEXIT` before its
            // descriptors are closed; settle until `is_alive` reports false too.
            Err(Errno::ESRCH) => Ok(!is_alive(&identity)),
            Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                detail: format!("permission denied signaling process {}", identity.pid),
                source: None,
            }),
            Err(err) => Err(OsError::Platform {
                detail: format!("failed signaling process {}: {err}", identity.pid),
            }),
        }
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_process worker task failed: {err}"),
    })??;

    if already_gone {
        return Ok(true);
    }

    if !force {
        let grace_deadline = Instant::now() + grace;
        loop {
            let alive = tokio::task::spawn_blocking(move || is_alive(&identity))
                .await
                .map_err(|err| OsError::Platform {
                    detail: format!("kill_process poll task failed: {err}"),
                })?;
            if !alive {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= grace_deadline {
                break;
            }
            let remaining = grace_deadline.saturating_duration_since(now);
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }

        let gone_on_escalate = tokio::task::spawn_blocking(move || -> Result<bool> {
            #[cfg(target_os = "linux")]
            if let Some(pidfd) = pinned_pidfd {
                return match pidfd.send_signal(Signal::SIGKILL) {
                    Ok(()) => Ok(false),
                    Err(Errno::ESRCH) => Ok(true),
                    Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                        detail: format!("permission denied escalating process {}", identity.pid),
                        source: None,
                    }),
                    Err(err) => Err(OsError::Platform {
                        detail: format!(
                            "failed sending SIGKILL to process {}: {err}",
                            identity.pid
                        ),
                    }),
                };
            }

            match super::of_pid(identity.pid)? {
                Some(observed) if identity.matches(&observed) => {}
                _ => return Ok(true),
            }
            match kill(nix_pid, Signal::SIGKILL) {
                Ok(()) => Ok(false),
                Err(Errno::ESRCH) => Ok(!is_alive(&identity)),
                Err(Errno::EPERM) => Err(OsError::PermissionDenied {
                    detail: format!("permission denied escalating process {}", identity.pid),
                    source: None,
                }),
                Err(err) => Err(OsError::Platform {
                    detail: format!("failed sending SIGKILL to process {}: {err}", identity.pid),
                }),
            }
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_process SIGKILL task failed: {err}"),
        })??;

        if gone_on_escalate {
            return Ok(true);
        }
    }

    let settle_deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let alive = tokio::task::spawn_blocking(move || is_alive(&identity))
            .await
            .map_err(|err| OsError::Platform {
                detail: format!("kill_process settle task failed: {err}"),
            })?;
        if !alive {
            return Ok(true);
        }
        let now = Instant::now();
        if now >= settle_deadline {
            return Ok(false);
        }
        let remaining = settle_deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

pub(super) async fn kill_tree(
    identity: &ProcessId,
    marker: Option<&str>,
    grace: Duration,
) -> Result<KillTreeReport> {
    let raw_pid = i32::try_from(identity.pid).map_err(|_| OsError::InvalidArgument {
        detail: format!("pid {} exceeds Unix pid_t range", identity.pid),
    })?;
    let root_nix_pid = Pid::from_raw(raw_pid);
    let identity = *identity;
    let marker_owned = marker.map(str::to_owned);
    let own_pid = std::process::id();

    let marker_phase1 = marker_owned.clone();

    #[cfg(target_os = "linux")]
    type RootPin = Option<Arc<linux_pidfd::PidFd>>;
    #[cfg(not(target_os = "linux"))]
    type RootPin = Option<()>;

    // Phase 1: check root identity (pinning via `pidfd_open` on Linux), collect verified targets
    // and any unverifiable orphan candidates off the async executor, then send SIGTERM ONLY to
    // verified processes.
    let (mut tracked, mut unverified, target_pgid, root_pidfd): (
        HashMap<u32, ProcessId>,
        HashMap<u32, ProcessId>,
        Option<Pid>,
        RootPin,
    ) = tokio::task::spawn_blocking(move || -> Result<_> {
        #[cfg(target_os = "linux")]
        let (root_alive, root_recycled, root_pidfd) = match linux_pidfd::open_verified(&identity) {
            linux_pidfd::VerifiedOpen::Pinned(fd) => (true, false, Some(Arc::new(fd))),
            linux_pidfd::VerifiedOpen::NotAProcess => {
                return Err(OsError::InvalidArgument {
                    detail: format!("id {} is a thread, not a process", identity.pid),
                });
            }
            linux_pidfd::VerifiedOpen::Refused(Errno::EPERM | Errno::EACCES) => {
                return Err(OsError::PermissionDenied {
                    detail: format!("permission denied opening process {}", identity.pid),
                    source: None,
                });
            }
            linux_pidfd::VerifiedOpen::Refused(err) => {
                return Err(OsError::Platform {
                    detail: format!("pidfd_open failed for process {}: {err}", identity.pid),
                });
            }
            linux_pidfd::VerifiedOpen::GoneOrMismatch => match super::of_pid(identity.pid) {
                Ok(Some(_)) => (false, true, None),
                Ok(None) => (false, false, None),
                Err(OsError::InvalidArgument { detail }) => {
                    return Err(OsError::InvalidArgument { detail });
                }
                Err(_) => (false, true, None),
            },
            linux_pidfd::VerifiedOpen::Unsupported => match super::of_pid(identity.pid) {
                Ok(Some(observed)) if identity.matches(&observed) => (true, false, None),
                Ok(Some(_)) => (false, true, None),
                Ok(None) => (false, false, None),
                Err(OsError::InvalidArgument { detail }) => {
                    return Err(OsError::InvalidArgument { detail });
                }
                Err(_) => (false, true, None),
            },
        };
        #[cfg(not(target_os = "linux"))]
        let (root_alive, root_recycled, root_pidfd) = match super::of_pid(identity.pid) {
            Ok(Some(observed)) if identity.matches(&observed) => (true, false, None),
            Ok(Some(_)) | Err(_) => (false, true, None),
            Ok(None) => (false, false, None),
        };

        let own_pgid = getpgrp();
        let own_sid = getsid(None).ok();
        let allow_group = root_nix_pid != own_pgid;
        let allow_session = Some(root_nix_pid) != own_sid;

        let mut tracked: HashMap<u32, ProcessId> = HashMap::new();
        let mut unverified: HashMap<u32, ProcessId> = HashMap::new();

        if root_alive {
            tracked.insert(identity.pid, identity);
            let live_leader = allow_group && getpgid(Some(root_nix_pid)).ok() == Some(root_nix_pid);
            collect_live_tree_into(&identity, root_nix_pid, live_leader, own_pid, &mut tracked);
            // When a detach marker is present, also capture any `setsid` grandchild that escaped
            // the root's process group or was reparented while `root` is still alive.
            if let Some(ref marker_val) = marker_phase1 {
                collect_marked_orphans_into(
                    &identity,
                    root_nix_pid,
                    allow_group,
                    allow_session,
                    false,
                    marker_val,
                    own_pid,
                    &mut tracked,
                    &mut unverified,
                );
            }

            let target_pgid = if live_leader {
                Some(root_nix_pid)
            } else {
                None
            };

            // Only signal the process group as a whole while `root` itself is still verified alive
            // (so `root.pid` cannot have been recycled into an unrelated group).
            if let Some(pgid) = target_pgid
                && is_alive(&identity)
            {
                let _ = killpg(pgid, Signal::SIGTERM);
            }
            #[cfg(target_os = "linux")]
            let skip_pid = if let Some(ref pidfd) = root_pidfd {
                let _ = pidfd.send_signal(Signal::SIGTERM);
                Some(identity.pid)
            } else {
                None
            };
            #[cfg(not(target_os = "linux"))]
            let skip_pid = None;
            signal_tracked(&tracked, Signal::SIGTERM, own_pid, skip_pid);
            return Ok((tracked, unverified, target_pgid, root_pidfd));
        }

        // Root is already gone. Never kill by session/group alone: require the detach environment
        // marker to prove ownership, and report unverifiable candidates without signaling them.
        match marker_phase1.as_deref() {
            Some(marker_val) => {
                collect_marked_orphans_into(
                    &identity,
                    root_nix_pid,
                    allow_group,
                    allow_session,
                    root_recycled,
                    marker_val,
                    own_pid,
                    &mut tracked,
                    &mut unverified,
                );
                if !tracked.is_empty() {
                    signal_tracked(&tracked, Signal::SIGTERM, own_pid, None);
                }
            }
            None => {
                if !root_recycled {
                    collect_unverified_candidates_into(
                        &identity,
                        root_nix_pid,
                        allow_group,
                        allow_session,
                        own_pid,
                        &mut unverified,
                    );
                }
            }
        }

        Ok((tracked, unverified, None, None))
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_tree worker task failed: {err}"),
    })??;

    if tracked.is_empty() {
        let unverified_survivors = filter_live_sorted(unverified, &tracked).await?;
        return Ok(KillTreeReport {
            all_gone: unverified_survivors.is_empty(),
            survivors: Vec::new(),
            unverified_survivors,
        });
    }

    // Poll only the already-tracked PIDs (plus killpg(0) while root itself is still alive) off the
    // async executor.
    let grace_deadline = Instant::now() + grace;
    loop {
        let tracked_snapshot: Vec<ProcessId> = tracked.values().copied().collect();
        let gone = tokio::task::spawn_blocking(move || {
            tracked_is_gone(&identity, target_pgid, &tracked_snapshot)
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_tree poll task failed: {err}"),
        })?;
        if gone {
            let unverified_survivors = filter_live_sorted(unverified, &tracked).await?;
            return Ok(KillTreeReport {
                all_gone: unverified_survivors.is_empty(),
                survivors: Vec::new(),
                unverified_survivors,
            });
        }
        let now = Instant::now();
        if now >= grace_deadline {
            break;
        }
        let remaining = grace_deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }

    // Phase 2: discover any children spawned during grace by live tracked processes (plus any new
    // marker-verified orphans when a marker is present), then escalate survivors to SIGKILL.
    let marker_phase2 = marker_owned;
    let (tracked, unverified) = tokio::task::spawn_blocking(move || {
        let root_still_alive = is_alive(&identity);
        let root_recycled = !root_still_alive && matches!(super::of_pid(identity.pid), Ok(Some(_)));
        let own_pgid = getpgrp();
        let own_sid = getsid(None).ok();
        let allow_group = root_nix_pid != own_pgid;
        let allow_session = Some(root_nix_pid) != own_sid;

        if root_still_alive {
            let live_leader = allow_group && getpgid(Some(root_nix_pid)).ok() == Some(root_nix_pid);
            collect_live_tree_into(&identity, root_nix_pid, live_leader, own_pid, &mut tracked);
            if let Some(ref marker_val) = marker_phase2 {
                collect_marked_orphans_into(
                    &identity,
                    root_nix_pid,
                    allow_group,
                    allow_session,
                    false,
                    marker_val,
                    own_pid,
                    &mut tracked,
                    &mut unverified,
                );
            }
        } else {
            collect_descendants_of_tracked_into(&identity, own_pid, &mut tracked);
            if let Some(ref marker_val) = marker_phase2 {
                collect_marked_orphans_into(
                    &identity,
                    root_nix_pid,
                    allow_group,
                    allow_session,
                    root_recycled,
                    marker_val,
                    own_pid,
                    &mut tracked,
                    &mut unverified,
                );
            }
        }

        if let Some(pgid) = target_pgid
            && is_alive(&identity)
        {
            let _ = killpg(pgid, Signal::SIGKILL);
        }
        #[cfg(target_os = "linux")]
        let skip_pid = if let Some(ref pidfd) = root_pidfd {
            let _ = pidfd.send_signal(Signal::SIGKILL);
            Some(identity.pid)
        } else {
            None
        };
        #[cfg(not(target_os = "linux"))]
        let skip_pid = {
            let _ = &root_pidfd;
            None
        };
        signal_tracked(&tracked, Signal::SIGKILL, own_pid, skip_pid);
        (tracked, unverified)
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_tree SIGKILL task failed: {err}"),
    })?;

    let settle_deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        let tracked_snapshot: Vec<ProcessId> = tracked.values().copied().collect();
        let gone = tokio::task::spawn_blocking(move || {
            tracked_is_gone(&identity, target_pgid, &tracked_snapshot)
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_tree settle task failed: {err}"),
        })?;
        if gone {
            let unverified_survivors = filter_live_sorted(unverified, &tracked).await?;
            return Ok(KillTreeReport {
                all_gone: unverified_survivors.is_empty(),
                survivors: Vec::new(),
                unverified_survivors,
            });
        }
        let now = Instant::now();
        if now >= settle_deadline {
            break;
        }
        let remaining = settle_deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }

    let tracked_snapshot: Vec<ProcessId> = tracked.values().copied().collect();
    let unverified_snapshot: Vec<ProcessId> = unverified
        .into_iter()
        .filter(|(pid, _)| !tracked.contains_key(pid))
        .map(|(_, id)| id)
        .collect();
    let (survivors, unverified_survivors) = tokio::task::spawn_blocking(move || {
        let mut left: Vec<ProcessId> = tracked_snapshot.into_iter().filter(is_alive).collect();
        left.sort_by_key(|id| id.pid);
        let mut unv: Vec<ProcessId> = unverified_snapshot.into_iter().filter(is_alive).collect();
        unv.sort_by_key(|id| id.pid);
        (left, unv)
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_tree survivor check failed: {err}"),
    })?;

    Ok(KillTreeReport {
        all_gone: survivors.is_empty() && unverified_survivors.is_empty(),
        survivors,
        unverified_survivors,
    })
}

async fn filter_live_sorted(
    unverified: HashMap<u32, ProcessId>,
    tracked: &HashMap<u32, ProcessId>,
) -> Result<Vec<ProcessId>> {
    if unverified.is_empty() {
        return Ok(Vec::new());
    }
    let candidates: Vec<ProcessId> = unverified
        .into_iter()
        .filter(|(pid, _)| !tracked.contains_key(pid))
        .map(|(_, id)| id)
        .collect();
    tokio::task::spawn_blocking(move || {
        let mut live: Vec<ProcessId> = candidates.into_iter().filter(is_alive).collect();
        live.sort_by_key(|id| id.pid);
        live
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_tree unverified survivor check failed: {err}"),
    })
}

fn tracked_is_gone(identity: &ProcessId, target_pgid: Option<Pid>, tracked: &[ProcessId]) -> bool {
    if tracked.iter().any(is_alive) {
        return false;
    }
    if let Some(pgid) = target_pgid
        && is_alive(identity)
    {
        match killpg(pgid, None) {
            Err(Errno::ESRCH) => {}
            _ => return false,
        }
    }
    true
}

fn signal_tracked(
    tracked: &HashMap<u32, ProcessId>,
    sig: Signal,
    own_pid: u32,
    already_signaled_pid: Option<u32>,
) {
    for proc_id in tracked.values() {
        if proc_id.pid == own_pid || already_signaled_pid == Some(proc_id.pid) {
            continue;
        }
        let _ = signal_verified_once(proc_id, sig);
    }
}

/// Discover live descendants of an alive `root` (and of any live already-tracked processes), plus
/// live members of `root`'s process group while `root` itself is still observed alive as its group
/// leader.
fn collect_live_tree_into(
    root: &ProcessId,
    root_nix_pid: Pid,
    include_live_group: bool,
    own_pid: u32,
    tracked: &mut HashMap<u32, ProcessId>,
) {
    let Ok(mut system) = System::new() else {
        return;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let (children_by_parent, start_by_pid) = build_sysinfo_maps(&system);
    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<(u32, u64)> = VecDeque::new();

    if include_live_group && is_alive(root) {
        for (sys_pid, proc_info) in system.processes() {
            if matches!(
                proc_info.status(),
                ProcessStatus::Zombie | ProcessStatus::Dead
            ) {
                continue;
            }
            let pid = sys_pid.as_u32();
            if pid == 0 || pid == own_pid || pid == root.pid {
                continue;
            }
            let Ok(raw) = i32::try_from(pid) else {
                continue;
            };
            if getpgid(Some(Pid::from_raw(raw))).ok() == Some(root_nix_pid)
                && let Ok(Some(id)) = super::of_pid(pid)
                && child_not_older_than_parent(
                    id.start_time,
                    root.start_time,
                    EXACT_START_BACKWARD_TOLERANCE,
                )
            {
                tracked.entry(pid).or_insert(id);
                if visited.insert(pid) {
                    queue.push_back((pid, proc_info.start_time()));
                }
            }
        }
    }

    if is_alive(root) && visited.insert(root.pid) {
        let root_sys_start = start_by_pid.get(&root.pid).copied().unwrap_or(0);
        queue.push_back((root.pid, root_sys_start));
    }

    seed_live_tracked(tracked, own_pid, &start_by_pid, &mut visited, &mut queue);
    bfs_descendants(
        root,
        own_pid,
        &children_by_parent,
        &mut visited,
        &mut queue,
        tracked,
    );
}

/// Walk descendants of already-verified live tracked processes without trusting any exited root
/// session or process group.
fn collect_descendants_of_tracked_into(
    root: &ProcessId,
    own_pid: u32,
    tracked: &mut HashMap<u32, ProcessId>,
) {
    let Ok(mut system) = System::new() else {
        return;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let (children_by_parent, start_by_pid) = build_sysinfo_maps(&system);
    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<(u32, u64)> = VecDeque::new();
    seed_live_tracked(tracked, own_pid, &start_by_pid, &mut visited, &mut queue);
    bfs_descendants(
        root,
        own_pid,
        &children_by_parent,
        &mut visited,
        &mut queue,
        tracked,
    );
}

/// Collect candidate orphan processes sharing an exited root's process group or session WITHOUT
/// signaling them (used when no detach marker is available).
fn collect_unverified_candidates_into(
    root: &ProcessId,
    root_nix_pid: Pid,
    allow_group: bool,
    allow_session: bool,
    own_pid: u32,
    unverified: &mut HashMap<u32, ProcessId>,
) {
    if !allow_group && !allow_session {
        return;
    }
    let Ok(mut system) = System::new() else {
        return;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    for (sys_pid, proc_info) in system.processes() {
        if matches!(
            proc_info.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) {
            continue;
        }
        let pid = sys_pid.as_u32();
        if pid == 0 || pid == own_pid || pid == root.pid {
            continue;
        }
        if is_session_or_group_candidate(pid, root, root_nix_pid, allow_group, allow_session)
            && let Ok(Some(id)) = super::of_pid(pid)
            && child_not_older_than_parent(
                id.start_time,
                root.start_time,
                EXACT_START_BACKWARD_TOLERANCE,
            )
        {
            unverified.entry(pid).or_insert(id);
        }
    }
}

/// Once the root process has exited, discover surviving orphans proved by `LEGION_DETACH_ID=<marker>`
/// (and their live descendants) into `tracked`, while recording any session/group candidates whose
/// environment cannot be read into `unverified` without signaling them.
#[allow(clippy::too_many_arguments)]
fn collect_marked_orphans_into(
    root: &ProcessId,
    root_nix_pid: Pid,
    allow_group: bool,
    allow_session: bool,
    root_recycled: bool,
    marker: &str,
    own_pid: u32,
    tracked: &mut HashMap<u32, ProcessId>,
    unverified: &mut HashMap<u32, ProcessId>,
) {
    let Ok(mut system) = System::new() else {
        return;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let (children_by_parent, start_by_pid) = build_sysinfo_maps(&system);
    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<(u32, u64)> = VecDeque::new();

    for (sys_pid, proc_info) in system.processes() {
        if matches!(
            proc_info.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) {
            continue;
        }
        let pid = sys_pid.as_u32();
        if pid == 0 || pid == own_pid || tracked.contains_key(&pid) {
            continue;
        }
        match check_env_marker(pid, crate::detach::DETACH_MARKER_ENV, marker) {
            EnvMarkerCheck::Matched => {
                if let Ok(Some(id)) = super::of_pid(pid) {
                    tracked.insert(pid, id);
                    unverified.remove(&pid);
                    if visited.insert(pid) {
                        queue.push_back((pid, proc_info.start_time()));
                    }
                }
            }
            EnvMarkerCheck::NotMatched => {}
            EnvMarkerCheck::Unverifiable => {
                if !root_recycled
                    && pid != root.pid
                    && is_session_or_group_candidate(
                        pid,
                        root,
                        root_nix_pid,
                        allow_group,
                        allow_session,
                    )
                    && let Ok(Some(id)) = super::of_pid(pid)
                    && child_not_older_than_parent(
                        id.start_time,
                        root.start_time,
                        EXACT_START_BACKWARD_TOLERANCE,
                    )
                {
                    unverified.entry(pid).or_insert(id);
                }
            }
        }
    }

    seed_live_tracked(tracked, own_pid, &start_by_pid, &mut visited, &mut queue);
    bfs_descendants(
        root,
        own_pid,
        &children_by_parent,
        &mut visited,
        &mut queue,
        tracked,
    );
}

fn is_session_or_group_candidate(
    pid: u32,
    root: &ProcessId,
    root_nix_pid: Pid,
    allow_group: bool,
    allow_session: bool,
) -> bool {
    let _ = root;
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    let nix_p = Pid::from_raw(raw);
    let in_group = allow_group && getpgid(Some(nix_p)).ok() == Some(root_nix_pid);
    let in_session = allow_session && getsid(Some(nix_p)).ok() == Some(root_nix_pid);
    in_group || in_session
}

#[allow(clippy::type_complexity)]
fn build_sysinfo_maps(system: &System) -> (HashMap<u32, Vec<(u32, u64)>>, HashMap<u32, u64>) {
    let mut children_by_parent: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    let mut start_by_pid: HashMap<u32, u64> = HashMap::new();
    for (sys_pid, proc_info) in system.processes() {
        if matches!(
            proc_info.status(),
            ProcessStatus::Zombie | ProcessStatus::Dead
        ) {
            continue;
        }
        let pid = sys_pid.as_u32();
        let started = proc_info.start_time();
        start_by_pid.insert(pid, started);
        if let Some(parent) = proc_info.parent() {
            children_by_parent
                .entry(parent.as_u32())
                .or_default()
                .push((pid, started));
        }
    }
    (children_by_parent, start_by_pid)
}

fn seed_live_tracked(
    tracked: &HashMap<u32, ProcessId>,
    own_pid: u32,
    start_by_pid: &HashMap<u32, u64>,
    visited: &mut HashSet<u32>,
    queue: &mut VecDeque<(u32, u64)>,
) {
    for (&pid, proc_id) in tracked {
        if pid != own_pid && is_alive(proc_id) && visited.insert(pid) {
            let sys_start = start_by_pid.get(&pid).copied().unwrap_or(0);
            queue.push_back((pid, sys_start));
        }
    }
}

fn bfs_descendants(
    root: &ProcessId,
    own_pid: u32,
    children_by_parent: &HashMap<u32, Vec<(u32, u64)>>,
    visited: &mut HashSet<u32>,
    queue: &mut VecDeque<(u32, u64)>,
    tracked: &mut HashMap<u32, ProcessId>,
) {
    while let Some((parent_pid, parent_start)) = queue.pop_front() {
        if let Some(children) = children_by_parent.get(&parent_pid) {
            for &(child_pid, child_start) in children {
                if child_pid == own_pid || !visited.insert(child_pid) {
                    continue;
                }
                if parent_start > 0
                    && !child_not_older_than_parent(
                        child_start,
                        parent_start,
                        SYSINFO_START_BACKWARD_TOLERANCE_SECS,
                    )
                {
                    continue;
                }
                if !tracked.contains_key(&child_pid)
                    && let Ok(Some(child_id)) = super::of_pid(child_pid)
                {
                    if !child_not_older_than_parent(
                        child_id.start_time,
                        root.start_time,
                        EXACT_START_BACKWARD_TOLERANCE,
                    ) {
                        continue;
                    }
                    tracked.insert(child_pid, child_id);
                }
                queue.push_back((child_pid, child_start));
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[allow(unsafe_code, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn linux_pidfd_verifies_identity_and_rejects_mismatch_or_absent() {
        let me = crate::procid::current().unwrap();
        match linux_pidfd::open_verified(&me) {
            linux_pidfd::VerifiedOpen::Pinned(pidfd) => {
                // Signal 0 validates the pidfd without delivering a signal.
                assert!(pidfd.send_raw(0).is_ok());
            }
            linux_pidfd::VerifiedOpen::Unsupported => {
                // Permitted only on kernels where `pidfd_open` returns ENOSYS.
            }
            linux_pidfd::VerifiedOpen::GoneOrMismatch
            | linux_pidfd::VerifiedOpen::NotAProcess
            | linux_pidfd::VerifiedOpen::Refused(_) => {
                panic!("pidfd_open unexpectedly rejected current process {me}");
            }
        }

        let mismatched = ProcessId::new(me.pid, me.start_time.wrapping_add(1));
        assert!(matches!(
            linux_pidfd::open_verified(&mismatched),
            linux_pidfd::VerifiedOpen::GoneOrMismatch | linux_pidfd::VerifiedOpen::Unsupported
        ));

        let absent = ProcessId::new(i32::MAX as u32, 1);
        assert!(matches!(
            linux_pidfd::open_verified(&absent),
            linux_pidfd::VerifiedOpen::GoneOrMismatch | linux_pidfd::VerifiedOpen::Unsupported
        ));
    }

    #[tokio::test]
    async fn linux_refuses_non_leader_thread_id_of_current_process() {
        let (tid_tx, tid_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            // SAFETY: `SYS_gettid` takes no arguments and returns the calling thread's kernel TID.
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
            let _ = tid_tx.send(tid);
            let _ = done_rx.recv();
        });
        let tid = tid_rx.recv().unwrap();
        assert_ne!(
            tid,
            std::process::id(),
            "worker thread must have distinct TID"
        );

        // `of_pid` refuses the thread ID as InvalidArgument, not None.
        assert!(matches!(
            crate::procid::of_pid(tid),
            Err(OsError::InvalidArgument { .. })
        ));

        // Even if a caller fabricates a ProcessId with the thread's start_ticks from /proc/<tid>/stat,
        // `pidfd_open`, `signal_process`, `kill_process`, and `kill_tree` all refuse it without
        // signaling our own process.
        let stat = std::fs::read_to_string(format!("/proc/{tid}/stat")).unwrap();
        let (_, start_ticks) = crate::procid::parse_linux_proc_stat(&stat).unwrap();
        let forged = ProcessId::new(tid, start_ticks);

        assert!(matches!(
            linux_pidfd::open_verified(&forged),
            linux_pidfd::VerifiedOpen::NotAProcess | linux_pidfd::VerifiedOpen::Unsupported
        ));
        assert!(matches!(
            crate::procid::signal_process(&forged, false).await,
            Err(OsError::InvalidArgument { .. })
        ));
        assert!(matches!(
            crate::procid::kill_process(&forged, false, Duration::from_millis(50)).await,
            Err(OsError::InvalidArgument { .. })
        ));
        assert!(matches!(
            crate::procid::kill_tree(&forged, Duration::from_millis(50)).await,
            Err(OsError::InvalidArgument { .. })
        ));

        let _ = done_tx.send(());
        worker.join().unwrap();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    #[test]
    fn macos_bsd_info_and_kinfo_match_for_current_process() {
        let pid = std::process::id();
        let raw_pid = i32::try_from(pid).unwrap();
        let bsd = macos_proc::query_bsd_info(raw_pid).expect("proc_pidinfo failed for self");
        let kinfo = macos_proc::query_kinfo(raw_pid).expect("sysctl kinfo_proc failed for self");
        assert_eq!(
            bsd.start_us, kinfo.start_us,
            "proc_pidinfo and kinfo_proc derived different start times"
        );
        assert_ne!(bsd.status, crate::procid::MACOS_SZOMB);
        assert_ne!(kinfo.status, crate::procid::MACOS_SZOMB);

        assert_eq!(macos_proc::query_kinfo(i32::MAX), None);
    }
}
