//! The one door between the power tools and the machine: run a command, schedule one, send a
//! datagram, find a program.
//!
//! Everything a power tool does to the machine, or reads from it to decide, goes through
//! [`Actuator`], so a test hands the tools a recording fake and nothing it runs can sleep, restart
//! or wake anything real. A build with the `testing` feature (every test build of the binary) uses
//! the fake whatever its environment says; see [`actuator`].

use std::net::{SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{BoxFuture, CancellationToken};
use legion_os::OsError;
use legion_os::detach::{DetachOptions, spawn_detached};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde_json::json;

use super::delay::DELAY_MODE;

/// How much of a command's output is kept: enough for any listing the power tools parse.
const CAPTURE_LIMIT: usize = 1024 * 1024;

/// How a command ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ran {
    /// Its exit status, or `None` when it was stopped by a signal or never finished.
    pub exit_code: Option<i32>,
    /// What it wrote to standard output.
    pub stdout: String,
    /// What it wrote to standard error.
    pub stderr: String,
    /// Whether it was stopped for taking longer than it was given.
    pub timed_out: bool,
}

impl Ran {
    /// A command that exited with `code` and printed `stdout`.
    pub fn exited(code: i32, stdout: impl Into<String>) -> Ran {
        Ran {
            exit_code: Some(code),
            stdout: stdout.into(),
            ..Ran::default()
        }
    }

    /// Whether it exited 0 in time.
    pub fn ok(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// Its first non-empty line of standard error, or else of standard output: what a message
    /// quotes.
    pub fn first_line(&self) -> &str {
        first_line(&self.stderr)
            .or_else(|| first_line(&self.stdout))
            .unwrap_or("")
    }

    /// What it said, standard error first, its first few non-empty lines joined into one: what a
    /// refusal is recognised by, since tools such as bcdedit say "Access is denied." only on
    /// their second line.
    pub fn said(&self) -> String {
        self.stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(4)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// What the power tools need from the machine.
pub trait Actuator: Send + Sync + 'static {
    /// Runs a command, exactly as given, to completion within `timeout`, in the `C` locale.
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        timeout: Duration,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Ran, ErrorBody>>;

    /// Starts a process that outlives this one, waits `delay` and then runs `argv`, appending what
    /// it prints to `log`. Returns once that process has started, with any warnings about how
    /// well it is detached.
    fn schedule<'a>(
        &'a self,
        argv: &'a [String],
        delay: Duration,
        log: Option<&'a Path>,
    ) -> BoxFuture<'a, Result<Vec<String>, ErrorBody>>;

    /// Sends one UDP datagram, broadcast allowed, from an unbound socket.
    fn send_datagram(&self, payload: &[u8], to: SocketAddrV4) -> Result<(), String>;

    /// Where a program of this name is on `PATH`, if anywhere.
    fn find_program(&self, name: &str) -> Option<PathBuf>;
}

/// The actuator the registered power tools use: the machine itself in a release build, and a
/// recording fake in any build with the `testing` feature or under `cargo test`, so no test can
/// act on the machine it runs on.
pub fn actuator() -> Arc<dyn Actuator> {
    #[cfg(any(test, feature = "testing"))]
    let chosen: Arc<dyn Actuator> = Arc::new(super::fake::RecordingActuator::from_env());
    #[cfg(not(any(test, feature = "testing")))]
    let chosen: Arc<dyn Actuator> = Arc::new(SystemActuator);
    chosen
}

/// The machine itself, through `legion-os`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemActuator;

impl Actuator for SystemActuator {
    fn run<'a>(
        &'a self,
        argv: &'a [String],
        timeout: Duration,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Ran, ErrorBody>> {
        Box::pin(async move {
            let spec = CommandSpec::from_argv(argv.iter().cloned())
                .map_err(|error| os_error(error, argv))?;
            let child_cancel = cancel.child_token();
            let _stop_on_drop = child_cancel.clone().drop_guard();
            let mut options = RunOptions::new(spec)
                .c_locale()
                .timeout(timeout)
                .capture_limit(CAPTURE_LIMIT)
                .cancel(child_cancel);
            if is_powershell(argv) {
                // An inherited PowerShell 7 module path breaks Windows PowerShell's own modules.
                options = options.env("PSModulePath", "");
            }
            let output = run(options).await.map_err(|error| os_error(error, argv))?;
            if output.cancelled {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("The call was cancelled while running {}.", program(argv)),
                ));
            }
            Ok(Ran {
                exit_code: output.exit_code,
                stdout: output.stdout.text,
                stderr: output.stderr.text,
                timed_out: output.timed_out,
            })
        })
    }

    fn schedule<'a>(
        &'a self,
        argv: &'a [String],
        delay: Duration,
        log: Option<&'a Path>,
    ) -> BoxFuture<'a, Result<Vec<String>, ErrorBody>> {
        Box::pin(async move {
            let exe = std::env::current_exe().map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("legion cannot find its own program to schedule the action: {error}."),
                )
            })?;
            let mut args = vec![
                DELAY_MODE.to_owned(),
                delay.as_secs().to_string(),
                "--".to_owned(),
            ];
            args.extend(argv.iter().cloned());
            let mut options = DetachOptions::new(CommandSpec::argv(exe, args)).name("power");
            if let Some(log) = log {
                if let Some(dir) = log.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                options = options.output_path(log);
            }
            let handle = spawn_detached(options)
                .await
                .map_err(|error| os_error(error, argv))?;
            Ok(handle.warnings)
        })
    }

    fn send_datagram(&self, payload: &[u8], to: SocketAddrV4) -> Result<(), String> {
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(|error| error.to_string())?;
        socket
            .set_broadcast(true)
            .map_err(|error| error.to_string())?;
        socket
            .send_to(payload, to)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn find_program(&self, name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    }
}

