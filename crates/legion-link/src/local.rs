//! Desktop-only local child-process pipe (`legion rpc` over stdio).
//!
//! Spawns a configured `legion` binary with `rpc` and attaches it to [`Session`] through
//! [`TransportHandle`], capturing stderr in [`StderrTail`].

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use legion_proto::{ErrorBody, ErrorCode};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::session::{Session, TransportHandle};
use crate::stderr::StderrTail;

/// Lifecycle handle for a locally spawned `legion rpc` child process.
pub struct LocalPipeHandle {
    child: Child,
    stderr: Arc<StderrTail>,
    stderr_task: Option<JoinHandle<()>>,
}

impl TransportHandle for LocalPipeHandle {
    fn stderr_tail(&self) -> Option<Arc<StderrTail>> {
        Some(Arc::clone(&self.stderr))
    }

    fn collect_exit_info(
        &mut self,
        wait_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = (Option<i32>, String)> + Send + '_>> {
        Box::pin(async move {
            let exit_code = match self.child.try_wait() {
                Ok(Some(status)) => status.code(),
                _ => match tokio::time::timeout(wait_limit, self.child.wait()).await {
                    Ok(Ok(status)) => status.code(),
                    _ => None,
                },
            };
            if let Some(mut task) = self.stderr_task.take()
                && tokio::time::timeout(wait_limit, &mut task).await.is_err()
            {
                task.abort();
            }
            (exit_code, self.stderr.text())
        })
    }

    fn close(mut self: Box<Self>, grace: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            if tokio::time::timeout(grace, self.child.wait())
                .await
                .is_err()
            {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
            }
            if let Some(task) = self.stderr_task.take() {
                task.abort();
            }
        })
    }
}

impl Drop for LocalPipeHandle {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.start_kill();
            let _ = self.child.try_wait();
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

/// Spawns `<program> rpc` as a child process and opens a [`Session`] over its stdin and stdout.
pub async fn open_local_session(
    program: &str,
    machine: &str,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<Session, ErrorBody> {
    if program.trim().is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Local legion executable path cannot be empty.",
        )
        .with_hint("Set `local_program` to the path of the `legion` binary."));
    }
    let mut cmd = Command::new(program);
    cmd.arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Unreachable,
            format!("Could not start `{program} rpc`: {err}."),
        )
        .with_machine(machine)
        .with_hint("Check that `legion` is installed and executable at that path.")
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
    let handle = LocalPipeHandle {
        child,
        stderr,
        stderr_task: Some(stderr_task),
    };

    Session::open_transport(
        stdout,
        stdin,
        Box::new(handle),
        machine,
        None,
        timeout,
        cancel,
    )
    .await
    .map_err(|err| err.into_error())
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
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn empty_or_missing_local_program_returns_clear_error() {
        let err = open_local_session(
            "",
            "local",
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::InvalidArgs);

        let err = open_local_session(
            "/nonexistent/legion-binary-for-test",
            "local",
            Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::Unreachable);
        assert_eq!(err.machine.as_deref(), Some("local"));
        assert!(err.hint.is_some());
    }
}
