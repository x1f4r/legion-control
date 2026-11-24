//! Detached job supervisor invoked via the hidden `legion job-supervise <id>` subcommand.
//!
//! Holding an exclusive kernel file lock on `<job_dir>/lock` for its whole lifetime is the
//! liveness proof readers rely on. Output chunks from `legion_os::run` are written straight to
//! `output.log` with `capture_limit(0)` so memory stays bounded regardless of output volume.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_core::{CancellationToken, Paths};
use legion_os::OsError;
use legion_os::lock::FileLock;
use legion_os::run::{ChunkCallback, OutputChunk, RunOptions, run};
use legion_os::shell::CommandSpec;

use super::platform;
use super::store::{
    self, CANCEL_FILE, EXIT_FILE, ExitOutcome, ExitRecord, JobRecord, LOCK_FILE, LOG_FILE, job_dir,
    now_rfc3339, read_env_file, read_job_record, validate_job_id, write_exit_record,
};

/// Hidden CLI subcommand name used when `job.start` re-executes the current binary.
pub(super) const SUBCOMMAND: &str = "job-supervise";
/// Optional flag carrying the explicit state directory so callers using `Paths::under` (such as
/// tests) do not need to mutate process environment variables.
pub(super) const STATE_DIR_FLAG: &str = "--state-dir";

/// How often the supervisor checks for `<job_dir>/cancel`.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Runs the hidden `job-supervise` subcommand when `args` starts with `"job-supervise"`.
///
/// Returns `None` for any other command line so callers (such as `legion-cli`'s `main`) can
/// dispatch normally without adding the subcommand to user-facing help.
pub fn try_supervise_cli(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut iter = args.into_iter();
    let first = iter.next()?;
    if first != OsStr::new(SUBCOMMAND) {
        return None;
    }
    let rest: Vec<OsString> = iter.collect();
    Some(run_cli(&rest))
}

fn run_cli(rest: &[OsString]) -> ExitCode {
    let (id, state_dir_opt) = match parse_args(rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            write_stderr(&format!("legion {SUBCOMMAND}: {message}\n"));
            return ExitCode::from(2);
        }
    };
    if let Err(error) = validate_job_id(&id) {
        write_stderr(&format!("legion {SUBCOMMAND}: {}\n", error.message));
        return ExitCode::from(2);
    }

    let state_dir = match state_dir_opt {
        Some(dir) => dir,
        None => match Paths::from_env() {
            Ok(paths) => paths.state_dir().to_path_buf(),
            Err(error) => {
                write_stderr(&format!("legion {SUBCOMMAND}: {}\n", error.message));
                return ExitCode::from(1);
            }
        },
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            write_stderr(&format!(
                "legion {SUBCOMMAND}: failed to start async runtime: {error}\n"
            ));
            return ExitCode::from(1);
        }
    };

    match runtime.block_on(supervise(&state_dir, &id)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            write_stderr(&format!("legion {SUBCOMMAND}: {message}\n"));
            ExitCode::from(1)
        }
    }
}

fn parse_args(rest: &[OsString]) -> Result<(String, Option<PathBuf>), String> {
    let mut id: Option<String> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut index = 0usize;

    while index < rest.len() {
        let arg = &rest[index];
        if arg == OsStr::new(STATE_DIR_FLAG) {
            let Some(value) = rest.get(index + 1) else {
                return Err(format!("{STATE_DIR_FLAG} needs a path."));
            };
            if state_dir.replace(PathBuf::from(value)).is_some() {
                return Err(format!("{STATE_DIR_FLAG} was given twice."));
            }
            index += 2;
            continue;
        }
        if let Some(text) = arg.to_str()
            && let Some(value) = text.strip_prefix("--state-dir=")
        {
            if value.is_empty() {
                return Err(format!("{STATE_DIR_FLAG} needs a path."));
            }
            if state_dir.replace(PathBuf::from(value)).is_some() {
                return Err(format!("{STATE_DIR_FLAG} was given twice."));
            }
            index += 1;
            continue;
        }
        let text = arg
            .to_str()
            .ok_or_else(|| "job id must be valid Unicode.".to_owned())?;
        if id.replace(text.to_owned()).is_some() {
            return Err(format!("unexpected argument {text:?}."));
        }
        index += 1;
    }

    let id = id.ok_or_else(|| "missing job id.".to_owned())?;
    Ok((id, state_dir))
}

struct LogSink {
    file: File,
    write_error: Option<String>,
}

