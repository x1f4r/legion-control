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
const MAX_ANCESTOR_HOPS: usize = 256;

pub(super) fn is_protected(identity: &ProcessId) -> Option<&'static str> {
    if identity.pid == 4 {
        return Some("Windows System process (pid 4)");
    }
    if is_ancestor_of_current(identity) {
        return Some("ancestor of current process");
    }
    win_proc::check_protected_system_process(identity)
}

fn is_ancestor_of_current(identity: &ProcessId) -> bool {
    let own_pid = std::process::id();
    let OpenPinnedOutcome::Pinned(self_proc) = win_proc::open_pinned(own_pid, false) else {
        return false;
    };
    let mut parent_pid = self_proc.parent_pid;
    let mut child_start = self_proc.id.start_time;
    drop(self_proc);

    let mut seen = HashSet::new();
    seen.insert(own_pid);

    for _ in 0..MAX_ANCESTOR_HOPS {
        if parent_pid == 0 || parent_pid == 4 || !seen.insert(parent_pid) {
            break;
        }
        match win_proc::open_pinned(parent_pid, false) {
            OpenPinnedOutcome::Pinned(parent_proc) => {
                // Stop if `parent_pid` has been recycled by a process newer than its child.
                if parent_proc.id.start_time > child_start {
                    break;
                }
                if parent_proc.id.pid == identity.pid
                    && parent_proc.id.start_time == identity.start_time
                {
                    return true;
                }
                child_start = parent_proc.id.start_time;
                parent_pid = parent_proc.parent_pid;
            }
            OpenPinnedOutcome::AccessDenied => {
                return parent_pid == identity.pid;
            }
            OpenPinnedOutcome::Gone | OpenPinnedOutcome::Failed => break,
        }
    }
    false
}

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

pub(super) async fn kill_process(
    identity: &ProcessId,
    _force: bool,
    grace: Duration,
) -> Result<bool> {
    let identity = *identity;

    let outcome = tokio::task::spawn_blocking(move || win_proc::open_verify_terminate(&identity))
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_process worker task failed: {err}"),
        })??;

    match outcome {
        TerminateOutcome::AlreadyGone | TerminateOutcome::IdentityMismatch => return Ok(true),
        TerminateOutcome::AccessDenied => {
            return Err(OsError::PermissionDenied {
                detail: format!("permission denied terminating process {}", identity.pid),
                source: None,
            });
        }
        TerminateOutcome::Terminated => {}
    }

    let settle = SETTLE_TIMEOUT.max(grace.min(Duration::from_secs(5)));
    let deadline = Instant::now() + settle;
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
        if now >= deadline {
            return Ok(false);
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }
}

pub(super) async fn kill_tree(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    let identity = *identity;
    let own_pid = std::process::id();

    let tracked_list: Vec<ProcessId> =
        tokio::task::spawn_blocking(move || -> Result<Vec<ProcessId>> {
            let mut pinned: HashMap<u32, PinnedProcess> = HashMap::new();
            let mut root_pinned = false;
            let mut root_live_unopened = false;
            // Held open (never terminated) if `identity.pid` was already reused by a newer process
            // so the PID stays pinned and direct orphan children of `identity` are upper-bounded
            // by the recycled process's creation time.
            let mut recycled_root_guard: Option<PinnedProcess> = None;
            let mut direct_child_max_start: Option<u64> = None;

            match win_proc::open_pinned(identity.pid, false) {
                OpenPinnedOutcome::Pinned(root_proc) => {
                    if identity.matches(&root_proc.id) {
                        let _ = root_proc.terminate();
                        pinned.insert(root_proc.id.pid, root_proc);
                        root_pinned = true;
                    } else if root_proc.id.start_time > identity.start_time {
                        // The original root exited and a newer process reused its PID. Orphans of
                        // the original root still carry `parent_pid == identity.pid` with creation
                        // times strictly earlier than the recycled process's creation time.
                        direct_child_max_start = Some(root_proc.id.start_time);
                        recycled_root_guard = Some(root_proc);
                    } else {
                        // The process currently holding `identity.pid` started before `identity`,
                        // so `identity` cannot correspond to this PID or any orphan under it.
                        return Ok(Vec::new());
                    }
                }
                // Root exited and its PID is currently vacant; Windows orphans keep their original
                // `parent_pid`, so keep walking from `(identity.pid, identity.start_time)`.
                OpenPinnedOutcome::Gone => {}
                OpenPinnedOutcome::AccessDenied | OpenPinnedOutcome::Failed => {
                    match super::of_pid(identity.pid) {
                        Ok(Some(observed)) if identity.matches(&observed) => {
                            root_live_unopened = true;
                        }
                        Ok(Some(observed)) if observed.start_time > identity.start_time => {
                            direct_child_max_start = Some(observed.start_time);
                        }
                        Ok(Some(_)) => return Ok(Vec::new()),
                        Ok(None) | Err(_) => {}
                    }
                }
            }

            // Re-snapshot and terminate newly discovered descendants in bounded waves while
            // keeping every previously opened handle alive so none of their PIDs can be reused
            // between passes.
            for pass in 0..MAX_WALK_PASSES {
                // If `identity.pid` was initially vacant and gets reused between passes, bound
                // direct children by the new occupant's creation time without touching it.
                if !root_pinned
                    && direct_child_max_start.is_none()
                    && let OpenPinnedOutcome::Pinned(occupant) =
                        win_proc::open_pinned(identity.pid, false)
                    && !identity.matches(&occupant.id)
                {
                    direct_child_max_start = Some(occupant.id.start_time);
                    recycled_root_guard = Some(occupant);
                }

                let newly_found = discover_and_terminate_pass(
                    &identity,
                    direct_child_max_start,
                    own_pid,
                    &mut pinned,
                )?;
                if newly_found == 0 && (pass > 0 || !root_pinned) {
                    break;
                }
            }

            drop(recycled_root_guard);
            let mut tracked: Vec<ProcessId> = pinned.into_values().map(|p| p.id).collect();
            if root_live_unopened && !tracked.iter().any(|p| p.pid == identity.pid) {
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
    direct_child_max_start: Option<u64>,
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
        let max_start = if parent.pid == root.pid {
            direct_child_max_start
        } else {
            None
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
                    // AND verify full 100-ns FILETIME parent-before-child ordering (plus the upper
                    // bound when `root.pid` was recycled by a newer process).
                    if child_proc.parent_pid != parent.pid
                        || child_proc.id.start_time < parent.start_time
                        || max_start.is_some_and(|limit| child_proc.id.start_time >= limit)
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
