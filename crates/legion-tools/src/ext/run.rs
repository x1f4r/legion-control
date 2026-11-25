//! Executing an extension tool through `legion-os`: bounded time and output, whole-tree stop on
//! timeout or cancel, live event streaming, and JSON/text output shaping.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use legion_core::proto::{ErrorBody, ErrorCode, EventKind, ToolSpec};
use legion_core::{BoxFuture, CANCEL_GRACE, CancellationToken, Ctx, Tool, ToolError};
use legion_os::OsError;
use legion_os::run::{OutputChunk, OutputStream, RunOptions, RunOutput, run};
use legion_os::shell::CommandSpec;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::def::ExtensionDef;
use crate::exec::text::Decoder;

/// Grace between polite termination and forced kill on timeout or cancel, well inside
/// [`CANCEL_GRACE`] so a cancelled call waits until the process tree is gone.
const KILL_GRACE: Duration = Duration::from_secs(2);
const _: () = assert!(KILL_GRACE.as_millis() * 2 < CANCEL_GRACE.as_millis());

/// Per-stream output capture limit (64 KiB), matching `exec.run`'s default.
const MAX_OUTPUT: usize = 64 * 1024;

/// Maximum bytes of stderr tail included in error details on a non-zero exit or timeout.
const STDERR_TAIL_BYTES: usize = 4096;

/// A loaded extension tool ready to run as a [`Tool`].
pub struct ExtensionTool {
    def: ExtensionDef,
    spec: ToolSpec,
}

impl ExtensionTool {
    /// Builds a runnable tool from a validated [`ExtensionDef`].
    pub fn new(def: ExtensionDef) -> Self {
        let spec = ToolSpec {
            name: def.name.clone(),
            description: def.description.clone(),
            effect: def.effect,
            input_schema: def.input_schema(),
        };
        ExtensionTool { def, spec }
    }

    /// Runs the extension command after validating arguments.
    async fn execute(&self, ctx: &Ctx, args: Value) -> Result<Value, ToolError> {
        let resolved = self.def.validate_args(args)?;
        let arg_envs = self.def.prepare_arg_env(&resolved)?;
        let (command, program) = command_of(&self.def)?;
        let cwd = resolve_cwd(self.def.cwd.as_deref(), ctx.paths().home())?;
        let timeout_s = self.def.effective_timeout_s();
        let timeout = Duration::from_secs_f64(timeout_s);
        let stdin_bytes =
            serde_json::to_vec(&Value::Object(resolved.clone())).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{}: arguments could not be written as JSON: {error}.",
                        self.def.name
                    ),
                )
            })?;

        let env_remove: Vec<OsString> = std::env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| name.to_string_lossy().starts_with("LEGION_"))
            .collect();
        let mut envs: Vec<(OsString, OsString)> = vec![
            (
                OsString::from("LEGION_TOOL"),
                OsString::from(&self.def.name),
            ),
            (
                OsString::from("LEGION_PRINCIPAL"),
                OsString::from(ctx.principal()),
            ),
            (
                OsString::from("LEGION_NODE"),
                OsString::from(ctx.node_name()),
            ),
        ];
        for (key, value) in arg_envs {
            envs.push((OsString::from(key), OsString::from(value)));
        }

        let (sender, mut receiver) = mpsc::unbounded_channel::<OutputChunk>();
        let budgets = [
            Arc::new(AtomicUsize::new(MAX_OUTPUT)),
            Arc::new(AtomicUsize::new(MAX_OUTPUT)),
        ];
        let forward = {
            let budgets = budgets.clone();
            Arc::new(move |mut chunk: OutputChunk| {
                let budget = &budgets[stream_index(chunk.stream)];
                let allowed = take_budget(budget, chunk.bytes.len());
                if allowed > 0 {
                    chunk.bytes.truncate(allowed);
                    let _ = sender.send(chunk);
                }
            })
        };

        let cancel = CancellationToken::new();
        let _stop_on_drop = cancel.clone().drop_guard();
        let mut options = RunOptions::new(command)
            .env_removes(env_remove)
            .envs(envs)
            .stdin(stdin_bytes)
            .timeout(timeout)
            .capture_limit(MAX_OUTPUT)
            .kill_grace(KILL_GRACE)
            .cancel(cancel.clone())
            .on_output(forward);
        if let Some(cwd) = &cwd {
            options = options.cwd(cwd);
        }
        let mut running = tokio::spawn(run(options));

        let mut decoders: [Decoder; 2] = Default::default();
        let kinds = [EventKind::Stdout, EventKind::Stderr];
        let mut emit = |stream: OutputStream, bytes: Option<&[u8]>| {
            let index = stream_index(stream);
            let text = match bytes {
                Some(bytes) => decoders[index].push(bytes),
                None => decoders[index].finish(),
            };
            if !text.is_empty() {
                ctx.emit(kinds[index], Value::String(text));
            }
        };
        let finished = loop {
            tokio::select! {
                Some(chunk) = receiver.recv() => emit(chunk.stream, Some(&chunk.bytes)),
                finished = &mut running => break finished,
                () = ctx.cancelled(), if !cancel.is_cancelled() => cancel.cancel(),
            }
        };
        while let Ok(chunk) = receiver.try_recv() {
            emit(chunk.stream, Some(&chunk.bytes));
        }
        emit(OutputStream::Stdout, None);
        emit(OutputStream::Stderr, None);

        let output = finished
            .map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!(
                        "{}: running the command stopped unexpectedly: {error}.",
                        self.def.name
                    ),
                )
            })?
            .map_err(|error| run_error(&self.def.name, error, &program, cwd.as_deref()))?;
        interpret_output(&self.def.name, timeout_s, output)
    }
}

