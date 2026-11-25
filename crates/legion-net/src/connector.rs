//! Process connectors: the single place in `legion-net` that spawns child processes whose stdin
//! and stdout form a pipe to a remote `legion rpc` or `legion stream` process.
//!
//! Nothing a caller supplies is ever quoted for a remote shell: SSH invokes one of a few fixed
//! command shapes built from a fixed [`Subcommand`] and, optionally, a strictly validated program
//! name or path.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

pub use legion_link::{RemoteOs, StderrTail, Subcommand, TransportHandle, remote_command};
use legion_proto::{ErrorBody, ErrorCode};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

use crate::config::validate_ssh_target;

/// Builds the exact argument vector passed to `ssh` (excluding `"ssh"` itself).
pub fn ssh_args(
    target: &str,
    connect_timeout_secs: u64,
    remote_cmd: &str,
) -> Result<Vec<String>, ErrorBody> {
    validate_ssh_target(target).map_err(|reason| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}."))
            .with_hint("Check the ssh target in fleet.toml.")
    })?;
    let timeout = connect_timeout_secs.max(1);
    Ok(vec![
        "-T".to_owned(),
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-o".to_owned(),
        format!("ConnectTimeout={timeout}"),
        "-o".to_owned(),
        "ServerAliveInterval=15".to_owned(),
        "-o".to_owned(),
        "ServerAliveCountMax=3".to_owned(),
        target.to_owned(),
        remote_cmd.to_owned(),
    ])
}

/// Derives the command argv for `subcommand` from a `fleet.toml` `command` entry.
///
/// For [`Subcommand::Rpc`] the argv is returned untouched. For [`Subcommand::Stream`], a trailing
/// `"rpc"` argument is swapped for `"stream"`; commands that do not end in `"rpc"` are refused
/// with `unsupported` rather than guessing where the subcommand lives.
pub(crate) fn command_argv_for_subcommand(
    argv: &[String],
    subcommand: Subcommand,
) -> Result<Vec<String>, ErrorBody> {
    if argv.is_empty() || argv[0].is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "The configured connector command is empty.",
        )
        .with_hint("Set command to a non-empty argv array in fleet.toml."));
    }
    match subcommand {
        Subcommand::Rpc => Ok(argv.to_vec()),
        Subcommand::Stream => {
            if argv.last().map(String::as_str) == Some("rpc") {
                let mut stream_argv = argv.to_vec();
                if let Some(last) = stream_argv.last_mut() {
                    *last = "stream".to_owned();
                }
                Ok(stream_argv)
            } else {
                Err(ErrorBody::new(
                    ErrorCode::Unsupported,
                    "This machine uses a custom command that does not end with `rpc`, so a `legion stream` pipe cannot be derived from it.",
                )
                .with_hint("End the command in fleet.toml with `rpc`, or connect over ssh."))
            }
        }
    }
}

/// A live connector process whose stdin and stdout are piped to a remote Legion subcommand.
pub struct Pipe {
    stdin: ChildStdin,
    stdout: ChildStdout,
    handle: PipeHandle,
}

impl Pipe {
    /// Spawns a custom connector command from `fleet.toml`.
    pub fn spawn_command(argv: &[String], subcommand: Subcommand) -> Result<Pipe, ErrorBody> {
        let argv = command_argv_for_subcommand(argv, subcommand)?;
        spawn_process(&argv[0], &argv[1..])
    }

    /// Spawns `ssh` targeting `target` with the fixed remote command shape for `os` and `subcommand`.
    pub fn spawn_ssh(
        target: &str,
        os: RemoteOs,
        subcommand: Subcommand,
        program: Option<&str>,
        connect_timeout: Duration,
    ) -> Result<Pipe, ErrorBody> {
        let remote_cmd = remote_command(os, subcommand, program)?;
        let timeout_secs = connect_timeout.as_secs().max(1);
        let args = ssh_args(target, timeout_secs, &remote_cmd)?;
        spawn_process("ssh", &args)
    }

    /// Splits the pipe into its raw stdin, stdout, and process lifecycle handle.
    pub fn into_parts(self) -> (ChildStdin, ChildStdout, PipeHandle) {
        (self.stdin, self.stdout, self.handle)
    }

