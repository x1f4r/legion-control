//! Unix detached process launcher and systemd scope teardown.
//!
//! On Linux, when a `systemd --user` manager is reachable, the target is started through
//! `systemd-run --user --scope --quiet --collect --unit=legion-job-<name> --` so the transient
//! scope's cgroup becomes the tree boundary: the job survives SSH session teardown (when linger is
//! enabled) and [`stop_detached`] can stop every process inside the scope—even descendants that
//! called `setsid`. On macOS, containers, or Linux systems without a user manager, the process
//! starts directly in a new session (`setsid`) and [`stop_detached`] delegates to
//! [`procid::kill_tree`].

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use process_wrap::std::{CommandWrap, ProcessSession};

#[cfg(target_os = "linux")]
use crate::procid::ProcessId;
use crate::procid::{self, KillTreeReport};
use crate::shell::{ResolvedCommand, find_executable_in_paths};
use crate::{OsError, Result};

use super::{DetachHandle, DetachOptions, open_append_stdio};

#[cfg(target_os = "linux")]
const SYSTEMD_USER_SCOPE_ARGS: &[&str] = &["--user", "--scope", "--quiet", "--collect"];

#[cfg(target_os = "linux")]
const SYSTEMD_CMD_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
const SCOPE_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[cfg(target_os = "linux")]
const SCOPE_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(target_os = "linux")]
const SCOPE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
const SCOPE_CONFIRM_POLL: Duration = Duration::from_millis(10);

#[cfg(target_os = "linux")]
const SYSTEMD_STDERR_CAPTURE_LIMIT: usize = 4096;

#[cfg(target_os = "linux")]
enum ScopeStderrCapture<'a> {
    /// Target has a configured stderr file; read any bytes `systemd-run` appended past `offset`.
    LogFile { path: &'a Path, offset: u64 },
    /// Target has null stderr; a tiny `/bin/sh` exec shim redirects fd 2 to `/dev/null` once
    /// `systemd-run` enters the scope, leaving our pipe open exclusively to `systemd-run`'s own
    /// error output before exec.
    Pipe(Option<std::process::ChildStderr>),
}

#[cfg(target_os = "linux")]
impl ScopeStderrCapture<'_> {
    fn read_error_output(&mut self) -> String {
        use std::io::{Read, Seek, SeekFrom};

        match self {
            Self::LogFile { path, offset } => {
                let Ok(mut file) = std::fs::File::open(path) else {
                    return String::new();
                };
                if file.seek(SeekFrom::Start(*offset)).is_err() {
                    return String::new();
                }
                let mut buf = Vec::new();
                let _ = file
                    .take(SYSTEMD_STDERR_CAPTURE_LIMIT as u64)
                    .read_to_end(&mut buf);
                String::from_utf8_lossy(&buf).trim().to_owned()
            }
            Self::Pipe(pipe_slot) => {
                let Some(pipe) = pipe_slot.take() else {
                    return String::new();
                };
                let mut buf = Vec::new();
                let _ = pipe
                    .take(SYSTEMD_STDERR_CAPTURE_LIMIT as u64)
                    .read_to_end(&mut buf);
                String::from_utf8_lossy(&buf).trim().to_owned()
            }
        }
    }
}

