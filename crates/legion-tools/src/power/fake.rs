//! A recording [`Actuator`] that acts on nothing: what every test, and every build with the
//! `testing` feature, gives the power tools.
//!
//! It answers each command from a list of replies matched by argv prefix, records every command,
//! schedule and datagram, and, when `LEGION_TEST_POWER` names a directory, reads its replies from
//! `replies.json` there and appends what it records to `actions.jsonl`, one JSON object per line,
//! so a test driving the real binary can see what a node would have done.

use std::io::Write;
use std::net::SocketAddrV4;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use legion_core::proto::ErrorBody;
use legion_core::{BoxFuture, CancellationToken};
use serde::{Deserialize, Serialize};

use super::actuator::{Actuator, Ran};

/// The environment variable naming the directory a test binary's fake reads and writes.
pub const TEST_POWER_DIR: &str = "LEGION_TEST_POWER";

/// One thing the fake was asked to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// A command run to completion.
    Run {
        /// The command line.
        argv: Vec<String>,
    },
    /// A command handed to a detached process to run after a delay.
    Schedule {
        /// The command line.
        argv: Vec<String>,
        /// The delay, in whole seconds.
        delay_s: u64,
    },
    /// A UDP datagram.
    Datagram {
        /// Where it went, `address:port`.
        to: String,
        /// Its bytes in lower-case hex.
        hex: String,
    },
}

/// A reply to every command whose argv starts with `argv`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    /// The argv prefix this reply answers.
    pub argv: Vec<String>,
    /// Exit status; 0 when absent.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Standard output.
    #[serde(default)]
    pub stdout: String,
    /// Standard error.
    #[serde(default)]
    pub stderr: String,
    /// Whether the command runs out of time.
    #[serde(default)]
    pub timed_out: bool,
    /// When set, the command cannot be run at all and fails with this error instead.
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

impl Reply {
    /// A reply for commands starting with `argv` that exits `code` printing `stdout`.
    pub fn new(argv: &[&str], code: i32, stdout: &str) -> Reply {
        Reply {
            argv: argv.iter().map(|part| (*part).to_owned()).collect(),
            exit_code: Some(code),
            stdout: stdout.to_owned(),
            ..Reply::default()
        }
    }

    fn outcome(&self) -> Result<Ran, ErrorBody> {
        match &self.error {
            Some(error) => Err(error.clone()),
            None => Ok(Ran {
                exit_code: if self.timed_out {
                    None
                } else {
                    Some(self.exit_code.unwrap_or(0))
                },
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
                timed_out: self.timed_out,
            }),
        }
    }
}

/// The recording fake. A command no reply matches exits 0 and prints nothing; every program is
/// found unless [`RecordingActuator::without_program`] says otherwise.
#[derive(Debug, Default)]
pub struct RecordingActuator {
    /// Replies, most specific wins; a prefix with several replies gives them in turn and then
    /// repeats the last.
    replies: Mutex<Vec<(Reply, usize)>>,
    records: Mutex<Vec<Record>>,
    missing: Mutex<Vec<String>>,
    file: Option<PathBuf>,
    refuse_datagrams: bool,
}

impl RecordingActuator {
    /// A fake that answers every command with exit 0 and records only in memory.
    pub fn new() -> RecordingActuator {
        RecordingActuator::default()
    }

    /// The fake a test binary uses: replies from `$LEGION_TEST_POWER/replies.json`, records
    /// appended to `$LEGION_TEST_POWER/actions.jsonl`; in memory only without the variable.
    pub fn from_env() -> RecordingActuator {
        let Some(dir) = std::env::var_os(TEST_POWER_DIR).map(PathBuf::from) else {
            return RecordingActuator::new();
        };
        let replies: Vec<Reply> = std::fs::read(dir.join("replies.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        RecordingActuator {
            replies: Mutex::new(replies.into_iter().map(|reply| (reply, 0)).collect()),
            file: Some(dir.join("actions.jsonl")),
            ..RecordingActuator::default()
        }
    }

    /// Adds a reply; several for the same prefix are given in the order they were added.
    #[must_use]
    pub fn reply(self, reply: Reply) -> RecordingActuator {
        lock(&self.replies).push((reply, 0));
        self
    }

    /// Makes [`Actuator::find_program`] find nothing called `name`.
    #[must_use]
    pub fn without_program(self, name: &str) -> RecordingActuator {
        lock(&self.missing).push(name.to_owned());
        self
    }

    /// Makes every datagram fail to send.
    #[must_use]
    pub fn refusing_datagrams(mut self) -> RecordingActuator {
        self.refuse_datagrams = true;
        self
    }

    /// Everything recorded so far, in order.
    pub fn records(&self) -> Vec<Record> {
        lock(&self.records).clone()
    }

    /// The command lines run so far, in order.
    pub fn runs(&self) -> Vec<Vec<String>> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Run { argv } => Some(argv),
                _ => None,
            })
            .collect()
    }

    /// The command lines scheduled so far, with their delays.
    pub fn schedules(&self) -> Vec<(Vec<String>, u64)> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                Record::Schedule { argv, delay_s } => Some((argv, delay_s)),
                _ => None,
            })
            .collect()
    }

    fn record(&self, record: Record) {
        if let Some(file) = &self.file {
            append(file, &record);
        }
        lock(&self.records).push(record);
    }

    fn answer(&self, argv: &[String]) -> Result<Ran, ErrorBody> {
        let mut replies = lock(&self.replies);
        let longest = replies
            .iter()
            .filter(|(reply, _)| argv.starts_with(&reply.argv))
            .map(|(reply, _)| reply.argv.len())
            .max();
        let Some(longest) = longest else {
            return Ok(Ran::exited(0, ""));
        };
        let mut matching: Vec<&mut (Reply, usize)> = replies
            .iter_mut()
            .filter(|(reply, _)| reply.argv.len() == longest && argv.starts_with(&reply.argv))
            .collect();
        // The first reply not yet given, or the last one again.
        let unused = matching.iter().position(|(_, used)| *used == 0);
        let index = unused.unwrap_or(matching.len() - 1);
        let Some(entry) = matching.get_mut(index) else {
            return Ok(Ran::exited(0, ""));
        };
        entry.1 += 1;
        entry.0.outcome()
    }
}