impl Tool for ExtensionTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(self.execute(ctx, args))
    }
}

fn command_of(def: &ExtensionDef) -> Result<(CommandSpec, String), ErrorBody> {
    if let Some(argv) = &def.run {
        let program = argv.first().cloned().unwrap_or_default();
        let spec = CommandSpec::from_argv(argv.clone()).map_err(|error| {
            ErrorBody::new(ErrorCode::Failed, format!("{}: {error}.", def.name))
        })?;
        Ok((spec, program))
    } else if let Some(cmd) = &def.cmd {
        let program = cmd.split_whitespace().next().unwrap_or_default().to_owned();
        Ok((CommandSpec::shell(cmd.clone()), program))
    } else {
        Err(ErrorBody::new(
            ErrorCode::Internal,
            format!("{}: definition has neither `run` nor `cmd`.", def.name),
        ))
    }
}

fn resolve_cwd(cwd: Option<&str>, home: Option<&Path>) -> Result<Option<PathBuf>, ErrorBody> {
    let Some(cwd) = cwd else {
        return Ok(None);
    };
    if cwd == "~" {
        return home.map(|h| Some(h.to_path_buf())).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                "Cannot expand `~` for `cwd`: the home directory is not known.",
            )
        });
    }
    if let Some(rest) = cwd.strip_prefix("~/").or_else(|| cwd.strip_prefix("~\\")) {
        return home.map(|h| Some(h.join(rest))).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                "Cannot expand `~` for `cwd`: the home directory is not known.",
            )
        });
    }
    Ok(Some(PathBuf::from(cwd)))
}

fn interpret_output(tool: &str, timeout_s: f64, output: RunOutput) -> Result<Value, ErrorBody> {
    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!("The call to {tool} was cancelled, and the command was stopped."),
        ));
    }
    if output.timed_out {
        let mut details = Map::new();
        details.insert("timeout_s".to_owned(), json!(timeout_s));
        let stderr = tail(&output.stderr.text, STDERR_TAIL_BYTES);
        if !stderr.is_empty() {
            details.insert("stderr".to_owned(), Value::String(stderr.to_owned()));
        }
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!("{tool} timed out after {timeout_s}s and was stopped."),
        )
        .with_details(Value::Object(details)));
    }
    if !output.success() {
        let mut details = Map::new();
        let message = match (output.exit_code, output.signal) {
            (Some(code), _) => {
                details.insert("exit_code".to_owned(), json!(code));
                format!("{tool} exited with status {code}.")
            }
            (None, Some(signal)) => {
                details.insert("signal".to_owned(), json!(signal));
                format!("{tool} was terminated by signal {signal}.")
            }
            (None, None) => format!("{tool} failed without an exit code."),
        };
        details.insert(
            "stderr".to_owned(),
            Value::String(tail(&output.stderr.text, STDERR_TAIL_BYTES).to_owned()),
        );
        return Err(ErrorBody::new(ErrorCode::Failed, message).with_details(Value::Object(details)));
    }

    if output.stdout.truncated {
        return Ok(json!({
            "output": output.stdout.text,
            "truncated": true,
        }));
    }
    match serde_json::from_str::<Value>(output.stdout.text.trim()) {
        Ok(object @ Value::Object(_)) => Ok(object),
        Ok(other) => Ok(json!({"value": other})),
        Err(_) => Ok(json!({
            "output": output.stdout.text,
            "truncated": false,
        })),
    }
}