pub(super) async fn spawn_detached(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<DetachHandle> {
    let mut warnings = Vec::new();
    let use_systemd_scope = if systemd_user_scope_available(options).await {
        if systemd_linger_enabled().await {
            true
        } else {
            // Without lingering, logind tears down `systemd --user` ~10 s after logout and takes
            // any transient user scope with it; fall back to `setsid` and warn the caller.
            warnings.push(super::LINGER_DISABLED_WARNING.to_owned());
            false
        }
    } else {
        false
    };

    // `systemd-run` itself exists even when the target executable does not, so validate the
    // target executable up front to preserve the typed `ProgramNotFound` error.
    if use_systemd_scope {
        ensure_target_exists(&resolved.program, options)?;
    }

    #[cfg(target_os = "linux")]
    let stderr_log_offset = if use_systemd_scope {
        options
            .stderr_path
            .as_deref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    };

    let (stdout_stdio, stderr_stdio) = open_append_stdio(
        options.stdout_path.as_deref(),
        options.stderr_path.as_deref(),
    )?;

    let program_display = resolved.program_display();
    let (mut wrapped, systemd_unit) = if use_systemd_scope {
        #[cfg(target_os = "linux")]
        {
            let (unit_stem, full_unit) = super::resolve_scope_unit_name(options.name.as_deref())?;
            let unit_arg = format!("--unit={unit_stem}");
            let pipe_systemd_stderr = options.stderr_path.is_none();
            let scope_stderr = if pipe_systemd_stderr {
                Stdio::piped()
            } else {
                stderr_stdio
            };
            let wrap = CommandWrap::with_new("systemd-run", |cmd| {
                cmd.args(SYSTEMD_USER_SCOPE_ARGS);
                cmd.arg(&unit_arg);
                cmd.arg("--");
                if pipe_systemd_stderr {
                    // When the caller requested null stderr, pipe `systemd-run`'s own stderr so
                    // scope-creation errors are surfaced while `/bin/sh` immediately redirects
                    // fd 2 to `/dev/null` before `exec`-ing the target in the same PID.
                    let sh = if Path::new("/bin/sh").is_file() {
                        "/bin/sh"
                    } else {
                        "sh"
                    };
                    cmd.args([sh, "-c", "exec 2>/dev/null; exec -- \"$@\"", "_"]);
                }
                cmd.arg(&resolved.program);
                cmd.args(&resolved.args);
                configure_common_cmd(cmd, resolved, options, stdout_stdio, scope_stderr);
            });
            (wrap, Some(full_unit))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let wrap = CommandWrap::with_new(&resolved.program, |cmd| {
                cmd.args(&resolved.args);
                configure_common_cmd(cmd, resolved, options, stdout_stdio, stderr_stdio);
            });
            (wrap, None)
        }
    } else {
        let wrap = CommandWrap::with_new(&resolved.program, |cmd| {
            cmd.args(&resolved.args);
            configure_common_cmd(cmd, resolved, options, stdout_stdio, stderr_stdio);
        });
        (wrap, None)
    };

    // Always place the detached process in a new session (`setsid`) so it has no controlling
    // terminal and sits in its own process group.
    wrapped.wrap(ProcessSession);

    let child = wrapped
        .spawn()
        .map_err(|err| OsError::from_spawn_io(program_display, err))?;
    let pid = child.id();

    // Capture the kernel start timestamp BEFORE confirming scope membership or spawning the
    // reaper thread so even a fast-exiting process still has an unreaped entry in the table.
    let process = procid::of_pid_including_zombie(pid)?.ok_or_else(|| OsError::Platform {
        detail: format!("could not read process identity for detached pid {pid}"),
    })?;

    let mut inner = child.into_inner();

    #[cfg(target_os = "linux")]
    if let Some(ref unit) = systemd_unit {
        let mut capture = match options.stderr_path.as_deref() {
            Some(path) => ScopeStderrCapture::LogFile {
                path,
                offset: stderr_log_offset,
            },
            None => ScopeStderrCapture::Pipe(inner.stderr().take()),
        };
        confirm_scope_entry(
            inner.as_mut(),
            pid,
            unit,
            &mut capture,
            SCOPE_CONFIRM_TIMEOUT,
        )
        .await?;
    }

    // If this process stays alive (for example as a long-running server), reap the detached child
    // once it eventually exits so it does not linger as a zombie. If this process exits first,
    // init/systemd adopts and reaps the detached child while it continues running.
    let _ = std::thread::Builder::new()
        .name(format!("legion-reap-{pid}"))
        .spawn(move || {
            let _ = inner.wait();
        });

    Ok(DetachHandle {
        process,
        systemd_unit,
        warnings,
    })
}

/// Poll `/proc/<pid>/cgroup` until `pid` enters `/<full_unit>`, returning an error carrying
/// whatever `systemd-run` printed to stderr if the process exits before entering the scope or if
/// confirmation times out.
#[cfg(target_os = "linux")]
async fn confirm_scope_entry(
    child: &mut dyn process_wrap::std::ChildWrapper,
    pid: u32,
    full_unit: &str,
    stderr_capture: &mut ScopeStderrCapture<'_>,
    timeout: Duration,
) -> Result<()> {
    let cgroup_path = format!("/proc/{pid}/cgroup");
    let stat_path = format!("/proc/{pid}/stat");
    let deadline = Instant::now() + timeout;

    loop {
        if let Ok(cgroup) = std::fs::read_to_string(&cgroup_path)
            && super::cgroup_matches_unit(&cgroup, full_unit)
        {
            return Ok(());
        }

        // Inspect `/proc/<pid>/stat` BEFORE calling `try_wait()` (which reaps the zombie and
        // removes `/proc/<pid>/cgroup`). Once the child is a zombie its cgroup is frozen, so one
        // final cgroup check reliably distinguishes a fast-exiting scoped job from a failed
        // `systemd-run` invocation.
        let exited_zombie = match std::fs::read_to_string(&stat_path) {
            Ok(stat) => super::super::procid::parse_linux_proc_stat(&stat)
                .is_some_and(|(state, _)| matches!(state, 'Z' | 'X' | 'x')),
            Err(_) => true,
        };

        if exited_zombie {
            if let Ok(cgroup) = std::fs::read_to_string(&cgroup_path)
                && super::cgroup_matches_unit(&cgroup, full_unit)
            {
                return Ok(());
            }
            let status_desc = match child.wait() {
                Ok(status) => status.to_string(),
                Err(err) => format!("wait failed: {err}"),
            };
            let stderr_text = stderr_capture.read_error_output();
            let detail = if stderr_text.is_empty() {
                format!(
                    "systemd-run exited ({status_desc}) before process {pid} entered scope {full_unit}"
                )
            } else {
                format!(
                    "systemd-run exited ({status_desc}) before process {pid} entered scope {full_unit}: {stderr_text}"
                )
            };
            return Err(OsError::Platform { detail });
        }

        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr_text = stderr_capture.read_error_output();
            let detail = if stderr_text.is_empty() {
                format!("timed out waiting for process {pid} to enter systemd scope {full_unit}")
            } else {
                format!(
                    "timed out waiting for process {pid} to enter systemd scope {full_unit}: {stderr_text}"
                )
            };
            return Err(OsError::TimedOut { detail });
        }

        let remaining = deadline.saturating_duration_since(now);
        tokio::time::sleep(SCOPE_CONFIRM_POLL.min(remaining)).await;
    }
}

