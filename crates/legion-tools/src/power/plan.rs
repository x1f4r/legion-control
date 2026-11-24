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
use serde::{Deserialize, Serialize};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The action as the result spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Sleep => "sleep",
            Action::Restart => "restart",
            Action::Shutdown => "shutdown",
        }
    }

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
    /// logind's `Can<Verb>` over `busctl`: whether this account may, and whether an inhibitor
    /// holds the action back. Asked whatever `[power] sudo` says, because a block inhibitor
    /// refuses root too.
    Logind(Vec<String>),
    /// `sudo -n -l -l <command>`: the rule that matches the command must let it run without a
    /// password. A plain `sudo -l` is not enough: with sudoers' default `listpw=any`, any
    /// unrelated NOPASSWD rule lets the listing succeed for a command that needs a password.
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
    /// The questions to ask first, in order, because the answer would otherwise never come back.
    pub checks: Vec<Check>,
    /// Whether the command takes the delay itself, so the runner starting it does not wait.
    pub waits_itself: bool,
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

/// Builds the plan for `action`. `ignore_inhibitors` adds systemctl's own `-i`
/// (`--check-inhibitors=no`). `find` looks a program up on `PATH` (Windows sleep needs one).
pub fn action_plan(
    platform: Platform,
    action: Action,
    delay_s: u32,
    sudo: bool,
    ignore_inhibitors: bool,
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
            checks: Vec::new(),
            waits_itself: false,
        });
    }
    let delay = delay_s.to_string();
    match platform {
        Platform::Linux => {
            let mut plain = strings(&["systemctl"]);
            if ignore_inhibitors {
                plain.push("-i".to_owned());
            }
            plain.push(action.systemctl_verb().to_owned());
            let mut checks = vec![Check::Logind(strings(&[
                "busctl",
                "call",
                "org.freedesktop.login1",
                "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager",
                action.logind_method(),
            ]))];
            if sudo {
                checks.push(Check::Sudo(sudo_check(&plain)));
            }
            Ok(ActionPlan {
                argv: maybe_sudo(platform, sudo, plain),
                timing: Timing::Delayed,
                checks,
                waits_itself: false,
            })
        }
        Platform::Macos => {
            let plain = match action {
                // pmset needs no privilege from the desktop, where it is what the Apple menu
                // does, but may over ssh: with [power] sudo it goes through sudo like the rest.
                Action::Sleep if !sudo => {
                    return Ok(ActionPlan {
                        argv: strings(&["pmset", "sleepnow"]),
                        timing: Timing::Delayed,
                        checks: Vec::new(),
                        waits_itself: false,
                    });
                }
                Action::Sleep => strings(&["pmset", "sleepnow"]),
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
                checks: vec![check],
                waits_itself: false,
            })
        }
        Platform::Windows => {
            let argv = match action {
                Action::Sleep => {
                    let tool = ["psshutdown64.exe", "psshutdown.exe"]
                        .into_iter()
                        .find_map(find)
                        .ok_or_else(no_psshutdown)?;
                    // psshutdown returns at once and leaves the countdown to a process of its
                    // own. Started by the node, it would run in the job object `run` contains
                    // its commands in, which is torn down, that process with it, when psshutdown
                    // exits; started by the detached runner it is outside any such job. The
                    // runner does not wait first: psshutdown takes the delay itself, and needs a
                    // delay, since with -t 0 it never returns in a session without a desktop.
                    return Ok(ActionPlan {
                        argv: vec![
                            tool.to_string_lossy().into_owned(),
                            "-d".to_owned(),
                            "-t".to_owned(),
                            delay,
                            "-accepteula".to_owned(),
                        ],
                        timing: Timing::Delayed,
                        checks: Vec::new(),
                        waits_itself: true,
                    });
                }
                Action::Restart => strings(&["shutdown", "/r", "/t", &delay]),
                Action::Shutdown => strings(&["shutdown", "/s", "/t", &delay]),
            };
            Ok(ActionPlan {
                argv,
                timing: Timing::SelfTimed,
                checks: Vec::new(),
                waits_itself: false,
            })
        }
    }
}