    /// Borrowed access to the bounded stderr collector.
    pub fn stderr_tail(&self) -> &Arc<StderrTail> {
        &self.handle.stderr
    }

    /// Closes stdin, waits up to `grace` for clean exit, and kills the process if it lingers.
    pub async fn close(self, grace: Duration) {
        drop(self.stdin);
        drop(self.stdout);
        self.handle.close(grace).await;
    }
}

/// Owns the connector child process and its background stderr drain task after stdin/stdout have
/// been split out.
pub struct PipeHandle {
    child: Child,
    stderr: Arc<StderrTail>,
    stderr_task: Option<JoinHandle<()>>,
}

impl PipeHandle {
    /// The bounded stderr collector attached to this process.
    pub fn stderr_tail(&self) -> Arc<StderrTail> {
        Arc::clone(&self.stderr)
    }

    /// Waits briefly for the process to exit and for stderr draining to flush, returning the exit
    /// code (if exited) and the captured stderr tail.
    pub async fn collect_exit_info(&mut self, wait_limit: Duration) -> (Option<i32>, String) {
        let exit_code = match self.child.try_wait() {
            Ok(Some(status)) => status.code(),
            _ => match tokio::time::timeout(wait_limit, self.child.wait()).await {
                Ok(Ok(status)) => status.code(),
                _ => None,
            },
        };
        if let Some(task) = self.stderr_task.take() {
            let mut task = task;
            if tokio::time::timeout(wait_limit, &mut task).await.is_err() {
                task.abort();
            }
        }
        (exit_code, self.stderr.text())
    }

    /// Waits up to `grace` for the child to exit cleanly (after stdin has been closed), then kills
    /// and reaps it if it is still running.
    pub async fn close(mut self, grace: Duration) {
        let exited = tokio::time::timeout(grace, self.child.wait()).await;
        if exited.is_err() {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

impl TransportHandle for PipeHandle {
    fn stderr_tail(&self) -> Option<Arc<StderrTail>> {
        Some(Arc::clone(&self.stderr))
    }

    fn collect_exit_info(
        &mut self,
        wait_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
        Box::pin(PipeHandle::collect_exit_info(self, wait_limit))
    }

    fn close(self: Box<Self>, grace: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(PipeHandle::close(*self, grace))
    }
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        // Non-blocking best-effort cleanup so dropping on an async runtime thread never sleeps.
        // Graceful EOF waiting lives in `PipeHandle::close` / `Router::shutdown`.
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.start_kill();
            let _ = self.child.try_wait();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

fn spawn_process(program: &str, args: &[String]) -> Result<Pipe, ErrorBody> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|err| {
        let hint = if program == "ssh" {
            "Install OpenSSH (`ssh`) and ensure it is on PATH."
        } else {
            "Check that the connector program in fleet.toml exists and is executable."
        };
        ErrorBody::new(
            ErrorCode::Unreachable,
            format!("Could not start `{program}`: {err}."),
        )
        .with_hint(hint)
    })?;

    let stdin = child.stdin.take().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Spawned `{program}` without a stdin pipe."),
        )
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Spawned `{program}` without a stdout pipe."),
        )
    })?;
    let stderr_pipe = child.stderr.take().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Spawned `{program}` without a stderr pipe."),
        )
    })?;

    let stderr = Arc::new(StderrTail::default());
    let stderr_task = tokio::spawn(drain_stderr(stderr_pipe, Arc::clone(&stderr)));
    Ok(Pipe {
        stdin,
        stdout,
        handle: PipeHandle {
            child,
            stderr,
            stderr_task: Some(stderr_task),
        },
    })
}

