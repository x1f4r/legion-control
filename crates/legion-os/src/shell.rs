//! Shell lowering, exact-argv specification, and `C` locale helpers.
//!
//! Callers choose explicitly between running an exact argument vector with no shell involved
//! and handing a single command string to the platform's default shell. On Windows, PowerShell
//! is invoked without an inherited `PSModulePath` because a PowerShell 7 value breaks Windows
//! PowerShell 5.1 module loading.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::{OsError, Result};

#[cfg(unix)]
#[path = "shell/unix.rs"]
mod platform;

#[cfg(windows)]
#[path = "shell/windows.rs"]
mod platform;

/// Environment pairs that force POSIX `C` locale output for parseable tool stdout/stderr.
pub const C_LOCALE_ENV: [(&str, &str); 2] = [("LC_ALL", "C"), ("LANG", "C")];

/// Specification of a command to execute.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandSpec {
    /// Invoke an executable directly without a shell so arguments are never re-parsed or interpolated.
    Argv {
        /// Executable name or path.
        program: OsString,
        /// Exact arguments passed verbatim to the executable.
        args: Vec<OsString>,
    },
    /// Pass one command string to the platform shell (`sh -c` on Unix, PowerShell on Windows).
    Shell(String),
}

impl CommandSpec {
    /// Build an exact-argv command from a program and argument iterator.
    pub fn argv<P, I, A>(program: P, args: I) -> Self
    where
        P: Into<OsString>,
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        Self::Argv {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Build an exact-argv command from a single non-empty argument vector where element 0 is the program.
    pub fn from_argv<I, A>(argv: I) -> Result<Self>
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        let mut iter = argv.into_iter().map(Into::into);
        let Some(program) = iter.next() else {
            return Err(OsError::InvalidArgument {
                detail: "command argv must contain at least a program name".into(),
            });
        };
        if program.is_empty() {
            return Err(OsError::InvalidArgument {
                detail: "command program name must not be empty".into(),
            });
        }
        Ok(Self::Argv {
            program,
            args: iter.collect(),
        })
    }

    /// Build a shell command string to be handed whole to the platform shell.
    pub fn shell(command: impl Into<String>) -> Self {
        Self::Shell(command.into())
    }
}

/// Concrete executable, arguments, and environment removals ready to apply to a process builder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCommand {
    /// Program name or path to spawn.
    pub program: OsString,
    /// Exact arguments passed to `program`.
    pub args: Vec<OsString>,
    /// Environment variables cleared before caller overrides are applied.
    pub env_remove: Vec<OsString>,
}

impl ResolvedCommand {
    /// Lossy string representation of the executable name for diagnostics and typed errors.
    pub fn program_display(&self) -> String {
        self.program.to_string_lossy().into_owned()
    }

    /// Build a [`std::process::Command`] pre-populated with this program, arguments, and env removals.
    pub fn to_std_command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        cmd
    }

    /// Build a [`tokio::process::Command`] pre-populated with this program, arguments, and env removals.
    pub fn to_tokio_command(&self) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.program);
        cmd.args(&self.args);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        cmd
    }
}

/// Lower a [`CommandSpec`] into a concrete [`ResolvedCommand`] for the current platform.
pub fn resolve(spec: &CommandSpec) -> Result<ResolvedCommand> {
    match spec {
        CommandSpec::Argv { program, args } => {
            if program.is_empty() {
                return Err(OsError::InvalidArgument {
                    detail: "command program name must not be empty".into(),
                });
            }
            Ok(ResolvedCommand {
                program: program.clone(),
                args: args.clone(),
                env_remove: Vec::new(),
            })
        }
        CommandSpec::Shell(command) => resolve_shell(command),
    }
}

/// Insert `LC_ALL=C` and `LANG=C` into any environment map or pair collection.
///
/// System utilities localize dates, numbers, and messages according to the caller's SSH session
/// locale; forcing the `C` locale keeps machine-parsed output predictable.
pub fn with_c_locale<E>(env: &mut E)
where
    E: Extend<(OsString, OsString)>,
{
    env.extend(
        C_LOCALE_ENV
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
    );
}

/// Set `LC_ALL=C` and `LANG=C` directly on a [`std::process::Command`].
pub fn apply_c_locale_std(command: &mut std::process::Command) {
    for (key, value) in C_LOCALE_ENV {
        command.env(key, value);
    }
}

/// Set `LC_ALL=C` and `LANG=C` directly on a [`tokio::process::Command`].
pub fn apply_c_locale_tokio(command: &mut tokio::process::Command) {
    for (key, value) in C_LOCALE_ENV {
        command.env(key, value);
    }
}

#[cfg(any(unix, windows))]
fn resolve_shell(command: &str) -> Result<ResolvedCommand> {
    Ok(platform::resolve_shell(command))
}

#[cfg(not(any(unix, windows)))]
fn resolve_shell(_command: &str) -> Result<ResolvedCommand> {
    Err(OsError::Unsupported {
        detail: "no platform shell is configured for this operating system".into(),
    })
}

