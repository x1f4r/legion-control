//! Additional environment, policy, and fleet checks appended to `meta.doctor`.
//!
//! Each check is built around a pure evaluation function over canned inputs so every branch can be
//! verified deterministically across Linux, macOS, and Windows without depending on host state.

use std::path::Path;
use std::time::Duration;

use legion_core::Ctx;
use legion_core::policy::{OWNER, Policy, Role};
use legion_core::proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde::Deserialize;
use serde_json::json;

use crate::agent::find_program_on_env_path;
use crate::package::{PackageManager, detect_package_manager};

use super::Check;

/// Timeout for probing the user's login shell `PATH` or `loginctl`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-machine timeout when `meta.doctor` checks fleet reachability.
const FLEET_PING_TIMEOUT_S: f64 = 5.0;

/// Maximum number of matching `exec` tool names shown per role in `meta.doctor`.
const MAX_NAMED_EXEC_TOOLS: usize = 5;

/// Unique sentinel wrapping `$PATH` in login-shell probe output so profile banners are ignored.
const PATH_MARKER: &str = "__LEGION_LOGIN_PATH__";

/// One machine's ping summary as returned by `fleet.ping`.
#[derive(Clone, Debug, Deserialize)]
pub struct FleetPingEntry {
    /// Machine name in `fleet.toml`.
    pub machine: String,
    /// Whether the machine answered `hello`.
    pub reachable: bool,
    /// Node name reported by the machine.
    #[serde(default)]
    pub node: Option<String>,
    /// Legion version reported by the machine.
    #[serde(default)]
    pub version: Option<String>,
    /// Round-trip latency in milliseconds.
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// Error body when unreachable.
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

#[derive(Deserialize)]
struct FleetPingResponse {
    results: Vec<FleetPingEntry>,
}

/// Runs the extra environment, policy, and (unless `quick` is set) fleet checks and appends them.
pub(super) async fn append_extra_checks(ctx: &Ctx, quick: bool, checks: &mut Vec<Check>) {
    let exe_path = legion_core::current_exe().ok();
    let package_manager = exe_path.as_deref().and_then(detect_package_manager);
    checks.push(check_installation(
        exe_path.as_deref(),
        package_manager.as_ref(),
    ));

    let exe_dir = exe_path
        .as_deref()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let login_path = query_login_shell_path(ctx).await;
    checks.push(check_executable_on_path(
        exe_dir.as_deref(),
        login_path.as_deref(),
        cfg!(windows),
    ));

    #[cfg(target_os = "linux")]
    {
        let (linger, user) = query_linux_lingering(ctx).await;
        checks.push(check_linux_lingering(linger, user.as_deref()));
    }

    let ssh_path = find_program_on_env_path("ssh");
    checks.push(check_ssh_on_path(ssh_path.as_deref()));

    if let Some(pending) = crate::power::pending::pending(ctx.paths(), ctx.boot_id().as_deref()) {
        checks.push(check_pending_power(&pending));
    }

    if let Ok(policy) = ctx.config().get::<Policy>() {
        let specs = ctx.tools();
        checks.push(check_policy_exec_roles(&policy, &specs));
    }

    if !quick
        && let Some(ping_tool) = ctx.node().registry().get("fleet.ping")
        && let Ok(value) = ping_tool
            .call(ctx, json!({"timeout_s": FLEET_PING_TIMEOUT_S}))
            .await
        && let Ok(parsed) = serde_json::from_value::<FleetPingResponse>(value)
    {
        checks.extend(fleet_ping_checks(&parsed.results));
    }
}

/// A warning while a sleep, restart or shutdown has been handed over and not carried out yet: the
/// machine is going to go down, and someone looking at its health should know, and how to stop it.
pub fn check_pending_power(pending: &crate::power::pending::Pending) -> Check {
    let what = match &pending.into {
        Some(into) => format!("{} into {into}", pending.action.as_str()),
        None => pending.action.as_str().to_owned(),
    };
    let due = crate::power::pending::due_at(pending)
        .map_or_else(String::new, |due| format!(", due at {due}"));
    Check {
        name: "pending power action".to_owned(),
        ok: true,
        warning: true,
        detail: format!(
            "A {what} handed over at {} is still to happen{due}.",
            pending.since
        ),
        hint: Some(
            "power.cancel calls it off until then; power.status shows it under pending.".to_owned(),
        ),
    }
}

/// Reports how this `legion` binary was installed and whether in-place self-update is managed by
/// a system package manager.
pub fn check_installation(exe: Option<&Path>, manager: Option<&PackageManager>) -> Check {
    let detail = match (exe, manager) {
        (Some(path), Some(pm)) => format!(
            "Installed at {}, managed by {} (upgrade through the package manager; meta.update is disabled).",
            path.display(),
            pm.display_name()
        ),
        (None, Some(pm)) => format!(
            "Managed by {} (upgrade through the package manager; meta.update is disabled).",
            pm.display_name()
        ),
        (Some(path), None) => format!(
            "Installed at {} as a standalone binary (self-update via meta.update is enabled).",
            path.display()
        ),
        (None, None) => {
            "Installed as a standalone binary (executable path could not be determined).".to_owned()
        }
    };
    Check {
        name: "installation".to_owned(),
        ok: true,
        warning: false,
        detail,
        hint: None,
    }
}

/// Checks whether the directory holding the running `legion` binary appears on a login shell's `PATH`.
pub fn check_executable_on_path(
    exe_dir: Option<&Path>,
    login_path: Option<&str>,
    windows: bool,
) -> Check {
    let Some(exe_dir) = exe_dir else {
        return Check {
            name: "executable on PATH".to_owned(),
            ok: true,
            warning: true,
            detail: "Could not determine the directory holding the running legion executable."
                .to_owned(),
            hint: Some(
                "Ensure `legion` is installed in a directory on your login shell PATH.".to_owned(),
            ),
        };
    };

    let Some(login_path) = login_path else {
        return Check {
            name: "executable on PATH".to_owned(),
            ok: true,
            warning: true,
            detail: format!(
                "Could not read login shell PATH to verify whether {} is listed.",
                exe_dir.display()
            ),
            hint: Some(path_hint(exe_dir, windows)),
        };
    };

    let sep = if windows { ';' } else { ':' };
    let exe_norm = normalize_dir_str(&exe_dir.to_string_lossy(), windows);
    let exe_canon = dunce::canonicalize(exe_dir).ok();

    let on_path = login_path
        .split(sep)
        .filter(|part| !part.is_empty())
        .any(|entry| {
            if normalize_dir_str(entry, windows) == exe_norm {
                return true;
            }
            if let Some(ref want) = exe_canon
                && let Ok(found) = dunce::canonicalize(Path::new(entry))
            {
                return &found == want;
            }
            false
        });

    if on_path {
        Check {
            name: "executable on PATH".to_owned(),
            ok: true,
            warning: false,
            detail: format!("{} is on PATH for login shells.", exe_dir.display()),
            hint: None,
        }
    } else {
        Check {
            name: "executable on PATH".to_owned(),
            ok: true,
            warning: true,
            detail: format!("{} is not on PATH for login shells.", exe_dir.display()),
            hint: Some(path_hint(exe_dir, windows)),
        }
    }
}

fn normalize_dir_str(raw: &str, windows: bool) -> String {
    let trimmed = raw.trim();
    if windows {
        let unified = trimmed.replace('/', "\\");
        let stripped = unified.trim_end_matches('\\');
        // Keep drive roots such as `C:\` distinct from bare `C:`.
        let kept = if stripped.len() == 2 && stripped.ends_with(':') {
            format!("{stripped}\\")
        } else {
            stripped.to_owned()
        };
        kept.to_ascii_lowercase()
    } else {
        let stripped = trimmed.trim_end_matches('/');
        if stripped.is_empty() && !trimmed.is_empty() {
            "/".to_owned()
        } else {
            stripped.to_owned()
        }
    }
}

fn path_hint(exe_dir: &Path, windows: bool) -> String {
    if windows {
        format!(
            "Add {} to your User or System PATH environment variable so shells and ssh sessions find `legion`.",
            exe_dir.display()
        )
    } else {
        format!(
            "Add `export PATH=\"{}:$PATH\"` to your login shell profile (or install legion in a directory already on PATH such as ~/.local/bin).",
            exe_dir.display()
        )
    }
}

/// Extracts the `$PATH` payload between [`PATH_MARKER`] sentinels from a login-shell probe.
pub fn extract_login_path(stdout: &str) -> Option<&str> {
    let (_, after_start) = stdout.split_once(PATH_MARKER)?;
    let (payload, _) = after_start.split_once(PATH_MARKER)?;
    Some(payload)
}

async fn query_login_shell_path(ctx: &Ctx) -> Option<String> {
    if cfg!(windows) {
        return std::env::var("PATH").ok();
    }

    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned());
    let shell_name = Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sh");

