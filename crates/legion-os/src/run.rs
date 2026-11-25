//! Bounded command execution with live streaming and whole-tree termination.
//!
//! [`run`] is strictly for bounded commands and never leaves background descendants behind: once
//! the direct child exits (or when a timeout or cancellation triggers), any remaining processes
//! in its containment boundary are terminated (`SIGTERM`, grace, `SIGKILL` on Unix;
//! `TerminateJobObject` on Windows) and output pipes are drained with a time bound. Processes
//! meant to outlive the call must be started through [`crate::detach`] instead.
//!
//! **Known Unix limitation:** because bounded runs use a POSIX process group rather than a
//! dedicated cgroup, a descendant that explicitly calls `setsid` (or moves itself to another
//! process group) escapes group termination. Pipe draining is still time-bounded so such a
//! descendant can never hang [`run`]. For full cgroup-level containment on Linux, use
//! [`crate::detach`] with a systemd user scope.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use process_wrap::tokio::{ChildWrapper, CommandWrap};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::shell::{self, CommandSpec};
use crate::{OsError, Result};

#[cfg(unix)]
#[path = "run/unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "run/windows.rs"]
mod platform;

/// Default maximum retained bytes per output stream (1 MiB).
pub const DEFAULT_CAPTURE_LIMIT: usize = 1024 * 1024;

/// Default grace period between `SIGTERM` and `SIGKILL` on Unix when stopping a timed-out or
/// cancelled command tree.
pub const DEFAULT_KILL_GRACE: Duration = Duration::from_secs(2);

/// Maximum time to drain buffered output from pipes after the process tree has been killed.
const POST_KILL_DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// Which standard stream an [`OutputChunk`] arrived on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// One raw chunk of output read while a command is running.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputChunk {
    /// Stream the bytes were read from.
    pub stream: OutputStream,
    /// Bytes delivered in this read.
    pub bytes: Vec<u8>,
}

/// Callback invoked synchronously from the stream reader task as output chunks arrive.
pub type ChunkCallback = Arc<dyn Fn(OutputChunk) + Send + Sync>;

/// Default virtual-address-space headroom added above `memory_limit` for Linux `RLIMIT_AS`
/// (192 MiB covers binary + libc + thread stack mappings while blocking multi-hundred-MiB bombs).
pub const DEFAULT_RLIMIT_AS_HEADROOM: u64 = 192 * 1024 * 1024;

/// Default RSS watchdog sampling interval when `memory_limit` is set.
pub const DEFAULT_RSS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Input configuration for [`run`].
pub struct RunOptions {
    /// Exact argv or shell string to execute.
    pub command: CommandSpec,
    /// Optional working directory; inherits the caller's current directory when `None`.
    pub cwd: Option<PathBuf>,
    /// When `true`, clears the inherited environment before applying [`Self::env`].
    pub env_clear: bool,
    /// Extra environment variables applied on top of the inherited (or cleared) environment.
    pub env: BTreeMap<OsString, OsString>,
    /// Inherited environment variable names to drop before applying explicit [`Self::env`] entries.
    pub env_remove: Vec<OsString>,
    /// Optional bytes written to the child's stdin before closing it; null stdin when `None`.
    pub stdin: Option<Vec<u8>>,
    /// Optional already-opened [`File`] attached directly as the child's stdin (supersedes
    /// [`Self::stdin`]), allowing a worker child to read an open file handle without opening paths.
    pub stdin_file: Option<File>,
    /// Optional wall-clock timeout; exceeding it terminates the entire process tree.
    pub timeout: Option<Duration>,
    /// Optional hard resident memory limit in bytes, enforced by platform OS limits (`RLIMIT_AS`
    /// on Linux, Job Object memory limits on Windows) and a cross-platform RSS watchdog.
    pub memory_limit: Option<u64>,
    /// Extra virtual-address-space headroom added to `memory_limit` for Linux `RLIMIT_AS`.
    pub rlimit_as_headroom: u64,
    /// Polling interval for the RSS watchdog when [`Self::memory_limit`] is set.
    pub rss_poll_interval: Duration,
    /// Grace period between `SIGTERM` and `SIGKILL` on Unix during timeout or cancellation.
    pub kill_grace: Duration,
    /// Maximum number of raw bytes kept per stream (head + tail combined).
    pub capture_limit: usize,
    /// When `true`, captures exact raw stdout bytes (up to [`Self::capture_limit`]) into
    /// [`RunOutput::stdout_bytes`] for binary worker framing.
    pub raw_stdout: bool,
    /// Cooperative cancellation token; cancelling stops the entire process tree.
    pub cancel: CancellationToken,
    /// Optional callback receiving raw stdout/stderr chunks as they arrive.
    pub on_output: Option<ChunkCallback>,
}

