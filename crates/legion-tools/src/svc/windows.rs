//! Windows Service Control Manager backend: PowerShell command builders and JSON parsers.
//!
//! Only the `system` scope exists on Windows. Service names and parameters are passed through
//! environment variables (`LEGION_SVC_*`) rather than interpolated into PowerShell text, and
//! PowerShell is started via exact argv with `-NoProfile -NonInteractive` and an empty
//! `PSModulePath` so an inherited PowerShell 7 path cannot break Windows PowerShell 5.1 cmdlets.

use std::collections::BTreeMap;

use jiff::Timestamp;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{BoxFuture, Ctx};
use serde::Deserialize;
use serde_json::{Value, json};

use super::backend::{
    ACTION_TIMEOUT, Backend, BackendLogs, BackendStatus, CmdSpec, LOGS_CAPTURE_LIMIT,
    QUERY_CAPTURE_LIMIT, QUERY_TIMEOUT, run_cmd,
};
use super::{Action, BackendKind, Scope, ServiceState, ServiceSummary};

/// Windows Service Control Manager backend.
pub(crate) struct Windows;

impl Backend for Windows {
    fn kind(&self) -> BackendKind {
        BackendKind::Windows
    }

    fn supports_user_scope(&self) -> bool {
        false
    }

    fn list<'a>(
        &'a self,
        ctx: &'a Ctx,
        scope: Scope,
    ) -> BoxFuture<'a, Result<Vec<ServiceSummary>, ErrorBody>> {
        Box::pin(async move {
            if scope == Scope::User {
                return Err(user_scope_unsupported());
            }
            let cmd = list_cmd();
            let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !out.success() {
                return Err(classify_error(&out.stderr.text, None, "list services"));
            }
            parse_list_json(&out.stdout.text)
        })
    }

    fn status<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
    ) -> BoxFuture<'a, Result<BackendStatus, ErrorBody>> {
        Box::pin(async move {
            if scope == Scope::User {
                return Err(user_scope_unsupported());
            }
            let cmd = status_cmd(name);
            let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !out.success() {
                return Err(classify_error(
                    &out.stderr.text,
                    Some((None, name)),
                    &format!("inspect service {name:?}"),
                ));
            }
            parse_status_json(&out.stdout.text, name)
        })
    }

    fn control<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
        action: Action,
        _use_sudo: bool,
    ) -> BoxFuture<'a, Result<(), ErrorBody>> {
        Box::pin(async move {
            if scope == Scope::User {
                return Err(user_scope_unsupported());
            }
            let cmd = control_cmd(name, action);
            let out = run_cmd(ctx, &cmd, ACTION_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !out.success() {
                return Err(classify_error(
                    &out.stderr.text,
                    Some((Some(action), name)),
                    &format!("{} service {name:?}", action.as_str()),
                ));
            }
            Ok(())
        })
    }

    fn logs<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
        lines: usize,
        since: Option<Timestamp>,
    ) -> BoxFuture<'a, Result<BackendLogs, ErrorBody>> {
        Box::pin(async move {
            if scope == Scope::User {
                return Err(user_scope_unsupported());
            }
            let ask = lines.saturating_add(1);
            let cmd = logs_cmd(name, ask, since);
            let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, LOGS_CAPTURE_LIMIT).await?;
            if !out.success() {
                let err = classify_error(
                    &out.stderr.text,
                    Some((None, name)),
                    &format!("read event logs for {name:?}"),
                );
                if err.code == ErrorCode::Denied {
                    return Err(err);
                }
                return Err(unsupported_logs_error(name, &out.stderr.text));
            }
            parse_event_log_json(&out.stdout.text, lines, out.stdout.truncated)
        })
    }
}

fn ps_cmd(script: &str) -> CmdSpec {
    CmdSpec::new(
        "powershell.exe",
        ["-NoProfile", "-NonInteractive", "-Command", script],
    )
    .with_env("PSModulePath", "")
}

