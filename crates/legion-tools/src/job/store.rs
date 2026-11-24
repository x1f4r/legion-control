//! On-disk job state under `<state dir>/jobs/<id>/`, liveness checks, idempotency lookup, and
//! retention pruning.
//!
//! Every read operation is strictly read-only: it never creates `<state dir>/jobs`, `jobs.lock`,
//! or a job's `lock` file. Liveness is proven by attempting a non-blocking shared kernel lock on
//! an already-existing `lock` file, with a second check of `exit.json` whenever the lock is free
//! so a supervisor exiting between the two checks never looks interrupted.

use std::collections::BTreeMap;
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::OsError;
use legion_os::atomic::{WriteOptions, write_atomic};
use legion_os::detach::{DetachHandle, detached_active};
use legion_os::lock::FileLock;
use legion_os::procid;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::platform;

/// Subdirectory of `<state dir>` holding one directory per job.
pub(super) const JOBS_DIR: &str = "jobs";
/// Node-wide kernel lock serializing job creation, idempotent key checks, and retention.
pub(super) const JOBS_LOCK: &str = "jobs.lock";
/// Job metadata written atomically before the supervisor starts.
pub(super) const JOB_FILE: &str = "job.json";
/// Private environment values for the job; never copied into `job.json` or replies.
pub(super) const ENV_FILE: &str = "env.json";
/// Exit record written atomically when the command finishes or is forcefully stopped.
pub(super) const EXIT_FILE: &str = "exit.json";
/// Placeholder file allocated by the supervisor on startup and removed immediately before
/// `exit.json` is written so a full disk cannot lose a finished job's exit code.
pub(super) const EXIT_RESERVE_FILE: &str = "exit.reserve";
/// Size of [`EXIT_RESERVE_FILE`] in bytes (16 KiB, comfortably above any `exit.json`).
pub(super) const EXIT_RESERVE_BYTES: usize = 16 * 1024;
/// Combined stdout and stderr output appended as chunks arrive.
pub(super) const LOG_FILE: &str = "output.log";
/// Single rotated segment holding the previous [`MAX_LOG_FILE_BYTES`] chunk of output.
pub(super) const LOG_ROTATED_FILE: &str = "output.log.1";
/// Rotation metadata recording how many earliest log bytes were dropped.
pub(super) const LOG_META_FILE: &str = "output.meta";
/// Maximum size of a single `output.log` segment before rotating once to `output.log.1` (256 MiB).
/// At most twice this cap is kept on disk across `output.log.1` and `output.log`.
pub(super) const MAX_LOG_FILE_BYTES: u64 = 256 * 1024 * 1024;
/// Kernel lock file held exclusively by the supervisor for its whole life.
pub(super) const LOCK_FILE: &str = "lock";
/// Cancel request marker file polled by the supervisor.
pub(super) const CANCEL_FILE: &str = "cancel";

/// Maximum number of finished jobs retained when starting a new job.
pub(super) const RETENTION_MAX_FINISHED: usize = 200;
/// Maximum age of finished jobs retained when starting a new job (30 days in milliseconds).
pub(super) const RETENTION_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Window during which a brand-new record with no lock yet counts as `starting` rather than
/// `interrupted`, covering the brief handoff while `job.start` spawns the supervisor.
const STARTING_GRACE_MS: i64 = 15_000;
/// Maximum characters kept in `job.list`'s command summary.
const COMMAND_SUMMARY_CHARS: usize = 200;
/// Maximum characters for a derived default job name.
const DEFAULT_NAME_CHARS: usize = 60;
/// Maximum length accepted for a job identifier.
const MAX_JOB_ID_LEN: usize = 128;

/// Process-local counter mixed into generated job IDs so tight back-to-back starts stay unique.
static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Derived lifecycle state of a background job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// The job record is written and the detached supervisor is starting up.
    Starting,
    /// The supervisor holds the job's kernel lock and is running the command.
    Running,
    /// The command exited with code 0.
    Succeeded,
    /// The command exited with a non-zero code, ended on a signal, or failed to start.
    Failed,
    /// The job was stopped via `job.cancel`.
    Cancelled,
    /// No exit record exists and the supervisor's lock is free (the supervisor died or the machine
    /// rebooted).
    Interrupted,
}

impl JobState {
    /// Whether the job may still be making progress or starting up.
    pub(super) fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

/// Terminal outcome written into `exit.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExitOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl From<ExitOutcome> for JobState {
    fn from(outcome: ExitOutcome) -> Self {
        match outcome {
            ExitOutcome::Succeeded => JobState::Succeeded,
            ExitOutcome::Failed => JobState::Failed,
            ExitOutcome::Cancelled => JobState::Cancelled,
        }
    }
}

