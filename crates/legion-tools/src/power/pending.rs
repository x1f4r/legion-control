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
//!
//! `power.cancel` and the runner can reach for the same record at the same moment, the one to call
//! the action off and the other to carry it out. A third lock, `fire.lock`, decides between them:
//! the runner takes it to see the record is still its own and to write that it has started
//! ([`fire`]); a cancel takes it to see the action has not started and to mark it given up
//! ([`call_off`]). Whichever comes second finds what the first wrote, so an action is never both
//! called off and carried out. The call that hands an action over also writes `runner.json`, the
//! detached runner's handle, so a cancel can stop that very process and nothing else.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use legion_core::Paths;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::detach::DetachHandle;
use legion_os::lock::{FileLock, LockMode};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::plan::{Action, ActionPlan, Timing};

/// How long a restart Windows times itself counts as pending after its delay, while Windows shuts
/// down; past that, on the same boot, it did not happen, and holds nothing up any more.
const SELF_TIMED_GRACE: Duration = Duration::from_secs(120);
/// How long the runner keeps the power lock after a sleep command returns, while the machine goes
/// to sleep.
const SLEEP_SETTLE_S: u32 = 5;
/// How long the runner and a cancel wait for `fire.lock`, which either holds only while it reads
/// and writes the record.
const FIRE_PATIENCE: Duration = Duration::from_secs(10);

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
    /// Seconds the runner waits before the command.
    pub delay_s: u32,
    /// Seconds the runner keeps the lock after the command: a sleep command returns before the
    /// machine is asleep, and a second sleep asked for in between would put it back to sleep as
    /// it woke.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub hold_s: u32,
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
    /// Whether the call gave up on it, or a cancel called it off, so a runner must not act.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub abandoned: bool,
    /// When the runner started the command, RFC 3339: from then on it can no longer be called off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired: Option<String>,
    /// Whether the call went ahead past the busy gate, so the runner does not ask again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
    /// The node's configuration directory, where the runner reads what keeps the machine busy
    /// when the action is due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<PathBuf>,
    /// For a restart into another system: how the firmware's next boot is read, so that clearing
    /// it again can be read back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readback: Option<super::boot::Readback>,
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
    /// When the command ended, RFC 3339; absent when it never ran here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<String>,
    /// For an action whose end nothing recorded, what became of it as far as can be known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fate: Option<Fate>,
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
    /// The principal that called it off, when `power.cancel` did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_by: Option<String>,
    /// When it was called off, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<String>,
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

/// What became of a handed-over action, as far as this node can know.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fate {
    /// Still to happen: its runner holds the lock, or, for an action the operating system times
    /// itself, its time and the margin a shutdown takes have not run out on this boot.
    Pending,
    /// The machine has booted since it was handed over: the restart or shutdown happened.
    Happened,
    /// Its time and the margin have passed and the machine is still on the boot it was handed over
    /// in: it did not happen (called off on the machine, as `shutdown /a` there does, or refused
    /// late).
    DidNotHappen,
    /// Its runner is gone: it finished (and its outcome, when written, says how) or stopped early.
    RunnerGone,
    /// The call that handed it over gave it up, so it never ran.
    GivenUp,
    /// `power.cancel` called it off before it ran.
    Cancelled,
    /// When it was due the machine was busy, so its runner did not run it (only for an action
    /// handed over without `force`); `said` says what kept the machine busy.
    Busy,
}

/// The record a call writes for an action it hands over, as planned for this platform: a runner
/// waits the delay unless the command waits itself, and after a sleep keeps the lock while the
/// machine goes to sleep; an action the operating system times itself has no runner.
pub fn record(
    plan: &ActionPlan,
    action: Action,
    into: Option<String>,
    delay_s: u32,
    boot_id: Option<String>,
    disarm: Option<Vec<String>>,
) -> Pending {
    Pending {
        id: new_id(),
        action,
        into,
        argv: plan.argv.clone(),
        delay_s: if plan.waits_itself { 0 } else { delay_s },
        hold_s: match action {
            Action::Sleep if plan.waits_itself => delay_s + SLEEP_SETTLE_S,
            Action::Sleep => SLEEP_SETTLE_S,
            Action::Restart | Action::Shutdown => 0,
        },
        since: now(),
        boot_id,
        runner: plan.timing == Timing::Delayed,
        taken: None,
        disarm,
        abandoned: false,
        fired: None,
        force: false,
        config_dir: None,
        readback: None,
    }
}

