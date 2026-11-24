//! `exec`: running a command that finishes, with its output captured and bounded.
//!
//! The process runs through `legion-os`, which contains it with everything it starts (a process
//! group, a job object), so a timeout or a cancel stops the whole tree. A cancelled call is not
//! answered until that tree is gone: the run stops it, asking first and forcing after
//! [`KILL_GRACE`], within the [`CANCEL_GRACE`] the node waits for, and only then does the tool
//! return. The run is spawned onto the runtime so that a future dropped anyway (the grace ran out,
//! the session ended) cannot take the run with it half way and leave descendants behind.
//!
//! Its result follows the rule `sys` states for both families: a field without a value is left
//! out, never `null`.

mod text;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use legion_core::proto::{Effect, ErrorBody, ErrorCode, EventKind};
use legion_core::{Access, CANCEL_GRACE, CancellationToken, Ctx, Registry, ToolError};
use legion_os::OsError;
use legion_os::run::{OutputChunk, OutputStream, RunOptions, RunOutput, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Adds the `exec` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "exec.run",
        Effect::Exec,
        "Run a command that finishes and return its exit code and output. Give `argv` to run a program with exactly these arguments, or `cmd` to hand one line to the platform shell (sh on Linux and macOS, PowerShell on Windows). The command is stopped, with everything it started, when `timeout_s` passes or the call is cancelled; a non-zero exit or a timeout is reported in the result, not as an error. Output is streamed as stdout and stderr events while it runs and capped at `max_output` bytes per stream, keeping the start and the end. The command gets legion's environment (except legion's own LEGION_* settings) plus `env`, as a command typed by the account would. Not for anything meant to keep running, such as a server or a watcher: that belongs to job.start. A command can do anything the account can, so a role's `paths` decide only the directory it starts in (the first of them when `cwd` is absent), and a role that must stay confined should not be given exec.",
        run_command,
    );
}

/// Arguments of `exec.run`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunArgs {
    /// A command line handed whole to the platform shell: sh on Linux and macOS, PowerShell on
    /// Windows. Give this or `argv`, not both.
    #[serde(default)]
    cmd: Option<String>,
    /// The program and its arguments, run exactly as given with no shell in between. Give this
    /// or `cmd`, not both.
    #[serde(default)]
    argv: Option<Vec<String>>,
    /// The absolute directory to run in. When absent: the first of the role's `paths` if it has
    /// any, otherwise legion's own.
    #[serde(default)]
    cwd: Option<String>,
    /// Environment variables to set on top of legion's own.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    /// Text written to the command's standard input, which is then closed. Without it the command
    /// reads end of input at once.
    #[serde(default)]
    stdin: Option<String>,
    /// Seconds before the command is stopped, up to 3600; 120 when absent.
    #[serde(default)]
    timeout_s: Option<f64>,
    /// Bytes of output kept per stream, up to 1048576; 65536 when absent. Beyond it the start and
    /// the end are kept and the middle is replaced by a note of how much was left out.
    #[serde(default)]
    max_output: Option<u64>,
}

/// What `exec.run` returns.
#[derive(Debug, Serialize)]
pub struct Ran {
    /// The exit code, when the command exited by itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The signal that ended it, on Linux and macOS, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// Its standard output, as text.
    pub stdout: String,
    /// Its standard error, as text.
    pub stderr: String,
    /// Whether standard output was longer than `max_output` and its middle was left out.
    pub stdout_truncated: bool,
    /// Whether standard error was.
    pub stderr_truncated: bool,
    /// Whether it ran out of time and was stopped.
    pub timed_out: bool,
    /// How long it ran, in milliseconds.
    pub duration_ms: u64,
}

/// How long a command asked to stop (on a timeout or a cancel) has before it is forced. Well inside
/// the node's [`CANCEL_GRACE`], so a cancelled call answers after its tree is gone.
const KILL_GRACE: Duration = Duration::from_secs(2);
const _: () = assert!(KILL_GRACE.as_millis() * 2 < CANCEL_GRACE.as_millis());

/// The default and the largest `timeout_s`.
const TIMEOUT_S: (f64, f64) = (120.0, 3600.0);
/// The default and the largest `max_output`.
const MAX_OUTPUT: (u64, u64) = (64 * 1024, 1024 * 1024);