    let (program, args): (&str, Vec<String>) = match shell_name {
        "fish" => (
            &shell,
            vec![
                "-l".to_owned(),
                "-c".to_owned(),
                format!("printf '{PATH_MARKER}%s{PATH_MARKER}' (string join : $PATH)"),
            ],
        ),
        "csh" | "tcsh" => (
            "/bin/sh",
            vec![
                "-l".to_owned(),
                "-c".to_owned(),
                format!("printf '{PATH_MARKER}%s{PATH_MARKER}' \"$PATH\""),
            ],
        ),
        _ => (
            &shell,
            vec![
                "-l".to_owned(),
                "-c".to_owned(),
                format!("printf '{PATH_MARKER}%s{PATH_MARKER}' \"$PATH\""),
            ],
        ),
    };

    let spec = CommandSpec::argv(program, args);
    let options = RunOptions::new(spec)
        .c_locale()
        .timeout(PROBE_TIMEOUT)
        .cancel(ctx.cancel_token().child_token());
    if let Ok(output) = run(options).await
        && output.success()
        && let Some(extracted) = extract_login_path(&output.stdout.text)
    {
        return Some(extracted.to_owned());
    }
    std::env::var("PATH").ok()
}

/// Parses `loginctl show-user --property=Linger` output (`Linger=yes`, `Linger=no`, `yes`, `no`).
pub fn parse_linger_output(stdout: &str) -> Option<bool> {
    for line in stdout.lines() {
        let trimmed = line.trim();
        let value = trimmed.strip_prefix("Linger=").unwrap_or(trimmed);
        match value {
            "yes" => return Some(true),
            "no" => return Some(false),
            _ => {}
        }
    }
    None
}

