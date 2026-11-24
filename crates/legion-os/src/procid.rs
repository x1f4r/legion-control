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

use serde::{Deserialize, Serialize};

use crate::{OsError, Result};

/// Tolerance applied by [`same_boot`] when comparing time-derived boot IDs (such as Windows
/// `LastBootUpTime`, which can shift by a few seconds when NTP adjusts the system clock).
const SAME_BOOT_TOLERANCE_SECS: u64 = 5;
const TICKS_PER_SEC: u64 = 10_000_000;
const SAME_BOOT_TOLERANCE_TICKS: u64 = SAME_BOOT_TOLERANCE_SECS * TICKS_PER_SEC;
const BOOT_TICKS_PREFIX: &str = "boot-ticks-";
const BOOT_SEC_PREFIX: &str = "boot-sec-";

/// Generous backward wall-clock step tolerance (24 hours) applied when comparing parent and child
/// creation times on platforms whose kernel process start timestamps are wall-clock based
/// (Windows `FILETIME` and macOS `proc_bsdinfo` epoch time).
///
/// Dual-boot machines and NTP syncs can step the system wall clock backward (up to ~14 hours for
/// UTC/local RTC timezone mismatches); without tolerance, a real child started after a backward
/// step looks older than its parent and would be skipped. Linux uses monotonic boot-relative
/// jiffies and needs zero backward tolerance.
#[allow(dead_code)]
pub(crate) const WALL_CLOCK_BACKWARD_TOLERANCE_SECS: u64 = 86_400;
#[allow(dead_code)]
pub(crate) const WALL_CLOCK_BACKWARD_TOLERANCE_MICROS: u64 =
    WALL_CLOCK_BACKWARD_TOLERANCE_SECS * 1_000_000;
#[allow(dead_code)]
pub(crate) const WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME: u64 =
    WALL_CLOCK_BACKWARD_TOLERANCE_SECS * TICKS_PER_SEC;

/// Return `true` when `child_start` is no more than `backward_tolerance` units earlier than
/// `parent_start`.
#[allow(dead_code)]
pub(crate) const fn child_not_older_than_parent(
    child_start: u64,
    parent_start: u64,
    backward_tolerance: u64,
) -> bool {
    child_start.saturating_add(backward_tolerance) >= parent_start
}

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
/// never masquerades as an earlier process. The serialized representation (`{ pid, start_time }`)
/// is stable across releases for persistence in job records.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ProcessId {
    /// Operating system process identifier.
    pub pid: u32,
    /// Full-precision kernel start timestamp for the process.
    ///
    /// Units per platform (never compared across machines or operating systems):
    /// - **Linux:** clock ticks (jiffies) since boot from `/proc/<pid>/stat` field 22, monotonic
    ///   and immune to wall-clock adjustments.
    /// - **macOS:** microseconds since the Unix epoch from `proc_bsdinfo`
    ///   (`pbi_start_tvsec * 1_000_000 + pbi_start_tvusec`); wall-clock based, so parent-before-child
    ///   ordering allows up to 24 hours of backward clock-step tolerance.
    /// - **Windows:** 100-nanosecond `FILETIME` ticks since 1601-01-01 UTC from
    ///   `GetProcessTimes` (`CreationTime`); wall-clock based, so parent-before-child ordering
    ///   allows up to 24 hours of backward clock-step tolerance.
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
    /// `true` once the root process and every discovered descendant have exited and no
    /// unverified orphan candidates remain.
    pub all_gone: bool,
    /// Verified target processes that remained alive after the kill and settle phase completed.
    pub survivors: Vec<ProcessId>,
    /// Candidate orphan processes discovered after an exited root that could not be proved to
    /// belong to the tree (for example because no detach marker was available or their environment
    /// could not be inspected) and were therefore intentionally left running.
    pub unverified_survivors: Vec<ProcessId>,
}