/// What has become of `record` at `now`, on the boot `boot_id`, with its runner holding its lock
/// or not. Nothing stays pending for ever: a runner's lock goes with its process, and an action
/// the operating system times lapses after its time and a margin.
pub fn fate(record: &Pending, now: Timestamp, boot_id: Option<&str>, runner_holds: bool) -> Fate {
    if record.abandoned {
        return Fate::GivenUp;
    }
    if record.runner {
        return if runner_holds {
            Fate::Pending
        } else {
            Fate::RunnerGone
        };
    }
    if let (Some(then), Some(current)) = (record.boot_id.as_deref(), boot_id)
        && then != current
    {
        return Fate::Happened;
    }
    match due(record) {
        Some(due) if now < due => Fate::Pending,
        _ => Fate::DidNotHappen,
    }
}

/// When a handed-over action is to happen: its hand-over time plus its delay, RFC 3339.
pub fn due_at(record: &Pending) -> Option<String> {
    let since: Timestamp = record.since.parse().ok()?;
    let due = since
        .checked_add(jiff::SignedDuration::from_secs(i64::from(record.delay_s)))
        .ok()?;
    Some(due.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// When an action the operating system times stops counting as pending: its delay and the margin
/// after it was handed over. `None` for a record whose time cannot be read, which then holds
/// nothing up.
fn due(record: &Pending) -> Option<Timestamp> {
    let since = record.since.parse::<Timestamp>().ok()?;
    let wait = Duration::from_secs(u64::from(record.delay_s)) + SELF_TIMED_GRACE;
    since
        .checked_add(jiff::SignedDuration::try_from(wait).ok()?)
        .ok()
}

/// The last handed-over action and how it ended, as far as can be known: the runner's own
/// outcome, or, for an action nothing recorded the end of (one Windows timed itself, or one whose
/// runner stopped early), what its record and the boot say became of it. Reads, writes nothing.
pub fn last_known(paths: &Paths, boot_id: Option<&str>) -> Option<Outcome> {
    let dir = dir(paths);
    let recorded: Option<Outcome> = std::fs::read(dir.join("last.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let unrecorded = read_pending(&dir).and_then(|record| {
        let fate = fate(&record, Timestamp::now(), boot_id, runner_holds(&dir));
        let told = recorded
            .as_ref()
            .is_some_and(|outcome| outcome.id == record.id);
        (fate != Fate::Pending && !told).then(|| Outcome {
            id: record.id,
            action: record.action,
            into: record.into,
            argv: record.argv,
            since: record.since,
            ran_at: None,
            fate: Some(fate),
            exit_code: None,
            timed_out: false,
            said: String::new(),
            disarmed: None,
            cancelled_by: None,
            cancelled_at: None,
        })
    });
    // The newer of the two; both write times the same way, so their text orders as they do.
    match (recorded, unrecorded) {
        (Some(recorded), Some(unrecorded)) if unrecorded.since > recorded.since => Some(unrecorded),
        (Some(recorded), _) => Some(recorded),
        (None, unrecorded) => unrecorded,
    }
}

/// Records how a handed-over action ended.
pub fn write_outcome(dir: &Path, outcome: &Outcome) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(outcome).map_err(|error| error.to_string())?;
    legion_core::config::write_atomically(&dir.join("last.json"), &text)
        .map_err(|error| error.to_string())
}

fn is_zero(value: &u32) -> bool {
    *value == 0
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
    let lock = take_call_lock(&dir)?;
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

/// The call lock for `power.cancel`, which comes to deal with what is pending, so only a call
/// still preparing its own action holds it up.
pub fn begin_cancel(paths: &Paths) -> Result<CallGuard, ErrorBody> {
    take_call_lock(&dir(paths)).map(|lock| CallGuard { _lock: lock })
}

fn take_call_lock(dir: &Path) -> Result<FileLock, ErrorBody> {
    std::fs::create_dir_all(dir).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be created: {error}.", dir.display()),
        )
    })?;
    match FileLock::try_acquire(dir.join("call.lock"), LockMode::Exclusive) {
        Ok(lock) => Ok(lock),
        Err(OsError::WouldBlock { .. }) => Err(busy_call(dir)),
        Err(error) => Err(lock_failed(dir, &error)),
    }
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
    (fate(&record, Timestamp::now(), boot_id, runner_holds(&dir)) == Fate::Pending)
        .then_some(record)
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
            .with_hint("Wait for it, or call it off with power.cancel: power.status shows what is pending. A machine that went to sleep or restarted has run it.")
            .with_details(json!({"pending": read_pending(dir)})),
        );
    }
    let record = read_pending(dir)?;
    if fate(&record, Timestamp::now(), boot_id, false) != Fate::Pending {
        return None;
    }
    Some(
        ErrorBody::new(
            ErrorCode::Conflict,
            format!("{} is still to happen.", describe(&record)),
        )
        .with_hint("Wait for it, or call it off with power.cancel.")
        .with_details(json!({"pending": record})),
    )
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

