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

/// Per-stream capture cap for `logs` commands (8 MiB, enough for up to 2000 bounded JSON entries).
pub(crate) const LOGS_CAPTURE_LIMIT: usize = 8 * 1024 * 1024;

/// Maximum number of near-match service names included in a `NotFound` hint.
pub(crate) const MAX_SUGGESTIONS: usize = 5;

/// Deduplicates suggested service names in order and caps the list at [`MAX_SUGGESTIONS`].
pub(crate) fn dedup_suggestions(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for item in items {
        if item.is_empty() || out.iter().any(|existing| existing == &item) {
            continue;
        }
        out.push(item);
        if out.len() >= MAX_SUGGESTIONS {
            break;
        }
    }
    out
}

/// Formats a `NotFound` hint naming up to [`MAX_SUGGESTIONS`] near-match service names, or falls
/// back to suggesting `svc.list` when none match.
pub(crate) fn not_found_hint(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        return "Run `svc.list` to see the services on this machine.".to_owned();
    }
    let quoted: Vec<String> = suggestions
        .iter()
        .take(MAX_SUGGESTIONS)
        .map(|s| format!("`{s}`"))
        .collect();
    format!(
        "Did you mean {}? Pass the exact service name as shown by `svc.list`.",
        quoted.join(", ")
    )
}

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

    /// Returns `[program, args...]` as a single vector for diagnostics and sudoers hints.
    pub(crate) fn argv(&self) -> Vec<String> {
        let mut full = Vec::with_capacity(self.args.len() + 1);
        full.push(self.program.clone());
        full.extend(self.args.iter().cloned());
        full
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
    /// Backend-specific run identity (such as systemd `InvocationID` or monotonic start time, or
    /// launchd `runs` counter) used to prove a restart actually created a new instance.
    pub(crate) instance_id: Option<String>,
    /// Whether this service is a one-shot or on-demand job that ran to completion and exited
    /// cleanly (systemd `Type=oneshot` with `Result=success`, or launchd on-demand with
    /// `runs > 0` and exit code 0).
    pub(crate) oneshot_completed: bool,
    /// Human-readable description, when available.
    pub(crate) description: Option<String>,
    /// Last reported exit code, when available (wide enough for signed Unix signals and unsigned
    /// 32-bit Windows service exit codes).
    pub(crate) exit_code: Option<i64>,
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

    /// Suggests up to [`MAX_SUGGESTIONS`] existing service names in `scope` that closely match
    /// `name` after an exact lookup returned `NotFound`. Never acts on them.
    fn suggest_names<'a>(
        &'a self,
        _ctx: &'a Ctx,
        _name: &'a str,
        _scope: Scope,
    ) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
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

/// Builds the privilege-refusal error for Unix system-scope control actions, including the exact
/// `sudo -n` argv Legion would run in `details.argv`.
pub(crate) fn unix_privilege_denied(
    action: Action,
    name: &str,
    use_sudo: bool,
    cmd: &CmdSpec,
    stderr: &str,
) -> ErrorBody {
    let verb = action.as_str();
    let sudo_argv = if use_sudo {
        cmd.argv()
    } else {
        cmd.clone().with_sudo_n().argv()
    };
    let hint = if use_sudo {
        "Allow the command in `details.argv` with `NOPASSWD` in sudoers, or set `[svc] sudo = false` in node.toml if this node should not escalate privileges."
    } else {
        "Set `[svc] sudo = true` in node.toml and allow the command in `details.argv` with `NOPASSWD` in sudoers so legion can run system service changes through `sudo -n`. Enabling this lets any principal allowed svc.start, svc.stop or svc.restart control system services as root."
    };
    let mut details = serde_json::Map::new();
    details.insert("argv".to_owned(), json!(sudo_argv));
    let detail = stderr.trim();
    if !detail.is_empty() {
        details.insert("stderr".to_owned(), Value::String(detail.to_owned()));
    }
    ErrorBody::new(
        ErrorCode::Denied,
        format!("Permission denied while trying to {verb} system service {name:?}."),
    )
    .with_hint(hint)
    .with_details(Value::Object(details))
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
