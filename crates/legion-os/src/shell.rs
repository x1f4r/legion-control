//! Shell lowering, exact-argv specification, and `C` locale helpers.
//!
//! Callers choose explicitly between running an exact argument vector with no shell involved
//! and handing a single command string to the platform's default shell. On Windows, PowerShell
//! is invoked without an inherited `PSModulePath` because a PowerShell 7 value breaks Windows
//! PowerShell 5.1 module loading.

use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{OsError, Result};

/// Maximum UTF-16LE Base64 payload length sent via PowerShell `-EncodedCommand` before spilling
/// the wrapper script to a private temporary `.ps1` file invoked with `-File`.
///
/// Leaves generous headroom under Windows `CreateProcessW`'s 32,767-character command-line limit
/// once the executable path and flags are included.
#[allow(dead_code)]
pub(crate) const POWERSHELL_MAX_ENCODED_LEN: usize = 24_000;

static TEMP_SCRIPT_SEQ: AtomicU64 = AtomicU64::new(0);

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

/// RAII guard that removes a spilled temporary PowerShell wrapper script when the last clone of
/// [`ResolvedCommand`] drops, unless disarmed after handoff to a detached process.
#[derive(Debug)]
struct TempScriptGuard {
    path: PathBuf,
    armed: AtomicBool,
}