fn sudo_check(plain: &[String]) -> Vec<String> {
    let mut argv = strings(&["sudo", "-n", "-l", "-l"]);
    argv.extend(plain.iter().cloned());
    argv
}

/// What sudo's long listing of one command says about its password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SudoRule {
    /// The rule that decides lets it run without a password; the text names where it is.
    NoPassword(String),
    /// The rule that decides wants a password; the text names where it is.
    Password(String),
    /// No rule could be read from the listing.
    Unreadable,
}

/// Reads `sudo -l -l <command>`: one entry per matching rule, each opening with `Sudoers entry:`
/// (or `LDAP Role:`), with an `Options:` line holding `!authenticate` for NOPASSWD. sudoers uses
/// the last rule that matches, so the last entry decides.
pub fn parse_sudo_listing(text: &str) -> SudoRule {
    let mut decided = SudoRule::Unreadable;
    let mut current: Option<(String, bool)> = None;
    let settle = |entry: Option<(String, bool)>, decided: &mut SudoRule| {
        if let Some((source, no_password)) = entry {
            *decided = if no_password {
                SudoRule::NoPassword(source)
            } else {
                SudoRule::Password(source)
            };
        }
    };
    for line in text.lines() {
        let trimmed = line.trim();
        let header = trimmed
            .strip_prefix("Sudoers entry:")
            .or_else(|| trimmed.strip_prefix("LDAP Role:"));
        if let Some(source) = header {
            settle(current.take(), &mut decided);
            current = Some((source.trim().to_owned(), false));
        } else if let (Some(options), Some((_, no_password))) =
            (trimmed.strip_prefix("Options:"), current.as_mut())
        {
            *no_password = options
                .split(',')
                .any(|option| option.trim() == "!authenticate");
        }
    }
    settle(current, &mut decided);
    decided
}

/// The command a `sudo -l -l <command>` listing names when it shows only that, as sudo-rs (the
/// `sudo` of Ubuntu 26.04 and later) does: its full path and arguments on one line, where the
/// original sudo prints the rule that matched.
pub fn matched_command(listing: &str) -> Option<String> {
    let line = listing
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    (line.starts_with('/') && !line.contains(':')).then(|| line.to_owned())
}

/// `sudo -n -l -l` with no command: every rule this account has, in the long format.
pub fn sudo_rules_command() -> Vec<String> {
    strings(&["sudo", "-n", "-l", "-l"])
}

/// Reads the whole long listing for the rule that decides `matched` (a full path and its
/// arguments): sudoers uses the last rule that matches, and a rule matches when its commands hold
/// `ALL`, the command line itself, or its program alone, which allows any arguments, and it runs
/// commands as root. A rule this does not read for certain (a wildcard, a negation, an alias) that
/// could be about this program makes the answer unreadable rather than a guess.
pub fn rule_for(listing: &str, matched: &str) -> SudoRule {
    let program = matched.split_whitespace().next().unwrap_or(matched);
    let mut decided = SudoRule::Unreadable;
    let mut entry: Option<RuleEntry> = None;
    let mut in_commands = false;
    let settle = |entry: Option<RuleEntry>, decided: &mut SudoRule| -> bool {
        let Some(entry) = entry else { return true };
        if entry.unsure {
            *decided = SudoRule::Unreadable;
            return false;
        }
        if let Some(no_password) = entry.matched
            && entry.as_root
        {
            *decided = if no_password {
                SudoRule::NoPassword(entry.source)
            } else {
                SudoRule::Password(entry.source)
            };
        }
        true
    };
    for line in listing.lines() {
        let trimmed = line.trim();
        let header = trimmed
            .strip_prefix("Sudoers entry:")
            .or_else(|| trimmed.strip_prefix("LDAP Role:"));
        if let Some(source) = header {
            if !settle(entry.take(), &mut decided) {
                return SudoRule::Unreadable;
            }
            entry = Some(RuleEntry {
                source: source.trim().to_owned(),
                ..RuleEntry::default()
            });
            in_commands = false;
            continue;
        }
        let Some(current) = entry.as_mut() else {
            continue;
        };
        if let Some(options) = trimmed.strip_prefix("Options:") {
            current.no_password = options
                .split(',')
                .any(|option| option.trim() == "!authenticate");
        } else if let Some(users) = trimmed.strip_prefix("RunAsUsers:") {
            current.as_root = users
                .split(',')
                .any(|user| matches!(user.trim(), "ALL" | "root"));
        } else if trimmed == "Commands:" {
            in_commands = true;
        } else if in_commands && !trimmed.is_empty() && !trimmed.ends_with(':') {
            let (tag, command) = match trimmed.split_once(": ") {
                Some((tag @ ("NOPASSWD" | "PASSWD"), rest)) => (Some(tag), rest.trim()),
                _ => (None, trimmed),
            };
            let no_password = match tag {
                Some("NOPASSWD") => true,
                Some(_) => false,
                None => current.no_password,
            };
            match command_matches(command, matched, program) {
                Some(true) => current.matched = Some(no_password),
                Some(false) => {}
                None => current.unsure = true,
            }
        } else if !trimmed.is_empty() {
            in_commands = false;
        }
    }
    if !settle(entry, &mut decided) {
        return SudoRule::Unreadable;
    }
    decided
}

