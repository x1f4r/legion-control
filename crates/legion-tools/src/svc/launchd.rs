//! macOS `launchd` backend: command builders and pure parsers for `launchctl`.
//!
//! User services live in the `gui/<uid>` domain; system services live in `system`. Because macOS
//! unified logging (`log show`) is not tied to a single per-service file, `svc.logs` reads the
//! tail of `StandardOutPath` / `StandardErrorPath` when the service's plist declares them and
//! returns `unsupported` with a `log show` hint otherwise.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_core::{BoxFuture, CancellationToken, Ctx};
use serde_json::{Value, json};

use super::backend::{
    ACTION_TIMEOUT, Backend, BackendLogs, BackendStatus, CmdSpec, QUERY_CAPTURE_LIMIT,
    QUERY_TIMEOUT, is_sudo_denied, run_cmd, unix_privilege_denied,
};
use super::{Action, BackendKind, Scope, ServiceState, ServiceSummary};

/// Maximum bytes read from the tail of a launchd log file so huge logs never exhaust memory.
const MAX_PLIST_LOG_READ_BYTES: usize = 2 * 1024 * 1024;

/// macOS launchd service manager backend.
pub(crate) struct Launchd;

impl Backend for Launchd {
    fn kind(&self) -> BackendKind {
        BackendKind::Launchd
    }

    fn list<'a>(
        &'a self,
        ctx: &'a Ctx,
        scope: Scope,
    ) -> BoxFuture<'a, Result<Vec<ServiceSummary>, ErrorBody>> {
        Box::pin(async move {
            match scope {
                Scope::User => {
                    let cmd = list_user_cmd();
                    let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
                    if !out.success() {
                        return Err(classify_error(
                            &out.stderr.text,
                            scope,
                            None,
                            false,
                            "list user services",
                        ));
                    }
                    Ok(parse_list(&out.stdout.text, Scope::User))
                }
                Scope::System => {
                    let cmd = print_domain_cmd("system");
                    let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
                    if !out.success() {
                        return Err(classify_error(
                            &out.stderr.text,
                            scope,
                            None,
                            false,
                            "list system services",
                        ));
                    }
                    Ok(parse_print_domain_services(&out.stdout.text, Scope::System))
                }
            }
        })
    }

    fn status<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
    ) -> BoxFuture<'a, Result<BackendStatus, ErrorBody>> {
        Box::pin(async move {
            let domain = resolve_domain(ctx, scope).await?;
            let cmd = print_service_cmd(&domain, name);
            let out = run_cmd(ctx, &cmd, QUERY_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
            if !out.success() {
                let err = classify_print_error(&out.stderr.text, name, scope);
                if err.code == ErrorCode::NotFound {
                    let dirs = plist_search_dirs(scope, ctx.paths().home());
                    if let Some(plist) = find_plist_in_dirs(name, &dirs) {
                        return Ok(unloaded_status(name, scope, &domain, &plist));
                    }
                }
                return Err(err);
            }
            parse_print(&out.stdout.text, name, scope, &domain)
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
            let domain = resolve_domain(ctx, scope).await?;
            let current = self.status(ctx, name, scope).await.ok();
            let loaded = current
                .as_ref()
                .and_then(|st| st.detail.get("loaded").and_then(Value::as_bool))
                .unwrap_or(true);
            let plist_path = current
                .as_ref()
                .and_then(|st| st.detail.get("path").and_then(Value::as_str))
                .map(str::to_owned);
            let keep_alive = current
                .as_ref()
                .and_then(|st| st.detail.get("keep_alive").and_then(Value::as_bool))
                .unwrap_or(false);

            match action {
                Action::Start | Action::Restart => {
                    if !loaded {
                        let Some(plist) = plist_path.as_deref() else {
                            return Err(ErrorBody::new(
                                ErrorCode::NotFound,
                                format!(
                                    "launchd service {name:?} is not loaded and no plist was found in the {} scope.",
                                    scope.as_str()
                                ),
                            )
                            .with_hint(
                                "Place the service's plist in LaunchAgents (user) or LaunchDaemons (system) before starting it.",
                            ));
                        };
                        let boot = bootstrap_cmd(&domain, plist, scope, use_sudo);
                        let out = run_cmd(ctx, &boot, ACTION_TIMEOUT, QUERY_CAPTURE_LIMIT).await?;
                        if !out.success() {
                            return Err(classify_error(
                                &out.stderr.text,
                                scope,
                                Some((action, name)),
                                use_sudo,
                                &format!("bootstrap service {name:?}"),
                            ));
                        }
                    }
                    // `kickstart` without `-k` starts a newly bootstrapped job that lacks
                    // `RunAtLoad` without killing one that already started on bootstrap;
                    // `restart` always passes `-k` for a fresh instance.
                    let kill_existing = action == Action::Restart;
                    let cmd = kickstart_cmd(&domain, name, kill_existing, scope, use_sudo);
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
                }
                Action::Stop => {
                    if !loaded {
                        return Ok(());
                    }
                    // KeepAlive jobs (unconditional or conditional via semaphores) respawn
                    // immediately on `kill SIGTERM`, so stop must unload them with `bootout`.
                    let cmd = if keep_alive {
                        bootout_cmd(&domain, name, scope, use_sudo)
                    } else {
                        kill_cmd(&domain, name, scope, use_sudo)
                    };
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
                }
            }
        })
    }

    fn logs<'a>(
        &'a self,
        ctx: &'a Ctx,
        name: &'a str,
        scope: Scope,
        lines: usize,
        _since: Option<Timestamp>,
    ) -> BoxFuture<'a, Result<BackendLogs, ErrorBody>> {
        Box::pin(async move {
            let status = self.status(ctx, name, scope).await?;
            let mut paths = Vec::new();
            for key in ["stdout_path", "stderr_path"] {
                if let Some(p) = status.detail.get(key).and_then(Value::as_str)
                    && !p.is_empty()
                    && !paths.iter().any(|existing: &String| existing == p)
                {
                    paths.push(p.to_owned());
                }
            }
            if paths.is_empty()
                && let Some(plist_path) = status.detail.get("path").and_then(Value::as_str)
                && let Ok(plist_text) = std::fs::read_to_string(plist_path)
            {
                for p in extract_plist_log_paths(&plist_text) {
                    if !paths.iter().any(|existing| existing == &p) {
                        paths.push(p);
                    }
                }
            }

            if paths.is_empty() {
                return Err(unsupported_logs_error(name));
            }

            let cancel = ctx.cancel_token().clone();
            let collected =
                tokio::task::spawn_blocking(move || read_log_tails(&paths, lines, &cancel))
                    .await
                    .map_err(|error| {
                        ErrorBody::new(
                            ErrorCode::Internal,
                            format!("Reading launchd log files stopped unexpectedly: {error}."),
                        )
                    })??;
            Ok(collected)
        })
    }
}

