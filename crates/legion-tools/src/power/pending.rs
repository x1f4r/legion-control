//! One power action at a time: the node-wide power lock, and the record of the action handed over.
//!
//! A sleep handed to a delayed runner is not over when the call that asked for it returns; a
//! second sleep a second later would put the machine back to sleep the moment it woke (its
//! runner's timer stands still while the machine sleeps), and a second restart could re-arm the
//! firmware for another system after the first one read its setting back. So power calls exclude
//! each other from before their busy gate until the action has run.
//!
//! A kernel lock cannot pass from the node to the runner it starts (files are opened
//! close-on-exec, and a Windows lock belongs to its handle), so there are two, both kernel locks
//! in `state/power` that a dead holder releases by dying, so nothing can stay stuck:
//!
//! - `call.lock`, held by a power call from before its busy gate until it has handed its action
//!   over or failed;
//! - `runner.lock`, held by the delayed runner from before its delay until its command has run.
//!
//! A call releases `call.lock` only once it has seen the runner holding `runner.lock` (or the
//! runner has said it took over), so a second call always finds one of the two held. The runner
//! reads what to run from `pending.json`, which the call writes, and acts only while that record
//! is still its own and not given up. A restart Windows times itself has no runner: its record
//! stands for it until the delay has passed, or the machine has booted since.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::lock::{FileLock, LockMode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::plan::Action;

/// How long a restart Windows times itself counts as pending after its delay, while Windows shuts
/// down.
const SELF_TIMED_GRACE: Duration = Duration::from_secs(120);

/// `state/power`: the locks, the pending record and the runner's log.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.state_dir().join("power")
}

/// The action a call handed over, as the runner reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// Unique to this hand-over; a runner acts only on its own.
    pub id: String,
    /// What it does.
    pub action: Action,
    /// The boot target a restart goes into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into: Option<String>,
    /// The command.
    pub argv: Vec<String>,
    /// Seconds between the hand-over and the command.
    pub delay_s: u32,
    /// When it was handed over, RFC 3339.
    pub since: String,
    /// The boot it was handed over in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// Whether a delayed runner carries it out; otherwise the operating system's own timer does.
    pub runner: bool,
    /// When the runner took it over, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<String>,
    /// For a restart into another system: the command that clears the firmware's next boot
    /// again, which the runner runs when the restart command fails, so the next restart, whatever
    /// causes it, does not land in that system unasked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disarm: Option<Vec<String>>,
    /// Whether the call gave up on it, so a runner that starts late must not act.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub abandoned: bool,
}

/// How a handed-over action ended, as its runner wrote it to `last.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// The hand-over's id.
    pub id: String,
    /// What it did.
    pub action: Action,
    /// The boot target a restart went into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into: Option<String>,
    /// The command.
    pub argv: Vec<String>,
    /// When it was handed over, RFC 3339.
    pub since: String,
    /// When the command ended, RFC 3339.
    pub ran_at: String,
    /// Its exit status; absent when it did not exit by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Whether it was stopped for taking too long.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    /// The end of what it wrote to standard error.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub said: String,
    /// The firmware's next boot cleared again after a failed restart, when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disarmed: Option<Disarmed>,
}

/// The clearing of the firmware's next boot after a failed restart into another system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disarmed {
    /// The command.
    pub argv: Vec<String>,
    /// Its exit status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The end of what it wrote to standard error.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub said: String,
}

/// The last handed-over action's outcome, when one has run.
pub fn last(paths: &Paths) -> Option<Outcome> {
    let bytes = std::fs::read(dir(paths).join("last.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Records how a handed-over action ended.
pub fn write_outcome(dir: &Path, outcome: &Outcome) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(outcome).map_err(|error| error.to_string())?;
    legion_core::config::write_atomically(&dir.join("last.json"), &text)
        .map_err(|error| error.to_string())
}

/// What a running call is doing, for the message a second call gets.
#[derive(Debug, Serialize, Deserialize)]
struct Current {
    action: Action,
    since: String,
}

/// The call lock, held while a power call prepares and hands over its action.
#[derive(Debug)]
pub struct CallGuard {
    _lock: FileLock,
}

/// Takes the call lock for `action`, or fails with `conflict` naming what is pending and since
/// when.
pub fn begin(paths: &Paths, action: Action, boot_id: Option<&str>) -> Result<CallGuard, ErrorBody> {
    let dir = dir(paths);
    std::fs::create_dir_all(&dir).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be created: {error}.", dir.display()),
        )
    })?;
    let lock = match FileLock::try_acquire(dir.join("call.lock"), LockMode::Exclusive) {
        Ok(lock) => lock,
        Err(OsError::WouldBlock { .. }) => return Err(busy_call(&dir)),
        Err(error) => return Err(lock_failed(&dir, &error)),
    };
    if let Some(conflict) = pending_conflict(&dir, boot_id) {
        return Err(conflict);
    }
    let current = Current {
        action,
        since: now(),
    };
    if let Ok(text) = serde_json::to_vec(&current) {
        // Only for the message a second call gets; losing it loses nothing else.
        let _ = legion_core::config::write_atomically(&dir.join("call.json"), &text);
    }
    Ok(CallGuard { _lock: lock })
}