/// Outcome of checking whether a live process's environment carries a specific `KEY=VALUE` marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub(crate) enum EnvMarkerCheck {
    /// The process environment was read and contains the exact `KEY=VALUE` entry.
    Matched,
    /// The process environment was read completely and does not contain the entry.
    NotMatched,
    /// The process environment could not be read (permission denied, unsupported OS, or truncated),
    /// so ownership cannot be proved or disproved.
    Unverifiable,
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
/// - **Windows:** `boot-win-<unbiased_ticks>` computed from
///   `NtQuerySystemInformation(SystemTimeOfDayInformation)` as `BootTime - BootTimeBias`, which
///   stays invariant across NTP and dual-boot wall-clock steps of any size. Note that Windows Fast
///   Startup (hybrid shutdown) hibernates the kernel session rather than performing a cold boot,
///   so the kernel boot timestamp—and therefore this ID—persists across a Fast Startup cycle.
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
/// Clock-step-invariant boot IDs (Linux `boot_id`, macOS `kern.bootsessionuuid`, and Windows
/// `boot-win-<unbiased_ticks>`) require exact equality. Legacy or fallback time-derived boot IDs
/// (`boot-ticks-<ticks>` and `boot-sec-<secs>`) allow up to 5 seconds of clock-step tolerance so
/// routine NTP adjustments do not falsely indicate a reboot.
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

/// Return `Some(reason)` if `identity` refers to a protected process that callers should refuse to
/// terminate, or `None` otherwise.
///
/// Always protects:
/// - PID 0 and the current process
/// - Any ancestor of the current process (walking the parent chain up to the root)
/// - **Unix:** PID 1 (`init` / `systemd` / `launchd`)
/// - **Windows:** PID 4 (`System`), any process marked critical by the kernel
///   (`IsProcessCritical`), and core session processes (`csrss`, `wininit`, `smss`, `lsass`,
///   `services`, `winlogon`)
pub fn is_protected(identity: &ProcessId) -> Option<&'static str> {
    if identity.pid == 0 {
        return Some("kernel / idle process (pid 0)");
    }
    if identity.pid == std::process::id() {
        return Some("current process");
    }
    is_protected_impl(identity)
}

/// Stop a single verified process without signaling its process group or walking descendants.
///
/// Returns `Ok(true)` once the exact process identified by `identity` is no longer alive
/// (including when it has already exited or its PID has been reused by another process), and
/// `Ok(false)` if it remained alive past the post-kill settle timeout.
///
/// - **Unix:** sends `SIGTERM`, polls up to `grace`, and escalates to `SIGKILL`; when `force` is
///   `true`, sends `SIGKILL` immediately without waiting for `grace`.
/// - **Windows:** opens `identity.pid` with `PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION`,
///   verifies its exact 100-ns `FILETIME` creation time while the held handle pins the PID against
///   reuse, and calls `TerminateProcess` directly through that handle (both `force = false` and
///   `force = true` terminate immediately via the pinned handle).
pub async fn kill_process(identity: &ProcessId, force: bool, grace: Duration) -> Result<bool> {
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
    kill_process_impl(identity, force, grace).await
}

/// Stop a process and all of its descendants when the root is not a direct child of this process.
///
/// While the verified root process is still alive:
/// - **Unix:** signals the root (via `pidfd_send_signal` on Linux), its isolated process group
///   when it leads one, and every discovered descendant with `SIGTERM`, waits up to `grace`, and
///   escalates any survivors to `SIGKILL`.
/// - **Windows:** pins the root's handle (preventing PID reuse across bounded re-snapshot waves),
///   walks live descendants while verifying each candidate's `parent_pid` and 100-ns `FILETIME`
///   creation time through an open handle, and calls `TerminateProcess` through those handles.
///
/// Once the root process has already exited, a plain [`kill_tree`] call has no detach marker to
/// prove ownership across PID reuse, so it **does not** terminate processes solely by session,
/// process group, or `parent_pid`; instead it leaves those candidates untouched and reports them
/// in [`KillTreeReport::unverified_survivors`] (with `all_gone == false`). For detached processes
/// launched by [`crate::detach::spawn_detached`], use [`crate::detach::stop_detached`], which uses
/// the recorded systemd scope, Windows Job Object, or `LEGION_DETACH_ID` environment marker.
pub async fn kill_tree(identity: &ProcessId, grace: Duration) -> Result<KillTreeReport> {
    kill_tree_with_marker(identity, None, grace).await
}