/// Resolves `gui/<uid>` (via `id -u`) for user scope, or `"system"` for system scope.
async fn resolve_domain(ctx: &Ctx, scope: Scope) -> Result<String, ErrorBody> {
    match scope {
        Scope::System => Ok("system".to_owned()),
        Scope::User => {
            let out = run_cmd(ctx, &uid_cmd(), QUERY_TIMEOUT, 1024).await?;
            if !out.success() {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Could not determine the current user id for launchd: {}",
                        out.stderr.text.trim()
                    ),
                ));
            }
            let uid = parse_uid(&out.stdout.text)?;
            Ok(format!("gui/{uid}"))
        }
    }
}

/// Builds `id -u` to obtain the numeric user id without platform-specific `cfg` code.
pub(crate) fn uid_cmd() -> CmdSpec {
    CmdSpec::new("id", ["-u"])
}

/// Parses numeric UID output from `id -u`.
pub(crate) fn parse_uid(text: &str) -> Result<u32, ErrorBody> {
    text.trim().parse::<u32>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Unexpected output from `id -u`: {:?}", text.trim()),
        )
    })
}

/// Builds `launchctl list` for the current user's GUI domain.
pub(crate) fn list_user_cmd() -> CmdSpec {
    CmdSpec::new("launchctl", ["list"])
}

/// Builds `launchctl print <domain>` (used for `system`).
pub(crate) fn print_domain_cmd(domain: &str) -> CmdSpec {
    CmdSpec::new("launchctl", ["print", domain])
}

/// Builds `launchctl print <domain>/<label>`.
pub(crate) fn print_service_cmd(domain: &str, label: &str) -> CmdSpec {
    CmdSpec::new("launchctl", ["print", &format!("{domain}/{label}")])
}

