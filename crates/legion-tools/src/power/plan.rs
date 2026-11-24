//! How each platform sleeps, restarts and shuts down, and how to ask it beforehand whether it will
//! let this account do so.
//!
//! Every builder here is a pure function of the platform and the settings, so the commands for all
//! three platforms are tested on any of them, and `dry_run` shows exactly what a real call runs.
//!
//! The defaults, and why:
//!
//! - **Linux**: `systemctl suspend|reboot|poweroff`, run by a detached runner a few seconds after
//!   the reply. Because that runner cannot report back, the node first asks the system whether it
//!   would allow the action: logind's `Can<Verb>` without sudo, `sudo -n -l <command>` with it.
//! - **macOS**: `pmset sleepnow`, which needs no privilege, and `shutdown -r now` / `shutdown -h
//!   now`, which need root: `sudo -n -l` with sudo, `id -u` without.
//! - **Windows**: `shutdown /r` and `shutdown /s` with `/t <delay>`, so Windows itself waits and the
//!   refusal (or a shutdown already scheduled) comes back at once. Sleep is Sysinternals
//!   `psshutdown -d -t <delay> -accepteula`, found on `PATH` when the call is made: the built-in
//!   `rundll32 powrprof.dll,SetSuspendState` and the .NET call are silently vetoed by the power
//!   policy on some machines (they exit 0 and the machine stays awake), and `psshutdown -t 0` never
//!   returns in a non-interactive session.

use std::path::PathBuf;

use legion_core::proto::{ErrorBody, ErrorCode};
use serde::Serialize;
use serde_json::{Value, json};

use super::actuator::Ran;

/// The delay a sleep is given, which `power.sleep` does not take as an argument: long enough for
/// the reply to leave.
pub const SLEEP_DELAY_S: u32 = 3;

/// The platforms the power tools know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// systemd.
    Linux,
    /// `pmset` and `shutdown`.
    Macos,
    /// `shutdown.exe`, `psshutdown` and `bcdedit`.
    Windows,
}

impl Platform {
    /// The platform `ctx.os()` names, or `unsupported`.
    pub fn of(os: &str) -> Result<Platform, ErrorBody> {
        match os {
            "linux" => Ok(Platform::Linux),
            "macos" => Ok(Platform::Macos),
            "windows" => Ok(Platform::Windows),
            other => Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!("The power tools do not know how to control a {other} machine."),
            )
            .with_hint("They support Linux (systemd), macOS and Windows.")),
        }
    }

    /// Whether `[power] sudo` means anything here.
    pub fn has_sudo(self) -> bool {
        self != Platform::Windows
    }
}

/// What a power call does to the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Suspend.
    Sleep,
    /// Restart, perhaps into another system.
    Restart,
    /// Power off.
    Shutdown,
}

impl Action {
    /// The action as a phrase: "restart this machine".
    pub fn phrase(self) -> &'static str {
        match self {
            Action::Sleep => "put this machine to sleep",
            Action::Restart => "restart this machine",
            Action::Shutdown => "shut this machine down",
        }
    }

    fn systemctl_verb(self) -> &'static str {
        match self {
            Action::Sleep => "suspend",
            Action::Restart => "reboot",
            Action::Shutdown => "poweroff",
        }
    }

    fn logind_method(self) -> &'static str {
        match self {
            Action::Sleep => "CanSuspend",
            Action::Restart => "CanReboot",
            Action::Shutdown => "CanPowerOff",
        }
    }
}

/// Who waits out the delay before the action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    /// A detached runner waits, then runs the command; the node only learns it was handed over.
    Delayed,
    /// The command takes the delay itself and returns at once, so it is run and its answer read.
    SelfTimed,
}

/// A read-only question to the system: would it let this account carry out the action?
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// logind's `Can<Verb>` over `busctl`: `yes` allows.
    Logind(Vec<String>),
    /// `sudo -n -l <command>`: exit 0 allows.
    Sudo(Vec<String>),
    /// `id -u`: `0` allows.
    Root(Vec<String>),
}