/// Builds the PowerShell command for `svc.list` using `Get-Service` and `Get-CimInstance Win32_Service`.
pub(crate) fn list_cmd() -> CmdSpec {
    const SCRIPT: &str = "\
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
$ErrorActionPreference = 'Stop'; \
$cim = @{}; \
try { Get-CimInstance -ClassName Win32_Service | ForEach-Object { $cim[$_.Name] = $_ } } catch {}; \
Get-Service | ForEach-Object { \
  $c = $cim[$_.Name]; \
  [PSCustomObject]@{ \
    Name = $_.Name; \
    DisplayName = $_.DisplayName; \
    Status = $_.Status.ToString(); \
    StartMode = if ($c) { $c.StartMode } else { $_.StartType.ToString() }; \
    Description = if ($c -and $c.Description) { $c.Description } else { $_.DisplayName } \
  } \
} | ConvertTo-Json -Compress";
    ps_cmd(SCRIPT)
}

/// Builds the PowerShell command for `svc.status` using `Get-Service` and `Get-CimInstance Win32_Service`.
pub(crate) fn status_cmd(name: &str) -> CmdSpec {
    const SCRIPT: &str = "\
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
$ErrorActionPreference = 'Stop'; \
$n = $env:LEGION_SVC_NAME; \
$s = Get-Service -Name $n -ErrorAction Stop; \
$escaped = $n.Replace(\"'\", \"''\").Replace('\\', '\\\\'); \
$c = $null; \
try { $c = Get-CimInstance -ClassName Win32_Service -Filter \"Name='$escaped'\" -ErrorAction SilentlyContinue } catch {}; \
[PSCustomObject]@{ \
  Name = $s.Name; \
  DisplayName = $s.DisplayName; \
  Status = $s.Status.ToString(); \
  StartMode = if ($c) { $c.StartMode } else { $s.StartType.ToString() }; \
  ProcessId = if ($c) { $c.ProcessId } else { $null }; \
  Description = if ($c -and $c.Description) { $c.Description } else { $s.DisplayName }; \
  PathName = if ($c) { $c.PathName } else { $null }; \
  ExitCode = if ($c) { $c.ExitCode } else { $null }; \
  ServiceSpecificExitCode = if ($c) { $c.ServiceSpecificExitCode } else { $null } \
} | ConvertTo-Json -Compress";
    ps_cmd(SCRIPT).with_env("LEGION_SVC_NAME", name)
}

/// Builds the PowerShell command for `svc.start`, `svc.stop` or `svc.restart`.
pub(crate) fn control_cmd(name: &str, action: Action) -> CmdSpec {
    let cmdlet = match action {
        Action::Start => "Start-Service",
        Action::Stop => "Stop-Service",
        Action::Restart => "Restart-Service",
    };
    let script = format!(
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
         $ErrorActionPreference = 'Stop'; \
         {cmdlet} -Name $env:LEGION_SVC_NAME -ErrorAction Stop"
    );
    ps_cmd(&script).with_env("LEGION_SVC_NAME", name)
}

/// Builds the PowerShell command for best-effort event log lookup via `Get-WinEvent`.
pub(crate) fn logs_cmd(name: &str, lines: usize, since: Option<Timestamp>) -> CmdSpec {
    const SCRIPT: &str = "\
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false); \
$ErrorActionPreference = 'Stop'; \
$n = $env:LEGION_SVC_NAME; \
$max = [int]$env:LEGION_SVC_LINES; \
$start = if ($env:LEGION_SVC_SINCE) { [DateTimeOffset]::Parse($env:LEGION_SVC_SINCE).UtcDateTime } else { $null }; \
$f1 = @{ LogName = 'System', 'Application'; ProviderName = $n }; \
if ($start) { $f1['StartTime'] = $start }; \
$events = @(); \
try { \
  $events = @(Get-WinEvent -FilterHashtable $f1 -MaxEvents $max -ErrorAction Stop) \
} catch { \
  $f2 = @{ LogName = 'System'; ProviderName = 'Service Control Manager' }; \
  if ($start) { $f2['StartTime'] = $start }; \
  try { \
    $events = @(Get-WinEvent -FilterHashtable $f2 -MaxEvents ($max * 5) -ErrorAction Stop | \
      Where-Object { $_.Message -and $_.Message.IndexOf($n, [StringComparison]::OrdinalIgnoreCase) -ge 0 } | \
      Select-Object -First $max) \
  } catch { $events = @() } \
}; \
$events | ForEach-Object { \
  [PSCustomObject]@{ \
    TimeCreated = $_.TimeCreated.ToUniversalTime().ToString('o'); \
    Level = $_.LevelDisplayName; \
    Provider = $_.ProviderName; \
    Id = $_.Id; \
    Message = $_.Message \
  } \
} | ConvertTo-Json -Compress";
    let mut cmd = ps_cmd(SCRIPT)
        .with_env("LEGION_SVC_NAME", name)
        .with_env("LEGION_SVC_LINES", lines.to_string());
    if let Some(ts) = since {
        cmd = cmd.with_env(
            "LEGION_SVC_SINCE",
            ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        );
    }
    cmd
}

/// PowerShell emits either a JSON object (one item) or a JSON array (several items).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many(Vec<T>),
    One(T),
}

