//! `job`: background commands that outlive the call, the `legion` process, and the SSH session
//! that started them.
//!
//! `job.start` writes the job record atomically under `<state dir>/jobs/<id>/`, then re-executes
//! the current binary as a detached supervisor (`legion job-supervise <id>`) via
//! `legion_os::detach::spawn_detached`. The supervisor holds an exclusive kernel file lock on
//! `<job_dir>/lock` for its entire lifetime, streams stdout and stderr to `output.log` in bounded
//! memory, and writes `exit.json` atomically when done. All read tools compute state on the fly
//! without writing to disk.

mod logs;
mod platform;
mod store;
mod supervise;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, Ctx, Paths, Registry, ToolError};
use legion_os::OsError;
use legion_os::detach::{
    DetachHandle, DetachOptions, detached_active, spawn_detached, stop_detached,
};
use legion_os::lock::{FileLock, LockMode};
use legion_os::procid;
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use store::{ExclusiveJob, JobState};
pub use supervise::try_supervise_cli;

use logs::{DEFAULT_LOG_LIMIT, DEFAULT_TAIL_LINES, MAX_LOG_LIMIT, MAX_TAIL_LINES};
use store::{
    ExitOutcome, ExitRecord, JobRecord, command_summary, compare_newest_first, default_job_name,
    derive_status_async, find_by_key, generate_job_id, job_dir, jobs_dir, jobs_lock_path,
    load_all_records, now_rfc3339, probe_lock, prune_retention, read_exit_record, read_job_record,
    remove_env_file, validate_job_id, write_cancel_request, write_env_file, write_exit_record,
    write_job_record,
};

/// Default and maximum `limit` for `job.list`.
const LIST_LIMIT: (u32, u32) = (50, 500);
/// Default and maximum `timeout_s` for `job.wait`.
const WAIT_TIMEOUT_S: (f64, f64) = (60.0, 3600.0);
/// Maximum bytes allowed for an idempotency `key`.
const MAX_KEY_BYTES: usize = 256;
/// Maximum time `job.start` waits for the node-wide jobs lock.
const JOBS_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum time `job.start` waits for the detached supervisor to acquire the job lock or finish.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// Poll interval while `job.start` waits for supervisor readiness.
const STARTUP_POLL: Duration = Duration::from_millis(20);
/// Cooperative grace period `job.cancel` gives the supervisor before escalating to `stop_detached`.
const CANCEL_GRACE: Duration = Duration::from_secs(6);
/// Poll interval during `job.cancel`.
const CANCEL_POLL: Duration = Duration::from_millis(50);
/// Settle window waiting for the kernel to release `lock` after `stop_detached`.
const LOCK_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Initial and maximum poll interval for `job.wait`.
const WAIT_INITIAL_POLL: Duration = Duration::from_millis(25);
const WAIT_MAX_POLL: Duration = Duration::from_millis(100);

/// Adds the `job` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "job.start",
        Effect::Exec,
        "Start a command as a detached background job that outlives this call, this legion process, and any ssh session. Give `argv` to run a program with exact arguments or `cmd` for the platform shell (sh on Linux and macOS, PowerShell on Windows). Returns as soon as the job supervisor holds its lock; afterwards poll `job.logs` with `next_offset` to read output as it arrives, call `job.wait` to wait for completion, or inspect `job.status`. Pass `key` to make retries after a dropped connection idempotent: a key matches an active job or one whose supervisor took the lock and ran (succeeded, failed, cancelled, or interrupted mid-run); a failed start whose supervisor never took the lock does not match and is replaced. Set `exclusive: true` when the job should mark this machine busy.",
        start,
    );
    registry.add_fn(
        "job.list",
        Effect::Read,
        "List background jobs recorded on this node, newest first, with their id, name, current derived state, timestamps, exit code, and command summary. Filter by `state` to find running, failed, or interrupted jobs. Any principal with the read effect sees every job's command and output on this node. Reading never modifies job files.",
        list,
    );
    registry.add_fn(
        "job.status",
        Effect::Read,
        "Inspect one background job's full record (without environment values), current derived state, exit code or signal, duration, output size, and failure or interruption detail without modifying anything on disk. Pass an `id` from `job.start` or `job.list`; use `job.logs` to read output or `job.wait` to wait until it finishes.",
        status,
    );
    registry.add_fn(
        "job.logs",
        Effect::Read,
        "Read captured stdout and stderr from a background job without loading the whole log into memory. Pass `offset` (feeding back `next_offset` from each reply) to stream new output incrementally while the job runs, or omit `offset` to read the last `tail` lines (default 100, max 5000). Replies are capped at `limit` bytes (default 65536, max 1048576), never split a UTF-8 character, and replace invalid UTF-8 bytes. Large logs rotate once into `output.log.1` and are read across both segments transparently; `output_dropped` and `dropped_bytes` report when earlier output was discarded by rotation. Any principal with the read effect sees every job's command and output on this node.",
        read_logs,
    );
    registry.add_fn(
        "job.wait",
        Effect::Read,
        "Wait until a background job is no longer starting or running and return the same status object as `job.status`, plus `timed_out`. Returns as soon as the job reaches a terminal state, or returns with `timed_out: true` when `timeout_s` elapses (default 60, max 3600). Stops waiting immediately if this call is cancelled. Use this after `job.start` instead of polling `job.status` in a tight loop.",
        wait,
    );
    registry.add_fn(
        "job.cancel",
        Effect::Exec,
        "Stop a running background job and its entire process tree. Asks the job supervisor to terminate the command cleanly and escalates to stopping the detached process group or systemd scope if it does not finish within a short grace period; set `force: true` to stop the detached handle immediately. Returns only once the job is gone or reports any surviving process ids. Any principal with the exec effect can cancel any job on this node. Cancelling a job that already finished is not an error and returns its final state.",
        cancel,
    );
}

