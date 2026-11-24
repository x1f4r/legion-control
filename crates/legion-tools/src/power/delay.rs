//! `legion power-delay <state/power directory> <id>`: the detached process a power action runs in.
//!
//! A power action has to happen after the reply that announces it has left, or the caller never
//! hears that it was accepted. A task inside the node would die with the session that asked, so
//! the node re-executes its own binary, detached through `legion-os`, as this runner. It takes the
//! runner lock first and holds it until its command has run (see [`super::pending`]), reads what
//! to run from the pending record the call wrote, waits, and then, under the lock `power.cancel`
//! takes too, looks once more that the record is still its own and not given up or called off and
//! notes that it starts, and runs the command exactly as written: no shell, nothing quoted.
//! What it prints, and the command's own output, go to `state/power/scheduled.log`.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use super::pending::{self, Disarmed, Outcome};

/// The first argument that makes `legion` this runner instead of a command line.
pub const DELAY_MODE: &str = "power-delay";

/// How long the runner waits for the runner lock: a runner still holding it is one whose command
/// has not finished, and this one gives up rather than queue behind it.
const LOCK_PATIENCE: Duration = Duration::from_secs(20);
/// How long the command may run before it is stopped, so the lock cannot be held for ever.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);
/// How much of the command's standard error is kept.
const SAID_LIMIT: usize = 2048;
/// How long clearing the firmware's next boot may take.
const DISARM_TIMEOUT: Duration = Duration::from_secs(30);

/// Exit codes: the command's own, or these when it was not run.
const NO_LOCK: u8 = 3;
const NOTHING_TO_DO: u8 = 4;

/// Runs the handed-over action when `args` (the process arguments after the program name) start
/// with [`DELAY_MODE`], returning its exit code; `None` for any other command line.
pub fn try_delay_cli(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut args = args.into_iter();
    if args.next()? != DELAY_MODE {
        return None;
    }
    let rest: Vec<OsString> = args.collect();
    let [dir, id] = rest.as_slice() else {
        say("usage: legion power-delay <state/power directory> <id>");
        return Some(ExitCode::from(2));
    };
    let Some(id) = id.to_str() else {
        return Some(ExitCode::from(2));
    };
    Some(ExitCode::from(carry_out(
        Path::new(dir),
        id,
        &SystemCommands,
    )))
}

/// How a command the runner ran ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Finished {
    /// Its exit status, when it exited.
    pub(crate) exit_code: Option<i32>,
    /// The end of what it wrote to standard error, or why it could not be started.
    pub(crate) said: String,
    /// Whether it was stopped for taking too long.
    pub(crate) timed_out: bool,
}

/// What the runner does to the world: wait, and run a command.
pub(crate) trait Commands {
    /// Waits.
    fn pause(&self, duration: Duration);
    /// Runs `argv` to completion, or stops it after `timeout`.
    fn run(&self, argv: &[String], timeout: Duration) -> Finished;
}