/// The `conflict` a call would get now, without taking anything: for a dry run.
pub fn would_conflict(paths: &Paths, boot_id: Option<&str>) -> Option<ErrorBody> {
    let dir = dir(paths);
    match FileLock::try_acquire(dir.join("call.lock"), LockMode::Exclusive) {
        Ok(lock) => drop(lock),
        Err(OsError::WouldBlock { .. }) => return Some(busy_call(&dir)),
        // No state directory yet: nothing can be pending.
        Err(_) => return None,
    }
    pending_conflict(&dir, boot_id)
}

/// The action handed over and not yet run, if any.
pub fn pending(paths: &Paths, boot_id: Option<&str>) -> Option<Pending> {
    let dir = dir(paths);
    let record = read_pending(&dir)?;
    is_live(&dir, &record, boot_id).then_some(record)
}

fn pending_conflict(dir: &Path, boot_id: Option<&str>) -> Option<ErrorBody> {
    if runner_holds(dir) {
        let what = read_pending(dir).map_or_else(
            || "An action handed to the delayed runner".to_owned(),
            |record| describe(&record),
        );
        return Some(
            ErrorBody::new(
                ErrorCode::Conflict,
                format!("{what} has not run yet, and one power action at a time is carried out."),
            )
            .with_hint("Wait for it: power.status shows what is pending. A machine that went to sleep or restarted has run it.")
            .with_details(json!({"pending": read_pending(dir)})),
        );
    }
    let record = read_pending(dir)?;
    if record.runner || !is_live(dir, &record, boot_id) {
        return None;
    }
    Some(
        ErrorBody::new(
            ErrorCode::Conflict,
            format!("{} is still to happen.", describe(&record)),
        )
        .with_hint("Wait for it; on Windows `shutdown /a` on the machine cancels it.")
        .with_details(json!({"pending": record})),
    )
}

fn is_live(dir: &Path, record: &Pending, boot_id: Option<&str>) -> bool {
    if record.abandoned {
        return false;
    }
    if record.runner {
        return runner_holds(dir);
    }
    let same_boot = match (&record.boot_id, boot_id) {
        (Some(then), Some(now)) => then == now,
        _ => true,
    };
    let due = record.since.parse::<Timestamp>().ok().and_then(|since| {
        let wait = Duration::from_secs(u64::from(record.delay_s)) + SELF_TIMED_GRACE;
        jiff::SignedDuration::try_from(wait)
            .ok()
            .and_then(|wait| since.checked_add(wait).ok())
    });
    same_boot && due.is_some_and(|due| Timestamp::now() < due)
}

fn describe(record: &Pending) -> String {
    let what = match (record.action, &record.into) {
        (Action::Restart, Some(into)) => format!("A restart into {into}"),
        (Action::Restart, None) => "A restart".to_owned(),
        (Action::Sleep, _) => "A sleep".to_owned(),
        (Action::Shutdown, _) => "A shutdown".to_owned(),
    };
    format!("{what} handed over at {}", record.since)
}

fn busy_call(dir: &Path) -> ErrorBody {
    let current: Option<Current> = std::fs::read(dir.join("call.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let what = current.map_or_else(
        || "Another power call is being carried out on this machine".to_owned(),
        |current| {
            format!(
                "Another power call ({}, started at {}) is being carried out on this machine",
                current.action.as_str(),
                current.since
            )
        },
    );
    ErrorBody::new(
        ErrorCode::Conflict,
        format!("{what}, and one power action at a time is carried out."),
    )
    .with_hint("Wait for it to return, then look at power.status before asking again.")
}

fn lock_failed(dir: &Path, error: &OsError) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "The power lock in {} cannot be taken: {error}.",
            dir.display()
        ),
    )
}

/// Whether a runner holds `runner.lock` now.
pub fn runner_holds(dir: &Path) -> bool {
    matches!(
        FileLock::try_acquire(dir.join("runner.lock"), LockMode::Exclusive),
        Err(OsError::WouldBlock { .. })
    )
}

/// The runner lock, waiting up to `patience` for it.
pub fn take_runner_lock(dir: &Path, patience: Duration) -> Option<FileLock> {
    FileLock::acquire_timeout(dir.join("runner.lock"), LockMode::Exclusive, patience).ok()
}