/// On-disk record stored in `<state dir>/jobs/<id>/job.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct JobRecord {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env_names: Vec<String>,
    pub principal: String,
    pub created: String,
    pub boot_id: String,
    #[serde(default)]
    pub exclusive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detach: Option<DetachHandle>,
}

/// On-disk completion record stored in `<state dir>/jobs/<id>/exit.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExitRecord {
    pub outcome: ExitOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub finished: String,
    #[serde(default)]
    pub forced: bool,
    /// `true` when `job.start` failed before the detached supervisor ever acquired `lock`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub startup_failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Status derived on read from `job.json`, `exit.json`, `lock`, `detached_active`, and
/// `output.log`.
#[derive(Clone, Debug)]
pub(super) struct DerivedStatus {
    pub state: JobState,
    pub supervised: Option<bool>,
    pub startup_failed: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub finished: Option<String>,
    pub duration_s: Option<f64>,
    pub output_bytes: u64,
    pub detail: Option<String>,
}

/// An exclusive background job that is currently starting or running on this node.
///
/// Busy checks (such as `power.*`) call [`super::exclusive_running_jobs`] to see which exclusive
/// jobs are holding the machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExclusiveJob {
    /// The job's identifier.
    pub id: String,
    /// The job's name.
    pub name: String,
    /// When the job was started (RFC 3339 UTC).
    pub since: String,
}

/// Current UTC time as RFC 3339 to the millisecond, matching the audit log.
pub(super) fn now_rfc3339() -> String {
    format_timestamp(jiff::Timestamp::now())
}

/// Formats `ts` as RFC 3339 UTC to the millisecond.
pub(super) fn format_timestamp(ts: jiff::Timestamp) -> String {
    ts.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Parses an RFC 3339 timestamp string.
pub(super) fn parse_timestamp(text: &str) -> Option<jiff::Timestamp> {
    text.trim().parse().ok()
}

/// Directory containing all job subdirectories under `state_dir`.
pub(super) fn jobs_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(JOBS_DIR)
}

/// Path to the node-wide jobs lock file beside `jobs/`.
pub(super) fn jobs_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(JOBS_LOCK)
}

/// Directory for one job. Caller must validate `id` first with [`validate_job_id`].
pub(super) fn job_dir(state_dir: &Path, id: &str) -> PathBuf {
    jobs_dir(state_dir).join(id)
}

/// Ensures `id` is a safe single path segment and valid systemd unit suffix.
pub(super) fn validate_job_id(id: &str) -> Result<(), ErrorBody> {
    let valid = !id.is_empty()
        && id.len() <= MAX_JOB_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !valid {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{id:?} is not a valid job id."),
        )
        .with_hint("Pass a job id returned by `job.start` or `job.list`.")
        .with_details(json!({"field": "id"})));
    }
    Ok(())
}

/// Generates a short, unique, time-sortable identifier safe for filenames and systemd unit names.
///
/// Format: 9 zero-padded base-36 characters of Unix epoch milliseconds followed by `-` and 6
/// base-36 characters hashed from OS entropy, process id, nanoseconds, and a monotonic counter.
pub(super) fn generate_job_id() -> String {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = u64::try_from(since_epoch.as_millis()).unwrap_or(u64::MAX);
    let nanos = since_epoch.subsec_nanos();
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(millis);
    hasher.write_u32(nanos);
    hasher.write_u64(seq);
    hasher.write_u32(pid);
    let seed_a = hasher.finish();
    let seed_b = RandomState::new().build_hasher().finish();

    let mut material = [0u8; 36];
    material[0..8].copy_from_slice(&millis.to_be_bytes());
    material[8..12].copy_from_slice(&nanos.to_be_bytes());
    material[12..20].copy_from_slice(&seq.to_be_bytes());
    material[20..28].copy_from_slice(&seed_a.to_be_bytes());
    material[28..36].copy_from_slice(&seed_b.to_be_bytes());
    let digest = blake3::hash(&material);
    let mut rand_bytes = [0u8; 8];
    rand_bytes.copy_from_slice(&digest.as_bytes()[..8]);
    let entropy = u64::from_be_bytes(rand_bytes) % 2_176_782_336; // 36^6

    format!("{}-{}", encode_base36(millis, 9), encode_base36(entropy, 6))
}

fn encode_base36(mut value: u64, width: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = vec![b'0'; width];
    for slot in out.iter_mut().rev() {
        let digit = usize::try_from(value % 36).unwrap_or(0);
        *slot = ALPHABET[digit];
        value /= 36;
    }
    String::from_utf8(out).unwrap_or_default()
}