impl fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunOptions")
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("env_remove", &self.env_remove)
            .field("stdin_len", &self.stdin.as_ref().map(Vec::len))
            .field("has_stdin_file", &self.stdin_file.is_some())
            .field("timeout", &self.timeout)
            .field("memory_limit", &self.memory_limit)
            .field("rlimit_as_headroom", &self.rlimit_as_headroom)
            .field("kill_grace", &self.kill_grace)
            .field("capture_limit", &self.capture_limit)
            .field("raw_stdout", &self.raw_stdout)
            .field("cancelled", &self.cancel.is_cancelled())
            .field("has_on_output", &self.on_output.is_some())
            .finish()
    }
}

impl RunOptions {
    /// Create run options for `command` with sensible defaults (1 MiB capture limit per stream,
    /// 2 s kill grace, no timeout, no memory limit, no cancellation).
    pub fn new(command: CommandSpec) -> Self {
        Self {
            command,
            cwd: None,
            env_clear: false,
            env: BTreeMap::new(),
            env_remove: Vec::new(),
            stdin: None,
            stdin_file: None,
            timeout: None,
            memory_limit: None,
            rlimit_as_headroom: DEFAULT_RLIMIT_AS_HEADROOM,
            rss_poll_interval: DEFAULT_RSS_POLL_INTERVAL,
            kill_grace: DEFAULT_KILL_GRACE,
            capture_limit: DEFAULT_CAPTURE_LIMIT,
            raw_stdout: false,
            cancel: CancellationToken::new(),
            on_output: None,
        }
    }

    /// Set the working directory for the child process.
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Set a single environment variable for the child process.
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Clear the inherited environment before applying explicit [`Self::env`] entries.
    pub fn env_clear(mut self, clear: bool) -> Self {
        self.env_clear = clear;
        self
    }

    /// Set multiple environment variables for the child process.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        for (k, v) in vars {
            self.env.insert(k.into(), v.into());
        }
        self
    }

    /// Remove an inherited environment variable before explicit [`Self::env`] entries are applied.
    pub fn env_remove(mut self, key: impl Into<OsString>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    /// Remove multiple inherited environment variables before explicit [`Self::env`] entries are applied.
    pub fn env_removes<I, K>(mut self, keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: Into<OsString>,
    {
        self.env_remove.extend(keys.into_iter().map(Into::into));
        self
    }

    /// Force the `C` locale (`LC_ALL=C`, `LANG=C`) for parseable command output.
    pub fn c_locale(mut self) -> Self {
        shell::with_c_locale(&mut self.env);
        self
    }

    /// Provide bytes to write to the child's stdin before closing it.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Attach an already-opened [`File`] directly to the child's stdin.
    pub fn stdin_file(mut self, file: File) -> Self {
        self.stdin_file = Some(file);
        self
    }

    /// Set a wall-clock timeout for the command.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Enforce a hard resident memory limit in bytes on the child process.
    pub fn memory_limit(mut self, bytes: u64) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    /// Override the Linux `RLIMIT_AS` headroom added above `memory_limit`.
    pub fn rlimit_as_headroom(mut self, bytes: u64) -> Self {
        self.rlimit_as_headroom = bytes;
        self
    }

    /// Capture exact raw stdout bytes into [`RunOutput::stdout_bytes`] (up to `capture_limit`).
    pub fn raw_stdout(mut self, enable: bool) -> Self {
        self.raw_stdout = enable;
        self
    }

    /// Set the grace period between `SIGTERM` and `SIGKILL` on Unix.
    pub fn kill_grace(mut self, grace: Duration) -> Self {
        self.kill_grace = grace;
        self
    }

    /// Set the per-stream capture limit in bytes.
    pub fn capture_limit(mut self, limit: usize) -> Self {
        self.capture_limit = limit;
        self
    }

    /// Attach a cooperative cancellation token.
    pub fn cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = token;
        self
    }

    /// Register a callback invoked as stdout and stderr chunks arrive.
    pub fn on_output(mut self, callback: ChunkCallback) -> Self {
        self.on_output = Some(callback);
        self
    }
}