/// Lists every `exclusive` background job that is currently starting or running on this node.
///
/// Used by busy checks (such as `power.*`) to see whether exclusive jobs are active. Pure read:
/// never writes or creates files under `paths.state_dir()`.
pub fn exclusive_running_jobs(paths: &Paths) -> Result<Vec<ExclusiveJob>, ErrorBody> {
    store::exclusive_running_jobs_in(paths.state_dir())
}

/// Arguments of `job.start`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartArgs {
    /// A command line handed whole to the platform shell: sh on Linux and macOS, PowerShell on
    /// Windows. Give this or `argv`, not both.
    #[serde(default)]
    cmd: Option<String>,
    /// The program and its arguments, run exactly as given with no shell in between. Give this
    /// or `cmd`, not both.
    #[serde(default)]
    argv: Option<Vec<String>>,
    /// The absolute directory to run in (accepts `~`); legion's own when absent.
    #[serde(default)]
    cwd: Option<String>,
    /// Environment variables to set on top of legion's own. Values are stored in a private file
    /// and never included in job records or replies.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    /// Short human-readable name for the job; defaults to the command's program name.
    #[serde(default)]
    name: Option<String>,
    /// Caller-chosen idempotency key. Matches an active job or one whose supervisor took the lock
    /// and ran (succeeded, failed, cancelled, or interrupted mid-run): re-calling with the same
    /// key and identical command, `cwd`, and environment variable names returns that job instead
    /// of starting a second one, while differing arguments fail with `conflict`. A failed start
    /// whose supervisor never took the lock does not match and is replaced.
    #[serde(default)]
    key: Option<String>,
    /// Mark this machine busy while the job runs (blocks unattended `power.*` operations).
    #[serde(default)]
    exclusive: bool,
}

/// What `job.start` returns.
#[derive(Debug, Serialize)]
pub struct Started {
    /// The job's identifier; pass it to `job.status`, `job.logs`, `job.wait`, or `job.cancel`.
    pub id: String,
    /// The job's name.
    pub name: String,
    /// The job's current derived state.
    pub state: JobState,
    /// When the job was created (RFC 3339 UTC).
    pub created: String,
    /// `true` when an existing job with the same `key` was returned instead of starting a new one.
    pub existing: bool,
    /// Non-fatal launch warnings from the platform detach layer (for example disabled systemd
    /// lingering).
    pub warnings: Vec<String>,
}

/// Arguments of `job.list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Keep only jobs currently in this derived state.
    #[serde(default)]
    state: Option<JobState>,
    /// Maximum number of jobs to return, 1 to 500; 50 when absent.
    #[serde(default)]
    #[schemars(range(min = 1, max = 500))]
    limit: Option<u32>,
}

/// One summary entry returned by `job.list`.
#[derive(Debug, Serialize)]
pub struct JobSummary {
    /// The job's identifier.
    pub id: String,
    /// The job's name.
    pub name: String,
    /// Current derived state.
    pub state: JobState,
    /// When the job was created (RFC 3339 UTC).
    pub created: String,
    /// When the job finished (RFC 3339 UTC), when it has an exit record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    /// Process exit code, when the command exited normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Command string or joined argv, cut at 200 characters.
    pub command: String,
}

/// What `job.list` returns.
#[derive(Debug, Serialize)]
pub struct JobList {
    /// Matching jobs, newest first.
    pub jobs: Vec<JobSummary>,
    /// Whether more matching jobs exist beyond `limit`.
    pub truncated: bool,
}

/// Arguments of `job.status`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    /// The job identifier returned by `job.start` or `job.list`.
    id: String,
}

/// What `job.status` returns.
#[derive(Debug, Serialize)]
pub struct JobStatus {
    /// The job's identifier.
    pub id: String,
    /// The job's name.
    pub name: String,
    /// Caller-chosen idempotency key, when one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Shell command string, when started with `cmd`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    /// Exact argument vector, when started with `argv`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// Working directory, when one was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Names of environment variables supplied to the job (values are never returned).
    pub env_names: Vec<String>,
    /// Principal that started the job.
    pub principal: String,
    /// When the job was created (RFC 3339 UTC).
    pub created: String,
    /// System boot identifier recorded when the job was created.
    pub boot_id: String,
    /// Whether the job marks the node busy while running.
    pub exclusive: bool,
    /// Platform detach handle of the supervisor once started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detach: Option<DetachHandle>,
    /// Current derived state.
    pub state: JobState,
    /// Whether the job supervisor is attached while the job is active (`false` when the supervisor
    /// died while the command tree keeps running; omitted once finished or interrupted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supervised: Option<bool>,
    /// Exit code when the command exited normally.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Signal number that ended the command on Unix, when one did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// When the job finished (RFC 3339 UTC), when an exit record exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    /// Duration in seconds (elapsed so far while running, or total once finished).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    /// Current size of captured log files (`output.log.1` and `output.log`) in bytes.
    pub output_bytes: u64,
    /// Explanation when the job was interrupted or failed to start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Arguments of `job.logs`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    /// The job identifier returned by `job.start` or `job.list`.
    id: String,
    /// Byte offset to read forward from (feed back `next_offset` to poll incrementally). Omit to
    /// read the last `tail` lines instead.
    #[serde(default)]
    offset: Option<u64>,
    /// Number of trailing lines to read when `offset` is omitted (1 to 5000; 100 by default).
    #[serde(default)]
    #[schemars(range(min = 1, max = 5000))]
    tail: Option<u32>,
    /// Maximum bytes to return in one reply (1 to 1048576; 65536 by default).
    #[serde(default)]
    #[schemars(range(min = 1, max = 1048576))]
    limit: Option<u64>,
}

/// What `job.logs` returns.
#[derive(Debug, Serialize)]
pub struct JobLogs {
    /// The job's identifier.
    pub id: String,
    /// Current derived state of the job.
    pub state: JobState,
    /// Log text for the returned byte range (invalid UTF-8 replaced with U+FFFD).
    pub content: String,
    /// Logical byte offset where `content` starts in the job's output stream.
    pub offset: u64,
    /// Logical byte offset immediately after `content`; pass as `offset` on the next call to
    /// follow output.
    pub next_offset: u64,
    /// Whether this read reached the current end of captured output.
    pub eof: bool,
    /// Whether bytes exist outside the returned range (earlier bytes in tail mode, or later bytes
    /// in offset mode).
    pub truncated: bool,
    /// `true` when earlier output was discarded because the log rotated past its size cap.
    pub output_dropped: bool,
    /// Number of earliest bytes dropped due to log rotation, present when `output_dropped` is
    /// `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
}

