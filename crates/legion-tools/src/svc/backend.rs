//! Backend abstraction and bounded command execution for the `svc` tool family.
//!
//! Each operating system's service manager lives in its own module implementing [`Backend`].
//! All three compile on every platform with no `cfg` gates; [`backend_for_os`] chooses the right
//! one from `ctx.os()` at run time.

use std::collections::BTreeMap;
use std::time::Duration;

use jiff::Timestamp;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{BoxFuture, Ctx};
use legion_os::OsError;
use legion_os::run::{RunOptions, RunOutput, run};
use legion_os::shell::CommandSpec;
use serde_json::{Value, json};

use super::launchd::Launchd;
use super::systemd::Systemd;
use super::windows::Windows;
use super::{Action, BackendKind, Scope, ServiceState, ServiceSummary};

/// Default timeout for quick read-only service manager queries (`list`, `status`, `logs`).
pub(crate) const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Default timeout for a service state-change command (`start`, `stop`, `restart`).
pub(crate) const ACTION_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-stream capture cap for `list` and `status` commands (2 MiB).
pub(crate) const QUERY_CAPTURE_LIMIT: usize = 2 * 1024 * 1024;

/// Per-stream capture cap for `logs` commands (4 MiB).
pub(crate) const LOGS_CAPTURE_LIMIT: usize = 4 * 1024 * 1024;

/// A system command ready to run with exact argv (never a shell string) and explicit environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CmdSpec {
    /// Executable name or path.
    pub(crate) program: String,
    /// Exact arguments passed verbatim to the executable.
    pub(crate) args: Vec<String>,
    /// Extra environment variables applied alongside the forced `C` locale.
    pub(crate) env: Vec<(String, String)>,
}

impl CmdSpec {
    /// Creates a command with exact arguments and no extra environment variables.
    pub(crate) fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            env: Vec::new(),
        }
    }

    /// Adds one environment variable.
    pub(crate) fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// Prefixes this command with `sudo -n` so a password prompt fails immediately rather than
    /// hanging the node.
    pub(crate) fn with_sudo_n(self) -> Self {
        let mut args = Vec::with_capacity(self.args.len() + 2);
        args.push("-n".to_owned());
        args.push(self.program);
        args.extend(self.args);
        Self {
            program: "sudo".to_owned(),
            args,
            env: self.env,
        }
    }
}

/// Detailed status returned by a backend for a single service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackendStatus {
    /// Canonical service name reported by the service manager.
    pub(crate) name: String,
    /// Scope in which the service lives.
    pub(crate) scope: Scope,
    /// Normalised service state.
    pub(crate) state: ServiceState,
    /// Whether the service starts automatically, when known.
    pub(crate) enabled: Option<bool>,
    /// Main process id, when running and known.
    pub(crate) pid: Option<u32>,
    /// When the service entered its current active state, RFC 3339 UTC.
    pub(crate) since: Option<String>,
    /// Human-readable description, when available.
    pub(crate) description: Option<String>,
    /// Last reported exit code, when available.
    pub(crate) exit_code: Option<i32>,
    /// A few raw backend properties useful for diagnosis.
    pub(crate) detail: BTreeMap<String, Value>,
}

/// Raw log lines retrieved by a backend before caller-level line truncation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackendLogs {
    /// Log lines, oldest first.
    pub(crate) lines: Vec<String>,
    /// Whether older lines or stream bytes were omitted by the backend.
    pub(crate) truncated: bool,
}

/// What every platform service-manager backend implements.
pub(crate) trait Backend: Send + Sync {
    /// Which service manager this backend drives.
    fn kind(&self) -> BackendKind;

    /// Whether this platform has a per-user service scope (`false` on Windows).
    fn supports_user_scope(&self) -> bool {
        true
    }

    /// Lists services in one scope.
    fn list<'a>(
        &'a self,
        ctx: &'a Ctx,
        scope: Scope,
    ) -> BoxFuture<'a, Result<Vec<ServiceSummary>, ErrorBody>>;

    /// Reads the current status of one named service in `scope`.
    fn status<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
    ) -> BoxFuture<'a, Result<BackendStatus, ErrorBody>>;

    /// Issues `start`, `stop` or `restart` for `name` in `scope`.
    ///
    /// `use_sudo` is `true` only when `scope == Scope::System` and `[svc] sudo = true` in
    /// `node.toml`.
    fn control<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
        action: Action,
        use_sudo: bool,
    ) -> BoxFuture<'a, Result<(), ErrorBody>>;

    /// Reads the last `lines` log lines for `name` in `scope`, optionally starting at `since`.
    fn logs<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
        lines: usize,
        since: Option<Timestamp>,
    ) -> BoxFuture<'a, Result<BackendLogs, ErrorBody>>;
}