/// Bounded capture of one output stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapturedOutput {
    /// Lossy UTF-8 text captured from the stream. When [`Self::truncated`] is `true`, retains
    /// both the head and the tail separated by an omission marker.
    pub text: String,
    /// `true` when the stream emitted more bytes than the configured capture limit.
    pub truncated: bool,
    /// Total raw bytes read from the stream before any truncation.
    pub total_bytes: u64,
}

/// Result of executing a command with [`run`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunOutput {
    /// Exit code returned by the process, when it exited normally.
    pub exit_code: Option<i32>,
    /// Unix signal number that terminated the process, when killed by a signal. Always `None` on Windows.
    pub signal: Option<i32>,
    /// Captured standard output.
    pub stdout: CapturedOutput,
    /// Exact raw standard output bytes when [`RunOptions::raw_stdout`] is `true` (bounded by
    /// `capture_limit`); empty otherwise.
    pub stdout_bytes: Vec<u8>,
    /// Captured standard error.
    pub stderr: CapturedOutput,
    /// `true` if the command exceeded its timeout and its process tree was terminated.
    pub timed_out: bool,
    /// `true` if the cancellation token fired and its process tree was terminated.
    pub cancelled: bool,
    /// `true` if the process was terminated for exceeding [`RunOptions::memory_limit`] (either
    /// directly by our RSS watchdog, by our worker capped allocator exit code, or by an explicit
    /// allocator OOM message/status under our limit — never an unrelated external `SIGKILL`).
    pub killed_for_memory: bool,
    /// `true` specifically when our RSS watchdog sampled the child above [`RunOptions::memory_limit`]
    /// and terminated it.
    pub memory_watchdog_killed: bool,
    /// Peak resident memory (RSS) in bytes observed for the child while it ran (when sampled).
    pub peak_rss_bytes: u64,
    /// Cumulative user + kernel CPU time observed for the child right before termination or exit,
    /// when available from the OS.
    pub cpu_time: Option<Duration>,
    /// Elapsed wall-clock duration of the call.
    pub duration: Duration,
}

impl RunOutput {
    /// Return `true` when the command exited with code 0 without timing out, being cancelled, or
    /// exceeding its memory limit.
    pub fn success(&self) -> bool {
        !self.timed_out && !self.cancelled && !self.killed_for_memory && self.exit_code == Some(0)
    }
}

