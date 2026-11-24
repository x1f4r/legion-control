//! Additional environment, policy, and fleet checks appended to `meta.doctor`.
//!
//! Each check is built around a pure evaluation function over canned inputs so every branch can be
//! verified deterministically across Linux, macOS, and Windows without depending on host state.

use std::path::Path;
use std::time::Duration;

use legion_core::Ctx;
use legion_core::policy::{OWNER, Policy, Role};
use legion_core::proto::{Effect, ErrorBody, ToolSpec};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde::Deserialize;
use serde_json::json;

use crate::agent::find_program_on_env_path;

use super::Check;

/// Timeout for probing the user's login shell `PATH` or `loginctl`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-machine timeout when `meta.doctor` checks fleet reachability.
const FLEET_PING_TIMEOUT_S: f64 = 5.0;

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
    let exe_dir = std::env::current_exe()
        .ok()
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

    if let Ok(policy) = ctx.config().get::<Policy>() {
        checks.push(check_policy_exec_roles(&policy));
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

/// Checks whether any role other than `owner` in `policy.toml` allows the `exec` effect.
pub fn check_policy_exec_roles(policy: &Policy) -> Check {
    let flagged: Vec<&str> = policy
        .roles
        .iter()
        .filter(|(name, role)| name.as_str() != OWNER && role_allows_exec(role))
        .map(|(name, _)| name.as_str())
        .collect();

    if flagged.is_empty() {
        Check {
            name: "policy exec roles".to_owned(),
            ok: true,
            warning: false,
            detail: "No non-owner role allows the `exec` effect.".to_owned(),
            hint: None,
        }
    } else {
        Check {
            name: "policy exec roles".to_owned(),
            ok: true,
            warning: true,
            detail: format!(
                "Role(s) {} allow the `exec` effect. Any role with `exec` is as strong as the account, admin included: a command can read, write, and start anything the operating system lets the user do, and neither `paths` nor denying `effect:admin` contains it.",
                flagged.join(", ")
            ),
            hint: Some(
                "Only assign roles with `exec` to principals you trust with the whole account. A role that needs a real boundary must omit `exec` (for example, `observer` or `editor`)."
                    .to_owned(),
            ),
        }
    }
}

fn role_allows_exec(role: &Role) -> bool {
    let probes = [
        ToolSpec {
            name: "exec.run".to_owned(),
            description: String::new(),
            effect: Effect::Exec,
            input_schema: json!({"type": "object"}),
        },
        ToolSpec {
            name: "job.start".to_owned(),
            description: String::new(),
            effect: Effect::Exec,
            input_schema: json!({"type": "object"}),
        },
        ToolSpec {
            name: "proj.run".to_owned(),
            description: String::new(),
            effect: Effect::Exec,
            input_schema: json!({"type": "object"}),
        },
        ToolSpec {
            name: "ext.exec".to_owned(),
            description: String::new(),
            effect: Effect::Exec,
            input_schema: json!({"type": "object"}),
        },
    ];
    probes.iter().any(|spec| {
        !role.deny.iter().any(|rule| rule.matches(spec))
            && role.allow.iter().any(|rule| rule.matches(spec))
    })
}

/// Converts `fleet.ping` outcomes into one `meta.doctor` check per machine.
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
            } else {
                let (detail, hint) = match &entry.error {
                    Some(err) => (err.message.clone(), err.hint.clone()),
                    None => (
                        format!("{} did not answer.", entry.machine),
                        Some(
                            "Check ssh connectivity and that `legion` is installed on the machine."
                                .to_owned(),
                        ),
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
    fn policy_exec_roles_warns_for_every_non_owner_role_allowing_exec() {
        let safe = parse::<Policy>(
            Path::new("policy.toml"),
            b"[roles.writer]\nallow = [\"effect:read\", \"effect:write\"]\npaths = [\"~/projects/**\"]\n",
        )
        .unwrap();
        let safe_check = check_policy_exec_roles(&safe);
        assert!(safe_check.ok && !safe_check.warning && safe_check.hint.is_none());

        // Built-in owner alone never triggers a warning.
        let owner_only = Policy::default();
        let owner_check = check_policy_exec_roles(&owner_only);
        assert!(owner_check.ok && !owner_check.warning && owner_check.hint.is_none());

        // An exec role WITHOUT `paths` and denying `effect:admin` still triggers a warning.
        let unconfined_exec = parse::<Policy>(
            Path::new("policy.toml"),
            b"[roles.operator]\nallow = [\"effect:read\", \"effect:write\", \"effect:exec\", \"effect:destroy\"]\ndeny = [\"effect:power\", \"effect:admin\"]\n",
        )
        .unwrap();
        let warn = check_policy_exec_roles(&unconfined_exec);
        assert!(warn.ok && warn.warning);
        assert!(warn.detail.contains("operator"));
        assert!(
            warn.detail
                .contains("as strong as the account, admin included")
        );
        assert!(warn.hint.is_some());

        // A role combining `paths` with `exec` is also named alongside any unconfined exec role.
        let both = parse::<Policy>(
            Path::new("policy.toml"),
            b"[roles.operator]\nallow = [\"effect:exec\"]\n[roles.runner]\nallow = [\"exec.run\"]\npaths = [\"~/projects/**\"]\n",
        )
        .unwrap();
        let both_warn = check_policy_exec_roles(&both);
        assert!(both_warn.ok && both_warn.warning);
        assert!(both_warn.detail.contains("operator"));
        assert!(both_warn.detail.contains("runner"));
    }

    #[test]
    fn fleet_ping_checks_maps_reachable_and_unreachable_machines() {
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
        ]);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "fleet: pi");
        assert!(checks[0].ok && !checks[0].warning);
        assert!(checks[0].detail.contains("12 ms"));

        assert_eq!(checks[1].name, "fleet: tower");
        assert!(!checks[1].ok);
        assert_eq!(checks[1].detail, "tower did not answer.");
        assert_eq!(checks[1].hint.as_deref(), Some("Check ssh tower."));
    }
}