pub(crate) async fn run_command(ctx: &Ctx, args: RunArgs) -> Result<Ran, ToolError> {
    let (command, program) = command_of(args.cmd, args.argv)?;
    let timeout = timeout_of(args.timeout_s)?;
    let limit = max_output_of(args.max_output)?;
    let env = env_of(
        args.env.unwrap_or_default(),
        std::env::vars_os().map(|(name, _)| name),
    )?;
    let cwd = start_dir(ctx, args.cwd).await?;

    // Each stream is forwarded as events until `limit` bytes have gone out, so a flood of output
    // costs the caller no more than the captured text does.
    let (sender, mut receiver) = mpsc::unbounded_channel::<OutputChunk>();
    let budgets = [
        Arc::new(AtomicUsize::new(limit)),
        Arc::new(AtomicUsize::new(limit)),
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

    // The run's own token, cancelled when the call is (which the loop below watches for) and when
    // this future is dropped without having seen it through.
    let cancel = CancellationToken::new();
    let _stop_on_drop = cancel.clone().drop_guard();
    let mut options = RunOptions::new(command)
        .envs(env)
        .timeout(timeout)
        .capture_limit(limit)
        .kill_grace(KILL_GRACE)
        .cancel(cancel.clone())
        .on_output(forward);
    if let Some(cwd) = &cwd {
        options = options.cwd(cwd);
    }
    if let Some(stdin) = args.stdin {
        options = options.stdin(stdin.into_bytes());
    }
    let mut running = tokio::spawn(run(options));

    let mut decoders: [text::Decoder; 2] = Default::default();
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
    // On a cancel the run is told to stop the tree, and is still awaited: it returns once the tree
    // is gone, so nothing the command started outlives the call's answer.
    let finished = loop {
        tokio::select! {
            Some(chunk) = receiver.recv() => emit(chunk.stream, Some(&chunk.bytes)),
            finished = &mut running => break finished,
            () = ctx.cancelled(), if !cancel.is_cancelled() => cancel.cancel(),
        }
    };
    // Everything read before the run returned is delivered before the result.
    while let Ok(chunk) = receiver.try_recv() {
        emit(chunk.stream, Some(&chunk.bytes));
    }
    // A character the stream never finished is a replacement character in the captured text, and
    // so in the events too.
    emit(OutputStream::Stdout, None);
    emit(OutputStream::Stderr, None);

    let output = finished
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Running the command stopped unexpectedly: {error}."),
            )
        })?
        .map_err(|error| run_error(error, &program, cwd.as_deref()))?;
    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            "The call to exec.run was cancelled, and the command was stopped.",
        ));
    }
    Ok(ran(output))
}

/// Where the command starts: `cwd` when given, checked like any path a tool is handed; otherwise
/// the first of the role's roots when it has any, and legion's own directory when it has none.
///
/// A role's `paths` cannot keep a command inside them, since a command can go anywhere the account
/// can; starting it in a root keeps a cooperating caller where it was meant to work.
///
/// Checking a path resolves every directory on the way, which waits on the file system, so it runs
/// on the blocking pool.
async fn start_dir(ctx: &Ctx, cwd: Option<String>) -> Result<Option<PathBuf>, ErrorBody> {
    let grant = ctx.grant().clone();
    let first_root = || grant.roots().and_then(|roots| roots.into_iter().next());
    let Some(dir) = cwd.or_else(first_root) else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || grant.check_path(dir, Access::Read).map(Some))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Checking the working directory stopped unexpectedly: {error}."),
            )
        })?
}

fn ran(output: RunOutput) -> Ran {
    Ran {
        exit_code: output.exit_code,
        signal: output.signal,
        stdout: output.stdout.text,
        stderr: output.stderr.text,
        stdout_truncated: output.stdout.truncated,
        stderr_truncated: output.stderr.truncated,
        timed_out: output.timed_out,
        duration_ms: u64::try_from(output.duration.as_millis()).unwrap_or(u64::MAX),
    }
}

fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    }
}

/// Takes up to `wanted` bytes from what is left of a stream's budget and returns how many it got.
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

