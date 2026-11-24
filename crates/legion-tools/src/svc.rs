//! `svc`: inspecting and controlling background services across Linux (systemd), macOS (launchd)
//! and Windows (the Service Control Manager).
//!
//! All three platform backends compile on every target with no `cfg` gates and are selected from
//! `ctx.os()` at run time. Each backend runs the operating system's own service tools through
//! `legion-os` with exact argv (never a shell string), a timeout and the `C` locale.

mod backend;
mod launchd;
mod systemd;
mod windows;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use legion_core::config::NodeFile;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Ctx, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::backend::{Backend, BackendStatus, backend_for_os};

/// Default and maximum number of services returned by `svc.list`.
const LIST_LIMIT: (u32, u32) = (100, 1000);

/// Default and maximum number of log lines returned by `svc.logs`.
const LOGS_LIMIT: (u32, u32) = (100, 2000);

/// Maximum allowed length of a service name in bytes.
const MAX_NAME_LEN: usize = 256;

/// Maximum UTF-8 bytes kept for a single log line before cutting and marking it.
const MAX_LOG_LINE_BYTES: usize = 2048;

/// Marker appended to any log line that exceeded [`MAX_LOG_LINE_BYTES`].
const LOG_LINE_CUT_MARKER: &str = " ... [truncated]";

/// Number of trailing log lines attached when a start/stop/restart leaves the service in an
/// unexpected state.
const FAILURE_LOG_LINES: usize = 20;

/// How long `svc.start`, `svc.stop` and `svc.restart` wait for the service to reach its expected
/// state after the control command finishes, and how long they wait to settle after first seeing
/// it so an immediate crash is not reported as running.
const DEFAULT_WAIT_POLICY: WaitPolicy = WaitPolicy {
    timeout: Duration::from_secs(10),
    poll_interval: Duration::from_millis(200),
    settle: Duration::from_secs(1),
};

/// Adds the `svc` tools.
pub fn register(registry: &mut Registry) {
    registry.add_fn(
        "svc.list",
        Effect::Read,
        "List background services managed by systemd (Linux), launchd (macOS) or the Windows Service Control Manager. `scope` selects \"user\" (per-user services), \"system\" (machine-wide services; the only scope on Windows) or \"all\" (default). Read-only and never escalates privileges.",
        list,
    );
    registry.add_fn(
        "svc.status",
        Effect::Read,
        "Read the current state, enabled flag, PID, start time, exit code and diagnostic properties of one background service. When `scope` is omitted, checks \"user\" first and then \"system\", returning which matched or `conflict` if the name exists in both. Read-only and never escalates privileges.",
        status,
    );
    registry.add_fn(
        "svc.start",
        Effect::Exec,
        "Start a background service and read its state back to confirm it is running. When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Changing a system service requires privileges: on Unix, legion never escalates unless `[svc] sudo = true` is set in node.toml (running `sudo -n`); on Windows the node must run elevated.",
        start,
    );
    registry.add_fn(
        "svc.stop",
        Effect::Exec,
        "Stop a background service and read its state back to confirm it is stopped. When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Changing a system service requires privileges: on Unix, legion never escalates unless `[svc] sudo = true` is set in node.toml (running `sudo -n`); on Windows the node must run elevated.",
        stop,
    );
    registry.add_fn(
        "svc.restart",
        Effect::Exec,
        "Restart a background service and read its state back to confirm it is running. When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Changing a system service requires privileges: on Unix, legion never escalates unless `[svc] sudo = true` is set in node.toml (running `sudo -n`); on Windows the node must run elevated.",
        restart,
    );
    registry.add_fn(
        "svc.logs",
        Effect::Read,
        "Read recent log lines for a background service (journald on Linux, declared plist log files on macOS, or Windows Event Log). When `scope` is omitted, checks \"user\" first and then \"system\" (`conflict` if in both). Read-only and never escalates privileges; long lines are cut and marked.",
        logs,
    );
}

/// Which service manager drives a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Linux systemd (`systemctl` / `journalctl`).
    Systemd,
    /// macOS launchd (`launchctl`).
    Launchd,
    /// Windows Service Control Manager (PowerShell `Get-Service` / `Win32_Service`).
    Windows,
}

/// Whether a service belongs to the current user or to the whole machine.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Per-user service manager (`systemctl --user` on Linux, `gui/<uid>` on macOS).
    User,
    /// Machine-wide service manager (the only scope on Windows).
    System,
}

impl Scope {
    /// The scope as written in JSON and messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::System => "system",
        }
    }
}

/// Scope selector accepted by `svc.list`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum ListScope {
    /// Only per-user services.
    User,
    /// Only machine-wide services.
    System,
    /// Both user and system services (where both exist).
    #[default]
    All,
}

/// Normalised state reported across all three service managers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    /// The service is active and running.
    Running,
    /// The service is inactive / stopped.
    Stopped,
    /// The service failed or exited with an error.
    Failed,
    /// Any transitional or platform-specific state (such as starting, stopping, reloading or
    /// paused).
    Other,
}

impl ServiceState {
    /// The state as written in JSON and messages.
    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Running => "running",
            ServiceState::Stopped => "stopped",
            ServiceState::Failed => "failed",
            ServiceState::Other => "other",
        }
    }
}

/// State filter accepted by `svc.list`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum StateFilter {
    /// Keep only running services.
    Running,
    /// Keep only stopped services.
    Stopped,
    /// Keep only failed services.
    Failed,
}

impl StateFilter {
    fn matches(self, state: ServiceState) -> bool {
        match self {
            StateFilter::Running => state == ServiceState::Running,
            StateFilter::Stopped => state == ServiceState::Stopped,
            StateFilter::Failed => state == ServiceState::Failed,
        }
    }
}

/// Whether a validated service name is used for a read-only query or a control action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NamePurpose {
    /// `svc.status` and `svc.logs`.
    Read,
    /// `svc.start`, `svc.stop` and `svc.restart`.
    Control,
}

/// Systemd unit suffixes known to `systemctl`.
const SYSTEMD_UNIT_SUFFIXES: [&str; 11] = [
    ".service",
    ".socket",
    ".device",
    ".mount",
    ".automount",
    ".swap",
    ".target",
    ".path",
    ".timer",
    ".slice",
    ".scope",
];

