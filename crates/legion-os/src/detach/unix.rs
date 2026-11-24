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

pub(super) async fn spawn_detached(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<DetachHandle> {
    let use_systemd_scope = systemd_user_scope_available(options).await;

    // `systemd-run` itself exists even when the target executable does not, so validate the
    // target executable up front to preserve the typed `ProgramNotFound` error.
    if use_systemd_scope {
        ensure_target_exists(&resolved.program, options)?;
    }

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
            let wrap = CommandWrap::with_new("systemd-run", |cmd| {
                cmd.args(SYSTEMD_USER_SCOPE_ARGS);
                cmd.arg(&unit_arg);
                cmd.arg("--");
                cmd.arg(&resolved.program);
                cmd.args(&resolved.args);
                configure_common_cmd(cmd, resolved, options, stdout_stdio, stderr_stdio);
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

    // Capture the kernel start timestamp BEFORE spawning the reaper thread so even a fast-exiting
    // detached process still has an unreaped entry in the process table.
    let process = procid::of_pid_including_zombie(pid)?.ok_or_else(|| OsError::Platform {
        detail: format!("could not read process identity for detached pid {pid}"),
    })?;

    // If this process stays alive (for example as a long-running server), reap the detached child
    // once it eventually exits so it does not linger as a zombie. If this process exits first,
    // init/systemd adopts and reaps the detached child while it continues running.
    let mut inner = child.into_inner();
    let _ = std::thread::Builder::new()
        .name(format!("legion-reap-{pid}"))
        .spawn(move || {
            let _ = inner.wait();
        });

    Ok(DetachHandle {
        process,
        systemd_unit,
        warnings: Vec::new(),
    })
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