/// Arguments of `job.wait`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    /// The job identifier returned by `job.start` or `job.list`.
    id: String,
    /// Seconds to wait before returning with `timed_out: true` (up to 3600; 60 when absent).
    #[serde(default)]
    timeout_s: Option<f64>,
}

/// What `job.wait` returns: the full [`JobStatus`] plus whether the wait timed out.
#[derive(Debug, Serialize)]
pub struct JobWait {
    /// Full job status at the moment `job.wait` returned.
    #[serde(flatten)]
    pub status: JobStatus,
    /// `true` when `timeout_s` elapsed while the job was still starting or running.
    pub timed_out: bool,
}

/// Arguments of `job.cancel`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CancelArgs {
    /// The job identifier returned by `job.start` or `job.list`.
    id: String,
    /// Skip waiting for the supervisor to cancel cooperatively and terminate the detached process
    /// tree or systemd scope immediately.
    #[serde(default)]
    force: bool,
}

/// What `job.cancel` returns.
#[derive(Debug, Serialize)]
pub struct CancelledJob {
    /// The job's identifier.
    pub id: String,
    /// Final derived state of the job.
    pub state: JobState,
    /// Process ids still alive if the system refused to stop them.
    pub survivors: Vec<u32>,
}

pub(crate) async fn start(ctx: &Ctx, args: StartArgs) -> Result<Started, ToolError> {
    validate_command(args.cmd.as_deref(), args.argv.as_deref())?;
    let env = args.env.unwrap_or_default();
    validate_env(&env)?;
    let env_names: Vec<String> = env.keys().cloned().collect();
    let explicit_name = validate_name(args.name)?;
    let key = validate_key(args.key)?;

    let cwd_path = args
        .cwd
        .as_deref()
        .map(|cwd| ctx.grant().check_path(cwd, Access::Read))
        .transpose()?;
    if let Some(ref cwd) = cwd_path {
        validate_existing_dir(cwd).await?;
    }
    let cwd_str = cwd_path.map(|path| path.to_string_lossy().into_owned());

    let state_dir = ctx.paths().state_dir().to_path_buf();
    let principal = ctx.principal().to_owned();

    // Ensure `<state dir>` exists before taking the node-wide jobs lock.
    let state_dir_for_prepare = state_dir.clone();
    blocking("job.start", move || {
        std::fs::create_dir_all(&state_dir_for_prepare).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Cannot create state directory {}: {err}.",
                    state_dir_for_prepare.display()
                ),
            )
        })
    })
    .await?;

    let lock_path = jobs_lock_path(&state_dir);
    let jobs_lock =
        FileLock::acquire_async(&lock_path, LockMode::Exclusive, Some(JOBS_LOCK_TIMEOUT))
            .await
            .map_err(|err| match err {
                OsError::TimedOut { .. } => ErrorBody::new(
                    ErrorCode::Timeout,
                    "Timed out waiting for another job.start call to finish.",
                )
                .with_hint("Retry the call shortly."),
                other => ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Cannot lock job state {}: {other}.", lock_path.display()),
                ),
            })?;

    // Check idempotency key, prune expired finished jobs, and write `job.json` + `env.json`
    // atomically while holding the node-wide jobs lock.
    let state_dir_for_create = state_dir.clone();
    let create_outcome = blocking("job.start", move || {
        let now = jiff::Timestamp::now();
        let boot = procid::boot_id().ok();
        if let Some(ref key_str) = key
            && let Some((_existing_dir, existing, derived)) =
                find_by_key(&state_dir_for_create, key_str, boot.as_deref(), now)?
        {
            if existing.cmd == args.cmd
                && existing.argv == args.argv
                && existing.cwd == cwd_str
                && existing.env_names == env_names
            {
                return Ok(CreateOutcome::Existing(Started {
                    id: existing.id,
                    name: existing.name,
                    state: derived.state,
                    created: existing.created,
                    existing: true,
                    warnings: Vec::new(),
                }));
            }
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "A job with key {key_str:?} (id {}) already exists with a different command, working directory, or environment.",
                    existing.id
                ),
            )
            .with_hint(
                "Use a new `key` for a different command, or inspect the existing job with `job.status`.",
            )
            .with_details(json!({"id": existing.id, "key": key_str})));
        }

        let jobs_root = jobs_dir(&state_dir_for_create);
        platform::ensure_private_dir(&jobs_root).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Cannot create jobs directory {}: {err}.",
                    jobs_root.display()
                ),
            )
        })?;

        prune_retention(&state_dir_for_create, now);

        let (id, dir) = allocate_job_dir(&state_dir_for_create)?;
        let name = explicit_name
            .unwrap_or_else(|| default_job_name(args.cmd.as_deref(), args.argv.as_deref(), &id));
        let boot_id = procid::boot_id().map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot determine the system boot id: {err}."),
            )
        })?;
        let record = JobRecord {
            id,
            name,
            key,
            cmd: args.cmd,
            argv: args.argv,
            cwd: cwd_str,
            env_names,
            principal,
            created: store::format_timestamp(now),
            boot_id,
            exclusive: args.exclusive,
            detach: None,
        };

        if !env.is_empty() {
            write_env_file(&dir, &env)?;
        }
        write_job_record(&dir, &record)?;
        Ok(CreateOutcome::Created {
            dir,
            record: Box::new(record),
        })
    })
    .await;

    let _ = jobs_lock.unlock();

    let (dir, mut record) = match create_outcome? {
        CreateOutcome::Existing(started) => return Ok(started),
        CreateOutcome::Created { dir, record } => (dir, *record),
    };

    let exe = match legion_core::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            record_start_failure(&dir, &err.message).await;
            return Err(err);
        }
    };

    let supervisor_cmd = CommandSpec::argv(
        exe,
        [
            supervise::SUBCOMMAND.to_owned(),
            record.id.clone(),
            supervise::STATE_DIR_FLAG.to_owned(),
            state_dir.to_string_lossy().into_owned(),
        ],
    );

    let handle = match spawn_detached(DetachOptions::new(supervisor_cmd).name(&record.id)).await {
        Ok(handle) => handle,
        Err(err) => {
            let message = format!("Could not launch the detached job supervisor: {err}.");
            record_start_failure(&dir, &message).await;
            return Err(ErrorBody::new(ErrorCode::Failed, message)
                .with_hint("Check `job.status` for the recorded failure details."));
        }
    };

    let warnings = handle.warnings.clone();
    record.detach = Some(handle.clone());
    let dir_for_update = dir.clone();
    let record_for_update = record.clone();
    blocking("job.start", move || {
        write_job_record(&dir_for_update, &record_for_update)
    })
    .await?;

    let state = wait_for_supervisor_ready(&dir, &record.id, &handle).await?;

    Ok(Started {
        id: record.id,
        name: record.name,
        state,
        created: record.created,
        existing: false,
        warnings,
    })
}

