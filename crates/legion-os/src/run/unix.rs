//! Unix process-group containment and `SIGTERM` -> `SIGKILL` escalation for [`super::run`].

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop, ProcessGroup};

use crate::Result;
#[allow(unused_imports)]
use crate::sandbox::SandboxPolicy;
use crate::shell::ResolvedCommand;

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const POST_KILL_WAIT: Duration = Duration::from_secs(2);

/// Kills the child's entire process group if the `run` future is dropped before completing.
///
/// Tokio's `kill_on_drop` only signals the direct child PID; without this guard an aborted task
/// or caller-side timeout would leak grandchildren in the child's process group.
pub(super) struct TreeDropGuard {
    pgid: Option<Pid>,
    armed: bool,
}

impl TreeDropGuard {
    pub(super) fn new(
        child_pid: Option<u32>,
        _memory_limit: Option<u64>,
        _memory_headroom: u64,
    ) -> Self {
        let pgid = child_pid
            .and_then(|pid| i32::try_from(pid).ok())
            .map(Pid::from_raw);
        Self { pgid, armed: true }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

/// Holds any parent-side sandbox resources (such as a Linux Landlock ruleset fd) alive across
/// `CommandWrap::spawn` and drops them immediately afterwards.
pub(super) struct PreSpawnSandbox {
    #[cfg(target_os = "linux")]
    ffmpeg_linux: Option<crate::sandbox::PreparedFfmpegSandbox>,
}

impl PreSpawnSandbox {
    pub(super) fn release(self) {}
}

/// Prepares Unix OS-level sandbox hooks for `policy` before building `CommandWrap`.
///
/// - On Linux: prepares a Landlock ruleset + seccomp network-deny filter for [`SandboxPolicy::Ffmpeg`].
/// - On macOS: wraps [`SandboxPolicy::Ffmpeg`] with `/usr/bin/sandbox-exec`.
/// - For [`SandboxPolicy::Worker`]: the worker installs its own strict sandbox before reading input.
pub(super) fn prepare_sandbox(
    policy: Option<&SandboxPolicy>,
    resolved: &mut ResolvedCommand,
) -> Result<PreSpawnSandbox> {
    #[cfg(target_os = "linux")]
    {
        let ffmpeg_linux = match policy {
            Some(SandboxPolicy::Ffmpeg {
                ffmpeg_bin,
                input_file,
                temp_dir,
            }) => Some(crate::sandbox::prepare_ffmpeg_sandbox(
                ffmpeg_bin,
                input_file.as_deref(),
                temp_dir,
            )?),
            _ => None,
        };
        let _ = resolved;
        Ok(PreSpawnSandbox { ffmpeg_linux })
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(SandboxPolicy::Ffmpeg {
            ffmpeg_bin,
            input_file,
            temp_dir,
        }) = policy
        {
            let (wrapped_prog, wrapped_args) = crate::sandbox::wrap_ffmpeg_command(
                ffmpeg_bin,
                input_file.as_deref(),
                temp_dir,
                &resolved.args,
            )?;
            resolved.program = wrapped_prog;
            resolved.args = wrapped_args;
        }
        Ok(PreSpawnSandbox {})
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (policy, resolved);
        Ok(PreSpawnSandbox {})
    }
}

/// Applies the platform hard memory limit and any prepared Linux `pre_exec` sandbox to `cmd`
/// before `exec`.
///
/// - **Linux**: sets `RLIMIT_AS` to `memory_limit + rlimit_as_headroom` (clamped to the current
///   hard limit) so runaway allocations fail with `ENOMEM` immediately in the kernel, and installs
///   any prepared `ffmpeg` Landlock + seccomp network-deny filter before `exec`.
/// - **Other non-macOS Unix**: sets `RLIMIT_DATA` to `memory_limit`.
/// - **macOS**: skipped because macOS `dyld` maps >2 GiB of shared cache into virtual space and
///   ignores `RLIMIT_DATA`; enforcement is provided by the parent RSS watchdog.
#[allow(unused_variables)]
pub(super) fn configure_pre_exec(
    cmd: &mut tokio::process::Command,
    memory_limit: Option<u64>,
    rlimit_as_headroom: u64,
    pre_spawn: &PreSpawnSandbox,
) {
    #[cfg(target_os = "linux")]
    {
        let rlimit_target = memory_limit.map(|limit| limit.saturating_add(rlimit_as_headroom));
        let ffmpeg_hook = pre_spawn
            .ffmpeg_linux
            .as_ref()
            .map(|prepared| (prepared.landlock_raw_fd(), prepared.seccomp_filter_clone()));
        if rlimit_target.is_some() || ffmpeg_hook.is_some() {
            // SAFETY: `pre_exec` runs between `fork` and `exec` and calls only async-signal-safe
            // syscalls (`getrlimit`, `setrlimit`, `prctl`, `landlock_restrict_self`).
            #[allow(unsafe_code)]
            unsafe {
                cmd.pre_exec(move || {
                    if let Some(target) = rlimit_target {
                        let mut cur = libc::rlimit {
                            rlim_cur: 0,
                            rlim_max: 0,
                        };
                        if libc::getrlimit(libc::RLIMIT_AS, &mut cur) == 0 {
                            let capped = (target as libc::rlim_t).min(cur.rlim_max);
                            let next = libc::rlimit {
                                rlim_cur: capped,
                                rlim_max: capped,
                            };
                            let _ = libc::setrlimit(libc::RLIMIT_AS, &next);
                        }
                    }
                    if let Some((landlock_fd, ref filter)) = ffmpeg_hook {
                        crate::sandbox::PreparedFfmpegSandbox::apply_in_pre_exec(
                            landlock_fd,
                            filter,
                        )?;
                    }
                    Ok(())
                });
            }
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if let Some(limit) = memory_limit {
        // SAFETY: `pre_exec` calls only async-signal-safe `getrlimit`/`setrlimit` syscalls.
        #[allow(unsafe_code)]
        unsafe {
            cmd.pre_exec(move || {
                let mut cur = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_DATA, &mut cur) == 0 {
                    let capped = (limit as libc::rlim_t).min(cur.rlim_max);
                    let next = libc::rlimit {
                        rlim_cur: capped,
                        rlim_max: capped,
                    };
                    let _ = libc::setrlimit(libc::RLIMIT_DATA, &next);
                }
                Ok(())
            });
        }
    }
}

impl Drop for TreeDropGuard {
    fn drop(&mut self) {
        if self.armed
            && let Some(group) = self.pgid
        {
            let _ = killpg(group, Signal::SIGKILL);
        }
    }
}

pub(super) fn configure_containment(wrapped: &mut CommandWrap, _suspend_for_memory_job: bool) {
    wrapped.wrap(ProcessGroup::leader()).wrap(KillOnDrop);
}

pub(super) fn extract_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), status.signal())
}