/// Derives a readable default name when `job.start` is called without `name`.
pub(super) fn default_job_name(
    cmd: Option<&str>,
    argv: Option<&[String]>,
    fallback_id: &str,
) -> String {
    let raw = match (cmd, argv) {
        (Some(cmd), _) => cmd.split_whitespace().next().unwrap_or_default(),
        (None, Some(argv)) => argv.first().map(String::as_str).unwrap_or_default(),
        (None, None) => "",
    };
    let stem = Path::new(raw)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(raw)
        .trim();
    if stem.is_empty() {
        fallback_id.to_owned()
    } else {
        cut_chars(stem, DEFAULT_NAME_CHARS)
    }
}

/// Formats a job's command for `job.list`, cut at 200 characters on a character boundary.
pub(super) fn command_summary(record: &JobRecord) -> String {
    let full = match (&record.cmd, &record.argv) {
        (Some(cmd), _) => cmd.clone(),
        (None, Some(argv)) => argv.join(" "),
        (None, None) => String::new(),
    };
    cut_chars(&full, COMMAND_SUMMARY_CHARS)
}

/// Returns the first `max` Unicode scalar values of `text`, appending `…` when cut.
pub(super) fn cut_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Reads `<job_dir>/job.json`, returning `not_found` with a hint to `job.list` when absent.
pub(super) fn read_job_record(dir: &Path, id: &str) -> Result<JobRecord, ErrorBody> {
    let path = dir.join(JOB_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("There is no job with id {id:?} on this machine."),
            )
            .with_hint("Run `job.list` to see the jobs recorded on this node.")
            .with_details(json!({"id": id})));
        }
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot read job record {}: {err}.", path.display()),
            ));
        }
    };
    serde_json::from_slice(&bytes).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Job record {} is not valid JSON: {err}.", path.display()),
        )
    })
}

/// Reads `<job_dir>/exit.json` if it exists and parses.
pub(super) fn read_exit_record(dir: &Path) -> Option<ExitRecord> {
    let bytes = fs::read(dir.join(EXIT_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Reads `<job_dir>/env.json` if present; returns an empty map when absent.
pub(super) fn read_env_file(dir: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = dir.join(ENV_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| format!("cannot parse {}: {err}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// Atomically writes `<job_dir>/job.json` with owner-only permissions.
pub(super) fn write_job_record(dir: &Path, record: &JobRecord) -> Result<(), ErrorBody> {
    let path = dir.join(JOB_FILE);
    let bytes = serde_json::to_vec_pretty(record).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Cannot serialize job record: {err}."),
        )
    })?;
    write_atomic(&path, &bytes, WriteOptions::with_mode(0o600)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot write job record {}: {err}.", path.display()),
        )
    })
}

/// Atomically writes `<job_dir>/env.json` with owner-only permissions on Unix (`0o600`).
///
/// On Windows the file has no access control list of its own beyond what its parent directory
/// inherits; the supervisor deletes `env.json` immediately after reading it so secret values sit
/// on disk only for milliseconds rather than for the job's retention period.
pub(super) fn write_env_file(dir: &Path, env: &BTreeMap<String, String>) -> Result<(), ErrorBody> {
    let path = dir.join(ENV_FILE);
    let bytes = serde_json::to_vec(env).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Cannot serialize job environment: {err}."),
        )
    })?;
    write_atomic(&path, &bytes, WriteOptions::with_mode(0o600)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot write job environment {}: {err}.", path.display()),
        )
    })
}

/// Removes `<job_dir>/env.json` once the supervisor has loaded it (or when startup fails).
pub(super) fn remove_env_file(dir: &Path) {
    let _ = fs::remove_file(dir.join(ENV_FILE));
}

/// Metadata written atomically to `<job_dir>/output.meta` when log rotation discards earlier
/// output beyond `output.log.1`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct LogMeta {
    #[serde(default)]
    pub dropped_bytes: u64,
}

/// Reads `<job_dir>/output.meta`, defaulting to zero dropped bytes when absent.
pub(super) fn read_log_meta(dir: &Path) -> LogMeta {
    fs::read(dir.join(LOG_META_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Atomically updates `<job_dir>/output.meta` with the total number of dropped log bytes.
pub(super) fn write_log_meta(dir: &Path, dropped_bytes: u64) -> Result<(), ErrorBody> {
    let path = dir.join(LOG_META_FILE);
    let meta = LogMeta { dropped_bytes };
    let bytes = serde_json::to_vec(&meta).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Cannot serialize job log metadata: {err}."),
        )
    })?;
    write_atomic(&path, &bytes, WriteOptions::with_mode(0o600)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot write log metadata {}: {err}.", path.display()),
        )
    })
}

