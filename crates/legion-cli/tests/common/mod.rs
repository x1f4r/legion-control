//! Runs the built `legion` binary inside a temporary directory of its own: `LEGION_HOME` and the home
//! directory both point into it, so nothing a test does reaches the real configuration.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

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
        std::fs::write(config.join(name), text).unwrap();
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
}

/// Whether `path` ends with these parts, which is how a test checks a resolved path without caring
/// how the platform spells the temporary directory above it.
pub fn ends_with(path: &str, parts: &[&str]) -> bool {
    let tail: PathBuf = parts.iter().collect();
    Path::new(path).ends_with(tail)
}
