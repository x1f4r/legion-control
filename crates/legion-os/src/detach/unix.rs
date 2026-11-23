//! Unix detached process launcher.
//!
//! On Linux, when a `systemd --user` manager is reachable, the target is started through
//! `systemd-run --user --scope --quiet --collect --` so it runs inside a transient user scope
//! rather than the caller's SSH session scope (which `systemd-logind` terminates on disconnect
//! under `KillUserProcesses=yes`). On macOS, containers, or Linux systems without a user manager,
//! the process starts directly in a new session (`setsid`). In both cases `ProcessSession`
//! detaches the process from any controlling terminal.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use process_wrap::std::{CommandWrap, ProcessSession};

use crate::procid::{self, ProcessId};
use crate::shell::{ResolvedCommand, find_executable_in_paths};
use crate::{OsError, Result};

use super::{DetachOptions, open_append_stdio};

#[cfg(target_os = "linux")]
const SYSTEMD_USER_SCOPE_ARGS: &[&str] = &["--user", "--scope", "--quiet", "--collect", "--"];

#[cfg(target_os = "linux")]
const SYSTEMD_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn spawn_detached(
    resolved: &ResolvedCommand,
    options: &DetachOptions,
) -> Result<ProcessId> {
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
    let mut wrapped = if use_systemd_scope {
        #[cfg(target_os = "linux")]
        {
            CommandWrap::with_new("systemd-run", |cmd| {
                cmd.args(SYSTEMD_USER_SCOPE_ARGS);
                cmd.arg(&resolved.program);
                cmd.args(&resolved.args);
                configure_common_cmd(cmd, resolved, options, stdout_stdio, stderr_stdio);
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            CommandWrap::with_new(&resolved.program, |cmd| {
                cmd.args(&resolved.args);
                configure_common_cmd(cmd, resolved, options, stdout_stdio, stderr_stdio);
            })
        }
    } else {
        CommandWrap::with_new(&resolved.program, |cmd| {
            cmd.args(&resolved.args);
            configure_common_cmd(cmd, resolved, options, stdout_stdio, stderr_stdio);
        })
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
    let identity = procid::of_pid_including_zombie(pid)?.ok_or_else(|| OsError::Platform {
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

    Ok(identity)
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
        .arg("true")
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

    match tokio::time::timeout(SYSTEMD_PROBE_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => status.success(),
        _ => {
            let _ = child.kill().await;
            false
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn systemd_user_scope_available(_options: &DetachOptions) -> bool {
    let _ = Duration::ZERO;
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