/// Whether one command of a sudo rule allows `matched` (`program` and its arguments): `Some` when
/// that is certain, `None` for a negation, a wildcard or an alias that could be about it.
fn command_matches(command: &str, matched: &str, program: &str) -> Option<bool> {
    let negated = command.starts_with('!');
    let body = command.trim_start_matches('!').trim();
    let first = body.split_whitespace().next().unwrap_or(body);
    let plain = first.starts_with('/') && !body.contains(['*', '?', '[', '\\']);
    if negated || !(plain || body == "ALL") {
        // Not something read here for certain: about this program, or anything at all, it decides
        // nothing that can be trusted; about another program, it does not matter.
        let other = first.starts_with('/') && !first.contains(['*', '?', '[']) && first != program;
        return if other { Some(false) } else { None };
    }
    Some(body == "ALL" || body == matched || body == program)
}

/// One rule of a long sudo listing, as [`rule_for`] reads it.
#[derive(Debug)]
struct RuleEntry {
    source: String,
    /// Whether its options say no password (`!authenticate`).
    no_password: bool,
    /// Whether it runs commands as root; true until its RunAsUsers says otherwise.
    as_root: bool,
    /// Whether one of its commands matched, and if so whether without a password.
    matched: Option<bool>,
    /// Whether it says something about the program this cannot read for certain.
    unsure: bool,
}

impl Default for RuleEntry {
    fn default() -> Self {
        RuleEntry {
            source: String::new(),
            no_password: false,
            as_root: true,
            matched: None,
            unsure: false,
        }
    }
}