/// Removes the pending record once its action has run or been called off, with the runner's
/// handle.
pub fn clear_pending(dir: &Path) {
    let _ = std::fs::remove_file(dir.join("pending.json"));
    let _ = std::fs::remove_file(dir.join("runner.json"));
}

/// The detached runner a call started, as `runner.json` keeps it for `power.cancel`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerHandle {
    /// The hand-over it carries out.
    pub id: String,
    /// What stops it and everything it started, by identity, never by a bare process id.
    pub handle: DetachHandle,
}

/// Records the runner started for the hand-over `id`.
pub fn write_runner(dir: &Path, id: &str, handle: &DetachHandle) -> Result<(), ErrorBody> {
    let text = serde_json::to_vec_pretty(&RunnerHandle {
        id: id.to_owned(),
        handle: handle.clone(),
    })
    .map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("The delayed runner's handle cannot be written as JSON: {error}."),
        )
    })?;
    legion_core::config::write_atomically(&dir.join("runner.json"), &text).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "The delayed runner's handle cannot be recorded in {}: {error}.",
                dir.display()
            ),
        )
    })
}

/// The runner started for the hand-over `id`, when its handle was recorded.
pub fn read_runner(dir: &Path, id: &str) -> Option<DetachHandle> {
    let bytes = std::fs::read(dir.join("runner.json")).ok()?;
    let runner: RunnerHandle = serde_json::from_slice(&bytes).ok()?;
    (runner.id == id).then_some(runner.handle)
}

/// What the runner found when it came to start its command.
#[derive(Debug, PartialEq, Eq)]
pub enum Fire {
    /// The record is still its own and not called off; it now says the command has started.
    Go(Box<Pending>),
    /// Given up, called off or replaced: nothing may run.
    Stop,
    /// `fire.lock` could not be taken, so whether the action was called off cannot be known and
    /// nothing runs.
    Unsure,
}

/// The runner's side of the race with `power.cancel`: under `fire.lock`, the record `id` still its
/// own and not given up is marked started and returned; otherwise nothing may run.
pub fn fire(dir: &Path, id: &str) -> Fire {
    let Some(_fire) = take_fire_lock(dir) else {
        return Fire::Unsure;
    };
    match read_pending(dir) {
        Some(mut record) if record.id == id && !record.abandoned => {
            record.fired = Some(now());
            match write_pending(dir, &record) {
                Ok(()) => Fire::Go(Box::new(record)),
                // Unwritten, a cancel could still think it in time: do not run.
                Err(_) => Fire::Unsure,
            }
        }
        _ => Fire::Stop,
    }
}

/// The runner's note that it has taken `id` over, written under `fire.lock` so it cannot undo a
/// cancel that came in between; `None` when the record is no longer its own to run.
pub fn take_over(dir: &Path, id: &str) -> Option<Pending> {
    let _fire = take_fire_lock(dir)?;
    let mut record = read_pending(dir).filter(|record| record.id == id && !record.abandoned)?;
    record.taken = Some(now());
    let _ = write_pending(dir, &record);
    Some(record)
}

/// What a cancel found when it came to call the action `id` off.
#[derive(Debug, PartialEq, Eq)]
pub enum CallOff {
    /// Marked given up before the runner started it: it will not run.
    CalledOff(Box<Pending>),
    /// The runner had already started the command, at this time.
    TooLate(Box<Pending>),
    /// No such action is pending any more.
    Gone,
    /// `fire.lock` could not be taken.
    Unsure,
}

/// `power.cancel`'s side of the race with the runner: under `fire.lock`, the record `id`, not yet
/// started, is marked given up so the runner will not run it.
pub fn call_off(dir: &Path, id: &str) -> CallOff {
    let Some(_fire) = take_fire_lock(dir) else {
        return CallOff::Unsure;
    };
    match read_pending(dir) {
        Some(record) if record.id == id && record.fired.is_some() => {
            CallOff::TooLate(Box::new(record))
        }
        Some(mut record) if record.id == id && !record.abandoned => {
            record.abandoned = true;
            match write_pending(dir, &record) {
                Ok(()) => CallOff::CalledOff(Box::new(record)),
                Err(_) => CallOff::Unsure,
            }
        }
        _ => CallOff::Gone,
    }
}