async fn drain_stderr(stderr: ChildStderr, tail: Arc<StderrTail>) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                tail.push_line(&text);
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_remote_commands_match_the_design_shapes() {
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Rpc, None).unwrap(),
            r#"sh -c 'command -v legion >/dev/null 2>&1 && exec legion rpc || exec "$HOME/.local/bin/legion" rpc'"#
        );
        assert_eq!(
            remote_command(RemoteOs::Windows, Subcommand::Rpc, None).unwrap(),
            "legion rpc"
        );
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Stream, None).unwrap(),
            r#"sh -c 'command -v legion >/dev/null 2>&1 && exec legion stream || exec "$HOME/.local/bin/legion" stream'"#
        );
        assert_eq!(
            remote_command(RemoteOs::Windows, Subcommand::Stream, None).unwrap(),
            "legion stream"
        );
    }

    #[test]
    fn custom_program_replaces_legion_and_refuses_unsafe_characters() {
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Rpc, Some("legion-beta")).unwrap(),
            r#"sh -c 'command -v legion-beta >/dev/null 2>&1 && exec legion-beta rpc || exec "$HOME/.local/bin/legion-beta" rpc'"#
        );
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Rpc, Some("/opt/legion/legion")).unwrap(),
            "sh -c 'exec /opt/legion/legion rpc'"
        );
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Stream, Some("~/tools/legion")).unwrap(),
            "sh -c 'exec ~/tools/legion stream'"
        );
        assert_eq!(
            remote_command(
                RemoteOs::Windows,
                Subcommand::Rpc,
                Some(r"C:\Tools\legion.exe")
            )
            .unwrap(),
            r"C:\Tools\legion.exe rpc"
        );
        for unsafe_prog in [
            "",
            "-v",
            "legion;id",
            "/path with space/legion",
            "legion'quote",
            r"C:\trailing\",
            "$(whoami)",
        ] {
            let err =
                remote_command(RemoteOs::Unix, Subcommand::Rpc, Some(unsafe_prog)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{unsafe_prog:?}");
        }
    }

    #[test]
    fn ssh_args_pass_target_as_one_argument_and_refuse_leading_dash() {
        let args = ssh_args("me@tower.example", 8, "legion rpc").unwrap();
        assert_eq!(
            args,
            vec![
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "me@tower.example",
                "legion rpc",
            ]
        );
        // Zero timeout clamps to 1 so OpenSSH accepts ConnectTimeout.
        let clamped = ssh_args("pi", 0, "legion rpc").unwrap();
        assert!(clamped.contains(&"ConnectTimeout=1".to_owned()));

        assert_eq!(
            ssh_args("-oProxyCommand=evil", 5, "legion rpc")
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgs
        );
    }

    #[test]
    fn no_ssh_invocation_contains_control_options() {
        for os in [RemoteOs::Unix, RemoteOs::Windows] {
            for subcommand in [Subcommand::Rpc, Subcommand::Stream] {
                for program in [None, Some("/opt/legion/legion")] {
                    let remote_cmd = remote_command(os, subcommand, program).unwrap();
                    let args = ssh_args("me@tower.example", 10, &remote_cmd).unwrap();
                    // Check every option flag and `-o` value before the target and remote command.
                    for arg in &args[..args.len() - 2] {
                        let lower = arg.to_ascii_lowercase();
                        assert!(
                            !lower.contains("controlmaster")
                                && !lower.contains("controlpath")
                                && !lower.contains("controlpersist")
                                && arg != "-M"
                                && arg != "-S"
                                && arg != "-O",
                            "unexpected control option {arg:?} in {args:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn command_subcommand_derivation_swaps_trailing_rpc_for_stream() {
        let base = vec![
            "docker".to_owned(),
            "exec".to_owned(),
            "-i".to_owned(),
            "box".to_owned(),
            "legion".to_owned(),
            "rpc".to_owned(),
        ];
        assert_eq!(
            command_argv_for_subcommand(&base, Subcommand::Rpc).unwrap(),
            base
        );
        assert_eq!(
            command_argv_for_subcommand(&base, Subcommand::Stream).unwrap(),
            vec!["docker", "exec", "-i", "box", "legion", "stream"]
        );

        let custom = vec!["my-bridge".to_owned(), "--node".to_owned(), "pi".to_owned()];
        assert_eq!(
            command_argv_for_subcommand(&custom, Subcommand::Stream)
                .unwrap_err()
                .code,
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn stderr_tail_is_bounded_and_keeps_the_latest_lines() {
        let tail = StderrTail::default();
        for i in 0..50 {
            tail.push_line(&format!("line-{i}\n"));
        }
        let text = tail.text();
        assert!(!text.contains("line-0"), "{text}");
        assert!(text.contains("line-49"), "{text}");
        assert!(text.len() <= 8 * 1024 + 200);
    }
}