/// Allocates real incompressible disk blocks in `<job_dir>/exit.reserve` so removing it right
/// before writing `exit.json` guarantees space even if the job fills the filesystem.
pub(super) fn create_exit_reserve(dir: &Path) -> io::Result<()> {
    let path = dir.join(EXIT_RESERVE_FILE);
    let mut file = platform::create_private_file(&path)?;
    // Fill with deterministic incompressible bytes (blake3 XOF) so compressing filesystems such as
    // btrfs or ZFS still allocate real physical blocks.
    let mut buf = [0u8; EXIT_RESERVE_BYTES];
    blake3::Hasher::new()
        .update(b"legion-job-exit-reserve")
        .finalize_xof()
        .fill(&mut buf);
    file.write_all(&buf)?;
    file.sync_all()
}

/// Removes `<job_dir>/exit.reserve` to release reserved disk space right before writing
/// `exit.json` (or on early supervisor exit).
pub(super) fn remove_exit_reserve(dir: &Path) {
    let _ = fs::remove_file(dir.join(EXIT_RESERVE_FILE));
}

/// Total bytes currently stored across `output.log.1` and `output.log`.
pub(super) fn total_log_bytes(dir: &Path) -> u64 {
    let active = fs::metadata(dir.join(LOG_FILE))
        .map(|meta| meta.len())
        .unwrap_or(0);
    let rotated = fs::metadata(dir.join(LOG_ROTATED_FILE))
        .map(|meta| meta.len())
        .unwrap_or(0);
    active.saturating_add(rotated)
}

/// Atomically writes `<job_dir>/exit.json` with owner-only permissions.
///
/// Always removes `<job_dir>/exit.reserve` immediately beforehand so reserved disk blocks are
/// freed for the exit record when the filesystem is full.
pub(super) fn write_exit_record(dir: &Path, exit: &ExitRecord) -> Result<(), ErrorBody> {
    remove_exit_reserve(dir);
    let path = dir.join(EXIT_FILE);
    let bytes = serde_json::to_vec_pretty(exit).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("Cannot serialize job exit record: {err}."),
        )
    })?;
    write_atomic(&path, &bytes, WriteOptions::with_mode(0o600)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot write exit record {}: {err}.", path.display()),
        )
    })
}

/// Atomically writes `<job_dir>/cancel` so the supervisor stops the job promptly.
pub(super) fn write_cancel_request(dir: &Path) -> Result<(), ErrorBody> {
    let path = dir.join(CANCEL_FILE);
    write_atomic(&path, b"cancel\n", WriteOptions::with_mode(0o600)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot write cancel request {}: {err}.", path.display()),
        )
    })
}

/// Tests whether the supervisor holds the exclusive kernel lock on `<job_dir>/lock` without
/// creating or modifying any file.
///
/// Returns `(lock_file_exists, lock_is_held)`.
pub(super) fn probe_lock(dir: &Path) -> (bool, bool) {
    let lock_path = dir.join(LOCK_FILE);
    // Check existence before calling `try_lock_shared`, because `legion_os::lock` opens with
    // `create(true)` and read tools must never create files.
    if fs::metadata(&lock_path).is_err() {
        return (false, false);
    }
    match FileLock::try_lock_shared(&lock_path) {
        Ok(guard) => {
            let _ = guard.unlock();
            (true, false)
        }
        Err(OsError::WouldBlock { .. }) => (true, true),
        Err(_) => (true, false),
    }
}

/// Intermediate result of synchronous local file/lock inspection before an optional
/// `detached_active` check.
enum LocalInspect {
    Ready(DerivedStatus),
    NeedsTreeCheck {
        handle: DetachHandle,
        lock_exists: bool,
        age_ms: i64,
        created_ts: Option<jiff::Timestamp>,
        output_bytes: u64,
    },
}

/// Checks `legion_os::detach::detached_active` from synchronous code without panicking when
/// invoked inside a Tokio runtime worker thread.
pub(super) fn detached_active_sync(handle: &DetachHandle) -> bool {
    let handle = handle.clone();
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return false;
                };
                rt.block_on(detached_active(&handle)).unwrap_or(false)
            })
            .join()
            .unwrap_or(false)
    })
}