/// The command to run and the program it names, for messages. Exactly one of `cmd` and `argv`.
fn command_of(
    cmd: Option<String>,
    argv: Option<Vec<String>>,
) -> Result<(CommandSpec, String), ErrorBody> {
    let invalid = |message: &str| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("exec.run: {message}")).with_hint(
            "Give `argv`, such as [\"git\", \"status\"], to run a program exactly, or `cmd`, such as \"git status | head\", for the platform shell.",
        )
    };
    match (cmd, argv) {
        (Some(_), Some(_)) => Err(invalid("give `cmd` or `argv`, not both.")),
        (None, None) => Err(invalid("give the command to run, as `cmd` or `argv`.")),
        (Some(cmd), None) if cmd.trim().is_empty() => Err(invalid("`cmd` is empty.")),
        (Some(cmd), None) if cmd.contains('\0') => Err(invalid(
            "`cmd` holds a NUL character, which no command line can carry.",
        )),
        (None, Some(argv)) if argv.iter().any(|arg| arg.contains('\0')) => Err(invalid(
            "an argument in `argv` holds a NUL character, which no command line can carry.",
        )),
        (Some(cmd), None) => {
            let program = cmd.split_whitespace().next().unwrap_or_default().to_owned();
            Ok((CommandSpec::shell(cmd), program))
        }
        (None, Some(argv)) => match argv.first() {
            None => Err(invalid("`argv` is empty.")),
            Some(program) if program.is_empty() => {
                Err(invalid("`argv` starts with an empty program name."))
            }
            Some(program) => {
                let program = program.clone();
                let spec =
                    CommandSpec::from_argv(argv).map_err(|error| invalid(&format!("{error}.")))?;
                Ok((spec, program))
            }
        },
    }
}

fn timeout_of(seconds: Option<f64>) -> Result<Duration, ErrorBody> {
    let seconds = seconds.unwrap_or(TIMEOUT_S.0);
    if !(seconds > 0.0 && seconds <= TIMEOUT_S.1) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "exec.run: `timeout_s` must be more than 0 and at most {}, not {seconds}.",
                TIMEOUT_S.1
            ),
        )
        .with_hint("Anything that needs longer than an hour belongs to job.start."));
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn max_output_of(bytes: Option<u64>) -> Result<usize, ErrorBody> {
    let bytes = bytes.unwrap_or(MAX_OUTPUT.0);
    if bytes > MAX_OUTPUT.1 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "exec.run: `max_output` must be at most {} bytes, not {bytes}.",
                MAX_OUTPUT.1
            ),
        )
        .with_hint("Write long output to a file and read it with the fs tools."));
    }
    Ok(usize::try_from(bytes).unwrap_or(usize::MAX))
}

/// The variables the command gets on top of legion's own: the caller's, and an empty value for each
/// of legion's own `LEGION_*` settings the caller did not set, which are about this node and not
/// for a command it runs (legion reads an empty one as unset). `inherited` names the variables
/// legion has.
fn env_of(
    env: BTreeMap<String, String>,
    inherited: impl IntoIterator<Item = OsString>,
) -> Result<Vec<(OsString, OsString)>, ErrorBody> {
    let mut vars = env
        .into_iter()
        .map(|(name, value)| {
            if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
                Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("exec.run: `env` cannot set {name:?}: a name is not empty and holds no `=`, and neither holds a NUL character."),
                )
                .with_details(json!({"field": format!("env.{name}")})))
            } else {
                Ok((OsString::from(name), OsString::from(value)))
            }
        })
        .collect::<Result<Vec<_>, ErrorBody>>()?;
    let own: Vec<OsString> = inherited
        .into_iter()
        .filter(|name| name.to_string_lossy().starts_with("LEGION_"))
        .filter(|name| vars.iter().all(|(set, _)| set != name))
        .collect();
    vars.extend(own.into_iter().map(|name| (name, OsString::new())));
    Ok(vars)
}