/// Internal variant of [`kill_tree`] that accepts an optional [`crate::detach::DETACH_MARKER_ENV`]
/// value so detached orphans can be proved by environment marker after the root has exited.
pub(crate) async fn kill_tree_with_marker(
    identity: &ProcessId,
    marker: Option<&str>,
    grace: Duration,
) -> Result<KillTreeReport> {
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
    kill_tree_impl(identity, marker, grace).await
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

/// Return `true` while `identity` itself is alive, or—when `marker` is provided—while any live
/// process carries `LEGION_DETACH_ID=<marker>` in its environment.
///
/// Once the root process has exited and no `marker` is available, returns `Ok(false)` rather than
/// trusting unverified session, process-group, or `parent_pid` matches across PID reuse. Runs all
/// OS queries off the async executor via `spawn_blocking` and performs no writes or signaling.
#[allow(dead_code)]
pub(crate) async fn tree_is_active(identity: &ProcessId) -> Result<bool> {
    tree_is_active_with_marker(identity, None).await
}

/// Internal variant of [`tree_is_active`] that proves surviving orphans after root exit via an
/// optional [`crate::detach::DETACH_MARKER_ENV`] value.
pub(crate) async fn tree_is_active_with_marker(
    identity: &ProcessId,
    marker: Option<&str>,
) -> Result<bool> {
    if identity.pid == 0 {
        return Ok(false);
    }
    tree_is_active_impl(identity, marker).await
}

#[cfg(any(unix, windows))]
async fn tree_is_active_impl(identity: &ProcessId, marker: Option<&str>) -> Result<bool> {
    platform::tree_is_active(identity, marker).await
}

#[cfg(not(any(unix, windows)))]
async fn tree_is_active_impl(_identity: &ProcessId, _marker: Option<&str>) -> Result<bool> {
    Err(OsError::Unsupported {
        detail: "process tree liveness check is not supported on this platform".into(),
    })
}

#[cfg(any(unix, windows))]
fn is_protected_impl(identity: &ProcessId) -> Option<&'static str> {
    platform::is_protected(identity)
}

#[cfg(not(any(unix, windows)))]
fn is_protected_impl(_identity: &ProcessId) -> Option<&'static str> {
    None
}

#[cfg(any(unix, windows))]
async fn kill_process_impl(identity: &ProcessId, force: bool, grace: Duration) -> Result<bool> {
    platform::kill_process(identity, force, grace).await
}

#[cfg(not(any(unix, windows)))]
async fn kill_process_impl(_identity: &ProcessId, _force: bool, _grace: Duration) -> Result<bool> {
    Err(OsError::Unsupported {
        detail: "single-process termination is not supported on this platform".into(),
    })
}

#[cfg(any(unix, windows))]
async fn kill_tree_impl(
    identity: &ProcessId,
    marker: Option<&str>,
    grace: Duration,
) -> Result<KillTreeReport> {
    platform::kill_tree(identity, marker, grace).await
}

#[cfg(not(any(unix, windows)))]
async fn kill_tree_impl(
    _identity: &ProcessId,
    _marker: Option<&str>,
    _grace: Duration,
) -> Result<KillTreeReport> {
    Err(OsError::Unsupported {
        detail: "process tree termination is not supported on this platform".into(),
    })
}

/// Return `true` when a NUL-separated Unix environment block (such as Linux `/proc/<pid>/environ`)
/// contains the exact `key=expected` entry.
#[allow(dead_code)]
pub(crate) fn environ_bytes_has_marker(bytes: &[u8], key: &str, expected: &str) -> bool {
    let key_bytes = key.as_bytes();
    let expected_bytes = expected.as_bytes();
    for entry in bytes.split(|&b| b == 0) {
        if let Some(rest) = entry.strip_prefix(key_bytes)
            && rest.first() == Some(&b'=')
            && &rest[1..] == expected_bytes
        {
            return true;
        }
    }
    false
}