enum CreateOutcome {
    Existing(Started),
    Created {
        dir: PathBuf,
        record: Box<JobRecord>,
    },
}

fn allocate_job_dir(state_dir: &Path) -> Result<(String, PathBuf), ErrorBody> {
    for _ in 0..32 {
        let id = generate_job_id();
        let dir = job_dir(state_dir, &id);
        match platform::create_private_dir_new(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("Cannot create job directory {}: {err}.", dir.display()),
                ));
            }
        }
    }
    Err(ErrorBody::new(
        ErrorCode::Internal,
        "Could not allocate a unique job directory after repeated attempts.",
    ))
}

async fn record_start_failure(dir: &Path, detail: &str) {
    let dir = dir.to_path_buf();
    let exit = ExitRecord {
        outcome: ExitOutcome::Failed,
        exit_code: None,
        signal: None,
        finished: now_rfc3339(),
        forced: false,
        startup_failed: true,
        detail: Some(detail.to_owned()),
    };
    let _ = tokio::task::spawn_blocking(move || {
        // Never overwrite an existing exit record or write one while the supervisor holds `lock`.
        if read_exit_record(&dir).is_some() || probe_lock(&dir).1 {
            return Ok(());
        }
        remove_env_file(&dir);
        write_exit_record(&dir, &exit)
    })
    .await;
}

/// Waits until the newly detached supervisor holds the job lock or has already written `exit.json`.
async fn wait_for_supervisor_ready(
    dir: &Path,
    id: &str,
    handle: &DetachHandle,
) -> Result<JobState, ErrorBody> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        let probe_dir = dir.to_path_buf();
        let proc_id = handle.process;
        let check = blocking("job.start", move || {
            if let Some(exit) = read_exit_record(&probe_dir) {
                return Ok(ReadyProbe::Finished(JobState::from(exit.outcome)));
            }
            if probe_lock(&probe_dir).1 {
                return Ok(ReadyProbe::Running);
            }
            if let Some(exit) = read_exit_record(&probe_dir) {
                return Ok(ReadyProbe::Finished(JobState::from(exit.outcome)));
            }
            if !procid::is_alive(&proc_id) {
                // Re-check once after seeing the process exit in case `exit.json` landed last.
                if let Some(exit) = read_exit_record(&probe_dir) {
                    return Ok(ReadyProbe::Finished(JobState::from(exit.outcome)));
                }
                return Ok(ReadyProbe::SupervisorExited);
            }
            Ok(ReadyProbe::Pending)
        })
        .await?;

        match check {
            ReadyProbe::Running => return Ok(JobState::Running),
            ReadyProbe::Finished(state) => return Ok(state),
            ReadyProbe::SupervisorExited => {
                if detached_active(handle).await.unwrap_or(false) {
                    return Ok(JobState::Running);
                }
                let message =
                    "The detached job supervisor exited before taking the job lock.".to_owned();
                record_start_failure(dir, &message).await;
                return Err(ErrorBody::new(ErrorCode::Failed, message).with_hint(
                    "Check `job.status` and `meta.doctor` for state directory permissions.",
                ));
            }
            ReadyProbe::Pending => {
                let now = Instant::now();
                if now >= deadline {
                    let stop_result = stop_detached(handle, Duration::from_secs(1)).await;
                    return finish_startup_timeout(
                        dir,
                        id,
                        handle,
                        stop_result,
                        LOCK_SETTLE_TIMEOUT,
                    )
                    .await;
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(STARTUP_POLL.min(remaining)).await;
            }
        }
    }
}