fn run_error(tool: &str, error: OsError, program: &str, cwd: Option<&Path>) -> ErrorBody {
    let cwd_text = || cwd.map_or_else(String::new, |cwd| cwd.display().to_string());
    match error {
        OsError::ProgramNotFound { program: missing } => ErrorBody::new(
            ErrorCode::NotFound,
            format!("{tool}: there is no program called {missing} on this machine."),
        )
        .with_hint(
            "Check the extension definition, or give the program's full path; legion looks programs up on its own PATH.",
        )
        .with_details(json!({"program": missing})),
        OsError::NotFound { .. } => ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "{tool}: the directory {} does not exist, so the command was not run.",
                cwd_text()
            ),
        )
        .with_hint("Fix `cwd` in the extension definition, or create the directory."),
        OsError::InvalidArgument { detail } if cwd.is_some() && detail.contains("directory") => {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{tool}: {} is not a directory, so the command cannot run in it.",
                    cwd_text()
                ),
            )
            .with_hint("Point `cwd` in the extension definition at a directory.")
        }
        OsError::Unsupported { detail } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("{tool}: this system cannot run {program}: {detail}."),
        ),
        OsError::PermissionDenied { detail, .. } => ErrorBody::new(
            ErrorCode::Failed,
            format!("{tool}: the system refused to run {program}: {detail}."),
        )
        .with_hint(
            "Check that the program is executable and that the account legion runs as may run it.",
        ),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("{tool}: {program} could not be started: {other}."),
        ),
    }
}

fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    }
}

fn take_budget(budget: &AtomicUsize, wanted: usize) -> usize {
    let mut left = budget.load(Ordering::Relaxed);
    loop {
        let taken = left.min(wanted);
        match budget.compare_exchange_weak(left, left - taken, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => return taken,
            Err(now) => left = now,
        }
    }
}

/// Returns the last at most `max_bytes` of `text`, cut on a UTF-8 character boundary.
fn tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use legion_os::run::CapturedOutput;

    use super::*;

    fn run_output(
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
        truncated: bool,
    ) -> RunOutput {
        RunOutput {
            exit_code,
            signal: None,
            stdout: CapturedOutput {
                text: stdout.to_owned(),
                truncated,
                total_bytes: stdout.len() as u64,
            },
            stderr: CapturedOutput {
                text: stderr.to_owned(),
                truncated: false,
                total_bytes: stderr.len() as u64,
            },
            timed_out: false,
            cancelled: false,
            killed_for_memory: false,
            peak_rss_bytes: 0,
            stdout_bytes: Vec::new(),
            duration: Duration::from_millis(5),
        }
    }

    #[test]
    fn interprets_json_object_scalar_and_text_results() {
        let obj =
            interpret_output("g.x", 60.0, run_output(Some(0), " {\"a\": 1}\n", "", false)).unwrap();
        assert_eq!(obj, json!({"a": 1}));

        let array =
            interpret_output("g.x", 60.0, run_output(Some(0), "[1, 2]", "", false)).unwrap();
        assert_eq!(array, json!({"value": [1, 2]}));

        let num = interpret_output("g.x", 60.0, run_output(Some(0), "42\n", "", false)).unwrap();
        assert_eq!(num, json!({"value": 42}));

        let text =
            interpret_output("g.x", 60.0, run_output(Some(0), "plain line\n", "", false)).unwrap();
        assert_eq!(text, json!({"output": "plain line\n", "truncated": false}));

        let trunc =
            interpret_output("g.x", 60.0, run_output(Some(0), "{\"a\":1}", "", true)).unwrap();
        assert_eq!(trunc, json!({"output": "{\"a\":1}", "truncated": true}));
    }

    #[test]
    fn non_zero_exit_reports_failed_with_exit_code_and_stderr_tail() {
        let long_err = format!("{}END_ERR", "e".repeat(5000));
        let err =
            interpret_output("g.x", 60.0, run_output(Some(7), "", &long_err, false)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        let details = err.details.unwrap();
        assert_eq!(details["exit_code"], json!(7));
        let stderr = details["stderr"].as_str().unwrap();
        assert!(stderr.ends_with("END_ERR"));
        assert!(stderr.len() <= STDERR_TAIL_BYTES);
    }

    #[test]
    fn tail_respects_utf8_boundaries() {
        let text = "ééé";
        assert_eq!(tail(text, 3), "é");
        assert_eq!(tail(text, 4), "éé");
    }
}
