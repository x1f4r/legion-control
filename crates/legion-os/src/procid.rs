//! Process identities, boot identifiers, and detached process-tree termination.
//!
//! Operating systems reuse process IDs—often quickly on Windows—so a PID alone cannot tell
//! whether a recorded job is still running or another process has taken its number. Every
//! [`ProcessId`] pairs the PID with the process's kernel start timestamp, and [`boot_id`]
//! identifies the current boot so records from before a reboot can be discarded immediately.

use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::{OsError, Result};

/// Tolerance applied by [`same_boot`] when comparing time-derived boot IDs (such as Windows
/// `LastBootUpTime`, which can shift by a few seconds when NTP adjusts the system clock).
const SAME_BOOT_TOLERANCE_SECS: u64 = 5;
const TICKS_PER_SEC: u64 = 10_000_000;
const SAME_BOOT_TOLERANCE_TICKS: u64 = SAME_BOOT_TOLERANCE_SECS * TICKS_PER_SEC;
const BOOT_TICKS_PREFIX: &str = "boot-ticks-";
const BOOT_SEC_PREFIX: &str = "boot-sec-";

#[cfg(unix)]
#[path = "procid/unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "procid/windows.rs"]
mod platform;

static CACHED_BOOT_ID: OnceLock<String> = OnceLock::new();

/// Unambiguous identity of a process within a single system boot.
///
/// Comparing two [`ProcessId`] values checks both `pid` and `start_time` so a recycled PID
/// never masquerades as an earlier process.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct ProcessId {
    /// Operating system process identifier.
    pub pid: u32,
    /// Kernel start timestamp for the process.
    ///
    /// On Linux this is the monotonic tick count since boot from `/proc/<pid>/stat` field 22,
    /// which is unaffected by wall-clock jumps. On macOS and Windows it is the process creation
    /// timestamp in seconds since the Unix epoch.
    pub start_time: u64,
}

impl ProcessId {
    /// Construct a [`ProcessId`] from a raw PID and start timestamp.
    pub const fn new(pid: u32, start_time: u64) -> Self {
        Self { pid, start_time }
    }

    /// Return `true` only when `observed` has the exact same PID and start timestamp as `self`.
    ///
    /// A process reusing `self.pid` at a different `start_time` returns `false`.
    pub const fn matches(&self, observed: &ProcessId) -> bool {
        self.pid == observed.pid && self.start_time == observed.start_time
    }
}

impl fmt::Display for ProcessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.pid, self.start_time)
    }
}

/// Result of attempting to terminate a process and its entire descendant tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KillTreeReport {
    /// `true` once the root process and every discovered descendant have exited.
    pub all_gone: bool,
    /// Processes that remained alive after the kill and settle phase completed.
    pub survivors: Vec<ProcessId>,
}

/// Return the [`ProcessId`] of the current process.
pub fn current() -> Result<ProcessId> {
    let pid = std::process::id();
    of_pid(pid)?.ok_or_else(|| OsError::Platform {
        detail: format!("could not read process identity for current pid {pid}"),
    })
}

/// Look up the [`ProcessId`] of a currently live (non-zombie) process by PID.
///
/// Returns `Ok(None)` if no live process currently has `pid`.
pub fn of_pid(pid: u32) -> Result<Option<ProcessId>> {
    if pid == 0 {
        return Ok(None);
    }
    of_pid_impl(pid, false)
}

/// Look up the [`ProcessId`] of `pid`, including a zombie that has not yet been reaped.
///
/// Detached launchers call this immediately after `spawn` while still holding the unreaped
/// child handle so a fast-exiting process still yields its exact start time.
#[allow(dead_code)]
pub(crate) fn of_pid_including_zombie(pid: u32) -> Result<Option<ProcessId>> {
    if pid == 0 {
        return Ok(None);
    }
    of_pid_impl(pid, true)
}

/// Return `true` if and only if the exact process named by `identity` is still alive.
///
/// Returns `false` if the process has exited, is a zombie, or its PID has been recycled by
/// another process with a different start time.
pub fn is_alive(identity: &ProcessId) -> bool {
    matches!(of_pid(identity.pid), Ok(Some(observed)) if identity.matches(&observed))
}

/// Return an identifier string that stays constant for the current boot and changes on every reboot.
///
/// - **Linux:** `/proc/sys/kernel/random/boot_id` (kernel UUID, unaffected by clock steps).
/// - **macOS:** `sysctl -n kern.bootsessionuuid` (kernel UUID, unaffected by clock steps).
/// - **Windows:** `boot-ticks-<ticks>` from `Win32_OperatingSystem.LastBootUpTime` UTC ticks
///   (single source and format; compare with [`same_boot`] so minor NTP clock steps do not look
///   like a reboot).
pub fn boot_id() -> Result<String> {
    if let Some(cached) = CACHED_BOOT_ID.get() {
        return Ok(cached.clone());
    }
    let id = read_boot_id_impl()?;
    let _ = CACHED_BOOT_ID.set(id.clone());
    Ok(id)
}