impl<T> OneOrMany<T> {
    fn into_vec(self) -> Vec<T> {
        match self {
            OneOrMany::Many(items) => items,
            OneOrMany::One(item) => vec![item],
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsServiceRow {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    status: Option<Value>,
    #[serde(default)]
    start_mode: Option<Value>,
    #[serde(default)]
    start_type: Option<Value>,
    #[serde(default)]
    process_id: Option<u32>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    path_name: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    service_specific_exit_code: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsEventRow {
    #[serde(default)]
    time_created: Option<String>,
    #[serde(default)]
    level: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    id: Option<u32>,
    #[serde(default)]
    message: Option<String>,
}

/// Parses PowerShell `ConvertTo-Json` output for `svc.list` (empty, single object, or array).
pub(crate) fn parse_list_json(text: &str) -> Result<Vec<ServiceSummary>, ErrorBody> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(Vec::new());
    }
    let parsed: OneOrMany<PsServiceRow> = serde_json::from_str(trimmed).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not parse Windows service list JSON: {error}."),
        )
    })?;
    Ok(parsed
        .into_vec()
        .into_iter()
        .map(|row| {
            let status_text = status_value_to_str(row.status.as_ref());
            let start_mode = row
                .start_mode
                .as_ref()
                .or(row.start_type.as_ref())
                .and_then(start_mode_value_to_str);
            let description = row
                .description
                .filter(|d| !d.trim().is_empty())
                .or(row.display_name)
                .filter(|d| !d.trim().is_empty());
            ServiceSummary {
                name: row.name,
                scope: Scope::System,
                state: map_windows_status(status_text.as_deref(), None),
                enabled: start_mode.as_deref().and_then(map_start_mode),
                description,
            }
        })
        .collect())
}

/// Parses PowerShell `ConvertTo-Json` output for `svc.status`.
pub(crate) fn parse_status_json(
    text: &str,
    requested_name: &str,
) -> Result<BackendStatus, ErrorBody> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("Windows service {requested_name:?} was not found."),
        )
        .with_hint("Run `svc.list` to see the services on this machine."));
    }
    let row: PsServiceRow = serde_json::from_str(trimmed).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not parse Windows service status JSON: {error}."),
        )
    })?;

    let status_text = status_value_to_str(row.status.as_ref());
    let start_mode = row
        .start_mode
        .as_ref()
        .or(row.start_type.as_ref())
        .and_then(start_mode_value_to_str);
    let pid = row.process_id.filter(|&p| p > 0);
    let effective_exit = match (row.exit_code, row.service_specific_exit_code) {
        (Some(1066), Some(specific)) if specific != 0 => Some(specific),
        (Some(code), _) if code != 0 => Some(code),
        _ => None,
    };
    let state = map_windows_status(status_text.as_deref(), effective_exit);
    let description = row
        .description
        .filter(|d| !d.trim().is_empty())
        .or_else(|| row.display_name.clone())
        .filter(|d| !d.trim().is_empty());

    let mut detail = BTreeMap::new();
    if let Some(s) = status_text {
        detail.insert("status".to_owned(), Value::String(s));
    }
    if let Some(m) = &start_mode {
        detail.insert("start_mode".to_owned(), Value::String(m.clone()));
    }
    if let Some(d) = row.display_name.filter(|d| !d.trim().is_empty()) {
        detail.insert("display_name".to_owned(), Value::String(d));
    }
    if let Some(p) = row.path_name.filter(|p| !p.trim().is_empty()) {
        detail.insert("path_name".to_owned(), Value::String(p));
    }
    if let Some(code) = row.exit_code {
        detail.insert("exit_code".to_owned(), json!(code));
    }

    Ok(BackendStatus {
        name: row.name,
        scope: Scope::System,
        state,
        enabled: start_mode.as_deref().and_then(map_start_mode),
        pid,
        since: None,
        description,
        exit_code: effective_exit,
        detail,
    })
}