/// Carries out the action `id` recorded in `dir`, returning the process exit code.
pub(crate) fn carry_out(dir: &Path, id: &str, commands: &dyn Commands) -> u8 {
    let Some(_lock) = pending::take_runner_lock(dir, LOCK_PATIENCE) else {
        say(&format!(
            "legion power-delay {id}: another runner still holds {}; nothing was run",
            dir.join("runner.lock").display()
        ));
        return NO_LOCK;
    };
    // The lock already says it was taken; the record says so too for a call that looks late.
    let Some(record) = pending::take_over(dir, id) else {
        say(&format!(
            "legion power-delay {id}: the call gave this action up or replaced it; nothing was run"
        ));
        return NOTHING_TO_DO;
    };
    say(&format!(
        "legion power-delay {id}: running {:?} in {} s",
        record.argv, record.delay_s
    ));
    commands.pause(Duration::from_secs(u64::from(record.delay_s)));
    let record = match pending::fire(dir, id) {
        pending::Fire::Go(record) => *record,
        pending::Fire::Stop => {
            say(&format!(
                "legion power-delay {id}: given up or called off during the delay; nothing was run"
            ));
            return NOTHING_TO_DO;
        }
        pending::Fire::Unsure => {
            say(&format!(
                "legion power-delay {id}: {} could not be taken, so whether this action was called off is unknown; nothing was run",
                dir.join("fire.lock").display()
            ));
            return NOTHING_TO_DO;
        }
    };
    let finished = commands.run(&record.argv, COMMAND_TIMEOUT);
    let succeeded = finished.exit_code == Some(0) && !finished.timed_out;
    // A restart into another system that did not happen leaves the firmware set to start it on
    // whatever restart comes next: clear that again, best effort, and say so in the outcome.
    let disarmed = match (&record.disarm, succeeded) {
        (Some(disarm), false) => {
            let cleared = commands.run(disarm, DISARM_TIMEOUT);
            say(&format!(
                "legion power-delay {id}: the restart failed, so the firmware's next boot was cleared with {disarm:?}: exit {:?}",
                cleared.exit_code
            ));
            Some(Disarmed {
                argv: disarm.clone(),
                exit_code: cleared.exit_code,
                said: cleared.said,
            })
        }
        _ => None,
    };
    let outcome = Outcome {
        id: record.id.clone(),
        action: record.action,
        into: record.into.clone(),
        argv: record.argv.clone(),
        since: record.since.clone(),
        ran_at: Some(pending::now()),
        fate: None,
        exit_code: finished.exit_code,
        timed_out: finished.timed_out,
        said: finished.said.clone(),
        disarmed,
        cancelled_by: None,
        cancelled_at: None,
    };
    if let Err(error) = pending::write_outcome(dir, &outcome) {
        say(&format!(
            "legion power-delay {id}: its outcome could not be recorded: {error}"
        ));
    }
    if record.hold_s > 0 {
        // The machine may go to sleep during this; the rest of the wait is after it wakes.
        commands.pause(Duration::from_secs(u64::from(record.hold_s)));
    }
    say(&format!(
        "legion power-delay {id}: {:?} ended: exit {:?}{}{}",
        record.argv,
        finished.exit_code,
        if finished.timed_out {
            ", stopped after the timeout"
        } else {
            ""
        },
        if finished.said.is_empty() {
            String::new()
        } else {
            format!(": {}", finished.said)
        }
    ));
    pending::clear_pending(dir);
    match finished.exit_code {
        Some(code) => u8::try_from(code.clamp(0, 255)).unwrap_or(1),
        None => 1,
    }
}

/// The machine's own processes.
struct SystemCommands;

impl Commands for SystemCommands {
    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn run(&self, argv: &[String], timeout: Duration) -> Finished {
        let Some((program, args)) = argv.split_first() else {
            return Finished {
                said: "no command".to_owned(),
                ..Finished::default()
            };
        };
        let mut child = match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => {
                return Finished {
                    said: format!("could not be started: {error}"),
                    ..Finished::default()
                };
            }
        };
        let reader = child.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut all = Vec::new();
                let _ = stderr.read_to_end(&mut all);
                // Into the log as well, where the command's standard output already goes.
                let _ = std::io::stdout().write_all(&all);
                all
            })
        });
        let started = Instant::now();
        let (status, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (Some(status), false),
                Ok(None) if started.elapsed() >= timeout => {
                    let _ = child.kill();
                    break (child.wait().ok(), true);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break (None, false),
            }
        };
        let said = reader
            .and_then(|reader| reader.join().ok())
            .map(|bytes| tail(&String::from_utf8_lossy(&bytes)))
            .unwrap_or_default();
        Finished {
            exit_code: status.and_then(|status| status.code()),
            said,
            timed_out,
        }
    }
}