/// Evaluates Linux systemd user lingering status and builds the check with the exact enable command.
pub fn check_linux_lingering(linger_enabled: Option<bool>, username: Option<&str>) -> Check {
    let user_label = username.filter(|u| !u.is_empty()).unwrap_or("<user>");
    let enable_cmd = format!("sudo loginctl enable-linger {user_label}");
    match linger_enabled {
        Some(true) => Check {
            name: "systemd lingering".to_owned(),
            ok: true,
            warning: false,
            detail: format!(
                "Systemd user lingering is enabled for {user_label}, so background jobs survive logout."
            ),
            hint: None,
        },
        Some(false) => Check {
            name: "systemd lingering".to_owned(),
            ok: true,
            warning: true,
            detail: format!(
                "Systemd user lingering is disabled for {user_label}; without it, background jobs may not survive logout."
            ),
            hint: Some(format!("Enable lingering with `{enable_cmd}`.")),
        },
        None => Check {
            name: "systemd lingering".to_owned(),
            ok: true,
            warning: true,
            detail: "Could not check whether systemd user lingering is enabled; without it, background jobs may not survive logout.".to_owned(),
            hint: Some(format!("On a systemd host, enable lingering with `{enable_cmd}`.")),
        },
    }
}

#[cfg(target_os = "linux")]
async fn query_linux_lingering(ctx: &Ctx) -> (Option<bool>, Option<String>) {
    let username = std::env::var("USER")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("LOGNAME").ok().filter(|s| !s.is_empty()));

    if let Some(ref user) = username
        && Path::new("/var/lib/systemd/linger").join(user).is_file()
    {
        return (Some(true), username);
    }

    let mut args = vec!["show-user".to_owned()];
    if let Some(ref user) = username {
        args.push(user.clone());
    }
    args.push("--property=Linger".to_owned());

    let options = RunOptions::new(CommandSpec::argv("loginctl", args))
        .c_locale()
        .timeout(PROBE_TIMEOUT)
        .cancel(ctx.cancel_token().child_token());
    let enabled = match run(options).await {
        Ok(output) if output.success() => parse_linger_output(&output.stdout.text),
        _ => None,
    };
    (enabled, username)
}

