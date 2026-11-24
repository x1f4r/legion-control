//! Windows Job Object containment for [`super::run`].
//!
//! Each child is assigned to a dedicated Windows Job Object with `KILL_ON_JOB_CLOSE`. Calling
//! `start_kill()` invokes `TerminateJobObject`, stopping the direct child and every descendant
//! process in the job atomically.

use std::process::ExitStatus;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

const POST_KILL_WAIT: Duration = Duration::from_secs(2);

/// No-op on Windows because `JobObject` with `KillOnDrop` sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
/// so dropping the wrapped child closes the Job handle and terminates all processes in the job.
pub(super) struct TreeDropGuard;

impl TreeDropGuard {
    pub(super) fn new(_child_pid: Option<u32>) -> Self {
        Self
    }

    pub(super) fn disarm(&mut self) {}
}

pub(super) fn configure_containment(wrapped: &mut CommandWrap) {
    wrapped
        .wrap(CreationFlags(CREATE_NO_WINDOW))
        .wrap(JobObject)
        .wrap(KillOnDrop);
}

pub(super) fn extract_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), None)
}

pub(super) async fn cleanup_after_direct_exit(
    child: &mut Box<dyn ChildWrapper>,
    _child_pid: Option<u32>,
    _grace: Duration,
) {
    // Terminate any grandchildren remaining in the Job Object once the direct child exits, then
    // drain the Job Object completion port within a bounded wait.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(POST_KILL_WAIT, child.wait()).await;
}

pub(super) async fn terminate_tree(
    child: &mut Box<dyn ChildWrapper>,
    _child_pid: Option<u32>,
    grace: Duration,
) -> Option<ExitStatus> {
    let _ = child.start_kill();
    let wait_budget = POST_KILL_WAIT.max(grace.min(Duration::from_secs(5)));
    tokio::time::timeout(wait_budget, child.wait())
        .await
        .ok()
        .and_then(std::result::Result::ok)
}