/// Read-only systemd unit suffixes allowed by `svc.status` and `svc.logs`.
const SYSTEMD_READONLY_SUFFIXES: [&str; 3] = [".timer", ".socket", ".path"];

/// Control verb issued by `svc.start`, `svc.stop` and `svc.restart`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Action::Start => "start",
            Action::Stop => "stop",
            Action::Restart => "restart",
        }
    }

    pub(crate) fn expected_state(self) -> ServiceState {
        match self {
            Action::Start | Action::Restart => ServiceState::Running,
            Action::Stop => ServiceState::Stopped,
        }
    }
}

/// Arguments of `svc.list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Which scope to list: `"user"`, `"system"`, or `"all"` (default `"all"`). On Windows only
    /// `"system"` exists.
    #[serde(default)]
    scope: ListScope,
    /// Keep only services whose name or description contains this substring (case-insensitive).
    #[serde(default)]
    filter: Option<String>,
    /// Keep only services in this state: `"running"`, `"stopped"` or `"failed"`.
    #[serde(default)]
    state: Option<StateFilter>,
    /// Maximum number of services to return, from 1 to 1000 (default 100).
    #[serde(default = "default_list_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u32,
}

fn default_list_limit() -> u32 {
    LIST_LIMIT.0
}

/// One service entry returned by `svc.list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ServiceSummary {
    /// Service name as understood by the platform service manager.
    pub name: String,
    /// Whether this service belongs to the user or system scope.
    pub scope: Scope,
    /// Normalised service state.
    pub state: ServiceState,
    /// Whether the service starts automatically, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Short description of the service, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// What `svc.list` returns.
#[derive(Debug, Serialize)]
pub struct ListResult {
    /// Which service manager answered.
    pub backend: BackendKind,
    /// Matching services, sorted by name then scope.
    pub services: Vec<ServiceSummary>,
    /// Whether more matching services existed beyond `limit`.
    pub truncated: bool,
}

/// Arguments of `svc.status`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    /// Service name (for example `nginx.service`, `com.example.worker` or `Spooler`).
    name: String,
    /// `"user"` or `"system"`. When omitted, checks `"user"` first and then `"system"`.
    #[serde(default)]
    scope: Option<Scope>,
}

/// What `svc.status` returns.
#[derive(Debug, Serialize)]
pub struct StatusResult {
    /// Canonical service name reported by the service manager.
    pub name: String,
    /// Which scope matched.
    pub scope: Scope,
    /// Which service manager answered.
    pub backend: BackendKind,
    /// Current normalised state.
    pub state: ServiceState,
    /// Whether the service starts automatically, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Main process id, when running and known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// When the service became active, RFC 3339 UTC, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Human-readable description, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Last exit code reported by the service manager, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// A few raw backend properties useful for diagnosis.
    pub detail: BTreeMap<String, Value>,
}

/// Arguments of `svc.start`, `svc.stop` and `svc.restart`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ActionArgs {
    /// Service name to control.
    name: String,
    /// `"user"` or `"system"`. When omitted, checks `"user"` first and then `"system"`.
    #[serde(default)]
    scope: Option<Scope>,
}

/// What `svc.start`, `svc.stop` and `svc.restart` return.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct ActionResult {
    /// Canonical service name reported by the service manager.
    pub name: String,
    /// Which scope was controlled.
    pub scope: Scope,
    /// Observed state after the command finished.
    pub state: ServiceState,
    /// Observed state before the command ran.
    pub previous_state: ServiceState,
}

/// Start of a log window: an RFC 3339 timestamp or a positive duration in seconds.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
enum SinceArg {
    /// Duration in seconds before now, such as `300` for the last five minutes.
    Seconds(f64),
    /// RFC 3339 timestamp, such as `2026-11-23T22:00:00Z`.
    Timestamp(String),
}

/// Arguments of `svc.logs`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LogsArgs {
    /// Service name to read logs for.
    name: String,
    /// `"user"` or `"system"`. When omitted, checks `"user"` first and then `"system"`.
    #[serde(default)]
    scope: Option<Scope>,
    /// Maximum number of lines to return, from 1 to 2000 (default 100).
    #[serde(default = "default_logs_limit")]
    #[schemars(range(min = 1, max = 2000))]
    lines: u32,
    /// Start of the log window: an RFC 3339 timestamp (such as `2026-11-23T22:00:00Z`) or a
    /// positive duration in seconds (such as `300`).
    #[serde(default)]
    since: Option<SinceArg>,
}

fn default_logs_limit() -> u32 {
    LOGS_LIMIT.0
}

/// What `svc.logs` returns.
#[derive(Debug, Serialize)]
pub struct LogsResult {
    /// Canonical service name reported by the service manager.
    pub name: String,
    /// Which scope matched.
    pub scope: Scope,
    /// Log lines, oldest first. Lines longer than 2048 bytes are cut and marked.
    pub lines: Vec<String>,
    /// Whether older lines or long line tails were omitted.
    pub truncated: bool,
}

/// Polling budget used after a control action to confirm the service reached its expected state.
#[derive(Clone, Copy, Debug)]
struct WaitPolicy {
    timeout: Duration,
    poll_interval: Duration,
    settle: Duration,
}

/// Snapshot of a running service's process/start identity before a `restart` call.
#[derive(Clone, Debug)]
struct StartIdentity {
    pid: Option<u32>,
    instance_id: Option<String>,
    since: Option<String>,
}

impl StartIdentity {
    fn from_status(status: &BackendStatus) -> Self {
        Self {
            pid: status.pid,
            instance_id: status.instance_id.clone(),
            since: status.since.clone(),
        }
    }

    fn has_identity(&self) -> bool {
        self.instance_id.is_some() || self.pid.is_some() || self.since.is_some()
    }