/// Builds `[sudo -n] launchctl kickstart [-k] <domain>/<label>`.
pub(crate) fn kickstart_cmd(
    domain: &str,
    label: &str,
    kill_existing: bool,
    scope: Scope,
    use_sudo: bool,
) -> CmdSpec {
    let target = format!("{domain}/{label}");
    let args: Vec<String> = if kill_existing {
        vec!["kickstart".to_owned(), "-k".to_owned(), target]
    } else {
        vec!["kickstart".to_owned(), target]
    };
    let cmd = CmdSpec::new("launchctl", args);
    if scope == Scope::System && use_sudo {
        cmd.with_sudo_n()
    } else {
        cmd
    }
}

/// Builds `[sudo -n] launchctl kill SIGTERM <domain>/<label>`.
pub(crate) fn kill_cmd(domain: &str, label: &str, scope: Scope, use_sudo: bool) -> CmdSpec {
    let cmd = CmdSpec::new(
        "launchctl",
        ["kill", "SIGTERM", &format!("{domain}/{label}")],
    );
    if scope == Scope::System && use_sudo {
        cmd.with_sudo_n()
    } else {
        cmd
    }
}

/// Builds `[sudo -n] launchctl bootout <domain>/<label>`.
pub(crate) fn bootout_cmd(domain: &str, label: &str, scope: Scope, use_sudo: bool) -> CmdSpec {
    let cmd = CmdSpec::new("launchctl", ["bootout", &format!("{domain}/{label}")]);
    if scope == Scope::System && use_sudo {
        cmd.with_sudo_n()
    } else {
        cmd
    }
}

/// Builds `[sudo -n] launchctl bootstrap <domain> <plist_path>`.
pub(crate) fn bootstrap_cmd(
    domain: &str,
    plist_path: &str,
    scope: Scope,
    use_sudo: bool,
) -> CmdSpec {
    let cmd = CmdSpec::new("launchctl", ["bootstrap", domain, plist_path]);
    if scope == Scope::System && use_sudo {
        cmd.with_sudo_n()
    } else {
        cmd
    }
}

/// Standard launchd plist search directories for `scope`, in priority order.
pub(crate) fn plist_search_dirs(scope: Scope, home: Option<&Path>) -> Vec<PathBuf> {
    match scope {
        Scope::User => {
            let mut dirs = Vec::new();
            if let Some(home) = home {
                dirs.push(home.join("Library").join("LaunchAgents"));
            }
            dirs.push(PathBuf::from("/Library/LaunchAgents"));
            dirs.push(PathBuf::from("/System/Library/LaunchAgents"));
            dirs
        }
        Scope::System => vec![
            PathBuf::from("/Library/LaunchDaemons"),
            PathBuf::from("/System/Library/LaunchDaemons"),
        ],
    }
}

/// Finds the plist file for `label` in `dirs`, checking `<label>.plist` first and falling back
/// to scanning XML `.plist` files for a matching `<key>Label</key>`.
pub(crate) fn find_plist_in_dirs(label: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let direct_name = format!("{label}.plist");
    for dir in dirs {
        let candidate = dir.join(&direct_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten().take(512) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("plist") || !path.is_file() {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path)
                && extract_plist_string(&text, "Label").as_deref() == Some(label)
            {
                return Some(path);
            }
        }
    }
    None
}

fn unloaded_status(label: &str, scope: Scope, domain: &str, plist: &Path) -> BackendStatus {
    let mut detail = BTreeMap::new();
    detail.insert("domain".to_owned(), Value::String(domain.to_owned()));
    detail.insert("loaded".to_owned(), Value::Bool(false));
    detail.insert(
        "path".to_owned(),
        Value::String(plist.to_string_lossy().into_owned()),
    );
    BackendStatus {
        name: label.to_owned(),
        scope,
        state: ServiceState::Stopped,
        enabled: None,
        pid: None,
        since: None,
        instance_id: None,
        oneshot_completed: false,
        description: None,
        exit_code: None,
        detail,
    }
}

