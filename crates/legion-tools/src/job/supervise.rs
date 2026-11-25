//! Detached job supervisor invoked via the hidden `legion job-supervise <id>` subcommand.
//!
//! Holding an exclusive kernel file lock on `<job_dir>/lock` for its whole lifetime is the
//! liveness proof readers rely on. Output chunks from `legion_os::run` are written straight to
//! `output.log` with `capture_limit(0)` so memory stays bounded regardless of output volume.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use legion_core::{CancellationToken, Paths};
use legion_os::OsError;
use legion_os::atomic::replace_file;
use legion_os::lock::{FileLock, LockMode};
use legion_os::run::{ChunkCallback, OutputChunk, OutputStream, RunOptions, run};
use legion_os::shell::CommandSpec;

use super::platform;
use super::store::{
    self, CANCEL_FILE, EXIT_FILE, ExitOutcome, ExitRecord, JobRecord, LOCK_FILE, LOG_FILE,
    LOG_ROTATED_FILE, MAX_LOG_FILE_BYTES, create_exit_reserve, job_dir, now_rfc3339, read_env_file,
    read_job_record, read_log_meta, remove_env_file, remove_exit_reserve, validate_job_id,
    write_exit_record, write_log_meta,
};

/// Hidden CLI subcommand name used when `job.start` re-executes the current binary.
pub(super) const SUBCOMMAND: &str = "job-supervise";
/// Optional flag carrying the explicit state directory so callers using `Paths::under` (such as
/// tests) do not need to mutate process environment variables.
pub(super) const STATE_DIR_FLAG: &str = "--state-dir";

/// How often the supervisor checks for `<job_dir>/cancel`.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long the supervisor retries taking the exclusive job lock so a reader's brief shared probe
/// landing between lock creation and locking never aborts startup.
const SUPERVISOR_LOCK_TIMEOUT: Duration = Duration::from_secs(1);
/// Maximum partial line bytes buffered per stream before flushing without a newline, keeping
/// supervisor memory strictly bounded while preventing stdout and stderr from splitting lines.
const MAX_LINE_BUFFER_BYTES: usize = 64 * 1024;

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

struct LogWriter {
    dir: PathBuf,
    file: Option<File>,
    current_bytes: u64,
    dropped_bytes: u64,
    cap: u64,
    write_error: Option<String>,
}

impl LogWriter {
    fn open(dir: PathBuf, cap: u64) -> io::Result<Self> {
        let file = platform::open_private_append(&dir.join(LOG_FILE))?;
        let current_bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        let dropped_bytes = read_log_meta(&dir).dropped_bytes;
        Ok(Self {
            dir,
            file: Some(file),
            current_bytes,
            dropped_bytes,
            cap,
            write_error: None,
        })
    }