    /// Returns `true` when `after` proves a new instance started compared to `self`.
    fn changed(&self, after: &BackendStatus) -> bool {
        if !self.has_identity() {
            return true;
        }
        if let (Some(a), Some(b)) = (&self.instance_id, &after.instance_id) {
            return a != b;
        }
        if let (Some(a), Some(b)) = (self.pid, after.pid)
            && a != b
        {
            return true;
        }
        if let (Some(a), Some(b)) = (&self.since, &after.since)
            && a != b
        {
            return true;
        }
        false
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<ListResult, ToolError> {
    if !(1..=LIST_LIMIT.1).contains(&args.limit) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "svc.list: `limit` must be between 1 and {}, not {}.",
                LIST_LIMIT.1, args.limit
            ),
        )
        .with_hint("Use `filter` or `state` to narrow the list."));
    }
    let backend = backend_for_os(ctx.os())?;
    let mut services = match args.scope {
        ListScope::User => backend.list(ctx, Scope::User).await?,
        ListScope::System => backend.list(ctx, Scope::System).await?,
        ListScope::All => {
            if !backend.supports_user_scope() {
                backend.list(ctx, Scope::System).await?
            } else {
                let user = backend.list(ctx, Scope::User).await;
                let system = backend.list(ctx, Scope::System).await;
                match (user, system) {
                    (Ok(mut u), Ok(s)) => {
                        u.extend(s);
                        u
                    }
                    (Err(_), Ok(s)) => s,
                    (Ok(u), Err(_)) => u,
                    (Err(u_err), Err(_)) => return Err(u_err),
                }
            }
        }
    };

    if let Some(needle) = args.filter.as_deref().filter(|s| !s.is_empty()) {
        let lower = needle.to_lowercase();
        services.retain(|svc| {
            svc.name.to_lowercase().contains(&lower)
                || svc
                    .description
                    .as_deref()
                    .is_some_and(|desc| desc.to_lowercase().contains(&lower))
        });
    }
    if let Some(state_filter) = args.state {
        services.retain(|svc| state_filter.matches(svc.state));
    }

    services.sort_by(|a, b| a.name.cmp(&b.name).then(a.scope.cmp(&b.scope)));
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let truncated = services.len() > limit;
    services.truncate(limit);

    Ok(ListResult {
        backend: backend.kind(),
        services,
        truncated,
    })
}

async fn status(ctx: &Ctx, args: StatusArgs) -> Result<StatusResult, ToolError> {
    let backend = backend_for_os(ctx.os())?;
    let name = prepare_name(&args.name, backend.kind(), NamePurpose::Read)?;
    let found = resolve_scope(backend, ctx, &name, args.scope).await?;
    Ok(StatusResult {
        name: found.name,
        scope: found.scope,
        backend: backend.kind(),
        state: found.state,
        enabled: found.enabled,
        pid: found.pid,
        since: found.since,
        description: found.description,
        exit_code: found.exit_code,
        detail: found.detail,
    })
}

async fn start(ctx: &Ctx, args: ActionArgs) -> Result<ActionResult, ToolError> {
    run_action(ctx, args, Action::Start).await
}

async fn stop(ctx: &Ctx, args: ActionArgs) -> Result<ActionResult, ToolError> {
    run_action(ctx, args, Action::Stop).await
}

async fn restart(ctx: &Ctx, args: ActionArgs) -> Result<ActionResult, ToolError> {
    run_action(ctx, args, Action::Restart).await
}

async fn run_action(
    ctx: &Ctx,
    args: ActionArgs,
    action: Action,
) -> Result<ActionResult, ToolError> {
    let backend = backend_for_os(ctx.os())?;
    let name = prepare_name(&args.name, backend.kind(), NamePurpose::Control)?;
    let node_cfg = ctx.config().get::<NodeFile>()?;
    execute_action(
        backend,
        ctx,
        &name,
        args.scope,
        action,
        node_cfg.svc.sudo,
        DEFAULT_WAIT_POLICY,
    )
    .await
}