/// Search `PATH` entries for an executable `name`, trying each extension in `extensions` when
/// `name` does not already include an extension.
#[allow(dead_code)]
pub(crate) fn find_executable_in_paths(
    name: &str,
    path_var: Option<&OsStr>,
    extensions: &[String],
) -> Option<PathBuf> {
    let path_var = path_var?;
    let has_ext = std::path::Path::new(name).extension().is_some();

    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if has_ext || extensions.is_empty() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !has_ext {
            for ext in extensions {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Parse a Windows `PATHEXT`-style semicolon-delimited extension list, falling back to the
/// standard executable extensions when unset or empty.
#[allow(dead_code)]
pub(crate) fn parse_pathext(pathext: Option<&OsStr>) -> Vec<String> {
    const DEFAULTS: &[&str] = &[".EXE", ".COM", ".BAT", ".CMD"];
    let parsed: Vec<String> = pathext
        .and_then(OsStr::to_str)
        .map(|raw| {
            raw.split(';')
                .map(str::trim)
                .filter(|ext| !ext.is_empty())
                .map(|ext| {
                    if ext.starts_with('.') {
                        ext.to_owned()
                    } else {
                        format!(".{ext}")
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    if parsed.is_empty() {
        DEFAULTS.iter().map(|s| (*s).to_owned()).collect()
    } else {
        parsed
    }
}

/// Quote a string as a PowerShell single-quoted literal.
///
/// PowerShell terminates single-quoted literals on Unicode smart single quotes (`U+2018`..=`U+201B`)
/// as well as ASCII `'`, so every one of them must be doubled to prevent breaking out of the literal.
#[allow(dead_code)]
pub(crate) fn powershell_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        out.push(ch);
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Escape one argument according to Windows `CommandLineToArgvW` / MSVCRT quoting rules.
#[allow(dead_code)]
pub(crate) fn windows_cmdline_escape(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return arg.to_owned();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..(backslashes * 2 + 1) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Encode UTF-8 text as UTF-16LE Base64 for PowerShell's `-EncodedCommand` parameter.
#[allow(dead_code)]
pub(crate) fn encode_utf16le_base64(text: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut bytes = Vec::with_capacity(text.len() * 2);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }

    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let triple = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);

        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;

    #[test]
    fn argv_rejects_empty_input() {
        let err = CommandSpec::from_argv(Vec::<String>::new()).unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));

        let err = CommandSpec::from_argv([""]).unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));

        let spec = CommandSpec::argv("", ["a"]);
        let err = resolve(&spec).unwrap_err();
        assert!(matches!(err, OsError::InvalidArgument { .. }));
    }

    #[test]
    fn argv_resolves_verbatim_without_env_removals() {
        let spec = CommandSpec::argv("git", ["status", "--short"]);
        let resolved = resolve(&spec).unwrap();
        assert_eq!(resolved.program, OsString::from("git"));
        assert_eq!(
            resolved.args,
            vec![OsString::from("status"), OsString::from("--short")]
        );
        assert!(resolved.env_remove.is_empty());
    }

    #[test]
    fn shell_resolves_for_current_platform() {
        let resolved = resolve(&CommandSpec::shell("echo hi")).unwrap();
        #[cfg(unix)]
        {
            assert_eq!(resolved.program, OsString::from("sh"));
            assert_eq!(
                resolved.args,
                vec![OsString::from("-c"), OsString::from("echo hi")]
            );
            assert!(resolved.env_remove.is_empty());
        }
        #[cfg(windows)]
        {
            assert_eq!(
                resolved.args,
                vec![
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-Command"),
                    OsString::from("echo hi"),
                ]
            );
            assert_eq!(resolved.env_remove, vec![OsString::from("PSModulePath")]);
        }
    }

    #[test]
    fn c_locale_sets_both_variables() {
        let mut env = BTreeMap::new();
        with_c_locale(&mut env);
        assert_eq!(env.get(OsStr::new("LC_ALL")), Some(&OsString::from("C")));
        assert_eq!(env.get(OsStr::new("LANG")), Some(&OsString::from("C")));
    }

    #[test]
    fn find_executable_discovers_candidate_with_extension() {
        let dir = tempfile::tempdir().unwrap();
        let fake_pwsh = dir.path().join("pwsh.exe");
        fs::write(&fake_pwsh, b"").unwrap();

        let path_var = std::env::join_paths([dir.path()]).unwrap();
        let exts = parse_pathext(Some(OsStr::new(".COM;.exe;.BAT")));
        let found = find_executable_in_paths("pwsh", Some(&path_var), &exts);
        assert_eq!(found.as_deref(), Some(fake_pwsh.as_path()));

        let missing = find_executable_in_paths("definitely-missing-tool", Some(&path_var), &exts);
        assert!(missing.is_none());
    }

    #[test]
    fn powershell_single_quote_escapes_ascii_and_unicode_quotes() {
        let input = "C:\\Users\\me\\O'Brien\\a\u{2018}b\u{2019}c\u{201a}d\u{201b}e";
        assert_eq!(
            powershell_single_quote(input),
            "'C:\\Users\\me\\O''Brien\\a\u{2018}\u{2018}b\u{2019}\u{2019}c\u{201a}\u{201a}d\u{201b}\u{201b}e'"
        );
    }

    #[test]
    fn windows_cmdline_escape_handles_spaces_quotes_and_backslashes() {
        assert_eq!(windows_cmdline_escape("plain"), "plain");
        assert_eq!(windows_cmdline_escape(""), "\"\"");
        assert_eq!(windows_cmdline_escape("a b"), "\"a b\"");
        assert_eq!(windows_cmdline_escape("a\\b"), "a\\b");
        assert_eq!(windows_cmdline_escape("a b\\"), "\"a b\\\\\"");
        assert_eq!(windows_cmdline_escape("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(
            windows_cmdline_escape("c:\\path\\\"quote\"\\"),
            "\"c:\\path\\\\\\\"quote\\\"\\\\\""
        );
    }

    #[test]
    fn utf16le_base64_matches_powershell_encoded_command() {
        // Known PowerShell -EncodedCommand vector for "hi": 68 00 69 00 -> "aABpAA=="
        assert_eq!(encode_utf16le_base64("hi"), "aABpAA==");
        assert_eq!(encode_utf16le_base64(""), "");
    }
}