/// Parse a macOS `sysctl([CTL_KERN, KERN_PROCARGS2, pid])` buffer and report whether the process's
/// environment block contains `key=expected`.
///
/// Returns `None` if `buf` is malformed or truncated before the environment section begins.
#[allow(dead_code)]
pub(crate) fn parse_macos_procargs2_env_has_marker(
    buf: &[u8],
    key: &str,
    expected: &str,
) -> Option<bool> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc < 0 {
        return None;
    }
    let mut rest = &buf[4..];

    // Skip NUL-terminated `exec_path`.
    let exec_end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[exec_end + 1..];

    // Skip NUL alignment padding between `exec_path` and `argv[0]`.
    while rest.first() == Some(&0) {
        rest = &rest[1..];
    }

    // Advance past exactly `argc` NUL-terminated argv entries (preserving empty `""` arguments by
    // advancing past a single NUL per argument).
    for _ in 0..argc {
        let arg_end = rest.iter().position(|&b| b == 0)?;
        rest = &rest[arg_end + 1..];
    }

    let key_bytes = key.as_bytes();
    let expected_bytes = expected.as_bytes();
    while !rest.is_empty() {
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        let entry = &rest[..end];
        if entry.is_empty() {
            // Double-NUL terminates `envp` before the `apple[]` auxiliary vector.
            break;
        }
        if let Some(val) = entry.strip_prefix(key_bytes)
            && val.first() == Some(&b'=')
            && &val[1..] == expected_bytes
        {
            return Some(true);
        }
        rest = if end < rest.len() {
            &rest[end + 1..]
        } else {
            &[]
        };
    }

    Some(false)
}

/// Check a Windows UTF-16 NUL-separated PEB environment block for `key=expected` (ASCII
/// case-insensitive key match, exact value match).
///
/// When `truncated` is `true` and the marker was not found before the buffer ended without seeing
/// the double-NUL terminator, returns [`EnvMarkerCheck::Unverifiable`] rather than falsely
/// claiming [`EnvMarkerCheck::NotMatched`].
#[allow(dead_code)]
pub(crate) fn windows_env_utf16_has_marker(
    units: &[u16],
    key: &str,
    expected: &str,
    truncated: bool,
) -> EnvMarkerCheck {
    let key_u16: Vec<u16> = key.encode_utf16().collect();
    let expected_u16: Vec<u16> = expected.encode_utf16().collect();

    let mut idx = 0usize;
    let mut saw_terminator = false;
    while idx < units.len() {
        let Some(rel_end) = units[idx..].iter().position(|&u| u == 0) else {
            break;
        };
        if rel_end == 0 {
            saw_terminator = true;
            break;
        }
        let entry = &units[idx..idx + rel_end];
        if let Some(eq_pos) = entry.iter().position(|&u| u == b'=' as u16) {
            let entry_key = &entry[..eq_pos];
            let entry_val = &entry[eq_pos + 1..];
            if entry_key.len() == key_u16.len()
                && entry_key
                    .iter()
                    .zip(key_u16.iter())
                    .all(|(&a, &b)| ascii_utf16_eq_ignore_case(a, b))
                && entry_val == expected_u16.as_slice()
            {
                return EnvMarkerCheck::Matched;
            }
        }
        idx += rel_end + 1;
    }

    if saw_terminator || !truncated {
        EnvMarkerCheck::NotMatched
    } else {
        EnvMarkerCheck::Unverifiable
    }
}

#[allow(dead_code)]
const fn ascii_utf16_eq_ignore_case(a: u16, b: u16) -> bool {
    let na = if a >= b'A' as u16 && a <= b'Z' as u16 {
        a + 32
    } else {
        a
    };
    let nb = if b >= b'A' as u16 && b <= b'Z' as u16 {
        b + 32
    } else {
        b
    };
    na == nb
}

