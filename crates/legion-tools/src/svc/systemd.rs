//! Linux `systemd` backend: command builders and pure parsers for `systemctl` and `journalctl`.

use std::collections::BTreeMap;

use jiff::Timestamp;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{BoxFuture, Ctx};
use serde_json::{Value, json};

use super::backend::{
    ACTION_TIMEOUT, Backend, BackendLogs, BackendStatus, CmdSpec, LOGS_CAPTURE_LIMIT,
    QUERY_CAPTURE_LIMIT, QUERY_TIMEOUT, is_sudo_denied, run_cmd, unix_privilege_denied,
};
use super::{Action, BackendKind, Scope, ServiceState, ServiceSummary};

/// Properties requested from `systemctl show`.
const SHOW_PROPERTIES: &str = "Id,LoadState,ActiveState,SubState,UnitFileState,MainPID,ActiveEnterTimestamp,ExecMainStatus,ExecMainCode,Description,Result,FragmentPath";

/// Linux systemd service manager backend.
pub(crate) struct Systemd;

impl Backend for Systemd {
    fn kind(&self) -> BackendKind {
        BackendKind::Systemd
    }

    fn list<'a>(
        &'a self,
        ctx: &'a Ctx,
        scope: Scope,
    ) -> BoxFuture<'a, Result<Vec<ServiceSummary>, ErrorBody>> {
        Box::pin(async move {
            let units_cmd = list_units_cmd(scope);
            let units_out = run_cmd(ctx, &units_cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !units_out.success() {
                return Err(classify_error(
                    &units_out.stderr.text,
                    scope,
                    None,
                    false,
                    "list services",
                ));
            }
            let units = parse_list_units(&units_out.stdout.text, scope);

            // Unit files fill in `enabled` and include installed services that are not currently
            // loaded into systemd's memory. If `list-unit-files` fails for any reason, keep the
            // loaded units rather than losing the whole listing.
            let files_cmd = list_unit_files_cmd(scope);
            let files = match run_cmd(ctx, &files_cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await {
                Ok(out) if out.success() => parse_list_unit_files(&out.stdout.text),
                _ => Vec::new(),
            };

            Ok(merge_list(units, files, scope))
        })
    }

    fn status<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
    ) -> BoxFuture<'a, Result<BackendStatus, ErrorBody>> {
        Box::pin(async move {
            let cmd = show_cmd(name, scope);
            let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !out.success() {
                return Err(classify_error(
                    &out.stderr.text,
                    scope,
                    None,
                    false,
                    &format!("inspect service {name:?}"),
                ));
            }
            parse_show(&out.stdout.text, name, scope)
        })
    }

    fn control<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
        action: Action,
        use_sudo: bool,
    ) -> BoxFuture<'a, Result<(), ErrorBody>> {
        Box::pin(async move {
            let cmd = control_cmd(name, scope, action, use_sudo);
            let out = run_cmd(ctx, &cmd, ACTION_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !out.success() {
                return Err(classify_error(
                    &out.stderr.text,
                    scope,
                    Some((action, name)),
                    use_sudo,
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
            // Ask for one extra line so we can tell whether older entries were omitted.
            let ask = lines.saturating_add(1);
            let cmd = logs_cmd(name, scope, ask, since);
            let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, LOGS_CAPTURE_LIMIT).await?;
            if !out.success() {
                return Err(classify_journal_error(&out.stderr.text, scope, name));
            }
            Ok(parse_journal_lines(
                &out.stdout.text,
                lines,
                out.stdout.truncated,
            ))
        })
    }
}

/// Builds `systemctl [--user] list-units ...`.
pub(crate) fn list_units_cmd(scope: Scope) -> CmdSpec {
    let mut args = Vec::new();
    if scope == Scope::User {
        args.push("--user");
    }
    args.extend([
        "list-units",
        "--type=service",
        "--all",
        "--no-pager",
        "--no-legend",
        "--plain",
        "--full",
    ]);
    with_systemd_env(CmdSpec::new("systemctl", args))
}

/// Builds `systemctl [--user] list-unit-files ...`.
pub(crate) fn list_unit_files_cmd(scope: Scope) -> CmdSpec {
    let mut args = Vec::new();
    if scope == Scope::User {
        args.push("--user");
    }
    args.extend([
        "list-unit-files",
        "--type=service",
        "--no-pager",
        "--no-legend",
        "--plain",
        "--full",
    ]);
    with_systemd_env(CmdSpec::new("systemctl", args))
}

/// Builds `systemctl [--user] show ... -- <name>`.
pub(crate) fn show_cmd(name: &str, scope: Scope) -> CmdSpec {
    let mut args = Vec::new();
    if scope == Scope::User {
        args.push("--user".to_owned());
    }
    args.push("show".to_owned());
    args.push("--no-pager".to_owned());
    args.push(format!("--property={SHOW_PROPERTIES}"));
    args.push("--".to_owned());
    args.push(name.to_owned());
    with_systemd_env(CmdSpec::new("systemctl", args))
}

/// Builds `[sudo -n] systemctl [--user] <start|stop|restart> --no-pager -- <name>`.
pub(crate) fn control_cmd(name: &str, scope: Scope, action: Action, use_sudo: bool) -> CmdSpec {
    let mut args = Vec::new();
    if scope == Scope::User {
        args.push("--user".to_owned());
    }
    args.push(action.as_str().to_owned());
    args.push("--no-pager".to_owned());
    args.push("--".to_owned());
    args.push(name.to_owned());
    let cmd = with_systemd_env(CmdSpec::new("systemctl", args));
    if scope == Scope::System && use_sudo {
        cmd.with_sudo_n()
    } else {
        cmd
    }
}

/// Builds `journalctl [--user] --no-pager --output=short-iso --utc --unit=<name> -n <n> [--since=@<ts>]`.
pub(crate) fn logs_cmd(
    name: &str,
    scope: Scope,
    lines: usize,
    since: Option<Timestamp>,
) -> CmdSpec {
    let mut args = Vec::new();
    if scope == Scope::User {
        args.push("--user".to_owned());
    }
    args.push("--no-pager".to_owned());
    args.push("--output=short-iso".to_owned());
    args.push("--utc".to_owned());
    args.push(format!("--unit={name}"));
    args.push("-n".to_owned());
    args.push(lines.to_string());
    if let Some(ts) = since {
        args.push(format!("--since=@{}", ts.as_second()));
    }
    with_systemd_env(CmdSpec::new("journalctl", args))
}

fn with_systemd_env(cmd: CmdSpec) -> CmdSpec {
    cmd.with_env("TZ", "UTC")
        .with_env("SYSTEMD_PAGER", "")
        .with_env("SYSTEMD_COLORS", "0")
}

/// One parsed row from `systemctl list-unit-files`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnitFileEntry {
    pub(crate) name: String,
    pub(crate) unit_file_state: String,
}

