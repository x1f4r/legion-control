//! Fixed remote command strings shared by every SSH client (`legion-link`, `legion-net`) and
//! `legion gate`.
//!
//! Nothing a caller supplies is ever quoted for a remote shell: every SSH pipe requests one of a
//! few fixed command shapes built from a [`Subcommand`], a [`RemoteOs`], and an optional strictly
//! validated program name or path.

use legion_proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};

/// Operating system family hint used to choose the remote ssh command shape without probing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum RemoteOs {
    /// Linux, macOS, or another POSIX-like host reached through `sh -c`.
    Unix,
    /// Windows host where `legion` is on `PATH`.
    Windows,
}

impl RemoteOs {
    /// The other operating system family, tried as a fallback when no hint is configured.
    pub fn other(self) -> RemoteOs {
        match self {
            RemoteOs::Unix => RemoteOs::Windows,
            RemoteOs::Windows => RemoteOs::Unix,
        }
    }

    /// The lower-case spelling written in `fleet.toml` and state files.
    pub fn as_str(self) -> &'static str {
        match self {
            RemoteOs::Unix => "unix",
            RemoteOs::Windows => "windows",
        }
    }
}

/// Fixed subcommand a connector can ask the remote `legion` binary to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Subcommand {
    /// Newline-delimited JSON-RPC 2.0 session (`legion rpc`).
    Rpc,
    /// Raw bulk-data pipe (`legion stream`), used by file transfer.
    Stream,
}

impl Subcommand {
    /// The fixed subcommand word passed to the remote `legion` binary.
    pub fn as_str(self) -> &'static str {
        match self {
            Subcommand::Rpc => "rpc",
            Subcommand::Stream => "stream",
        }
    }
}

/// Checks that a configured remote `program` needs no shell quoting on any supported shell.
///
/// Only ASCII letters, digits, `_ . / ~ : - \` are allowed. Leading `-` is refused so the name
/// cannot be parsed as an option by `command -v` or `exec`, and a trailing `\` is refused because
/// `fish` treats `\'` inside single quotes as an escaped quote.
pub fn validate_program(program: &str) -> Result<(), String> {
    if program.is_empty() {
        return Err("program cannot be empty".to_owned());
    }
    if program.starts_with('-') {
        return Err(format!(
            "program {program:?} cannot begin with `-`, which a shell builtin would read as an option"
        ));
    }
    if program.ends_with('\\') {
        return Err(format!("program {program:?} cannot end with `\\`"));
    }
    if let Some(bad) = program.chars().find(|&c| !is_safe_program_char(c)) {
        return Err(format!(
            "program {program:?} contains {bad:?}; only A-Z, a-z, 0-9, `_`, `.`, `/`, `~`, `:`, `-`, and `\\` are allowed so nothing is ever quoted for a remote shell"
        ));
    }
    Ok(())
}

fn is_safe_program_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '~' | ':' | '-' | '\\')
}

/// Builds the remote command passed as the exec string over SSH.
///
/// The remote string is always one of two fixed shapes:
/// - Unix: invokes `sh -c '...'` so it works identically regardless of whether the remote login
///   shell is `sh`, `bash`, `zsh` or `fish`. Without a path override, it checks `PATH` first and
///   falls back to `$HOME/.local/bin/<program>` because non-interactive SSH rarely loads user PATH
///   entries.
/// - Windows: invokes `<program> <subcommand>` directly, since the Windows installer adds `legion`
///   to `PATH`.
pub fn remote_command(
    os: RemoteOs,
    subcommand: Subcommand,
    program: Option<&str>,
) -> Result<String, ErrorBody> {
    let sub = subcommand.as_str();
    match program {
        None => Ok(match os {
            RemoteOs::Unix => format!(
                "sh -c 'command -v legion >/dev/null 2>&1 && exec legion {sub} || exec \"$HOME/.local/bin/legion\" {sub}'"
            ),
            RemoteOs::Windows => format!("legion {sub}"),
        }),
        Some(prog) => {
            validate_program(prog).map_err(|reason| {
                ErrorBody::new(ErrorCode::InvalidArgs, format!("{reason}."))
                    .with_hint("Use a program path with no spaces or shell metacharacters.")
            })?;
            Ok(match os {
                RemoteOs::Windows => format!("{prog} {sub}"),
                RemoteOs::Unix => {
                    if is_explicit_path(prog) {
                        format!("sh -c 'exec {prog} {sub}'")
                    } else {
                        format!(
                            "sh -c 'command -v {prog} >/dev/null 2>&1 && exec {prog} {sub} || exec \"$HOME/.local/bin/{prog}\" {sub}'"
                        )
                    }
                }
            })
        }
    }
}

fn is_explicit_path(program: &str) -> bool {
    program.contains('/')
        || program.contains('\\')
        || program.contains(':')
        || program.starts_with('~')
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn default_remote_commands_match_the_design_shapes() {
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Rpc, None).unwrap(),
            r#"sh -c 'command -v legion >/dev/null 2>&1 && exec legion rpc || exec "$HOME/.local/bin/legion" rpc'"#
        );
        assert_eq!(
            remote_command(RemoteOs::Windows, Subcommand::Rpc, None).unwrap(),
            "legion rpc"
        );
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Stream, None).unwrap(),
            r#"sh -c 'command -v legion >/dev/null 2>&1 && exec legion stream || exec "$HOME/.local/bin/legion" stream'"#
        );
        assert_eq!(
            remote_command(RemoteOs::Windows, Subcommand::Stream, None).unwrap(),
            "legion stream"
        );
    }

    #[test]
    fn custom_program_replaces_legion_and_refuses_unsafe_characters() {
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Rpc, Some("legion-beta")).unwrap(),
            r#"sh -c 'command -v legion-beta >/dev/null 2>&1 && exec legion-beta rpc || exec "$HOME/.local/bin/legion-beta" rpc'"#
        );
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Rpc, Some("/opt/legion/legion")).unwrap(),
            "sh -c 'exec /opt/legion/legion rpc'"
        );
        assert_eq!(
            remote_command(RemoteOs::Unix, Subcommand::Stream, Some("~/tools/legion")).unwrap(),
            "sh -c 'exec ~/tools/legion stream'"
        );
        assert_eq!(
            remote_command(
                RemoteOs::Windows,
                Subcommand::Rpc,
                Some(r"C:\Tools\legion.exe")
            )
            .unwrap(),
            r"C:\Tools\legion.exe rpc"
        );
        for unsafe_prog in [
            "",
            "-v",
            "legion;id",
            "/path with space/legion",
            "legion'quote",
            r"C:\trailing\",
            "$(whoami)",
        ] {
            let err =
                remote_command(RemoteOs::Unix, Subcommand::Rpc, Some(unsafe_prog)).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{unsafe_prog:?}");
        }
    }
}