/// Finalizes a `job.start` startup timeout after attempting `stop_detached`.
///
/// Writes `exit.json` only once the stop succeeded with no survivors, the lock is free, and
/// `detached_active` is false; otherwise leaves `exit.json` unwritten and reports that the job is
/// still running along with any surviving process ids.
async fn finish_startup_timeout(
    dir: &Path,
    id: &str,
    handle: &DetachHandle,
    stop_result: Result<procid::KillTreeReport, OsError>,
    lock_settle: Duration,
) -> Result<JobState, ErrorBody> {
    let (stop_err, survivors) = match stop_result {
        Ok(report) => (
            None,
            report
                .survivors
                .into_iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
        ),
        Err(err) => (Some(err.to_string()), Vec::new()),
    };

    if stop_err.is_none() && survivors.is_empty() {
        wait_for_lock_release(dir, lock_settle).await;
    }

    let tree_active = if stop_err.is_some() || !survivors.is_empty() {
        true
    } else {
        detached_active(handle).await.unwrap_or(true)
    };

    let dir_check = dir.to_path_buf();
    let (existing_exit, lock_held) = blocking("job.start", move || {
        let exit = read_exit_record(&dir_check);
        let lock_held = probe_lock(&dir_check).1;
        let exit = exit.or_else(|| read_exit_record(&dir_check));
        Ok((exit, lock_held))
    })
    .await?;

    if let Some(exit) = existing_exit
        && !tree_active
        && !lock_held
    {
        return Ok(JobState::from(exit.outcome));
    }

    if stop_err.is_none() && survivors.is_empty() && !tree_active && !lock_held {
        let message =
            "The detached job supervisor did not become ready within 10 seconds and was stopped."
                .to_owned();
        record_start_failure(dir, &message).await;
        return Err(ErrorBody::new(ErrorCode::Timeout, message)
            .with_hint("Retry `job.start` or check system load with `sys.info`.")
            .with_details(json!({
                "id": id,
                "state": JobState::Failed,
                "survivors": [],
            })));
    }

    let detail = match stop_err {
        Some(err) => format!(
            "The detached job supervisor for {id} did not become ready within 10 seconds and stopping it failed ({err}); the job is still running."
        ),
        None if !survivors.is_empty() => format!(
            "The detached job supervisor for {id} did not become ready within 10 seconds and could not be stopped; the job is still running (surviving pids: {survivors:?})."
        ),
        None => format!(
            "The detached job supervisor for {id} did not become ready within 10 seconds and is still running."
        ),
    };
    Err(ErrorBody::new(ErrorCode::Timeout, detail)
        .with_hint(
            "Inspect the job with `job.status` or stop it with `job.cancel` (`force: true`).",
        )
        .with_details(json!({
            "id": id,
            "state": JobState::Running,
            "survivors": survivors,
        })))
}

enum ReadyProbe {
    Running,
    Finished(JobState),
    SupervisorExited,
    Pending,
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<JobList, ToolError> {
    let limit = args.limit.unwrap_or(LIST_LIMIT.0);
    if limit == 0 || limit > LIST_LIMIT.1 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "job.list: `limit` must be between 1 and {}, not {limit}.",
                LIST_LIMIT.1
            ),
        )
        .with_hint("Filter by `state` to narrow the list instead of asking for more."));
    }
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let filter_state = args.state;
    let keep = usize::try_from(limit).unwrap_or(usize::MAX);

    let (records, boot) = blocking("job.list", move || {
        let mut records = load_all_records(&state_dir)?;
        records.sort_by(|(_, a), (_, b)| compare_newest_first(a, b));
        let boot = procid::boot_id().ok();
        Ok((records, boot))
    })
    .await?;

    let now = jiff::Timestamp::now();
    let mut jobs = Vec::new();
    let mut truncated = false;
    for (dir, record) in records {
        let derived = derive_status_async(&dir, &record, boot.as_deref(), now).await;
        if filter_state.is_some_and(|want| derived.state != want) {
            continue;
        }
        if jobs.len() == keep {
            truncated = true;
            break;
        }
        jobs.push(JobSummary {
            command: command_summary(&record),
            id: record.id,
            name: record.name,
            state: derived.state,
            created: record.created,
            finished: derived.finished,
            exit_code: derived.exit_code,
        });
    }
    Ok(JobList { jobs, truncated })
}

async fn status(ctx: &Ctx, args: StatusArgs) -> Result<JobStatus, ToolError> {
    validate_job_id(&args.id)?;
    let state_dir = ctx.paths().state_dir().to_path_buf();
    load_status_async(&state_dir, &args.id).await
}

async fn load_status_async(state_dir: &Path, id: &str) -> Result<JobStatus, ErrorBody> {
    let dir = job_dir(state_dir, id);
    let dir_for_read = dir.clone();
    let id_for_read = id.to_owned();
    let (record, boot) = blocking("job.status", move || {
        let record = read_job_record(&dir_for_read, &id_for_read)?;
        let boot = procid::boot_id().ok();
        Ok((record, boot))
    })
    .await?;

    let now = jiff::Timestamp::now();
    let derived = derive_status_async(&dir, &record, boot.as_deref(), now).await;
    Ok(JobStatus {
        id: record.id,
        name: record.name,
        key: record.key,
        cmd: record.cmd,
        argv: record.argv,
        cwd: record.cwd,
        env_names: record.env_names,
        principal: record.principal,
        created: record.created,
        boot_id: record.boot_id,
        exclusive: record.exclusive,
        detach: record.detach,
        state: derived.state,
        supervised: derived.supervised,
        exit_code: derived.exit_code,
        signal: derived.signal,
        finished: derived.finished,
        duration_s: derived.duration_s,
        output_bytes: derived.output_bytes,
        detail: derived.detail,
    })
}

async fn read_logs(ctx: &Ctx, args: LogsArgs) -> Result<JobLogs, ToolError> {
    validate_job_id(&args.id)?;
    if args.offset.is_some() && args.tail.is_some() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "job.logs: give `offset` or `tail`, not both.",
        )
        .with_hint(
            "Pass `offset` (starting at 0 or a previous `next_offset`) to poll incrementally, or omit `offset` and use `tail` for the last lines.",
        ));
    }
    let tail = args.tail.unwrap_or(DEFAULT_TAIL_LINES);
    if tail == 0 || tail > MAX_TAIL_LINES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("job.logs: `tail` must be between 1 and {MAX_TAIL_LINES}, not {tail}."),
        )
        .with_hint("Use `offset` to page through longer logs incrementally."));
    }
    let limit = args.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if limit == 0 || limit > MAX_LOG_LIMIT {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("job.logs: `limit` must be between 1 and {MAX_LOG_LIMIT} bytes, not {limit}."),
        )
        .with_hint("Poll with `next_offset` to read larger logs in chunks."));
    }

    let state_dir = ctx.paths().state_dir().to_path_buf();
    let id = args.id;
    let offset = args.offset;
    let tail_usize = usize::try_from(tail).unwrap_or(usize::MAX);
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);

    let dir = job_dir(&state_dir, &id);
    let dir_for_read = dir.clone();
    let id_for_read = id.clone();
    let (record, boot) = blocking("job.logs", move || {
        let record = read_job_record(&dir_for_read, &id_for_read)?;
        let boot = procid::boot_id().ok();
        Ok((record, boot))
    })
    .await?;

    let derived = derive_status_async(&dir, &record, boot.as_deref(), jiff::Timestamp::now()).await;
    let actively_writing = derived.supervised == Some(true);

    let slice = blocking("job.logs", move || match offset {
        Some(off) => logs::read_offset(&dir, off, limit_usize, actively_writing),
        None => logs::read_tail(&dir, tail_usize, limit_usize, actively_writing),
    })
    .await?;

    Ok(JobLogs {
        id,
        state: derived.state,
        content: slice.content,
        offset: slice.offset,
        next_offset: slice.next_offset,
        eof: slice.eof,
        truncated: slice.truncated,
        output_dropped: slice.output_dropped,
        dropped_bytes: slice.dropped_bytes,
    })
}