pub(super) async fn detached_active(handle: &DetachHandle) -> Result<bool> {
    #[cfg(target_os = "linux")]
    if let Some(ref unit) = handle.systemd_unit {
        return systemd_scope_active(unit, &handle.process).await;
    }

    procid::tree_is_active(&handle.process).await
}

#[cfg(target_os = "linux")]
async fn systemd_scope_active(unit: &str, root_process: &ProcessId) -> Result<bool> {
    let unit_owned = unit.to_owned();
    let root = *root_process;

    // Fast path off the async worker thread: check root liveness and read the standard cgroup v2
    // user scope directory directly (zero subprocesses for the common case).
    let direct = tokio::task::spawn_blocking(move || -> Option<bool> {
        if procid::is_alive(&root) {
            return Some(true);
        }

        // Guard against any path traversal in a deserialized handle's `systemd_unit`.
        if unit_owned.is_empty()
            || unit_owned.contains('/')
            || unit_owned.contains('\\')
            || unit_owned == "."
            || unit_owned == ".."
        {
            return Some(false);
        }

        let uid = nix::unistd::Uid::current().as_raw();
        let app_slice = Path::new("/sys/fs/cgroup")
            .join("user.slice")
            .join(format!("user-{uid}.slice"))
            .join(format!("user@{uid}.service"))
            .join("app.slice");

        if !app_slice.is_dir() {
            // Non-standard slice or cgroup v1 layout; fall back to `systemctl --user show`.
            return None;
        }

        let scope_dir = app_slice.join(&unit_owned);
        if !scope_dir.is_dir() {
            // Standard cgroup v2 user app.slice exists and the transient scope directory is gone
            // (and the root process is already dead).
            return Some(false);
        }

        // Check `cgroup.procs` for any live (non-zombie) process directly inside the scope.
        if let Ok(procs) = std::fs::read_to_string(scope_dir.join("cgroup.procs")) {
            let own_pid = std::process::id();
            for line in procs.lines() {
                if let Ok(pid) = line.trim().parse::<u32>()
                    && pid != own_pid
                    && matches!(procid::of_pid(pid), Ok(Some(_)))
                {
                    return Some(true);
                }
            }
        }

        // Check `cgroup.events` (`populated 0/1`) so nested sub-cgroups are accounted for too.
        if let Ok(events) = std::fs::read_to_string(scope_dir.join("cgroup.events"))
            && let Some(populated) = super::parse_cgroup_events_populated(&events)
        {
            return Some(populated);
        }

        None
    })
    .await
    .map_err(|err| OsError::Platform {
        detail: format!("detached_active worker task failed: {err}"),
    })?;

    if let Some(active) = direct {
        return Ok(active);
    }

    // Fallback for cgroup v1 or custom slice layouts: query systemd directly.
    let state = query_scope_state(unit).await;
    if !state.live_pids().is_empty() || state.is_active() {
        return Ok(true);
    }
    procid::tree_is_active(root_process).await
}