impl Actuator for RecordingActuator {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        _timeout: Duration,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Ran, ErrorBody>> {
        self.record(Record::Run {
            argv: argv.to_vec(),
        });
        let outcome = self.answer(argv);
        Box::pin(async move { outcome })
    }

    fn schedule<'a>(
        &'a self,
        argv: &'a [String],
        delay: Duration,
        _log: Option<&'a Path>,
    ) -> BoxFuture<'a, Result<Vec<String>, ErrorBody>> {
        self.record(Record::Schedule {
            argv: argv.to_vec(),
            delay_s: delay.as_secs(),
        });
        Box::pin(async { Ok(Vec::new()) })
    }

    fn send_datagram(&self, payload: &[u8], to: SocketAddrV4) -> Result<(), String> {
        if self.refuse_datagrams {
            return Err("Network is unreachable (os error 101)".to_owned());
        }
        self.record(Record::Datagram {
            to: to.to_string(),
            hex: payload.iter().map(|byte| format!("{byte:02x}")).collect(),
        });
        Ok(())
    }

    fn find_program(&self, name: &str) -> Option<PathBuf> {
        if lock(&self.missing).iter().any(|missing| missing == name) {
            None
        } else {
            Some(PathBuf::from(name))
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn append(file: &Path, record: &Record) {
    let Ok(line) = serde_json::to_string(record) else {
        return;
    };
    if let Ok(mut out) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
    {
        let _ = writeln!(out, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[tokio::test]
    async fn replies_match_the_longest_prefix_in_turn_and_everything_is_recorded() {
        let fake = RecordingActuator::new()
            .reply(Reply::new(&["probe"], 1, "idle"))
            .reply(Reply::new(&["probe"], 0, "busy"))
            .reply(Reply::new(&["probe", "special"], 2, "special"));
        let cancel = CancellationToken::new();
        let second = Duration::from_secs(1);
        let first = fake.run(&argv(&["probe"]), second, &cancel).await.unwrap();
        let then = fake
            .run(&argv(&["probe", "x"]), second, &cancel)
            .await
            .unwrap();
        let again = fake.run(&argv(&["probe"]), second, &cancel).await.unwrap();
        let special = fake
            .run(&argv(&["probe", "special", "x"]), second, &cancel)
            .await
            .unwrap();
        let other = fake.run(&argv(&["other"]), second, &cancel).await.unwrap();
        assert_eq!(
            (
                first.stdout.as_str(),
                then.stdout.as_str(),
                again.stdout.as_str()
            ),
            ("idle", "busy", "busy")
        );
        assert_eq!(special.exit_code, Some(2));
        assert_eq!(other, Ran::exited(0, ""));
        fake.schedule(
            &argv(&["systemctl", "reboot"]),
            Duration::from_secs(3),
            None,
        )
        .await
        .unwrap();
        fake.send_datagram(&[0xff, 0x02], "192.0.2.255:9".parse().unwrap())
            .unwrap();
        assert_eq!(fake.runs().len(), 5);
        assert_eq!(fake.schedules(), [(argv(&["systemctl", "reboot"]), 3)]);
        assert_eq!(
            fake.records().last(),
            Some(&Record::Datagram {
                to: "192.0.2.255:9".to_owned(),
                hex: "ff02".to_owned()
            })
        );
    }

    #[tokio::test]
    async fn a_reply_can_time_out_or_fail_to_start() {
        let fake = RecordingActuator::new()
            .reply(Reply {
                argv: argv(&["slow"]),
                timed_out: true,
                ..Reply::default()
            })
            .reply(Reply {
                argv: argv(&["absent"]),
                error: Some(ErrorBody::new(
                    legion_core::proto::ErrorCode::Unsupported,
                    "not installed",
                )),
                ..Reply::default()
            })
            .without_program("psshutdown64.exe");
        let cancel = CancellationToken::new();
        let slow = fake
            .run(&argv(&["slow"]), Duration::from_secs(1), &cancel)
            .await
            .unwrap();
        assert!(slow.timed_out && slow.exit_code.is_none());
        assert!(
            fake.run(&argv(&["absent"]), Duration::from_secs(1), &cancel)
                .await
                .is_err()
        );
        assert_eq!(fake.find_program("psshutdown64.exe"), None);
        assert!(fake.find_program("psshutdown.exe").is_some());
    }
}