/// Run a command to completion, bounded by timeout, cancellation, and per-stream capture limits.
///
/// If the requested executable does not exist, returns [`OsError::ProgramNotFound`] naming it.
/// On timeout or cancellation, the entire process tree is stopped before returning.
pub async fn run(options: RunOptions) -> Result<RunOutput> {
    let started = Instant::now();
    let resolved = shell::resolve(&options.command)?;

    // Validate cwd explicitly so a missing working directory is never misreported as a missing
    // executable (both produce ErrorKind::NotFound from spawn).
    if let Some(ref cwd) = options.cwd {
        let meta = tokio::fs::metadata(cwd)
            .await
            .map_err(|err| match err.kind() {
                io::ErrorKind::NotFound => OsError::NotFound {
                    detail: format!("working directory not found: {}", cwd.display()),
                },
                _ => OsError::from_io(
                    format!("cannot access working directory {}", cwd.display()),
                    err,
                ),
            })?;
        if !meta.is_dir() {
            return Err(OsError::InvalidArgument {
                detail: format!("working directory is not a directory: {}", cwd.display()),
            });
        }
    }

    if options.cancel.is_cancelled() {
        return Ok(RunOutput {
            exit_code: None,
            signal: None,
            stdout: CapturedOutput::default(),
            stdout_bytes: Vec::new(),
            stderr: CapturedOutput::default(),
            timed_out: false,
            cancelled: true,
            killed_for_memory: false,
            memory_watchdog_killed: false,
            peak_rss_bytes: 0,
            cpu_time: None,
            duration: started.elapsed(),
        });
    }

    let program_display = resolved.program_display();
    let mut stdin_file = options.stdin_file;
    let has_stdin_bytes = stdin_file.is_none() && options.stdin.is_some();
    let memory_limit = options.memory_limit;
    let rlimit_as_headroom = options.rlimit_as_headroom;
    let env_clear = options.env_clear;
    let mut wrapped = CommandWrap::with_new(&resolved.program, |cmd| {
        cmd.args(&resolved.args);
        if env_clear {
            cmd.env_clear();
        } else {
            for key in &resolved.env_remove {
                cmd.env_remove(key);
            }
            for key in &options.env_remove {
                cmd.env_remove(key);
            }
        }
        for (key, value) in &options.env {
            cmd.env(key, value);
        }
        if let Some(ref cwd) = options.cwd {
            cmd.current_dir(cwd);
        }
        if let Some(file) = stdin_file.take() {
            cmd.stdin(Stdio::from(file));
        } else if has_stdin_bytes {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        configure_pre_exec_memory_limit(cmd, memory_limit, rlimit_as_headroom);
    });
    configure_containment(&mut wrapped, memory_limit.is_some())?;

    let mut child = wrapped
        .spawn()
        .map_err(|err| OsError::from_spawn_io(program_display, err))?;
    let child_pid = child.id();
    let mut tree_guard = create_drop_guard(child_pid, memory_limit, rlimit_as_headroom);

    let stdin_task = if has_stdin_bytes {
        match (child.stdin().take(), options.stdin) {
            (Some(mut stdin_pipe), Some(bytes)) => Some(tokio::spawn(async move {
                let _ = stdin_pipe.write_all(&bytes).await;
                let _ = stdin_pipe.shutdown().await;
            })),
            _ => None,
        }
    } else {
        None
    };

    let drain_stop = CancellationToken::new();
    let _drain_guard = drain_stop.clone().drop_guard();
    let raw_stdout = options.raw_stdout;
    let mut stdout_task = child.stdout().take().map(|pipe| {
        tokio::spawn(read_bounded_stream(
            pipe,
            options.capture_limit,
            OutputStream::Stdout,
            options.on_output.clone(),
            drain_stop.clone(),
            raw_stdout,
        ))
    });
    let mut stderr_task = child.stderr().take().map(|pipe| {
        tokio::spawn(read_bounded_stream(
            pipe,
            options.capture_limit,
            OutputStream::Stderr,
            options.on_output.clone(),
            drain_stop.clone(),
            false,
        ))
    });

    let deadline = options.timeout.map(|d| tokio::time::Instant::now() + d);
    let peak_rss = Arc::new(AtomicU64::new(0));
    let mut exit_status: Option<ExitStatus> = None;
    let mut timed_out = false;
    let mut cancelled = false;
    let mut memory_watchdog_killed = false;
    let mut cpu_time: Option<Duration> = None;

    // Phase 1: wait for the DIRECT child to exit (never blocking on grandchildren in a Job Object
    // or process group), or for timeout / cancellation / RSS watchdog breach.
    tokio::select! {
        status_res = child.inner_mut().wait() => {
            exit_status = Some(status_res.map_err(|err| OsError::from_io("failed waiting for child process", err))?);
        }
        () = wait_deadline(deadline) => {
            timed_out = true;
            cpu_time = child_pid.and_then(crate::mem_limit::process_cpu_time);
        }
        () = options.cancel.cancelled() => {
            cancelled = true;
        }
        () = watch_memory_limit(child_pid, memory_limit, options.rss_poll_interval, Arc::clone(&peak_rss)) => {
            memory_watchdog_killed = true;
            cpu_time = child_pid.and_then(crate::mem_limit::process_cpu_time);
        }
    }

    let mut stdout_captured: Option<(CapturedOutput, Vec<u8>)> = None;
    let mut stderr_captured: Option<(CapturedOutput, Vec<u8>)> = None;

    // Phase 2: stop any surviving processes in the group/job under one consistent rule on every
    // platform—whether the direct child exited normally or we hit timeout/cancellation/OOM.
    if memory_watchdog_killed {
        // Immediate kill with zero grace so a runaway allocator stops at once.
        let kill_status = terminate_tree(&mut child, child_pid, Duration::ZERO).await;
        if exit_status.is_none() {
            exit_status = kill_status;
        }
    } else if timed_out || cancelled {
        let kill_status = terminate_tree(&mut child, child_pid, options.kill_grace).await;
        if exit_status.is_none() {
            exit_status = kill_status;
        }
    } else {
        cleanup_after_direct_exit(&mut child, child_pid, options.kill_grace).await;
    }

    // Phase 3: drain buffered pipe output within a time bound so any escaped descendant holding
    // stdout/stderr open can never hang the caller.
    let _ = tokio::time::timeout(
        POST_KILL_DRAIN_TIMEOUT,
        drain_pipes(
            &mut stdout_task,
            &mut stderr_task,
            &mut stdout_captured,
            &mut stderr_captured,
        ),
    )
    .await;

    drain_stop.cancel();
    drain_pipes(
        &mut stdout_task,
        &mut stderr_task,
        &mut stdout_captured,
        &mut stderr_captured,
    )
    .await;

    if let Some(handle) = stdin_task {
        handle.abort();
    }
    tree_guard.disarm();
    drop(child);

    let (exit_code, signal) = exit_status.map_or((None, None), extract_status);
    let (stdout, stdout_bytes) = stdout_captured.unwrap_or_default();
    let (stderr, _) = stderr_captured.unwrap_or_default();

    // Only classify a death as `killed_for_memory` when THIS node requested a memory limit for the
    // child and its own memory limit decided it:
    // - our RSS watchdog observed `rss > limit`,
    // - under an active `memory_limit`, our worker's `CappedAlloc` exited with
    //   `CAP_EXCEEDED_EXIT_CODE` (`107`), or
    // - under an active `memory_limit`, stderr explicitly reports an allocation failure (e.g.
    //   Rust's `memory allocation of ... failed` under `RLIMIT_AS`) or Windows `STATUS_NO_MEMORY`.
    // Never classify an unrelated child's exit code 107 or an external `SIGKILL` as a memory kill.
    let mut killed_for_memory = memory_watchdog_killed
        || (memory_limit.is_some()
            && exit_code == Some(crate::capped_alloc::CAP_EXCEEDED_EXIT_CODE));
    if memory_limit.is_some()
        && !killed_for_memory
        && !timed_out
        && !cancelled
        && exit_code != Some(0)
    {
        let err_lower = stderr.text.to_ascii_lowercase();
        const WINDOWS_STATUS_NO_MEMORY: i32 = 0xC000_0017u32 as i32;
        if err_lower.contains("memory allocation of")
            || err_lower.contains("out of memory")
            || err_lower.contains("cannot allocate memory")
            || exit_code == Some(WINDOWS_STATUS_NO_MEMORY)
        {
            killed_for_memory = true;
        }
    }

    Ok(RunOutput {
        exit_code,
        signal,
        stdout,
        stdout_bytes,
        stderr,
        timed_out,
        cancelled,
        killed_for_memory,
        memory_watchdog_killed,
        peak_rss_bytes: peak_rss.load(Ordering::Relaxed),
        cpu_time,
        duration: started.elapsed(),
    })
}

async fn watch_memory_limit(
    child_pid: Option<u32>,
    memory_limit: Option<u64>,
    poll_interval: Duration,
    peak_rss: Arc<AtomicU64>,
) {
    let (Some(pid), Some(limit)) = (child_pid, memory_limit) else {
        std::future::pending::<()>().await;
        return;
    };
    let step = poll_interval.max(Duration::from_millis(5));
    loop {
        if let Some(rss) = crate::mem_limit::process_rss_bytes(pid) {
            peak_rss.fetch_max(rss, Ordering::Relaxed);
            if rss > limit {
                return;
            }
        }
        tokio::time::sleep(step).await;
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

async fn drain_pipes(
    stdout_task: &mut Option<JoinHandle<(CapturedOutput, Vec<u8>)>>,
    stderr_task: &mut Option<JoinHandle<(CapturedOutput, Vec<u8>)>>,
    stdout_captured: &mut Option<(CapturedOutput, Vec<u8>)>,
    stderr_captured: &mut Option<(CapturedOutput, Vec<u8>)>,
) {
    while stdout_task.is_some() || stderr_task.is_some() {
        match (stdout_task.as_mut(), stderr_task.as_mut()) {
            (Some(out_handle), Some(err_handle)) => {
                tokio::select! {
                    res = out_handle => {
                        if let Ok(captured) = res {
                            *stdout_captured = Some(captured);
                        }
                        *stdout_task = None;
                    }
                    res = err_handle => {
                        if let Ok(captured) = res {
                            *stderr_captured = Some(captured);
                        }
                        *stderr_task = None;
                    }
                }
            }
            (Some(out_handle), None) => {
                if let Ok(captured) = out_handle.await {
                    *stdout_captured = Some(captured);
                }
                *stdout_task = None;
            }
            (None, Some(err_handle)) => {
                if let Ok(captured) = err_handle.await {
                    *stderr_captured = Some(captured);
                }
                *stderr_task = None;
            }
            (None, None) => break,
        }
    }
}

async fn read_bounded_stream<R>(
    mut reader: R,
    limit: usize,
    stream: OutputStream,
    on_output: Option<ChunkCallback>,
    stop: CancellationToken,
    keep_raw: bool,
) -> (CapturedOutput, Vec<u8>)
where
    R: AsyncRead + Unpin,
{
    let mut capture = BoundedCapture::new(limit);
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            read_res = reader.read(&mut buf) => {
                match read_res {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if keep_raw {
                            let room = limit.saturating_sub(raw.len());
                            if room > 0 {
                                raw.extend_from_slice(&chunk[..room.min(chunk.len())]);
                            }
                            capture.total_bytes = capture.total_bytes.saturating_add(chunk.len() as u64);
                            if chunk.len() > room {
                                capture.truncated = true;
                            }
                        } else {
                            capture.push(chunk);
                        }
                        if let Some(ref cb) = on_output {
                            cb(OutputChunk {
                                stream,
                                bytes: chunk.to_vec(),
                            });
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    (capture.finish(), raw)
}

/// Memory-bounded head-and-tail byte accumulator for one output stream.
struct BoundedCapture {
    limit: usize,
    head_limit: usize,
    tail_limit: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: u64,
    truncated: bool,
}

impl BoundedCapture {
    fn new(limit: usize) -> Self {
        let head_limit = limit.div_ceil(2);
        let tail_limit = limit - head_limit;
        Self {
            limit,
            head_limit,
            tail_limit,
            head: Vec::new(),
            tail: VecDeque::new(),
            total_bytes: 0,
            truncated: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes.len() as u64);

        let head_room = self.head_limit.saturating_sub(self.head.len());
        let take_head = head_room.min(bytes.len());
        if take_head > 0 {
            self.head.extend_from_slice(&bytes[..take_head]);
        }

        let rest = &bytes[take_head..];
        if rest.is_empty() {
            return;
        }

        if self.tail_limit == 0 {
            self.truncated = true;
            return;
        }

        if rest.len() >= self.tail_limit {
            if !self.tail.is_empty() || rest.len() > self.tail_limit {
                self.truncated = true;
            }
            self.tail.clear();
            let start = rest.len() - self.tail_limit;
            self.tail.extend(&rest[start..]);
        } else {
            let excess = (self.tail.len() + rest.len()).saturating_sub(self.tail_limit);
            if excess > 0 {
                self.truncated = true;
                self.tail.drain(..excess);
            }
            self.tail.extend(rest);
        }
    }

    fn finish(self) -> CapturedOutput {
        if !self.truncated {
            let mut combined = self.head;
            combined.extend(self.tail);
            return CapturedOutput {
                text: String::from_utf8_lossy(&combined).into_owned(),
                truncated: false,
                total_bytes: self.total_bytes,
            };
        }

        if self.limit == 0 {
            return CapturedOutput {
                text: String::new(),
                truncated: true,
                total_bytes: self.total_bytes,
            };
        }

        let retained = (self.head.len() + self.tail.len()) as u64;
        let omitted = self.total_bytes.saturating_sub(retained);
        let head_str = String::from_utf8_lossy(&self.head);
        let tail_bytes: Vec<u8> = self.tail.into_iter().collect();
        let tail_str = String::from_utf8_lossy(&tail_bytes);

        let text = if tail_str.is_empty() {
            format!("{head_str}\n[... {omitted} bytes omitted ...]")
        } else {
            format!("{head_str}\n[... {omitted} bytes omitted ...]\n{tail_str}")
        };

        CapturedOutput {
            text,
            truncated: true,
            total_bytes: self.total_bytes,
        }
    }
}

#[cfg(any(unix, windows))]
fn create_drop_guard(
    child_pid: Option<u32>,
    memory_limit: Option<u64>,
    memory_headroom: u64,
) -> platform::TreeDropGuard {
    platform::TreeDropGuard::new(child_pid, memory_limit, memory_headroom)
}

#[cfg(not(any(unix, windows)))]
struct FallbackDropGuard;

#[cfg(not(any(unix, windows)))]
impl FallbackDropGuard {
    fn disarm(&mut self) {}
}

#[cfg(not(any(unix, windows)))]
fn create_drop_guard(
    _child_pid: Option<u32>,
    _memory_limit: Option<u64>,
    _memory_headroom: u64,
) -> FallbackDropGuard {
    FallbackDropGuard
}

#[cfg(any(unix, windows))]
fn configure_pre_exec_memory_limit(
    cmd: &mut tokio::process::Command,
    memory_limit: Option<u64>,
    rlimit_as_headroom: u64,
) {
    platform::configure_pre_exec_memory_limit(cmd, memory_limit, rlimit_as_headroom);
}

#[cfg(not(any(unix, windows)))]
fn configure_pre_exec_memory_limit(
    _cmd: &mut tokio::process::Command,
    _memory_limit: Option<u64>,
    _rlimit_as_headroom: u64,
) {
}

#[cfg(any(unix, windows))]
fn configure_containment(wrapped: &mut CommandWrap, suspend_for_memory_job: bool) -> Result<()> {
    platform::configure_containment(wrapped, suspend_for_memory_job);
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn configure_containment(_wrapped: &mut CommandWrap, _suspend_for_memory_job: bool) -> Result<()> {
    Err(OsError::Unsupported {
        detail: "process-tree containment is not supported on this platform".into(),
    })
}

#[cfg(any(unix, windows))]
async fn cleanup_after_direct_exit(
    child: &mut Box<dyn ChildWrapper>,
    child_pid: Option<u32>,
    grace: Duration,
) {
    platform::cleanup_after_direct_exit(child, child_pid, grace).await;
}

#[cfg(not(any(unix, windows)))]
async fn cleanup_after_direct_exit(
    _child: &mut Box<dyn ChildWrapper>,
    _child_pid: Option<u32>,
    _grace: Duration,
) {
}

#[cfg(any(unix, windows))]
async fn terminate_tree(
    child: &mut Box<dyn ChildWrapper>,
    child_pid: Option<u32>,
    grace: Duration,
) -> Option<ExitStatus> {
    platform::terminate_tree(child, child_pid, grace).await
}

#[cfg(not(any(unix, windows)))]
async fn terminate_tree(
    _child: &mut Box<dyn ChildWrapper>,
    _child_pid: Option<u32>,
    _grace: Duration,
) -> Option<ExitStatus> {
    None
}

#[cfg(any(unix, windows))]
fn extract_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    platform::extract_status(status)
}

#[cfg(not(any(unix, windows)))]
fn extract_status(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    (status.code(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_capture_preserves_exact_bytes_up_to_limit() {
        let mut cap = BoundedCapture::new(10);
        cap.push(b"0123");
        cap.push(b"456789");
        let out = cap.finish();
        assert!(!out.truncated);
        assert_eq!(out.total_bytes, 10);
        assert_eq!(out.text, "0123456789");
    }

    #[test]
    fn bounded_capture_keeps_head_and_tail_independent_of_chunking() {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"HEAD_MARKER:");
        payload.extend(std::iter::repeat_n(b'x', 500));
        payload.extend_from_slice(b":TAIL_MARKER");

        let mut single = BoundedCapture::new(40);
        single.push(&payload);
        let out_single = single.finish();

        let mut chunked = BoundedCapture::new(40);
        for chunk in payload.chunks(7) {
            chunked.push(chunk);
        }
        let out_chunked = chunked.finish();

        assert_eq!(out_single, out_chunked);
        assert!(out_single.truncated);
        assert_eq!(out_single.total_bytes, payload.len() as u64);
        assert!(out_single.text.starts_with("HEAD_MARKER:"));
        assert!(out_single.text.ends_with(":TAIL_MARKER"));
        assert!(out_single.text.contains("bytes omitted"));
    }

    #[test]
    fn bounded_capture_handles_zero_limit_and_invalid_utf8() {
        let mut zero = BoundedCapture::new(0);
        zero.push(b"dropped");
        let out = zero.finish();
        assert!(out.truncated);
        assert_eq!(out.total_bytes, 7);
        assert_eq!(out.text, "");

        let mut lossy = BoundedCapture::new(32);
        lossy.push(&[0xff, b'o', b'k', 0xfe]);
        let out = lossy.finish();
        assert!(!out.truncated);
        assert_eq!(out.total_bytes, 4);
        assert_eq!(out.text, "\u{fffd}ok\u{fffd}");
    }

    #[tokio::test]
    async fn run_reports_missing_program_with_typed_error() {
        let missing = "legion-definitely-nonexistent-program-9c8a7b";
        let err = run(RunOptions::new(CommandSpec::argv(
            missing,
            Vec::<&str>::new(),
        )))
        .await
        .unwrap_err();
        match err {
            OsError::ProgramNotFound { program } => assert_eq!(program, missing),
            other => panic!("expected ProgramNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_reports_missing_cwd_as_not_found_not_program_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent_cwd = dir.path().join("does-not-exist");
        let err = run(
            RunOptions::new(CommandSpec::argv("definitely-unused", Vec::<&str>::new()))
                .cwd(&nonexistent_cwd),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, OsError::NotFound { .. }));
    }

    #[tokio::test]
    async fn run_respects_pre_cancelled_token_without_spawning() {
        let token = CancellationToken::new();
        token.cancel();
        let out = run(RunOptions::new(CommandSpec::argv(
            "nonexistent-never-spawned",
            Vec::<&str>::new(),
        ))
        .cancel(token))
        .await
        .unwrap();
        assert!(out.cancelled);
        assert!(!out.timed_out);
        assert!(!out.success());
    }

    #[tokio::test]
    async fn run_exit_107_without_memory_limit_is_not_killed_for_memory() {
        let cmd = if cfg!(windows) {
            CommandSpec::shell("exit /b 107")
        } else {
            CommandSpec::shell("exit 107")
        };
        let out = run(RunOptions::new(cmd)).await.unwrap();
        assert_eq!(
            out.exit_code,
            Some(crate::capped_alloc::CAP_EXCEEDED_EXIT_CODE)
        );
        assert!(
            !out.killed_for_memory,
            "exit 107 must not be treated as killed_for_memory when no memory_limit was set"
        );
    }
}