pub(super) async fn stop_detached(
    handle: &DetachHandle,
    grace: Duration,
) -> Result<KillTreeReport> {
    #[cfg(target_os = "linux")]
    if let Some(ref unit) = handle.systemd_unit {
        return stop_systemd_scope(unit, &handle.process, grace).await;
    }

    procid::kill_tree(&handle.process, grace).await
}

#[cfg(target_os = "linux")]
async fn stop_systemd_scope(
    unit: &str,
    root_process: &ProcessId,
    grace: Duration,
) -> Result<KillTreeReport> {
    let initial = query_scope_state(unit).await;

    // If the scope is already gone (or never activated), check whether the root process itself is
    // still alive and handle it via kill_tree if so.
    if !initial.is_active() && initial.live_pids().is_empty() {
        if procid::is_alive(root_process) {
            return procid::kill_tree(root_process, grace).await;
        }
        return Ok(KillTreeReport {
            all_gone: true,
            survivors: Vec::new(),
        });
    }

    // Phase 1: SIGTERM every process inside the scope cgroup, including `setsid` descendants.
    let _ = run_systemctl(&[
        "--user",
        "kill",
        "--kill-whom=all",
        "--signal=SIGTERM",
        unit,
    ])
    .await;

    let grace_deadline = Instant::now() + grace;
    loop {
        let state = query_scope_state(unit).await;
        if scope_is_gone(&state, root_process) {
            return Ok(KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
            });
        }
        let now = Instant::now();
        if now >= grace_deadline {
            break;
        }
        let remaining = grace_deadline.saturating_duration_since(now);
        tokio::time::sleep(SCOPE_POLL_INTERVAL.min(remaining)).await;
    }

    // Phase 2: escalate to SIGKILL across the entire cgroup and stop the transient scope unit.
    let _ = run_systemctl(&[
        "--user",
        "kill",
        "--kill-whom=all",
        "--signal=SIGKILL",
        unit,
    ])
    .await;
    let _ = run_systemctl(&["--user", "stop", unit]).await;

    let settle_deadline = Instant::now() + SCOPE_SETTLE_TIMEOUT;
    loop {
        let state = query_scope_state(unit).await;
        if scope_is_gone(&state, root_process) {
            return Ok(KillTreeReport {
                all_gone: true,
                survivors: Vec::new(),
            });
        }
        let now = Instant::now();
        if now >= settle_deadline {
            break;
        }
        let remaining = settle_deadline.saturating_duration_since(now);
        tokio::time::sleep(SCOPE_POLL_INTERVAL.min(remaining)).await;
    }

    let final_state = query_scope_state(unit).await;
    let mut survivors = final_state.live_pids();
    if procid::is_alive(root_process) && !survivors.iter().any(|id| id.pid == root_process.pid) {
        survivors.push(*root_process);
    }
    survivors.sort_by_key(|id| id.pid);
    Ok(KillTreeReport {
        all_gone: survivors.is_empty(),
        survivors,
    })
}

#[cfg(target_os = "linux")]
struct ScopeState {
    active_state: String,
    control_group: String,
}

#[cfg(target_os = "linux")]
impl ScopeState {
    fn is_active(&self) -> bool {
        matches!(
            self.active_state.as_str(),
            "active" | "activating" | "deactivating" | "reloading"
        )
    }