async fn wait(ctx: &Ctx, args: WaitArgs) -> Result<JobWait, ToolError> {
    validate_job_id(&args.id)?;
    let timeout = wait_timeout_of(args.timeout_s)?;
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let id = args.id;

    let deadline = tokio::time::Instant::now() + timeout;
    let mut pause = WAIT_INITIAL_POLL;

    loop {
        let current = load_status_async(&state_dir, &id).await?;
        if !current.state.is_active() {
            return Ok(JobWait {
                status: current,
                timed_out: false,
            });
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(JobWait {
                status: current,
                timed_out: true,
            });
        }
        let remaining = deadline.saturating_duration_since(now);
        tokio::select! {
            biased;
            () = ctx.cancelled() => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    "The call to job.wait was cancelled before the job finished.",
                ));
            }
            () = tokio::time::sleep(pause.min(remaining)) => {}
        }
        pause = (pause * 2).min(WAIT_MAX_POLL);
    }
}

async fn cancel(ctx: &Ctx, args: CancelArgs) -> Result<CancelledJob, ToolError> {
    validate_job_id(&args.id)?;
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let id = args.id;
    let dir = job_dir(&state_dir, &id);

    let dir_for_read = dir.clone();
    let id_for_read = id.clone();
    let (initial_record, current_boot) = blocking("job.cancel", move || {
        let record = read_job_record(&dir_for_read, &id_for_read)?;
        let boot = procid::boot_id().ok();
        Ok((record, boot))
    })
    .await?;
    let same_boot = current_boot
        .as_deref()
        .is_none_or(|cur| procid::same_boot(&initial_record.boot_id, cur));

    let stop_grace = if args.force {
        Duration::ZERO
    } else {
        Duration::from_secs(1)
    };

    // Never touch records from a previous boot: nothing from that boot can still be running, so
    // report the true derived state without writing `cancel` or `exit.json`.
    if !same_boot {
        let derived = derive_status_async(
            &dir,
            &initial_record,
            current_boot.as_deref(),
            jiff::Timestamp::now(),
        )
        .await;
        return Ok(CancelledJob {
            id,
            state: derived.state,
            survivors: Vec::new(),
        });
    }

    // Cancelling a job that already has an exit record is not an error: wait briefly if the
    // supervisor is still releasing its lock, sweep any surviving detached descendants on the
    // same boot, and return the job's final state unchanged (or `running` if descendants survive).
    let dir_for_exit = dir.clone();
    if let Some(exit) = blocking("job.cancel", move || Ok(read_exit_record(&dir_for_exit))).await? {
        wait_for_lock_release(&dir, LOCK_SETTLE_TIMEOUT).await;
        let record = reload_job_record(&dir, &id, initial_record).await;
        let (survivors, still_active) =
            sweep_job_handle(&id, &record, same_boot, stop_grace).await?;
        let state = if still_active {
            JobState::Running
        } else {
            JobState::from(exit.outcome)
        };
        return Ok(CancelledJob {
            id,
            state,
            survivors,
        });
    }

    // If the job is already dead (`interrupted`) and nothing under its handle is active, leave its
    // directory untouched so cancel never rewrites history.
    let initial_status = derive_status_async(
        &dir,
        &initial_record,
        current_boot.as_deref(),
        jiff::Timestamp::now(),
    )
    .await;
    if !initial_status.state.is_active() {
        let (survivors, still_active) = if initial_status.state == JobState::Interrupted {
            (Vec::new(), false)
        } else {
            // `exit.json` appeared between the earlier check and `derive_status_async`.
            wait_for_lock_release(&dir, LOCK_SETTLE_TIMEOUT).await;
            let record = reload_job_record(&dir, &id, initial_record).await;
            sweep_job_handle(&id, &record, same_boot, stop_grace).await?
        };
        let state = if still_active {
            JobState::Running
        } else {
            initial_status.state
        };
        return Ok(CancelledJob {
            id,
            state,
            survivors,
        });
    }

    // Write the cancel marker file so a live supervisor stops the command tree cooperatively.
    let dir_for_cancel = dir.clone();
    blocking("job.cancel", move || write_cancel_request(&dir_for_cancel)).await?;

    if !args.force {
        let deadline = Instant::now() + CANCEL_GRACE;
        loop {
            let probe_dir = dir.clone();
            let detach_proc = initial_record.detach.as_ref().map(|h| h.process);
            let check = blocking("job.cancel", move || {
                let (_, lock_held) = probe_lock(&probe_dir);
                let exit = read_exit_record(&probe_dir);
                if let Some(exit) = exit {
                    return Ok(CancelPoll::Finished {
                        state: JobState::from(exit.outcome),
                        lock_held,
                    });
                }
                if !lock_held && detach_proc.is_none_or(|proc_id| !procid::is_alive(&proc_id)) {
                    return Ok(CancelPoll::SupervisorGone);
                }
                Ok(CancelPoll::StillRunning)
            })
            .await?;

            match check {
                CancelPoll::Finished { state, lock_held } => {
                    if lock_held {
                        wait_for_lock_release(&dir, LOCK_SETTLE_TIMEOUT).await;
                    }
                    let record = reload_job_record(&dir, &id, initial_record).await;
                    let (survivors, still_active) =
                        sweep_job_handle(&id, &record, same_boot, Duration::from_secs(1)).await?;
                    let reported_state = if still_active {
                        JobState::Running
                    } else {
                        state
                    };
                    return Ok(CancelledJob {
                        id,
                        state: reported_state,
                        survivors,
                    });
                }
                CancelPoll::SupervisorGone => break,
                CancelPoll::StillRunning => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    tokio::time::sleep(CANCEL_POLL.min(remaining)).await;
                }
            }
        }
    }

    // Re-read the record in case `job.start` attached the detach handle while we waited.
    let record = reload_job_record(&dir, &id, initial_record).await;
    let (survivors, still_active) = sweep_job_handle(&id, &record, same_boot, stop_grace).await?;

    if !still_active && survivors.is_empty() {
        wait_for_lock_release(&dir, LOCK_SETTLE_TIMEOUT).await;
        // Re-check both the detached tree and the kernel lock right before writing `exit.json`.
        // Never write an exit record while the lock is held (for example if the starter died
        // before writing `detach` into `job.json`) or while anything under the handle is active.
        let tree_still_active = match record.detach.as_ref() {
            Some(handle) if same_boot => detached_active(handle).await.unwrap_or(true),
            _ => false,
        };
        if !tree_still_active {
            let dir_for_finalize = dir.clone();
            let final_state = blocking("job.cancel", move || {
                if let Some(exit) = read_exit_record(&dir_for_finalize) {
                    return Ok(Some(JobState::from(exit.outcome)));
                }
                if probe_lock(&dir_for_finalize).1 {
                    return Ok(None);
                }
                if let Some(exit) = read_exit_record(&dir_for_finalize) {
                    return Ok(Some(JobState::from(exit.outcome)));
                }
                let exit = ExitRecord {
                    outcome: ExitOutcome::Cancelled,
                    exit_code: None,
                    signal: None,
                    finished: now_rfc3339(),
                    forced: true,
                    startup_failed: false,
                    detail: Some(
                        "Cancelled forcefully; detached process tree was stopped.".to_owned(),
                    ),
                };
                write_exit_record(&dir_for_finalize, &exit)?;
                Ok(Some(JobState::Cancelled))
            })
            .await?;
            if let Some(state) = final_state {
                return Ok(CancelledJob {
                    id,
                    state,
                    survivors,
                });
            }
        }
    }

    Ok(CancelledJob {
        id: record.id,
        state: JobState::Running,
        survivors,
    })
}