static SYSTEMD: Systemd = Systemd;
static LAUNCHD: Launchd = Launchd;
static WINDOWS: Windows = Windows;

/// Chooses the service-manager backend for `os` (`ctx.os()`).
pub(crate) fn backend_for_os(os: &str) -> Result<&'static dyn Backend, ErrorBody> {
    match os {
        "linux" => Ok(&SYSTEMD),
        "macos" => Ok(&LAUNCHD),
        "windows" => Ok(&WINDOWS),
        other => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            format!("Background service management is not supported on {other}."),
        )
        .with_hint("The svc tools support Linux (systemd), macOS (launchd) and Windows.")),
    }
}

/// Runs `cmd` through `legion_os::run` with exact argv, the `C` locale, a timeout and
/// cooperative cancellation.
///
/// The run is spawned onto the runtime with a child cancellation token whose drop guard fires if
/// the caller drops our future, so a cancelled call always stops the child process tree cleanly.
pub(crate) async fn run_cmd(
    ctx: &Ctx,
    cmd: &CmdSpec,
    timeout: Duration,
    capture_limit: usize,
) -> Result<RunOutput, ErrorBody> {
    let spec = CommandSpec::argv(&cmd.program, &cmd.args);
    let cancel = ctx.cancel_token().child_token();
    let _stop_on_drop = cancel.clone().drop_guard();

    let mut options = RunOptions::new(spec)
        .c_locale()
        .timeout(timeout)
        .capture_limit(capture_limit)
        .cancel(cancel);
    for (key, value) in &cmd.env {
        options = options.env(key, value);
    }

    let program = cmd.program.clone();
    let output = tokio::spawn(run(options))
        .await
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Running {program} stopped unexpectedly: {error}."),
            )
        })?
        .map_err(|error| map_os_error(error, &program))?;

    if output.cancelled {
        return Err(ErrorBody::new(
            ErrorCode::Cancelled,
            format!("The call was cancelled while running {program}."),
        ));
    }
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "{program} did not finish within {} seconds.",
                timeout.as_secs()
            ),
        )
        .with_hint("Check whether the service manager is responsive on this machine."));
    }
    Ok(output)
}

fn map_os_error(error: OsError, program: &str) -> ErrorBody {
    match error {
        OsError::ProgramNotFound { program: missing } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("The service manager tool `{missing}` is not installed on this machine."),
        )
        .with_hint(format!(
            "Install `{missing}` or ensure it is on PATH for the account legion runs as."
        ))
        .with_details(json!({"program": missing})),
        OsError::PermissionDenied { detail, .. } => ErrorBody::new(
            ErrorCode::Denied,
            format!("The system refused to execute `{program}`: {detail}."),
        )
        .with_hint("Check that the account legion runs as is allowed to run this program."),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not start `{program}`: {other}."),
        ),
    }
}

/// Builds the privilege-refusal error for Unix system-scope control actions.
pub(crate) fn unix_privilege_denied(
    action: Action,
    name: &str,
    use_sudo: bool,
    stderr: &str,
) -> ErrorBody {
    let verb = action.as_str();
    let hint = if use_sudo {
        "Allow non-interactive sudo (`NOPASSWD`) for this command in sudoers, or set `[svc] sudo = false` in node.toml if this node should not escalate privileges."
    } else {
        "Set `[svc] sudo = true` in node.toml and configure non-interactive sudoers rules (`NOPASSWD`) to let legion run system service changes through `sudo -n`. Enabling this lets any principal allowed svc.start, svc.stop or svc.restart control system services as root."
    };
    let mut err = ErrorBody::new(
        ErrorCode::Denied,
        format!("Permission denied while trying to {verb} system service {name:?}."),
    )
    .with_hint(hint);
    let detail = stderr.trim();
    if !detail.is_empty() {
        err = err.with_details(json!({"stderr": detail}));
    }
    err
}

/// Whether `stderr` indicates a non-interactive `sudo -n` refusal.
pub(crate) fn is_sudo_denied(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("a password is required")
        || lower.contains("password is required")
        || lower.contains("not in the sudoers file")
        || lower.contains("not allowed to execute")
        || lower.contains("sorry, user")
        || lower.contains("no tty present")
}
