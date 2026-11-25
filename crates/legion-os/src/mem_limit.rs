//! Effective memory limit detection (cgroup v2/v1, systemd `MemoryMax`, Windows Job Object, and
//! host RAM) plus fast per-process resident memory (RSS) sampling for watchdogs.

use std::time::Duration;

use sysinfo::{MemoryRefreshKind, RefreshKind, System};

/// Sentinel threshold above which cgroup v1 `memory.limit_in_bytes` means "unlimited"
/// (kernels report `PAGE_COUNTER_MAX` ~ `9.22e18` bytes).
#[cfg(any(target_os = "linux", test))]
const CGROUP_UNLIMITED_THRESHOLD: u64 = 1u64 << 60;

/// Returns the effective memory available to this process in bytes: the minimum of the host's
/// physical RAM and any active container or service limit (Linux cgroup v2 `memory.max` — which
/// systemd `MemoryMax=` sets — or cgroup v1 `memory.limit_in_bytes`, or a Windows Job Object
/// memory limit).
pub fn effective_memory_bytes() -> u64 {
    let host_ram = host_total_memory_bytes();
    let container_limit = container_or_job_memory_limit();
    match (host_ram, container_limit) {
        (0, Some(limit)) => limit,
        (ram, Some(limit)) => ram.min(limit),
        (ram, None) => ram,
    }
}

/// Samples the resident memory (RSS) in bytes of process `pid`, returning `None` if the process
/// has exited or cannot be queried.
pub fn process_rss_bytes(pid: u32) -> Option<u64> {
    imp::process_rss_bytes(pid)
}

/// Samples the cumulative CPU time (user + kernel) consumed by process `pid`, returning `None` if
/// the process has exited or cannot be queried.
pub fn process_cpu_time(pid: u32) -> Option<Duration> {
    imp::process_cpu_time(pid)
}

fn host_total_memory_bytes() -> u64 {
    System::new_with_specifics(
        RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
    )
    .map(|sys| sys.total_memory())
    .unwrap_or(0)
}

fn container_or_job_memory_limit() -> Option<u64> {
    imp::container_or_job_memory_limit()
}

/// Parses a cgroup v2 `memory.max` file (`"max"` or decimal byte count).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_cgroup_v2_memory_max(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "max" {
        return None;
    }
    let bytes: u64 = trimmed.parse().ok()?;
    (bytes > 0 && bytes < CGROUP_UNLIMITED_THRESHOLD).then_some(bytes)
}

/// Parses a cgroup v1 `memory.limit_in_bytes` file, ignoring kernel "unlimited" sentinels.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_cgroup_v1_limit(text: &str) -> Option<u64> {
    let bytes: u64 = text.trim().parse().ok()?;
    (bytes > 0 && bytes < CGROUP_UNLIMITED_THRESHOLD).then_some(bytes)
}

/// Extracts the cgroup v2 unified hierarchy relative path from `/proc/self/cgroup` content
/// (the `0::/<path>` line).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_proc_cgroup_v2_path(text: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let id = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if id == "0" && controllers.is_empty() {
            return Some(path.trim().to_owned());
        }
    }
    None
}

/// Extracts the cgroup v1 `memory` controller relative path from `/proc/self/cgroup` content.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_proc_cgroup_v1_memory_path(text: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.splitn(3, ':');
        let _id = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if controllers.split(',').any(|c| c == "memory") {
            return Some(path.trim().to_owned());
        }
    }
    None
}

#[cfg(target_os = "linux")]
mod imp {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use super::{
        parse_cgroup_v1_limit, parse_cgroup_v2_memory_max, parse_proc_cgroup_v1_memory_path,
        parse_proc_cgroup_v2_path,
    };

    fn page_size() -> u64 {
        static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
        *PAGE_SIZE.get_or_init(|| {
            // SAFETY: `sysconf(_SC_PAGESIZE)` is thread-safe and has no preconditions.
            #[allow(unsafe_code)]
            let raw = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            u64::try_from(raw).ok().filter(|&v| v > 0).unwrap_or(4096)
        })
    }