async fn reload_job_record(dir: &Path, id: &str, fallback: JobRecord) -> JobRecord {
    let dir = dir.to_path_buf();
    let id = id.to_owned();
    blocking("job.cancel", move || read_job_record(&dir, &id))
        .await
        .unwrap_or(fallback)
}

/// Calls `stop_detached` on the job's stored handle (when on the same boot) and confirms via
/// `detached_active` that no processes remain under the handle.
async fn sweep_job_handle(
    id: &str,
    record: &JobRecord,
    same_boot: bool,
    grace: Duration,
) -> Result<(Vec<u32>, bool), ErrorBody> {
    if !same_boot {
        return Ok((Vec::new(), false));
    }
    let Some(ref handle) = record.detach else {
        return Ok((Vec::new(), false));
    };
    let report = stop_detached(handle, grace).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not stop detached job {id}: {err}."),
        )
        .with_hint("Inspect remaining processes with `sys.procs` or retry with `force: true`.")
    })?;
    let survivors: Vec<u32> = report.survivors.iter().map(|proc_id| proc_id.pid).collect();
    let still_active = if !survivors.is_empty() {
        true
    } else {
        detached_active(handle).await.unwrap_or(true)
    };
    Ok((survivors, still_active))
}

enum CancelPoll {
    Finished { state: JobState, lock_held: bool },
    SupervisorGone,
    StillRunning,
}

async fn wait_for_lock_release(dir: &Path, limit: Duration) {
    let deadline = Instant::now() + limit;
    loop {
        let probe_dir = dir.to_path_buf();
        let held = tokio::task::spawn_blocking(move || probe_lock(&probe_dir).1)
            .await
            .unwrap_or(false);
        if !held || Instant::now() >= deadline {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        tokio::time::sleep(Duration::from_millis(20).min(remaining)).await;
    }
}

/// Validates that exactly one of `cmd` or `argv` is supplied, using the same rules and wording as
/// `exec.run`.
fn validate_command(cmd: Option<&str>, argv: Option<&[String]>) -> Result<(), ErrorBody> {
    let invalid = |message: &str| {
        ErrorBody::new(ErrorCode::InvalidArgs, format!("job.start: {message}")).with_hint(
            "Give `argv`, such as [\"git\", \"status\"], to run a program exactly, or `cmd`, such as \"git status | head\", for the platform shell.",
        )
    };
    match (cmd, argv) {
        (Some(_), Some(_)) => Err(invalid("give `cmd` or `argv`, not both.")),
        (None, None) => Err(invalid("give the command to run, as `cmd` or `argv`.")),
        (Some(cmd), None) if cmd.trim().is_empty() => Err(invalid("`cmd` is empty.")),
        (Some(_), None) => Ok(()),
        (None, Some(argv)) => match argv.first() {
            None => Err(invalid("`argv` is empty.")),
            Some(program) if program.is_empty() => {
                Err(invalid("`argv` starts with an empty program name."))
            }
            Some(_) => Ok(()),
        },
    }
}

fn validate_env(env: &BTreeMap<String, String>) -> Result<(), ErrorBody> {
    for (name, value) in env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "job.start: `env` cannot set {name:?}: a name is not empty and holds no `=`, and neither holds a NUL character."
                ),
            )
            .with_details(json!({"field": format!("env.{name}")})));
        }
    }
    Ok(())
}

