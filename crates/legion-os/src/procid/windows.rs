//! Windows process identity, boot identity, and handle-pinned process-tree termination.
//!
//! Termination opens each process with `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`,
//! verifies its exact 100-ns `FILETIME` creation time while the held handle pins the PID against
//! reuse, and calls `TerminateProcess` through that handle—eliminating bare-PID TOCTOU races.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::win_proc::{self, OpenPinnedOutcome, PinnedProcess, TerminateOutcome};
use crate::{OsError, Result};

use super::{
    EnvMarkerCheck, KillTreeReport, ProcessId, WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME,
    child_not_older_than_parent, is_alive,
};

const POLL_INTERVAL: Duration = Duration::from_millis(25);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_WALK_PASSES: usize = 8;
const MAX_ANCESTOR_HOPS: usize = 256;

/// Verify that `child` is causally newer than `parent`, preferring monotonic Windows kernel
/// `ProcessStartKey` values (immune to NTP / dual-boot wall-clock steps and recycled PIDs).
///
/// Falls back to wall-clock `FILETIME` comparison only when either process lacks a start key,
/// restricting backward clock-step tolerance to non-protected same-user processes so recycled
/// parent PIDs never match boot-time or elevated system processes.
fn is_causally_not_older_than_parent(
    child: &PinnedProcess,
    parent_id: &ProcessId,
    parent_start_key: Option<u64>,
    max_start_time: Option<u64>,
    max_start_key: Option<u64>,
) -> bool {
    if child.parent_pid != parent_id.pid {
        return false;
    }
    if let (Some(ck), Some(pk)) = (child.start_key, parent_start_key) {
        if ck <= pk {
            return false;
        }
        if max_start_key.is_some_and(|limit| ck >= limit) {
            return false;
        }
        return true;
    }
    if max_start_time.is_some_and(|limit| child.id.start_time >= limit) {
        return false;
    }
    if child.id.start_time >= parent_id.start_time {
        return true;
    }
    child_not_older_than_parent(
        child.id.start_time,
        parent_id.start_time,
        WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME,
    ) && !child.is_protected_system()
        && child.same_user_as_current()
}

pub(super) fn is_protected(identity: &ProcessId) -> Option<&'static str> {
    if identity.pid == 4 {
        return Some("Windows System process (pid 4)");
    }
    if is_ancestor_of_current(identity) {
        return Some("ancestor of current process");
    }
    win_proc::check_protected_system_process(identity)
}

fn windows_info_is_same_account_sshd(info: &win_proc::WindowsProcSshdInfo) -> bool {
    if !info.same_account || info.id.pid <= 4 || info.id.pid == std::process::id() {
        return false;
    }
    let Some(cmdline) = info.cmdline.as_deref() else {
        return false;
    };
    let argv = super::split_windows_cmdline(cmdline);
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    super::is_sshd_session_process(&info.image_path, &argv_refs, true)
}

pub(super) fn sshd_session_ancestor() -> Option<ProcessId> {
    let own_pid = std::process::id();
    let OpenPinnedOutcome::Pinned(self_proc) = win_proc::open_pinned_query_only(own_pid, false)
    else {
        return None;
    };
    let parent_pid = self_proc.parent_pid;
    let child_start = self_proc.id.start_time;
    let child_start_key = self_proc.start_key;
    drop(self_proc);

    // On Windows, `sshd.exe` session workers do not rewrite argv[0] with setproctitle, so only
    // accept a bare same-account `sshd.exe` / `sshd-session.exe` when it is the direct parent of
    // this gate process (hop 0) — never walk through shell wrappers to a higher process.
    if parent_pid <= 4 || parent_pid == own_pid {
        return None;
    }
    let info = win_proc::inspect_process_for_sshd(parent_pid)?;
    let valid_order = match (child_start_key, info.start_key) {
        (Some(ck), Some(pk)) => ck > pk,
        _ => child_not_older_than_parent(
            child_start,
            info.id.start_time,
            WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME,
        ),
    };
    if !valid_order {
        return None;
    }
    windows_info_is_same_account_sshd(&info).then_some(info.id)
}

pub(super) fn verify_same_account_sshd(identity: &ProcessId) -> bool {
    if identity.pid <= 4 || identity.pid == std::process::id() {
        return false;
    }
    let Some(info) = win_proc::inspect_process_for_sshd(identity.pid) else {
        return false;
    };
    identity.matches(&info.id) && windows_info_is_same_account_sshd(&info)
}