/// Parses `launchctl list` tab-separated output (`PID\tStatus\tLabel`).
pub(crate) fn parse_list(text: &str, scope: Scope) -> Vec<ServiceSummary> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("PID") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(pid_str), Some(status_str), Some(label)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let pid = pid_str.parse::<u32>().ok().filter(|&p| p > 0);
        let status = status_str.parse::<i32>().ok();
        let state = state_from_pid_and_exit(pid, status);
        out.push(ServiceSummary {
            name: label.to_owned(),
            scope,
            state,
            enabled: None,
            description: None,
        });
    }
    out
}

/// Parses the `services = { ... }` section of `launchctl print system` (or `gui/<uid>`).
pub(crate) fn parse_print_domain_services(text: &str, scope: Scope) -> Vec<ServiceSummary> {
    let mut out = Vec::new();
    let mut in_services = false;
    for raw in text.lines() {
        let line = raw.trim();
        if !in_services {
            if line == "services = {" {
                in_services = true;
            }
            continue;
        }
        if line == "}" {
            break;
        }
        let mut parts = line.split_whitespace();
        let (Some(pid_str), Some(status_str), Some(label)) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let pid = pid_str.parse::<u32>().ok().filter(|&p| p > 0);
        let status = status_str.parse::<i32>().ok();
        out.push(ServiceSummary {
            name: label.to_owned(),
            scope,
            state: state_from_pid_and_exit(pid, status),
            enabled: None,
            description: None,
        });
    }
    out
}

/// Parses `launchctl print <domain>/<label>` output into a [`BackendStatus`].
pub(crate) fn parse_print(
    text: &str,
    label: &str,
    scope: Scope,
    domain: &str,
) -> Result<BackendStatus, ErrorBody> {
    if text.trim().is_empty() || text.contains("Could not find service") {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("Service {label:?} was not found in launchd domain {domain}."),
        )
        .with_hint("Run `svc.list` to see the services loaded on this machine."));
    }

    let mut raw_state: Option<String> = None;
    let mut pid: Option<u32> = None;
    let mut exit_code: Option<i32> = None;
    let mut program: Option<String> = None;
    let mut path: Option<String> = None;
    let mut runs: Option<u64> = None;
    let mut stdout_path: Option<String> = None;
    let mut stderr_path: Option<String> = None;
    let mut svc_type: Option<String> = None;
    let mut properties: Option<String> = None;
    let mut keep_alive = false;
    let mut in_semaphores = false;

    // Only parse top-level key = value lines inside the service block (depth == 1) so nested
    // blocks like `environment = { ... }` cannot shadow top-level fields. Also verify the header
    // line names the exact requested label, and detect conditional KeepAlive inside
    // `semaphores = { ... }`.
    let mut depth: usize = 0;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.ends_with('{') {
            if depth == 0
                && let Some((header, _)) = line.split_once('=')
            {
                let expected_header = format!("{domain}/{label}");
                let actual = header.trim();
                if actual != expected_header && actual != label {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("launchd returned {actual:?} instead of exact label {label:?}."),
                    )
                    .with_hint("Give the exact launchd service label as shown by `svc.list`."));
                }
            }
            if depth == 1 {
                if line.starts_with("keepalive") {
                    keep_alive = true;
                } else if line.starts_with("semaphores") {
                    in_semaphores = true;
                }
            }
            depth += 1;
            continue;
        }
        if line == "}" {
            depth = depth.saturating_sub(1);
            if depth <= 1 {
                in_semaphores = false;
            }
            continue;
        }
        if in_semaphores && depth == 2 {
            // Any entry inside `semaphores = { ... }` (such as `successful exit => 0` or
            // `crashed => 1`) represents a conditional KeepAlive policy.
            keep_alive = true;
            continue;
        }
        if depth != 1 {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "state" => raw_state = Some(val.to_owned()),
            "pid" => pid = val.parse::<u32>().ok().filter(|&p| p > 0),
            "last exit code" => {
                // launchctl prints e.g. `0`, `78`, `15: Terminated`, or `(never exited)`.
                let code_part = val.split(':').next().unwrap_or(val).trim();
                exit_code = code_part.parse::<i32>().ok();
            }
            "program" => program = Some(val.to_owned()),
            "path" => path = Some(val.to_owned()),
            "runs" => runs = val.parse::<u64>().ok(),
            "stdout path" => stdout_path = Some(val.to_owned()),
            "stderr path" => stderr_path = Some(val.to_owned()),
            "type" => svc_type = Some(val.to_owned()),
            "properties" => {
                properties = Some(val.to_owned());
                if val
                    .split('|')
                    .any(|flag| flag.trim().eq_ignore_ascii_case("keepalive"))
                {
                    keep_alive = true;
                }
            }
            "keep alive" | "keepalive" => {
                keep_alive = val == "true" || val == "1";
            }
            _ => {}
        }
    }

    let state = match raw_state.as_deref() {
        Some("running") => ServiceState::Running,
        Some("not running" | "waiting" | "stopped") => match exit_code {
            Some(code) if code != 0 && code != 15 && code != -15 => ServiceState::Failed,
            _ => ServiceState::Stopped,
        },
        Some(_) => ServiceState::Other,
        None => state_from_pid_and_exit(pid, exit_code),
    };

    let mut detail = BTreeMap::new();
    detail.insert("domain".to_owned(), Value::String(domain.to_owned()));
    detail.insert("loaded".to_owned(), Value::Bool(true));
    if let Some(s) = raw_state {
        detail.insert("launchd_state".to_owned(), Value::String(s));
    }
    if let Some(p) = path {
        detail.insert("path".to_owned(), Value::String(p));
    }
    if let Some(t) = svc_type {
        detail.insert("type".to_owned(), Value::String(t));
    }
    if let Some(p) = properties {
        detail.insert("properties".to_owned(), Value::String(p));
    }
    if let Some(r) = runs {
        detail.insert("runs".to_owned(), json!(r));
    }
    if let Some(p) = stdout_path {
        detail.insert("stdout_path".to_owned(), Value::String(p));
    }
    if let Some(p) = stderr_path {
        detail.insert("stderr_path".to_owned(), Value::String(p));
    }
    if keep_alive {
        detail.insert("keep_alive".to_owned(), Value::Bool(true));
    }

    let oneshot_completed = !keep_alive
        && state == ServiceState::Stopped
        && exit_code == Some(0)
        && runs.is_some_and(|r| r > 0);

    Ok(BackendStatus {
        name: label.to_owned(),
        scope,
        state,
        enabled: None,
        pid,
        since: None,
        instance_id: runs.map(|r| format!("runs:{r}")),
        oneshot_completed,
        description: program,
        exit_code: exit_code.map(i64::from),
        detail,
    })
}