/// Parses PowerShell `Get-WinEvent | ConvertTo-Json` output (empty, single object, or array).
///
/// `Get-WinEvent` returns newest events first, so this reverses them to match `svc.logs`'s
/// oldest-first ordering.
pub(crate) fn parse_event_log_json(
    text: &str,
    limit: usize,
    stream_truncated: bool,
) -> Result<BackendLogs, ErrorBody> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(BackendLogs {
            lines: Vec::new(),
            truncated: stream_truncated,
        });
    }
    let parsed: OneOrMany<PsEventRow> = serde_json::from_str(trimmed).map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Could not parse Windows Event Log JSON: {error}."),
        )
    })?;
    let mut rows = parsed.into_vec();
    let mut truncated = stream_truncated;
    if rows.len() > limit {
        truncated = true;
        rows.truncate(limit);
    }
    rows.reverse();

    let lines = rows
        .into_iter()
        .map(|row| {
            let ts = row.time_created.unwrap_or_default();
            let level = row.level.unwrap_or_else(|| "Info".to_owned());
            let provider = row.provider.unwrap_or_else(|| "EventLog".to_owned());
            let id = row.id.map_or_else(String::new, |id| format!("({id})"));
            let msg = row
                .message
                .unwrap_or_default()
                .replace(['\r', '\n'], " ")
                .trim()
                .to_owned();
            if ts.is_empty() {
                format!("[{level}] {provider}{id}: {msg}")
            } else {
                format!("{ts} [{level}] {provider}{id}: {msg}")
            }
        })
        .collect();

    Ok(BackendLogs { lines, truncated })
}

fn status_value_to_str(val: Option<&Value>) -> Option<String> {
    match val? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => match n.as_i64()? {
            1 => Some("Stopped".to_owned()),
            2 => Some("StartPending".to_owned()),
            3 => Some("StopPending".to_owned()),
            4 => Some("Running".to_owned()),
            5 => Some("ContinuePending".to_owned()),
            6 => Some("PausePending".to_owned()),
            7 => Some("Paused".to_owned()),
            other => Some(other.to_string()),
        },
        _ => None,
    }
}

fn start_mode_value_to_str(val: &Value) -> Option<String> {
    match val {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => match n.as_i64()? {
            0 => Some("Boot".to_owned()),
            1 => Some("System".to_owned()),
            2 => Some("Automatic".to_owned()),
            3 => Some("Manual".to_owned()),
            4 => Some("Disabled".to_owned()),
            other => Some(other.to_string()),
        },
        _ => None,
    }
}

fn map_windows_status(status: Option<&str>, exit_code: Option<i32>) -> ServiceState {
    match status.map(str::to_ascii_lowercase).as_deref() {
        Some("running") => ServiceState::Running,
        Some("stopped") => match exit_code {
            Some(code) if code != 0 => ServiceState::Failed,
            _ => ServiceState::Stopped,
        },
        _ => ServiceState::Other,
    }
}

fn map_start_mode(mode: &str) -> Option<bool> {
    match mode.to_ascii_lowercase().as_str() {
        "auto" | "automatic" | "automaticdelayedstart" | "boot" | "system" => Some(true),
        "manual" | "disabled" => Some(false),
        _ => None,
    }
}