/// The last [`SAID_LIMIT`] bytes of `text`, trimmed, cut on a character boundary.
fn tail(text: &str) -> String {
    let text = text.trim();
    let mut start = text.len().saturating_sub(SAID_LIMIT);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The log the runner's output goes to.
pub(crate) fn log_path(dir: &Path) -> PathBuf {
    dir.join("scheduled.log")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::power::pending::{Pending, now, read_pending, runner_holds, write_pending};
    use crate::power::plan::Action;

    /// Records what it was asked to run, and checks the runner lock is held while it runs.
    #[derive(Default)]
    struct Recorded {
        dir: PathBuf,
        ran: Mutex<Vec<Vec<String>>>,
        paused: Mutex<Vec<Duration>>,
        exit_code: Option<i32>,
        give_up_during_pause: bool,
        cancel_during_pause: bool,
    }

    impl Commands for Recorded {
        fn pause(&self, duration: Duration) {
            assert!(runner_holds(&self.dir), "the delay runs under the lock");
            self.paused.lock().unwrap().push(duration);
            if self.give_up_during_pause {
                let mut record = read_pending(&self.dir).unwrap();
                record.abandoned = true;
                write_pending(&self.dir, &record).unwrap();
            }
            if self.cancel_during_pause {
                assert!(matches!(
                    pending::call_off(&self.dir, "a1"),
                    pending::CallOff::CalledOff(_)
                ));
            }
        }

        fn run(&self, argv: &[String], _timeout: Duration) -> Finished {
            assert!(runner_holds(&self.dir), "the command runs under the lock");
            let mut ran = self.ran.lock().unwrap();
            ran.push(argv.to_vec());
            // The action gets exit_code; anything after it (the disarm) succeeds.
            Finished {
                exit_code: if ran.len() == 1 {
                    self.exit_code
                } else {
                    Some(0)
                },
                said: if ran.len() == 1 && self.exit_code != Some(0) {
                    "sudo: a password is required".to_owned()
                } else {
                    String::new()
                },
                ..Finished::default()
            }
        }
    }

    fn handed_over(dir: &Path) -> Pending {
        let record = Pending {
            id: "a1".to_owned(),
            action: Action::Sleep,
            into: None,
            argv: vec!["systemctl".to_owned(), "suspend".to_owned()],
            delay_s: 3,
            hold_s: 0,
            since: now(),
            boot_id: None,
            runner: true,
            taken: None,
            disarm: None,
            abandoned: false,
            fired: None,
        };
        write_pending(dir, &record).unwrap();
        record
    }

    #[test]
    fn only_its_own_mode_is_taken() {
        let os = |parts: &[&str]| parts.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(try_delay_cli(os(&["job-supervise", "x"])).is_none());
        assert!(try_delay_cli(os(&["meta.version"])).is_none());
        assert!(try_delay_cli(os(&[])).is_none());
        assert_eq!(
            try_delay_cli(os(&[DELAY_MODE, "only-one"])),
            Some(ExitCode::from(2))
        );
    }

    #[test]
    fn the_runner_waits_then_runs_its_own_action_under_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        handed_over(dir.path());
        let commands = Recorded {
            dir: dir.path().to_path_buf(),
            exit_code: Some(0),
            ..Recorded::default()
        };
        assert_eq!(carry_out(dir.path(), "a1", &commands), 0);
        assert_eq!(*commands.paused.lock().unwrap(), [Duration::from_secs(3)]);
        assert_eq!(
            *commands.ran.lock().unwrap(),
            [vec!["systemctl".to_owned(), "suspend".to_owned()]]
        );
        assert!(
            read_pending(dir.path()).is_none(),
            "done, so nothing pending"
        );
        assert!(!runner_holds(dir.path()));
    }

    #[test]
    fn a_runner_does_nothing_for_an_action_that_is_not_its_own_or_was_given_up() {
        let dir = tempfile::tempdir().unwrap();
        handed_over(dir.path());
        let commands = Recorded {
            dir: dir.path().to_path_buf(),
            ..Recorded::default()
        };
        assert_eq!(
            carry_out(dir.path(), "someone-else", &commands),
            NOTHING_TO_DO
        );

        let mut record = handed_over(dir.path());
        record.abandoned = true;
        write_pending(dir.path(), &record).unwrap();
        assert_eq!(carry_out(dir.path(), "a1", &commands), NOTHING_TO_DO);

        handed_over(dir.path());
        let giving_up = Recorded {
            dir: dir.path().to_path_buf(),
            give_up_during_pause: true,
            ..Recorded::default()
        };
        assert_eq!(carry_out(dir.path(), "a1", &giving_up), NOTHING_TO_DO);
        assert!(commands.ran.lock().unwrap().is_empty());
        assert!(giving_up.ran.lock().unwrap().is_empty());
    }

    #[test]
    fn a_runner_whose_action_is_called_off_during_the_delay_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        handed_over(dir.path());
        let commands = Recorded {
            dir: dir.path().to_path_buf(),
            exit_code: Some(0),
            cancel_during_pause: true,
            ..Recorded::default()
        };
        assert_eq!(carry_out(dir.path(), "a1", &commands), NOTHING_TO_DO);
        assert!(commands.ran.lock().unwrap().is_empty());
        // Once it has started the command, a cancel finds it too late.
        handed_over(dir.path());
        let started = Recorded {
            dir: dir.path().to_path_buf(),
            exit_code: Some(0),
            ..Recorded::default()
        };
        assert_eq!(carry_out(dir.path(), "a1", &started), 0);
        assert_eq!(started.ran.lock().unwrap().len(), 1);
    }

    #[test]
    fn the_runner_records_how_its_action_ended() {
        let dir = tempfile::tempdir().unwrap();
        let record = handed_over(dir.path());
        let commands = Recorded {
            dir: dir.path().to_path_buf(),
            exit_code: Some(0),
            ..Recorded::default()
        };
        carry_out(dir.path(), "a1", &commands);
        let outcome: Outcome =
            serde_json::from_slice(&std::fs::read(dir.path().join("last.json")).unwrap()).unwrap();
        assert_eq!(outcome.id, "a1");
        assert_eq!(outcome.argv, record.argv);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.disarmed, None);
    }

    #[test]
    fn a_failed_restart_into_another_system_clears_the_firmware_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = handed_over(dir.path());
        record.action = Action::Restart;
        record.into = Some("windows".to_owned());
        record.argv = vec![
            "sudo".to_owned(),
            "-n".to_owned(),
            "systemctl".to_owned(),
            "reboot".to_owned(),
        ];
        record.disarm = Some(vec![
            "sudo".to_owned(),
            "-n".to_owned(),
            "efibootmgr".to_owned(),
            "--delete-bootnext".to_owned(),
        ]);
        write_pending(dir.path(), &record).unwrap();
        let commands = Recorded {
            dir: dir.path().to_path_buf(),
            exit_code: Some(1),
            ..Recorded::default()
        };
        assert_eq!(carry_out(dir.path(), "a1", &commands), 1);
        assert_eq!(commands.ran.lock().unwrap().last(), record.disarm.as_ref());
        let outcome: Outcome =
            serde_json::from_slice(&std::fs::read(dir.path().join("last.json")).unwrap()).unwrap();
        assert_eq!(outcome.exit_code, Some(1));
        assert_eq!(outcome.said, "sudo: a password is required");
        let disarmed = outcome.disarmed.unwrap();
        assert_eq!(Some(&disarmed.argv), record.disarm.as_ref());
        assert_eq!(disarmed.exit_code, Some(0));

        // A restart that worked leaves the firmware as armed.
        let done = tempfile::tempdir().unwrap();
        let mut ok = record.clone();
        ok.taken = None;
        write_pending(done.path(), &ok).unwrap();
        let worked = Recorded {
            dir: done.path().to_path_buf(),
            exit_code: Some(0),
            ..Recorded::default()
        };
        carry_out(done.path(), "a1", &worked);
        assert_eq!(worked.ran.lock().unwrap().len(), 1);
    }

    #[test]
    fn after_a_sleep_command_the_runner_keeps_the_lock_a_while() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = handed_over(dir.path());
        record.delay_s = 0;
        record.hold_s = 8;
        write_pending(dir.path(), &record).unwrap();
        let commands = Recorded {
            dir: dir.path().to_path_buf(),
            exit_code: Some(0),
            ..Recorded::default()
        };
        carry_out(dir.path(), "a1", &commands);
        // Both waits under the lock (Recorded::pause checks), the second after the command.
        assert_eq!(
            *commands.paused.lock().unwrap(),
            [Duration::from_secs(0), Duration::from_secs(8)]
        );
    }

    #[test]
    fn the_tail_of_what_a_command_said_is_kept() {
        assert_eq!(tail("  short \n"), "short");
        let long = format!("{}é{}", "x".repeat(4000), "y".repeat(100));
        let kept = tail(&long);
        assert!(kept.len() <= SAID_LIMIT);
        assert!(kept.ends_with(&"y".repeat(100)));
    }
}
