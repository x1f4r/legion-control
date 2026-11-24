//! Windows process identity, boot identity, and process-tree termination.

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::windows::process::CommandExt;
use std::process::Stdio;
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::{OsError, Result};

use super::{KillTreeReport, ProcessId, is_alive};

const TASKKILL_TIMEOUT: Duration = Duration::from_secs(15);
const BOOT_ID_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn of_pid(pid: u32, _include_zombie: bool) -> Result<Option<ProcessId>> {
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
    let start_time = proc_info.start_time();
    if start_time == 0 {
        return Err(OsError::PermissionDenied {
            detail: format!("cannot query creation time for process {pid}"),
            source: None,
        });
    }
    Ok(Some(ProcessId::new(pid, start_time)))
}

pub(super) fn boot_id() -> Result<String> {
    // Always use one single source and format (`boot-ticks-<ticks>` from Win32_OperatingSystem
    // LastBootUpTime) with a hard timeout so a stalled WMI service cannot hang a caller and two
    // incompatible boot-id formats are never mixed. Callers compare with `procid::same_boot`.
    let mut cmd = std::process::Command::new("powershell.exe");
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().Ticks",
    ])
    .env_remove("PSModulePath")
    .creation_flags(CREATE_NO_WINDOW.0);

    let stdout = super::run_command_stdout_with_timeout(cmd, BOOT_ID_TIMEOUT, "powershell.exe")?;
    let ticks = stdout.trim();
    if !ticks.is_empty()
        && ticks.bytes().all(|b| b.is_ascii_digit())
        && ticks.parse::<u64>().is_ok()
    {
        return Ok(format!("boot-ticks-{ticks}"));
    }

    Err(OsError::Platform {
        detail: format!("unexpected Windows LastBootUpTime output: {ticks:?}"),
    })
}

pub(super) async fn kill_tree(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    if !is_alive(identity) {
        return Ok(KillTreeReport {
            all_gone: true,
            survivors: Vec::new(),
        });
    }

    let own_pid = std::process::id();
    let mut tracked: HashMap<u32, ProcessId> = HashMap::new();
    tracked.insert(identity.pid, *identity);
    collect_descendants_into(identity, own_pid, &mut tracked);

    run_taskkill(identity.pid).await?;

    // Also terminate any discovered descendants that survived (for example if an intermediate
    // child exited earlier and broke the parent chain that taskkill /T traverses).
    for proc_id in tracked.values() {
        if proc_id.pid == identity.pid || proc_id.pid == own_pid {
            continue;
        }
        if is_alive(proc_id) {
            let _ = run_taskkill(proc_id.pid).await;
        }
    }

    let settle = SETTLE_TIMEOUT.max(grace.min(Duration::from_secs(5)));
    let deadline = Instant::now() + settle;
    loop {
        if !tracked.values().any(is_alive) {
            return Ok(KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
            });
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }

    let mut survivors: Vec<ProcessId> = tracked.values().copied().filter(is_alive).collect();
    survivors.sort_by_key(|id| id.pid);
    Ok(KillTreeReport {
        all_gone: survivors.is_empty(),
        survivors,
    })
}

async fn run_taskkill(pid: u32) -> Result<()> {
    let mut cmd = tokio::process::Command::new("taskkill.exe");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW.0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|err| OsError::from_spawn_io("taskkill.exe", err))?;

    match tokio::time::timeout(TASKKILL_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(OsError::from_io("failed waiting for taskkill.exe", err)),
        Err(_) => {
            let _ = child.kill().await;
            Err(OsError::TimedOut {
                detail: format!("taskkill.exe did not finish stopping pid {pid} in time"),
            })
        }
    }
}

fn collect_descendants_into(root: &ProcessId, own_pid: u32, tracked: &mut HashMap<u32, ProcessId>) {
    let Ok(mut system) = System::new() else {
        return;
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());

    let mut children_by_parent: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    for (sys_pid, proc_info) in system.processes() {
        let pid = sys_pid.as_u32();
        let started = proc_info.start_time();
        if started == 0 {
            continue;
        }
        if let Some(parent) = proc_info.parent() {
            children_by_parent
                .entry(parent.as_u32())
                .or_default()
                .push((pid, started));
        }
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<(u32, u64)> = VecDeque::new();
    visited.insert(root.pid);
    queue.push_back((root.pid, root.start_time));

    while let Some((parent_pid, parent_start)) = queue.pop_front() {
        if let Some(children) = children_by_parent.get(&parent_pid) {
            for &(child_pid, child_start) in children {
                if child_pid == own_pid || !visited.insert(child_pid) {
                    continue;
                }
                // Reject a process that started earlier than its nominal parent (PID reuse).
                if child_start.saturating_add(1) < parent_start {
                    continue;
                }
                tracked
                    .entry(child_pid)
                    .or_insert_with(|| ProcessId::new(child_pid, child_start));
                queue.push_back((child_pid, child_start));
            }
        }
    }
}
