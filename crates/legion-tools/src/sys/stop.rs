//! Stopping one process and leaving its children running.
//!
//! `legion-os` stops whole process trees; stopping a single process is the one thing `sys.kill`
//! needs that it does not offer, so it lives here, built on the signals `sysinfo` can send on
//! every platform. Where a polite request does not exist (Windows has no terminate signal for a
//! process it did not start), the process is forced straight away.

use std::time::{Duration, Instant};

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::procid::{self, ProcessId};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, Signal, System};

/// How often a stopping process is looked at.
const POLL: Duration = Duration::from_millis(25);
/// How long a forced process is given to disappear.
const SETTLE: Duration = Duration::from_secs(2);

/// Asks the process to exit, forces it after `grace` (at once when `grace` is zero), and returns
/// its id if it is still running after that.
pub(super) async fn one(identity: ProcessId, grace: Duration) -> Result<Vec<u32>, ErrorBody> {
    if !grace.is_zero() && send(identity, Signal::Term).await? && gone_within(identity, grace).await
    {
        return Ok(Vec::new());
    }
    send(identity, Signal::Kill).await?;
    if gone_within(identity, SETTLE).await {
        Ok(Vec::new())
    } else {
        Ok(vec![identity.pid])
    }
}

/// Sends a signal to the process if it is still the one identified. Returns whether the signal
/// exists on this platform; fails when the system refuses to deliver it.
async fn send(identity: ProcessId, signal: Signal) -> Result<bool, ErrorBody> {
    let sent = tokio::task::spawn_blocking(move || {
        if !procid::is_alive(&identity) {
            return Some(true);
        }
        let mut system = System::new().ok()?;
        let pid = Pid::from_u32(identity.pid);
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing(),
        );
        match system.process(pid) {
            Some(process) => process.kill_with(signal),
            None => Some(true),
        }
    })
    .await
    .ok()
    .flatten();
    match sent {
        Some(true) => Ok(true),
        None if signal == Signal::Term => Ok(false),
        _ if !procid::is_alive(&identity) => Ok(true),
        _ => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "The system refused to stop process {}.",
                identity.pid
            ),
        )
        .with_hint(
            "It probably belongs to another account; stop it as that account or as an administrator.",
        )),
    }
}

/// Whether the process is gone before `limit` passes.
async fn gone_within(identity: ProcessId, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !procid::is_alive(&identity) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL).await;
    }
}