impl Check {
    /// The command that asks.
    pub fn argv(&self) -> &[String] {
        match self {
            Check::Logind(argv) | Check::Sudo(argv) | Check::Root(argv) => argv,
        }
    }
}

/// How to carry out one action on one platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionPlan {
    /// The command.
    pub argv: Vec<String>,
    /// Who waits out the delay.
    pub timing: Timing,
    /// The question to ask first, when the answer would otherwise never come back.
    pub check: Option<Check>,
}

/// `["sudo", "-n", …argv]`: a password prompt fails at once instead of hanging the call.
pub fn with_sudo(argv: &[String]) -> Vec<String> {
    let mut wrapped = strings(&["sudo", "-n"]);
    wrapped.extend(argv.iter().cloned());
    wrapped
}

/// `argv` through `sudo -n` when `sudo` is set and the platform has it.
pub fn maybe_sudo(platform: Platform, sudo: bool, argv: Vec<String>) -> Vec<String> {
    if sudo && platform.has_sudo() {
        with_sudo(&argv)
    } else {
        argv
    }
}

/// Builds the plan for `action`. `find` looks a program up on `PATH` (Windows sleep needs one).
pub fn action_plan(
    platform: Platform,
    action: Action,
    delay_s: u32,
    sudo: bool,
    sleep_override: Option<&[String]>,
    find: &dyn Fn(&str) -> Option<PathBuf>,
) -> Result<ActionPlan, ErrorBody> {
    if action == Action::Sleep
        && let Some(argv) = sleep_override
    {
        // Written in node.toml: run exactly as written, after the delay.
        return Ok(ActionPlan {
            argv: argv.to_vec(),
            timing: Timing::Delayed,
            check: None,
        });
    }
    let delay = delay_s.to_string();
    match platform {
        Platform::Linux => {
            let plain = strings(&["systemctl", action.systemctl_verb()]);
            let check = if sudo {
                Check::Sudo(sudo_check(&plain))
            } else {
                Check::Logind(strings(&[
                    "busctl",
                    "call",
                    "org.freedesktop.login1",
                    "/org/freedesktop/login1",
                    "org.freedesktop.login1.Manager",
                    action.logind_method(),
                ]))
            };
            Ok(ActionPlan {
                argv: maybe_sudo(platform, sudo, plain),
                timing: Timing::Delayed,
                check: Some(check),
            })
        }
        Platform::Macos => {
            let plain = match action {
                Action::Sleep => {
                    // pmset needs no privilege: it is what the Apple menu does.
                    return Ok(ActionPlan {
                        argv: strings(&["pmset", "sleepnow"]),
                        timing: Timing::Delayed,
                        check: None,
                    });
                }
                Action::Restart => strings(&["shutdown", "-r", "now"]),
                Action::Shutdown => strings(&["shutdown", "-h", "now"]),
            };
            let check = if sudo {
                Check::Sudo(sudo_check(&plain))
            } else {
                Check::Root(strings(&["id", "-u"]))
            };
            Ok(ActionPlan {
                argv: maybe_sudo(platform, sudo, plain),
                timing: Timing::Delayed,
                check: Some(check),
            })
        }
        Platform::Windows => {
            let argv = match action {
                Action::Sleep => {
                    let tool = ["psshutdown64.exe", "psshutdown.exe"]
                        .into_iter()
                        .find_map(find)
                        .ok_or_else(no_psshutdown)?;
                    vec![
                        tool.to_string_lossy().into_owned(),
                        "-d".to_owned(),
                        "-t".to_owned(),
                        delay,
                        "-accepteula".to_owned(),
                    ]
                }
                Action::Restart => strings(&["shutdown", "/r", "/t", &delay]),
                Action::Shutdown => strings(&["shutdown", "/s", "/t", &delay]),
            };
            Ok(ActionPlan {
                argv,
                timing: Timing::SelfTimed,
                check: None,
            })
        }
    }
}

