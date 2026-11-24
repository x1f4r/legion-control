//! Windows process identity, boot identity, and handle-pinned process-tree termination.
//!
//! Termination opens each process with `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`,
//! verifies its exact 100-ns `FILETIME` creation time while the held handle pins the PID against
//! reuse, and calls `TerminateProcess` through that handle—eliminating bare-PID TOCTOU races.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crate::win_proc::{self, OpenPinnedOutcome, PinnedProcess, TerminateOutcome};
use crate::{OsError, Result};

use super::{KillTreeReport, ProcessId, is_alive};

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_WALK_PASSES: usize = 8;

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
            let mut pinned: HashMap<u32, PinnedProcess> = HashMap::new();
            let mut root_pinned = false;

            // Pin and verify the root first so its PID cannot be reused while we walk descendants.
            match win_proc::open_pinned(identity.pid, false) {
                OpenPinnedOutcome::Pinned(root_proc) => {
                    if !identity.matches(&root_proc.id) {
                        return Ok(Vec::new());
                    }
                    let _ = root_proc.terminate();
                    pinned.insert(root_proc.id.pid, root_proc);
                    root_pinned = true;
                }
                OpenPinnedOutcome::Gone => return Ok(Vec::new()),
                OpenPinnedOutcome::AccessDenied | OpenPinnedOutcome::Failed => {
                    // Could not open the root handle; check whether the exact root is still alive
                    // so we can still attempt to discover its children and report it as a survivor.
                    if !is_alive(&identity) {
                        return Ok(Vec::new());
                    }
                }
            }

            // Re-snapshot and terminate newly discovered descendants in bounded waves while
            // keeping every previously opened handle alive so none of their PIDs can be reused
            // between passes.
            for pass in 0..MAX_WALK_PASSES {
                let newly_found = discover_and_terminate_pass(&identity, own_pid, &mut pinned)?;
                if newly_found == 0 && (pass > 0 || !root_pinned) {
                    break;
                }
            }

            let mut tracked: Vec<ProcessId> = pinned.into_values().map(|p| p.id).collect();
            if !root_pinned && !tracked.iter().any(|p| p.pid == identity.pid) {
                tracked.push(identity);
            }
            Ok(tracked)
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

/// Take one process-table snapshot, verify every candidate child's `parent_pid` and creation time
/// directly from a newly pinned handle, terminate newly confirmed descendants, and retain all
/// pinned handles in `pinned` across passes.
fn discover_and_terminate_pass(
    root: &ProcessId,
    own_pid: u32,
    pinned: &mut HashMap<u32, PinnedProcess>,
) -> Result<usize> {
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

    let mut queue: VecDeque<ProcessId> = VecDeque::new();
    let mut queued: HashSet<u32> = HashSet::new();
    queue.push_back(*root);
    queued.insert(root.pid);
    for proc_handle in pinned.values() {
        if queued.insert(proc_handle.id.pid) {
            queue.push_back(proc_handle.id);
        }
    }

    let mut visited_candidates: HashSet<u32> = HashSet::new();
    let mut newly_found = 0usize;

    while let Some(parent) = queue.pop_front() {
        let Some(children) = children_by_parent.get(&parent.pid) else {
            continue;
        };
        for &child_pid in children {
            if child_pid == own_pid || pinned.contains_key(&child_pid) {
                continue;
            }
            if !visited_candidates.insert(child_pid) {
                continue;
            }

            match win_proc::open_pinned(child_pid, false) {
                OpenPinnedOutcome::Pinned(child_proc) => {
                    // Validate parent_pid directly on the opened handle (not just the snapshot)
                    // AND verify full 100-ns FILETIME parent-before-child ordering.
                    if child_proc.parent_pid != parent.pid
                        || child_proc.id.start_time < parent.start_time
                    {
                        continue;
                    }
                    let child_id = child_proc.id;
                    let outcome = child_proc.terminate();
                    let _ = matches!(
                        outcome,
                        TerminateOutcome::Terminated
                            | TerminateOutcome::AlreadyGone
                            | TerminateOutcome::IdentityMismatch
                            | TerminateOutcome::AccessDenied
                    );
                    pinned.insert(child_pid, child_proc);
                    if queued.insert(child_pid) {
                        queue.push_back(child_id);
                    }
                    newly_found += 1;
                }
                // Single-process open/time failures never abort the rest of the wave.
                OpenPinnedOutcome::Gone
                | OpenPinnedOutcome::AccessDenied
                | OpenPinnedOutcome::Failed => {}
            }
        }
    }

    Ok(newly_found)
}