/// Extracts a `<string>` value for `key` from an XML `.plist` document.
fn extract_plist_string(plist_xml: &str, key: &str) -> Option<String> {
    let needle = format!("<key>{key}</key>");
    let idx = plist_xml.find(&needle)?;
    let after = &plist_xml[idx + needle.len()..];
    let start = after.find("<string>")?;
    let value_start = &after[start + "<string>".len()..];
    let end = value_start.find("</string>")?;
    let val = value_start[..end].trim();
    (!val.is_empty()).then(|| val.to_owned())
}

/// Extracts `StandardOutPath` and `StandardErrorPath` from an XML launchd `.plist` document.
pub(crate) fn extract_plist_log_paths(plist_xml: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for key in ["StandardOutPath", "StandardErrorPath"] {
        if let Some(path) = extract_plist_string(plist_xml, key)
            && !paths.iter().any(|p| p == &path)
        {
            paths.push(path);
        }
    }
    paths
}

fn read_log_tails(
    paths: &[String],
    limit: usize,
    cancel: &CancellationToken,
) -> Result<BackendLogs, ErrorBody> {
    let mut lines = Vec::new();
    let mut truncated = false;
    let mut read_any = false;
    for path_str in paths {
        if cancel.is_cancelled() {
            return Err(ErrorBody::new(
                ErrorCode::Cancelled,
                "The call to svc.logs was cancelled while reading launchd log files.",
            ));
        }
        let path = Path::new(path_str);
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        read_any = true;
        let mut file = std::fs::File::open(path).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Could not read log file {}: {error}.", path.display()),
            )
            .with_hint("Check that the account legion runs as has read access to the log file.")
        })?;
        let file_len = meta.len();
        let mut buf = Vec::new();
        if file_len > MAX_PLIST_LOG_READ_BYTES as u64 {
            use std::io::{Read, Seek, SeekFrom};
            let _ = file.seek(SeekFrom::End(-(MAX_PLIST_LOG_READ_BYTES as i64)));
            let _ = file.read_to_end(&mut buf);
            truncated = true;
        } else {
            use std::io::Read;
            let _ = file.read_to_end(&mut buf);
        }
        let text = String::from_utf8_lossy(&buf);
        let mut file_lines = text.lines();
        if file_len > MAX_PLIST_LOG_READ_BYTES as u64 {
            // Drop the partial first line after seeking into the middle of a large file.
            let _ = file_lines.next();
        }
        for line in file_lines {
            lines.push(line.trim_end_matches('\r').to_owned());
        }
    }

    if !read_any {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "The declared launchd log file(s) ({}) do not exist yet.",
                paths.join(", ")
            ),
        )
        .with_hint("Start the service or check macOS unified logs with `log show`."));
    }

    if lines.len() > limit {
        truncated = true;
        let drop_count = lines.len() - limit;
        lines.drain(..drop_count);
    }
    Ok(BackendLogs { lines, truncated })
}

