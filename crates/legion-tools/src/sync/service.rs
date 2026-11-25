//! Platform service unit generation and install/remove helpers for `legion sync run`.
//!
//! Generates the systemd user unit (Linux), launchd user agent plist (macOS), and Task Scheduler
//! logon task XML (Windows), and provides one install/remove entry point per platform used when
//! a machine gains its first sync folder or loses its last one.
//!
//! Safety guard: tests, `LEGION_HOME` overrides, explicit homes, and `LEGION_SYNC_NO_SERVICE=1`
//! never touch systemd, launchd, or Windows Task Scheduler.

use std::path::{Path, PathBuf};
use std::process::Command;

use legion_core::Paths;

/// Systemd user unit filename.
pub const SYSTEMD_UNIT_NAME: &str = "legion-sync.service";

/// macOS launchd user agent label.
pub const LAUNCHD_LABEL: &str = "dev.legion.sync";

/// Windows Task Scheduler task name.
pub const WINDOWS_TASK_NAME: &str = "LegionSync";

/// Outcome of reconciling the local sync runner service unit with folder membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    /// Service unit was installed/started because the node has at least one local sync folder.
    Installed,
    /// Service unit was stopped/removed because the node has no local sync folders.
    Removed,
    /// Skipped (for example isolated `LEGION_HOME`, test environment, or `LEGION_SYNC_NO_SERVICE`).
    Skipped(String),
    /// Service management could not complete (non-fatal warning surfaced to caller).
    Warning(String),
}

/// Renders a systemd user unit file for `legion sync run`.
pub fn render_systemd_user_unit(legion_bin: &Path, env_overrides: &[(&str, &str)]) -> String {
    let bin = quote_systemd_exec(legion_bin);
    let mut env_lines = String::new();
    for (k, v) in env_overrides {
        env_lines.push_str(&format!("Environment=\"{k}={}\"\n", escape_systemd_env(v)));
    }
    format!(
        "\
# Legion sync folder runner, systemd user unit.
#
# Watches, scans, and reconciles sync folders listed in sync.toml for this machine.
# Managed automatically when the first local sync folder is created/added and removed
# when the last local sync folder leaves this machine.

[Unit]
Description=Legion sync folder runner
Documentation=man:legion(1) https://github.com/x1f4r/legion-control
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
{env_lines}ExecStart={bin} sync run
Restart=on-failure
RestartSec=5s
TimeoutStopSec=20s
UMask=0077
KeyringMode=private
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"
    )
}

/// Renders a macOS `launchd` user agent `.plist` for `legion sync run`.
pub fn render_launchd_plist(
    legion_bin: &Path,
    label: &str,
    log_dir: &Path,
    env_overrides: &[(&str, &str)],
) -> String {
    let bin = xml_escape(&legion_bin.to_string_lossy());
    let label_esc = xml_escape(label);
    let stdout_path = xml_escape(&log_dir.join("sync-runner.out.log").to_string_lossy());
    let stderr_path = xml_escape(&log_dir.join("sync-runner.err.log").to_string_lossy());
    let env_block = if env_overrides.is_empty() {
        String::new()
    } else {
        let mut buf = String::from("    <key>EnvironmentVariables</key>\n    <dict>\n");
        for (k, v) in env_overrides {
            buf.push_str(&format!(
                "      <key>{}</key>\n      <string>{}</string>\n",
                xml_escape(k),
                xml_escape(v),
            ));
        }
        buf.push_str("    </dict>\n");
        buf
    };
    format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
  <dict>
    <key>Label</key>
    <string>{label_esc}</string>
    <key>ProgramArguments</key>
    <array>
      <string>{bin}</string>
      <string>sync</string>
      <string>run</string>
    </array>
{env_block}    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
      <key>SuccessfulExit</key>
      <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{stdout_path}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_path}</string>
  </dict>
</plist>
"
    )
}

/// Renders a Windows Task Scheduler 1.2 XML definition that launches `legion sync run` at logon.
pub fn render_windows_task_xml(
    legion_bin: &Path,
    working_dir: &Path,
    user_id: Option<&str>,
) -> String {
    let bin = xml_escape(&legion_bin.to_string_lossy());
    let work = xml_escape(&working_dir.to_string_lossy());
    let principal = match user_id {
        Some(uid) if !uid.is_empty() => format!(
            "\
    <Principal id=\"Author\">
      <UserId>{}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>",
            xml_escape(uid)
        ),
        _ => "\
    <Principal id=\"Author\">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>"
            .to_owned(),
    };
    format!(
        "\
<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">
  <RegistrationInfo>
    <Description>Legion sync folder runner</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
{principal}
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>3</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context=\"Author\">
    <Exec>
      <Command>{bin}</Command>
      <Arguments>sync run</Arguments>
      <WorkingDirectory>{work}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"
    )
}