/// Derives a job's live state purely from disk state, kernel locks, and `detached_active` without
/// writing anything (async path for tool handlers).
pub(super) async fn derive_status_async(
    dir: &Path,
    record: &JobRecord,
    current_boot_id: Option<&str>,
    now: jiff::Timestamp,
) -> DerivedStatus {
    match inspect_local(dir, record, current_boot_id, now) {
        LocalInspect::Ready(status) => status,
        LocalInspect::NeedsTreeCheck {
            handle,
            lock_exists,
            age_ms,
            created_ts,
            output_bytes,
        } => {
            let active = detached_active(&handle).await.unwrap_or(false);
            finish_same_boot_unlocked(
                dir,
                record,
                &handle,
                lock_exists,
                age_ms,
                created_ts,
                output_bytes,
                active,
                now,
            )
        }
    }
}

/// Derives a job's live state purely from disk state, kernel locks, and `detached_active` without
/// writing anything (sync path for `exclusive_running_jobs`, retention, and unit tests).
pub(super) fn derive_status(
    dir: &Path,
    record: &JobRecord,
    current_boot_id: Option<&str>,
    now: jiff::Timestamp,
) -> DerivedStatus {
    match inspect_local(dir, record, current_boot_id, now) {
        LocalInspect::Ready(status) => status,
        LocalInspect::NeedsTreeCheck {
            handle,
            lock_exists,
            age_ms,
            created_ts,
            output_bytes,
        } => {
            let active = detached_active_sync(&handle);
            finish_same_boot_unlocked(
                dir,
                record,
                &handle,
                lock_exists,
                age_ms,
                created_ts,
                output_bytes,
                active,
                now,
            )
        }
    }
}