async fn execute_action(
    backend: &dyn Backend,
    ctx: &Ctx,
    name: &str,
    requested_scope: Option<Scope>,
    action: Action,
    sudo_enabled: bool,
    wait: WaitPolicy,
) -> Result<ActionResult, ToolError> {
    let before = resolve_scope(backend, ctx, name, requested_scope).await?;
    let scope = before.scope;
    let canonical = before.name.clone();
    let previous_state = before.state;
    let before_identity = StartIdentity::from_status(&before);
    let need_new_instance = action == Action::Restart
        && previous_state == ServiceState::Running
        && before_identity.has_identity();
    let use_sudo = scope == Scope::System && sudo_enabled;

    if let Err(mut error) = backend
        .control(ctx, &canonical, scope, action, use_sudo)
        .await
    {
        if error.code == ErrorCode::Failed {
            attach_failure_logs(backend, ctx, &canonical, scope, &mut error).await;
        }
        return Err(error);
    }

    let expected = action.expected_state();
    let deadline = Instant::now() + wait.timeout;
    let mut saw_unchanged_running = false;
    loop {
        if ctx.is_cancelled() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                format!(
                    "The call to svc.{} was cancelled while waiting for {canonical:?}.",
                    action.as_str()
                ),
            ));
        }

        let last_state = match backend.status(ctx, &canonical, scope).await {
            Ok(now) => {
                if now.state == expected {
                    if expected == ServiceState::Running {
                        if need_new_instance && !before_identity.changed(&now) {
                            saw_unchanged_running = true;
                        } else {
                            // Wait a short settle period and re-read so a service that crashes
                            // right after starting is not falsely reported as running.
                            if !wait.settle.is_zero() {
                                tokio::select! {
                                    biased;
                                    () = ctx.cancelled() => {
                                        return Err(ErrorBody::new(
                                            ErrorCode::Cancelled,
                                            format!(
                                                "The call to svc.{} was cancelled while confirming {canonical:?} stayed running.",
                                                action.as_str()
                                            ),
                                        ));
                                    }
                                    () = tokio::time::sleep(wait.settle) => {}
                                }
                            }
                            let settled = backend.status(ctx, &canonical, scope).await?;
                            if settled.state == ServiceState::Running
                                && (!need_new_instance || before_identity.changed(&settled))
                            {
                                return Ok(ActionResult {
                                    name: settled.name,
                                    scope,
                                    state: settled.state,
                                    previous_state,
                                });
                            }
                            let mut error = ErrorBody::new(
                                ErrorCode::Failed,
                                format!(
                                    "Service {canonical:?} in the {} scope started but transitioned to `{}` within {} ms after {}.",
                                    scope.as_str(),
                                    settled.state.as_str(),
                                    wait.settle.as_millis(),
                                    action.as_str()
                                ),
                            )
                            .with_hint(
                                "The service exited shortly after starting; inspect the attached logs or call `svc.logs`.",
                            )
                            .with_details(json!({
                                "name": canonical,
                                "scope": scope,
                                "state": settled.state,
                                "previous_state": previous_state,
                                "exit_code": settled.exit_code,
                            }));
                            attach_failure_logs(backend, ctx, &canonical, scope, &mut error).await;
                            return Err(error);
                        }
                    } else {
                        return Ok(ActionResult {
                            name: now.name,
                            scope,
                            state: now.state,
                            previous_state,
                        });
                    }
                }
                // For `svc.stop`, a failed unit with no running process is already not running;
                // report its true state (`failed`) instead of waiting for it to become `stopped`.
                if expected == ServiceState::Stopped
                    && now.state == ServiceState::Failed
                    && now.pid.is_none()
                {
                    return Ok(ActionResult {
                        name: now.name,
                        scope,
                        state: ServiceState::Failed,
                        previous_state,
                    });
                }
                // Stop waiting immediately if start/restart transitioned to failed.
                if expected == ServiceState::Running && now.state == ServiceState::Failed {
                    let mut error = ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Service {canonical:?} in the {} scope entered the `failed` state after {}.",
                            scope.as_str(),
                            action.as_str()
                        ),
                    )
                    .with_hint("Inspect the attached logs or call `svc.logs` for details.")
                    .with_details(json!({
                        "name": canonical,
                        "scope": scope,
                        "state": now.state,
                        "previous_state": previous_state,
                    }));
                    attach_failure_logs(backend, ctx, &canonical, scope, &mut error).await;
                    return Err(error);
                }
                now.state
            }
            // Stopping a transient systemd unit or booting out a launchd job removes it from the
            // active manager; once we already verified it existed, NotFound means it is stopped.
            Err(err) if err.code == ErrorCode::NotFound && expected == ServiceState::Stopped => {
                return Ok(ActionResult {
                    name: canonical,
                    scope,
                    state: ServiceState::Stopped,
                    previous_state,
                });
            }
            Err(err) => return Err(err),
        };

        if Instant::now() >= deadline {
            let message = if saw_unchanged_running {
                format!(
                    "Service {canonical:?} in the {} scope stayed running with the same process/start identity after restart.",
                    scope.as_str()
                )
            } else {
                format!(
                    "Service {canonical:?} in the {} scope did not reach `{}` within {} seconds after {} (still `{}`).",
                    scope.as_str(),
                    expected.as_str(),
                    wait.timeout.as_secs(),
                    action.as_str(),
                    last_state.as_str()
                )
            };
            let mut error = ErrorBody::new(ErrorCode::Failed, message)
                .with_hint(
                    "Check `svc.status` and `svc.logs` to see why the service did not finish transitioning.",
                )
                .with_details(json!({
                    "name": canonical,
                    "scope": scope,
                    "state": last_state,
                    "previous_state": previous_state,
                }));
            attach_failure_logs(backend, ctx, &canonical, scope, &mut error).await;
            return Err(error);
        }

        tokio::select! {
            biased;
            () = ctx.cancelled() => {
                return Err(ErrorBody::new(
                    ErrorCode::Cancelled,
                    format!("The call to svc.{} was cancelled while waiting for {canonical:?}.", action.as_str()),
                ));
            }
            () = tokio::time::sleep(wait.poll_interval) => {}
        }
    }
}

async fn attach_failure_logs(
    backend: &dyn Backend,
    ctx: &Ctx,
    name: &str,
    scope: Scope,
    error: &mut ErrorBody,
) {
    if let Ok(log_out) = backend
        .logs(ctx, name, scope, FAILURE_LOG_LINES, None)
        .await
    {
        let (lines, _) = format_log_lines(log_out.lines);
        if !lines.is_empty() {
            let mut map = match error.details.take() {
                Some(Value::Object(m)) => m,
                Some(other) => {
                    let mut m = serde_json::Map::new();
                    m.insert("detail".to_owned(), other);
                    m
                }
                None => serde_json::Map::new(),
            };
            map.insert("logs".to_owned(), json!(lines));
            error.details = Some(Value::Object(map));
        }
    }
}

async fn logs(ctx: &Ctx, args: LogsArgs) -> Result<LogsResult, ToolError> {
    if !(1..=LOGS_LIMIT.1).contains(&args.lines) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "svc.logs: `lines` must be between 1 and {}, not {}.",
                LOGS_LIMIT.1, args.lines
            ),
        )
        .with_hint("Use `since` to narrow the time window instead of asking for more lines."));
    }
    let backend = backend_for_os(ctx.os())?;
    let name = prepare_name(&args.name, backend.kind(), NamePurpose::Read)?;
    let since = args
        .since
        .map(|raw| parse_since(raw, Timestamp::now()))
        .transpose()?;
    let found = resolve_scope(backend, ctx, &name, args.scope).await?;
    let limit = usize::try_from(args.lines).unwrap_or(usize::MAX);
    let raw = backend
        .logs(ctx, &found.name, found.scope, limit, since)
        .await?;
    let (lines, line_cut) = format_log_lines(raw.lines);
    Ok(LogsResult {
        name: found.name,
        scope: found.scope,
        lines,
        truncated: raw.truncated || line_cut,
    })
}