fn validate_name(name: Option<String>) -> Result<Option<String>, ErrorBody> {
    match name {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "job.start: `name` must not be empty.",
                )
                .with_hint("Omit `name` to use the command name automatically.")
                .with_details(json!({"field": "name"})));
            }
            Ok(Some(trimmed.to_owned()))
        }
    }
}

fn validate_key(key: Option<String>) -> Result<Option<String>, ErrorBody> {
    match key {
        None => Ok(None),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.len() > MAX_KEY_BYTES || trimmed.contains('\0') {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "job.start: `key` must be non-empty, at most {MAX_KEY_BYTES} bytes, and contain no NUL characters."
                    ),
                )
                .with_hint("Choose a short stable identifier such as \"build-2026-01\".")
                .with_details(json!({"field": "key"})));
            }
            Ok(Some(trimmed.to_owned()))
        }
    }
}

async fn validate_existing_dir(cwd: &Path) -> Result<(), ErrorBody> {
    let cwd_owned = cwd.to_path_buf();
    blocking("job.start", move || match std::fs::metadata(&cwd_owned) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} is not a directory, so the command cannot run in it.",
                cwd_owned.display()
            ),
        )
        .with_hint("Give a directory as `cwd`.")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "The directory {} does not exist, so the job was not started.",
                cwd_owned.display()
            ),
        )
        .with_hint("Give an existing directory as `cwd`.")),
        Err(err) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot access directory {}: {err}.", cwd_owned.display()),
        )),
    })
    .await
}

fn wait_timeout_of(seconds: Option<f64>) -> Result<Duration, ErrorBody> {
    let seconds = seconds.unwrap_or(WAIT_TIMEOUT_S.0);
    if !(seconds > 0.0 && seconds <= WAIT_TIMEOUT_S.1) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "job.wait: `timeout_s` must be more than 0 and at most {}, not {seconds}.",
                WAIT_TIMEOUT_S.1
            ),
        )
        .with_hint("Call `job.wait` again if the job is still running when a wait returns."));
    }
    Ok(Duration::from_secs_f64(seconds))
}

async fn blocking<T: Send + 'static>(
    tool: &'static str,
    work: impl FnOnce() -> Result<T, ErrorBody> + Send + 'static,
) -> Result<T, ErrorBody> {
    tokio::task::spawn_blocking(work).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("{tool} stopped unexpectedly: {err}."),
        )
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_arg_validation_matches_exec_rules() {
        assert_eq!(
            validate_command(None, None).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            validate_command(Some("echo hi"), Some(&["echo".into()]))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            validate_command(Some("  "), None).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            validate_command(None, Some(&[])).unwrap_err().code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            validate_command(None, Some(&[String::new()]))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgs
        );
        assert!(validate_command(Some("echo hi"), None).is_ok());
        assert!(validate_command(None, Some(&["echo".into()])).is_ok());

        assert!(validate_name(Some("  ".into())).is_err());
        assert_eq!(
            validate_name(Some(" build ".into())).unwrap().as_deref(),
            Some("build")
        );
        assert!(validate_key(Some(String::new())).is_err());
        assert!(validate_key(Some("x".repeat(MAX_KEY_BYTES + 1))).is_err());
        assert!(validate_env(&BTreeMap::from([("A=B".into(), "1".into())])).is_err());
        assert!(wait_timeout_of(Some(0.0)).is_err());
        assert!(wait_timeout_of(Some(3601.0)).is_err());
        assert_eq!(wait_timeout_of(None).unwrap(), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn startup_timeout_refuses_exit_record_when_survivors_or_lock_remain() {
        let dead_handle = DetachHandle {
            process: procid::ProcessId {
                pid: u32::MAX,
                start_time: 1,
            },
            systemd_unit: None,
            windows_job: None,
            marker: None,
            warnings: Vec::new(),
        };

        // 1. Survivors reported by `stop_detached`: never write `exit.json`, report `running`.
        let tmp1 = tempfile::tempdir().unwrap();
        let current = procid::current().unwrap();
        let err = finish_startup_timeout(
            tmp1.path(),
            "job-surv",
            &dead_handle,
            Ok(procid::KillTreeReport {
                all_gone: false,
                survivors: vec![current],
                unverified_survivors: Vec::new(),
            }),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(!tmp1.path().join(store::EXIT_FILE).exists());
        let details = err.details.unwrap();
        assert_eq!(details["state"], json!("running"));
        assert_eq!(details["survivors"], json!([current.pid]));

        // 2. Empty `stop_detached` report while the supervisor still holds `lock`: never write
        //    `exit.json`, report `running`.
        let tmp2 = tempfile::tempdir().unwrap();
        let guard = FileLock::try_lock_exclusive(tmp2.path().join(store::LOCK_FILE)).unwrap();
        let err = finish_startup_timeout(
            tmp2.path(),
            "job-locked",
            &dead_handle,
            Ok(procid::KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
                unverified_survivors: Vec::new(),
            }),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(!tmp2.path().join(store::EXIT_FILE).exists());
        assert_eq!(err.details.unwrap()["state"], json!("running"));
        drop(guard);

        // 3. `stop_detached` returned an error: never write `exit.json`, report `running`.
        let tmp3 = tempfile::tempdir().unwrap();
        let err = finish_startup_timeout(
            tmp3.path(),
            "job-stop-err",
            &dead_handle,
            Err(OsError::Platform {
                detail: "simulated stop failure".into(),
            }),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
        assert!(!tmp3.path().join(store::EXIT_FILE).exists());
        assert_eq!(err.details.unwrap()["state"], json!("running"));

        // 4. Clean stop, free lock, inactive tree: writes `exit.json` as failed.
        let tmp4 = tempfile::tempdir().unwrap();
        let err = finish_startup_timeout(
            tmp4.path(),
            "job-clean",
            &dead_handle,
            Ok(procid::KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
                unverified_survivors: Vec::new(),
            }),
            Duration::from_millis(50),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout);
        let exit = read_exit_record(tmp4.path()).expect("exit.json should be written");
        assert_eq!(exit.outcome, ExitOutcome::Failed);
        assert_eq!(err.details.unwrap()["state"], json!("failed"));
    }
}