/// The answer the whole listing gives for `matched`, after the listing for the command itself
/// named only the command.
pub fn read_rules(listed: &Result<Ran, ErrorBody>, matched: &str) -> Answer {
    let ran = match listed {
        Ok(ran) if ran.ok() => ran,
        Ok(ran) => {
            return Answer::Unknown(format!(
                "sudo listed the command but not its rules: {}",
                nonempty(ran.first_line(), "sudo -l -l failed")
            ));
        }
        Err(error) => return Answer::Unknown(error.message.clone()),
    };
    match rule_for(&ran.stdout, matched) {
        SudoRule::NoPassword(_) => Answer::Allowed,
        SudoRule::Password(source) => Answer::Refused(format!(
            "the sudo rule for it{} asks for a password, and a command run after the reply has nobody to type one",
            if source.is_empty() {
                String::new()
            } else {
                format!(" ({source})")
            }
        )),
        SudoRule::Unreadable => Answer::Unknown(format!(
            "sudo allows {matched}, but which of its rules decides whether that needs a password could not be read for certain"
        )),
    }
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
    /// logind holds the action back for an inhibitor (`inhibited`, `inhibitor-blocked`), or
    /// for one and authentication besides (`challenge-inhibitor-blocked`).
    Inhibited {
        /// What logind answered.
        said: String,
        /// Whether this account would also have to authenticate.
        challenge: bool,
    },
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
                "inhibited" | "inhibitor-blocked" | "challenge-inhibitor-blocked" => {
                    Answer::Inhibited {
                        said: said.to_owned(),
                        challenge: said.starts_with("challenge"),
                    }
                }
                other => Answer::Unknown(format!("logind answered {other:?}")),
            }
        }
        Check::Sudo(_) => {
            if !ran.ok() {
                return Answer::Refused(nonempty(
                    &ran.said(),
                    "sudo does not allow this command without a password",
                ));
            }
            match parse_sudo_listing(&ran.stdout) {
                SudoRule::NoPassword(_) => Answer::Allowed,
                SudoRule::Password(source) => Answer::Refused(format!(
                    "the sudo rule for it ({source}) asks for a password, and a command run after the reply has nobody to type one"
                )),
                SudoRule::Unreadable => Answer::Unknown(
                    "sudo allowed the listing but did not show the rule for the command, so whether it needs a password is unknown".to_owned(),
                ),
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

/// One logind inhibitor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Inhibitor {
    /// What it holds back, such as `shutdown:sleep`.
    pub what: String,
    /// Who holds it.
    pub who: String,
    /// Why, in its own words.
    pub why: String,
    /// `block`, or `delay` (which only postpones).
    pub mode: String,
    /// The user it runs as.
    pub uid: u64,
    /// Its process.
    pub pid: u64,
}

/// The read-only command that lists logind's inhibitors, as JSON.
pub fn inhibitors_command() -> Vec<String> {
    strings(&[
        "busctl",
        "--json=short",
        "call",
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "ListInhibitors",
    ])
}

/// The block inhibitors that hold `action` back, from `busctl --json=short` output of
/// `ListInhibitors` (`{"type":"a(ssssuu)","data":[[[what, who, why, mode, uid, pid], …]]}`).
pub fn parse_inhibitors(text: &str, action: Action) -> Vec<Inhibitor> {
    let category = match action {
        Action::Sleep => "sleep",
        Action::Restart | Action::Shutdown => "shutdown",
    };
    let parsed: Value = serde_json::from_str(text.trim()).unwrap_or(Value::Null);
    let rows = parsed["data"]
        .get(0)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| {
            let field = |index: usize| row.get(index).and_then(Value::as_str).map(str::to_owned);
            let number = |index: usize| row.get(index).and_then(Value::as_u64).unwrap_or(0);
            Some(Inhibitor {
                what: field(0)?,
                who: field(1)?,
                why: field(2)?,
                mode: field(3)?,
                uid: number(4),
                pid: number(5),
            })
        })
        .filter(|inhibitor| {
            inhibitor.mode == "block" && inhibitor.what.split(':').any(|what| what == category)
        })
        .collect()
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
        .with_hint("Wait for it, or call it off with power.cancel (or `shutdown /a` on the machine for one Legion did not start) and ask again.")
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
        action_plan(platform, action, 3, sudo, false, None, &|name| {
            Some(PathBuf::from(format!("C:\\Tools\\{name}")))
        })
        .unwrap()
    }

    fn logind(method: &str) -> Check {
        Check::Logind(strings(&[
            "busctl",
            "call",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            method,
        ]))
    }

    #[test]
    fn linux_uses_systemd_and_asks_logind_always_and_sudo_when_set() {
        let plain = plan(Platform::Linux, Action::Restart, false);
        assert_eq!(plain.argv, strings(&["systemctl", "reboot"]));
        assert_eq!(plain.timing, Timing::Delayed);
        assert_eq!(plain.checks, [logind("CanReboot")]);
        // A block inhibitor refuses root too, so logind is asked whatever sudo says.
        let sudo = plan(Platform::Linux, Action::Shutdown, true);
        assert_eq!(sudo.argv, strings(&["sudo", "-n", "systemctl", "poweroff"]));
        assert_eq!(
            sudo.checks,
            [
                logind("CanPowerOff"),
                Check::Sudo(strings(&[
                    "sudo",
                    "-n",
                    "-l",
                    "-l",
                    "systemctl",
                    "poweroff"
                ]))
            ]
        );
        let sleep = plan(Platform::Linux, Action::Sleep, false);
        assert_eq!(sleep.argv, strings(&["systemctl", "suspend"]));
        assert_eq!(sleep.checks, [logind("CanSuspend")]);
        let past = action_plan(
            Platform::Linux,
            Action::Sleep,
            3,
            true,
            true,
            None,
            &everything,
        )
        .unwrap();
        assert_eq!(
            past.argv,
            strings(&["sudo", "-n", "systemctl", "-i", "suspend"])
        );
        assert_eq!(
            past.checks[1],
            Check::Sudo(strings(&[
                "sudo",
                "-n",
                "-l",
                "-l",
                "systemctl",
                "-i",
                "suspend"
            ]))
        );
    }

    #[test]
    fn macos_sleeps_through_sudo_when_told_and_restarts_as_root() {
        let sleep = plan(Platform::Macos, Action::Sleep, false);
        assert_eq!(sleep.argv, strings(&["pmset", "sleepnow"]));
        assert!(sleep.checks.is_empty());
        // Over ssh pmset may need root: [power] sudo applies, with the same check.
        let sudo_sleep = plan(Platform::Macos, Action::Sleep, true);
        assert_eq!(
            sudo_sleep.argv,
            strings(&["sudo", "-n", "pmset", "sleepnow"])
        );
        assert_eq!(
            sudo_sleep.checks,
            [Check::Sudo(strings(&[
                "sudo", "-n", "-l", "-l", "pmset", "sleepnow"
            ]))]
        );
        let restart = plan(Platform::Macos, Action::Restart, false);
        assert_eq!(restart.argv, strings(&["shutdown", "-r", "now"]));
        assert_eq!(restart.checks, [Check::Root(strings(&["id", "-u"]))]);
        let off = plan(Platform::Macos, Action::Shutdown, true);
        assert_eq!(off.argv, strings(&["sudo", "-n", "shutdown", "-h", "now"]));
        assert_eq!(
            off.checks,
            [Check::Sudo(strings(&[
                "sudo", "-n", "-l", "-l", "shutdown", "-h", "now"
            ]))]
        );
    }

    #[test]
    fn windows_lets_shutdown_and_psshutdown_take_the_delay_and_ignores_sudo() {
        let restart = plan(Platform::Windows, Action::Restart, true);
        assert_eq!(restart.argv, strings(&["shutdown", "/r", "/t", "3"]));
        assert_eq!(restart.timing, Timing::SelfTimed);
        assert!(restart.checks.is_empty());
        let off = action_plan(
            Platform::Windows,
            Action::Shutdown,
            10,
            false,
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
        // Started by the detached runner, which does not wait first: psshutdown waits itself.
        assert_eq!(sleep.timing, Timing::Delayed);
        assert!(sleep.waits_itself);
        // Only the 32-bit one installed.
        let found = action_plan(
            Platform::Windows,
            Action::Sleep,
            3,
            false,
            false,
            None,
            &|name| (name == "psshutdown.exe").then(|| PathBuf::from("psshutdown.exe")),
        )
        .unwrap();
        assert_eq!(found.argv[0], "psshutdown.exe");
        let missing = action_plan(
            Platform::Windows,
            Action::Sleep,
            3,
            false,
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
            let plan = action_plan(
                platform,
                Action::Sleep,
                3,
                true,
                true,
                Some(&custom),
                &everything,
            )
            .unwrap();
            assert_eq!(plan.argv, custom);
            assert_eq!(plan.timing, Timing::Delayed);
            assert!(plan.checks.is_empty());
        }
    }

    /// `sudo -l -l systemctl reboot` where a NOPASSWD rule matches.
    const NOPASSWD_RULE: &str = "Sudoers entry: /etc/sudoers.d/power\n    RunAsUsers: root\n    Options: !authenticate\n    Commands:\n\t/usr/bin/systemctl reboot\n    Matched: /usr/bin/systemctl reboot\n";

    /// The same where only the group's password rule matches.
    const PASSWORD_RULE: &str = "Sudoers entry: /etc/sudoers\n    RunAsUsers: ALL\n    Commands:\n\tALL\n    Matched: /usr/bin/systemctl reboot\n";

    #[test]
    fn the_rule_that_decides_says_whether_a_password_is_needed() {
        assert_eq!(
            parse_sudo_listing(NOPASSWD_RULE),
            SudoRule::NoPassword("/etc/sudoers.d/power".to_owned())
        );
        assert_eq!(
            parse_sudo_listing(PASSWORD_RULE),
            SudoRule::Password("/etc/sudoers".to_owned())
        );
        // Both match: sudoers uses the last one.
        assert_eq!(
            parse_sudo_listing(&format!("{PASSWORD_RULE}\n{NOPASSWD_RULE}")),
            SudoRule::NoPassword("/etc/sudoers.d/power".to_owned())
        );
        assert_eq!(
            parse_sudo_listing(&format!("{NOPASSWD_RULE}\n{PASSWORD_RULE}")),
            SudoRule::Password("/etc/sudoers".to_owned())
        );
        // Options other than NOPASSWD, or the opposite, do not count.
        let setenv = NOPASSWD_RULE.replace("!authenticate", "setenv, authenticate");
        assert_eq!(
            parse_sudo_listing(&setenv),
            SudoRule::Password("/etc/sudoers.d/power".to_owned())
        );
        let several = NOPASSWD_RULE.replace("!authenticate", "setenv, !authenticate");
        assert!(matches!(
            parse_sudo_listing(&several),
            SudoRule::NoPassword(_)
        ));
        // No rule at all.
        assert_eq!(parse_sudo_listing(""), SudoRule::Unreadable);
        assert_eq!(
            parse_sudo_listing("/usr/bin/systemctl reboot\n"),
            SudoRule::Unreadable
        );
    }

    /// What sudo-rs 0.2 prints for `sudo -n -l -l` and `sudo -n -l -l <command>` on Ubuntu 26.04.
    const SUDO_RS_ALL: &str = "User runner may run the following commands on host:\n\nSudoers entry:\n    RunAsUsers: ALL\n    Options: !authenticate\n    Commands:\n\tALL\n";

    #[test]
    fn a_listing_that_names_only_the_command_is_read_from_the_rules() {
        assert_eq!(
            matched_command("/usr/bin/systemctl reboot\n").as_deref(),
            Some("/usr/bin/systemctl reboot")
        );
        assert_eq!(matched_command("Sudoers entry: /etc/sudoers\n"), None);
        assert_eq!(matched_command(""), None);
        let matched = "/usr/bin/systemctl reboot";
        assert!(matches!(
            rule_for(SUDO_RS_ALL, matched),
            SudoRule::NoPassword(_)
        ));
        // The last rule that matches decides, a narrow one included.
        let narrow = "Sudoers entry: /etc/sudoers\n    RunAsUsers: ALL\n    Commands:\n\tALL\n\nSudoers entry: /etc/sudoers.d/power\n    RunAsUsers: root\n    Options: !authenticate\n    Commands:\n\t/usr/bin/systemctl reboot\n\t/usr/bin/systemctl poweroff\n";
        assert_eq!(
            rule_for(narrow, matched),
            SudoRule::NoPassword("/etc/sudoers.d/power".to_owned())
        );
        assert_eq!(
            rule_for(narrow, "/usr/bin/systemctl suspend"),
            SudoRule::Password("/etc/sudoers".to_owned())
        );
        // The program alone allows any arguments; a rule for another program is beside the point.
        let program = "Sudoers entry:\n    RunAsUsers: ALL\n    Options: !authenticate\n    Commands:\n\t/usr/bin/systemctl\n\t/usr/sbin/shutdown -h now\n";
        assert!(matches!(
            rule_for(program, matched),
            SudoRule::NoPassword(_)
        ));
        // A rule for another user does not run the command as root.
        let other_user = "Sudoers entry:\n    RunAsUsers: backup\n    Options: !authenticate\n    Commands:\n\tALL\n";
        assert_eq!(rule_for(other_user, matched), SudoRule::Unreadable);
        // A wildcard, negation or alias about it is not guessed at.
        for unsure in [
            "\t/usr/bin/systemctl *\n",
            "\t!/usr/bin/systemctl reboot\n",
            "\tPOWER_COMMANDS\n",
        ] {
            let listing = format!(
                "{SUDO_RS_ALL}\nSudoers entry:\n    RunAsUsers: ALL\n    Options: !authenticate\n    Commands:\n{unsure}"
            );
            assert_eq!(
                rule_for(&listing, matched),
                SudoRule::Unreadable,
                "{unsure}"
            );
        }
        let password = SUDO_RS_ALL.replace("    Options: !authenticate\n", "");
        assert!(matches!(
            rule_for(&password, matched),
            SudoRule::Password(_)
        ));
        assert_eq!(
            read_rules(&Ok(Ran::exited(0, SUDO_RS_ALL)), matched),
            Answer::Allowed
        );
        assert!(matches!(
            read_rules(&Ok(Ran::exited(0, &password)), matched),
            Answer::Refused(_)
        ));
        assert!(matches!(
            read_rules(&Ok(Ran::exited(1, "")), matched),
            Answer::Unknown(_)
        ));
    }

    #[test]
    fn logind_inhibitor_answers_are_told_apart() {
        let check = logind("CanSuspend");
        let answer = |said: &str| read_check(&check, &Ok(Ran::exited(0, said)));
        assert_eq!(
            answer("s \"inhibitor-blocked\"\n"),
            Answer::Inhibited {
                said: "inhibitor-blocked".to_owned(),
                challenge: false
            }
        );
        assert_eq!(
            answer("s \"inhibited\"\n"),
            Answer::Inhibited {
                said: "inhibited".to_owned(),
                challenge: false
            }
        );
        assert_eq!(
            answer("s \"challenge-inhibitor-blocked\"\n"),
            Answer::Inhibited {
                said: "challenge-inhibitor-blocked".to_owned(),
                challenge: true
            }
        );
    }

    #[test]
    fn only_block_inhibitors_of_the_action_are_named() {
        let listed = r#"{"type":"a(ssssuu)","data":[[["shutdown:sleep","Updater","Installing packages","block",0,812],["sleep","Media Player","Playing","block",1000,2044],["handle-lid-switch","Desktop","Lid handled","block",1000,1500],["sleep","Network","Bring links down","delay",0,600]]]}"#;
        let sleep = parse_inhibitors(listed, Action::Sleep);
        let who: Vec<&str> = sleep.iter().map(|i| i.who.as_str()).collect();
        assert_eq!(who, ["Updater", "Media Player"]);
        assert_eq!(sleep[0].pid, 812);
        let restart = parse_inhibitors(listed, Action::Restart);
        assert_eq!(restart.len(), 1);
        assert_eq!(restart[0].why, "Installing packages");
        assert!(parse_inhibitors("not json", Action::Sleep).is_empty());
        assert!(parse_inhibitors(r#"{"type":"a(ssssuu)","data":[[]]}"#, Action::Sleep).is_empty());
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

        let sudo = Check::Sudo(strings(&["sudo", "-n", "-l", "-l", "systemctl", "reboot"]));
        assert_eq!(
            answer(&sudo, Ran::exited(0, NOPASSWD_RULE)),
            Answer::Allowed
        );
        assert!(matches!(
            answer(&sudo, Ran::exited(0, PASSWORD_RULE)),
            Answer::Refused(said) if said.contains("/etc/sudoers") && said.contains("password")
        ));
        // A short listing names the command but not its rule: whether it needs a password is
        // unknown, which is not the same as no.
        assert!(matches!(
            answer(&sudo, Ran::exited(0, "/usr/bin/systemctl reboot\n")),
            Answer::Unknown(_)
        ));
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