/// Compare two boot identifiers returned by [`boot_id`].
///
/// UUID-based boot IDs (Linux `boot_id` and macOS `kern.bootsessionuuid`) require exact equality.
/// Time-derived boot IDs (`boot-ticks-<ticks>` on Windows and `boot-sec-<secs>` on fallback Unix
/// platforms) allow up to 5 seconds of clock-step tolerance so routine NTP adjustments do not
/// falsely indicate a reboot.
pub fn same_boot(a: &str, b: &str) -> bool {
    let a = a.trim();
    let b = b.trim();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    if let (Some(ta), Some(tb)) = (
        parse_prefixed_u64(a, BOOT_TICKS_PREFIX),
        parse_prefixed_u64(b, BOOT_TICKS_PREFIX),
    ) {
        return ta.abs_diff(tb) <= SAME_BOOT_TOLERANCE_TICKS;
    }
    if let (Some(sa), Some(sb)) = (
        parse_prefixed_u64(a, BOOT_SEC_PREFIX),
        parse_prefixed_u64(b, BOOT_SEC_PREFIX),
    ) {
        return sa.abs_diff(sb) <= SAME_BOOT_TOLERANCE_SECS;
    }
    false
}

fn parse_prefixed_u64(value: &str, prefix: &str) -> Option<u64> {
    let rest = value.strip_prefix(prefix)?;
    if rest.is_empty() || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    rest.parse::<u64>().ok()
}

/// Stop a process and all of its descendants when the root is not a direct child of this process.
///
/// On Unix, signals both the target's process group (when the target leads its own group distinct
/// from ours) and every discovered descendant with `SIGTERM`, waits up to `grace`, and escalates
/// any survivors to `SIGKILL`. On Windows, runs `taskkill /T /F` against the root and any
/// orphaned descendants discovered in the process table. Returns only after every tracked process
/// has exited or the post-kill settle period elapses, reporting any survivors.
pub async fn kill_tree(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    if identity.pid == 0 {
        return Err(OsError::InvalidArgument {
            detail: "cannot terminate pid 0".into(),
        });
    }
    if identity.pid == std::process::id() {
        return Err(OsError::InvalidArgument {
            detail: "refusing to terminate the current process".into(),
        });
    }
    kill_tree_impl(identity, grace).await
}

#[cfg(any(unix, windows))]
fn of_pid_impl(pid: u32, include_zombie: bool) -> Result<Option<ProcessId>> {
    platform::of_pid(pid, include_zombie)
}

#[cfg(not(any(unix, windows)))]
fn of_pid_impl(_pid: u32, _include_zombie: bool) -> Result<Option<ProcessId>> {
    Err(OsError::Unsupported {
        detail: "process identity lookup is not supported on this platform".into(),
    })
}

#[cfg(any(unix, windows))]
fn read_boot_id_impl() -> Result<String> {
    platform::boot_id()
}

#[cfg(not(any(unix, windows)))]
fn read_boot_id_impl() -> Result<String> {
    Err(OsError::Unsupported {
        detail: "boot id lookup is not supported on this platform".into(),
    })
}

#[cfg(any(unix, windows))]
async fn kill_tree_impl(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    platform::kill_tree(identity, grace).await
}

#[cfg(not(any(unix, windows)))]
async fn kill_tree_impl(_identity: &ProcessId, _grace: Duration) -> Result<KillTreeReport> {
    Err(OsError::Unsupported {
        detail: "process tree termination is not supported on this platform".into(),
    })
}

/// Parse `(state, start_ticks)` from a Linux `/proc/<pid>/stat` line.
///
/// The command name in field 2 is wrapped in parentheses and may itself contain spaces and
/// parentheses, so parsing anchors at the last `)` before reading field 3 (`state`) and
/// field 22 (`starttime`).
#[allow(dead_code)]
pub(crate) fn parse_linux_proc_stat(contents: &str) -> Option<(char, u64)> {
    let close = contents.rfind(')')?;
    let rest = contents.get(close + 1..)?.trim_start();
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    // Field 3 (index 0) was consumed above; field 22 is index 19, which is 18 skips further.
    let start_ticks = fields.nth(18)?.parse::<u64>().ok()?;
    Some((state, start_ticks))
}