fn sudo_check(plain: &[String]) -> Vec<String> {
    let mut argv = strings(&["sudo", "-n", "-l"]);
    argv.extend(plain.iter().cloned());
    argv
}

fn no_psshutdown() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Unsupported,
        "Windows has no sleep command that works reliably without Sysinternals PsShutdown, and neither psshutdown64.exe nor psshutdown.exe is on PATH.",
    )
    .with_hint(
        "Install PsShutdown from Sysinternals somewhere on PATH, or set sleep = [program and arguments] under [power] in node.toml. The built-in rundll32 and .NET sleep calls are not used because some machines' power policy silently vetoes them: they report success and the machine stays awake.",
    )
}

/// What the system said to a [`Check`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// It would allow the action.
    Allowed,
    /// It would refuse; the text is what it said.
    Refused(String),
    /// The action is not available on this machine at all; the text says why.
    Unavailable(String),
    /// The question itself could not be asked or answered.
    Unknown(String),
}

/// Reads the answer to a check.
pub fn read_check(check: &Check, outcome: &Result<Ran, ErrorBody>) -> Answer {
    let ran = match outcome {
        Ok(ran) => ran,
        Err(error) => return Answer::Unknown(error.message.clone()),
    };
    if ran.timed_out {
        return Answer::Unknown(format!(
            "`{}` did not answer in time",
            check.argv().join(" ")
        ));
    }
    match check {
        Check::Logind(_) => {
            if !ran.ok() {
                return Answer::Unknown(format!(
                    "logind could not be asked: {}",
                    nonempty(ran.first_line(), "busctl failed")
                ));
            }
            // busctl prints the one string the method returns: s "yes".
            let said = ran
                .stdout
                .trim()
                .trim_start_matches('s')
                .trim()
                .trim_matches('"');
            match said {
                "yes" => Answer::Allowed,
                "na" => Answer::Unavailable(
                    "logind says this machine cannot do that at all (it may be disabled in its configuration)".to_owned(),
                ),
                "no" | "challenge" => Answer::Refused(format!(
                    "logind answered {said:?}: this account may not do it without authenticating"
                )),
                other => Answer::Unknown(format!("logind answered {other:?}")),
            }
        }
        Check::Sudo(_) => {
            if ran.ok() {
                Answer::Allowed
            } else {
                Answer::Refused(nonempty(
                    &ran.said(),
                    "sudo does not allow this command without a password",
                ))
            }
        }
        Check::Root(_) => match ran.stdout.trim() {
            "0" if ran.ok() => Answer::Allowed,
            _ if ran.ok() => {
                Answer::Refused("this needs root, and legion does not run as root".to_owned())
            }
            _ => Answer::Unknown(nonempty(ran.first_line(), "id -u failed").to_owned()),
        },
    }
}

fn nonempty(text: &str, otherwise: &str) -> String {
    if text.is_empty() {
        otherwise.to_owned()
    } else {
        text.to_owned()
    }
}