pub(super) fn executable_of(identity: &ProcessId) -> Option<PathBuf> {
    win_proc::query_verified_image_path(identity)
}

fn is_ancestor_of_current(identity: &ProcessId) -> bool {
    let own_pid = std::process::id();
    let OpenPinnedOutcome::Pinned(self_proc) = win_proc::open_pinned_query_only(own_pid, false)
    else {
        return false;
    };
    let mut parent_pid = self_proc.parent_pid;
    let mut child_start = self_proc.id.start_time;
    let mut child_start_key = self_proc.start_key;
    drop(self_proc);

    let mut seen = HashSet::new();
    seen.insert(own_pid);

    for _ in 0..MAX_ANCESTOR_HOPS {
        if parent_pid == 0 || parent_pid == 4 || !seen.insert(parent_pid) {
            break;
        }
        match win_proc::open_pinned_query_only(parent_pid, false) {
            OpenPinnedOutcome::Pinned(parent_proc) => {
                // Stop if `parent_pid` has been recycled by a process newer than its child.
                let valid_order = match (child_start_key, parent_proc.start_key) {
                    (Some(ck), Some(pk)) => ck > pk,
                    _ => child_not_older_than_parent(
                        child_start,
                        parent_proc.id.start_time,
                        WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME,
                    ),
                };
                if !valid_order {
                    break;
                }
                if parent_proc.id.pid == identity.pid
                    && parent_proc.id.start_time == identity.start_time
                {
                    return true;
                }
                child_start = parent_proc.id.start_time;
                child_start_key = parent_proc.start_key;
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

pub(super) async fn find_marked_live_processes(marker: &str) -> Result<Vec<ProcessId>> {
    let marker_owned = marker.to_owned();
    let own_pid = std::process::id();
    tokio::task::spawn_blocking(move || -> Result<Vec<ProcessId>> {
        let entries = win_proc::snapshot_processes()?;
        let mut found = Vec::new();
        for entry in entries {
            if entry.pid == 0 || entry.pid == own_pid {
                continue;
            }
            if win_proc::check_process_env_marker(
                entry.pid,
                crate::detach::DETACH_MARKER_ENV,
                &marker_owned,
            ) == EnvMarkerCheck::Matched
                && let OpenPinnedOutcome::Pinned(proc_handle) =
                    win_proc::open_pinned_query_only(entry.pid, false)
            {
                found.push(proc_handle.id);
            }
        }
        found.sort_by_key(|id| id.pid);
        Ok(found)
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("find_marked_live_processes worker task failed: {err}"),
    })?
}

pub(super) async fn tree_is_active(identity: &ProcessId, marker: Option<&str>) -> Result<bool> {
    let identity = *identity;
    let marker_owned = marker.map(str::to_owned);
    let own_pid = std::process::id();

    tokio::task::spawn_blocking(move || -> Result<bool> {
        // Fast path: check root directly using read-only query rights.
        match win_proc::open_pinned_query_only(identity.pid, false) {
            OpenPinnedOutcome::Pinned(root_proc) => {
                if identity.matches(&root_proc.id) {
                    return Ok(true);
                }
            }
            OpenPinnedOutcome::Gone => {}
            OpenPinnedOutcome::AccessDenied | OpenPinnedOutcome::Failed => {
                if let Ok(Some(observed)) = super::of_pid(identity.pid)
                    && identity.matches(&observed)
                {
                    return Ok(true);
                }
            }
        }

        // Once the root itself is gone, treat another process as part of the tree ONLY if it
        // carries the detach environment marker proving ownership across PID reuse.
        let Some(ref marker_val) = marker_owned else {
            return Ok(false);
        };

        let entries = win_proc::snapshot_processes()?;
        for entry in entries {
            if entry.pid == 0 || entry.pid == own_pid {
                continue;
            }
            if win_proc::check_process_env_marker(
                entry.pid,
                crate::detach::DETACH_MARKER_ENV,
                marker_val,
            ) == EnvMarkerCheck::Matched
                && matches!(
                    win_proc::open_pinned_query_only(entry.pid, false),
                    OpenPinnedOutcome::Pinned(_)
                )
            {
                return Ok(true);
            }
        }
        Ok(false)
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("tree_is_active worker task failed: {err}"),
    })?
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

pub(super) async fn kill_tree(
    identity: &ProcessId,
    marker: Option<&str>,
    grace: Duration,
) -> Result<KillTreeReport> {
    let identity = *identity;
    let marker_owned = marker.map(str::to_owned);
    let own_pid = std::process::id();

    let (tracked_list, unverified_list): (Vec<ProcessId>, Vec<ProcessId>) =
        tokio::task::spawn_blocking(move || -> Result<(Vec<ProcessId>, Vec<ProcessId>)> {
            let mut pinned: HashMap<u32, PinnedProcess> = HashMap::new();
            let mut unverified: HashMap<u32, ProcessId> = HashMap::new();
            let mut root_pinned = false;
            let mut root_live_unopened = false;
            let mut root_start_key: Option<u64> = None;
            let mut recycled_root_guard: Option<PinnedProcess> = None;
            let mut direct_child_max_start: Option<u64> = None;
            let mut direct_child_max_start_key: Option<u64> = None;
            let mut root_preceded_by_older = false;

            match win_proc::open_pinned(identity.pid, false) {
                OpenPinnedOutcome::Pinned(root_proc) => {
                    if identity.matches(&root_proc.id) {
                        root_start_key = root_proc.start_key;
                        let _ = root_proc.terminate();
                        pinned.insert(root_proc.id.pid, root_proc);
                        root_pinned = true;
                    } else if root_proc.id.start_time > identity.start_time {
                        direct_child_max_start = Some(root_proc.id.start_time);
                        direct_child_max_start_key = root_proc.start_key;
                        recycled_root_guard = Some(root_proc);
                    } else {
                        root_preceded_by_older = true;
                    }
                }
                OpenPinnedOutcome::Gone => {}
                OpenPinnedOutcome::AccessDenied | OpenPinnedOutcome::Failed => {
                    match super::of_pid(identity.pid) {
                        Ok(Some(observed)) if identity.matches(&observed) => {
                            root_live_unopened = true;
                        }
                        Ok(Some(observed)) if observed.start_time > identity.start_time => {
                            direct_child_max_start = Some(observed.start_time);
                        }
                        Ok(Some(_)) => {
                            root_preceded_by_older = true;
                        }
                        Ok(None) | Err(_) => {}
                    }
                }
            }

            if root_pinned || root_live_unopened {
                // While `root` is pinned in `pinned`, Windows cannot reuse `identity.pid`.
                for pass in 0..MAX_WALK_PASSES {
                    let mut newly_found = discover_and_terminate_pass(
                        &identity,
                        root_start_key,
                        direct_child_max_start,
                        direct_child_max_start_key,
                        own_pid,
                        &mut pinned,
                    )?;
                    if let Some(ref marker_val) = marker_owned {
                        newly_found += discover_marked_orphans_pass(
                            &identity,
                            root_start_key,
                            direct_child_max_start,
                            direct_child_max_start_key,
                            false,
                            marker_val,
                            own_pid,
                            &mut pinned,
                            &mut unverified,
                        )?;
                    }
                    if newly_found == 0 && (pass > 0 || !root_pinned) {
                        break;
                    }
                }
            } else {
                // Root is already gone. Never kill by `parent_pid` alone across Windows PID reuse:
                // require `LEGION_DETACH_ID=<marker>` proof, or report unverified candidates.
                match marker_owned.as_deref() {
                    Some(marker_val) => {
                        for pass in 0..MAX_WALK_PASSES {
                            let mut newly_found = discover_marked_orphans_pass(
                                &identity,
                                root_start_key,
                                direct_child_max_start,
                                direct_child_max_start_key,
                                root_preceded_by_older,
                                marker_val,
                                own_pid,
                                &mut pinned,
                                &mut unverified,
                            )?;
                            if !pinned.is_empty() {
                                newly_found += discover_children_of_pinned_pass(
                                    &identity,
                                    root_start_key,
                                    own_pid,
                                    &mut pinned,
                                )?;
                            }
                            if newly_found == 0 && pass > 0 {
                                break;
                            }
                        }
                    }
                    None => {
                        if !root_preceded_by_older {
                            collect_unverified_orphans_readonly(
                                &identity,
                                root_start_key,
                                direct_child_max_start,
                                direct_child_max_start_key,
                                own_pid,
                                &mut unverified,
                            )?;
                        }
                    }
                }
            }

            drop(recycled_root_guard);
            let tracked_pids: HashSet<u32> = pinned.keys().copied().collect();
            let mut tracked: Vec<ProcessId> = pinned.into_values().map(|p| p.id).collect();
            if root_live_unopened && !tracked.iter().any(|p| p.pid == identity.pid) {
                tracked.push(identity);
            }
            let unverified_out: Vec<ProcessId> = unverified
                .into_iter()
                .filter(|(pid, _)| !tracked_pids.contains(pid))
                .map(|(_, id)| id)
                .collect();
            Ok((tracked, unverified_out))
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_tree worker task failed: {err}"),
        })??;

    if tracked_list.is_empty() {
        let unverified_survivors = tokio::task::spawn_blocking(move || {
            let mut unv: Vec<ProcessId> = unverified_list.into_iter().filter(is_alive).collect();
            unv.sort_by_key(|id| id.pid);
            unv
        })
        .await
        .map_err(|err| OsError::Platform {
            detail: format!("kill_tree unverified survivor check failed: {err}"),
        })?;
        return Ok(KillTreeReport {
            all_gone: unverified_survivors.is_empty(),
            survivors: Vec::new(),
            unverified_survivors,
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
            let unverified_survivors = tokio::task::spawn_blocking(move || {
                let mut unv: Vec<ProcessId> =
                    unverified_list.into_iter().filter(is_alive).collect();
                unv.sort_by_key(|id| id.pid);
                unv
            })
            .await
            .map_err(|err| OsError::Platform {
                detail: format!("kill_tree unverified survivor check failed: {err}"),
            })?;
            return Ok(KillTreeReport {
                all_gone: unverified_survivors.is_empty(),
                survivors: Vec::new(),
                unverified_survivors,
            });
        }
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }

    let (survivors, unverified_survivors) = tokio::task::spawn_blocking(move || {
        let mut left: Vec<ProcessId> = tracked_list.into_iter().filter(is_alive).collect();
        left.sort_by_key(|id| id.pid);
        let mut unv: Vec<ProcessId> = unverified_list.into_iter().filter(is_alive).collect();
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

fn collect_unverified_orphans_readonly(
    root: &ProcessId,
    root_start_key: Option<u64>,
    direct_child_max_start: Option<u64>,
    direct_child_max_start_key: Option<u64>,
    own_pid: u32,
    unverified: &mut HashMap<u32, ProcessId>,
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

    let mut queue: VecDeque<(ProcessId, Option<u64>)> = VecDeque::new();
    let mut visited: HashSet<u32> = HashSet::new();
    queue.push_back((*root, root_start_key));
    visited.insert(root.pid);

    while let Some((parent, parent_start_key)) = queue.pop_front() {
        let Some(children) = children_by_parent.get(&parent.pid) else {
            continue;
        };
        let (max_start, max_start_key) = if parent.pid == root.pid {
            (direct_child_max_start, direct_child_max_start_key)
        } else {
            (None, None)
        };
        for &child_pid in children {
            if child_pid == own_pid || child_pid == 4 || !visited.insert(child_pid) {
                continue;
            }
            if let OpenPinnedOutcome::Pinned(child) =
                win_proc::open_pinned_query_only(child_pid, false)
                && is_causally_not_older_than_parent(
                    &child,
                    &parent,
                    parent_start_key,
                    max_start,
                    max_start_key,
                )
            {
                let child_id = child.id;
                let child_key = child.start_key;
                unverified.entry(child_pid).or_insert(child_id);
                queue.push_back((child_id, child_key));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn discover_marked_orphans_pass(
    root: &ProcessId,
    root_start_key: Option<u64>,
    direct_child_max_start: Option<u64>,
    direct_child_max_start_key: Option<u64>,
    root_preceded_by_older: bool,
    marker: &str,
    own_pid: u32,
    pinned: &mut HashMap<u32, PinnedProcess>,
    unverified: &mut HashMap<u32, ProcessId>,
) -> Result<usize> {
    let entries = win_proc::snapshot_processes()?;
    let mut newly_found = 0usize;
    for entry in entries {
        if entry.pid == 0
            || entry.pid == 4
            || entry.pid == own_pid
            || pinned.contains_key(&entry.pid)
        {
            continue;
        }
        match win_proc::check_process_env_marker(
            entry.pid,
            crate::detach::DETACH_MARKER_ENV,
            marker,
        ) {
            EnvMarkerCheck::Matched => {
                if let OpenPinnedOutcome::Pinned(proc_handle) =
                    win_proc::open_pinned(entry.pid, false)
                {
                    if proc_handle.is_protected_system() {
                        continue;
                    }
                    let _ = proc_handle.terminate();
                    pinned.insert(entry.pid, proc_handle);
                    unverified.remove(&entry.pid);
                    newly_found += 1;
                }
            }
            EnvMarkerCheck::NotMatched => {}
            EnvMarkerCheck::Unverifiable => {
                if !root_preceded_by_older
                    && entry.parent_pid == root.pid
                    && entry.pid != root.pid
                    && let OpenPinnedOutcome::Pinned(child) =
                        win_proc::open_pinned_query_only(entry.pid, false)
                    && is_causally_not_older_than_parent(
                        &child,
                        root,
                        root_start_key,
                        direct_child_max_start,
                        direct_child_max_start_key,
                    )
                {
                    unverified.entry(entry.pid).or_insert(child.id);
                }
            }
        }
    }
    Ok(newly_found)
}

fn discover_children_of_pinned_pass(
    root: &ProcessId,
    root_start_key: Option<u64>,
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

    let mut queue: VecDeque<(ProcessId, Option<u64>)> = VecDeque::new();
    let mut queued: HashSet<u32> = HashSet::new();
    for proc_handle in pinned.values() {
        if queued.insert(proc_handle.id.pid) {
            queue.push_back((proc_handle.id, proc_handle.start_key));
        }
    }

    let mut visited_candidates: HashSet<u32> = HashSet::new();
    let mut newly_found = 0usize;

    while let Some((parent, parent_start_key)) = queue.pop_front() {
        let Some(children) = children_by_parent.get(&parent.pid) else {
            continue;
        };
        for &child_pid in children {
            if child_pid == own_pid || child_pid == 4 || pinned.contains_key(&child_pid) {
                continue;
            }
            if !visited_candidates.insert(child_pid) {
                continue;
            }
            if let OpenPinnedOutcome::Pinned(child_proc) = win_proc::open_pinned(child_pid, false) {
                if child_proc.is_protected_system()
                    || !is_causally_not_older_than_parent(
                        &child_proc,
                        &parent,
                        parent_start_key,
                        None,
                        None,
                    )
                {
                    continue;
                }
                let newer_than_root = match (child_proc.start_key, root_start_key) {
                    (Some(ck), Some(rk)) => ck > rk,
                    _ => child_not_older_than_parent(
                        child_proc.id.start_time,
                        root.start_time,
                        WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME,
                    ),
                };
                if !newer_than_root {
                    continue;
                }
                let child_id = child_proc.id;
                let child_key = child_proc.start_key;
                let _ = child_proc.terminate();
                pinned.insert(child_pid, child_proc);
                if queued.insert(child_pid) {
                    queue.push_back((child_id, child_key));
                }
                newly_found += 1;
            }
        }
    }

    Ok(newly_found)
}

/// Take one process-table snapshot, verify every candidate child's `parent_pid` and causal start
/// order directly from a newly pinned handle, terminate newly confirmed descendants, and retain
/// all pinned handles in `pinned` across passes.
fn discover_and_terminate_pass(
    root: &ProcessId,
    root_start_key: Option<u64>,
    direct_child_max_start: Option<u64>,
    direct_child_max_start_key: Option<u64>,
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

    let mut queue: VecDeque<(ProcessId, Option<u64>)> = VecDeque::new();
    let mut queued: HashSet<u32> = HashSet::new();
    queue.push_back((*root, root_start_key));
    queued.insert(root.pid);
    for proc_handle in pinned.values() {
        if queued.insert(proc_handle.id.pid) {
            queue.push_back((proc_handle.id, proc_handle.start_key));
        }
    }

    let mut visited_candidates: HashSet<u32> = HashSet::new();
    let mut newly_found = 0usize;

    while let Some((parent, parent_start_key)) = queue.pop_front() {
        let Some(children) = children_by_parent.get(&parent.pid) else {
            continue;
        };
        let (max_start, max_start_key) = if parent.pid == root.pid {
            (direct_child_max_start, direct_child_max_start_key)
        } else {
            (None, None)
        };
        for &child_pid in children {
            if child_pid == own_pid || child_pid == 4 || pinned.contains_key(&child_pid) {
                continue;
            }
            if !visited_candidates.insert(child_pid) {
                continue;
            }

            match win_proc::open_pinned(child_pid, false) {
                OpenPinnedOutcome::Pinned(child_proc) => {
                    // Validate parent_pid and monotonic kernel creation order directly on the
                    // pinned handle so a recycled parent PID can never match an older unrelated
                    // process, and never touch protected system processes.
                    if child_proc.is_protected_system()
                        || !is_causally_not_older_than_parent(
                            &child_proc,
                            &parent,
                            parent_start_key,
                            max_start,
                            max_start_key,
                        )
                    {
                        continue;
                    }
                    let child_id = child_proc.id;
                    let child_key = child_proc.start_key;
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
                        queue.push_back((child_id, child_key));
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