impl Drop for TempScriptGuard {
    fn drop(&mut self) {
        if self.armed.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Concrete executable, arguments, and environment removals ready to apply to a process builder.
///
/// When a long Windows shell command spills to a temporary `.ps1` file, this handle owns the
/// cleanup guard and removes the file once dropped (including on error or cancellation).
#[derive(Clone, Debug)]
pub struct ResolvedCommand {
    /// Program name or path to spawn.
    pub program: OsString,
    /// Exact arguments passed to `program`.
    pub args: Vec<OsString>,
    /// Environment variables cleared before caller overrides are applied.
    pub env_remove: Vec<OsString>,
    temp_script: Option<Arc<TempScriptGuard>>,
}

impl PartialEq for ResolvedCommand {
    fn eq(&self, other: &Self) -> bool {
        self.program == other.program
            && self.args == other.args
            && self.env_remove == other.env_remove
    }
}

impl Eq for ResolvedCommand {}

impl ResolvedCommand {
    /// Construct a [`ResolvedCommand`] without a backing temporary script.
    pub(crate) fn new(program: OsString, args: Vec<OsString>, env_remove: Vec<OsString>) -> Self {
        Self {
            program,
            args,
            env_remove,
            temp_script: None,
        }
    }

    /// Disarm Rust-side deletion of a spilled temporary script once ownership has been handed off
    /// to a detached process that deletes the script as its own first action.
    #[allow(dead_code)]
    pub(crate) fn disarm_temp_script(&self) {
        if let Some(ref guard) = self.temp_script {
            guard.armed.store(false, Ordering::Release);
        }
    }

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
            Ok(ResolvedCommand::new(
                program.clone(),
                args.clone(),
                Vec::new(),
            ))
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
    platform::resolve_shell(command)
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

/// Wrap a PowerShell command string so stdin and stdout use UTF-8 without a BOM, negative native
/// crash exit codes (such as `0xC0000005`) survive intact, failing cmdlets report `1` without
/// leaking a stale native exit code from an earlier command, and succeeding final commands exit
/// `0`.
///
/// With plain `-Command`, Windows PowerShell decodes stdin and encodes stdout in the active OEM
/// code page and collapses failing native exit codes to `1`. Running this wrapper fixes all three
/// for Windows PowerShell 5.1 and PowerShell 7.
#[allow(dead_code)]
pub(crate) fn build_powershell_shell_wrapper(command: &str) -> String {
    build_powershell_shell_wrapper_with_cleanup(command, None)
}

#[allow(dead_code)]
pub(crate) fn build_powershell_shell_wrapper_with_cleanup(
    command: &str,
    self_script_path: Option<&str>,
) -> String {
    let mut lines = Vec::with_capacity(12);
    if let Some(path) = self_script_path {
        lines.push(format!(
            "Remove-Item -LiteralPath {} -Force -ErrorAction SilentlyContinue",
            powershell_single_quote(path)
        ));
    }
    lines.extend([
        "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)".to_owned(),
        "[Console]::InputEncoding = [Console]::OutputEncoding".to_owned(),
        "$OutputEncoding = [Console]::OutputEncoding".to_owned(),
        "$global:LASTEXITCODE = $null".to_owned(),
        "$__legion_err_count = $Error.Count".to_owned(),
        command.to_owned(),
        "$__legion_ok = $?".to_owned(),
        "$__legion_code = $global:LASTEXITCODE".to_owned(),
        "if ($__legion_ok) { exit 0 }".to_owned(),
        "if ($Error.Count -gt $__legion_err_count -and $Error[0].FullyQualifiedErrorId -notlike 'NativeCommand*') { exit 1 }".to_owned(),
        "if ($__legion_code -is [int] -and $__legion_code -ne 0) { exit $__legion_code }".to_owned(),
        "exit 1".to_owned(),
    ]);
    lines.join("\n")
}

/// Build a PowerShell [`ResolvedCommand`] for `command`, using `-EncodedCommand` when it fits
/// safely within Windows command-line limits and spilling to a private temporary `.ps1` file
/// invoked with `-File` otherwise.
#[allow(dead_code)]
pub(crate) fn resolve_powershell_command(
    program: OsString,
    command: &str,
    temp_dir: &Path,
) -> Result<ResolvedCommand> {
    let env_remove = vec![OsString::from("PSModulePath")];
    let wrapper = build_powershell_shell_wrapper(command);
    let encoded = encode_utf16le_base64(&wrapper);
    if encoded.len() <= POWERSHELL_MAX_ENCODED_LEN {
        return Ok(ResolvedCommand::new(
            program,
            vec![
                OsString::from("-NoProfile"),
                OsString::from("-NonInteractive"),
                OsString::from("-EncodedCommand"),
                OsString::from(encoded),
            ],
            env_remove,
        ));
    }

    let script_path = write_private_temp_ps1(temp_dir, command)?;
    let guard = Arc::new(TempScriptGuard {
        path: script_path.clone(),
        armed: AtomicBool::new(true),
    });
    Ok(ResolvedCommand {
        program,
        args: vec![
            OsString::from("-NoProfile"),
            OsString::from("-NonInteractive"),
            OsString::from("-ExecutionPolicy"),
            OsString::from("Bypass"),
            OsString::from("-File"),
            script_path.into_os_string(),
        ],
        env_remove,
        temp_script: Some(guard),
    })
}

#[allow(dead_code)]
fn write_private_temp_ps1(temp_dir: &Path, command: &str) -> Result<PathBuf> {
    let pid = std::process::id();
    for _ in 0..32 {
        let seq = TEMP_SCRIPT_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path = temp_dir.join(format!("legion-ps-{pid}-{seq}-{nanos}.ps1"));

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let mut file = match opts.open(&path) {
            Ok(f) => f,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(OsError::from_io(
                    format!(
                        "cannot create temporary PowerShell script {}",
                        path.display()
                    ),
                    err,
                ));
            }
        };

        let path_str = path.to_string_lossy().into_owned();
        let script = build_powershell_shell_wrapper_with_cleanup(command, Some(&path_str));
        // Prefix a UTF-8 BOM so Windows PowerShell 5.1 reads the `.ps1` file as UTF-8.
        let write_result = file
            .write_all(&[0xEF, 0xBB, 0xBF])
            .and_then(|()| file.write_all(script.as_bytes()))
            .and_then(|()| file.flush());
        drop(file);
        if let Err(err) = write_result {
            let _ = std::fs::remove_file(&path);
            return Err(OsError::from_io(
                format!(
                    "cannot write temporary PowerShell script {}",
                    path.display()
                ),
                err,
            ));
        }
        return Ok(path);
    }

    Err(OsError::Platform {
        detail: format!(
            "exhausted attempts to allocate a unique temporary PowerShell script in {}",
            temp_dir.display()
        ),
    })
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
            let expected_encoded =
                encode_utf16le_base64(&build_powershell_shell_wrapper("echo hi"));
            assert_eq!(
                resolved.args,
                vec![
                    OsString::from("-NoProfile"),
                    OsString::from("-NonInteractive"),
                    OsString::from("-EncodedCommand"),
                    OsString::from(expected_encoded),
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

    #[test]
    fn powershell_shell_wrapper_sets_utf8_preserves_exit_code_and_round_trips_base64() {
        let cmd = "git status --short; echo 'caf\u{00e9}'";
        let wrapper = build_powershell_shell_wrapper(cmd);
        assert!(
            wrapper.contains(
                "[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)"
            )
        );
        assert!(wrapper.contains("[Console]::InputEncoding = [Console]::OutputEncoding"));
        assert!(wrapper.contains("$OutputEncoding = [Console]::OutputEncoding"));
        assert!(wrapper.contains("$global:LASTEXITCODE = $null"));
        assert!(wrapper.contains("$__legion_err_count = $Error.Count"));
        assert!(wrapper.contains(cmd));
        assert!(wrapper.contains("if ($__legion_ok) { exit 0 }"));
        assert!(wrapper.contains("FullyQualifiedErrorId -notlike 'NativeCommand*'"));
        assert!(wrapper.contains(
            "if ($__legion_code -is [int] -and $__legion_code -ne 0) { exit $__legion_code }"
        ));
        assert!(!wrapper.contains("[Math]::Max"));
        assert!(wrapper.ends_with("exit 1"));

        let encoded = encode_utf16le_base64(&wrapper);
        let decoded = decode_utf16le_base64(&encoded);
        assert_eq!(decoded, wrapper);
    }

    #[test]
    fn resolve_powershell_command_spills_long_scripts_to_temp_file_and_cleans_up_on_drop() {
        let dir = tempfile::tempdir().unwrap();

        let short =
            resolve_powershell_command(OsString::from("pwsh.exe"), "echo hi", dir.path()).unwrap();
        assert_eq!(short.args[2], OsString::from("-EncodedCommand"));
        assert_eq!(short.env_remove, vec![OsString::from("PSModulePath")]);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);

        let long_cmd = format!("Write-Output '{}'", "x".repeat(12_000));
        let spilled =
            resolve_powershell_command(OsString::from("pwsh.exe"), &long_cmd, dir.path()).unwrap();
        assert_eq!(spilled.args[2], OsString::from("-ExecutionPolicy"));
        assert_eq!(spilled.args[3], OsString::from("Bypass"));
        assert_eq!(spilled.args[4], OsString::from("-File"));

        let script_path = PathBuf::from(&spilled.args[5]);
        assert!(script_path.starts_with(dir.path()));
        assert!(script_path.is_file());

        let raw = fs::read(&script_path).unwrap();
        assert!(raw.starts_with(&[0xEF, 0xBB, 0xBF]));
        let text = std::str::from_utf8(&raw[3..]).unwrap();
        let first_line = text.lines().next().unwrap();
        assert!(first_line.starts_with("Remove-Item -LiteralPath "));
        assert!(text.contains(&long_cmd));

        // Cloning keeps the file alive until the last handle drops.
        let cloned = spilled.clone();
        drop(spilled);
        assert!(script_path.is_file());
        drop(cloned);
        assert!(!script_path.exists());

        // Disarming leaves cleanup to the script's own first-line Remove-Item (used by detach).
        let detached =
            resolve_powershell_command(OsString::from("pwsh.exe"), &long_cmd, dir.path()).unwrap();
        let detached_path = PathBuf::from(&detached.args[5]);
        detached.disarm_temp_script();
        drop(detached);
        assert!(detached_path.is_file());
    }

    fn decode_utf16le_base64(encoded: &str) -> String {
        fn val(b: u8) -> u8 {
            match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                other => panic!("unexpected base64 byte: {other}"),
            }
        }
        let raw = encoded.as_bytes();
        let (quads, rem) = raw.as_chunks::<4>();
        assert!(rem.is_empty());
        let mut bytes = Vec::with_capacity(quads.len() * 3);
        for chunk in quads {
            let v0 = val(chunk[0]) as u32;
            let v1 = val(chunk[1]) as u32;
            let v2 = if chunk[2] == b'=' {
                0
            } else {
                val(chunk[2]) as u32
            };
            let v3 = if chunk[3] == b'=' {
                0
            } else {
                val(chunk[3]) as u32
            };
            let triple = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
            bytes.push(((triple >> 16) & 0xff) as u8);
            if chunk[2] != b'=' {
                bytes.push(((triple >> 8) & 0xff) as u8);
            }
            if chunk[3] != b'=' {
                bytes.push((triple & 0xff) as u8);
            }
        }
        let (pairs, byte_rem) = bytes.as_chunks::<2>();
        assert!(byte_rem.is_empty());
        let units: Vec<u16> = pairs.iter().map(|&pair| u16::from_le_bytes(pair)).collect();
        String::from_utf16(&units).unwrap()
    }
}