fn unsupported_logs_error(name: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        format!(
            "launchd service {name:?} does not declare StandardOutPath or StandardErrorPath log files."
        ),
    )
    .with_hint(format!(
        "Use macOS unified logging instead: `log show --predicate 'subsystem == \"{name}\" OR process == \"{name}\"' --last 1h`."
    ))
}

fn state_from_pid_and_exit(pid: Option<u32>, exit_code: Option<i32>) -> ServiceState {
    if pid.is_some() {
        return ServiceState::Running;
    }
    match exit_code {
        None | Some(0) | Some(-15) | Some(15) => ServiceState::Stopped,
        Some(_) => ServiceState::Failed,
    }
}

fn classify_print_error(stderr: &str, name: &str, scope: Scope) -> ErrorBody {
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("could not find service") || lower.contains("not find service") {
        return ErrorBody::new(
            ErrorCode::NotFound,
            format!(
                "Service {name:?} was not found in the {} scope.",
                scope.as_str()
            ),
        )
        .with_hint("Run `svc.list` to see the services loaded on this machine.");
    }
    if lower.contains("could not find domain") {
        return ErrorBody::new(
            ErrorCode::Failed,
            format!("The launchd {} domain is not available in this session.", scope.as_str()),
        )
        .with_hint("A macOS GUI user session is required for user-scope services, or pass `scope: \"system\"`.");
    }
    classify_error(
        trimmed,
        scope,
        None,
        false,
        &format!("inspect service {name:?}"),
    )
}