/// Checks whether the `ssh` client is available on `PATH`.
pub fn check_ssh_on_path(found: Option<&Path>) -> Check {
    match found {
        Some(path) => Check {
            name: "ssh".to_owned(),
            ok: true,
            warning: false,
            detail: format!("ssh found at {}.", path.display()),
            hint: None,
        },
        None => Check {
            name: "ssh".to_owned(),
            ok: true,
            warning: true,
            detail: "ssh was not found on PATH.".to_owned(),
            hint: Some(
                "Install an OpenSSH client and ensure `ssh` is on PATH so Legion can reach remote machines."
                    .to_owned(),
            ),
        },
    }
}

/// Checks whether any role other than `owner` in `policy.toml` allows any live `Effect::Exec` tool
/// (built-in or extension) from `specs`, naming the matching tools (bounded per role).
pub fn check_policy_exec_roles(policy: &Policy, specs: &[ToolSpec]) -> Check {
    let mut exec_specs: Vec<&ToolSpec> = specs
        .iter()
        .filter(|spec| spec.effect == Effect::Exec)
        .collect();
    exec_specs.sort_by(|a, b| a.name.cmp(&b.name));
    exec_specs.dedup_by(|a, b| a.name == b.name);

    let mut flagged: Vec<String> = Vec::new();
    for (name, role) in &policy.roles {
        if name.as_str() == OWNER {
            continue;
        }
        let matched: Vec<&str> = exec_specs
            .iter()
            .filter(|spec| {
                !role.deny.iter().any(|rule| rule.matches(spec))
                    && role.allow.iter().any(|rule| rule.matches(spec))
            })
            .map(|spec| spec.name.as_str())
            .collect();
        if !matched.is_empty() {
            flagged.push(format!("{name} ({})", format_bounded_tools(&matched)));
        } else if exec_specs.is_empty() && role_allows_exec_effect_broadly(role) {
            flagged.push(format!("{name} (effect:exec)"));
        }
    }

    if flagged.is_empty() {
        Check {
            name: "policy exec roles".to_owned(),
            ok: true,
            warning: false,
            detail: "No non-owner role allows any `exec` tool.".to_owned(),
            hint: None,
        }
    } else {
        Check {
            name: "policy exec roles".to_owned(),
            ok: true,
            warning: true,
            detail: format!(
                "Role(s) {} allow `exec` tools. Any role with `exec` is as strong as the account, admin included: a command can read, write, and start anything the operating system lets the user do, and neither `paths` nor denying `effect:admin` contains it.",
                flagged.join("; ")
            ),
            hint: Some(
                "Only assign roles with `exec` to principals you trust with the whole account. A role that needs a real boundary must omit `exec` (for example, `observer` or `editor`)."
                    .to_owned(),
            ),
        }
    }
}

fn format_bounded_tools(tools: &[&str]) -> String {
    if tools.len() <= MAX_NAMED_EXEC_TOOLS {
        tools.join(", ")
    } else {
        let shown = tools[..MAX_NAMED_EXEC_TOOLS].join(", ");
        let extra = tools.len() - MAX_NAMED_EXEC_TOOLS;
        format!("{shown}, and {extra} more")
    }
}

fn role_allows_exec_effect_broadly(role: &Role) -> bool {
    let probe = ToolSpec {
        name: "exec.run".to_owned(),
        description: String::new(),
        effect: Effect::Exec,
        input_schema: json!({"type": "object"}),
    };
    !role.deny.iter().any(|rule| rule.matches(&probe))
        && role.allow.iter().any(|rule| rule.matches(&probe))
}

fn is_unreachable_warning(error: Option<&ErrorBody>) -> bool {
    match error {
        None => true,
        Some(err) => match err.code {
            ErrorCode::Unreachable | ErrorCode::Timeout => {
                let non_protocol = err
                    .details
                    .as_ref()
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str())
                    == Some("non_protocol_output")
                    || err.message.contains("non-protocol lines")
                    || err.message.contains("hello reply was invalid");
                !non_protocol
            }
            _ => false,
        },
    }
}