/// Why a command did not start, in the closed codes.
fn run_error(error: OsError, program: &str, cwd: Option<&std::path::Path>) -> ErrorBody {
    let cwd_text = || cwd.map_or_else(String::new, |cwd| cwd.display().to_string());
    match error {
        OsError::ProgramNotFound { program: missing } => ErrorBody::new(
            ErrorCode::NotFound,
            format!("There is no program called {missing} on this machine."),
        )
        .with_hint("Check the spelling, or give its full path; legion looks programs up on its own PATH, which over ssh is often shorter than a login shell's.")
        .with_details(json!({"program": missing})),
        OsError::NotFound { .. } => ErrorBody::new(
            ErrorCode::NotFound,
            format!("The directory {} does not exist, so the command was not run.", cwd_text()),
        )
        .with_hint("Give an existing directory as `cwd`."),
        OsError::InvalidArgument { detail } if cwd.is_some() && detail.contains("directory") => {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{} is not a directory, so the command cannot run in it.", cwd_text()),
            )
            .with_hint("Give a directory as `cwd`.")
        }
        OsError::Unsupported { detail } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("This system cannot run {program}: {detail}."),
        ),
        OsError::PermissionDenied { detail, .. } => ErrorBody::new(
            ErrorCode::Failed,
            format!("The system refused to run {program}: {detail}."),
        )
        .with_hint("Check that it is executable and that the account legion runs as may run it."),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("{program} could not be started: {other}."),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_of_cmd_and_argv_is_taken() {
        let code = |result: Result<(CommandSpec, String), ErrorBody>| result.map_err(|e| e.code);
        assert_eq!(
            code(command_of(None, None)).err(),
            Some(ErrorCode::InvalidArgs)
        );
        assert_eq!(
            code(command_of(Some("x".into()), Some(vec!["x".into()]))).err(),
            Some(ErrorCode::InvalidArgs)
        );
        assert_eq!(
            code(command_of(Some(" ".into()), None)).err(),
            Some(ErrorCode::InvalidArgs)
        );
        assert_eq!(
            code(command_of(None, Some(vec![]))).err(),
            Some(ErrorCode::InvalidArgs)
        );
        assert_eq!(
            code(command_of(None, Some(vec![String::new()]))).err(),
            Some(ErrorCode::InvalidArgs)
        );
        assert_eq!(
            code(command_of(None, Some(vec!["git".into(), "a\0b".into()]))).err(),
            Some(ErrorCode::InvalidArgs)
        );
        assert_eq!(
            code(command_of(Some("echo a\0b".into()), None)).err(),
            Some(ErrorCode::InvalidArgs)
        );
        let unsupported = run_error(
            OsError::Unsupported {
                detail: "no shell".into(),
            },
            "x",
            None,
        );
        assert_eq!(unsupported.code, ErrorCode::Unsupported);
        let (spec, program) = command_of(None, Some(vec!["git".into(), "a b".into()])).unwrap();
        assert_eq!(spec, CommandSpec::argv("git", ["a b"]));
        assert_eq!(program, "git");
        let (spec, program) = command_of(Some("git status | head".into()), None).unwrap();
        assert_eq!(spec, CommandSpec::shell("git status | head"));
        assert_eq!(program, "git");
    }

    #[test]
    fn limits_are_checked() {
        assert_eq!(timeout_of(None).unwrap(), Duration::from_secs(120));
        assert_eq!(timeout_of(Some(0.5)).unwrap(), Duration::from_millis(500));
        for wrong in [0.0, -1.0, 3600.5, f64::NAN, f64::INFINITY] {
            assert!(timeout_of(Some(wrong)).is_err(), "{wrong}");
        }
        assert_eq!(max_output_of(None).unwrap(), 65536);
        assert_eq!(max_output_of(Some(0)).unwrap(), 0);
        assert!(max_output_of(Some(1024 * 1024 + 1)).is_err());
        let none = || std::iter::empty::<OsString>();
        assert!(env_of(BTreeMap::from([("A=B".into(), "x".into())]), none()).is_err());
        assert!(env_of(BTreeMap::from([(String::new(), "x".into())]), none()).is_err());
        assert_eq!(
            env_of(BTreeMap::from([("A".into(), "x=y".into())]), none())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn legion_own_settings_are_emptied_unless_the_caller_sets_them() {
        let inherited = ["PATH", "LEGION_HOME", "LEGION_TOKEN"].map(OsString::from);
        let vars = env_of(
            BTreeMap::from([("LEGION_HOME".into(), "/home/me/other".into())]),
            inherited,
        )
        .unwrap();
        let vars: BTreeMap<OsString, OsString> = vars.into_iter().collect();
        assert_eq!(vars.len(), 2, "{vars:?}");
        assert_eq!(vars[&OsString::from("LEGION_HOME")], "/home/me/other");
        assert_eq!(vars[&OsString::from("LEGION_TOKEN")], "");
    }

    #[test]
    fn the_event_budget_is_shared_out_until_it_is_spent() {
        let budget = AtomicUsize::new(10);
        assert_eq!(take_budget(&budget, 4), 4);
        assert_eq!(take_budget(&budget, 8), 6);
        assert_eq!(take_budget(&budget, 1), 0);
    }
}