/// Whether a failed command's words are the system refusing this account, rather than the command
/// failing for some other reason.
pub fn looks_refused(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "permission denied",
        "operation not permitted",
        "password is required",
        "not in the sudoers file",
        "not allowed to execute",
        "sorry, user",
        "no tty present",
        "interactive authentication required",
        "access denied",
        "access is denied",
        "not super-user",
        "must be superuser",
        "must be run as root",
        "requires elevation",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// The `denied` error for a refusal: what was refused, what the system said, the exact command
/// that would be allowed in `details.argv`, and the setting that decides whether sudo is used.
pub fn refused(
    platform: Platform,
    what: &str,
    sudo: bool,
    argv: &[String],
    said: &str,
) -> ErrorBody {
    if platform == Platform::Windows {
        return ErrorBody::new(
            ErrorCode::Denied,
            format!("Windows refused to {what}: {said}."),
        )
        .with_hint(
            "Run legion elevated: an ssh login by a member of Administrators is, a desktop PowerShell is not. [power] sudo has no effect on Windows.",
        )
        .with_details(json!({"argv": argv, "said": said}));
    }
    let (shown, hint) = if sudo {
        (
            argv.to_vec(),
            "Allow the command in details.argv without a password (NOPASSWD) in sudoers, or set sudo = false under [power] in node.toml if this node should not escalate.",
        )
    } else {
        (
            with_sudo(argv),
            "Set sudo = true under [power] in node.toml and allow the command in details.argv without a password (NOPASSWD) in sudoers, so legion runs it through sudo -n. That lets every principal allowed the power tools do this as root.",
        )
    };
    ErrorBody::new(
        ErrorCode::Denied,
        format!("The system will not let this account {what}: {said}."),
    )
    .with_hint(hint)
    .with_details(json!({"argv": shown, "said": said}))
}

/// What a self-timed Windows `shutdown` or `psshutdown` answered, as an error when it refused.
pub fn read_self_timed(argv: &[String], what: &str, ran: &Ran) -> Result<(), ErrorBody> {
    if ran.ok() {
        return Ok(());
    }
    let said = nonempty(&ran.said(), "no message");
    let details: Value = json!({"argv": argv, "exit_code": ran.exit_code, "said": said});
    if ran.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "`{}` did not return, so whether the machine is going to {what} is unknown.",
                argv.join(" ")
            ),
        )
        .with_hint("Check the machine in a minute: if it still answers, the action did not take.")
        .with_details(details));
    }
    match ran.exit_code {
        // ERROR_ACCESS_DENIED
        Some(5) => Err(refused(Platform::Windows, what, false, argv, &said)),
        // ERROR_SHUTDOWN_IN_PROGRESS, ERROR_SHUTDOWN_IS_SCHEDULED
        Some(1115 | 1190) => Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "A restart or shutdown is already scheduled or under way on this machine ({said})."
            ),
        )
        .with_hint("Wait for it, or cancel it on the machine with `shutdown /a` and ask again.")
        .with_details(details)),
        _ if looks_refused(&said) => Err(refused(Platform::Windows, what, false, argv, &said)),
        _ => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("`{}` refused to {what}: {said}.", argv.join(" ")),
        )
        .with_details(details)),
    }
}