/// Run a short helper command synchronously with a hard timeout and return its stdout.
///
/// Used by macOS and Windows `boot_id` lookups so a stalled WMI/sysctl call cannot block a caller
/// indefinitely.
#[allow(dead_code)]
pub(crate) fn run_command_stdout_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    label: &str,
) -> Result<String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|err| OsError::from_spawn_io(label, err))?;

    let stdout_pipe = child.stdout.take();
    let reader = std::thread::spawn(move || -> String {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                let now = Instant::now();
                if now >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(OsError::TimedOut {
                        detail: format!(
                            "`{label}` did not respond within {} ms",
                            timeout.as_millis()
                        ),
                    });
                }
                let remaining = deadline.saturating_duration_since(now);
                std::thread::sleep(Duration::from_millis(20).min(remaining));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(OsError::from_io(
                    format!("failed waiting for `{label}`"),
                    err,
                ));
            }
        }
    };

    let stdout = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(OsError::Platform {
            detail: format!("`{label}` exited with status {status}"),
        });
    }
    Ok(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_distinguishes_recycled_pid_with_different_start_time() {
        let original = ProcessId::new(4242, 100_000);
        let same = ProcessId::new(4242, 100_000);
        let recycled = ProcessId::new(4242, 100_500);
        let different_pid = ProcessId::new(4243, 100_000);

        assert!(original.matches(&same));
        assert_eq!(original, same);
        assert!(!original.matches(&recycled));
        assert_ne!(original, recycled);
        assert!(!original.matches(&different_pid));
    }

    #[test]
    fn current_process_identity_is_alive_and_stable() {
        let me = current().unwrap();
        assert_eq!(me.pid, std::process::id());
        assert!(me.start_time > 0);
        assert!(is_alive(&me));

        // Same PID with a forged start time must not count as alive.
        let recycled = ProcessId::new(me.pid, me.start_time.wrapping_add(1));
        assert!(!is_alive(&recycled));

        let looked_up = of_pid(me.pid).unwrap().unwrap();
        assert!(me.matches(&looked_up));
    }

    #[test]
    fn boot_id_is_non_empty_and_consistent_across_calls() {
        let first = boot_id().unwrap();
        let second = boot_id().unwrap();
        assert!(!first.trim().is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn parse_linux_proc_stat_handles_parens_and_spaces_in_comm() {
        let line = "12345 (my (weird) proc) S 1 12345 12345 0 -1 4194304 100 0 0 0 10 5 0 0 20 0 1 0 987654321 1024 50";
        assert_eq!(parse_linux_proc_stat(line), Some(('S', 987_654_321)));

        let zombie = "99 (defunct) Z 1 99 99 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 42 0 0";
        assert_eq!(parse_linux_proc_stat(zombie), Some(('Z', 42)));

        assert_eq!(parse_linux_proc_stat("malformed"), None);
    }

    #[test]
    fn same_boot_handles_uuids_and_tolerant_time_derived_ids() {
        // UUID boot IDs (Linux boot_id and macOS kern.bootsessionuuid) require exact match.
        let uuid_a = "550e8400-e29b-41d4-a716-446655440000";
        let uuid_b = "550e8400-e29b-41d4-a716-446655440001";
        assert!(same_boot(uuid_a, uuid_a));
        assert!(!same_boot(uuid_a, uuid_b));

        // Windows LastBootUpTime ticks allow up to 5 seconds of clock-step drift.
        let base_ticks: u64 = 638_600_000_000_000_000;
        let a = format!("boot-ticks-{base_ticks}");
        let drift_3s = format!("boot-ticks-{}", base_ticks + 3 * TICKS_PER_SEC);
        let drift_5s = format!("boot-ticks-{}", base_ticks + 5 * TICKS_PER_SEC);
        let drift_over = format!("boot-ticks-{}", base_ticks + 5 * TICKS_PER_SEC + 1);
        assert!(same_boot(&a, &a));
        assert!(same_boot(&a, &drift_3s));
        assert!(same_boot(&drift_3s, &a));
        assert!(same_boot(&a, &drift_5s));
        assert!(!same_boot(&a, &drift_over));

        // Fallback Unix second-based IDs allow up to 5 seconds of drift.
        assert!(same_boot("boot-sec-1700000000", "boot-sec-1700000005"));
        assert!(!same_boot("boot-sec-1700000000", "boot-sec-1700000006"));

        // Empty, malformed, and cross-format comparisons never match.
        assert!(!same_boot("", ""));
        assert!(!same_boot(&a, uuid_a));
        assert!(!same_boot(&a, "boot-sec-1700000000"));
        assert!(!same_boot("boot-ticks-abc", "boot-ticks-abc-2"));
    }

    #[tokio::test]
    async fn kill_tree_refuses_current_process_and_zero_pid() {
        let me = current().unwrap();
        let err = kill_tree(&me, Duration::from_millis(10)).await.unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));

        let zero = ProcessId::new(0, 1);
        let err = kill_tree(&zero, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));
    }

    #[tokio::test]
    async fn kill_tree_reports_gone_for_absent_pid() {
        let me = current().unwrap();
        let absent = ProcessId::new(i32::MAX as u32, 1);
        let report = kill_tree(&absent, Duration::from_millis(10)).await.unwrap();
        assert!(report.all_gone);
        assert!(report.survivors.is_empty());
        assert!(is_alive(&me));
    }
}