fn inspect_local(
    dir: &Path,
    record: &JobRecord,
    current_boot_id: Option<&str>,
    now: jiff::Timestamp,
) -> LocalInspect {
    let output_bytes = total_log_bytes(dir);
    let created_ts = parse_timestamp(&record.created);

    if let Some(exit) = read_exit_record(dir) {
        return LocalInspect::Ready(derived_from_exit(exit, created_ts, output_bytes));
    }

    let (lock_exists, lock_held) = probe_lock(dir);
    if lock_held {
        let duration_s = created_ts.map(|started| elapsed_seconds(started, now));
        return LocalInspect::Ready(DerivedStatus {
            state: JobState::Running,
            supervised: Some(true),
            startup_failed: false,
            exit_code: None,
            signal: None,
            finished: None,
            duration_s,
            output_bytes,
            detail: None,
        });
    }

    // The supervisor writes `exit.json` before releasing `lock`. Re-checking after seeing the lock
    // free closes the window where a job finished cleanly between our first check and the lock
    // probe.
    if let Some(exit) = read_exit_record(dir) {
        return LocalInspect::Ready(derived_from_exit(exit, created_ts, output_bytes));
    }

    let same_boot = current_boot_id.is_none_or(|cur| procid::same_boot(&record.boot_id, cur));
    let age_ms = created_ts
        .map(|started| {
            now.as_millisecond()
                .saturating_sub(started.as_millisecond())
        })
        .unwrap_or(STARTING_GRACE_MS);

    if !same_boot {
        let detail = interrupted_detail(dir, record, false, lock_exists, created_ts, now);
        return LocalInspect::Ready(DerivedStatus {
            state: JobState::Interrupted,
            supervised: None,
            startup_failed: false,
            exit_code: None,
            signal: None,
            finished: None,
            duration_s: None,
            output_bytes,
            detail: Some(detail),
        });
    }

    if let Some(handle) = record.detach.clone() {
        return LocalInspect::NeedsTreeCheck {
            handle,
            lock_exists,
            age_ms,
            created_ts,
            output_bytes,
        };
    }

    if age_ms < STARTING_GRACE_MS {
        let duration_s = created_ts.map(|started| elapsed_seconds(started, now));
        return LocalInspect::Ready(DerivedStatus {
            state: JobState::Starting,
            supervised: Some(true),
            startup_failed: false,
            exit_code: None,
            signal: None,
            finished: None,
            duration_s,
            output_bytes,
            detail: None,
        });
    }

    let detail = interrupted_detail(dir, record, true, lock_exists, created_ts, now);
    LocalInspect::Ready(DerivedStatus {
        state: JobState::Interrupted,
        supervised: None,
        startup_failed: false,
        exit_code: None,
        signal: None,
        finished: None,
        duration_s: None,
        output_bytes,
        detail: Some(detail),
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_same_boot_unlocked(
    dir: &Path,
    record: &JobRecord,
    handle: &DetachHandle,
    lock_exists: bool,
    age_ms: i64,
    created_ts: Option<jiff::Timestamp>,
    output_bytes: u64,
    tree_active: bool,
    now: jiff::Timestamp,
) -> DerivedStatus {
    // Before the supervisor has created `lock`, a live root within the startup grace window is
    // still starting normally.
    if !lock_exists && age_ms < STARTING_GRACE_MS && procid::is_alive(&handle.process) {
        let duration_s = created_ts.map(|started| elapsed_seconds(started, now));
        return DerivedStatus {
            state: JobState::Starting,
            supervised: Some(true),
            startup_failed: false,
            exit_code: None,
            signal: None,
            finished: None,
            duration_s,
            output_bytes,
            detail: None,
        };
    }

    // With no exit record and the lock free, if anything started under the detach handle is still
    // active on the same boot, the job is still running without its supervisor.
    if tree_active {
        let duration_s = created_ts.map(|started| elapsed_seconds(started, now));
        return DerivedStatus {
            state: JobState::Running,
            supervised: Some(false),
            startup_failed: false,
            exit_code: None,
            signal: None,
            finished: None,
            duration_s,
            output_bytes,
            detail: Some(
                "The job supervisor is gone, so output is no longer captured; the command is still running and `job.cancel` will stop it."
                    .to_owned(),
            ),
        };
    }

    let detail = interrupted_detail(dir, record, true, lock_exists, created_ts, now);
    DerivedStatus {
        state: JobState::Interrupted,
        supervised: None,
        startup_failed: false,
        exit_code: None,
        signal: None,
        finished: None,
        duration_s: None,
        output_bytes,
        detail: Some(detail),
    }
}

fn derived_from_exit(
    exit: ExitRecord,
    created_ts: Option<jiff::Timestamp>,
    output_bytes: u64,
) -> DerivedStatus {
    let duration_s = created_ts
        .zip(parse_timestamp(&exit.finished))
        .map(|(started, finished)| elapsed_seconds(started, finished));
    DerivedStatus {
        state: JobState::from(exit.outcome),
        supervised: None,
        startup_failed: exit.startup_failed,
        exit_code: exit.exit_code,
        signal: exit.signal,
        finished: Some(exit.finished),
        duration_s,
        output_bytes,
        detail: exit.detail,
    }
}

fn interrupted_detail(
    dir: &Path,
    record: &JobRecord,
    same_boot: bool,
    lock_exists: bool,
    created_ts: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> String {
    let ran_for = interrupted_runtime_phrase(dir, created_ts, now);
    if !same_boot {
        return format!(
            "The machine rebooted before the job finished; it had been running for {ran_for}."
        );
    }
    if !lock_exists {
        return format!(
            "The job supervisor exited before taking the job lock; the job was created {ran_for} ago."
        );
    }
    match record.detach.as_ref() {
        Some(handle) => format!(
            "The job supervisor (pid {}) stopped unexpectedly without writing an exit record; it had been running for {ran_for}.",
            handle.process.pid
        ),
        None => format!(
            "The job supervisor stopped unexpectedly without writing an exit record; it had been running for {ran_for}."
        ),
    }
}

/// Estimates how long an interrupted job ran from `created`, the last log write time, and `now`.
fn interrupted_runtime_phrase(
    dir: &Path,
    created_ts: Option<jiff::Timestamp>,
    now: jiff::Timestamp,
) -> String {
    let Some(created) = created_ts else {
        return "an unknown duration".to_owned();
    };
    let since_created = elapsed_seconds(created, now);
    let last_log = [LOG_FILE, LOG_ROTATED_FILE]
        .into_iter()
        .filter_map(|name| {
            let meta = fs::metadata(dir.join(name)).ok()?;
            (meta.len() > 0)
                .then(|| meta.modified().ok())
                .flatten()
                .and_then(|mtime| jiff::Timestamp::try_from(mtime).ok())
        })
        .filter(|&mtime| mtime >= created)
        .max();

    match last_log {
        Some(last_write) => {
            let active = elapsed_seconds(created, last_write);
            if since_created - active >= 2.0 {
                format!(
                    "at least {} (last output after {}, started {} ago)",
                    format_duration(active),
                    format_duration(active),
                    format_duration(since_created)
                )
            } else {
                format_duration(since_created)
            }
        }
        None => format_duration(since_created),
    }
}

fn elapsed_seconds(start: jiff::Timestamp, end: jiff::Timestamp) -> f64 {
    let ms = end.as_millisecond().saturating_sub(start.as_millisecond());
    if ms <= 0 {
        0.0
    } else {
        let secs = ms as f64 / 1000.0;
        (secs * 1000.0).round() / 1000.0
    }
}

fn format_duration(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else if seconds < 3600.0 {
        let total = seconds.round() as u64;
        format!("{}m {}s", total / 60, total % 60)
    } else {
        let total = seconds.round() as u64;
        format!("{}h {}m", total / 3600, (total % 3600) / 60)
    }
}

/// Reads every valid job record under `<state dir>/jobs/` without writing anything.
pub(super) fn load_all_records(state_dir: &Path) -> Result<Vec<(PathBuf, JobRecord)>, ErrorBody> {
    let root = jobs_dir(state_dir);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot list jobs directory {}: {err}.", root.display()),
            ));
        }
    };

    let mut records = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(bytes) = fs::read(path.join(JOB_FILE)) else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<JobRecord>(&bytes) else {
            continue;
        };
        records.push((path, record));
    }
    Ok(records)
}