pub(crate) fn classify_error(
    stderr: &str,
    scope: Scope,
    control: Option<(Action, &str)>,
    use_sudo: bool,
    what: &str,
) -> ErrorBody {
    let trimmed = stderr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some((action, name)) = control {
        let denied = is_sudo_denied(trimmed)
            || lower.contains("operation not permitted")
            || lower.contains("permission denied")
            || lower.contains("not privileged")
            || lower.contains("privileged");
        if scope == Scope::System && denied {
            return unix_privilege_denied(action, name, use_sudo, trimmed);
        }
        if lower.contains("could not find service") {
            return ErrorBody::new(
                ErrorCode::NotFound,
                format!(
                    "Service {name:?} was not found in the {} scope.",
                    scope.as_str()
                ),
            )
            .with_hint("Run `svc.list` to see the services loaded on this machine.");
        }
    }
    ErrorBody::new(
        ErrorCode::Failed,
        if trimmed.is_empty() {
            format!("launchctl failed while trying to {what}.")
        } else {
            format!("launchctl failed while trying to {what}: {trimmed}")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builders_use_gui_uid_domain_and_respect_sudo() {
        assert_eq!(parse_uid("501\n").unwrap(), 501);
        assert!(parse_uid("not-a-uid").is_err());

        let print = print_service_cmd("gui/501", "com.example.worker");
        assert_eq!(print.program, "launchctl");
        assert_eq!(print.args, ["print", "gui/501/com.example.worker"]);

        let restart_no_sudo =
            kickstart_cmd("system", "com.example.daemon", true, Scope::System, false);
        assert_eq!(restart_no_sudo.program, "launchctl");
        assert_eq!(
            restart_no_sudo.args,
            ["kickstart", "-k", "system/com.example.daemon"]
        );

        let restart_sudo = kickstart_cmd("system", "com.example.daemon", true, Scope::System, true);
        assert_eq!(restart_sudo.program, "sudo");
        assert_eq!(
            restart_sudo.args,
            [
                "-n",
                "launchctl",
                "kickstart",
                "-k",
                "system/com.example.daemon"
            ]
        );

        let stop = kill_cmd("gui/501", "com.example.worker", Scope::User, true);
        assert_eq!(stop.program, "launchctl");
        assert_eq!(stop.args, ["kill", "SIGTERM", "gui/501/com.example.worker"]);

        let bootout = bootout_cmd("system", "com.example.daemon", Scope::System, true);
        assert_eq!(bootout.program, "sudo");
        assert_eq!(
            bootout.args,
            ["-n", "launchctl", "bootout", "system/com.example.daemon"]
        );

        let bootstrap = bootstrap_cmd(
            "gui/501",
            "/Users/me/Library/LaunchAgents/com.example.worker.plist",
            Scope::User,
            false,
        );
        assert_eq!(
            bootstrap.args,
            [
                "bootstrap",
                "gui/501",
                "/Users/me/Library/LaunchAgents/com.example.worker.plist"
            ]
        );
    }

    #[test]
    fn parse_list_and_domain_services() {
        let list_out = "\
PID\tStatus\tLabel
4821\t0\tcom.example.worker
-\t0\tcom.example.idle
-\t78\tcom.example.broken
-\t-15\tcom.example.stopped
";
        let services = parse_list(list_out, Scope::User);
        assert_eq!(services.len(), 4);
        assert_eq!(services[0].name, "com.example.worker");
        assert_eq!(services[0].state, ServiceState::Running);
        assert_eq!(services[1].state, ServiceState::Stopped);
        assert_eq!(services[2].state, ServiceState::Failed);
        assert_eq!(services[3].state, ServiceState::Stopped);

        let domain_out = "\
system = {
    type = system
    services = {
             123      -       com.apple.syslogd
               0      0       com.example.daily
               0     78       com.example.failed
    }
}
";
        let sys = parse_print_domain_services(domain_out, Scope::System);
        assert_eq!(sys.len(), 3);
        assert_eq!(sys[0].name, "com.apple.syslogd");
        assert_eq!(sys[0].state, ServiceState::Running);
        assert_eq!(sys[1].state, ServiceState::Stopped);
        assert_eq!(sys[2].state, ServiceState::Failed);
    }

    #[test]
    fn parse_print_handles_running_stopped_and_not_loaded() {
        let running = "\
gui/501/com.example.worker = {
    active count = 1
    path = /Users/me/Library/LaunchAgents/com.example.worker.plist
    type = LaunchAgent
    state = running
    program = /usr/local/bin/worker
    stdout path = /Users/me/Library/Logs/worker.out.log
    stderr path = /Users/me/Library/Logs/worker.err.log
    environment = {
        state = ignored-nested
    }
    pid = 4821
    runs = 3
    last exit code = 0
}
";
        let st = parse_print(running, "com.example.worker", Scope::User, "gui/501").unwrap();
        assert_eq!(st.state, ServiceState::Running);
        assert_eq!(st.pid, Some(4821));
        assert_eq!(st.exit_code, Some(0));
        assert_eq!(st.description.as_deref(), Some("/usr/local/bin/worker"));
        assert_eq!(
            st.detail.get("stdout_path"),
            Some(&json!("/Users/me/Library/Logs/worker.out.log"))
        );

        let failed = "\
system/com.example.broken = {
    active count = 0
    type = LaunchDaemon
    state = not running
    last exit code = 78
}
";
        let st = parse_print(failed, "com.example.broken", Scope::System, "system").unwrap();
        assert_eq!(st.state, ServiceState::Failed);
        assert_eq!(st.pid, None);
        assert_eq!(st.exit_code, Some(78));

        let err = classify_print_error(
            "Could not find service \"com.example.missing\" in domain for user gui: 501\n",
            "com.example.missing",
            Scope::User,
        );
        assert_eq!(err.code, ErrorCode::NotFound);

        let mismatched =
            parse_print(running, "com.example.other", Scope::User, "gui/501").unwrap_err();
        assert_eq!(mismatched.code, ErrorCode::NotFound);

        let ondemand = "\
gui/501/com.example.task = {
    active count = 0
    type = LaunchAgent
    state = not running
    runs = 1
    last exit code = 0
}
";
        let st = parse_print(ondemand, "com.example.task", Scope::User, "gui/501").unwrap();
        assert_eq!(st.state, ServiceState::Stopped);
        assert!(st.oneshot_completed);
    }

    #[test]
    fn extract_plist_log_paths_finds_stdout_and_stderr() {
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.example.worker</string>
    <key>StandardOutPath</key>
    <string>/Users/me/Library/Logs/worker.out.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/me/Library/Logs/worker.err.log</string>
</dict>
</plist>"#;
        assert_eq!(
            extract_plist_log_paths(plist),
            [
                "/Users/me/Library/Logs/worker.out.log",
                "/Users/me/Library/Logs/worker.err.log",
            ]
        );
        assert!(extract_plist_log_paths("<plist><dict/></plist>").is_empty());
    }

    #[test]
    fn unsupported_logs_error_mentions_log_show() {
        let err = unsupported_logs_error("com.example.worker");
        assert_eq!(err.code, ErrorCode::Unsupported);
        assert!(err.hint.as_deref().unwrap_or("").contains("log show"));
    }

    #[test]
    fn read_log_tails_returns_last_lines_from_declared_files() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("worker.log");
        std::fs::write(&log_path, "first\nsecond\nthird\nfourth\n").unwrap();
        let cancel = CancellationToken::new();
        let out = read_log_tails(&[log_path.to_string_lossy().into_owned()], 2, &cancel).unwrap();
        assert!(out.truncated);
        assert_eq!(out.lines, ["third", "fourth"]);

        let missing = read_log_tails(
            &[dir
                .path()
                .join("missing.log")
                .to_string_lossy()
                .into_owned()],
            10,
            &cancel,
        )
        .unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
    }

    #[test]
    fn parse_print_detects_keepalive_from_properties_and_semaphores() {
        let via_properties = "\
gui/501/com.example.daemon = {
    type = LaunchAgent
    state = running
    pid = 100
    properties = keepalive | runatload | inferred program
}
";
        let st = parse_print(via_properties, "com.example.daemon", Scope::User, "gui/501").unwrap();
        assert_eq!(st.detail.get("keep_alive"), Some(&json!(true)));
        assert_eq!(st.detail.get("loaded"), Some(&json!(true)));

        let via_semaphores = "\
gui/501/com.example.cond = {
    type = LaunchAgent
    state = running
    pid = 101
    properties = runatload
    semaphores = {
        successful exit => 0
    }
}
";
        let st = parse_print(via_semaphores, "com.example.cond", Scope::User, "gui/501").unwrap();
        assert_eq!(st.detail.get("keep_alive"), Some(&json!(true)));

        let empty_semaphores = "\
gui/501/com.example.plain = {
    type = LaunchAgent
    state = running
    pid = 102
    properties = runatload
    semaphores = {
    }
}
";
        let st = parse_print(
            empty_semaphores,
            "com.example.plain",
            Scope::User,
            "gui/501",
        )
        .unwrap();
        assert_eq!(st.detail.get("keep_alive"), None);
    }

    #[test]
    fn find_plist_in_dirs_matches_filename_and_xml_label() {
        let dir = tempfile::tempdir().unwrap();
        let agents = dir.path().join("Library").join("LaunchAgents");
        std::fs::create_dir_all(&agents).unwrap();

        let direct = agents.join("com.example.direct.plist");
        std::fs::write(&direct, "<plist><dict/></plist>").unwrap();

        let renamed = agents.join("custom-name.plist");
        std::fs::write(
            &renamed,
            "<plist><dict><key>Label</key><string>com.example.renamed</string></dict></plist>",
        )
        .unwrap();

        let dirs = plist_search_dirs(Scope::User, Some(dir.path()));
        assert_eq!(dirs[0], agents);
        assert_eq!(
            find_plist_in_dirs("com.example.direct", &dirs).as_deref(),
            Some(direct.as_path())
        );
        assert_eq!(
            find_plist_in_dirs("com.example.renamed", &dirs).as_deref(),
            Some(renamed.as_path())
        );
        assert_eq!(find_plist_in_dirs("com.example.absent", &dirs), None);
    }
}