/// Resolves which scope a named service lives in and returns its current status.
///
/// When `requested` is `None` on platforms with both user and system scopes, probes `user` first
/// and then `system`; returns `Conflict` if the service exists in both, `NotFound` if in neither.
async fn resolve_scope(
    backend: &dyn Backend,
    ctx: &Ctx,
    name: &str,
    requested: Option<Scope>,
) -> Result<BackendStatus, ErrorBody> {
    match requested {
        Some(Scope::User) if !backend.supports_user_scope() => Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "This operating system only has system-scope services.",
        )
        .with_hint("Omit `scope` or pass `scope: \"system\"`.")),
        Some(scope) => backend.status(ctx, name, scope).await,
        None if !backend.supports_user_scope() => backend.status(ctx, name, Scope::System).await,
        None => {
            let user = backend.status(ctx, name, Scope::User).await;
            let system = backend.status(ctx, name, Scope::System).await;
            match (user, system) {
                (Ok(_), Ok(_)) => Err(ErrorBody::new(
                    ErrorCode::Conflict,
                    format!("Service {name:?} exists in both the user and system scopes."),
                )
                .with_hint("Pass `scope: \"user\"` or `scope: \"system\"` to choose which one.")
                .with_details(json!({"name": name, "scopes": ["user", "system"]}))),
                (Ok(u), Err(_)) => Ok(u),
                (Err(_), Ok(s)) => Ok(s),
                (Err(u_err), Err(s_err)) => {
                    if s_err.code == ErrorCode::NotFound
                        && (u_err.code == ErrorCode::NotFound
                            || u_err.message.contains("user manager"))
                    {
                        Err(ErrorBody::new(
                            ErrorCode::NotFound,
                            format!("Service {name:?} was not found in the user or system scope."),
                        )
                        .with_hint("Run `svc.list` to see the services on this machine."))
                    } else if u_err.code == ErrorCode::NotFound {
                        Err(s_err)
                    } else {
                        Err(u_err)
                    }
                }
            }
        }
    }
}

/// Validates and normalises a service name before it is passed to any system command.
///
/// Rejects empty or whitespace-only names, names longer than 256 bytes, leading `-`, control
/// characters, wildcard characters (`*`, `?`, `[`, `]`), and path separators (`/` everywhere; `\`
/// everywhere except valid systemd `\xHH` unit-name hex escapes on Linux).
///
/// On systemd, bare names without a known unit suffix get `.service`; control operations allow
/// only `.service` units, while read-only operations (`svc.status`, `svc.logs`) also allow
/// `.timer`, `.socket` and `.path` units. Targets, slices, scopes, mounts and other unit types
/// are always refused so a service call cannot trigger `exit.target` or `reboot.target`.
pub(crate) fn prepare_name(
    name: &str,
    backend: BackendKind,
    purpose: NamePurpose,
) -> Result<String, ErrorBody> {
    let invalid = |reason: &str| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Invalid service name {name:?}: {reason}."),
        )
        .with_hint("Give the exact service name as shown by `svc.list`, with no wildcards, leading `-`, path separators or control characters.")
    };

    if name.is_empty() || name.trim().is_empty() {
        return Err(invalid("service name must not be empty"));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(invalid(&format!(
            "service name must be at most {MAX_NAME_LEN} bytes"
        )));
    }
    if name.starts_with('-') {
        return Err(invalid("service name must not start with `-`"));
    }
    if name.chars().any(char::is_control) {
        return Err(invalid("service name must not contain control characters"));
    }
    if name.contains(['*', '?', '[', ']']) {
        return Err(invalid(
            "service name must not contain wildcard characters (`*`, `?`, `[`, `]`)",
        ));
    }
    if name.contains('/') {
        return Err(invalid("service name must not contain path separators"));
    }
    if name.contains('\\') {
        let allowed_systemd_escape =
            backend == BackendKind::Systemd && !has_non_escape_backslash(name);
        if !allowed_systemd_escape {
            return Err(invalid("service name must not contain path separators"));
        }
    }

    if backend == BackendKind::Systemd {
        let matched_suffix = SYSTEMD_UNIT_SUFFIXES
            .iter()
            .copied()
            .find(|suffix| name.ends_with(suffix));
        return match matched_suffix {
            Some(".service") => Ok(name.to_owned()),
            Some(suffix) if SYSTEMD_READONLY_SUFFIXES.contains(&suffix) => match purpose {
                NamePurpose::Read => Ok(name.to_owned()),
                NamePurpose::Control => Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!(
                        "This tool controls services (`.service` units), not `{suffix}` unit {name:?}."
                    ),
                )
                .with_hint(
                    "Pass a `.service` unit name; `.timer`, `.socket` and `.path` units are only accepted by `svc.status` and `svc.logs`.",
                )),
            },
            Some(suffix) => Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "This tool only handles services (`.service` units), not `{suffix}` unit {name:?}."
                ),
            )
            .with_hint(
                "Pass a `.service` unit name (or `.timer`, `.socket`, `.path` for read-only `svc.status` and `svc.logs`).",
            )),
            None => {
                let normalized = format!("{name}.service");
                if normalized.len() > MAX_NAME_LEN {
                    return Err(invalid(&format!(
                        "service name must be at most {MAX_NAME_LEN} bytes including `.service`"
                    )));
                }
                Ok(normalized)
            }
        };
    }

    Ok(name.to_owned())
}

/// Returns `true` if `name` contains a `\` that is not a systemd `\xHH` two-digit hex escape.
fn has_non_escape_backslash(name: &str) -> bool {
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            let valid = i + 3 < bytes.len()
                && bytes[i + 1] == b'x'
                && bytes[i + 2].is_ascii_hexdigit()
                && bytes[i + 3].is_ascii_hexdigit();
            if !valid {
                return true;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    false
}

/// Parses `svc.logs`'s `since` argument into a UTC [`Timestamp`].
fn parse_since(arg: SinceArg, now: Timestamp) -> Result<Timestamp, ErrorBody> {
    match arg {
        SinceArg::Seconds(secs) => seconds_ago(secs, now),
        SinceArg::Timestamp(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    "svc.logs: `since` must not be empty.",
                )
                .with_hint(
                    "Pass an RFC 3339 timestamp such as `2026-11-23T22:00:00Z` or a positive number of seconds such as `300`.",
                ));
            }
            if let Ok(secs) = trimmed.parse::<f64>() {
                return seconds_ago(secs, now);
            }
            trimmed.parse::<Timestamp>().map_err(|_| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("svc.logs: `since` value {raw:?} is not a valid RFC 3339 timestamp or duration in seconds."),
                )
                .with_hint("Use an RFC 3339 timestamp such as `2026-11-23T22:00:00Z` or a positive number of seconds such as `300`.")
            })
        }
    }
}