/// Finds the newest matching job with `key` under `state_dir`.
///
/// A key matches a job that is active (`starting` or `running`) or that ran (its supervisor took
/// `lock` and finished as `succeeded`, `failed`, `cancelled`, or was interrupted mid-run). A job
/// whose start failed before the supervisor ever acquired `lock` does not match and is skipped so
/// retries can replace it.
pub(super) fn find_by_key(
    state_dir: &Path,
    key: &str,
    current_boot_id: Option<&str>,
    now: jiff::Timestamp,
) -> Result<Option<(PathBuf, JobRecord, DerivedStatus)>, ErrorBody> {
    let mut matches: Vec<(PathBuf, JobRecord)> = load_all_records(state_dir)?
        .into_iter()
        .filter(|(_, record)| record.key.as_deref() == Some(key))
        .collect();
    matches.sort_by(|(_, a), (_, b)| compare_newest_first(a, b));
    for (dir, record) in matches {
        let derived = derive_status(&dir, &record, current_boot_id, now);
        if key_matches_job(&dir, &derived) {
            return Ok(Some((dir, record, derived)));
        }
    }
    Ok(None)
}

fn key_matches_job(dir: &Path, derived: &DerivedStatus) -> bool {
    if derived.state.is_active() {
        return true;
    }
    if derived.state == JobState::Interrupted {
        // Matches only if the supervisor ever got far enough to create and take `lock`.
        return probe_lock(dir).0;
    }
    !derived.startup_failed
}

/// Ordering comparator sorting newer jobs first, breaking ties by id descending.
pub(super) fn compare_newest_first(a: &JobRecord, b: &JobRecord) -> std::cmp::Ordering {
    let a_ts = parse_timestamp(&a.created);
    let b_ts = parse_timestamp(&b.created);
    b_ts.cmp(&a_ts).then_with(|| b.id.cmp(&a.id))
}