/// Parses `systemctl list-units --type=service --all --no-pager --no-legend --plain --full`.
pub(crate) fn parse_list_units(text: &str, scope: Scope) -> Vec<ServiceSummary> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Tolerate a leading status bullet if an older systemd version or caller emits one.
        let line = line
            .strip_prefix('\u{25cf}')
            .or_else(|| line.strip_prefix('\u{00d7}'))
            .or_else(|| line.strip_prefix('*'))
            .map_or(line, str::trim_start);

        let mut parts = line.split_whitespace();
        let (Some(name), Some(load), Some(active), Some(_sub)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // Skip phantom references to unit files that do not exist when they are not active.
        if load == "not-found" && active == "inactive" {
            continue;
        }
        let desc_tokens: Vec<&str> = parts.collect();
        let description = if desc_tokens.is_empty() {
            None
        } else {
            Some(desc_tokens.join(" "))
        };
        let enabled = if load == "masked" { Some(false) } else { None };
        out.push(ServiceSummary {
            name: name.to_owned(),
            scope,
            state: map_active_state(active),
            enabled,
            description,
        });
    }
    out
}

/// Parses `systemctl list-unit-files --type=service --no-pager --no-legend --plain --full`.
pub(crate) fn parse_list_unit_files(text: &str) -> Vec<UnitFileEntry> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        // Template definitions (`foo@.service`) cannot be started without an instance name.
        if name.ends_with("@.service") {
            continue;
        }
        out.push(UnitFileEntry {
            name: name.to_owned(),
            unit_file_state: state.to_owned(),
        });
    }
    out
}