    fn write_bytes(&mut self, mut bytes: &[u8]) {
        if self.write_error.is_some() || bytes.is_empty() {
            return;
        }
        while !bytes.is_empty() {
            if self.cap > 0
                && self.current_bytes >= self.cap
                && let Err(err) = self.rotate()
            {
                self.write_error = Some(err.to_string());
                return;
            }
            let take = if self.cap == 0 {
                bytes.len()
            } else {
                let room = usize::try_from(self.cap.saturating_sub(self.current_bytes))
                    .unwrap_or(usize::MAX);
                bytes.len().min(room)
            };
            let Some(ref mut file) = self.file else {
                self.write_error = Some("job log file handle is closed".to_owned());
                return;
            };
            if let Err(err) = file.write_all(&bytes[..take]) {
                self.write_error = Some(err.to_string());
                return;
            }
            self.current_bytes = self
                .current_bytes
                .saturating_add(u64::try_from(take).unwrap_or(u64::MAX));
            bytes = &bytes[take..];
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        // Sync and close the active log handle before renaming (required on Windows).
        if let Some(file) = self.file.take() {
            let _ = file.sync_data();
        }
        let active_path = self.dir.join(LOG_FILE);
        let rotated_path = self.dir.join(LOG_ROTATED_FILE);
        let old_rotated_len = fs::metadata(&rotated_path)
            .map(|meta| meta.len())
            .unwrap_or(0);

        replace_file(&active_path, &rotated_path)
            .map_err(|err| io::Error::other(err.to_string()))?;

        if old_rotated_len > 0 {
            self.dropped_bytes = self.dropped_bytes.saturating_add(old_rotated_len);
            let _ = write_log_meta(&self.dir, self.dropped_bytes);
        }

        let fresh = platform::open_private_append(&active_path)?;
        self.file = Some(fresh);
        self.current_bytes = 0;
        Ok(())
    }

    fn finish(&mut self) -> Option<String> {
        if let Some(file) = self.file.take() {
            let _ = file.sync_data();
        }
        self.write_error.take()
    }
}

/// Merges stdout and stderr into `output.log` under one lock, buffering incomplete trailing lines
/// per stream (up to `max_line_buffer`) so the two streams never split each other's lines.
struct LogSink {
    writer: LogWriter,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    max_line_buffer: usize,
}

impl LogSink {
    fn open(dir: PathBuf, cap: u64) -> io::Result<Self> {
        Self::open_with_line_cap(dir, cap, MAX_LINE_BUFFER_BYTES)
    }

    fn open_with_line_cap(dir: PathBuf, cap: u64, max_line_buffer: usize) -> io::Result<Self> {
        Ok(Self {
            writer: LogWriter::open(dir, cap)?,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            max_line_buffer,
        })
    }

    fn write_stream(&mut self, stream: OutputStream, bytes: &[u8]) {
        if self.writer.write_error.is_some() || bytes.is_empty() {
            return;
        }
        let buf = match stream {
            OutputStream::Stdout => &mut self.stdout_buf,
            OutputStream::Stderr => &mut self.stderr_buf,
        };
        if let Some(last_nl) = bytes.iter().rposition(|&b| b == b'\n') {
            if !buf.is_empty() {
                self.writer.write_bytes(buf);
                buf.clear();
            }
            self.writer.write_bytes(&bytes[..=last_nl]);
            let tail = &bytes[last_nl + 1..];
            if !tail.is_empty() {
                if tail.len() > self.max_line_buffer {
                    self.writer.write_bytes(tail);
                } else {
                    buf.extend_from_slice(tail);
                }
            }
        } else if buf.len().saturating_add(bytes.len()) > self.max_line_buffer {
            if !buf.is_empty() {
                self.writer.write_bytes(buf);
                buf.clear();
            }
            self.writer.write_bytes(bytes);
        } else {
            buf.extend_from_slice(bytes);
        }
    }