fn is_powershell(argv: &[String]) -> bool {
    argv.first().is_some_and(|program| {
        let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
        name.eq_ignore_ascii_case("powershell.exe") || name.eq_ignore_ascii_case("powershell")
    })
}

/// The program a command line starts, for messages.
pub(crate) fn program(argv: &[String]) -> &str {
    argv.first().map_or("", String::as_str)
}

fn os_error(error: OsError, argv: &[String]) -> ErrorBody {
    match error {
        OsError::ProgramNotFound { program: missing } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "`{missing}` is not installed on this machine, or not on the PATH legion sees."
            ),
        )
        .with_hint(format!(
            "Install `{missing}`, or make sure it is on PATH for the account legion runs as."
        ))
        .with_details(json!({"program": missing, "argv": argv})),
        OsError::PermissionDenied { detail, .. } => ErrorBody::new(
            ErrorCode::Denied,
            format!("The system refused to start `{}`: {detail}.", program(argv)),
        )
        .with_hint("Check that the account legion runs as may run this program.")
        .with_details(json!({"argv": argv})),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("`{}` could not be started: {other}.", program(argv)),
        )
        .with_details(json!({"argv": argv})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_quotes_its_first_useful_line() {
        let ran = Ran {
            exit_code: Some(1),
            stdout: "out\n".to_owned(),
            stderr: "\n  sudo: a password is required\nmore\n".to_owned(),
            timed_out: false,
        };
        assert_eq!(ran.first_line(), "sudo: a password is required");
        assert!(!ran.ok());
        assert_eq!(Ran::exited(0, "x\n").first_line(), "x");
        let bcdedit = Ran::exited(
            1,
            "The boot configuration data store could not be opened.\r\nAccess is denied.\r\n",
        );
        assert_eq!(
            bcdedit.said(),
            "The boot configuration data store could not be opened. Access is denied."
        );
        assert!(Ran::exited(0, "").ok());
    }

    #[test]
    fn powershell_is_recognised_by_its_file_name() {
        for yes in [
            "powershell.exe",
            "PowerShell.EXE",
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe",
        ] {
            assert!(is_powershell(&[yes.to_owned()]), "{yes}");
        }
        assert!(!is_powershell(&["pwsh".to_owned()]));
        assert!(!is_powershell(&[]));
    }
}