    /// Reads resident set size directly from `/proc/<pid>/statm` (fast, zero process scan).
    pub(super) fn process_rss_bytes(pid: u32) -> Option<u64> {
        let text = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident_pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
        Some(resident_pages.saturating_mul(page_size()))
    }

    fn clock_ticks_per_sec() -> u64 {
        static TICKS: OnceLock<u64> = OnceLock::new();
        *TICKS.get_or_init(|| {
            // SAFETY: `sysconf(_SC_CLK_TCK)` is thread-safe and has no preconditions.
            #[allow(unsafe_code)]
            let raw = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            u64::try_from(raw).ok().filter(|&v| v > 0).unwrap_or(100)
        })
    }

    /// Reads total user + kernel CPU time directly from `/proc/<pid>/stat`.
    pub(super) fn process_cpu_time(pid: u32) -> Option<std::time::Duration> {
        let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = text.rsplit_once(')')?.1;
        let mut fields = after_comm.split_whitespace();
        // Fields after `)` start at index 0 = state (field 3); utime is field 14 (idx 11), stime is field 15 (idx 12).
        let utime: u64 = fields.nth(11)?.parse().ok()?;
        let stime: u64 = fields.next()?.parse().ok()?;
        let total_ticks = utime.saturating_add(stime);
        let tps = clock_ticks_per_sec();
        let nanos = u128::from(total_ticks).saturating_mul(1_000_000_000) / u128::from(tps);
        Some(std::time::Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }

    pub(super) fn container_or_job_memory_limit() -> Option<u64> {
        let proc_cg = fs::read_to_string("/proc/self/cgroup").ok()?;
        let mut best: Option<u64> = None;

        // Cgroup v2 (unified hierarchy, including systemd `MemoryMax=` and Docker/Podman v2).
        // Walk from the process's cgroup up to `/sys/fs/cgroup` and take the tightest `memory.max`.
        if let Some(rel) = parse_proc_cgroup_v2_path(&proc_cg) {
            let root = Path::new("/sys/fs/cgroup");
            let mut cur = join_cgroup_rel(root, &rel);
            loop {
                if let Ok(text) = fs::read_to_string(cur.join("memory.max"))
                    && let Some(limit) = parse_cgroup_v2_memory_max(&text)
                {
                    best = Some(best.map_or(limit, |prev| prev.min(limit)));
                }
                if cur == root {
                    break;
                }
                match cur.parent() {
                    Some(parent) if parent.starts_with(root) => cur = parent.to_path_buf(),
                    _ => break,
                }
            }
        }

        // Cgroup v1 fallback (`/sys/fs/cgroup/memory/.../memory.limit_in_bytes`).
        if let Some(rel) = parse_proc_cgroup_v1_memory_path(&proc_cg) {
            let root = Path::new("/sys/fs/cgroup/memory");
            if root.is_dir() {
                let mut cur = join_cgroup_rel(root, &rel);
                loop {
                    if let Ok(text) = fs::read_to_string(cur.join("memory.limit_in_bytes"))
                        && let Some(limit) = parse_cgroup_v1_limit(&text)
                    {
                        best = Some(best.map_or(limit, |prev| prev.min(limit)));
                    }
                    if cur == root {
                        break;
                    }
                    match cur.parent() {
                        Some(parent) if parent.starts_with(root) => cur = parent.to_path_buf(),
                        _ => break,
                    }
                }
            }
        }

        best
    }

    fn join_cgroup_rel(root: &Path, rel: &str) -> PathBuf {
        let stripped = rel.trim_start_matches('/');
        if stripped.is_empty() {
            root.to_path_buf()
        } else {
            root.join(stripped)
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
mod imp {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

    pub(super) fn process_rss_bytes(pid: u32) -> Option<u64> {
        let mut sys = System::new().ok()?;
        let spid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        sys.process(spid).map(|p| p.memory())
    }

    pub(super) fn process_cpu_time(pid: u32) -> Option<std::time::Duration> {
        let mut sys = System::new().ok()?;
        let spid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        sys.process(spid)
            .map(|p| std::time::Duration::from_millis(p.accumulated_cpu_time()))
    }

    pub(super) fn container_or_job_memory_limit() -> Option<u64> {
        None
    }
}

#[cfg(windows)]
mod imp {
    use std::mem::size_of;
    use std::time::Duration;

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    pub(super) fn process_rss_bytes(pid: u32) -> Option<u64> {
        let mut sys = System::new().ok()?;
        let spid = Pid::from_u32(pid);
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[spid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        sys.process(spid).map(|p| p.memory())
    }

    pub(super) fn process_cpu_time(pid: u32) -> Option<Duration> {
        // SAFETY: opens the PID with read-only limited query access and closes the handle before returning.
        #[allow(unsafe_code)]
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: `handle` is valid and all four `FILETIME` pointers refer to initialized locals.
        #[allow(unsafe_code)]
        let ok = unsafe {
            GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user).is_ok()
        };
        // SAFETY: `handle` is owned by this function.
        #[allow(unsafe_code)]
        unsafe {
            let _ = CloseHandle(handle);
        }
        if !ok {
            return None;
        }
        let k = (u64::from(kernel.dwHighDateTime) << 32) | u64::from(kernel.dwLowDateTime);
        let u = (u64::from(user.dwHighDateTime) << 32) | u64::from(user.dwLowDateTime);
        let ticks_100ns = k.saturating_add(u);
        Some(Duration::from_nanos(ticks_100ns.saturating_mul(100)))
    }

    pub(super) fn container_or_job_memory_limit() -> Option<u64> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: passing `None` queries the current process's Job Object (if any) into a properly
        // aligned output buffer whose byte length matches `size_of`.
        #[allow(unsafe_code)]
        let ok = unsafe {
            QueryInformationJobObject(
                None,
                JobObjectExtendedLimitInformation,
                (&raw mut info).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>()).unwrap_or(0),
                None,
            )
        }
        .is_ok();
        if !ok {
            return None;
        }
        let flags = info.BasicLimitInformation.LimitFlags;
        let mut best: Option<u64> = None;
        if (flags & JOB_OBJECT_LIMIT_JOB_MEMORY) != Default::default() && info.JobMemoryLimit > 0 {
            let v = info.JobMemoryLimit as u64;
            best = Some(best.map_or(v, |p| p.min(v)));
        }
        if (flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY) != Default::default()
            && info.ProcessMemoryLimit > 0
        {
            let v = info.ProcessMemoryLimit as u64;
            best = Some(best.map_or(v, |p| p.min(v)));
        }
        best
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub(super) fn process_rss_bytes(_pid: u32) -> Option<u64> {
        None
    }

    pub(super) fn process_cpu_time(_pid: u32) -> Option<std::time::Duration> {
        None
    }

    pub(super) fn container_or_job_memory_limit() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_parsers_handle_max_sentinels_and_finite_limits() {
        assert_eq!(parse_cgroup_v2_memory_max("max\n"), None);
        assert_eq!(parse_cgroup_v2_memory_max("536870912\n"), Some(536_870_912));
        assert_eq!(parse_cgroup_v1_limit("9223372036854771712\n"), None);
        assert_eq!(parse_cgroup_v1_limit("1073741824\n"), Some(1_073_741_824));

        let proc_v2 = "0::/user.slice/user-1000.slice/session-1.scope\n";
        assert_eq!(
            parse_proc_cgroup_v2_path(proc_v2).as_deref(),
            Some("/user.slice/user-1000.slice/session-1.scope")
        );
        let proc_v1 = "8:memory:/docker/abc123\n4:cpu,cpuacct:/docker/abc123\n";
        assert_eq!(
            parse_proc_cgroup_v1_memory_path(proc_v1).as_deref(),
            Some("/docker/abc123")
        );
    }

    #[test]
    fn self_rss_and_effective_memory_are_nonzero() {
        let pid = std::process::id();
        let rss = process_rss_bytes(pid);
        assert!(rss.is_some_and(|b| b > 0), "expected positive RSS: {rss:?}");
        assert!(effective_memory_bytes() > 0);
    }
}
