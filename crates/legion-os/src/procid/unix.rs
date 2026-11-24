//! Unix process identity, boot identity, and process-tree termination.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, getpgid, getpgrp};
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

use crate::{OsError, Result};

use super::{KillTreeReport, ProcessId, is_alive};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(target_os = "linux")]
pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
    use std::fs;
    use std::io;

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
    use std::mem::{size_of, zeroed};

    use crate::procid::ProcessId;
    use crate::{OsError, Result};

    // Darwin `<sys/proc.h>` `SZOMB` state indicating an exited, unreaped zombie process.
    const SZOMB: u32 = 5;

    pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
        let Ok(raw_pid) = i32::try_from(pid) else {
            return Err(OsError::InvalidArgument {
                detail: format!("pid {pid} exceeds Unix pid_t range"),
            });
        };

        // SAFETY: `libc::proc_bsdinfo` is a plain C struct safe to zero-initialize; `proc_pidinfo`
        // writes at most `size_of::<libc::proc_bsdinfo>()` bytes into `info` and returns the byte
        // count written (0 or negative when the PID does not exist or cannot be queried).
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
            return Ok(None);
        }
        if !include_zombie && info.pbi_status == SZOMB {
            return Ok(None);
        }
        let start_us = info
            .pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec);
        if start_us == 0 {
            return Ok(None);
        }
        Ok(Some(ProcessId::new(pid, start_us)))
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
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OsError::Platform {
            detail: "sysctl kern.bootsessionuuid returned empty output".into(),
        });
    }
    Ok(trimmed.to_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn boot_id() -> Result<String> {
    let secs = System::boot_time().map_err(|err| OsError::Platform {
        detail: format!("failed to read system boot time: {err}"),
    })?;
    Ok(format!("boot-sec-{secs}"))
}

pub(super) async fn kill_tree(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    let raw_pid = i32::try_from(identity.pid).map_err(|_| OsError::InvalidArgument {
        detail: format!("pid {} exceeds Unix pid_t range", identity.pid),
    })?;
    let root_nix_pid = Pid::from_raw(raw_pid);
    let identity = *identity;
    let own_pid = std::process::id();

    // Run the initial liveness check, full-table descendant discovery, and SIGTERM wave off the
    // async worker thread.
    let (mut tracked, target_pgid) = tokio::task::spawn_blocking(move || {
        if !is_alive(&identity) {
            return (HashMap::new(), None);
        }

        let own_pgid = getpgrp();
        // Only signal the process group as a whole when the target leads its own group and that
        // group is not ours; otherwise signaling the group would hit unrelated siblings or ourselves.
        let target_pgid = getpgid(Some(root_nix_pid))
            .ok()
            .filter(|&pgid| pgid == root_nix_pid && pgid != own_pgid);

        let mut tracked: HashMap<u32, ProcessId> = HashMap::new();
        tracked.insert(identity.pid, identity);
        collect_tree_into(&identity, target_pgid, own_pid, &mut tracked);

        if let Some(pgid) = target_pgid
            && root_not_recycled(&identity)
        {
            let _ = killpg(pgid, Signal::SIGTERM);
        }
        signal_tracked(&tracked, Signal::SIGTERM, own_pid);
        (tracked, target_pgid)
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_tree worker task failed: {err}"),
    })?;

    if tracked.is_empty() {
        return Ok(KillTreeReport {
            all_gone: true,
            survivors: Vec::new(),
        });
    }

    // Poll only the already-tracked PIDs (plus killpg(0) when isolated) off the async executor.
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
            return Ok(KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
            });
        }
        let now = Instant::now();
        if now >= grace_deadline {
            break;
        }
        let remaining = grace_deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }

    // Phase 2: one last descendant discovery pass to catch any child spawned during grace, then
    // escalate all survivors to SIGKILL.
    tracked = tokio::task::spawn_blocking(move || {
        if root_not_recycled(&identity) {
            collect_tree_into(&identity, target_pgid, own_pid, &mut tracked);
            if let Some(pgid) = target_pgid {
                let _ = killpg(pgid, Signal::SIGKILL);
            }
        }
        signal_tracked(&tracked, Signal::SIGKILL, own_pid);
        tracked
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
            return Ok(KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
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
    let survivors = tokio::task::spawn_blocking(move || {
        let mut left: Vec<ProcessId> = tracked_snapshot.into_iter().filter(is_alive).collect();
        left.sort_by_key(|id| id.pid);
        left
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("kill_tree survivor check failed: {err}"),
    })?;

    Ok(KillTreeReport {
        all_gone: survivors.is_empty(),
        survivors,
    })
}

/// True unless a different process has already reused `identity.pid`.
fn root_not_recycled(identity: &ProcessId) -> bool {
    match super::of_pid(identity.pid) {
        Ok(Some(observed)) => identity.matches(&observed),
        Ok(None) => true,
        Err(_) => false,
    }
}

fn tracked_is_gone(identity: &ProcessId, target_pgid: Option<Pid>, tracked: &[ProcessId]) -> bool {
    if tracked.iter().any(is_alive) {
        return false;
    }
    if let Some(pgid) = target_pgid
        && root_not_recycled(identity)
    {
        match killpg(pgid, None) {
            Err(Errno::ESRCH) => {}
            _ => return false,
        }
    }
    true
}

fn signal_tracked(tracked: &HashMap<u32, ProcessId>, sig: Signal, own_pid: u32) {
    for proc_id in tracked.values() {
        if proc_id.pid == own_pid {
            continue;
        }
        if !is_alive(proc_id) {
            continue;
        }
        if let Ok(raw) = i32::try_from(proc_id.pid) {
            let _ = kill(Pid::from_raw(raw), sig);
        }
    }
}

fn collect_tree_into(
    root: &ProcessId,
    target_pgid: Option<Pid>,
    own_pid: u32,
    tracked: &mut HashMap<u32, ProcessId>,
) {
    let Ok(mut system) = System::new() else {
        return;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

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

        // Also capture any process still in the target's isolated process group even if its
        // direct parent exited and it was reparented to init.
        if let Some(pgid) = target_pgid
            && pid != own_pid
            && let Ok(raw) = i32::try_from(pid)
            && getpgid(Some(Pid::from_raw(raw))).ok() == Some(pgid)
            && !tracked.contains_key(&pid)
            && let Ok(Some(id)) = super::of_pid(pid)
        {
            tracked.insert(pid, id);
        }
    }

    if !is_alive(root) {
        return;
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<(u32, u64)> = VecDeque::new();
    let root_sys_start = start_by_pid.get(&root.pid).copied().unwrap_or(0);
    visited.insert(root.pid);
    queue.push_back((root.pid, root_sys_start));

    while let Some((parent_pid, parent_start)) = queue.pop_front() {
        if let Some(children) = children_by_parent.get(&parent_pid) {
            for &(child_pid, child_start) in children {
                if child_pid == own_pid || !visited.insert(child_pid) {
                    continue;
                }
                // Allow 1 second of rounding slack because sysinfo exposes epoch seconds.
                if parent_start > 0 && child_start.saturating_add(1) < parent_start {
                    continue;
                }
                if !tracked.contains_key(&child_pid)
                    && let Ok(Some(child_id)) = super::of_pid(child_pid)
                {
                    tracked.insert(child_pid, child_id);
                }
                queue.push_back((child_pid, child_start));
            }
        }
    }
}