/// Merges loaded units with installed unit files so stopped disabled services and `enabled`
/// flags appear in `svc.list`.
pub(crate) fn merge_list(
    units: Vec<ServiceSummary>,
    unit_files: Vec<UnitFileEntry>,
    scope: Scope,
) -> Vec<ServiceSummary> {
    let mut by_name: BTreeMap<String, ServiceSummary> = BTreeMap::new();
    for unit in units {
        by_name.insert(unit.name.clone(), unit);
    }
    for file in unit_files {
        let enabled = map_unit_file_state(&file.unit_file_state);
        by_name
            .entry(file.name.clone())
            .and_modify(|existing| {
                if existing.enabled.is_none() {
                    existing.enabled = enabled;
                }
            })
            .or_insert_with(|| ServiceSummary {
                name: file.name,
                scope,
                state: ServiceState::Stopped,
                enabled,
                description: None,
            });
    }
    by_name.into_values().collect()
}

/// Parses `systemctl show` key=value output into a [`BackendStatus`].
///
/// Fails with `conflict` if `systemctl show` emitted more than one unit block (which would happen
/// if a pattern ever slipped past validation).
pub(crate) fn parse_show(
    text: &str,
    requested_name: &str,
    scope: Scope,
) -> Result<BackendStatus, ErrorBody> {
    let mut blocks: Vec<BTreeMap<String, String>> = Vec::new();
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            // A repeated `Id=` without an intervening blank line also marks a second unit.
            if k == "Id" && current.contains_key("Id") {
                blocks.push(std::mem::take(&mut current));
            }
            current.insert(k.to_owned(), v.to_owned());
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    if blocks.len() > 1 {
        let matched: Vec<&str> = blocks
            .iter()
            .filter_map(|block| block.get("Id").map(String::as_str))
            .collect();
        return Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Multiple systemd units matched {requested_name:?} ({}).",
                matched.join(", ")
            ),
        )
        .with_hint("Give a single exact unit name without wildcards.")
        .with_details(json!({"matches": matched})));
    }

    let props = blocks.into_iter().next().unwrap_or_default();

    let load_state = props.get("LoadState").map(String::as_str).unwrap_or("");
    let active_state = props.get("ActiveState").map(String::as_str).unwrap_or("");
    let sub_state = props.get("SubState").map(String::as_str).unwrap_or("");
    let unit_file_state = props.get("UnitFileState").map(String::as_str).unwrap_or("");

    if props.is_empty()
        || (load_state == "not-found" && active_state == "inactive" && unit_file_state.is_empty())
    {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "Service {requested_name:?} was not found in the {} scope.",
                scope.as_str()
            ),
        )
        .with_hint("Run `svc.list` to see the services on this machine."));
    }

    let id = props
        .get("Id")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| requested_name.to_owned());

    let state = map_active_state(active_state);
    let enabled = map_unit_file_state(unit_file_state).or_else(|| {
        if load_state == "masked" {
            Some(false)
        } else {
            None
        }
    });

    let pid = props
        .get("MainPID")
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&pid| pid > 0);

    let since = if state == ServiceState::Running {
        props
            .get("ActiveEnterTimestamp")
            .and_then(|raw| parse_systemd_timestamp(raw))
    } else {
        None
    };

    let description = props.get("Description").filter(|s| !s.is_empty()).cloned();

    let exec_code = props
        .get("ExecMainCode")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);
    let exec_status = props
        .get("ExecMainStatus")
        .and_then(|v| v.parse::<i32>().ok());
    let exit_code = match exec_status {
        Some(status) if exec_code != 0 || status != 0 => Some(status),
        _ => None,
    };

    let mut detail = BTreeMap::new();
    for (prop_key, detail_key) in [
        ("LoadState", "load_state"),
        ("ActiveState", "active_state"),
        ("SubState", "sub_state"),
        ("UnitFileState", "unit_file_state"),
        ("Result", "result"),
        ("FragmentPath", "fragment_path"),
    ] {
        if let Some(val) = props.get(prop_key)
            && !val.is_empty()
        {
            detail.insert(detail_key.to_owned(), Value::String(val.clone()));
        }
    }
    if !sub_state.is_empty() {
        detail.insert("sub_state".to_owned(), Value::String(sub_state.to_owned()));
    }

    Ok(BackendStatus {
        name: id,
        scope,
        state,
        enabled,
        pid,
        since,
        description,
        exit_code,
        detail,
    })
}