/// Removes finished jobs beyond [`RETENTION_MAX_FINISHED`] or finished more than 30 days ago.
///
/// Running and starting jobs are never removed. Called under the node-wide jobs lock on
/// `job.start`.
pub(super) fn prune_retention(state_dir: &Path, now: jiff::Timestamp) {
    let Ok(records) = load_all_records(state_dir) else {
        return;
    };
    let current_boot = procid::boot_id().ok();

    let mut terminal: Vec<(i64, String, PathBuf)> = Vec::new();
    for (dir, record) in records {
        let status = derive_status(&dir, &record, current_boot.as_deref(), now);
        if status.state.is_active() {
            continue;
        }
        let stamp_ms = status
            .finished
            .as_deref()
            .and_then(parse_timestamp)
            .or_else(|| parse_timestamp(&record.created))
            .map(jiff::Timestamp::as_millisecond)
            .unwrap_or(0);
        terminal.push((stamp_ms, record.id, dir));
    }

    // Keep the newest finished jobs up to the cap; delete older excess or anything past 30 days.
    terminal.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let now_ms = now.as_millisecond();

    for (index, (stamp_ms, _id, dir)) in terminal.into_iter().enumerate() {
        let too_many = index >= RETENTION_MAX_FINISHED;
        let too_old = now_ms.saturating_sub(stamp_ms) > RETENTION_MAX_AGE_MS;
        if too_many || too_old {
            // Double-check the supervisor lock is not held before deleting the directory.
            if !probe_lock(&dir).1 {
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }
}

/// Lists every `exclusive` job that is currently starting or running under `state_dir`.
///
/// Pure read: never creates or modifies any files.
pub(super) fn exclusive_running_jobs_in(state_dir: &Path) -> Result<Vec<ExclusiveJob>, ErrorBody> {
    let records = load_all_records(state_dir)?;
    let current_boot = procid::boot_id().ok();
    let now = jiff::Timestamp::now();
    let mut busy = Vec::new();
    for (dir, record) in records {
        if !record.exclusive {
            continue;
        }
        let status = derive_status(&dir, &record, current_boot.as_deref(), now);
        if status.state.is_active() {
            busy.push(ExclusiveJob {
                id: record.id,
                name: record.name,
                since: record.created,
            });
        }
    }
    busy.sort_by(|a, b| a.since.cmp(&b.since).then_with(|| a.id.cmp(&b.id)));
    Ok(busy)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(id: &str, created: &str, boot_id: &str) -> JobRecord {
        JobRecord {
            id: id.to_owned(),
            name: "sample".to_owned(),
            key: None,
            cmd: Some("echo hi".to_owned()),
            argv: None,
            cwd: None,
            env_names: Vec::new(),
            principal: "owner".to_owned(),
            created: created.to_owned(),
            boot_id: boot_id.to_owned(),
            exclusive: false,
            detach: None,
        }
    }

    #[test]
    fn generated_ids_are_short_unique_sortable_and_unit_safe() {
        let mut ids = Vec::new();
        for _ in 0..20 {
            let id = generate_job_id();
            assert!(validate_job_id(&id).is_ok(), "{id}");
            assert_eq!(id.len(), 16, "{id}");
            ids.push(id);
        }
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());

        let earlier = encode_base36(1_700_000_000_000, 9);
        let later = encode_base36(1_700_000_000_001, 9);
        assert!(earlier < later, "{earlier} vs {later}");
    }

    #[test]
    fn invalid_ids_including_traversal_are_rejected() {
        for bad in ["", "../etc", "a/b", "a\\b", "a b", "a.b", "id\0x"] {
            assert_eq!(
                validate_job_id(bad).unwrap_err().code,
                ErrorCode::InvalidArgs,
                "{bad:?}"
            );
        }
        for good in ["0mk8p4z2q-a7f3k9", "job_1", "ABC-123"] {
            assert!(validate_job_id(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn derive_status_identifies_reboot_vs_dead_supervisor_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("job-1");
        fs::create_dir_all(&dir).unwrap();
        let created = "2026-01-01T00:00:00.000Z";
        let now: jiff::Timestamp = "2026-01-01T00:01:30.000Z".parse().unwrap();

        // Rebooted (different boot id), no lock file at all: must not create lock.
        let rec = sample_record("job-1", created, "old-boot-uuid");
        let status = derive_status(&dir, &rec, Some("new-boot-uuid"), now);
        assert_eq!(status.state, JobState::Interrupted);
        let detail = status.detail.unwrap();
        assert!(detail.contains("rebooted"), "{detail}");
        assert!(detail.contains("1m 30s"), "{detail}");
        assert!(!dir.join(LOCK_FILE).exists());

        // Same boot with free lock file: supervisor died.
        fs::write(dir.join(LOCK_FILE), b"").unwrap();
        let status = derive_status(&dir, &rec, Some("old-boot-uuid"), now);
        assert_eq!(status.state, JobState::Interrupted);
        let detail = status.detail.unwrap();
        assert!(detail.contains("supervisor"), "{detail}");
        assert!(detail.contains("1m 30s"), "{detail}");
    }

    #[test]
    fn derive_status_reports_running_when_lock_is_held_and_starting_before_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("job-2");
        fs::create_dir_all(&dir).unwrap();
        let now = jiff::Timestamp::now();
        let rec = sample_record("job-2", &format_timestamp(now), "same-boot");

        let starting = derive_status(&dir, &rec, Some("same-boot"), now);
        assert_eq!(starting.state, JobState::Starting);
        assert!(!dir.join(LOCK_FILE).exists());

        let guard = FileLock::try_lock_exclusive(dir.join(LOCK_FILE)).unwrap();
        let running = derive_status(&dir, &rec, Some("same-boot"), now);
        assert_eq!(running.state, JobState::Running);
        drop(guard);
    }

    #[test]
    fn default_name_and_command_summary_cut_on_character_boundaries() {
        assert_eq!(
            default_job_name(Some("/usr/bin/cargo build --release"), None, "id1"),
            "cargo"
        );
        assert_eq!(
            default_job_name(None, Some(&["/opt/tools/runner".into(), "x".into()]), "id1"),
            "runner"
        );
        assert_eq!(default_job_name(Some("   "), None, "id1"), "id1");

        let mut rec = sample_record("id1", "2026-01-01T00:00:00.000Z", "b");
        rec.cmd = Some("é".repeat(250));
        let summary = command_summary(&rec);
        assert_eq!(summary.chars().count(), 201);
        assert!(summary.ends_with('…'));
    }
}