    fn live_pids(&self) -> Vec<ProcessId> {
        if self.control_group.is_empty() || !self.control_group.starts_with('/') {
            return Vec::new();
        }
        let procs_path = format!("/sys/fs/cgroup{}/cgroup.procs", self.control_group);
        let Ok(contents) = std::fs::read_to_string(&procs_path) else {
            return Vec::new();
        };
        let own_pid = std::process::id();
        let mut out = Vec::new();
        for line in contents.lines() {
            if let Ok(pid) = line.trim().parse::<u32>()
                && pid != own_pid
                && let Ok(Some(id)) = procid::of_pid(pid)
            {
                out.push(id);
            }
        }
        out
    }
}

#[cfg(target_os = "linux")]
fn scope_is_gone(state: &ScopeState, root_process: &ProcessId) -> bool {
    !state.is_active() && state.live_pids().is_empty() && !procid::is_alive(root_process)
}

#[cfg(target_os = "linux")]
async fn query_scope_state(unit: &str) -> ScopeState {
    let output = run_systemctl(&[
        "--user",
        "show",
        "--property=ActiveState",
        "--property=ControlGroup",
        unit,
    ])
    .await;

    let mut active_state = String::new();
    let mut control_group = String::new();
    if let Some(text) = output {
        for line in text.lines() {
            if let Some(val) = line.strip_prefix("ActiveState=") {
                active_state = val.trim().to_owned();
            } else if let Some(val) = line.strip_prefix("ControlGroup=") {
                control_group = val.trim().to_owned();
            }
        }
    }
    ScopeState {
        active_state,
        control_group,
    }
}