/// Parses `journalctl` output lines, keeping up to `limit` newest lines.
pub(crate) fn parse_journal_lines(text: &str, limit: usize, stream_truncated: bool) -> BackendLogs {
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| {
            !line.is_empty() && *line != "-- No entries --" && *line != "-- Journal begins"
        })
        .filter(|line| !line.starts_with("-- Journal begins") && !line.starts_with("-- Boot "))
        .map(str::to_owned)
        .collect();

    let mut truncated = stream_truncated;
    if lines.len() > limit {
        truncated = true;
        let drop_count = lines.len() - limit;
        lines.drain(..drop_count);
    }
    BackendLogs { lines, truncated }
}

/// Converts a systemd timestamp such as `Mon 2026-11-23 22:59:06 UTC` to RFC 3339 UTC.
pub(crate) fn parse_systemd_timestamp(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "n/a" {
        return None;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    // Expect either `YYYY-MM-DD HH:MM:SS <TZ>` or `Day YYYY-MM-DD HH:MM:SS <TZ>`.
    let (date, time, tz) = match tokens.as_slice() {
        [date, time, tz] => (*date, *time, *tz),
        [_day, date, time, tz] => (*date, *time, *tz),
        _ => return None,
    };
    let iso = match tz {
        "UTC" | "GMT" | "Z" => format!("{date}T{time}Z"),
        offset if (offset.starts_with('+') || offset.starts_with('-')) && offset.len() == 5 => {
            format!("{date}T{time}{}:{}", &offset[..3], &offset[3..])
        }
        offset if (offset.starts_with('+') || offset.starts_with('-')) && offset.len() == 6 => {
            format!("{date}T{time}{offset}")
        }
        _ => return None,
    };
    let ts: Timestamp = iso.parse().ok()?;
    Some(ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn map_active_state(active: &str) -> ServiceState {
    match active {
        "active" => ServiceState::Running,
        "inactive" => ServiceState::Stopped,
        "failed" => ServiceState::Failed,
        _ => ServiceState::Other,
    }
}

fn map_unit_file_state(state: &str) -> Option<bool> {
    match state {
        "enabled" | "enabled-runtime" | "alias" => Some(true),
        "disabled" | "masked" | "masked-runtime" => Some(false),
        _ => None,
    }
}

/// Whether `stderr` indicates no systemd user manager is reachable for this session.
pub(crate) fn is_missing_user_manager(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("failed to connect to bus")
        || lower.contains("failed to connect to user scope bus")
        || lower.contains("no medium found")
        || lower.contains("xdg_runtime_dir")
        || lower.contains("dbus_session_bus_address")
}

/// Classifies a non-zero `systemctl` exit into the closed `legion-proto` error codes.
pub(crate) fn classify_error(
    stderr: &str,
    scope: Scope,
    control: Option<(Action, &str)>,
    use_sudo: bool,
    what: &str,
) -> ErrorBody {
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();

    if scope == Scope::User && is_missing_user_manager(trimmed) {
        return ErrorBody::new(
            ErrorCode::Failed,
            "No systemd user manager is running for this session.",
        )
        .with_hint(
            "Start a user systemd session (ensure XDG_RUNTIME_DIR is set or run `loginctl enable-linger`) or pass `scope: \"system\"`.",
        )
        .with_details(json!({"stderr": trimmed}));
    }

    if lower.contains("system has not been booted with systemd") || lower.contains("can't operate")
    {
        return ErrorBody::new(
            ErrorCode::Unsupported,
            "systemd is not running as PID 1 on this machine.",
        )
        .with_hint("This machine does not appear to be booted with systemd.");
    }

    if let Some((action, name)) = control {
        let perm_denied = is_sudo_denied(trimmed)
            || lower.contains("interactive authentication required")
            || lower.contains("access denied")
            || lower.contains("permission denied")
            || lower.contains("authentication is required")
            || lower.contains("not authorized");
        if scope == Scope::System && perm_denied {
            return unix_privilege_denied(action, name, use_sudo, trimmed);
        }
        if lower.contains("not found")
            || lower.contains("not loaded")
            || lower.contains("could not be found")
        {
            return ErrorBody::new(
                ErrorCode::NotFound,
                format!(
                    "Service {name:?} was not found in the {} scope.",
                    scope.as_str()
                ),
            )
            .with_hint("Run `svc.list` to see the services on this machine.");
        }
        if lower.contains("is masked") {
            return ErrorBody::new(
                ErrorCode::Failed,
                format!("Service {name:?} is masked and cannot be started."),
            )
            .with_hint("Unmask the unit with `systemctl unmask` before starting it.")
            .with_details(json!({"stderr": trimmed}));
        }
    }

    let detail = if trimmed.is_empty() {
        format!("systemctl failed while trying to {what}.")
    } else {
        format!("systemctl failed while trying to {what}: {trimmed}")
    };
    ErrorBody::new(ErrorCode::Failed, detail)
}

fn classify_journal_error(stderr: &str, scope: Scope, name: &str) -> ErrorBody {
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("insufficient permissions") || lower.contains("permission denied") {
        return ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Permission denied reading journal logs for {} service {name:?}.",
                scope.as_str()
            ),
        )
        .with_hint(
            "Add the account legion runs as to the `systemd-journal` (or `adm`) group to read system service logs.",
        )
        .with_details(json!({"stderr": trimmed}));
    }
    if scope == Scope::User && is_missing_user_manager(trimmed) {
        return ErrorBody::new(
            ErrorCode::Failed,
            "No systemd user manager journal is available for this session.",
        )
        .with_hint("Ensure a user systemd session is active or pass `scope: \"system\"`.");
    }
    ErrorBody::new(
        ErrorCode::Failed,
        if trimmed.is_empty() {
            format!("journalctl failed while reading logs for {name:?}.")
        } else {
            format!("journalctl failed while reading logs for {name:?}: {trimmed}")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_respect_scope_and_sudo_rule() {
        let user_list = list_units_cmd(Scope::User);
        assert_eq!(user_list.program, "systemctl");
        assert_eq!(user_list.args[0], "--user");
        assert!(user_list.args.contains(&"list-units".to_owned()));

        let sys_show = show_cmd("nginx.service", Scope::System);
        assert_eq!(sys_show.program, "systemctl");
        assert!(!sys_show.args.contains(&"--user".to_owned()));
        assert_eq!(
            &sys_show.args[sys_show.args.len() - 2..],
            ["--", "nginx.service"]
        );

        let no_sudo = control_cmd("nginx.service", Scope::System, Action::Restart, false);
        assert_eq!(no_sudo.program, "systemctl");
        assert_eq!(
            no_sudo.args,
            ["restart", "--no-pager", "--", "nginx.service"]
        );

        let with_sudo = control_cmd("nginx.service", Scope::System, Action::Restart, true);
        assert_eq!(with_sudo.program, "sudo");
        assert_eq!(
            with_sudo.args,
            [
                "-n",
                "systemctl",
                "restart",
                "--no-pager",
                "--",
                "nginx.service"
            ]
        );

        // User scope never uses sudo even if the flag is true.
        let user_ctrl = control_cmd("worker.service", Scope::User, Action::Start, true);
        assert_eq!(user_ctrl.program, "systemctl");
        assert_eq!(
            user_ctrl.args,
            ["--user", "start", "--no-pager", "--", "worker.service"]
        );

        let ts: Timestamp = "2026-11-23T22:00:00Z".parse().unwrap();
        let logs = logs_cmd("app@1.service", Scope::User, 51, Some(ts));
        assert_eq!(logs.program, "journalctl");
        assert!(logs.args.contains(&"--user".to_owned()));
        assert!(logs.args.contains(&"--unit=app@1.service".to_owned()));
        assert!(logs.args.contains(&format!("--since=@{}", ts.as_second())));
    }

    #[test]
    fn parse_list_units_handles_escapes_at_signs_failed_masked_and_not_found() {
        let canned = "\
app-gnome\\x2dkeyring@autostart.service loaded inactive dead Certificate and Key Storage
dbus-broker.service                    loaded active   running D-Bus User Message Bus
worker@42.service                      loaded failed   failed  Background worker 42
legacy.service                         masked inactive dead    legacy.service
ghost.service                          not-found inactive dead ghost.service
* bulleted.service                     loaded active   running Unit with leading marker
";
        let units = parse_list_units(canned, Scope::User);
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "app-gnome\\x2dkeyring@autostart.service",
                "dbus-broker.service",
                "worker@42.service",
                "legacy.service",
                "bulleted.service",
            ]
        );
        assert_eq!(units[0].state, ServiceState::Stopped);
        assert_eq!(units[1].state, ServiceState::Running);
        assert_eq!(units[2].state, ServiceState::Failed);
        assert_eq!(units[3].state, ServiceState::Stopped);
        assert_eq!(units[3].enabled, Some(false));
        assert_eq!(
            units[4].description.as_deref(),
            Some("Unit with leading marker")
        );
    }

    #[test]
    fn merge_list_combines_unit_files_and_skips_templates() {
        let units = parse_list_units(
            "dbus-broker.service loaded active running D-Bus User Message Bus\n",
            Scope::User,
        );
        let files = parse_list_unit_files(
            "\
dbus-broker.service   enabled  enabled
offline-job.service   disabled disabled
getty@.service        static   -
",
        );
        let merged = merge_list(units, files, Scope::User);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "dbus-broker.service");
        assert_eq!(merged[0].state, ServiceState::Running);
        assert_eq!(merged[0].enabled, Some(true));
        assert_eq!(merged[1].name, "offline-job.service");
        assert_eq!(merged[1].state, ServiceState::Stopped);
        assert_eq!(merged[1].enabled, Some(false));
    }

    #[test]
    fn parse_show_handles_running_failed_masked_and_not_found() {
        let running = "\
Id=worker@1.service
Description=Worker instance 1
LoadState=loaded
ActiveState=active
SubState=running
UnitFileState=enabled
MainPID=4242
ActiveEnterTimestamp=Mon 2026-11-23 22:59:06 UTC
ExecMainCode=0
ExecMainStatus=0
Result=success
FragmentPath=/etc/systemd/system/worker@.service
";
        let st = parse_show(running, "worker@1", Scope::System).unwrap();
        assert_eq!(st.name, "worker@1.service");
        assert_eq!(st.state, ServiceState::Running);
        assert_eq!(st.enabled, Some(true));
        assert_eq!(st.pid, Some(4242));
        assert_eq!(st.since.as_deref(), Some("2026-11-23T22:59:06Z"));
        assert_eq!(st.exit_code, None);
        assert_eq!(st.description.as_deref(), Some("Worker instance 1"));

        let failed = "\
Id=broken.service
Description=Broken service
LoadState=loaded
ActiveState=failed
SubState=failed
UnitFileState=disabled
MainPID=0
ActiveEnterTimestamp=
ExecMainCode=1
ExecMainStatus=127
Result=exit-code
";
        let st = parse_show(failed, "broken.service", Scope::User).unwrap();
        assert_eq!(st.state, ServiceState::Failed);
        assert_eq!(st.enabled, Some(false));
        assert_eq!(st.pid, None);
        assert_eq!(st.exit_code, Some(127));
        assert_eq!(st.detail.get("result"), Some(&json!("exit-code")));

        let masked = "\
Id=masked.service
Description=masked.service
LoadState=masked
ActiveState=inactive
SubState=dead
UnitFileState=masked
MainPID=0
ExecMainCode=0
ExecMainStatus=0
";
        let st = parse_show(masked, "masked.service", Scope::System).unwrap();
        assert_eq!(st.state, ServiceState::Stopped);
        assert_eq!(st.enabled, Some(false));
        assert_eq!(st.detail.get("load_state"), Some(&json!("masked")));

        let not_found = "\
Id=ghost.service
LoadState=not-found
ActiveState=inactive
SubState=dead
UnitFileState=
MainPID=0
";
        let err = parse_show(not_found, "ghost.service", Scope::User).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);

        let multi = "\
Id=legion-a.service
LoadState=loaded
ActiveState=active
SubState=running

Id=legion-b.service
LoadState=loaded
ActiveState=active
SubState=running
";
        let err = parse_show(multi, "legion-*.service", Scope::User).unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert!(err.message.contains("legion-a.service"), "{}", err.message);
        assert!(err.message.contains("legion-b.service"), "{}", err.message);
    }

    #[test]
    fn parse_timestamp_rejects_localised_names_and_accepts_c_locale_utc() {
        assert_eq!(
            parse_systemd_timestamp("Mon 2026-11-23 22:59:06 UTC").as_deref(),
            Some("2026-11-23T22:59:06Z")
        );
        assert_eq!(
            parse_systemd_timestamp("2026-11-23 23:59:06 +0100").as_deref(),
            Some("2026-11-23T22:59:06Z")
        );
        // Non-UTC localised timezone abbreviation (which C locale + TZ=UTC prevents) is ignored
        // rather than emitting an invalid RFC 3339 string.
        assert_eq!(parse_systemd_timestamp("Mo 2026-11-23 23:59:06 MEZ"), None);
        assert_eq!(parse_systemd_timestamp("n/a"), None);
    }

    #[test]
    fn classify_error_detects_missing_user_manager_and_privilege_refusals() {
        let missing = classify_error(
            "Failed to connect to bus: No medium found\n",
            Scope::User,
            None,
            false,
            "list services",
        );
        assert_eq!(missing.code, ErrorCode::Failed);
        assert!(missing.message.contains("user manager"));

        let denied = classify_error(
            "Failed to restart nginx.service: Interactive authentication required.\n",
            Scope::System,
            Some((Action::Restart, "nginx.service")),
            false,
            "restart service",
        );
        assert_eq!(denied.code, ErrorCode::Denied);
        assert!(
            denied
                .hint
                .as_deref()
                .unwrap_or("")
                .contains("[svc] sudo = true")
        );

        let sudo_denied = classify_error(
            "sudo: a password is required\n",
            Scope::System,
            Some((Action::Stop, "nginx.service")),
            true,
            "stop service",
        );
        assert_eq!(sudo_denied.code, ErrorCode::Denied);
        assert!(
            sudo_denied
                .hint
                .as_deref()
                .unwrap_or("")
                .contains("NOPASSWD")
        );
    }

    #[test]
    fn parse_journal_lines_handles_no_entries_and_truncation() {
        let empty = parse_journal_lines("-- No entries --\n", 10, false);
        assert!(empty.lines.is_empty());
        assert!(!empty.truncated);

        let sample = "line 1\nline 2\nline 3\n";
        let cut = parse_journal_lines(sample, 2, false);
        assert_eq!(cut.lines, ["line 2", "line 3"]);
        assert!(cut.truncated);
    }
}
