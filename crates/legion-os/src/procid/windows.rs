//! Windows process identity, boot identity, and handle-pinned process-tree termination.
//!
//! Termination opens each process with `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`,
//! verifies its exact 100-ns `FILETIME` creation time while the held handle pins the PID against
//! reuse, and calls `TerminateProcess` through that handle—eliminating bare-PID TOCTOU races.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::win_proc::{self, TerminateOutcome};
use crate::{OsError, Result};

use super::{KillTreeReport, ProcessId, is_alive};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn of_pid(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
    win_proc::query_process_identity(pid, include_zombie)
}

pub(super) fn boot_id() -> Result<String> {
    // Query `NtQuerySystemInformation(SystemTimeOfDayInformation)` directly without spawning
    // PowerShell/WMI: `BootTime - BootTimeBias` stays constant across NTP or dual-boot wall-clock
    // steps of any magnitude.
    let unbiased = win_proc::query_unbiased_boot_time()?;
    Ok(format!("boot-win-{unbiased}"))
}

pub(super) async fn kill_tree(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    let identity = *identity;
    let own_pid = std::process::id();

    let tracked_list: Vec<ProcessId> =
        tokio::task::spawn_blocking(move || -> Result<Vec<ProcessId>> {
            if !is_alive(&identity) {
                return Ok(Vec::new());
            }

            let mut tracked: HashMap<u32, ProcessId> = HashMap::new();
            tracked.insert(identity.pid, identity);
            collect_descendants_into(&identity, own_pid, &mut tracked)?;
            terminate_tracked(&tracked, own_pid)?;

            // Take one more snapshot right after the first wave in case a process spawned a late child
            // just before TerminateProcess landed.
            if collect_descendants_into(&identity, own_pid, &mut tracked).is_ok() {
                terminate_tracked(&tracked, own_pid)?;
            }

            Ok(tracked.into_values().collect())
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_tree worker task failed: {err}"),
        })??;

    if tracked_list.is_empty() {
        return Ok(KillTreeReport {
            all_gone: true,
            survivors: Vec::new(),
        });
    }

    let settle = SETTLE_TIMEOUT.max(grace.min(Duration::from_secs(5)));
    let deadline = Instant::now() + settle;
    loop {
        let probe_list = tracked_list.clone();
        let any_alive = tokio::task::spawn_blocking(move || probe_list.iter().any(is_alive))
            .await
            .map_err(|err| OsError::Platform {
                detail: format!("kill_tree poll task failed: {err}"),
            })?;
        if !any_alive {
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

    let survivors = tokio::task::spawn_blocking(move || {
        let mut left: Vec<ProcessId> = tracked_list.into_iter().filter(is_alive).collect();
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

fn terminate_tracked(tracked: &HashMap<u32, ProcessId>, own_pid: u32) -> Result<()> {
    for proc_id in tracked.values() {
        if proc_id.pid == own_pid {
            continue;
        }
        match win_proc::open_verify_terminate(proc_id)? {
            TerminateOutcome::Terminated
            | TerminateOutcome::AlreadyGone
            | TerminateOutcome::IdentityMismatch
            | TerminateOutcome::AccessDenied => {}
        }
    }
    Ok(())
}

fn collect_descendants_into(
    root: &ProcessId,
    own_pid: u32,
    tracked: &mut HashMap<u32, ProcessId>,
) -> Result<()> {
    let entries = win_proc::snapshot_processes()?;
    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in entries {
        if entry.pid == 0 || entry.parent_pid == 0 || entry.pid == entry.parent_pid {
            continue;
        }
        children_by_parent
            .entry(entry.parent_pid)
            .or_default()
            .push(entry.pid);
    }

    let mut visited: HashSet<u32> = HashSet::new();
    let mut queue: VecDeque<ProcessId> = VecDeque::new();
    visited.insert(root.pid);
    queue.push_back(*root);

    while let Some(parent) = queue.pop_front() {
        if let Some(children) = children_by_parent.get(&parent.pid) {
            for &child_pid in children {
                if child_pid == own_pid || !visited.insert(child_pid) {
                    continue;
                }
                let Ok(Some(child_id)) = win_proc::query_process_identity(child_pid, false) else {
                    continue;
                };
                // Full 100-ns FILETIME precision: a real child cannot be created before its parent.
                if child_id.start_time < parent.start_time {
                    continue;
                }
                tracked.entry(child_pid).or_insert(child_id);
                queue.push_back(child_id);
            }
        }
    }
    Ok(())
}