fn quote_systemd_exec(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        s.into_owned()
    } else {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

fn escape_systemd_env(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Returns `true` when automatic host service installation/removal must be skipped (tests,
/// `LEGION_HOME` isolation, or `LEGION_SYNC_NO_SERVICE`).
pub fn should_skip_service_management(paths: &Paths) -> Option<&'static str> {
    if cfg!(any(test, feature = "testing")) {
        return Some("disabled in test builds");
    }
    if paths.explicit_home().is_some() {
        return Some("disabled for isolated LEGION_HOME");
    }
    if std::env::var_os("LEGION_HOME").is_some() {
        return Some("disabled when LEGION_HOME is set");
    }
    if std::env::var_os("LEGION_SYNC_NO_SERVICE").is_some() {
        return Some("disabled by LEGION_SYNC_NO_SERVICE");
    }
    None
}

/// Installs/starts or stops/removes the host OS runner service when the node gains its first
/// sync folder (`has_local_folders = true`) or removes its last (`has_local_folders = false`).
pub fn reconcile_host_service(paths: &Paths, has_local_folders: bool) -> ServiceAction {
    if let Some(reason) = should_skip_service_management(paths) {
        return ServiceAction::Skipped(reason.to_owned());
    }
    let legion_bin = legion_core::current_exe().unwrap_or_else(|_| PathBuf::from("legion"));
    #[cfg(target_os = "linux")]
    {
        if has_local_folders {
            match install_systemd_user_unit(&legion_bin, paths.home()) {
                Ok(()) => ServiceAction::Installed,
                Err(msg) => ServiceAction::Warning(msg),
            }
        } else {
            match remove_systemd_user_unit(paths.home()) {
                Ok(()) => ServiceAction::Removed,
                Err(msg) => ServiceAction::Warning(msg),
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if has_local_folders {
            match install_launchd_agent(&legion_bin, paths.home(), paths.state_dir()) {
                Ok(()) => ServiceAction::Installed,
                Err(msg) => ServiceAction::Warning(msg),
            }
        } else {
            match remove_launchd_agent(paths.home()) {
                Ok(()) => ServiceAction::Removed,
                Err(msg) => ServiceAction::Warning(msg),
            }
        }
    }
    #[cfg(windows)]
    {
        if has_local_folders {
            match install_windows_logon_task(&legion_bin, paths.state_dir()) {
                Ok(()) => ServiceAction::Installed,
                Err(msg) => ServiceAction::Warning(msg),
            }
        } else {
            match remove_windows_logon_task() {
                Ok(()) => ServiceAction::Removed,
                Err(msg) => ServiceAction::Warning(msg),
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (legion_bin, has_local_folders);
        ServiceAction::Skipped("no platform service manager on this OS".to_owned())
    }
}

/// Linux: writes `~/.config/systemd/user/legion-sync.service` and enables/starts it via
/// `systemctl --user`.
pub fn install_systemd_user_unit(legion_bin: &Path, home: Option<&Path>) -> Result<(), String> {
    let unit_dir = systemd_user_unit_dir(home)?;
    std::fs::create_dir_all(&unit_dir)
        .map_err(|e| format!("could not create {}: {e}", unit_dir.display()))?;
    let unit_path = unit_dir.join(SYSTEMD_UNIT_NAME);
    let text = render_systemd_user_unit(legion_bin, &[]);
    legion_core::config::write_atomically(&unit_path, text.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", unit_path.display()))?;
    run_service_cmd("systemctl", &["--user", "daemon-reload"])?;
    run_service_cmd("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT_NAME])?;
    Ok(())
}

/// Linux: disables/stops `legion-sync.service` and removes the user unit file.
pub fn remove_systemd_user_unit(home: Option<&Path>) -> Result<(), String> {
    let unit_dir = systemd_user_unit_dir(home)?;
    let unit_path = unit_dir.join(SYSTEMD_UNIT_NAME);
    let _ = run_service_cmd("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT_NAME]);
    match std::fs::remove_file(&unit_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("could not remove {}: {e}", unit_path.display())),
    }
    let _ = run_service_cmd("systemctl", &["--user", "daemon-reload"]);
    Ok(())
}

fn systemd_user_unit_dir(home: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("systemd").join("user"));
    }
    let home = home.ok_or_else(|| "cannot determine home directory for systemd unit".to_owned())?;
    Ok(home.join(".config").join("systemd").join("user"))
}

/// macOS: writes `~/Library/LaunchAgents/dev.legion.sync.plist` and loads it via `launchctl`.
pub fn install_launchd_agent(
    legion_bin: &Path,
    home: Option<&Path>,
    state_dir: &Path,
) -> Result<(), String> {
    let home = home.ok_or_else(|| "cannot determine home directory for launchd agent".to_owned())?;
    let agents_dir = home.join("Library").join("LaunchAgents");
    std::fs::create_dir_all(&agents_dir)
        .map_err(|e| format!("could not create {}: {e}", agents_dir.display()))?;
    let log_dir = state_dir.join("sync");
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| format!("could not create {}: {e}", log_dir.display()))?;
    let plist_path = agents_dir.join(format!("{LAUNCHD_LABEL}.plist"));
    let text = render_launchd_plist(legion_bin, LAUNCHD_LABEL, &log_dir, &[]);
    legion_core::config::write_atomically(&plist_path, text.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", plist_path.display()))?;
    let plist_str = plist_path.to_string_lossy().into_owned();
    let _ = run_service_cmd("launchctl", &["unload", &plist_str]);
    run_service_cmd("launchctl", &["load", "-w", &plist_str])?;
    Ok(())
}

/// macOS: unloads and removes `~/Library/LaunchAgents/dev.legion.sync.plist`.
pub fn remove_launchd_agent(home: Option<&Path>) -> Result<(), String> {
    let home = home.ok_or_else(|| "cannot determine home directory for launchd agent".to_owned())?;
    let plist_path = home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"));
    let plist_str = plist_path.to_string_lossy().into_owned();
    let _ = run_service_cmd("launchctl", &["unload", "-w", &plist_str]);
    match std::fs::remove_file(&plist_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("could not remove {}: {e}", plist_path.display())),
    }
    Ok(())
}

/// Windows: creates and starts the `LegionSync` logon task via `schtasks`.
pub fn install_windows_logon_task(legion_bin: &Path, state_dir: &Path) -> Result<(), String> {
    let sync_dir = state_dir.join("sync");
    std::fs::create_dir_all(&sync_dir)
        .map_err(|e| format!("could not create {}: {e}", sync_dir.display()))?;
    let xml_path = sync_dir.join("legion-sync-task.xml");
    let xml = render_windows_task_xml(legion_bin, &sync_dir, None);
    legion_core::config::write_atomically(&xml_path, xml.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", xml_path.display()))?;
    let xml_str = xml_path.to_string_lossy().into_owned();
    run_service_cmd(
        "schtasks",
        &["/Create", "/TN", WINDOWS_TASK_NAME, "/XML", &xml_str, "/F"],
    )?;
    let _ = run_service_cmd("schtasks", &["/Run", "/TN", WINDOWS_TASK_NAME]);
    Ok(())
}

/// Windows: stops and removes the `LegionSync` logon task via `schtasks`.
pub fn remove_windows_logon_task() -> Result<(), String> {
    let _ = run_service_cmd("schtasks", &["/End", "/TN", WINDOWS_TASK_NAME]);
    let _ = run_service_cmd("schtasks", &["/Delete", "/TN", WINDOWS_TASK_NAME, "/F"]);
    Ok(())
}

fn run_service_cmd(program: &str, args: &[&str]) -> Result<(), String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("could not run `{program}`: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(format!(
            "`{program} {}` exited with {}: {stderr}",
            args.join(" "),
            output.status
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_valid_systemd_user_unit() {
        let unit = render_systemd_user_unit(Path::new("/usr/bin/legion"), &[]);
        assert!(unit.contains("[Unit]"));
        assert!(unit.contains("Description=Legion sync folder runner"));
        assert!(unit.contains("After=network-online.target"));
        assert!(unit.contains("[Service]"));
        assert!(unit.contains("ExecStart=/usr/bin/legion sync run"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("UMask=0077"));
        assert!(unit.contains("[Install]"));
        assert!(unit.contains("WantedBy=default.target"));

        let quoted = render_systemd_user_unit(
            Path::new("/opt/my tools/legion"),
            &[("LEGION_HOME", "/var/lib/legion")],
        );
        assert!(quoted.contains("ExecStart=\"/opt/my tools/legion\" sync run"));
        assert!(quoted.contains("Environment=\"LEGION_HOME=/var/lib/legion\""));
    }

    #[test]
    fn renders_valid_launchd_plist_with_xml_escaping() {
        let plist = render_launchd_plist(
            Path::new("/Users/me/.local/bin/legion"),
            LAUNCHD_LABEL,
            Path::new("/Users/me/Library/Logs/A&B"),
            &[("LEGION_HOME", "/Users/me/.legion<1>")],
        );
        assert!(plist.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(plist.contains("<string>dev.legion.sync</string>"));
        assert!(plist.contains("<string>/Users/me/.local/bin/legion</string>"));
        assert!(plist.contains("<string>sync</string>"));
        assert!(plist.contains("<string>run</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n      <false/>"));
        assert!(plist.contains("A&amp;B/sync-runner.out.log"));
        assert!(plist.contains("/Users/me/.legion&lt;1&gt;"));
    }

    #[test]
    fn renders_valid_windows_logon_task_xml() {
        let xml = render_windows_task_xml(
            Path::new(r"C:\Users\me\AppData\Local\Programs\legion.exe"),
            Path::new(r"C:\Users\me\AppData\Local\legion\sync"),
            Some(r"PC\me"),
        );
        assert!(xml.contains("<Task version=\"1.2\""));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<Command>C:\\Users\\me\\AppData\\Local\\Programs\\legion.exe</Command>"));
        assert!(xml.contains("<Arguments>sync run</Arguments>"));
        assert!(xml.contains("<UserId>PC\\me</UserId>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    }

    #[test]
    fn service_management_is_always_skipped_in_tests_and_explicit_homes() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::under(tmp.path());
        assert!(matches!(
            reconcile_host_service(&paths, true),
            ServiceAction::Skipped(_)
        ));
        assert!(matches!(
            reconcile_host_service(&paths, false),
            ServiceAction::Skipped(_)
        ));
    }
}