/// The pending record, if there is one that reads.
pub fn read_pending(dir: &Path) -> Option<Pending> {
    let bytes = std::fs::read(dir.join("pending.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Replaces the pending record.
pub fn write_pending(dir: &Path, record: &Pending) -> Result<(), ErrorBody> {
    let text = serde_json::to_vec_pretty(record).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("The pending power action cannot be written as JSON: {error}."),
        )
    })?;
    std::fs::create_dir_all(dir)
        .and_then(|()| legion_core::config::write_atomically(&dir.join("pending.json"), &text))
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "The pending power action cannot be recorded in {}: {error}.",
                    dir.display()
                ),
            )
        })
}

/// Removes the pending record once its action has run.
pub fn clear_pending(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("pending.json"));
}

/// Waits until the runner for `id` has taken over: it holds the runner lock, or has said so in
/// the record. On timeout the record is marked given up, so a runner that starts later does
/// nothing, and `false` is returned.
pub async fn wait_for_runner(dir: &Path, id: &str, patience: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        let record = read_pending(dir);
        let taken = record
            .as_ref()
            .is_some_and(|record| record.id == id && record.taken.is_some());
        if taken || runner_holds(dir) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            if let Some(mut record) = record
                && record.id == id
            {
                record.abandoned = true;
                let _ = write_pending(dir, &record);
                // It may have taken over while this was written.
                if read_pending(dir).is_some_and(|again| again.taken.is_some()) {
                    record.abandoned = false;
                    let _ = write_pending(dir, &record);
                    return true;
                }
            }
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A new hand-over id.
pub fn new_id() -> String {
    let nanos = Timestamp::now().as_nanosecond();
    format!("{nanos:x}-{:x}", std::process::id())
}

/// Now, RFC 3339 UTC to the second.
pub fn now() -> String {
    Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        (dir, paths)
    }

    fn record(runner: bool) -> Pending {
        Pending {
            id: new_id(),
            action: Action::Restart,
            into: Some("windows".to_owned()),
            argv: vec!["systemctl".to_owned(), "reboot".to_owned()],
            delay_s: 3,
            since: now(),
            boot_id: Some("boot-1".to_owned()),
            runner,
            taken: None,
            disarm: None,
            abandoned: false,
        }
    }

    #[test]
    fn a_second_call_meets_the_first() {
        let (_dir, paths) = paths();
        let first = begin(&paths, Action::Sleep, Some("boot-1")).unwrap();
        let second = begin(&paths, Action::Restart, Some("boot-1")).unwrap_err();
        assert_eq!(second.code, ErrorCode::Conflict);
        assert!(
            second.message.contains("(sleep, started at"),
            "{}",
            second.message
        );
        assert!(would_conflict(&paths, Some("boot-1")).is_some());
        drop(first);
        assert!(begin(&paths, Action::Restart, Some("boot-1")).is_ok());
    }

    #[test]
    fn a_runner_holding_its_lock_keeps_its_action_pending() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        let pending_restart = record(true);
        write_pending(&dir, &pending_restart).unwrap();
        // Not held: the runner is gone, and so is the action.
        assert!(pending(&paths, Some("boot-1")).is_none());
        let runner = take_runner_lock(&dir, Duration::from_secs(1)).unwrap();
        assert!(runner_holds(&dir));
        assert_eq!(pending(&paths, Some("boot-1")), Some(pending_restart));
        let error = begin(&paths, Action::Sleep, Some("boot-1")).unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict);
        assert!(
            error
                .message
                .starts_with("A restart into windows handed over at"),
            "{}",
            error.message
        );
        drop(runner);
        assert!(begin(&paths, Action::Sleep, Some("boot-1")).is_ok());
    }

    #[test]
    fn a_restart_windows_times_itself_stays_pending_until_due_or_a_new_boot() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        write_pending(&dir, &record(false)).unwrap();
        assert_eq!(
            begin(&paths, Action::Restart, Some("boot-1"))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // Booted since: it happened.
        assert!(begin(&paths, Action::Restart, Some("boot-2")).is_ok());
        // Long past due: it did not, and does not hold anything up any more.
        let mut old = record(false);
        old.since = "2026-01-01T00:00:00Z".to_owned();
        write_pending(&dir, &old).unwrap();
        assert!(begin(&paths, Action::Restart, Some("boot-1")).is_ok());
    }

    #[tokio::test]
    async fn a_call_waits_for_its_runner_and_gives_up_on_one_that_never_starts() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        let mut taken = record(true);
        taken.taken = Some(now());
        write_pending(&dir, &taken).unwrap();
        assert!(wait_for_runner(&dir, &taken.id, Duration::from_secs(1)).await);

        let late = record(true);
        write_pending(&dir, &late).unwrap();
        assert!(!wait_for_runner(&dir, &late.id, Duration::from_millis(100)).await);
        assert!(read_pending(&dir).unwrap().abandoned);
    }
}
