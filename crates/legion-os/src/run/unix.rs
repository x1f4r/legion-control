//! Unix process-group containment and `SIGTERM` -> `SIGKILL` escalation for [`super::run`].

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop, ProcessGroup};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const POST_KILL_WAIT: Duration = Duration::from_secs(2);

pub(super) fn configure_containment(wrapped: &mut CommandWrap) {
    wrapped.wrap(ProcessGroup::leader()).wrap(KillOnDrop);
}

pub(super) fn extract_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), status.signal())
}

pub(super) async fn terminate_tree(
    child: &mut Box<dyn ChildWrapper>,
    child_pid: Option<u32>,
    grace: Duration,
) -> Option<ExitStatus> {
    let pgid = child_pid
        .and_then(|pid| i32::try_from(pid).ok())
        .map(Pid::from_raw);

    // Send SIGTERM to the entire process group first so cooperative commands can flush or clean up.
    let _ = child.signal(Signal::SIGTERM as i32);
    if let Some(group) = pgid {
        let _ = killpg(group, Signal::SIGTERM);
    }

    let grace_deadline = Instant::now() + grace;
    let mut exit_status: Option<ExitStatus> = None;

    // Wait up to `grace` for the direct child to exit.
    if let Ok(Ok(status)) = tokio::time::timeout(grace, child.wait()).await {
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
        let _ = child.start_kill();
        if let Some(group) = pgid {
            let _ = killpg(group, Signal::SIGKILL);
        }

        if exit_status.is_none()
            && let Ok(Ok(status)) = tokio::time::timeout(POST_KILL_WAIT, child.wait()).await
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