/// Recover the clock-step-invariant Windows boot timestamp from `SYSTEM_TIMEOFDAY_INFORMATION`'s
/// `BootTime` and `BootTimeBias` fields.
///
/// `NtSetSystemTime` applies the same signed delta to both `KeBootTime` and `KeBootTimeBias`
/// (with two's-complement wrap on `u64` for backward steps), so wrapping subtraction cancels any
/// wall-clock adjustment of arbitrary size.
#[allow(dead_code)]
pub(crate) const fn unbiased_windows_boot_ticks(boot_time: i64, boot_time_bias: u64) -> u64 {
    (boot_time as u64).wrapping_sub(boot_time_bias)
}

/// Return `true` when `image_path` names one of the Windows core session executables that must
/// never be terminated (`csrss`, `wininit`, `smss`, `lsass`, `services`, `winlogon`).
#[allow(dead_code)]
pub(crate) fn is_windows_core_session_image(image_path: &str) -> bool {
    let file_name = image_path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(image_path)
        .trim();
    let lower = file_name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(
        stem,
        "csrss" | "wininit" | "smss" | "lsass" | "services" | "winlogon"
    )
}

/// Parse `(state, ppid, start_ticks)` from a Linux `/proc/<pid>/stat` line.
///
/// The command name in field 2 is wrapped in parentheses and may itself contain spaces and
/// parentheses, so parsing anchors at the last `)` before reading field 3 (`state`), field 4
/// (`ppid`), and field 22 (`starttime`).
#[allow(dead_code)]
pub(crate) fn parse_linux_proc_stat_full(contents: &str) -> Option<(char, u32, u64)> {
    let close = contents.rfind(')')?;
    let rest = contents.get(close + 1..)?.trim_start();
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse::<u32>().ok()?;
    // Fields 3 and 4 (indices 0 and 1) were consumed above; field 22 is index 19, which is 17 skips further.
    let start_ticks = fields.nth(17)?.parse::<u64>().ok()?;
    Some((state, ppid, start_ticks))
}