fn user_scope_unsupported() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        "Windows services only exist in the system scope.",
    )
    .with_hint("Omit `scope` or pass `scope: \"system\"`.")
}

fn unsupported_logs_error(name: &str, stderr: &str) -> ErrorBody {
    let mut err = ErrorBody::new(
        ErrorCode::Unsupported,
        format!("Could not read Windows Event Log entries for service {name:?}."),
    )
    .with_hint(format!(
        "Check Event Viewer or run `Get-WinEvent -LogName System -MaxEvents 50` in an elevated PowerShell session to inspect logs for {name:?}."
    ));
    let trimmed = stderr.trim();
    if !trimmed.is_empty() {
        err = err.with_details(json!({"stderr": trimmed}));
    }
    err
}

/// Classifies a PowerShell failure into the closed `legion-proto` error codes.
pub(crate) fn classify_error(
    stderr: &str,
    target: Option<(Option<Action>, &str)>,
    what: &str,
) -> ErrorBody {
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();

    if lower.contains("access is denied")
        || lower.contains("unauthorizedaccessexception")
        || lower.contains("permissiondenied")
        || lower.contains("cannot open")
        || lower.contains("requires elevation")
    {
        let message = match target {
            Some((Some(action), name)) => format!(
                "Access denied while trying to {} Windows service {name:?}.",
                action.as_str()
            ),
            _ => format!("Access denied while trying to {what}."),
        };
        return ErrorBody::new(ErrorCode::Denied, message)
            .with_hint(
                "Run the legion node elevated (as Administrator) to control Windows services.",
            )
            .with_details(json!({"stderr": trimmed}));
    }

    if (lower.contains("cannot find any service")
        || lower.contains("no service")
        || lower.contains("objectnotfound"))
        && let Some((_, name)) = target
    {
        return ErrorBody::new(
            ErrorCode::NotFound,
            format!("Windows service {name:?} was not found."),
        )
        .with_hint("Run `svc.list` to see the services on this machine.");
    }

    ErrorBody::new(
        ErrorCode::Failed,
        if trimmed.is_empty() {
            format!("PowerShell failed while trying to {what}.")
        } else {
            format!("PowerShell failed while trying to {what}: {trimmed}")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_use_noprofile_noninteractive_and_env_not_interpolation() {
        let list = list_cmd();
        assert_eq!(list.program, "powershell.exe");
        assert_eq!(
            &list.args[..3],
            ["-NoProfile", "-NonInteractive", "-Command"]
        );
        assert!(
            list.env
                .contains(&("PSModulePath".to_owned(), String::new()))
        );

        let status = status_cmd("Spooler");
        assert!(status.args[3].contains("Get-Service"));
        assert!(status.args[3].contains("Get-CimInstance"));
        assert!(!status.args[3].contains("Spooler"));
        assert!(
            status
                .env
                .contains(&("LEGION_SVC_NAME".to_owned(), "Spooler".to_owned()))
        );

        let restart = control_cmd("Spooler", Action::Restart);
        assert!(restart.args[3].contains("Restart-Service"));
        assert!(!restart.args[3].contains("Spooler"));

        let ts: Timestamp = "2026-11-23T22:00:00Z".parse().unwrap();
        let logs = logs_cmd("Spooler", 25, Some(ts));
        assert!(logs.args[3].contains("Get-WinEvent"));
        assert!(logs.env.contains(&(
            "LEGION_SVC_SINCE".to_owned(),
            "2026-11-23T22:00:00Z".to_owned()
        )));
    }

    #[test]
    fn parse_list_json_handles_single_object_array_and_numeric_status() {
        let single = r#"{"Name":"Spooler","DisplayName":"Print Spooler","Status":"Running","StartMode":"Auto","Description":"Loads files to memory for printing"}"#;
        let one = parse_list_json(single).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "Spooler");
        assert_eq!(one[0].scope, Scope::System);
        assert_eq!(one[0].state, ServiceState::Running);
        assert_eq!(one[0].enabled, Some(true));

        let many = r#"[
            {"Name":"Spooler","DisplayName":"Print Spooler","Status":4,"StartType":2},
            {"Name":"WSearch","DisplayName":"Windows Search","Status":"Stopped","StartMode":"Disabled"},
            {"Name":"CustomSvc","DisplayName":"Custom","Status":"StartPending","StartMode":"Manual"}
        ]"#;
        let list = parse_list_json(many).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].state, ServiceState::Running);
        assert_eq!(list[0].enabled, Some(true));
        assert_eq!(list[1].state, ServiceState::Stopped);
        assert_eq!(list[1].enabled, Some(false));
        assert_eq!(list[2].state, ServiceState::Other);
        assert_eq!(list[2].enabled, Some(false));

        assert!(parse_list_json("").unwrap().is_empty());
        assert!(parse_list_json("null").unwrap().is_empty());
    }

    #[test]
    fn parse_status_json_extracts_pid_start_mode_and_exit_code() {
        let running = r#"{
            "Name": "Spooler",
            "DisplayName": "Print Spooler",
            "Status": "Running",
            "StartMode": "Auto",
            "ProcessId": 2408,
            "Description": "Manages print queues",
            "PathName": "C:\\Windows\\System32\\spoolsv.exe",
            "ExitCode": 0,
            "ServiceSpecificExitCode": 0
        }"#;
        let st = parse_status_json(running, "Spooler").unwrap();
        assert_eq!(st.name, "Spooler");
        assert_eq!(st.state, ServiceState::Running);
        assert_eq!(st.enabled, Some(true));
        assert_eq!(st.pid, Some(2408));
        assert_eq!(st.exit_code, None);
        assert_eq!(st.detail.get("start_mode"), Some(&json!("Auto")));

        let failed = r#"{
            "Name": "BrokenSvc",
            "DisplayName": "Broken Service",
            "Status": "Stopped",
            "StartMode": "Manual",
            "ProcessId": 0,
            "ExitCode": 1066,
            "ServiceSpecificExitCode": 42
        }"#;
        let st = parse_status_json(failed, "BrokenSvc").unwrap();
        assert_eq!(st.state, ServiceState::Failed);
        assert_eq!(st.pid, None);
        assert_eq!(st.exit_code, Some(42));
    }

    #[test]
    fn parse_event_log_json_handles_one_and_many_and_reverses_order() {
        let one = r#"{"TimeCreated":"2026-11-23T22:00:00Z","Level":"Information","Provider":"Service Control Manager","Id":7036,"Message":"The Spooler service entered the running state."}"#;
        let logs = parse_event_log_json(one, 10, false).unwrap();
        assert_eq!(logs.lines.len(), 1);
        assert!(!logs.truncated);
        assert!(logs.lines[0].contains("7036"));

        let many = r#"[
            {"TimeCreated":"2026-11-23T22:02:00Z","Level":"Error","Provider":"Spooler","Id":2,"Message":"newest"},
            {"TimeCreated":"2026-11-23T22:01:00Z","Level":"Warning","Provider":"Spooler","Id":1,"Message":"middle"},
            {"TimeCreated":"2026-11-23T22:00:00Z","Level":"Information","Provider":"Spooler","Id":0,"Message":"oldest"}
        ]"#;
        let logs = parse_event_log_json(many, 2, false).unwrap();
        assert!(logs.truncated);
        assert_eq!(logs.lines.len(), 2);
        assert!(logs.lines[0].contains("middle"));
        assert!(logs.lines[1].contains("newest"));
    }

    #[test]
    fn classify_error_maps_elevation_and_not_found() {
        let denied = classify_error(
            "Stop-Service : Cannot open Spooler service on computer '.'. Access is denied.",
            Some((Some(Action::Stop), "Spooler")),
            "stop service",
        );
        assert_eq!(denied.code, ErrorCode::Denied);
        assert!(denied.hint.as_deref().unwrap_or("").contains("elevated"));

        let missing = classify_error(
            "Get-Service : Cannot find any service with service name 'NoSuchSvc'.",
            Some((None, "NoSuchSvc")),
            "inspect service",
        );
        assert_eq!(missing.code, ErrorCode::NotFound);
    }
}