/// Converts `fleet.ping` outcomes into one `meta.doctor` check per machine.
///
/// A machine that simply does not answer (unreachable or timed out) becomes a warning (`ok: true`,
/// `warning: true`) because switched-off machines are normal; protocol mismatches, unparseable
/// replies, and local configuration errors remain failures (`ok: false`).
pub fn fleet_ping_checks(results: &[FleetPingEntry]) -> Vec<Check> {
    results
        .iter()
        .map(|entry| {
            let name = format!("fleet: {}", entry.machine);
            if entry.reachable {
                let node = entry.node.as_deref().unwrap_or(&entry.machine);
                let version = entry.version.as_deref().unwrap_or("unknown");
                let detail = match entry.latency_ms {
                    Some(ms) => format!("Answered as {node} (v{version}, {ms} ms)."),
                    None => format!("Answered as {node} (v{version})."),
                };
                Check {
                    name,
                    ok: true,
                    warning: false,
                    detail,
                    hint: None,
                }
            } else if is_unreachable_warning(entry.error.as_ref()) {
                let default_hint = format!(
                    "If {} should be online, check ssh connectivity and that `legion` is installed on it.",
                    entry.machine
                );
                let (detail, hint) = match &entry.error {
                    Some(err) => (
                        format!(
                            "{} did not answer (it may simply be off): {}",
                            entry.machine, err.message
                        ),
                        err.hint.clone().or(Some(default_hint)),
                    ),
                    None => (
                        format!("{} did not answer (it may simply be off).", entry.machine),
                        Some(default_hint),
                    ),
                };
                Check {
                    name,
                    ok: true,
                    warning: true,
                    detail,
                    hint,
                }
            } else {
                let (detail, hint) = match &entry.error {
                    Some(err) => (err.message.clone(), err.hint.clone()),
                    None => (
                        format!("{} returned an unexpected error.", entry.machine),
                        None,
                    ),
                };
                Check {
                    name,
                    ok: false,
                    warning: false,
                    detail,
                    hint,
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use legion_core::config::parse;
    use legion_core::proto::ErrorCode;

    use super::*;

    #[test]
    fn a_pending_power_action_is_a_warning_that_names_power_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let paths = legion_core::Paths::under(dir.path());
        let power = crate::power::pending::dir(&paths);
        assert!(crate::power::pending::pending(&paths, Some("boot-1")).is_none());
        let record = crate::power::pending::Pending {
            id: "a1".to_owned(),
            action: crate::power::plan::Action::Restart,
            into: Some("windows".to_owned()),
            argv: vec![
                "shutdown".to_owned(),
                "/r".to_owned(),
                "/t".to_owned(),
                "600".to_owned(),
            ],
            delay_s: 600,
            hold_s: 0,
            since: crate::power::pending::now(),
            boot_id: Some("boot-1".to_owned()),
            runner: false,
            taken: None,
            disarm: None,
            abandoned: false,
            fired: None,
            force: false,
            config_dir: None,
            readback: None,
        };
        crate::power::pending::write_pending(&power, &record).unwrap();
        let pending = crate::power::pending::pending(&paths, Some("boot-1")).unwrap();
        let check = check_pending_power(&pending);
        assert!(check.ok && check.warning);
        assert!(
            check
                .detail
                .starts_with("A restart into windows handed over at")
                && check.detail.contains(", due at "),
            "{}",
            check.detail
        );
        assert!(check.hint.unwrap().contains("power.cancel"));
    }

    #[test]
    fn executable_on_path_checks_unix_and_windows_paths() {
        let on_unix = check_executable_on_path(
            Some(Path::new("/home/me/.local/share/legion")),
            Some("/usr/bin:/home/me/.local/share/legion/:/bin"),
            false,
        );
        assert!(on_unix.ok && !on_unix.warning && on_unix.hint.is_none());

        let missing_unix = check_executable_on_path(
            Some(Path::new("/home/me/.local/share/legion")),
            Some("/usr/bin:/bin"),
            false,
        );
        assert!(missing_unix.ok && missing_unix.warning);
        assert!(
            missing_unix
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("/home/me/.local/share/legion"))
        );

        let on_windows = check_executable_on_path(
            Some(Path::new(r"C:\Users\me\AppData\Local\legion")),
            Some(r"C:\Windows\System32;c:\users\me\appdata\local\legion\"),
            true,
        );
        assert!(on_windows.ok && !on_windows.warning);

        let missing_windows = check_executable_on_path(
            Some(Path::new(r"C:\Users\me\legion")),
            Some(r"C:\Windows\System32"),
            true,
        );
        assert!(missing_windows.ok && missing_windows.warning && missing_windows.hint.is_some());

        assert_eq!(
            extract_login_path(
                "MOTD banner\n__LEGION_LOGIN_PATH__/usr/bin:/bin__LEGION_LOGIN_PATH__\n"
            ),
            Some("/usr/bin:/bin")
        );
        assert_eq!(extract_login_path("no markers here"), None);
    }

    #[test]
    fn linux_lingering_check_parses_output_and_gives_exact_enable_command() {
        assert_eq!(parse_linger_output("Linger=yes\n"), Some(true));
        assert_eq!(parse_linger_output("Linger=no\n"), Some(false));
        assert_eq!(parse_linger_output(""), None);

        let enabled = check_linux_lingering(Some(true), Some("me"));
        assert!(enabled.ok && !enabled.warning && enabled.hint.is_none());

        let disabled = check_linux_lingering(Some(false), Some("me"));
        assert!(disabled.ok && disabled.warning);
        assert!(disabled.detail.contains("survive logout"));
        assert!(
            disabled
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("sudo loginctl enable-linger me"))
        );

        let unknown = check_linux_lingering(None, None);
        assert!(unknown.ok && unknown.warning);
        assert!(
            unknown
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("sudo loginctl enable-linger <user>"))
        );
    }

    #[test]
    fn ssh_check_reports_present_and_missing() {
        let present = check_ssh_on_path(Some(Path::new("/usr/bin/ssh")));
        assert!(present.ok && !present.warning && present.hint.is_none());

        let missing = check_ssh_on_path(None);
        assert!(missing.ok && missing.warning && missing.hint.is_some());
    }

    #[test]
    fn policy_exec_roles_warns_against_every_live_exec_spec_with_bounded_tool_list() {
        let make_spec = |name: &str, effect: Effect| ToolSpec {
            name: name.to_owned(),
            description: String::new(),
            effect,
            input_schema: json!({"type": "object"}),
        };
        let live_specs = vec![
            make_spec("fs.read", Effect::Read),
            make_spec("exec.run", Effect::Exec),
            make_spec("job.start", Effect::Exec),
            make_spec("job.cancel", Effect::Exec),
            make_spec("proj.pull", Effect::Exec),
            make_spec("proj.run", Effect::Exec),
            make_spec("svc.restart", Effect::Exec),
            make_spec("ops.deploy", Effect::Exec),
            make_spec("ops.info", Effect::Read),
        ];

        let safe = parse::<Policy>(
            Path::new("policy.toml"),
            b"[roles.writer]\nallow = [\"effect:read\", \"effect:write\", \"ops.info\"]\npaths = [\"~/projects/**\"]\n",
        )
        .unwrap();
        let safe_check = check_policy_exec_roles(&safe, &live_specs);
        assert!(safe_check.ok && !safe_check.warning && safe_check.hint.is_none());

        // Built-in owner alone never triggers a warning.
        let owner_only = Policy::default();
        let owner_check = check_policy_exec_roles(&owner_only, &live_specs);
        assert!(owner_check.ok && !owner_check.warning && owner_check.hint.is_none());

        // Role allowing `proj.pull` by exact name and role matching an exec extension by glob
        // (`ops.*`) are both caught and name the matching tools; broad roles bound the tool list.
        let policy = parse::<Policy>(
            Path::new("policy.toml"),
            b"\
[roles.puller]
allow = [\"proj.pull\"]

[roles.deployer]
allow = [\"ops.*\"]

[roles.operator]
allow = [\"effect:read\", \"effect:write\", \"effect:exec\", \"effect:destroy\"]
deny = [\"effect:power\", \"effect:admin\"]
",
        )
        .unwrap();
        let warn = check_policy_exec_roles(&policy, &live_specs);
        assert!(warn.ok && warn.warning);
        assert!(
            warn.detail.contains("puller (proj.pull)"),
            "{}",
            warn.detail
        );
        assert!(
            warn.detail.contains("deployer (ops.deploy)"),
            "{}",
            warn.detail
        );
        assert!(
            warn.detail.contains("operator (") && warn.detail.contains("and 2 more)"),
            "{}",
            warn.detail
        );
        assert!(
            warn.detail
                .contains("as strong as the account, admin included")
        );
        assert!(warn.hint.is_some());
    }

    #[test]
    fn fleet_ping_checks_maps_reachable_unreachable_and_protocol_failures() {
        let checks = fleet_ping_checks(&[
            FleetPingEntry {
                machine: "pi".to_owned(),
                reachable: true,
                node: Some("pi".to_owned()),
                version: Some("0.1.0".to_owned()),
                latency_ms: Some(12),
                error: None,
            },
            FleetPingEntry {
                machine: "tower".to_owned(),
                reachable: false,
                node: None,
                version: None,
                latency_ms: None,
                error: Some(
                    ErrorBody::new(ErrorCode::Unreachable, "tower did not answer.")
                        .with_hint("Check ssh tower."),
                ),
            },
            FleetPingEntry {
                machine: "laptop".to_owned(),
                reachable: false,
                node: None,
                version: None,
                latency_ms: None,
                error: Some(ErrorBody::new(
                    ErrorCode::Timeout,
                    "Timed out waiting for laptop after 5s.",
                )),
            },
            FleetPingEntry {
                machine: "oldbox".to_owned(),
                reachable: false,
                node: None,
                version: None,
                latency_ms: None,
                error: Some(ErrorBody::new(
                    ErrorCode::Unsupported,
                    "oldbox speaks protocol 99, and this node speaks protocol 1.",
                )),
            },
            FleetPingEntry {
                machine: "garbled".to_owned(),
                reachable: false,
                node: None,
                version: None,
                latency_ms: None,
                error: Some(ErrorBody::new(
                    ErrorCode::Failed,
                    "Could not open a legion session on garbled: hello reply was invalid (missing field).",
                )),
            },
            FleetPingEntry {
                machine: "noisy".to_owned(),
                reachable: false,
                node: None,
                version: None,
                latency_ms: None,
                error: Some(
                    ErrorBody::new(
                        ErrorCode::Unreachable,
                        "Could not open a legion session on noisy: received 200 non-protocol lines (4096 bytes) before hello.",
                    )
                    .with_details(json!({"reason": "non_protocol_output"})),
                ),
            },
        ]);
        assert_eq!(checks.len(), 6);
        assert_eq!(checks[0].name, "fleet: pi");
        assert!(checks[0].ok && !checks[0].warning);
        assert!(checks[0].detail.contains("12 ms"));

        // Unreachable and timed-out machines are warnings (`ok: true`, `warning: true`).
        assert_eq!(checks[1].name, "fleet: tower");
        assert!(checks[1].ok && checks[1].warning);
        assert!(checks[1].detail.contains("tower") && checks[1].detail.contains("off"));
        assert_eq!(checks[1].hint.as_deref(), Some("Check ssh tower."));

        assert_eq!(checks[2].name, "fleet: laptop");
        assert!(checks[2].ok && checks[2].warning);
        assert!(checks[2].detail.contains("laptop") && checks[2].detail.contains("off"));

        // Protocol mismatch and unparseable replies stay failures (`ok: false`).
        assert_eq!(checks[3].name, "fleet: oldbox");
        assert!(!checks[3].ok && !checks[3].warning);
        assert!(checks[3].detail.contains("protocol 99"));

        assert_eq!(checks[4].name, "fleet: garbled");
        assert!(!checks[4].ok && !checks[4].warning);
        assert!(checks[4].detail.contains("hello reply was invalid"));

        assert_eq!(checks[5].name, "fleet: noisy");
        assert!(!checks[5].ok && !checks[5].warning);
        assert!(checks[5].detail.contains("non-protocol lines"));
    }

    #[test]
    fn installation_check_reports_standalone_and_package_managers() {
        let standalone = check_installation(Some(Path::new("/home/me/.local/bin/legion")), None);
        assert_eq!(standalone.name, "installation");
        assert!(standalone.ok && !standalone.warning && standalone.hint.is_none());
        assert!(standalone.detail.contains("standalone"));

        let deb = check_installation(
            Some(Path::new("/usr/bin/legion")),
            Some(&PackageManager::Deb),
        );
        assert!(deb.ok && !deb.warning);
        assert!(deb.detail.contains("deb (apt)"));
        assert!(deb.detail.contains("meta.update is disabled"));

        let pacman = check_installation(
            Some(Path::new("/usr/bin/legion")),
            Some(&PackageManager::Pacman),
        );
        assert!(pacman.ok && !pacman.warning);
        assert!(pacman.detail.contains("pacman"));
    }
}