/// Parse `(state, start_ticks)` from a Linux `/proc/<pid>/stat` line.
#[allow(dead_code)]
pub(crate) fn parse_linux_proc_stat(contents: &str) -> Option<(char, u64)> {
    let (state, _ppid, start_ticks) = parse_linux_proc_stat_full(contents)?;
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
    fn process_id_serializes_with_stable_schema() {
        let id = ProcessId::new(1234, 133_700_000_000_000_001);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#"{"pid":1234,"start_time":133700000000000001}"#);
        let decoded: ProcessId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, id);
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
        let line = "12345 (my (weird) proc) S 777 12345 12345 0 -1 4194304 100 0 0 0 10 5 0 0 20 0 1 0 987654321 1024 50";
        assert_eq!(parse_linux_proc_stat(line), Some(('S', 987_654_321)));
        assert_eq!(
            parse_linux_proc_stat_full(line),
            Some(('S', 777, 987_654_321))
        );

        let zombie = "99 (defunct) Z 1 99 99 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 42 0 0";
        assert_eq!(parse_linux_proc_stat(zombie), Some(('Z', 42)));

        assert_eq!(parse_linux_proc_stat("malformed"), None);
    }

    #[test]
    fn is_protected_identifies_self_ancestors_and_core_system_processes() {
        let me = current().unwrap();
        assert!(is_protected(&me).is_some());
        assert!(is_protected(&ProcessId::new(0, 1)).is_some());

        #[cfg(unix)]
        {
            assert!(is_protected(&ProcessId::new(1, 1)).is_some());
            // Walk up to our immediate parent and confirm it is protected when alive.
            let ppid = nix::unistd::getppid().as_raw() as u32;
            if let Ok(Some(parent_id)) = of_pid(ppid) {
                assert!(
                    is_protected(&parent_id).is_some(),
                    "parent process {parent_id} should be protected"
                );
            }
        }

        // Non-existent high PID is not protected.
        assert!(is_protected(&ProcessId::new(i32::MAX as u32, 1)).is_none());

        // Windows core session image matcher (platform-independent helper).
        for path in [
            r"C:\Windows\System32\csrss.exe",
            r"C:\Windows\System32\WININIT.EXE",
            "smss.exe",
            r"\SystemRoot\System32\lsass.exe",
            "services.exe",
            "winlogon",
        ] {
            assert!(is_windows_core_session_image(path), "expected {path}");
        }
        for path in [
            r"C:\Users\me\worker.exe",
            "lsass_helper.exe",
            "my-services.exe",
            "",
        ] {
            assert!(!is_windows_core_session_image(path), "unexpected {path}");
        }
    }

    #[test]
    fn same_boot_handles_uuids_and_tolerant_time_derived_ids() {
        // UUID boot IDs (Linux boot_id and macOS kern.bootsessionuuid) require exact match.
        let uuid_a = "550e8400-e29b-41d4-a716-446655440000";
        let uuid_b = "550e8400-e29b-41d4-a716-446655440001";
        assert!(same_boot(uuid_a, uuid_a));
        assert!(!same_boot(uuid_a, uuid_b));

        // Windows unbiased boot IDs are clock-step-invariant and require exact equality.
        let win_a = "boot-win-133700000000000000";
        let win_b = "boot-win-133700000000000001";
        assert!(same_boot(win_a, win_a));
        assert!(!same_boot(win_a, win_b));

        // Legacy Windows LastBootUpTime ticks allow up to 5 seconds of clock-step drift.
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

    #[test]
    fn child_not_older_than_parent_respects_platform_clock_tolerance() {
        // Monotonic (Linux): exact ordering only.
        assert!(child_not_older_than_parent(100, 100, 0));
        assert!(child_not_older_than_parent(101, 100, 0));
        assert!(!child_not_older_than_parent(99, 100, 0));

        // Wall-clock (macOS / Windows): tolerates backward steps up to 24 hours (e.g. dual-boot
        // RTC timezone step), while still rejecting much older unrelated processes.
        let parent_ft: u64 = 133_700_000_000_000_000;
        let two_hours_ft = 2 * 3_600 * TICKS_PER_SEC;
        let twenty_five_hours_ft = 25 * 3_600 * TICKS_PER_SEC;
        assert!(child_not_older_than_parent(
            parent_ft - two_hours_ft,
            parent_ft,
            WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME
        ));
        assert!(child_not_older_than_parent(
            parent_ft - WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME,
            parent_ft,
            WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME
        ));
        assert!(!child_not_older_than_parent(
            parent_ft - twenty_five_hours_ft,
            parent_ft,
            WALL_CLOCK_BACKWARD_TOLERANCE_FILETIME
        ));
    }

    #[test]
    fn unbiased_windows_boot_ticks_cancels_positive_and_negative_clock_steps() {
        let initial_boot: i64 = 133_700_000_000_000_000;
        let base = unbiased_windows_boot_ticks(initial_boot, 0);
        assert_eq!(base, initial_boot as u64);

        // +2 hour dual-boot RTC jump (both BootTime and BootTimeBias increase by +7200s).
        let plus_2h: i64 = 7_200 * TICKS_PER_SEC as i64;
        assert_eq!(
            unbiased_windows_boot_ticks(initial_boot + plus_2h, plus_2h as u64),
            base
        );

        // -2 hour backward clock step (BootTime decreases, BootTimeBias wraps in two's complement).
        let minus_2h: i64 = -7_200 * TICKS_PER_SEC as i64;
        assert_eq!(
            unbiased_windows_boot_ticks(initial_boot + minus_2h, minus_2h as u64),
            base
        );
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
    async fn kill_process_refuses_current_process_and_zero_pid_and_handles_absent() {
        let me = current().unwrap();
        let err = kill_process(&me, false, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));

        let zero = ProcessId::new(0, 1);
        let err = kill_process(&zero, true, Duration::from_millis(10))
            .await
            .unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));

        let absent = ProcessId::new(i32::MAX as u32, 1);
        let gone = kill_process(&absent, false, Duration::from_millis(10))
            .await
            .unwrap();
        assert!(gone);
        assert!(is_alive(&me));
    }

    #[test]
    fn env_marker_parsers_handle_linux_macos_and_windows_layouts() {
        // Linux `/proc/<pid>/environ` NUL-separated bytes.
        let linux_env = b"PATH=/usr/bin\0LEGION_DETACH_ID=abc-123\0HOME=/home/me\0";
        assert!(environ_bytes_has_marker(
            linux_env,
            "LEGION_DETACH_ID",
            "abc-123"
        ));
        assert!(!environ_bytes_has_marker(
            linux_env,
            "LEGION_DETACH_ID",
            "abc-12"
        ));
        assert!(!environ_bytes_has_marker(
            linux_env,
            "LEGION_DETACH_ID",
            "abc-1234"
        ));
        assert!(!environ_bytes_has_marker(
            b"",
            "LEGION_DETACH_ID",
            "abc-123"
        ));

        // macOS `KERN_PROCARGS2`: argc + exec_path + NUL padding + argc args (including empty "")
        // + NUL-separated envp + double-NUL + apple[].
        let mut procargs = Vec::new();
        procargs.extend_from_slice(&3i32.to_ne_bytes());
        procargs.extend_from_slice(b"/usr/local/bin/worker\0\0\0\0");
        procargs.extend_from_slice(b"worker\0\0--flag\0");
        procargs.extend_from_slice(b"FOO=bar\0LEGION_DETACH_ID=mac-99\0\0executable_path=/bin\0");
        assert_eq!(
            parse_macos_procargs2_env_has_marker(&procargs, "LEGION_DETACH_ID", "mac-99"),
            Some(true)
        );
        assert_eq!(
            parse_macos_procargs2_env_has_marker(&procargs, "LEGION_DETACH_ID", "other"),
            Some(false)
        );
        // Must not mistake an argv token looking like `LEGION_DETACH_ID=...` for an env var.
        let mut argv_spoof = Vec::new();
        argv_spoof.extend_from_slice(&2i32.to_ne_bytes());
        argv_spoof.extend_from_slice(b"/bin/echo\0\0\0");
        argv_spoof.extend_from_slice(b"echo\0LEGION_DETACH_ID=spoof\0");
        argv_spoof.extend_from_slice(b"HOME=/Users/me\0\0");
        assert_eq!(
            parse_macos_procargs2_env_has_marker(&argv_spoof, "LEGION_DETACH_ID", "spoof"),
            Some(false)
        );
        // Truncated/malformed buffer returns `None` (unverifiable).
        assert_eq!(
            parse_macos_procargs2_env_has_marker(&[1, 2], "LEGION_DETACH_ID", "mac-99"),
            None
        );

        // Windows UTF-16 NUL-separated PEB environment block (case-insensitive key, exact value).
        let win_block: Vec<u16> = "PATH=C:\\Windows\0legion_detach_id=Win-42\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(
            windows_env_utf16_has_marker(&win_block, "LEGION_DETACH_ID", "Win-42", false),
            EnvMarkerCheck::Matched
        );
        assert_eq!(
            windows_env_utf16_has_marker(&win_block, "LEGION_DETACH_ID", "win-42", false),
            EnvMarkerCheck::NotMatched
        );
        let partial_no_term: Vec<u16> = "PATH=C:\\Windows\0FOO=bar\0".encode_utf16().collect();
        assert_eq!(
            windows_env_utf16_has_marker(&partial_no_term, "LEGION_DETACH_ID", "Win-42", true),
            EnvMarkerCheck::Unverifiable
        );
        assert_eq!(
            windows_env_utf16_has_marker(&partial_no_term, "LEGION_DETACH_ID", "Win-42", false),
            EnvMarkerCheck::NotMatched
        );
    }

    #[tokio::test]
    async fn kill_tree_reports_gone_for_absent_pid() {
        let me = current().unwrap();
        let absent = ProcessId::new(i32::MAX as u32, 1);
        let report = kill_tree(&absent, Duration::from_millis(10)).await.unwrap();
        assert!(report.all_gone);
        assert!(report.survivors.is_empty());
        assert!(report.unverified_survivors.is_empty());
        assert!(is_alive(&me));
    }
}
