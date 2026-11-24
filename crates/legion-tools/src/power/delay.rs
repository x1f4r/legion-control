//! `legion power-delay <seconds> -- <program> [args…]`: the detached process a scheduled power
//! action runs in.
//!
//! A power action has to happen after the reply that announces it has left, or the caller never
//! hears that it was accepted. A task inside the node would die with the session that asked, so
//! the node re-executes its own binary, detached through `legion-os`, which waits and then runs
//! the command exactly as given: no shell, nothing quoted. What it prints, and the command's own
//! output, go to the log the node names (`state/power/scheduled.log`).

use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

/// The first argument that makes `legion` this runner instead of a command line.
pub const DELAY_MODE: &str = "power-delay";

/// The longest delay the runner accepts; the tools give at most a minute.
const LONGEST_DELAY_S: u64 = 3600;

/// Runs the delayed action when `args` (the process arguments after the program name) start with
/// [`DELAY_MODE`], returning its exit code; `None` for any other command line.
pub fn try_delay_cli(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut args = args.into_iter();
    if args.next()? != DELAY_MODE {
        return None;
    }
    let rest: Vec<OsString> = args.collect();
    let Some((delay, argv)) = parse(&rest) else {
        say("usage: legion power-delay <seconds> -- <program> [args...]");
        return Some(ExitCode::from(2));
    };
    let shown: Vec<String> = argv
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    say(&format!(
        "legion power-delay: running {shown:?} in {} s",
        delay.as_secs()
    ));
    std::thread::sleep(delay);
    let Some((program, args)) = argv.split_first() else {
        return Some(ExitCode::from(2));
    };
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .status();
    match status {
        Ok(status) => {
            say(&format!("legion power-delay: {shown:?} ended: {status}"));
            let code = status.code().unwrap_or(1).clamp(0, 255);
            Some(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
        Err(error) => {
            say(&format!(
                "legion power-delay: {shown:?} could not be started: {error}"
            ));
            Some(ExitCode::from(1))
        }
    }
}

/// `<seconds> -- <program> [args…]`.
fn parse(rest: &[OsString]) -> Option<(Duration, &[OsString])> {
    let (seconds, rest) = rest.split_first()?;
    let seconds: u64 = seconds.to_str()?.parse().ok()?;
    let (separator, argv) = rest.split_first()?;
    if separator != "--" || argv.is_empty() || seconds > LONGEST_DELAY_S {
        return None;
    }
    Some((Duration::from_secs(seconds), argv))
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_its_own_mode_is_taken() {
        assert!(try_delay_cli(os(&["job-supervise", "x"])).is_none());
        assert!(try_delay_cli(os(&["meta.version"])).is_none());
        assert!(try_delay_cli(os(&[])).is_none());
    }

    #[test]
    fn the_delay_and_the_command_are_read_exactly() {
        let args = os(&["3", "--", "sudo", "-n", "systemctl", "reboot"]);
        let (delay, argv) = parse(&args).unwrap();
        assert_eq!(delay, Duration::from_secs(3));
        assert_eq!(argv, os(&["sudo", "-n", "systemctl", "reboot"]).as_slice());
        for wrong in [
            os(&["3", "systemctl", "reboot"]),
            os(&["3", "--"]),
            os(&["x", "--", "reboot"]),
            os(&["-1", "--", "reboot"]),
            os(&["3601", "--", "reboot"]),
            os(&[]),
        ] {
            assert!(parse(&wrong).is_none(), "{wrong:?}");
        }
    }
}