/// A list of string literals as owned strings.
pub fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn everything(_: &str) -> Option<PathBuf> {
        None
    }

    fn plan(platform: Platform, action: Action, sudo: bool) -> ActionPlan {
        action_plan(platform, action, 3, sudo, None, &|name| {
            Some(PathBuf::from(format!("C:\\Tools\\{name}")))
        })
        .unwrap()
    }

    #[test]
    fn linux_uses_systemd_and_asks_logind_or_sudo_first() {
        let plain = plan(Platform::Linux, Action::Restart, false);
        assert_eq!(plain.argv, strings(&["systemctl", "reboot"]));
        assert_eq!(plain.timing, Timing::Delayed);
        assert_eq!(
            plain.check,
            Some(Check::Logind(strings(&[
                "busctl",
                "call",
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                "CanReboot"
            ])))
        );
        let sudo = plan(Platform::Linux, Action::Shutdown, true);
        assert_eq!(sudo.argv, strings(&["sudo", "-n", "systemctl", "poweroff"]));
        assert_eq!(
            sudo.check,
            Some(Check::Sudo(strings(&[
                "sudo",
                "-n",
                "-l",
                "systemctl",
                "poweroff"
            ])))
        );
        let sleep = plan(Platform::Linux, Action::Sleep, false);
        assert_eq!(sleep.argv, strings(&["systemctl", "suspend"]));
        assert!(matches!(&sleep.check, Some(Check::Logind(argv)) if argv[5] == "CanSuspend"));
    }

    #[test]
    fn macos_sleeps_without_privilege_and_restarts_as_root() {
        let sleep = plan(Platform::Macos, Action::Sleep, true);
        assert_eq!(sleep.argv, strings(&["pmset", "sleepnow"]));
        assert_eq!(sleep.check, None);
        let restart = plan(Platform::Macos, Action::Restart, false);
        assert_eq!(restart.argv, strings(&["shutdown", "-r", "now"]));
        assert_eq!(restart.check, Some(Check::Root(strings(&["id", "-u"]))));
        let off = plan(Platform::Macos, Action::Shutdown, true);
        assert_eq!(off.argv, strings(&["sudo", "-n", "shutdown", "-h", "now"]));
        assert_eq!(
            off.check,
            Some(Check::Sudo(strings(&[
                "sudo", "-n", "-l", "shutdown", "-h", "now"
            ])))
        );
    }

    #[test]
    fn windows_lets_shutdown_and_psshutdown_take_the_delay_and_ignores_sudo() {
        let restart = plan(Platform::Windows, Action::Restart, true);
        assert_eq!(restart.argv, strings(&["shutdown", "/r", "/t", "3"]));
        assert_eq!(restart.timing, Timing::SelfTimed);
        assert_eq!(restart.check, None);
        let off = action_plan(
            Platform::Windows,
            Action::Shutdown,
            10,
            false,
            None,
            &everything,
        )
        .unwrap();
        assert_eq!(off.argv, strings(&["shutdown", "/s", "/t", "10"]));
        let sleep = plan(Platform::Windows, Action::Sleep, false);
        assert_eq!(
            sleep.argv,
            strings(&[
                "C:\\Tools\\psshutdown64.exe",
                "-d",
                "-t",
                "3",
                "-accepteula"
            ])
        );
        // Only the 32-bit one installed.
        let found = action_plan(Platform::Windows, Action::Sleep, 3, false, None, &|name| {
            (name == "psshutdown.exe").then(|| PathBuf::from("psshutdown.exe"))
        })
        .unwrap();
        assert_eq!(found.argv[0], "psshutdown.exe");
        let missing = action_plan(
            Platform::Windows,
            Action::Sleep,
            3,
            false,
            None,
            &everything,
        )
        .unwrap_err();
        assert_eq!(missing.code, ErrorCode::Unsupported);
        assert!(missing.hint.unwrap().contains("silently vetoes"));
    }

    #[test]
    fn a_sleep_command_from_node_toml_runs_as_written_on_every_platform() {
        let custom = strings(&["systemctl", "hibernate"]);
        for platform in [Platform::Linux, Platform::Macos, Platform::Windows] {
            let plan =
                action_plan(platform, Action::Sleep, 3, true, Some(&custom), &everything).unwrap();
            assert_eq!(plan.argv, custom);
            assert_eq!(plan.timing, Timing::Delayed);
            assert_eq!(plan.check, None);
        }
    }

    #[test]
    fn check_answers_are_read_strictly() {
        let logind = Check::Logind(strings(&["busctl"]));
        let answer = |check: &Check, ran: Ran| read_check(check, &Ok(ran));
        assert_eq!(
            answer(&logind, Ran::exited(0, "s \"yes\"\n")),
            Answer::Allowed
        );
        assert!(matches!(
            answer(&logind, Ran::exited(0, "s \"challenge\"\n")),
            Answer::Refused(_)
        ));
        assert!(matches!(
            answer(&logind, Ran::exited(0, "s \"no\"\n")),
            Answer::Refused(_)
        ));
        assert!(matches!(
            answer(&logind, Ran::exited(0, "s \"na\"\n")),
            Answer::Unavailable(_)
        ));
        assert!(matches!(
            answer(&logind, Ran::exited(0, "")),
            Answer::Unknown(_)
        ));
        assert!(matches!(
            answer(&logind, Ran::exited(1, "Failed to connect to bus")),
            Answer::Unknown(_)
        ));

        let sudo = Check::Sudo(strings(&["sudo", "-n", "-l", "systemctl", "reboot"]));
        assert_eq!(
            answer(&sudo, Ran::exited(0, "/usr/bin/systemctl reboot\n")),
            Answer::Allowed
        );
        let refused = Ran {
            stderr: "sudo: a password is required\n".to_owned(),
            ..Ran::exited(1, "")
        };
        assert_eq!(
            answer(&sudo, refused),
            Answer::Refused("sudo: a password is required".to_owned())
        );
        assert!(matches!(
            read_check(
                &sudo,
                &Err(ErrorBody::new(ErrorCode::Unsupported, "no sudo"))
            ),
            Answer::Unknown(_)
        ));

        let root = Check::Root(strings(&["id", "-u"]));
        assert_eq!(answer(&root, Ran::exited(0, "0\n")), Answer::Allowed);
        assert!(matches!(
            answer(&root, Ran::exited(0, "501\n")),
            Answer::Refused(_)
        ));
    }

    #[test]
    fn a_refusal_names_the_setting_and_the_exact_command() {
        let plain = strings(&["systemctl", "reboot"]);
        let without = refused(
            Platform::Linux,
            "restart this machine",
            false,
            &plain,
            "Interactive authentication required",
        );
        assert_eq!(without.code, ErrorCode::Denied);
        assert!(
            without
                .hint
                .as_deref()
                .unwrap()
                .contains("sudo = true under [power]")
        );
        assert_eq!(
            without.details.as_ref().unwrap()["argv"],
            json!(["sudo", "-n", "systemctl", "reboot"])
        );
        let with = refused(
            Platform::Linux,
            "restart this machine",
            true,
            &with_sudo(&plain),
            "sudo: a password is required",
        );
        assert!(with.hint.as_deref().unwrap().contains("NOPASSWD"));
        assert_eq!(
            with.details.as_ref().unwrap()["argv"],
            json!(["sudo", "-n", "systemctl", "reboot"])
        );
        let windows = refused(
            Platform::Windows,
            "restart this machine",
            false,
            &plain,
            "x",
        );
        assert!(windows.hint.unwrap().contains("elevated"));
    }

    #[test]
    fn windows_shutdown_answers_map_to_their_codes() {
        let argv = strings(&["shutdown", "/r", "/t", "3"]);
        assert!(read_self_timed(&argv, "restart", &Ran::exited(0, "")).is_ok());
        let denied = Ran {
            stderr: "Access is denied.(5)\n".to_owned(),
            ..Ran::exited(5, "")
        };
        assert_eq!(
            read_self_timed(&argv, "restart", &denied).unwrap_err().code,
            ErrorCode::Denied
        );
        let scheduled = Ran::exited(1190, "A system shutdown has already been scheduled.(1190)");
        assert_eq!(
            read_self_timed(&argv, "restart", &scheduled)
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(
            read_self_timed(
                &argv,
                "restart",
                &Ran::exited(87, "The parameter is incorrect.")
            )
            .unwrap_err()
            .code,
            ErrorCode::Failed
        );
        let hung = Ran {
            timed_out: true,
            ..Ran::default()
        };
        assert_eq!(
            read_self_timed(&argv, "restart", &hung).unwrap_err().code,
            ErrorCode::Timeout
        );
    }

    #[test]
    fn refusals_are_told_from_other_failures() {
        for refusal in [
            "sudo: a password is required",
            "Could not set BootNext: Permission denied",
            "Failed to reboot system via logind: Interactive authentication required.",
            "shutdown: NOT super-user",
            "The requested operation requires elevation.",
            "Access is denied.",
        ] {
            assert!(looks_refused(refusal), "{refusal}");
        }
        for other in [
            "EFI variables are not supported on this system.",
            "Invalid BootNext value",
            "",
        ] {
            assert!(!looks_refused(other), "{other}");
        }
    }
}
