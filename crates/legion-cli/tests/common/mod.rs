//! Runs the built `legion` binary inside a temporary directory of its own: `LEGION_HOME` and the home
//! directory both point into it, so nothing a test does reaches the real configuration.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::Value;

/// How long a test waits for a line before calling the binary stuck.
pub const PATIENCE: Duration = Duration::from_secs(30);

pub struct Home {
    dir: tempfile::TempDir,
}

/// What one run of the binary came to.
pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// stdout as the one JSON value it should be.
    pub fn json(&self) -> Value {
        serde_json::from_str(self.stdout.trim()).unwrap_or_else(|error| {
            panic!(
                "stdout is not JSON ({error}):\n{}\nstderr:\n{}",
                self.stdout, self.stderr
            )
        })
    }
}

impl Home {
    pub fn new() -> Home {
        let home = Home {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(home.home().join("projects").join("app")).unwrap();
        std::fs::create_dir_all(home.home().join(".ssh")).unwrap();
        home
    }

    /// The directory `~` stands for.
    pub fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    pub fn state(&self) -> PathBuf {
        self.dir.path().join("legion").join("state")
    }

    pub fn write_config(&self, name: &str, text: &str) {
        let config = self.dir.path().join("legion").join("config");
        std::fs::create_dir_all(&config).unwrap();
        legion_core::config::write_atomically(&config.join(name), text.as_bytes()).unwrap();
    }

    /// The audit log, one value per line.
    pub fn audit(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state().join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_legion"));
        command
            .args(args)
            .env("LEGION_HOME", self.dir.path().join("legion"))
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .current_dir(self.dir.path());
        command
    }

    pub fn run(&self, args: &[&str]) -> Run {
        self.run_with_stdin(args, "")
    }

    pub fn run_with_stdin(&self, args: &[&str], stdin: &str) -> Run {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        let Output {
            status,
            stdout,
            stderr,
        } = child.wait_with_output().unwrap();
        Run {
            code: status.code().unwrap_or(-1),
            stdout: String::from_utf8(stdout).unwrap(),
            stderr: String::from_utf8(stderr).unwrap(),
        }
    }

    /// `legion rpc` with these arguments, ready to be talked to.
    pub fn session(&self, args: &[&str]) -> Session {
        let mut full = vec!["rpc"];
        full.extend_from_slice(args);
        let mut child = self
            .command(&full)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        // A thread reads, so a test can wait for a line with a deadline instead of hanging.
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Session {
            child,
            stdin,
            lines,
        }
    }
}

/// A running `legion rpc`.
pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Session {
    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is already closed");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    pub fn send_json(&mut self, value: &Value) {
        self.send(&value.to_string());
    }

    /// The next line the session writes, as JSON.
    pub fn next(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(PATIENCE)
            .unwrap_or_else(|error| panic!("no line from legion rpc: {error}"));
        serde_json::from_str(line.trim_end())
            .unwrap_or_else(|error| panic!("{line:?} is not JSON: {error}"))
    }

    /// Lines until one is the reply to `id`, returning it and the notifications seen on the way.
    pub fn reply_to(&mut self, id: &Value) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let message = self.next();
            if message.get("id") == Some(id) && message.get("method").is_none() {
                return (message, seen);
            }
            seen.push(message);
        }
    }

    /// Closes stdin and waits for the process to end, returning its exit code.
    pub fn finish(&mut self) -> i32 {
        drop(self.stdin.take());
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            if std::time::Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("legion rpc did not end after its input did");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What is left on stdout once the process has ended: every line up to the end of the output.
    pub fn rest(&self) -> Vec<String> {
        let mut rest = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(PATIENCE) {
            rest.push(line);
        }
        rest
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether `path` ends with these parts, which is how a test checks a resolved path without caring
/// how the platform spells the temporary directory above it.
pub fn ends_with(path: &str, parts: &[&str]) -> bool {
    let tail: PathBuf = parts.iter().collect();
    Path::new(path).ends_with(tail)
}