pub(super) async fn cleanup_after_direct_exit(
    _child: &mut Box<dyn ChildWrapper>,
    child_pid: Option<u32>,
    grace: Duration,
) {
    let Some(group) = child_pid
        .and_then(|pid| i32::try_from(pid).ok())
        .map(Pid::from_raw)
    else {
        return;
    };

    // Fast path: when no background descendants remain in the process group, avoid signaling or waiting.
    if matches!(killpg(group, None), Err(Errno::ESRCH)) {
        return;
    }

    let _ = killpg(group, Signal::SIGTERM);
    let grace_deadline = Instant::now() + grace;
    while Instant::now() < grace_deadline {
        if matches!(killpg(group, None), Err(Errno::ESRCH)) {
            return;
        }
        let remaining = grace_deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
    }

    if !matches!(killpg(group, None), Err(Errno::ESRCH)) {
        let _ = killpg(group, Signal::SIGKILL);
        let settle_deadline = Instant::now() + POST_KILL_WAIT;
        while Instant::now() < settle_deadline {
            if matches!(killpg(group, None), Err(Errno::ESRCH)) {
                break;
            }
            let remaining = settle_deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }
}

pub(super) async fn terminate_tree(
    child: &mut Box<dyn ChildWrapper>,
    child_pid: Option<u32>,
    grace: Duration,
) -> Option<ExitStatus> {
    let pgid = child_pid
        .and_then(|pid| i32::try_from(pid).ok())
        .map(Pid::from_raw);

    // Send SIGTERM once to the process group so cooperative commands that treat a second signal
    // as an immediate force-quit still run their cleanup handler.
    if let Some(group) = pgid {
        let _ = killpg(group, Signal::SIGTERM);
    } else {
        let _ = child.signal(Signal::SIGTERM as i32);
    }

    let grace_deadline = Instant::now() + grace;
    let mut exit_status: Option<ExitStatus> = None;

    // Wait up to `grace` for the direct child to exit.
    if let Ok(Ok(status)) = tokio::time::timeout(grace, child.inner_mut().wait()).await {
        exit_status = Some(status);
    }

    // Even if the direct child exited on SIGTERM, grandchildren in the same process group might
    // still be shutting down (or ignoring SIGTERM). Wait until the whole group clears or `grace`
    // expires before escalating to SIGKILL.
    if let Some(group) = pgid {
        while Instant::now() < grace_deadline {
            if matches!(killpg(group, None), Err(Errno::ESRCH)) {
                break;
            }
            let remaining = grace_deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
        }
    }

    let group_still_alive = pgid
        .map(|group| !matches!(killpg(group, None), Err(Errno::ESRCH)))
        .unwrap_or(false);

    if exit_status.is_none() || group_still_alive {
        if let Some(group) = pgid {
            let _ = killpg(group, Signal::SIGKILL);
        } else {
            let _ = child.start_kill();
        }

        if exit_status.is_none()
            && let Ok(Ok(status)) =
                tokio::time::timeout(POST_KILL_WAIT, child.inner_mut().wait()).await
        {
            exit_status = Some(status);
        }

        if let Some(group) = pgid {
            let settle_deadline = Instant::now() + POST_KILL_WAIT;
            while Instant::now() < settle_deadline {
                if matches!(killpg(group, None), Err(Errno::ESRCH)) {
                    break;
                }
                let remaining = settle_deadline.saturating_duration_since(Instant::now());
                tokio::time::sleep(POLL_INTERVAL.min(remaining)).await;
            }
        }
    }

    exit_status
}