fn seconds_ago(secs: f64, now: Timestamp) -> Result<Timestamp, ErrorBody> {
    const MAX_DURATION_SECS: f64 = 10.0 * 365.25 * 86_400.0;
    if !(secs > 0.0 && secs.is_finite() && secs <= MAX_DURATION_SECS) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("svc.logs: `since` duration must be a positive finite number of seconds, not {secs}."),
        )
        .with_hint("Pass a positive number of seconds such as `300`, or an RFC 3339 timestamp."));
    }
    let whole = secs.round() as i64;
    Timestamp::from_second(now.as_second().saturating_sub(whole)).map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "svc.logs: `since` duration is out of range.",
        )
    })
}

/// Cuts any log line longer than [`MAX_LOG_LINE_BYTES`] on a UTF-8 boundary and marks it.
pub(crate) fn format_log_lines(lines: Vec<String>) -> (Vec<String>, bool) {
    let mut any_cut = false;
    let out = lines
        .into_iter()
        .map(|line| {
            if line.len() <= MAX_LOG_LINE_BYTES {
                line
            } else {
                any_cut = true;
                let mut cut = MAX_LOG_LINE_BYTES;
                while cut > 0 && !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                let mut shortened = line[..cut].to_owned();
                shortened.push_str(LOG_LINE_CUT_MARKER);
                shortened
            }
        })
        .collect();
    (out, any_cut)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use legion_core::{BoxFuture, Call, Door, Node, Paths, Registry};

    use super::*;
    use crate::svc::backend::BackendLogs;

    #[test]
    fn name_validation_accepts_valid_names_and_rejects_bad_ones() {
        for good in [
            "nginx",
            "nginx.service",
            "worker@42.service",
            "dbus-:1.30-org.example@0.service",
            "com.example.worker",
            "Spooler",
        ] {
            for kind in [
                BackendKind::Systemd,
                BackendKind::Launchd,
                BackendKind::Windows,
            ] {
                assert!(
                    prepare_name(good, kind, NamePurpose::Control).is_ok(),
                    "{good} on {kind:?}"
                );
            }
        }

        // Systemd appends `.service` when no unit suffix is present.
        assert_eq!(
            prepare_name("nginx", BackendKind::Systemd, NamePurpose::Control).unwrap(),
            "nginx.service"
        );
        assert_eq!(
            prepare_name(
                "org.example.worker",
                BackendKind::Systemd,
                NamePurpose::Read
            )
            .unwrap(),
            "org.example.worker.service"
        );
        // Read-only suffixes are accepted for Read and refused for Control.
        for ro in ["backup.timer", "dbus.socket", "watch.path"] {
            assert_eq!(
                prepare_name(ro, BackendKind::Systemd, NamePurpose::Read).unwrap(),
                ro
            );
            let err = prepare_name(ro, BackendKind::Systemd, NamePurpose::Control).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{ro}");
        }
        // Dangerous unit types like targets, slices, scopes and mounts are always refused.
        for non_svc in [
            "exit.target",
            "reboot.target",
            "user.slice",
            "init.scope",
            "home.mount",
        ] {
            for purpose in [NamePurpose::Read, NamePurpose::Control] {
                let err = prepare_name(non_svc, BackendKind::Systemd, purpose).unwrap_err();
                assert_eq!(err.code, ErrorCode::InvalidArgs, "{non_svc} {purpose:?}");
            }
        }

        // Systemd hex escapes are accepted on systemd and rejected on launchd/windows.
        assert!(
            prepare_name(
                "app-gnome\\x2dkeyring@autostart.service",
                BackendKind::Systemd,
                NamePurpose::Control
            )
            .is_ok()
        );
        assert!(
            prepare_name(
                "app-gnome\\x2dkeyring@autostart.service",
                BackendKind::Windows,
                NamePurpose::Control
            )
            .is_err()
        );
        assert!(
            prepare_name(
                "app-gnome\\x2dkeyring@autostart.service",
                BackendKind::Launchd,
                NamePurpose::Control
            )
            .is_err()
        );

        let too_long = "a".repeat(MAX_NAME_LEN + 1);
        for bad in [
            "",
            "   ",
            "-user",
            "--all",
            "legion-review-*",
            "worker-?.service",
            "worker-[0-9].service",
            "foo/bar",
            "../etc/passwd",
            "foo\\bar",
            "C:\\Windows\\svc",
            "foo\\x2",
            "foo\\xZZ",
            "bad\nname",
            "bad\0name",
            "bad\tname",
            &too_long,
        ] {
            for kind in [
                BackendKind::Systemd,
                BackendKind::Launchd,
                BackendKind::Windows,
            ] {
                let err = prepare_name(bad, kind, NamePurpose::Control).unwrap_err();
                assert_eq!(err.code, ErrorCode::InvalidArgs, "{bad:?} on {kind:?}");
            }
        }
    }

    #[test]
    fn parse_since_handles_rfc3339_and_seconds() {
        let now: Timestamp = "2026-11-23T22:05:00Z".parse().unwrap();
        assert_eq!(
            parse_since(SinceArg::Seconds(300.0), now).unwrap(),
            "2026-11-23T22:00:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            parse_since(SinceArg::Timestamp("60".to_owned()), now).unwrap(),
            "2026-11-23T22:04:00Z".parse::<Timestamp>().unwrap()
        );
        assert_eq!(
            parse_since(SinceArg::Timestamp("2026-11-23T20:00:00Z".to_owned()), now).unwrap(),
            "2026-11-23T20:00:00Z".parse::<Timestamp>().unwrap()
        );
        for bad in [
            SinceArg::Seconds(0.0),
            SinceArg::Seconds(-10.0),
            SinceArg::Seconds(f64::NAN),
            SinceArg::Timestamp(String::new()),
            SinceArg::Timestamp("yesterday".to_owned()),
        ] {
            assert_eq!(
                parse_since(bad, now).unwrap_err().code,
                ErrorCode::InvalidArgs
            );
        }
    }

    #[test]
    fn format_log_lines_cuts_and_marks_long_lines_on_utf8_boundary() {
        let short = "short line".to_owned();
        let long = "é".repeat(1500); // 3000 UTF-8 bytes
        let (out, cut) = format_log_lines(vec![short.clone(), long]);
        assert!(cut);
        assert_eq!(out[0], short);
        assert!(out[1].ends_with(LOG_LINE_CUT_MARKER));
        assert!(out[1].len() <= MAX_LOG_LINE_BYTES + LOG_LINE_CUT_MARKER.len());
    }

    type StatusQueueMap = BTreeMap<(Scope, String), Vec<Result<BackendStatus, ErrorBody>>>;
    type ControlCall = (String, Scope, Action, bool);

    #[derive(Clone)]
    struct FakeBackend {
        kind: BackendKind,
        supports_user: bool,
        statuses: Arc<Mutex<StatusQueueMap>>,
        control_log: Arc<Mutex<Vec<ControlCall>>>,
        logs: Vec<String>,
    }

    impl FakeBackend {
        fn new(kind: BackendKind, supports_user: bool) -> Self {
            Self {
                kind,
                supports_user,
                statuses: Arc::new(Mutex::new(BTreeMap::new())),
                control_log: Arc::new(Mutex::new(Vec::new())),
                logs: vec!["boom: exit 1".to_owned()],
            }
        }

        fn push_status(&self, scope: Scope, name: &str, state: ServiceState) {
            self.push_status_with(
                scope,
                name,
                state,
                (state == ServiceState::Running).then_some(123),
                None,
            );
        }

        fn push_status_with(
            &self,
            scope: Scope,
            name: &str,
            state: ServiceState,
            pid: Option<u32>,
            instance_id: Option<&str>,
        ) {
            let status = BackendStatus {
                name: name.to_owned(),
                scope,
                state,
                enabled: Some(true),
                pid,
                since: None,
                instance_id: instance_id.map(str::to_owned),
                description: Some("test service".to_owned()),
                exit_code: (state == ServiceState::Failed).then_some(1),
                detail: BTreeMap::new(),
            };
            self.statuses
                .lock()
                .unwrap()
                .entry((scope, name.to_owned()))
                .or_default()
                .push(Ok(status));
        }
    }

    impl Backend for FakeBackend {
        fn kind(&self) -> BackendKind {
            self.kind
        }

        fn supports_user_scope(&self) -> bool {
            self.supports_user
        }

        fn list<'a>(
            &'a self,
            _ctx: &'a Ctx,
            _scope: Scope,
        ) -> BoxFuture<'a, Result<Vec<ServiceSummary>, ErrorBody>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn status<'a>(
            &'a self,
            _ctx: &'a Ctx,
            name: &'a str,
            scope: Scope,
        ) -> BoxFuture<'a, Result<BackendStatus, ErrorBody>> {
            let result = {
                let mut map = self.statuses.lock().unwrap();
                match map.get_mut(&(scope, name.to_owned())) {
                    Some(queue) if queue.len() > 1 => queue.remove(0),
                    Some(queue) if !queue.is_empty() => queue[0].clone(),
                    _ => Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("not found: {name}"),
                    )),
                }
            };
            Box::pin(async move { result })
        }

        fn control<'a>(
            &'a self,
            _ctx: &'a Ctx,
            name: &'a str,
            scope: Scope,
            action: Action,
            use_sudo: bool,
        ) -> BoxFuture<'a, Result<(), ErrorBody>> {
            self.control_log
                .lock()
                .unwrap()
                .push((name.to_owned(), scope, action, use_sudo));
            Box::pin(async { Ok(()) })
        }

        fn logs<'a>(
            &'a self,
            _ctx: &'a Ctx,
            _name: &'a str,
            _scope: Scope,
            _lines: usize,
            _since: Option<Timestamp>,
        ) -> BoxFuture<'a, Result<BackendLogs, ErrorBody>> {
            let lines = self.logs.clone();
            Box::pin(async move {
                Ok(BackendLogs {
                    lines,
                    truncated: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn scope_resolution_and_action_verification_work_through_fake_backend() {
        async fn probe_verify(ctx: &Ctx, _args: legion_core::NoArgs) -> Result<Value, ToolError> {
            let fake = FakeBackend::new(BackendKind::Systemd, true);
            fake.push_status(Scope::User, "user-only.service", ServiceState::Running);
            fake.push_status(Scope::System, "sys-only.service", ServiceState::Stopped);
            fake.push_status(Scope::User, "both.service", ServiceState::Running);
            fake.push_status(Scope::System, "both.service", ServiceState::Stopped);

            let u = resolve_scope(&fake, ctx, "user-only.service", None)
                .await
                .unwrap();
            assert_eq!(u.scope, Scope::User);

            let s = resolve_scope(&fake, ctx, "sys-only.service", None)
                .await
                .unwrap();
            assert_eq!(s.scope, Scope::System);

            let conflict = resolve_scope(&fake, ctx, "both.service", None)
                .await
                .unwrap_err();
            assert_eq!(conflict.code, ErrorCode::Conflict);

            let explicit = resolve_scope(&fake, ctx, "both.service", Some(Scope::System))
                .await
                .unwrap();
            assert_eq!(explicit.scope, Scope::System);

            let missing = resolve_scope(&fake, ctx, "nowhere.service", None)
                .await
                .unwrap_err();
            assert_eq!(missing.code, ErrorCode::NotFound);

            // Windows rejects explicit user scope and resolves omitted scope to system.
            let win = FakeBackend::new(BackendKind::Windows, false);
            win.push_status(Scope::System, "Spooler", ServiceState::Running);
            assert_eq!(
                resolve_scope(&win, ctx, "Spooler", Some(Scope::User))
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::Unsupported
            );
            assert_eq!(
                resolve_scope(&win, ctx, "Spooler", None)
                    .await
                    .unwrap()
                    .scope,
                Scope::System
            );

            // Action reads back state after control and passes sudo only for system scope.
            let fast = WaitPolicy {
                timeout: Duration::from_millis(80),
                poll_interval: Duration::from_millis(10),
                settle: Duration::from_millis(15),
            };
            let act = FakeBackend::new(BackendKind::Systemd, true);
            act.push_status(Scope::System, "app.service", ServiceState::Stopped);
            act.push_status(Scope::System, "app.service", ServiceState::Other);
            act.push_status(Scope::System, "app.service", ServiceState::Running);
            act.push_status(Scope::System, "app.service", ServiceState::Running);
            let started = execute_action(
                &act,
                ctx,
                "app.service",
                Some(Scope::System),
                Action::Start,
                true,
                fast,
            )
            .await
            .unwrap();
            assert_eq!(started.previous_state, ServiceState::Stopped);
            assert_eq!(started.state, ServiceState::Running);
            assert_eq!(
                act.control_log.lock().unwrap().as_slice(),
                &[("app.service".to_owned(), Scope::System, Action::Start, true)]
            );

            // Service that shows running right after start but crashes during settle fails.
            let crash_soon = FakeBackend::new(BackendKind::Systemd, true);
            crash_soon.push_status(Scope::User, "crashy.service", ServiceState::Stopped);
            crash_soon.push_status(Scope::User, "crashy.service", ServiceState::Running);
            crash_soon.push_status(Scope::User, "crashy.service", ServiceState::Failed);
            let err = execute_action(
                &crash_soon,
                ctx,
                "crashy.service",
                Some(Scope::User),
                Action::Start,
                false,
                fast,
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Failed);
            assert_eq!(
                err.details.as_ref().and_then(|d| d.get("logs")),
                Some(&json!(["boom: exit 1"]))
            );

            // Restart must observe a new PID or instance id; an unchanged running service fails.
            let no_restart = FakeBackend::new(BackendKind::Systemd, true);
            no_restart.push_status_with(
                Scope::User,
                "sticky.service",
                ServiceState::Running,
                Some(500),
                Some("inv-1"),
            );
            let err = execute_action(
                &no_restart,
                ctx,
                "sticky.service",
                Some(Scope::User),
                Action::Restart,
                false,
                fast,
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Failed);
            assert!(
                err.message.contains("same process/start identity"),
                "{}",
                err.message
            );

            // Restart that gets a new instance id and survives settle succeeds.
            let did_restart = FakeBackend::new(BackendKind::Systemd, true);
            did_restart.push_status_with(
                Scope::User,
                "fresh.service",
                ServiceState::Running,
                Some(500),
                Some("inv-1"),
            );
            did_restart.push_status_with(
                Scope::User,
                "fresh.service",
                ServiceState::Running,
                Some(501),
                Some("inv-2"),
            );
            let restarted = execute_action(
                &did_restart,
                ctx,
                "fresh.service",
                Some(Scope::User),
                Action::Restart,
                false,
                fast,
            )
            .await
            .unwrap();
            assert_eq!(restarted.previous_state, ServiceState::Running);
            assert_eq!(restarted.state, ServiceState::Running);

            // Service that transitions to `failed` returns `ErrorCode::Failed` with logs.
            let fail_backend = FakeBackend::new(BackendKind::Systemd, true);
            fail_backend.push_status(Scope::User, "bad.service", ServiceState::Stopped);
            fail_backend.push_status(Scope::User, "bad.service", ServiceState::Failed);
            let err = execute_action(
                &fail_backend,
                ctx,
                "bad.service",
                Some(Scope::User),
                Action::Start,
                false,
                fast,
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Failed);
            assert_eq!(
                err.details.as_ref().and_then(|d| d.get("logs")),
                Some(&json!(["boom: exit 1"]))
            );

            // Service that never reaches expected state within wait budget fails with logs.
            let stuck = FakeBackend::new(BackendKind::Systemd, true);
            stuck.push_status(Scope::User, "stuck.service", ServiceState::Stopped);
            stuck.push_status(Scope::User, "stuck.service", ServiceState::Other);
            let err = execute_action(
                &stuck,
                ctx,
                "stuck.service",
                Some(Scope::User),
                Action::Start,
                false,
                fast,
            )
            .await
            .unwrap_err();
            assert_eq!(err.code, ErrorCode::Failed);
            assert_eq!(
                err.details.as_ref().and_then(|d| d.get("logs")),
                Some(&json!(["boom: exit 1"]))
            );

            // Stopping a transient unit that disappears after stop reports `stopped`.
            let transient = FakeBackend::new(BackendKind::Systemd, true);
            transient.push_status(Scope::User, "temp.service", ServiceState::Running);
            transient
                .statuses
                .lock()
                .unwrap()
                .entry((Scope::User, "temp.service".to_owned()))
                .or_default()
                .push(Err(ErrorBody::new(ErrorCode::NotFound, "unloaded")));
            let stopped = execute_action(
                &transient,
                ctx,
                "temp.service",
                Some(Scope::User),
                Action::Stop,
                false,
                fast,
            )
            .await
            .unwrap();
            assert_eq!(stopped.previous_state, ServiceState::Running);
            assert_eq!(stopped.state, ServiceState::Stopped);

            // Stopping a failed unit with no running PID succeeds immediately and reports `failed`.
            let failed_stop = FakeBackend::new(BackendKind::Systemd, true);
            failed_stop.push_status_with(
                Scope::User,
                "dead-failed.service",
                ServiceState::Failed,
                None,
                None,
            );
            let stopped_failed = execute_action(
                &failed_stop,
                ctx,
                "dead-failed.service",
                Some(Scope::User),
                Action::Stop,
                false,
                fast,
            )
            .await
            .unwrap();
            assert_eq!(stopped_failed.previous_state, ServiceState::Failed);
            assert_eq!(stopped_failed.state, ServiceState::Failed);

            Ok(json!({}))
        }

        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::new();
        reg.add_fn(
            "probe.verify",
            Effect::Read,
            "Verifies scope resolution and action polling.",
            probe_verify,
        );
        let node = Node::new(Paths::under(dir.path()), reg);
        node.call(Call::new(Door::Cli, "probe.verify", json!({})))
            .await
            .unwrap();
    }
}