    fn finish(&mut self) -> Option<String> {
        if !self.stdout_buf.is_empty() {
            self.writer.write_bytes(&self.stdout_buf);
            self.stdout_buf.clear();
        }
        if !self.stderr_buf.is_empty() {
            self.writer.write_bytes(&self.stderr_buf);
            self.stderr_buf.clear();
        }
        self.writer.finish()
    }
}

/// Runs the supervisor lifecycle for one job: acquires the exclusive lock, reserves space for
/// `exit.json`, executes the command without a timeout, streams capped output to `output.log`
/// (rotating once to `output.log.1`), watches for cancellation, writes `exit.json`, and releases
/// the lock.
async fn supervise(state_dir: &Path, id: &str) -> Result<(), String> {
    supervise_with_cap(state_dir, id, MAX_LOG_FILE_BYTES).await
}

async fn supervise_with_cap(state_dir: &Path, id: &str, max_log_bytes: u64) -> Result<(), String> {
    let dir = job_dir(state_dir, id);
    let lock_path = dir.join(LOCK_FILE);

    let lock = match FileLock::acquire_async(
        &lock_path,
        LockMode::Exclusive,
        Some(SUPERVISOR_LOCK_TIMEOUT),
    )
    .await
    {
        Ok(guard) => guard,
        Err(OsError::WouldBlock { .. } | OsError::TimedOut { .. }) => {
            // Another supervisor genuinely holds this job's lock; exit without touching its state.
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
        remove_env_file(&dir);
        remove_exit_reserve(&dir);
        let _ = lock.unlock();
        return Ok(());
    }

    if dir.join(CANCEL_FILE).exists() {
        remove_env_file(&dir);
        let exit = ExitRecord {
            outcome: ExitOutcome::Cancelled,
            exit_code: None,
            signal: None,
            finished: now_rfc3339(),
            forced: false,
            startup_failed: false,
            detail: Some("Cancelled before the command started.".to_owned()),
            result: None,
            error: None,
        };
        let _ = write_exit_record(&dir, &exit);
        let _ = lock.unlock();
        return Ok(());
    }

    // Reserve disk blocks for `exit.json` before starting the command so filling the filesystem
    // cannot lose the exit code; `write_exit_record` removes the reserve file immediately before
    // writing `exit.json`.
    let _ = create_exit_reserve(&dir);

    let record = match read_job_record(&dir, id) {
        Ok(record) => record,
        Err(error) => {
            remove_env_file(&dir);
            record_failure(&dir, error.message);
            let _ = lock.unlock();
            return Ok(());
        }
    };

    let env = match read_env_file(&dir) {
        Ok(env) => {
            // Delete `env.json` as soon as it is loaded into memory so secret values do not sit on
            // disk while the job runs or during retention.
            remove_env_file(&dir);
            env
        }
        Err(message) => {
            remove_env_file(&dir);
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

    let log_sink = match LogSink::open(dir.clone(), max_log_bytes) {
        Ok(sink) => sink,
        Err(err) => {
            record_failure(
                &dir,
                format!("Could not open job log file for writing: {err}."),
            );
            let _ = lock.unlock();
            return Ok(());
        }
    };

    let sink = Arc::new(Mutex::new(log_sink));
    let on_output: ChunkCallback = {
        let sink = Arc::clone(&sink);
        Arc::new(move |chunk: OutputChunk| {
            if let Ok(mut guard) = sink.lock() {
                guard.write_stream(chunk.stream, &chunk.bytes);
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
    // `on_output` appends every raw chunk straight to `output.log` with bounded single rotation.
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
        guard.finish()
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
                startup_failed: false,
                detail: write_error.map(|err| format!("Writing output.log failed: {err}.")),
                result: None,
                error: None,
            }
        }
        Err(os_err) => ExitRecord {
            outcome: ExitOutcome::Failed,
            exit_code: None,
            signal: None,
            finished: now_rfc3339(),
            forced: false,
            startup_failed: false,
            detail: Some(describe_spawn_error(&os_err, &record)),
            result: None,
            error: None,
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
        startup_failed: false,
        detail: Some(detail),
        result: None,
        error: None,
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::job::store::{read_exit_record, write_job_record};

    #[tokio::test]
    async fn supervisor_retries_exclusive_lock_past_transient_shared_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        let id = "lock-probe-race";
        let dir = job_dir(&state_dir, id);
        fs::create_dir_all(&dir).unwrap();

        let record = JobRecord {
            id: id.to_owned(),
            name: "probe-test".to_owned(),
            kind: store::JobKind::Exec,
            title: None,
            key: None,
            cmd: Some("echo lock-retry-ok".to_owned()),
            argv: None,
            cwd: None,
            env_names: Vec::new(),
            principal: "owner".to_owned(),
            device: None,
            created: now_rfc3339(),
            boot_id: "test-boot".to_owned(),
            exclusive: false,
            launching: None,
            detach: None,
        };
        write_job_record(&dir, &record).unwrap();

        // Simulate a reader holding a brief shared lock right as the supervisor starts.
        let shared = FileLock::try_lock_shared(dir.join(LOCK_FILE)).unwrap();
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = shared.unlock();
        });

        supervise(&state_dir, id).await.unwrap();
        releaser.await.unwrap();

        let exit = read_exit_record(&dir).expect("exit.json was not written");
        assert_eq!(exit.outcome, ExitOutcome::Succeeded);
        assert_eq!(exit.exit_code, Some(0));
        assert!(!dir.join(store::EXIT_RESERVE_FILE).exists());
        let log = fs::read_to_string(dir.join(LOG_FILE)).unwrap();
        assert_eq!(log.trim(), "lock-retry-ok");
    }

    #[tokio::test]
    async fn supervisor_rotates_log_at_cap_and_removes_exit_reserve() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        let id = "rotate-cap-test";
        let dir = job_dir(&state_dir, id);
        fs::create_dir_all(&dir).unwrap();

        // Emit 900 bytes (~3.5x a 256-byte cap) with distinct numbered lines and exit with code 3.
        let mut expected = String::new();
        for idx in 0..90 {
            expected.push_str(&format!("line-{idx:04}\n"));
        }
        let payload_file = tmp.path().join("payload.txt");
        fs::write(&payload_file, &expected).unwrap();
        #[cfg(unix)]
        let cmd = format!("cat {:?} && exit 3", payload_file.to_string_lossy());
        #[cfg(windows)]
        let cmd = format!(
            "[Console]::Write([System.IO.File]::ReadAllText({:?})); exit 3",
            payload_file.to_string_lossy()
        );

        let record = JobRecord {
            id: id.to_owned(),
            name: "rotate-test".to_owned(),
            kind: store::JobKind::Exec,
            title: None,
            key: None,
            cmd: Some(cmd),
            argv: None,
            cwd: None,
            env_names: Vec::new(),
            principal: "owner".to_owned(),
            device: None,
            created: now_rfc3339(),
            boot_id: "test-boot".to_owned(),
            exclusive: false,
            launching: None,
            detach: None,
        };
        write_job_record(&dir, &record).unwrap();

        let cap = 256u64;
        supervise_with_cap(&state_dir, id, cap).await.unwrap();

        // Exit code is preserved and `exit.reserve` was removed before writing `exit.json`.
        let exit = read_exit_record(&dir).expect("exit.json missing");
        assert_eq!(exit.outcome, ExitOutcome::Failed);
        assert_eq!(exit.exit_code, Some(3));
        assert!(!dir.join(store::EXIT_RESERVE_FILE).exists());

        // Each segment stays within `cap`, so on-disk storage never exceeds `2 * cap`.
        let rot_len = fs::metadata(dir.join(LOG_ROTATED_FILE)).unwrap().len();
        let cur_len = fs::metadata(dir.join(LOG_FILE)).unwrap().len();
        assert!(rot_len <= cap, "output.log.1 exceeded cap: {rot_len}");
        assert!(cur_len <= cap, "output.log exceeded cap: {cur_len}");
        assert!(rot_len + cur_len <= 2 * cap);

        let meta = read_log_meta(&dir);
        assert!(meta.dropped_bytes > 0, "expected dropped_bytes > 0");
        assert_eq!(
            meta.dropped_bytes + rot_len + cur_len,
            expected.len() as u64
        );

        // `logs::read_offset` reads seamlessly across `output.log.1` + `output.log` and reports
        // `output_dropped: true`.
        let slice = crate::job::logs::read_offset(&dir, 0, 1024, false).unwrap();
        assert!(slice.output_dropped);
        assert_eq!(slice.dropped_bytes, Some(meta.dropped_bytes));
        assert_eq!(slice.offset, meta.dropped_bytes);
        assert_eq!(slice.next_offset, expected.len() as u64);
        assert!(slice.eof);
        assert_eq!(slice.content, &expected[meta.dropped_bytes as usize..]);
    }

    #[test]
    fn log_sink_keeps_interleaved_stream_lines_intact_and_flushes_trailing_partial() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sink = LogSink::open(tmp.path().to_path_buf(), 0).unwrap();

        // Interleave partial UTF-8 and partial lines across stdout and stderr.
        sink.write_stream(OutputStream::Stdout, b"out-1: \xe2\x82");
        sink.write_stream(OutputStream::Stderr, b"err-1: first ");
        sink.write_stream(OutputStream::Stdout, b"\xac\nout-2: half ");
        sink.write_stream(OutputStream::Stderr, b"line\nerr-2: complete\nerr-tail");
        sink.write_stream(OutputStream::Stdout, b"finished\nout-tail");
        assert_eq!(sink.finish(), None);

        let content = fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert_eq!(
            content,
            "out-1: €\nerr-1: first line\nerr-2: complete\nout-2: half finished\nout-tailerr-tail"
        );
    }

    #[test]
    fn log_sink_flushes_overlong_partial_line_to_bound_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sink = LogSink::open_with_line_cap(tmp.path().to_path_buf(), 0, 16).unwrap();

        sink.write_stream(OutputStream::Stdout, b"0123456789");
        assert_eq!(sink.stdout_buf.len(), 10);
        // Pushing 10 more bytes exceeds the 16-byte cap and flushes immediately.
        sink.write_stream(OutputStream::Stdout, b"abcdefghij");
        assert!(sink.stdout_buf.is_empty());
        sink.write_stream(OutputStream::Stdout, b"\n");
        assert_eq!(sink.finish(), None);

        let content = fs::read_to_string(tmp.path().join(LOG_FILE)).unwrap();
        assert_eq!(content, "0123456789abcdefghij\n");
    }

    #[tokio::test]
    async fn late_starting_supervisor_refuses_to_run_after_recorded_startup_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state");
        let id = "late-start-job";
        let dir = job_dir(&state_dir, id);
        fs::create_dir_all(&dir).unwrap();

        let sentinel = tmp.path().join("must-not-exist.txt");
        let cmd = if cfg!(windows) {
            format!(
                "Set-Content -LiteralPath '{}' -Value 'ran'",
                sentinel.display()
            )
        } else {
            format!("printf ran > '{}'", sentinel.display())
        };
        let record = JobRecord {
            id: id.to_owned(),
            name: "late-start".to_owned(),
            kind: store::JobKind::Exec,
            title: None,
            key: Some("late-key".to_owned()),
            cmd: Some(cmd),
            argv: None,
            cwd: None,
            env_names: vec!["SECRET".to_owned()],
            principal: "owner".to_owned(),
            device: None,
            created: now_rfc3339(),
            boot_id: "test-boot".to_owned(),
            exclusive: false,
            launching: None,
            detach: None,
        };
        write_job_record(&dir, &record).unwrap();
        store::write_env_file(
            &dir,
            &std::collections::BTreeMap::from([("SECRET".to_owned(), "value".to_owned())]),
        )
        .unwrap();

        // Simulate `job.start` recording startup failure after `spawn_detached` timed out.
        let startup_exit = ExitRecord {
            outcome: ExitOutcome::Failed,
            exit_code: None,
            signal: None,
            finished: now_rfc3339(),
            forced: false,
            startup_failed: true,
            detail: Some("Could not launch the detached job supervisor: timed out.".to_owned()),
            result: None,
            error: None,
        };
        write_exit_record(&dir, &startup_exit).unwrap();

        // A supervisor process starting late after that timeout must exit without running the
        // command, remove `env.json`, and leave the recorded startup failure untouched.
        supervise_with_cap(&state_dir, id, MAX_LOG_FILE_BYTES)
            .await
            .unwrap();

        assert!(
            !sentinel.exists(),
            "late-starting supervisor must never execute a job whose startup already failed"
        );
        assert!(
            !dir.join(store::ENV_FILE).exists(),
            "late-starting supervisor should clean up env.json"
        );
        let preserved = read_exit_record(&dir).expect("exit.json must remain");
        assert!(preserved.startup_failed);
        assert_eq!(preserved.outcome, ExitOutcome::Failed);
    }
}