#[cfg(target_os = "linux")]
async fn run_systemctl(args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new("systemctl");
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(SYSTEMD_CMD_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn configure_common_cmd(
    cmd: &mut std::process::Command,
    resolved: &ResolvedCommand,
    options: &DetachOptions,
    stdout_stdio: Stdio,
    stderr_stdio: Stdio,
) {
    for key in &resolved.env_remove {
        cmd.env_remove(key);
    }
    for (key, value) in &options.env {
        cmd.env(key, value);
    }
    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
        .stdout(stdout_stdio)
        .stderr(stderr_stdio);
    // Ensure no caller file descriptors above stdio (fd >= 3, such as extra pipes or redirected
    // descriptors) survive into the detached process across either `systemd-run --scope` or
    // direct `setsid` launches.
    close_fds::install_cloexec_above_stdio(cmd);
}

#[allow(unsafe_code)]
mod close_fds {
    use std::os::unix::process::CommandExt;

    pub(super) fn install_cloexec_above_stdio(cmd: &mut std::process::Command) {
        // SAFETY: The closure runs in the forked child immediately before `exec`. Every call it
        // makes (`syscall(SYS_close_range)`, `open`, `syscall(SYS_getdents64)`, `fcntl`, `close`,
        // `getrlimit`) is async-signal-safe and performs zero heap allocations. Setting
        // `FD_CLOEXEC` (rather than closing immediately) preserves Rust's internal `O_CLOEXEC`
        // exec-error pipe while guaranteeing the kernel closes every fd >= 3 at `exec`.
        unsafe {
            cmd.pre_exec(|| {
                mark_fds_cloexec_from_3();
                Ok(())
            });
        }
    }

    #[cfg(target_os = "linux")]
    unsafe fn mark_fds_cloexec_from_3() {
        // Fast path (Linux >= 5.11): mark all open fds >= 3 close-on-exec in one kernel call.
        // SAFETY: `SYS_close_range` with `CLOSE_RANGE_CLOEXEC` is an async-signal-safe syscall
        // with no heap allocation.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if rc == 0 {
            return;
        }

        // Fallback when `close_range` is unavailable (older kernel or seccomp): walk
        // `/proc/self/fd` with raw `open` + `SYS_getdents64` into a fixed stack buffer.
        if unsafe { mark_proc_self_fd_cloexec() } {
            return;
        }

        // Last resort when `/proc` is not mounted.
        unsafe { mark_rlimit_cloexec() };
    }

    #[cfg(target_os = "freebsd")]
    unsafe fn mark_fds_cloexec_from_3() {
        // SAFETY: `close_range` is an async-signal-safe syscall with no heap allocation.
        if unsafe {
            libc::close_range(
                3,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC as libc::c_int,
            )
        } == 0
        {
            return;
        }
        unsafe { mark_rlimit_cloexec() };
    }

    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    unsafe fn mark_fds_cloexec_from_3() {
        // On macOS and other POSIX targets without `close_range`, avoid `opendir("/dev/fd")`
        // because `opendir` calls `malloc` (not async-signal-safe in `pre_exec`); sweep
        // `fcntl(fd, F_SETFD, FD_CLOEXEC)` across `3..RLIMIT_NOFILE` instead.
        unsafe { mark_rlimit_cloexec() };
    }

    #[cfg(target_os = "linux")]
    unsafe fn mark_proc_self_fd_cloexec() -> bool {
        // SAFETY: Static NUL-terminated path; raw `open` is async-signal-safe and allocates nothing.
        let dir_fd = unsafe {
            libc::open(
                c"/proc/self/fd".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if dir_fd < 0 {
            return false;
        }

        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `dir_fd` is a valid directory descriptor and `buf` is a stack-allocated
            // writable byte array; `SYS_getdents64` is async-signal-safe.
            let n =
                unsafe { libc::syscall(libc::SYS_getdents64, dir_fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            let len = n as usize;
            let mut offset = 0usize;
            while offset + 19 < len {
                // Stable Linux kernel `struct linux_dirent64` layout:
                // d_ino (8) + d_off (8) + d_reclen (2) + d_type (1) + d_name (NUL-terminated).
                let reclen = u16::from_ne_bytes([buf[offset + 16], buf[offset + 17]]) as usize;
                if reclen == 0 || offset + reclen > len {
                    break;
                }
                let name = &buf[offset + 19..offset + reclen];
                if let Some(fd) = parse_fd_name(name)
                    && fd >= 3
                    && fd != dir_fd
                {
                    // SAFETY: `fcntl(F_SETFD, FD_CLOEXEC)` is async-signal-safe; EBADF is ignored.
                    let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
                }
                offset += reclen;
            }
        }

        // SAFETY: `dir_fd` was opened above and is closed once.
        let _ = unsafe { libc::close(dir_fd) };
        true
    }

    unsafe fn mark_rlimit_cloexec() {
        const MAX_SWEEP: u64 = 65_536;
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `getrlimit` is async-signal-safe and writes into stack-allocated `limit`.
        let upper = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
            #[allow(clippy::unnecessary_cast)]
            let cur = limit.rlim_cur as u64;
            if cur == 0 || cur > MAX_SWEEP {
                MAX_SWEEP
            } else {
                cur
            }
        } else {
            1024
        };

        let mut fd: libc::c_int = 3;
        while (fd as u64) < upper {
            // SAFETY: `fcntl(F_SETFD, FD_CLOEXEC)` is async-signal-safe; invalid fds return EBADF.
            let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
            fd += 1;
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn parse_fd_name(bytes: &[u8]) -> Option<libc::c_int> {
        let mut acc: i64 = 0;
        let mut saw_digit = false;
        for &b in bytes {
            if b == 0 {
                break;
            }
            if !b.is_ascii_digit() {
                return None;
            }
            saw_digit = true;
            acc = acc.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
            if acc > i64::from(libc::c_int::MAX) {
                return None;
            }
        }
        if saw_digit {
            Some(acc as libc::c_int)
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
async fn systemd_user_scope_available(options: &DetachOptions) -> bool {
    let mut probe = tokio::process::Command::new("systemd-run");
    probe
        .args(SYSTEMD_USER_SCOPE_ARGS)
        .args(["--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    // Respect any session-bus environment overrides provided by the caller.
    for key in ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(val) = options.env.get(OsStr::new(key)) {
            probe.env(key, val);
        }
    }

    let Ok(mut child) = probe.spawn() else {
        return false;
    };

    match tokio::time::timeout(SYSTEMD_CMD_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = child.kill().await;
            false
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn systemd_user_scope_available(_options: &DetachOptions) -> bool {
    let _ = (Duration::ZERO, Instant::now());
    false
}

#[cfg(target_os = "linux")]
async fn systemd_linger_enabled() -> bool {
    let uid = nix::unistd::Uid::current();
    let username = nix::unistd::User::from_uid(uid)
        .ok()
        .flatten()
        .map(|u| u.name)
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok());

    if let Some(ref name) = username
        && super::linger_file_exists(Path::new("/var/lib/systemd/linger"), name)
    {
        return true;
    }

    let uid_str = uid.as_raw().to_string();
    let mut cmd = tokio::process::Command::new("loginctl");
    cmd.args(["show-user", &uid_str, "--property=Linger"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    if let Ok(Ok(output)) = tokio::time::timeout(SYSTEMD_CMD_TIMEOUT, cmd.output()).await
        && output.status.success()
        && let Some(enabled) =
            super::parse_loginctl_linger(&String::from_utf8_lossy(&output.stdout))
    {
        return enabled;
    }

    false
}

#[cfg(not(target_os = "linux"))]
async fn systemd_linger_enabled() -> bool {
    false
}

fn ensure_target_exists(program: &OsStr, options: &DetachOptions) -> Result<()> {
    let program_path = Path::new(program);
    let has_separator = program.to_string_lossy().contains('/');
    if has_separator {
        let candidate = if program_path.is_relative() {
            options
                .cwd
                .as_ref()
                .map(|cwd| cwd.join(program_path))
                .unwrap_or_else(|| program_path.to_path_buf())
        } else {
            program_path.to_path_buf()
        };
        if !candidate.exists() {
            return Err(OsError::ProgramNotFound {
                program: program.to_string_lossy().into_owned(),
            });
        }
        return Ok(());
    }

    let path_var: Option<OsString> = options
        .env
        .get(OsStr::new("PATH"))
        .cloned()
        .or_else(|| std::env::var_os("PATH"));
    let name = program.to_string_lossy();
    if find_executable_in_paths(&name, path_var.as_deref(), &[]).is_none() {
        return Err(OsError::ProgramNotFound {
            program: name.into_owned(),
        });
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn parse_fd_name_accepts_non_negative_integers_only() {
        assert_eq!(close_fds::parse_fd_name(b"0\0"), Some(0));
        assert_eq!(close_fds::parse_fd_name(b"3\0junk"), Some(3));
        assert_eq!(close_fds::parse_fd_name(b"1024\0"), Some(1024));
        assert_eq!(close_fds::parse_fd_name(b".\0"), None);
        assert_eq!(close_fds::parse_fd_name(b"..\0"), None);
        assert_eq!(close_fds::parse_fd_name(b"\0"), None);
        assert_eq!(close_fds::parse_fd_name(b"3a\0"), None);
        assert_eq!(close_fds::parse_fd_name(b"99999999999999999999\0"), None);
    }

    #[tokio::test]
    async fn confirm_scope_entry_reports_stderr_on_early_exit() {
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                "echo 'Failed to start transient scope unit: Unit exists.' >&2; exit 1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut capture = ScopeStderrCapture::Pipe(child.stderr.take());

        let err = confirm_scope_entry(
            &mut child,
            pid,
            "legion-job-never-entered-1.scope",
            &mut capture,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            matches!(err, OsError::Platform { .. }),
            "expected Platform error, got {err:?}"
        );
        assert!(
            msg.contains("Unit exists"),
            "error must include systemd-run stderr, got: {msg}"
        );
    }

    #[tokio::test]
    async fn confirm_scope_entry_reports_log_file_stderr_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("err.log");
        std::fs::write(&log_path, b"pre-existing-line\n").unwrap();
        let offset = std::fs::metadata(&log_path).unwrap().len();

        let log_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "echo 'bus disconnected' >&2; sleep 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log_file))
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut capture = ScopeStderrCapture::LogFile {
            path: &log_path,
            offset,
        };

        let err = confirm_scope_entry(
            &mut child,
            pid,
            "legion-job-never-entered-2.scope",
            &mut capture,
            Duration::from_millis(120),
        )
        .await
        .unwrap_err();

        let msg = err.to_string();
        assert!(matches!(err, OsError::TimedOut { .. }), "got {err:?}");
        assert!(msg.contains("bus disconnected"), "got: {msg}");
        assert!(
            !msg.contains("pre-existing-line"),
            "must not include log bytes written before spawn: {msg}"
        );
    }
}