/// Runs the supervisor lifecycle for one job: acquires the exclusive lock, executes the command
/// without a timeout, streams output to `output.log`, watches for cancellation, writes
/// `exit.json`, and releases the lock.
async fn supervise(state_dir: &Path, id: &str) -> Result<(), String> {
    let dir = job_dir(state_dir, id);
    let lock_path = dir.join(LOCK_FILE);

    let lock = match FileLock::try_lock_exclusive(&lock_path) {
        Ok(guard) => guard,
        Err(OsError::WouldBlock { .. }) => {
            // Another supervisor already owns this job; exit without touching its state.
            return Ok(());
        }
        Err(err) => {
            return Err(format!(
                "cannot acquire job lock {}: {err}",
                lock_path.display()
            ));
        }
    };

    // If a forced cancel already finalized the job before the supervisor acquired the lock, do not
    // run the command.
    if dir.join(EXIT_FILE).exists() {
        let _ = lock.unlock();
        return Ok(());
    }

    if dir.join(CANCEL_FILE).exists() {
        let exit = ExitRecord {
            outcome: ExitOutcome::Cancelled,
            exit_code: None,
            signal: None,
            finished: now_rfc3339(),
            forced: false,
            detail: Some("Cancelled before the command started.".to_owned()),
        };
        let _ = write_exit_record(&dir, &exit);
        let _ = lock.unlock();
        return Ok(());
    }

    let record = match read_job_record(&dir, id) {
        Ok(record) => record,
        Err(error) => {
            record_failure(&dir, error.message);
            let _ = lock.unlock();
            return Ok(());
        }
    };

    let env = match read_env_file(&dir) {
        Ok(env) => env,
        Err(message) => {
            record_failure(&dir, format!("Could not read job environment: {message}."));
            let _ = lock.unlock();
            return Ok(());
        }
    };

    let command = match command_spec(&record) {
        Ok(spec) => spec,
        Err(message) => {
            record_failure(&dir, message);
            let _ = lock.unlock();
            return Ok(());
        }
    };

    let log_file = match platform::open_private_append(&dir.join(LOG_FILE)) {
        Ok(file) => file,
        Err(err) => {
            record_failure(
                &dir,
                format!("Could not open job log file for writing: {err}."),
            );
            let _ = lock.unlock();
            return Ok(());
        }
    };

    let sink = Arc::new(Mutex::new(LogSink {
        file: log_file,
        write_error: None,
    }));
    let on_output: ChunkCallback = {
        let sink = Arc::clone(&sink);
        Arc::new(move |chunk: OutputChunk| {
            if let Ok(mut guard) = sink.lock()
                && guard.write_error.is_none()
                && let Err(err) = guard.file.write_all(&chunk.bytes)
            {
                guard.write_error = Some(err.to_string());
            }
        })
    };

    let cancel_token = CancellationToken::new();
    let watcher_stop = CancellationToken::new();
    let cancel_path = dir.join(CANCEL_FILE);
    let watcher = tokio::spawn({
        let cancel_token = cancel_token.clone();
        let watcher_stop = watcher_stop.clone();
        async move {
            loop {
                if cancel_path.exists() {
                    cancel_token.cancel();
                    break;
                }
                tokio::select! {
                    biased;
                    () = watcher_stop.cancelled() => break,
                    () = tokio::time::sleep(CANCEL_POLL_INTERVAL) => {}
                }
            }
        }
    });

    // Bounded memory regardless of output size: `capture_limit(0)` retains nothing in memory while
    // `on_output` appends every raw chunk straight to `output.log`.
    let mut options = RunOptions::new(command)
        .envs(env)
        .capture_limit(0)
        .cancel(cancel_token)
        .on_output(on_output);
    if let Some(ref cwd) = record.cwd {
        options = options.cwd(cwd);
    }

    let run_result = run(options).await;

    watcher_stop.cancel();
    let _ = watcher.await;

    let write_error = if let Ok(mut guard) = sink.lock() {
        let _ = guard.file.sync_data();
        guard.write_error.take()
    } else {
        None
    };
    drop(sink);

    let exit = match run_result {
        Ok(output) => {
            let outcome = if output.cancelled {
                ExitOutcome::Cancelled
            } else if output.exit_code == Some(0) && output.signal.is_none() {
                ExitOutcome::Succeeded
            } else {
                ExitOutcome::Failed
            };
            ExitRecord {
                outcome,
                exit_code: output.exit_code,
                signal: output.signal,
                finished: now_rfc3339(),
                forced: false,
                detail: write_error.map(|err| format!("Writing output.log failed: {err}.")),
            }
        }
        Err(os_err) => ExitRecord {
            outcome: ExitOutcome::Failed,
            exit_code: None,
            signal: None,
            finished: now_rfc3339(),
            forced: false,
            detail: Some(describe_spawn_error(&os_err, &record)),
        },
    };

    let write_res = write_exit_record(&dir, &exit);
    let _ = lock.unlock();
    write_res.map_err(|err| err.message)
}

fn record_failure(dir: &Path, detail: String) {
    let exit = ExitRecord {
        outcome: ExitOutcome::Failed,
        exit_code: None,
        signal: None,
        finished: now_rfc3339(),
        forced: false,
        detail: Some(detail),
    };
    let _ = write_exit_record(dir, &exit);
}

fn command_spec(record: &JobRecord) -> Result<CommandSpec, String> {
    match (&record.cmd, &record.argv) {
        (Some(cmd), None) => Ok(CommandSpec::shell(cmd.clone())),
        (None, Some(argv)) => CommandSpec::from_argv(argv.iter().cloned())
            .map_err(|err| format!("Invalid job command in {}: {err}.", store::JOB_FILE)),
        _ => Err(format!(
            "Job record {} must contain exactly one of `cmd` or `argv`.",
            store::JOB_FILE
        )),
    }
}

fn describe_spawn_error(error: &OsError, record: &JobRecord) -> String {
    match error {
        OsError::ProgramNotFound { program } => {
            format!("There is no program called {program} on this machine.")
        }
        OsError::NotFound { detail } => match record.cwd.as_deref() {
            Some(cwd) => format!("The working directory {cwd} does not exist ({detail})."),
            None => format!("The command could not be started: {detail}."),
        },
        OsError::PermissionDenied { detail, .. } => {
            format!("The system refused to run the command: {detail}.")
        }
        other => format!("The command could not be started: {other}."),
    }
}

fn write_stderr(text: &str) {
    let _ = std::io::stderr().lock().write_all(text.as_bytes());
}