fn take_fire_lock(dir: &Path) -> Option<FileLock> {
    FileLock::acquire_timeout(dir.join("fire.lock"), LockMode::Exclusive, FIRE_PATIENCE).ok()
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

    fn sample(runner: bool) -> Pending {
        Pending {
            id: new_id(),
            action: Action::Restart,
            into: Some("windows".to_owned()),
            argv: vec!["systemctl".to_owned(), "reboot".to_owned()],
            delay_s: 3,
            hold_s: 0,
            since: now(),
            boot_id: Some("boot-1".to_owned()),
            runner,
            taken: None,
            disarm: None,
            abandoned: false,
            fired: None,
            force: false,
            config_dir: None,
            readback: None,
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
        let pending_restart = sample(true);
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
        write_pending(&dir, &sample(false)).unwrap();
        assert_eq!(
            begin(&paths, Action::Restart, Some("boot-1"))
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        // Booted since: it happened.
        assert!(begin(&paths, Action::Restart, Some("boot-2")).is_ok());
        // Long past due: it did not, and does not hold anything up any more.
        let mut old = sample(false);
        old.since = "2026-01-01T00:00:00Z".to_owned();
        write_pending(&dir, &old).unwrap();
        assert!(begin(&paths, Action::Restart, Some("boot-1")).is_ok());
    }

    #[test]
    fn a_handed_over_action_is_pending_only_while_it_can_still_happen_on_each_platform() {
        use crate::power::plan::{Platform, action_plan};
        let since: Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();
        let at = |seconds: i64| {
            since
                .checked_add(jiff::SignedDuration::from_secs(seconds))
                .unwrap()
        };
        let found = |name: &str| Some(PathBuf::from(name));
        for platform in [Platform::Linux, Platform::Macos, Platform::Windows] {
            // Built as a real call builds it, from this platform's plan.
            let plan =
                action_plan(platform, Action::Restart, 5, true, false, None, &found).unwrap();
            let mut handed = record(&plan, Action::Restart, None, 5, Some("boot-1".into()), None);
            handed.since = "2026-11-24T10:00:00Z".to_owned();
            if platform == Platform::Windows {
                // Windows times it itself: pending, runner or none, until its time and the margin
                // are up on the same boot; a new boot means it happened.
                assert!(!handed.runner);
                for (seconds, boot, expected) in [
                    (1, "boot-1", Fate::Pending),
                    (124, "boot-1", Fate::Pending),
                    (126, "boot-1", Fate::DidNotHappen),
                    (100_000, "boot-1", Fate::DidNotHappen),
                    (10, "boot-2", Fate::Happened),
                ] {
                    assert_eq!(
                        fate(&handed, at(seconds), Some(boot), false),
                        expected,
                        "{platform:?} after {seconds} s on {boot}"
                    );
                }
            } else {
                // A runner carries it out: pending exactly while the runner holds its lock, which
                // goes with its process.
                assert!(handed.runner);
                assert_eq!(fate(&handed, at(1), Some("boot-1"), true), Fate::Pending);
                assert_eq!(
                    fate(&handed, at(1), Some("boot-1"), false),
                    Fate::RunnerGone
                );
            }
            handed.abandoned = true;
            assert_eq!(fate(&handed, at(1), Some("boot-1"), true), Fate::GivenUp);
            // A time that cannot be read holds nothing up.
            handed.abandoned = false;
            handed.since = "yesterday".to_owned();
            assert_ne!(fate(&handed, at(1), Some("boot-1"), false), Fate::Pending);
        }
        // Windows sleeps through the runner, which does not wait first and holds on after.
        let plan = action_plan(
            Platform::Windows,
            Action::Sleep,
            3,
            false,
            false,
            None,
            &found,
        )
        .unwrap();
        let sleep = record(&plan, Action::Sleep, None, 3, None, None);
        assert!(sleep.runner);
        assert_eq!((sleep.delay_s, sleep.hold_s), (0, 8));
    }

    #[test]
    fn status_learns_what_became_of_an_action_nothing_recorded_the_end_of() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        let mut windows_restart = sample(false);
        windows_restart.since = "2026-01-01T00:00:00Z".to_owned();
        write_pending(&dir, &windows_restart).unwrap();
        let lapsed = last_known(&paths, Some("boot-1")).unwrap();
        assert_eq!(lapsed.fate, Some(Fate::DidNotHappen));
        assert_eq!(lapsed.id, windows_restart.id);
        assert_eq!(
            last_known(&paths, Some("boot-2")).unwrap().fate,
            Some(Fate::Happened)
        );
        // Still to come: not the last action yet.
        write_pending(&dir, &sample(false)).unwrap();
        assert!(last_known(&paths, Some("boot-1")).is_none());

        // A runner that wrote its outcome is told by that outcome.
        let runner = sample(true);
        write_pending(&dir, &runner).unwrap();
        let outcome = Outcome {
            id: runner.id.clone(),
            action: runner.action,
            into: None,
            argv: runner.argv.clone(),
            since: runner.since.clone(),
            ran_at: Some(now()),
            fate: None,
            exit_code: Some(0),
            timed_out: false,
            said: String::new(),
            disarmed: None,
            cancelled_by: None,
            cancelled_at: None,
        };
        write_outcome(&dir, &outcome).unwrap();
        assert_eq!(last_known(&paths, Some("boot-1")), Some(outcome));
    }

    #[test]
    fn a_cancel_and_the_runner_firing_never_both_win() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        // The runner first: too late to call off.
        let record = sample(true);
        write_pending(&dir, &record).unwrap();
        assert!(matches!(fire(&dir, &record.id), Fire::Go(_)));
        assert!(matches!(call_off(&dir, &record.id), CallOff::TooLate(_)));
        // The cancel first: the runner finds it called off.
        let record = sample(true);
        write_pending(&dir, &record).unwrap();
        assert!(matches!(call_off(&dir, &record.id), CallOff::CalledOff(_)));
        assert_eq!(fire(&dir, &record.id), Fire::Stop);
        assert_eq!(take_over(&dir, &record.id), None);
        // Someone else's record, or none: nothing to fire or call off.
        assert_eq!(fire(&dir, "other"), Fire::Stop);
        assert_eq!(call_off(&dir, "other"), CallOff::Gone);
        clear_pending(&dir);
        assert_eq!(call_off(&dir, &record.id), CallOff::Gone);

        // Both at once, many times over: always exactly one of them wins.
        for _ in 0..200 {
            let record = sample(true);
            write_pending(&dir, &record).unwrap();
            let start = std::sync::Arc::new(std::sync::Barrier::new(2));
            let (runner_dir, runner_id, runner_start) = (
                dir.clone(),
                record.id.clone(),
                std::sync::Arc::clone(&start),
            );
            let runner = std::thread::spawn(move || {
                runner_start.wait();
                fire(&runner_dir, &runner_id)
            });
            start.wait();
            let cancel = call_off(&dir, &record.id);
            let fired = runner.join().unwrap();
            match (&fired, &cancel) {
                (Fire::Go(_), CallOff::TooLate(_)) | (Fire::Stop, CallOff::CalledOff(_)) => {}
                other => panic!("both or neither won: {other:?}"),
            }
        }
    }

    #[test]
    fn the_runners_handle_is_kept_for_its_own_hand_over_only() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        std::fs::create_dir_all(&dir).unwrap();
        let handle = DetachHandle {
            process: legion_os::procid::ProcessId::new(4242, 7),
            root_start_key: None,
            systemd_unit: Some("legion-job-power.scope".to_owned()),
            windows_job: None,
            marker: Some("m".to_owned()),
            warnings: Vec::new(),
        };
        write_runner(&dir, "a1", &handle).unwrap();
        assert_eq!(read_runner(&dir, "a1"), Some(handle));
        assert_eq!(read_runner(&dir, "a2"), None);
        clear_pending(&dir);
        assert_eq!(read_runner(&dir, "a1"), None);
    }

    #[tokio::test]
    async fn a_call_waits_for_its_runner_and_gives_up_on_one_that_never_starts() {
        let (_dir, paths) = paths();
        let dir = dir(&paths);
        let mut taken = sample(true);
        taken.taken = Some(now());
        write_pending(&dir, &taken).unwrap();
        assert!(wait_for_runner(&dir, &taken.id, Duration::from_secs(1)).await);

        let late = sample(true);
        write_pending(&dir, &late).unwrap();
        assert!(!wait_for_runner(&dir, &late.id, Duration::from_millis(100)).await);
        assert!(read_pending(&dir).unwrap().abandoned);
    }
}
