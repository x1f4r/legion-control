//! Safe reading, parsing, quoting, and atomic editing of OpenSSH `authorized_keys` files.
//!
//! Guarantees:
//! - Only lines carrying `restrict`, an exact `legion gate` forced command, and a matching
//!   `legion-invite:<id>` or `legion-device:<name>` comment are ever touched.
//! - Every foreign line, comment, blank line, line ending, and missing trailing newline is
//!   preserved byte for byte.
//! - On Unix, `~/.ssh` is kept or set to mode `0o700` and `authorized_keys` to mode `0o600`
//!   atomically before rename.
//! - On Windows, when the account is in the `Administrators` group and `sshd_config` uses
//!   `administrators_authorized_keys`, writes `%ProgramData%\ssh\administrators_authorized_keys`
//!   and applies the `Administrators:F` + `SYSTEM:F` ACL to the staged temp file before rename.
//! - Never follows a symlink in `~/.ssh` or `~/.ssh/authorized_keys` outside the account's home
//!   directory unless configured explicitly via `[devices] authorized_keys` in `node.toml`.

use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use jiff::Timestamp;
use legion_core::Paths;
use legion_core::config::NodeFile;
use legion_core::policy::{Principal, expand_tilde};
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::atomic::{WriteOptions, write_atomic_prepared};

/// Comment prefix marking temporary single-use invite lines in `authorized_keys`.
pub const INVITE_COMMENT_PREFIX: &str = "legion-invite:";

/// Comment prefix marking permanent paired device lines in `authorized_keys`.
pub const DEVICE_COMMENT_PREFIX: &str = "legion-device:";

/// Resolved `authorized_keys` destination and platform handling flags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthKeysTarget {
    /// Configured or default path to `authorized_keys`.
    pub path: PathBuf,
    /// `true` when using the default `~/.ssh/authorized_keys` under the user's home directory.
    pub is_default_home_ssh: bool,
    /// `true` when writing Windows's `%ProgramData%\ssh\administrators_authorized_keys`.
    pub windows_admin_acl: bool,
}

/// Resolves which `authorized_keys` file this node manages.
pub fn resolve_target(paths: &Paths, node_file: &NodeFile) -> Result<AuthKeysTarget, ErrorBody> {
    if let Some(configured) = &node_file.devices.authorized_keys {
        let expanded = expand_tilde(Path::new(configured), paths.home())?;
        let windows_admin_acl = is_windows_admin_auth_keys_path(&expanded);
        return Ok(AuthKeysTarget {
            path: expanded,
            is_default_home_ssh: false,
            windows_admin_acl,
        });
    }

    if cfg!(windows) && legion_os::user::is_windows_administrator() {
        let program_data_ssh = windows_program_data_ssh_dir();
        if windows_sshd_uses_admin_file(&program_data_ssh) {
            return Ok(AuthKeysTarget {
                path: program_data_ssh.join("administrators_authorized_keys"),
                is_default_home_ssh: false,
                windows_admin_acl: true,
            });
        }
    }

    let home = paths.home().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            "Cannot locate ~/.ssh/authorized_keys because the home directory is not known.",
        )
        .with_hint(
            "Set HOME (or USERPROFILE on Windows), or set `[devices] authorized_keys` in node.toml.",
        )
    })?;
    Ok(AuthKeysTarget {
        path: home.join(".ssh").join("authorized_keys"),
        is_default_home_ssh: true,
        windows_admin_acl: false,
    })
}

fn windows_program_data_ssh_dir() -> PathBuf {
    std::env::var_os("ProgramData")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("ssh")
}

fn is_windows_admin_auth_keys_path(path: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let expected = windows_program_data_ssh_dir().join("administrators_authorized_keys");
    path.to_string_lossy()
        .eq_ignore_ascii_case(&expected.to_string_lossy())
}

fn windows_sshd_uses_admin_file(program_data_ssh: &Path) -> bool {
    match std::fs::read_to_string(program_data_ssh.join("sshd_config")) {
        Ok(text) => windows_sshd_uses_admin_file_from_text(&text),
        // Default Windows OpenSSH configuration routes Administrators to
        // `%ProgramData%\ssh\administrators_authorized_keys`.
        Err(_) => true,
    }
}

/// Determines from Windows `sshd_config` text whether `Match Group administrators` routes
/// `AuthorizedKeysFile` to `administrators_authorized_keys`.
pub fn windows_sshd_uses_admin_file_from_text(config: &str) -> bool {
    let mut in_admin_match = false;
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(keyword) = words.next() else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("Match") {
            let rest: Vec<&str> = words.collect();
            in_admin_match = match_line_targets_administrators(&rest);
            continue;
        }
        if in_admin_match && keyword.eq_ignore_ascii_case("AuthorizedKeysFile") {
            let targets: Vec<&str> = words.collect();
            return targets.iter().any(|target| {
                target
                    .to_ascii_lowercase()
                    .contains("administrators_authorized_keys")
            });
        }
    }
    false
}

fn match_line_targets_administrators(tokens: &[&str]) -> bool {
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i].eq_ignore_ascii_case("Group")
            && let Some(groups) = tokens.get(i + 1)
            && groups
                .split(',')
                .any(|g| g.trim_matches('"').eq_ignore_ascii_case("administrators"))
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Verifies symlink safety for `target` and returns the concrete path to read/write.
///
/// For the default `~/.ssh/authorized_keys`, refuses if `~/.ssh` or `~/.ssh/authorized_keys` is a
/// symbolic link that is dangling or points outside the account's home directory. If a symlink
/// points cleanly inside the account's home directory, returns the resolved target so atomic
/// renames update the target file without destroying the symlink.
pub fn resolve_safe_write_path(
    target: &AuthKeysTarget,
    home: Option<&Path>,
) -> Result<PathBuf, ErrorBody> {
    if target.is_default_home_ssh {
        let home = home.ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Failed,
                "Cannot check ~/.ssh symlink safety because the home directory is not known.",
            )
        })?;
        let canon_home = dunce::canonicalize(home).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Home directory {} cannot be resolved: {error}.",
                    home.display()
                ),
            )
        })?;

        let ssh_dir = target.path.parent().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("{} has no parent directory.", target.path.display()),
            )
        })?;

        let effective_ssh_dir = match std::fs::symlink_metadata(ssh_dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link_target = std::fs::read_link(ssh_dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unreadable>".to_owned());
                let resolved = dunce::canonicalize(ssh_dir).map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing to follow symbolic link {} -> {link_target}: {error}.",
                            ssh_dir.display()
                        ),
                    )
                    .with_hint(
                        "Replace ~/.ssh with a real directory or set `[devices] authorized_keys` explicitly in node.toml.",
                    )
                })?;
                if !resolved.starts_with(&canon_home) {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing to follow symbolic link {} pointing outside home directory (to {}).",
                            ssh_dir.display(),
                            resolved.display()
                        ),
                    )
                    .with_hint(
                        "If this is intentional, set `[devices] authorized_keys` explicitly in node.toml.",
                    ));
                }
                resolved
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} exists and is not a directory.", ssh_dir.display()),
                ));
            }
            Ok(_) => ssh_dir.to_path_buf(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => ssh_dir.to_path_buf(),
            Err(error) => {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be inspected: {error}.", ssh_dir.display()),
                ));
            }
        };

        let candidate = effective_ssh_dir.join("authorized_keys");
        return match std::fs::symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let link_target = std::fs::read_link(&candidate)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unreadable>".to_owned());
                let resolved = dunce::canonicalize(&candidate).map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing to follow symbolic link {} -> {link_target}: {error}.",
                            candidate.display()
                        ),
                    )
                    .with_hint(
                        "Replace ~/.ssh/authorized_keys with a regular file or set `[devices] authorized_keys` explicitly in node.toml.",
                    )
                })?;
                if !resolved.starts_with(&canon_home) {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        format!(
                            "Refusing to follow symbolic link {} pointing outside home directory (to {}).",
                            candidate.display(),
                            resolved.display()
                        ),
                    )
                    .with_hint(
                        "If this is intentional, set `[devices] authorized_keys` explicitly in node.toml.",
                    ));
                }
                if !resolved.is_file() {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!("{} does not point to a regular file.", candidate.display()),
                    ));
                }
                Ok(resolved)
            }
            Ok(meta) if meta.is_dir() => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} is a directory, not a file.", candidate.display()),
            )),
            Ok(_) => Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(candidate),
            Err(error) => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be inspected: {error}.", candidate.display()),
            )),
        };
    }

    // For Windows administrators_authorized_keys, refuse symlinks outright.
    match std::fs::symlink_metadata(&target.path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if target.windows_admin_acl {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing to follow symbolic link at {}.",
                        target.path.display()
                    ),
                )
                .with_hint("Use a regular file for administrators_authorized_keys."));
            }
            let resolved = dunce::canonicalize(&target.path).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Configured authorized_keys symlink {} cannot be resolved: {error}.",
                        target.path.display()
                    ),
                )
            })?;
            Ok(resolved)
        }
        Ok(meta) if meta.is_dir() => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} is a directory, not a file.", target.path.display()),
        )),
        Ok(_) => Ok(target.path.clone()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(target.path.clone()),
        Err(error) => Err(ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be inspected: {error}.", target.path.display()),
        )),
    }
}

/// Verifies on Unix that neither `exe` (after resolving symlinks) nor any of its parent
/// directories is group- or world-writable (`mode & 0o022 != 0`), refusing to embed a binary path
/// that another local account could replace into an `authorized_keys` forced command.
///
/// No-op on non-Unix platforms.
pub fn verify_exe_not_group_or_world_writable(exe: &Path) -> Result<(), ErrorBody> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let canon = dunce::canonicalize(exe).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Legion executable path {} cannot be resolved: {error}.",
                    exe.display()
                ),
            )
        })?;
        for path in canon.ancestors() {
            let meta = std::fs::metadata(path).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be inspected: {error}.", path.display()),
                )
            })?;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o022 != 0 {
                let what = if path == canon {
                    "the legion executable"
                } else {
                    "an ancestor directory of the legion executable"
                };
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    format!(
                        "Refusing to place {} in a forced command because {what} ({}) is group- or world-writable (mode {:04o}).",
                        exe.display(),
                        path.display(),
                        mode,
                    ),
                )
                .with_hint(
                    "Install legion in a directory writable only by its owner (such as ~/.local/bin) and remove group/other write bits (`chmod go-w`) before pairing.",
                ));
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = exe;
        Ok(())
    }
}

/// Verifies on Unix that the effective user ID (`euid`) owns every existing path among
/// `authorized_keys`, its parent directory (and `~/.ssh` plus `home` for the default location),
/// refusing to rewrite another user's file (for example when `sudo legion pair` runs as root with
/// a user's `HOME`).
///
/// No-op on non-Unix platforms.
pub fn verify_owner_matches_euid(
    write_path: &Path,
    home: Option<&Path>,
    is_default_home_ssh: bool,
    euid: Option<u32>,
) -> Result<(), ErrorBody> {
    verify_owner_matches_euid_with(write_path, home, is_default_home_ssh, euid, |path| {
        match legion_os::user::path_owner_uid(path) {
            Ok(uid) => Ok(uid),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    })
}

fn verify_owner_matches_euid_with(
    write_path: &Path,
    home: Option<&Path>,
    is_default_home_ssh: bool,
    euid: Option<u32>,
    mut owner_uid_of: impl FnMut(&Path) -> io::Result<Option<u32>>,
) -> Result<(), ErrorBody> {
    let Some(euid) = euid else {
        return Ok(());
    };
    let mut check_path = |path: &Path| -> Result<bool, ErrorBody> {
        let Some(owner_uid) = owner_uid_of(path).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be inspected: {error}.", path.display()),
            )
        })?
        else {
            return Ok(false);
        };
        if owner_uid != euid {
            let owner_label = legion_os::user::username_for_uid(owner_uid)
                .map(|name| format!("{name} (uid {owner_uid})"))
                .unwrap_or_else(|| format!("uid {owner_uid}"));
            let runner_label = legion_os::user::username_for_uid(euid)
                .map(|name| format!("{name} (uid {euid})"))
                .unwrap_or_else(|| format!("uid {euid}"));
            let hint_user = legion_os::user::username_for_uid(owner_uid)
                .unwrap_or_else(|| format!("#{owner_uid}"));
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!(
                    "Refusing to modify {} because {} is owned by {owner_label}, not the running account {runner_label}.",
                    write_path.display(),
                    path.display(),
                ),
            )
            .with_hint(format!(
                "Run legion as {hint_user} (for example `sudo -u {hint_user} legion ...`) so authorized_keys keeps its owner and SSH login stays working, or point `[devices] authorized_keys` at a file owned by this account."
            )));
        }
        Ok(true)
    };

    // Check every existing path among the file itself, its parent directory, and (for the default
    // ~/.ssh/authorized_keys) ~/.ssh and home, rather than stopping at the first existing path.
    let mut candidates: Vec<PathBuf> = vec![write_path.to_path_buf()];
    if let Some(parent) = write_path.parent()
        && !candidates.iter().any(|p| p == parent)
    {
        candidates.push(parent.to_path_buf());
    }
    if is_default_home_ssh && let Some(home) = home {
        let default_ssh = home.join(".ssh");
        if !candidates.iter().any(|p| p == &default_ssh) {
            candidates.push(default_ssh);
        }
        if !candidates.iter().any(|p| p == home) {
            candidates.push(home.to_path_buf());
        }
    }

    let mut checked = false;
    for candidate in &candidates {
        if check_path(candidate)? {
            checked = true;
        }
    }
    if !checked {
        for ancestor in write_path.ancestors().skip(1) {
            if check_path(ancestor)? {
                break;
            }
        }
    }
    Ok(())
}

/// Target shell syntax used by `sshd` to invoke `authorized_keys` `command="..."` forced commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForcedCommandShell {
    /// POSIX `/bin/sh`-compatible quoting (`'...'` with `'\''`).
    Posix,
    /// Windows `cmd.exe` quoting (`"..."`, refusing cmd metacharacters `%`, `&`, `(`, `)`, `^`,
    /// `!`, and `"`).
    WindowsCmd,
    /// Windows PowerShell (`powershell.exe` / `pwsh.exe`) call syntax (`& '<exe>' ...` with `'`
    /// doubled and `@`-safe single-quoted arguments).
    WindowsPowerShell,
}

impl ForcedCommandShell {
    /// Detects the forced-command shell for the current host: [`ForcedCommandShell::Posix`] on
    /// Unix, or the shell configured in `HKLM\SOFTWARE\OpenSSH\DefaultShell` on Windows (defaulting
    /// to [`ForcedCommandShell::WindowsCmd`] when absent, and refusing unrecognised shells).
    pub fn host() -> Result<ForcedCommandShell, ErrorBody> {
        if cfg!(windows) {
            let detected = legion_os::user::windows_sshd_default_shell().map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "Cannot read Windows OpenSSH DefaultShell from the registry: {error}."
                    ),
                )
                .with_hint(
                    "Check permissions on HKLM\\SOFTWARE\\OpenSSH\\DefaultShell or remove the value to use cmd.exe.",
                )
            })?;
            Self::from_windows_shell(detected)
        } else {
            Ok(ForcedCommandShell::Posix)
        }
    }

    /// Maps a classified Windows OpenSSH `DefaultShell` into a [`ForcedCommandShell`], refusing
    /// shells whose quoting syntax Legion cannot generate safely.
    pub fn from_windows_shell(
        shell: legion_os::user::WindowsSshShell,
    ) -> Result<ForcedCommandShell, ErrorBody> {
        match shell {
            legion_os::user::WindowsSshShell::Cmd => Ok(ForcedCommandShell::WindowsCmd),
            legion_os::user::WindowsSshShell::PowerShell => {
                Ok(ForcedCommandShell::WindowsPowerShell)
            }
            legion_os::user::WindowsSshShell::Posix => Ok(ForcedCommandShell::Posix),
            legion_os::user::WindowsSshShell::Unknown(raw) => Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "Unsupported Windows OpenSSH DefaultShell `{raw}`: Legion cannot safely quote forced commands for this shell."
                ),
            )
            .with_hint(
                "Set HKLM\\SOFTWARE\\OpenSSH\\DefaultShell to cmd.exe, PowerShell (powershell.exe or pwsh.exe), or a POSIX shell (such as bash.exe), or remove DefaultShell to use cmd.exe.",
            )),
        }
    }
}

/// Characters `cmd.exe` interprets even inside or around double quotes (or that break quoting).
const CMD_UNSAFE_CHARS: &[char] = &['%', '&', '(', ')', '^', '!', '"'];

/// Unicode single-quote variants that PowerShell's lexer treats as single-quoted string delimiters.
const PS_UNSAFE_SINGLE_QUOTES: &[char] = &['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];

/// Formats the value placed inside `command="..."` in `authorized_keys`.
///
/// The executable path must be absolute, valid UTF-8, and free of control characters.
/// Quoting follows `shell`:
/// - [`ForcedCommandShell::Posix`]: bare ASCII tokens or `'...'` with `'\''`.
/// - [`ForcedCommandShell::WindowsCmd`]: refuses `%`, `&`, `(`, `)`, `^`, `!`, and `"` in paths
///   and arguments, and double-quotes tokens with spaces or non-ASCII characters.
/// - [`ForcedCommandShell::WindowsPowerShell`]: emits `& '<exe>' ...` with `'` doubled (`''`),
///   refuses Unicode single-quote delimiters (`U+2018`..=`U+201B`), and single-quotes any argument
///   that could trigger PowerShell parsing or splatting (including leading `@`, spaces, quotes,
///   `$`, or non-ASCII).
///
/// Finally escapes every `"` as `\"` for OpenSSH's `authorized_keys` option parser (which only
/// unescapes `\"` inside double-quoted option values).
pub fn format_forced_command(
    exe: &Path,
    args: &[&str],
    shell: ForcedCommandShell,
) -> Result<String, ErrorBody> {
    if !exe.is_absolute() && !looks_windows_absolute(exe) {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Legion executable path {} is not absolute and cannot be placed in authorized_keys.",
                exe.display()
            ),
        ));
    }
    let exe_str = exe.to_str().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Legion executable path {} is not valid Unicode.",
                exe.display()
            ),
        )
    })?;
    if exe_str.is_empty() || exe_str.chars().any(char::is_control) {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            "Legion executable path contains control characters and cannot be placed in authorized_keys.",
        ));
    }
    for arg in args {
        if arg.is_empty() || arg.chars().any(char::is_control) {
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("Forced command argument {arg:?} is not safe for authorized_keys."),
            ));
        }
    }

    let command = match shell {
        ForcedCommandShell::Posix => {
            let quote_posix = |token: &str| -> String {
                let plain_safe = token.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'/' | b'.' | b'-' | b'_' | b'+' | b':' | b'@')
                });
                if plain_safe {
                    token.to_owned()
                } else {
                    let escaped_single = token.replace('\'', "'\\''");
                    format!("'{escaped_single}'")
                }
            };
            let mut out = quote_posix(exe_str);
            for arg in args {
                out.push(' ');
                out.push_str(&quote_posix(arg));
            }
            out
        }
        ForcedCommandShell::WindowsCmd => {
            let check_cmd_token = |label: &str, token: &str| -> Result<(), ErrorBody> {
                if let Some(bad) = token.chars().find(|ch| CMD_UNSAFE_CHARS.contains(ch)) {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Cannot place {label} {token:?} in a Windows cmd.exe forced command because it contains `{bad}`."
                        ),
                    )
                    .with_hint(
                        "Move legion.exe / LEGION_HOME to a path without `% & ( ) ^ ! \"`, or set HKLM\\SOFTWARE\\OpenSSH\\DefaultShell to PowerShell.",
                    ));
                }
                Ok(())
            };
            let quote_cmd = |label: &str, token: &str| -> Result<String, ErrorBody> {
                check_cmd_token(label, token)?;
                let plain_safe = token.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'\\' | b'/' | b':' | b'.' | b'-' | b'_' | b'+' | b'@')
                });
                if plain_safe {
                    Ok(token.to_owned())
                } else if token.ends_with('\\') {
                    Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Cannot quote {label} {token:?} for Windows cmd.exe because it ends with a trailing backslash."
                        ),
                    )
                    .with_hint("Remove the trailing backslash from the path."))
                } else {
                    Ok(format!("\"{token}\""))
                }
            };
            let mut out = quote_cmd("executable path", exe_str)?;
            for arg in args {
                out.push(' ');
                out.push_str(&quote_cmd("forced command argument", arg)?);
            }
            out
        }
        ForcedCommandShell::WindowsPowerShell => {
            let check_ps_token = |label: &str, token: &str| -> Result<(), ErrorBody> {
                if let Some(bad) = token
                    .chars()
                    .find(|ch| PS_UNSAFE_SINGLE_QUOTES.contains(ch))
                {
                    return Err(ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Cannot place {label} {token:?} in a Windows PowerShell forced command because it contains Unicode quote U+{:04X}, which PowerShell treats as a string delimiter.",
                            u32::from(bad)
                        ),
                    )
                    .with_hint(
                        "Use a path or name without Unicode curly single quotes (U+2018..U+201B), or switch HKLM\\SOFTWARE\\OpenSSH\\DefaultShell to cmd.exe or a POSIX shell.",
                    ));
                }
                Ok(())
            };
            let quote_ps_single = |label: &str, token: &str| -> Result<String, ErrorBody> {
                check_ps_token(label, token)?;
                let escaped = token.replace('\'', "''");
                Ok(format!("'{escaped}'"))
            };
            // Always invoke the executable via PowerShell's call operator `& '...'` so spaces,
            // quotes, non-ASCII, and drive paths are executed rather than parsed as a string literal.
            let mut out = format!("& {}", quote_ps_single("executable path", exe_str)?);
            for arg in args {
                check_ps_token("forced command argument", arg)?;
                out.push(' ');
                // Single-quote anything other than simple bare ASCII words/flags (`gate`, `--as`,
                // hex ids, etc.) so leading `@` (splatting), `$`, `:`, spaces, backslashes, quotes,
                // and non-ASCII are always passed verbatim.
                let plain_safe = !arg.starts_with('@')
                    && arg
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
                if plain_safe {
                    out.push_str(arg);
                } else {
                    out.push_str(&quote_ps_single("forced command argument", arg)?);
                }
            }
            out
        }
    };

    // OpenSSH `opt_dequote` inside `command="..."` recognizes only `\"` -> `"` and leaves every
    // other backslash untouched, so replacing `"` with `\"` round-trips faithfully as long as the
    // command does not end with `\` (it always ends with a validated argument or closing quote).
    Ok(command.replace('"', "\\\""))
}

fn looks_windows_absolute(path: &Path) -> bool {
    let Some(text) = path.to_str() else {
        return false;
    };
    let bytes = text.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// Formats an OpenSSH `expiry-time` timestamp in the portable 12-digit `YYYYMMDDHHMM` form
/// (without a trailing `Z`) so older servers such as OpenSSH 8.9 on Ubuntu 22.04 accept it.
///
/// Because `YYYYMMDDHHMM` without `Z` is interpreted in sshd's local time zone, this adds 14 hours
/// (the maximum UTC+14 offset) and rounds up to the next whole minute so sshd never rejects a
/// still-valid invite early in any time zone. Exact UTC expiry is enforced by `legion gate --enrol`
/// and `sweep` using `state/devices.json`.
pub fn format_sshd_expiry(time: Timestamp) -> String {
    let mut secs = time.as_second().saturating_add(14 * 3600);
    let nanos = time.subsec_nanosecond();
    let rem = secs.rem_euclid(60);
    if rem != 0 || nanos > 0 {
        secs = secs.saturating_add(60 - rem);
    }
    let rounded = Timestamp::from_second(secs).unwrap_or(time);
    rounded.strftime("%Y%m%d%H%M").to_string()
}

/// Validates an OS account name pinned via `--account` on `legion gate`.
pub fn validate_account_name(raw: &str) -> Result<(), ErrorBody> {
    if raw.is_empty()
        || raw.len() > 128
        || raw.starts_with('-')
        || raw.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{raw:?} is not a valid OS account name."),
        )
        .with_hint(
            "Pass a non-empty OS account name without a leading '-', spaces, or control characters.",
        ));
    }
    Ok(())
}

/// Compares two OS account names, ignoring ASCII case on Windows and requiring exact match on Unix.
pub fn account_names_match(a: &str, b: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// Returns the current OS account name via [`legion_os::user::current_username`].
pub fn current_account_name() -> Result<String, ErrorBody> {
    legion_os::user::current_username().map_err(|error| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot determine the current OS account name: {error}."),
        )
    })
}

/// Verifies that the OS account running `legion gate` matches the optional `--account` pinned on
/// the `authorized_keys` line (case-insensitively on Windows, exact on Unix).
pub fn verify_gate_account(expected: Option<&str>) -> Result<(), ErrorBody> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = legion_os::user::current_username().map_err(|error| {
        ErrorBody::new(
            ErrorCode::Denied,
            format!("Cannot verify OS account against pinned account {expected}: {error}."),
        )
    })?;
    if !account_names_match(&actual, expected, cfg!(windows)) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("This key is pinned to OS account {expected}, not {actual}."),
        )
        .with_hint("Connect using the OS account this device was paired for."));
    }
    Ok(())
}

/// Verifies that `authorized_keys` still contains a Legion-managed device line for `device`
/// pinned to `principal`, `paths`, and `account` (when present or required on Windows's shared
/// `administrators_authorized_keys`).
///
/// Called by `legion gate --as <principal> --device <name>` immediately after registering its
/// session record so a gate starting on an already-open multiplexed SSH connection (`ControlMaster`)
/// or racing with `device.remove` refuses to serve once the line has been removed. Strictly
/// read-only.
pub fn verify_device_authorized(
    paths: &Paths,
    node_file: &NodeFile,
    device: &str,
    principal: &str,
    account: Option<&str>,
) -> Result<(), ErrorBody> {
    let target = resolve_target(paths, node_file)?;
    let write_path = resolve_safe_write_path(&target, paths.home())?;
    let auth_keys = read_auth_keys(&write_path)?;
    let expected_account = match account {
        Some(acct) => Some(acct.to_owned()),
        None if target.windows_admin_acl => Some(current_account_name()?),
        None => None,
    };
    let require_pinned_account = target.windows_admin_acl || account.is_some();

    let authorized = auth_keys.lines.iter().any(|line| {
        matches!(
            &line.kind,
            LineKind::Device {
                name,
                principal: line_principal,
                home,
                account: line_account,
                ..
            } if name == device
                && line_principal == principal
                && line_matches_home(home.as_ref(), paths)
                && line_matches_account(
                    line_account.as_deref(),
                    expected_account.as_deref(),
                    require_pinned_account,
                    cfg!(windows),
                )
        )
    });

    if authorized {
        Ok(())
    } else {
        Err(ErrorBody::new(
            ErrorCode::Denied,
            format!("Device {device} is no longer authorized in authorized_keys."),
        )
        .with_hint("Pair the device again if access should be restored."))
    }
}

/// Pinned Legion home embedded in every Legion-managed `authorized_keys` forced command so `sshd`
/// session environment variables (`AcceptEnv LEGION_HOME`, `XDG_CONFIG_HOME`, `XDG_STATE_HOME`)
/// can never redirect a paired key to another configuration or policy directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinnedHome {
    /// Explicit `LEGION_HOME` root (`--home <root>`, containing `config/` and `state/`).
    Root(PathBuf),
    /// Resolved split config and state directories (`--config-dir <cfg> --state-dir <st>`), used
    /// when Legion runs from the account's standard config/state locations.
    Split {
        /// Pinned configuration directory (`node.toml`, `policy.toml`, ...).
        config_dir: PathBuf,
        /// Pinned state directory (`audit.jsonl`, `devices.json`, ...).
        state_dir: PathBuf,
    },
}

impl PinnedHome {
    /// Extracts the pinned home flags to embed in forced commands for `paths`.
    pub fn from_paths(paths: &Paths) -> PinnedHome {
        if let Some(root) = paths.explicit_home() {
            PinnedHome::Root(root.to_path_buf())
        } else {
            PinnedHome::Split {
                config_dir: paths.config_dir().to_path_buf(),
                state_dir: paths.state_dir().to_path_buf(),
            }
        }
    }

    /// Returns the configuration directory referenced by this pinned home.
    pub fn config_dir(&self) -> PathBuf {
        match self {
            PinnedHome::Root(root) => root.join("config"),
            PinnedHome::Split { config_dir, .. } => config_dir.clone(),
        }
    }

    /// Returns the state directory referenced by this pinned home.
    pub fn state_dir(&self) -> PathBuf {
        match self {
            PinnedHome::Root(root) => root.join("state"),
            PinnedHome::Split { state_dir, .. } => state_dir.clone(),
        }
    }

    /// Returns `true` when this pinned home refers to the same config and state directories as
    /// `paths`.
    pub fn matches_paths(&self, paths: &Paths) -> bool {
        paths_equal(&self.config_dir(), paths.config_dir())
            && paths_equal(&self.state_dir(), paths.state_dir())
    }

    /// Human-readable path identifying this pinned home in `device.list` and `meta.doctor`.
    pub fn display_label(&self) -> String {
        match self {
            PinnedHome::Root(root) => root.display().to_string(),
            PinnedHome::Split { config_dir, .. } => config_dir.display().to_string(),
        }
    }

    /// Returns `true` when the pinned home's config directory and `node.toml` both exist on disk.
    pub fn config_exists(&self) -> bool {
        let cfg = self.config_dir();
        cfg.is_dir() && cfg.join("node.toml").is_file()
    }

    fn push_cli_args(&self, out: &mut Vec<String>) -> Result<(), ErrorBody> {
        let to_utf8 = |label: &str, path: &Path| -> Result<String, ErrorBody> {
            path.to_str().map(ToOwned::to_owned).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{label} {} is not valid Unicode.", path.display()),
                )
            })
        };
        match self {
            PinnedHome::Root(root) => {
                out.push("--home".to_owned());
                out.push(to_utf8("Legion home path", root)?);
            }
            PinnedHome::Split {
                config_dir,
                state_dir,
            } => {
                out.push("--config-dir".to_owned());
                out.push(to_utf8("Legion config directory", config_dir)?);
                out.push("--state-dir".to_owned());
                out.push(to_utf8("Legion state directory", state_dir)?);
            }
        }
        Ok(())
    }
}

/// Formats a temporary invite line for `authorized_keys`.
///
/// Always pins `pinned_home` (`--home <root>` or `--config-dir <cfg> --state-dir <st>`) in the
/// forced command so `legion gate --enrol` always reads the exact Legion home that issued the
/// invite regardless of `sshd`'s session environment or `AcceptEnv`. When `account` is `Some`
/// (such as on Windows's shared `administrators_authorized_keys`), appends `--account <name>` so
/// only that OS account may use the temporary key.
pub fn format_invite_line(
    exe: &Path,
    id: &str,
    expires: Timestamp,
    pubkey: &str,
    pinned_home: &PinnedHome,
    account: Option<&str>,
    shell: ForcedCommandShell,
) -> Result<String, ErrorBody> {
    let mut owned_args = vec!["gate".to_owned(), "--enrol".to_owned(), id.to_owned()];
    if let Some(acct) = account {
        validate_account_name(acct)?;
        owned_args.push("--account".to_owned());
        owned_args.push(acct.to_owned());
    }
    pinned_home.push_cli_args(&mut owned_args)?;
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let cmd = format_forced_command(exe, &arg_refs, shell)?;
    let expiry = format_sshd_expiry(expires);
    Ok(format!(
        "restrict,expiry-time=\"{expiry}\",command=\"{cmd}\" {pubkey} {INVITE_COMMENT_PREFIX}{id}"
    ))
}

/// Formats a permanent paired device line for `authorized_keys`.
///
/// Includes `--device <device_name>` so `legion gate --as` tracks live sessions for immediate
/// revocation by `device.remove`, and always pins `pinned_home` (`--home <root>` or
/// `--config-dir <cfg> --state-dir <st>`) so `legion gate --as` always reads the same Legion home
/// regardless of `sshd`'s session environment or `AcceptEnv`. When `account` is `Some` (such as on
/// Windows's shared `administrators_authorized_keys`), appends `--account <name>` so no other
/// account on the same machine can authenticate with this key.
pub fn format_device_line(
    exe: &Path,
    principal: &str,
    pubkey: &str,
    device_name: &str,
    pinned_home: &PinnedHome,
    account: Option<&str>,
    shell: ForcedCommandShell,
) -> Result<String, ErrorBody> {
    let mut owned_args = vec![
        "gate".to_owned(),
        "--as".to_owned(),
        principal.to_owned(),
        "--device".to_owned(),
        device_name.to_owned(),
    ];
    if let Some(acct) = account {
        validate_account_name(acct)?;
        owned_args.push("--account".to_owned());
        owned_args.push(acct.to_owned());
    }
    pinned_home.push_cli_args(&mut owned_args)?;
    let arg_refs: Vec<&str> = owned_args.iter().map(String::as_str).collect();
    let cmd = format_forced_command(exe, &arg_refs, shell)?;
    Ok(format!(
        "restrict,command=\"{cmd}\" {pubkey} {DEVICE_COMMENT_PREFIX}{device_name}"
    ))
}

/// Classification of one line in `authorized_keys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// Any line not managed by Legion (comments, blank lines, user keys); preserved byte for byte.
    Foreign,
    /// Temporary invite line with `restrict`, an exact `legion gate --enrol <id>` command, and a
    /// matching `legion-invite:<id>` comment.
    Invite {
        /// Invite id after `legion-invite:` (matches `--enrol <id>`).
        id: String,
        /// Base64 key blob on the line.
        key_b64: String,
        /// Raw `expiry-time` option value on the line, when present.
        expiry_time: Option<String>,
        /// Pinned Legion home (`--home` or `--config-dir`/`--state-dir`) from the forced command.
        home: Option<PinnedHome>,
        /// Pinned `--account` name extracted from the forced command, when present.
        account: Option<String>,
    },
    /// Paired device line with `restrict`, an exact `legion gate --as <principal> --device <name>`
    /// command, and a matching `legion-device:<name>` comment.
    Device {
        /// Device name after `legion-device:` (matches `--device <name>`).
        name: String,
        /// Base64 key blob on the line.
        key_b64: String,
        /// Principal extracted from `command="... gate --as <principal> --device <name>"`.
        principal: String,
        /// Pinned Legion home (`--home` or `--config-dir`/`--state-dir`) from the forced command.
        home: Option<PinnedHome>,
        /// Pinned `--account` name extracted from the forced command, when present.
        account: Option<String>,
    },
}

/// One line in `authorized_keys`, retaining its exact original bytes.
#[derive(Clone, Debug)]
pub struct AuthKeyLine {
    /// Exact original bytes including any `\r\n` or `\n` (or missing newline on the final line).
    pub raw: Vec<u8>,
    /// Parsed Legion classification.
    pub kind: LineKind,
}

/// Parsed representation of an `authorized_keys` file that preserves foreign lines byte for byte.
#[derive(Clone, Debug, Default)]
pub struct AuthKeysFile {
    /// Lines in file order.
    pub lines: Vec<AuthKeyLine>,
}

impl AuthKeysFile {
    /// Parses `bytes` into lines without altering any byte sequence.
    pub fn parse(bytes: &[u8]) -> AuthKeysFile {
        if bytes.is_empty() {
            return AuthKeysFile { lines: Vec::new() };
        }
        let lines = bytes
            .split_inclusive(|&b| b == b'\n')
            .map(|slice| AuthKeyLine {
                raw: slice.to_vec(),
                kind: classify_line(slice),
            })
            .collect();
        AuthKeysFile { lines }
    }

    /// Keeps only lines for which `keep` returns `true`, returning whether anything was removed.
    pub fn retain(&mut self, mut keep: impl FnMut(&AuthKeyLine) -> bool) -> bool {
        let before = self.lines.len();
        self.lines.retain(|line| match line.kind {
            // Foreign lines are unconditionally preserved byte for byte.
            LineKind::Foreign => true,
            _ => keep(line),
        });
        self.lines.len() != before
    }

    /// Appends a new Legion-managed line, inserting a newline before it only if the previous last
    /// line lacked one.
    pub fn append_line(&mut self, line: &str) {
        let crlf = self
            .lines
            .iter()
            .any(|existing| existing.raw.ends_with(b"\r\n"));
        let eol: &[u8] = if crlf { b"\r\n" } else { b"\n" };
        if let Some(last) = self.lines.last_mut()
            && !last.raw.ends_with(b"\n")
        {
            last.raw.extend_from_slice(eol);
        }
        let mut raw = line.as_bytes().to_vec();
        raw.extend_from_slice(eol);
        let kind = classify_line(&raw);
        self.lines.push(AuthKeyLine { raw, kind });
    }

    /// Reassembles the file's exact bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let total: usize = self.lines.iter().map(|l| l.raw.len()).sum();
        let mut out = Vec::with_capacity(total);
        for line in &self.lines {
            out.extend_from_slice(&line.raw);
        }
        out
    }

    /// Returns `true` if any key line in the file (managed by Legion or foreign) carries a public
    /// key whose decoded SSH wire blob equals `target_blob`.
    pub fn contains_key_blob(&self, target_blob: &[u8]) -> bool {
        self.lines
            .iter()
            .filter_map(|line| extract_line_key_blob(&line.raw))
            .any(|blob| blob == target_blob)
    }
}

/// Strips trailing `\n` and optional `\r` from a raw `authorized_keys` line.
fn strip_eol(raw: &[u8]) -> &[u8] {
    let without_lf = raw.strip_suffix(b"\n").unwrap_or(raw);
    without_lf.strip_suffix(b"\r").unwrap_or(without_lf)
}

/// Extracts and base64-decodes the SSH key wire blob from any `authorized_keys` line (foreign or
/// Legion-managed), parsing options, key type, and base64 as ASCII bytes so arbitrary non-UTF-8
/// comment bytes cannot hide an existing key from duplicate detection.
fn extract_line_key_blob(raw: &[u8]) -> Option<Vec<u8>> {
    let body = trim_leading_ws_bytes(strip_eol(raw));
    if body.is_empty() || body.starts_with(b"#") {
        return None;
    }
    let (_, rest) = split_options_and_rest_bytes(body);
    let (key_type, after_type) = split_ws_token_bytes(rest)?;
    if !is_known_ssh_key_type_bytes(key_type) {
        return None;
    }
    let (key_b64, _) = split_ws_token_bytes(after_type)?;
    STANDARD.decode(key_b64).ok()
}

/// Maximum attempts to re-read and re-apply an `authorized_keys` edit when a concurrent out-of-band
/// edit changes the file before rename.
const MAX_CONCURRENT_EDIT_RETRIES: usize = 4;

/// Sentinel detail carried through `OsError::Platform` to signal a pre-rename stamp mismatch.
const CONCURRENT_EDIT_SENTINEL: &str = "legion-authkeys-concurrent-edit";

/// File identity, size, and timestamps captured when reading `authorized_keys`, compared right
/// before atomic rename so an out-of-band hand edit is never silently overwritten.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileStamp {
    /// The file did not exist when read.
    Missing,
    /// The file existed with this identity and metadata.
    Present {
        /// File byte length.
        len: u64,
        /// Last modification timestamp.
        modified: Option<SystemTime>,
        /// Unix device id.
        #[cfg(unix)]
        dev: u64,
        /// Unix inode number.
        #[cfg(unix)]
        ino: u64,
        /// Unix status-change seconds (catches same-size in-place edits).
        #[cfg(unix)]
        ctime: i64,
        /// Unix status-change nanoseconds.
        #[cfg(unix)]
        ctime_nsec: i64,
    },
}

impl FileStamp {
    fn from_metadata(meta: &std::fs::Metadata) -> FileStamp {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            FileStamp::Present {
                len: meta.len(),
                modified: meta.modified().ok(),
                dev: meta.dev(),
                ino: meta.ino(),
                ctime: meta.ctime(),
                ctime_nsec: meta.ctime_nsec(),
            }
        }
        #[cfg(not(unix))]
        {
            FileStamp::Present {
                len: meta.len(),
                modified: meta.modified().ok(),
            }
        }
    }
}

/// Outcome of [`write_auth_keys_if_unchanged`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The file matched the expected stamp right before rename and was updated atomically.
    Written,
    /// The file changed on disk since it was read; the staged temp file was discarded untouched.
    ConcurrentEdit,
}

fn stamp_path_os(path: &Path) -> legion_os::Result<FileStamp> {
    match std::fs::metadata(path) {
        Ok(meta) => Ok(FileStamp::from_metadata(&meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(FileStamp::Missing),
        Err(error) => Err(legion_os::OsError::Platform {
            detail: format!("cannot inspect {}: {error}", path.display()),
        }),
    }
}

/// Reads `path` together with its [`FileStamp`] from the same open handle, returning an empty
/// [`AuthKeysFile`] and [`FileStamp::Missing`] when the file does not exist yet.
pub fn read_auth_keys_stamped(path: &Path) -> Result<(AuthKeysFile, FileStamp), ErrorBody> {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let meta = file.metadata().map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be inspected: {error}.", path.display()),
                )
            })?;
            let stamp = FileStamp::from_metadata(&meta);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be read: {error}.", path.display()),
                )
            })?;
            Ok((AuthKeysFile::parse(&bytes), stamp))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok((AuthKeysFile::default(), FileStamp::Missing))
        }
        Err(error) => {
            let mut body = ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be read: {error}.", path.display()),
            );
            if cfg!(windows) && error.kind() == io::ErrorKind::PermissionDenied {
                body = body.with_hint(
                    "Run from an elevated Administrator terminal so Legion can read administrators_authorized_keys.",
                );
            }
            Err(body)
        }
    }
}

/// Reads `path`, returning an empty [`AuthKeysFile`] when the file does not exist yet.
pub fn read_auth_keys(path: &Path) -> Result<AuthKeysFile, ErrorBody> {
    read_auth_keys_stamped(path).map(|(file, _)| file)
}

/// Atomically writes `bytes` to `write_path` only if `write_path`'s current [`FileStamp`] still
/// equals `expected` immediately before rename.
///
/// If an out-of-band edit changed `write_path` since it was read, discards the staged temp file
/// and returns [`WriteOutcome::ConcurrentEdit`] so the caller can re-read and retry without
/// losing the concurrent edit.
///
/// Legion processes serialize their own edits with the sidecar lock file. Because external tools
/// and hand editors do not acquire that lock, a tiny TOCTOU window remains between the pre-rename
/// metadata check and the atomic rename itself; this check catches every concurrent edit that
/// lands before that final syscall.
pub fn write_auth_keys_if_unchanged(
    target: &AuthKeysTarget,
    write_path: &Path,
    bytes: &[u8],
    expected: &FileStamp,
) -> Result<WriteOutcome, ErrorBody> {
    if let Some(parent) = write_path.parent() {
        let is_own_ssh_dir = target.is_default_home_ssh && target.path.parent() == Some(parent);
        ensure_parent_dir(parent, is_own_ssh_dir)?;
    }

    let mode = desired_auth_keys_mode(write_path);
    let options = WriteOptions::with_mode(mode).force_mode(true);
    let need_win_acl = target.windows_admin_acl;
    let check_path = write_path.to_path_buf();
    let expected_stamp = expected.clone();
    match write_atomic_prepared(write_path, bytes, options, move |temp_path| {
        if need_win_acl {
            legion_os::user::apply_windows_admin_ssh_acl(temp_path)?;
        }
        let current = stamp_path_os(&check_path)?;
        if current != expected_stamp {
            return Err(legion_os::OsError::Platform {
                detail: CONCURRENT_EDIT_SENTINEL.to_owned(),
            });
        }
        Ok(())
    }) {
        Ok(()) => Ok(WriteOutcome::Written),
        Err(legion_os::OsError::Platform { detail }) if detail == CONCURRENT_EDIT_SENTINEL => {
            Ok(WriteOutcome::ConcurrentEdit)
        }
        Err(error) => {
            let mut body = ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be written: {error}.", write_path.display()),
            );
            if target.windows_admin_acl {
                body = body.with_hint(
                    "Run from an elevated Administrator terminal so Legion can update administrators_authorized_keys and set its Administrators/SYSTEM ACL.",
                );
            }
            Err(body)
        }
    }
}

/// Runs an `authorized_keys` read-modify-write closure under the caller's lock, retrying from a
/// fresh read if a concurrent out-of-band hand edit modifies `write_path` before rename.
///
/// The closure receives the freshly read [`AuthKeysFile`] and returns `(Some(updated), value)` to
/// commit the edit or `(None, value)` when no write is needed.
///
/// Note that external editors that do not take Legion's lock still have a tiny TOCTOU window
/// between the pre-rename metadata stamp check and the atomic rename syscall.
pub fn edit_auth_keys_with_retry<T>(
    target: &AuthKeysTarget,
    write_path: &Path,
    edit: impl FnMut(AuthKeysFile) -> Result<(Option<AuthKeysFile>, T), ErrorBody>,
) -> Result<T, ErrorBody> {
    edit_auth_keys_with_hook(target, write_path, edit, |_| {})
}

fn edit_auth_keys_with_hook<T>(
    target: &AuthKeysTarget,
    write_path: &Path,
    mut edit: impl FnMut(AuthKeysFile) -> Result<(Option<AuthKeysFile>, T), ErrorBody>,
    mut before_write: impl FnMut(usize),
) -> Result<T, ErrorBody> {
    for attempt in 0..MAX_CONCURRENT_EDIT_RETRIES {
        let (file, stamp) = read_auth_keys_stamped(write_path)?;
        let (maybe_updated, value) = edit(file)?;
        let Some(updated) = maybe_updated else {
            return Ok(value);
        };
        before_write(attempt);
        match write_auth_keys_if_unchanged(target, write_path, &updated.to_bytes(), &stamp)? {
            WriteOutcome::Written => return Ok(value),
            WriteOutcome::ConcurrentEdit => continue,
        }
    }
    Err(ErrorBody::new(
        ErrorCode::Conflict,
        format!(
            "{} changed concurrently while Legion was updating it.",
            write_path.display()
        ),
    )
    .with_hint(
        "Retry once no other editor or process is modifying authorized_keys at the same time.",
    ))
}

/// Writes `bytes` atomically to `write_path`, ensuring safe Unix directory/file modes and
/// Windows `administrators_authorized_keys` ACLs and ownership.
///
/// On Unix:
/// - Only creates or tightens `~/.ssh` itself (`0o700`), never `chmod`ing the directory of a
///   symlink target outside `~/.ssh`.
/// - Tightens permissive file modes (e.g. `0o644` -> `0o600`) while preserving stricter existing
///   owner modes (e.g. `0o400` stays `0o400`).
pub fn write_auth_keys(
    target: &AuthKeysTarget,
    write_path: &Path,
    bytes: &[u8],
) -> Result<(), ErrorBody> {
    if let Some(parent) = write_path.parent() {
        let is_own_ssh_dir = target.is_default_home_ssh && target.path.parent() == Some(parent);
        ensure_parent_dir(parent, is_own_ssh_dir)?;
    }

    let mode = desired_auth_keys_mode(write_path);
    let options = WriteOptions::with_mode(mode).force_mode(true);
    let need_win_acl = target.windows_admin_acl;
    write_atomic_prepared(write_path, bytes, options, move |temp_path| {
        if need_win_acl {
            legion_os::user::apply_windows_admin_ssh_acl(temp_path)?;
        }
        Ok(())
    })
    .map_err(|error| {
        let mut body = ErrorBody::new(
            ErrorCode::Failed,
            format!("{} cannot be written: {error}.", write_path.display()),
        );
        if target.windows_admin_acl {
            body = body.with_hint(
                "Run from an elevated Administrator terminal so Legion can update administrators_authorized_keys and set its Administrators/SYSTEM ACL.",
            );
        }
        body
    })
}

fn desired_auth_keys_mode(write_path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(write_path) {
            let tightened = meta.permissions().mode() & 0o600;
            if tightened & 0o400 != 0 {
                return tightened;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = write_path;
    }
    0o600
}

pub(crate) fn ensure_parent_dir(dir: &Path, is_default_ssh_dir: bool) -> Result<(), ErrorBody> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.exists() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            if is_default_ssh_dir {
                builder.mode(0o700);
            }
            builder.create(dir).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be created: {error}.", dir.display()),
                )
            })?;
        }
        if is_default_ssh_dir {
            let meta = std::fs::metadata(dir).map_err(|error| {
                ErrorBody::new(
                    ErrorCode::Failed,
                    format!("{} cannot be inspected: {error}.", dir.display()),
                )
            })?;
            if meta.permissions().mode() & 0o077 != 0 {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(
                    |error| {
                        ErrorBody::new(
                            ErrorCode::Failed,
                            format!("Cannot set mode 0700 on {}: {error}.", dir.display()),
                        )
                    },
                )?;
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = is_default_ssh_dir;
        std::fs::create_dir_all(dir).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} cannot be created: {error}.", dir.display()),
            )
        })
    }
}

fn classify_line(raw: &[u8]) -> LineKind {
    let trimmed = trim_leading_ws_bytes(strip_eol(raw));
    if trimmed.is_empty() || trimmed.starts_with(b"#") {
        return LineKind::Foreign;
    }

    let (options_bytes, rest) = split_options_and_rest_bytes(trimmed);
    let Some(options_bytes) = options_bytes else {
        return LineKind::Foreign;
    };
    let Ok(options_str) = std::str::from_utf8(options_bytes) else {
        return LineKind::Foreign;
    };
    let Some((key_type, after_type)) = split_ws_token_bytes(rest) else {
        return LineKind::Foreign;
    };
    if !is_known_ssh_key_type_bytes(key_type) {
        return LineKind::Foreign;
    }
    let Some((key_b64_bytes, after_key)) = split_ws_token_bytes(after_type) else {
        return LineKind::Foreign;
    };
    let Ok(key_b64) = std::str::from_utf8(key_b64_bytes) else {
        return LineKind::Foreign;
    };
    let Ok(comment_raw) = std::str::from_utf8(after_key) else {
        return LineKind::Foreign;
    };
    let comment = comment_raw.trim_end_matches([' ', '\t']);

    let Some(parsed_opts) = parse_openssh_options(options_str) else {
        return LineKind::Foreign;
    };
    let has_restrict = parsed_opts
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("restrict") && v.is_none());
    if !has_restrict {
        return LineKind::Foreign;
    }

    let mut command_values = parsed_opts
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("command"));
    let Some((_, Some(command))) = command_values.next() else {
        return LineKind::Foreign;
    };
    if command_values.next().is_some() {
        return LineKind::Foreign;
    }

    let Some(parsed_cmd) = parse_exact_gate_command(command) else {
        return LineKind::Foreign;
    };

    if let Some(id) = comment.strip_prefix(INVITE_COMMENT_PREFIX)
        && !id.is_empty()
        && !id.chars().any(char::is_whitespace)
        && let GateCommandMode::Invite { id: ref cmd_id } = parsed_cmd.mode
        && cmd_id == id
    {
        let expiry_time = parsed_opts
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("expiry-time"))
            .and_then(|(_, v)| v.clone());
        return LineKind::Invite {
            id: id.to_owned(),
            key_b64: key_b64.to_owned(),
            expiry_time,
            home: parsed_cmd.home,
            account: parsed_cmd.account,
        };
    }

    if let Some(name) = comment.strip_prefix(DEVICE_COMMENT_PREFIX)
        && !name.is_empty()
        && !name.chars().any(char::is_whitespace)
        && let GateCommandMode::Device { principal, device } = parsed_cmd.mode
        && device == name
    {
        return LineKind::Device {
            name: name.to_owned(),
            key_b64: key_b64.to_owned(),
            principal,
            home: parsed_cmd.home,
            account: parsed_cmd.account,
        };
    }

    LineKind::Foreign
}

fn trim_leading_ws_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some(&first) = bytes.first() {
        if first == b' ' || first == b'\t' {
            bytes = &bytes[1..];
        } else {
            break;
        }
    }
    bytes
}

/// Splits an `authorized_keys` line (as raw bytes) into `(optional_options, rest_starting_with_key_type)`,
/// respecting OpenSSH double-quoted option strings and `\"` escapes without requiring UTF-8 in the
/// trailing comment.
fn split_options_and_rest_bytes(line: &[u8]) -> (Option<&[u8]>, &[u8]) {
    if let Some((first, _)) = split_ws_token_bytes(line)
        && is_known_ssh_key_type_bytes(first)
    {
        return (None, line);
    }
    let mut i = 0;
    let mut in_quotes = false;
    while i < line.len() {
        match line[i] {
            b'\\' if in_quotes && line.get(i + 1) == Some(&b'"') => {
                i += 2;
                continue;
            }
            b'"' => in_quotes = !in_quotes,
            b' ' | b'\t' if !in_quotes => {
                let options = &line[..i];
                let rest = trim_leading_ws_bytes(&line[i..]);
                return (Some(options), rest);
            }
            _ => {}
        }
        i += 1;
    }
    (None, line)
}

fn split_ws_token_bytes(s: &[u8]) -> Option<(&[u8], &[u8])> {
    let s = trim_leading_ws_bytes(s);
    if s.is_empty() {
        return None;
    }
    match s.iter().position(|&b| b == b' ' || b == b'\t') {
        Some(idx) => Some((&s[..idx], trim_leading_ws_bytes(&s[idx..]))),
        None => Some((s, &[])),
    }
}

fn is_known_ssh_key_type_bytes(token: &[u8]) -> bool {
    matches!(
        token,
        b"ssh-ed25519"
            | b"ssh-rsa"
            | b"ssh-dss"
            | b"ecdsa-sha2-nistp256"
            | b"ecdsa-sha2-nistp384"
            | b"ecdsa-sha2-nistp521"
            | b"sk-ssh-ed25519@openssh.com"
            | b"sk-ecdsa-sha2-nistp256@openssh.com"
    ) || token.ends_with(b"-cert-v01@openssh.com")
}

/// Parses an OpenSSH `authorized_keys` options list into `(name, optional_value)` pairs.
///
/// Rejects malformed options such as unclosed double quotes or trailing characters after a quoted
/// option value.
fn parse_openssh_options(options: &str) -> Option<Vec<(String, Option<String>)>> {
    let bytes = options.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        let name = &options[name_start..i];
        if name.is_empty() {
            return None;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            if i < bytes.len() && bytes[i] == b'"' {
                i += 1;
                let mut val = String::new();
                let mut closed = false;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        closed = true;
                        break;
                    }
                    if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'"') {
                        val.push('"');
                        i += 2;
                    } else {
                        let ch = options[i..].chars().next()?;
                        val.push(ch);
                        i += ch.len_utf8();
                    }
                }
                if !closed {
                    return None;
                }
                if i < bytes.len() {
                    if bytes[i] != b',' {
                        return None;
                    }
                    i += 1;
                    if i == bytes.len() {
                        return None;
                    }
                }
                out.push((name.to_owned(), Some(val)));
            } else {
                let val_start = i;
                while i < bytes.len() && bytes[i] != b',' {
                    i += 1;
                }
                out.push((name.to_owned(), Some(options[val_start..i].to_owned())));
                if i < bytes.len() {
                    i += 1;
                    if i == bytes.len() {
                        return None;
                    }
                }
            }
        } else {
            out.push((name.to_owned(), None));
            if i < bytes.len() {
                i += 1;
                if i == bytes.len() {
                    return None;
                }
            }
        }
    }
    Some(out)
}

enum GateCommandMode {
    Device { principal: String, device: String },
    Invite { id: String },
}

struct ParsedGateCommand {
    mode: GateCommandMode,
    home: Option<PinnedHome>,
    account: Option<String>,
}

fn exe_looks_like_legion(exe: &str) -> bool {
    let path = Path::new(exe);
    if !path.is_absolute() && !looks_windows_absolute(path) {
        return false;
    }
    let basename = exe.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = basename
        .strip_suffix(".exe")
        .or_else(|| basename.strip_suffix(".EXE"))
        .unwrap_or(basename);
    let lower = stem.to_ascii_lowercase();
    lower == "legion" || lower.starts_with("legion-") || lower.starts_with("legion_")
}

/// Strictly parses an unescaped forced command and returns `Some` only when it is an exact
/// `legion gate --as <principal> --device <name>` or `legion gate --enrol <id>` invocation with
/// no unknown flags, extra arguments, duplicate flags, or shell operators.
fn parse_exact_gate_command(command: &str) -> Option<ParsedGateCommand> {
    let tokens = split_forced_command_tokens_strict(command)?;
    let (exe, subcmd, rest) = if tokens.first().map(String::as_str) == Some("&") {
        if tokens.len() < 3 {
            return None;
        }
        (&tokens[1], &tokens[2], &tokens[3..])
    } else {
        if tokens.len() < 2 {
            return None;
        }
        (&tokens[0], &tokens[1], &tokens[2..])
    };
    if !exe_looks_like_legion(exe) || subcmd != "gate" {
        return None;
    }

    let mut principal: Option<String> = None;
    let mut device: Option<String> = None;
    let mut enrol_id: Option<String> = None;
    let mut home_root: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut account: Option<String> = None;

    let mut i = 0;
    while i < rest.len() {
        let token = &rest[i];
        let (flag, value, step) = if let Some((k, v)) = token.split_once('=') {
            (k, v, 1)
        } else {
            let v = rest.get(i + 1)?;
            (token.as_str(), v.as_str(), 2)
        };
        match flag {
            "--as" => {
                if principal.is_some() || Principal::parse(value).is_err() {
                    return None;
                }
                principal = Some(value.to_owned());
            }
            "--device" => {
                if device.is_some()
                    || value.is_empty()
                    || value.chars().any(|c| c.is_control() || c.is_whitespace())
                {
                    return None;
                }
                device = Some(value.to_owned());
            }
            "--enrol" => {
                if enrol_id.is_some()
                    || value.is_empty()
                    || value.chars().any(|c| c.is_control() || c.is_whitespace())
                {
                    return None;
                }
                enrol_id = Some(value.to_owned());
            }
            "--home" => {
                let p = Path::new(value);
                if home_root.is_some()
                    || value.is_empty()
                    || (!p.is_absolute() && !looks_windows_absolute(p))
                {
                    return None;
                }
                home_root = Some(PathBuf::from(value));
            }
            "--config-dir" => {
                let p = Path::new(value);
                if config_dir.is_some()
                    || value.is_empty()
                    || (!p.is_absolute() && !looks_windows_absolute(p))
                {
                    return None;
                }
                config_dir = Some(PathBuf::from(value));
            }
            "--state-dir" => {
                let p = Path::new(value);
                if state_dir.is_some()
                    || value.is_empty()
                    || (!p.is_absolute() && !looks_windows_absolute(p))
                {
                    return None;
                }
                state_dir = Some(PathBuf::from(value));
            }
            "--account" => {
                if account.is_some() || validate_account_name(value).is_err() {
                    return None;
                }
                account = Some(value.to_owned());
            }
            _ => return None,
        }
        i += step;
    }

    let home = match (home_root, config_dir, state_dir) {
        (Some(root), None, None) => Some(PinnedHome::Root(root)),
        (None, Some(config_dir), Some(state_dir)) => Some(PinnedHome::Split {
            config_dir,
            state_dir,
        }),
        (None, None, None) => None,
        _ => return None,
    };

    let mode = match (principal, device, enrol_id) {
        (Some(principal), Some(device), None) => GateCommandMode::Device { principal, device },
        (None, None, Some(id)) => GateCommandMode::Invite { id },
        _ => return None,
    };
    Some(ParsedGateCommand {
        mode,
        home,
        account,
    })
}

/// Strictly splits an unescaped forced command into argv-like tokens, supporting POSIX `'...'`
/// (with `'\''`), PowerShell `& '...'` (with `''`), and Windows `cmd.exe` `"..."` segments.
///
/// Returns `None` on unclosed quotes or unquoted shell operators/metacharacters.
fn split_forced_command_tokens_strict(command: &str) -> Option<Vec<String>> {
    let is_powershell = command.starts_with("& ") || command.starts_with("&\t");
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let chars: Vec<char> = command.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        match ch {
            ' ' | '\t' => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
                i += 1;
            }
            '\'' => {
                in_token = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        if is_powershell && chars.get(i + 1) == Some(&'\'') {
                            // PowerShell doubled single quote inside '...'.
                            current.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        if chars[i].is_control() {
                            return None;
                        }
                        current.push(chars[i]);
                        i += 1;
                    }
                }
                if !closed {
                    return None;
                }
            }
            '"' => {
                in_token = true;
                i += 1;
                let mut closed = false;
                while i < chars.len() {
                    if chars[i] == '"' {
                        i += 1;
                        closed = true;
                        break;
                    }
                    if chars[i].is_control() {
                        return None;
                    }
                    current.push(chars[i]);
                    i += 1;
                }
                if !closed {
                    return None;
                }
            }
            '\\' if !is_powershell && chars.get(i + 1) == Some(&'\'') => {
                in_token = true;
                current.push('\'');
                i += 2;
            }
            '&' if is_powershell
                && tokens.is_empty()
                && !in_token
                && matches!(chars.get(i + 1), Some(' ' | '\t')) =>
            {
                tokens.push("&".to_owned());
                i += 1;
            }
            ';' | '|' | '<' | '>' | '`' | '$' | '(' | ')' | '&' | '*' | '?' | '{' | '}' | '['
            | ']' | '!' | '#' => return None,
            c if c.is_control() => return None,
            _ => {
                in_token = true;
                current.push(ch);
                i += 1;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Some(tokens)
}

/// Returns `true` when a line's pinned `--account` matches `caller_account`.
///
/// When `require_pinned` is `true` (on Windows's shared `administrators_authorized_keys`), only
/// lines explicitly pinned to `caller_account` match so one Administrator account cannot list or
/// remove another Administrator's paired devices or invites. Otherwise, unpinned lines belong to
/// the file's single user while pinned lines match only the named account.
pub fn line_matches_account(
    line_account: Option<&str>,
    caller_account: Option<&str>,
    require_pinned: bool,
    case_insensitive: bool,
) -> bool {
    match (line_account, caller_account) {
        (Some(la), Some(ca)) => account_names_match(la, ca, case_insensitive),
        (Some(_), None) => false,
        (None, _) => !require_pinned,
    }
}

/// Returns `true` when a line's pinned home matches `paths`'s Legion home.
///
/// Lines pinned to a different home belong to another Legion home sharing this `authorized_keys`
/// file.
pub fn line_matches_home(line_home: Option<&PinnedHome>, paths: &Paths) -> bool {
    match line_home {
        Some(pinned) => pinned.matches_paths(paths),
        None => paths.explicit_home().is_none(),
    }
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => {
            if cfg!(windows) {
                a.to_string_lossy()
                    .eq_ignore_ascii_case(&b.to_string_lossy())
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Exact mirror of OpenSSH `auth-options.c` `opt_dequote` for round-trip verification.
    fn openssh_opt_dequote(quoted_inner: &str) -> String {
        let mut out = String::new();
        let chars: Vec<char> = quoted_inner.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\\' && chars.get(i + 1) == Some(&'"') {
                out.push('"');
                i += 2;
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }

    #[test]
    fn forced_command_quoting_handles_plain_spaces_and_quotes_on_unix_and_windows() {
        let plain_unix = format_forced_command(
            Path::new("/home/me/.local/bin/legion"),
            &["gate", "--as", "phone"],
            ForcedCommandShell::Posix,
        )
        .unwrap();
        assert_eq!(plain_unix, "/home/me/.local/bin/legion gate --as phone");

        let space_unix = format_forced_command(
            Path::new("/Users/me/My Apps/legion"),
            &["gate", "--as", "phone"],
            ForcedCommandShell::Posix,
        )
        .unwrap();
        assert_eq!(
            openssh_opt_dequote(&space_unix),
            "'/Users/me/My Apps/legion' gate --as phone"
        );

        let quote_unix = format_forced_command(
            Path::new(r#"/Users/me/O'Neil "Lab"/legion"#),
            &["gate", "--enrol", "abc123"],
            ForcedCommandShell::Posix,
        )
        .unwrap();
        assert!(quote_unix.contains("\\\"Lab\\\""));
        assert_eq!(
            openssh_opt_dequote(&quote_unix),
            r#"''/Users/me/O'\''Neil "Lab"/legion'' gate --enrol abc123"#
                .strip_prefix('\'')
                .unwrap()
                .replacen("'' ", "' ", 1)
        );

        // Windows cmd.exe: plain, spaces, non-ASCII, and metacharacter refusals.
        let plain_cmd = format_forced_command(
            Path::new(r"C:\Users\me\AppData\Local\legion\legion.exe"),
            &["gate", "--as", "phone"],
            ForcedCommandShell::WindowsCmd,
        )
        .unwrap();
        assert_eq!(
            plain_cmd,
            r"C:\Users\me\AppData\Local\legion\legion.exe gate --as phone"
        );

        let space_unicode_cmd = format_forced_command(
            Path::new(r"C:\Users\me\My Légion 日本\legion.exe"),
            &[
                "gate",
                "--as",
                "agent:example",
                "--home",
                r"C:\Users\me\Legion Home ü",
            ],
            ForcedCommandShell::WindowsCmd,
        )
        .unwrap();
        assert_eq!(
            openssh_opt_dequote(&space_unicode_cmd),
            r#""C:\Users\me\My Légion 日本\legion.exe" gate --as agent:example --home "C:\Users\me\Legion Home ü""#
        );
        for bad_cmd_path in [
            r"C:\Users\me\%USERNAME%\legion.exe",
            r"C:\Users\me\A&B\legion.exe",
            r"C:\Program Files (x86)\legion.exe",
            r"C:\Users\me\a^b\legion.exe",
            r"C:\Users\me\bang!\legion.exe",
            r#"C:\Users\me\"quote"\legion.exe"#,
        ] {
            assert!(
                format_forced_command(
                    Path::new(bad_cmd_path),
                    &["gate", "--as", "phone"],
                    ForcedCommandShell::WindowsCmd,
                )
                .is_err(),
                "cmd should reject {bad_cmd_path}"
            );
        }

        // Windows PowerShell: `& '...'` with doubled single quotes, spaces, non-ASCII, and `@`-safe
        // argument quoting (preventing PowerShell splatting of `@`-prefixed arguments).
        let ps_cmd = format_forced_command(
            Path::new(r"C:\Users\me\O'Neil\My Légion 日本\legion.exe"),
            &[
                "gate",
                "--as",
                "@phone",
                "--account",
                "O'Neil",
                "--home",
                r"C:\Users\me\O'Neil\Program Files (x86)\Legion Home",
            ],
            ForcedCommandShell::WindowsPowerShell,
        )
        .unwrap();
        assert_eq!(
            openssh_opt_dequote(&ps_cmd),
            r"& 'C:\Users\me\O''Neil\My Légion 日本\legion.exe' gate --as '@phone' --account 'O''Neil' --home 'C:\Users\me\O''Neil\Program Files (x86)\Legion Home'"
        );

        // PowerShell treats Unicode single-quote variants U+2018..=U+201B as string delimiters,
        // so paths or arguments containing any of them must be refused rather than escaped.
        for bad_quote in ['\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let bad_exe = format!(r"C:\Users\O{bad_quote}Neil\legion.exe");
            let exe_err = format_forced_command(
                Path::new(&bad_exe),
                &["gate", "--as", "phone"],
                ForcedCommandShell::WindowsPowerShell,
            )
            .unwrap_err();
            assert_eq!(exe_err.code, ErrorCode::Failed, "{bad_quote:?}");

            let bad_arg = format!(r"C:\Users\O{bad_quote}Neil\legion");
            let arg_err = format_forced_command(
                Path::new(r"C:\legion\legion.exe"),
                &["gate", "--as", "phone", "--home", &bad_arg],
                ForcedCommandShell::WindowsPowerShell,
            )
            .unwrap_err();
            assert_eq!(arg_err.code, ErrorCode::Failed, "{bad_quote:?}");
        }

        // Round-trip parsing of a PowerShell device line recovers principal, account, and home.
        let ps_line = format_device_line(
            Path::new(r"C:\Users\me\O'Neil\My Légion\legion.exe"),
            "agent:example",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000",
            "pixel",
            &PinnedHome::Root(PathBuf::from(r"C:\Users\me\O'Neil\Legion Home")),
            Some("O'Neil"),
            ForcedCommandShell::WindowsPowerShell,
        )
        .unwrap();
        let parsed_ps = AuthKeysFile::parse(format!("{ps_line}\n").as_bytes());
        assert!(matches!(
            &parsed_ps.lines[0].kind,
            LineKind::Device {
                name,
                principal,
                home: Some(PinnedHome::Root(h)),
                account: Some(a),
                ..
            } if name == "pixel"
                && principal == "agent:example"
                && h == Path::new(r"C:\Users\me\O'Neil\Legion Home")
                && a == "O'Neil"
        ));

        // Windows POSIX DefaultShell (e.g. Git Bash) single-quotes Windows paths safely and
        // round-trips through strict classification; unknown shells are refused with Unsupported.
        assert_eq!(
            ForcedCommandShell::from_windows_shell(legion_os::user::WindowsSshShell::Posix)
                .unwrap(),
            ForcedCommandShell::Posix
        );
        let unknown_err = ForcedCommandShell::from_windows_shell(
            legion_os::user::WindowsSshShell::Unknown(r"C:\Program Files\nu\nu.exe".to_owned()),
        )
        .unwrap_err();
        assert_eq!(unknown_err.code, ErrorCode::Unsupported);
        assert!(
            unknown_err
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("DefaultShell")),
            "{:?}",
            unknown_err.hint
        );
        let win_posix_line = format_device_line(
            Path::new(r"C:\Users\me\O'Neil\legion.exe"),
            "agent:example",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000",
            "pixel",
            &PinnedHome::Root(PathBuf::from(r"C:\Users\me\Legion Home")),
            Some("me"),
            ForcedCommandShell::Posix,
        )
        .unwrap();
        assert!(
            win_posix_line.contains(r#"command="'C:\Users\me\O'\''Neil\legion.exe'"#),
            "{win_posix_line}"
        );
        let parsed_win_posix = AuthKeysFile::parse(format!("{win_posix_line}\n").as_bytes());
        assert!(matches!(
            &parsed_win_posix.lines[0].kind,
            LineKind::Device {
                name,
                principal,
                home: Some(PinnedHome::Root(h)),
                account: Some(a),
                ..
            } if name == "pixel"
                && principal == "agent:example"
                && h == Path::new(r"C:\Users\me\Legion Home")
                && a == "me"
        ));

        // Split default-home lines pin both `--config-dir` and `--state-dir` and round-trip cleanly.
        let split_line = format_device_line(
            Path::new("/home/me/.local/bin/legion"),
            "phone",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000",
            "pixel",
            &PinnedHome::Split {
                config_dir: PathBuf::from("/home/me/.config/legion"),
                state_dir: PathBuf::from("/home/me/.local/state/legion"),
            },
            None,
            ForcedCommandShell::Posix,
        )
        .unwrap();
        assert!(
            split_line.contains(
                "--config-dir /home/me/.config/legion --state-dir /home/me/.local/state/legion"
            ),
            "{split_line}"
        );
        let parsed_split = AuthKeysFile::parse(format!("{split_line}\n").as_bytes());
        assert!(matches!(
            &parsed_split.lines[0].kind,
            LineKind::Device {
                name,
                principal,
                home: Some(PinnedHome::Split { config_dir, state_dir }),
                ..
            } if name == "pixel"
                && principal == "phone"
                && config_dir == Path::new("/home/me/.config/legion")
                && state_dir == Path::new("/home/me/.local/state/legion")
        ));

        // Control characters and relative paths are refused.
        assert!(
            format_forced_command(
                Path::new("/home/me/bad\nlegion"),
                &["gate"],
                ForcedCommandShell::Posix
            )
            .is_err()
        );
        assert!(
            format_forced_command(
                Path::new("relative/legion"),
                &["gate"],
                ForcedCommandShell::Posix
            )
            .is_err()
        );
    }

    #[test]
    fn foreign_lines_comments_and_missing_trailing_newline_are_preserved_byte_for_byte() {
        let initial = b"# Header comment\n\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000 me@laptop\nrestrict,command=\"/home/me/legion gate --as phone --device pixel\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI1111111111111111111111111111111111111111111 legion-device:pixel\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI2222222222222222222222222222222222222222222 final-no-newline";
        let mut parsed = AuthKeysFile::parse(initial);
        assert_eq!(parsed.lines.len(), 5);
        assert_eq!(parsed.lines[0].kind, LineKind::Foreign);
        assert_eq!(parsed.lines[1].kind, LineKind::Foreign);
        assert_eq!(parsed.lines[2].kind, LineKind::Foreign);
        assert!(matches!(
            &parsed.lines[3].kind,
            LineKind::Device { name, principal, .. } if name == "pixel" && principal == "phone"
        ));
        assert_eq!(parsed.lines[4].kind, LineKind::Foreign);

        // Removing the device line preserves every other line byte for byte, including the missing
        // trailing newline on the final foreign line.
        assert!(parsed.retain(
            |line| !matches!(&line.kind, LineKind::Device { name, .. } if name == "pixel")
        ));
        let expected = b"# Header comment\n\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000 me@laptop\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI2222222222222222222222222222222222222222222 final-no-newline";
        assert_eq!(parsed.to_bytes(), expected);

        // Foreign lines can never be dropped even if the predicate returns false.
        assert!(!parsed.retain(|_| false));
        assert_eq!(parsed.to_bytes(), expected);
    }

    #[test]
    fn appending_to_file_without_trailing_newline_inserts_separator() {
        let initial = b"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000 foreign";
        let mut parsed = AuthKeysFile::parse(initial);
        parsed.append_line("restrict,command=\"/home/me/legion gate --as phone --device tablet\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI1111111111111111111111111111111111111111111 legion-device:tablet");
        let bytes = parsed.to_bytes();
        let text = std::str::from_utf8(&bytes).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], std::str::from_utf8(initial).unwrap());
        assert!(lines[1].ends_with("legion-device:tablet"));
    }

    #[test]
    fn parses_windows_sshd_config_admin_match_block() {
        let default_cfg = "\
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
Match Group administrators
       AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
";
        assert!(windows_sshd_uses_admin_file_from_text(default_cfg));

        let commented_out = "\
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
# Match Group administrators
#       AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
";
        assert!(!windows_sshd_uses_admin_file_from_text(commented_out));

        let other_match = "\
Match Group users
    AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys
";
        assert!(!windows_sshd_uses_admin_file_from_text(other_match));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outside_home_is_refused_and_symlink_inside_home_is_followed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        let outside_file = outside.join("authorized_keys");
        std::fs::write(&outside_file, "").unwrap();
        symlink(&outside_file, home.join(".ssh").join("authorized_keys")).unwrap();

        let target = AuthKeysTarget {
            path: home.join(".ssh").join("authorized_keys"),
            is_default_home_ssh: true,
            windows_admin_acl: false,
        };
        let err = resolve_safe_write_path(&target, Some(&home)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(err.message.contains("outside home"), "{}", err.message);

        // Symlink inside home is accepted and resolves to its target file.
        std::fs::remove_file(home.join(".ssh").join("authorized_keys")).unwrap();
        let inside_dir = home.join("dotfiles");
        std::fs::create_dir_all(&inside_dir).unwrap();
        let inside_file = inside_dir.join("authorized_keys");
        std::fs::write(&inside_file, "").unwrap();
        symlink(&inside_file, home.join(".ssh").join("authorized_keys")).unwrap();
        let resolved = resolve_safe_write_path(&target, Some(&home)).unwrap();
        assert_eq!(resolved, dunce::canonicalize(&inside_file).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn write_auth_keys_sets_ssh_dir_700_and_authorized_keys_600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let ssh_dir = home.join(".ssh");
        let auth_file = ssh_dir.join("authorized_keys");
        let target = AuthKeysTarget {
            path: auth_file.clone(),
            is_default_home_ssh: true,
            windows_admin_acl: false,
        };

        // Creates ~/.ssh with 0700 and authorized_keys with 0600.
        write_auth_keys(&target, &auth_file, b"# test\n").unwrap();
        assert_eq!(
            std::fs::metadata(&ssh_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&auth_file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Tightens permissive permissions back to 0700 / 0600 on subsequent edits.
        std::fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_auth_keys(&target, &auth_file, b"# updated\n").unwrap();
        assert_eq!(
            std::fs::metadata(&ssh_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&auth_file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Keeps stricter existing file mode (0400 stays 0400, never loosened to 0600).
        std::fs::set_permissions(&auth_file, std::fs::Permissions::from_mode(0o400)).unwrap();
        write_auth_keys(&target, &auth_file, b"# readonly\n").unwrap();
        assert_eq!(
            std::fs::metadata(&auth_file).unwrap().permissions().mode() & 0o777,
            0o400
        );

        // Does NOT chmod the parent directory of a symlink target outside ~/.ssh.
        let dotfiles = home.join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::set_permissions(&dotfiles, std::fs::Permissions::from_mode(0o755)).unwrap();
        let linked_target = dotfiles.join("authorized_keys");
        std::fs::write(&linked_target, "# dotfiles\n").unwrap();
        std::fs::set_permissions(&linked_target, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::remove_file(&auth_file).unwrap();
        std::os::unix::fs::symlink(&linked_target, &auth_file).unwrap();

        let resolved = resolve_safe_write_path(&target, Some(&home)).unwrap();
        write_auth_keys(&target, &resolved, b"# dotfiles updated\n").unwrap();
        assert_eq!(
            std::fs::metadata(&dotfiles).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&resolved).unwrap().permissions().mode() & 0o777,
            0o400
        );
    }

    #[test]
    fn concurrent_hand_edit_is_detected_and_retried_from_fresh_read() {
        let dir = tempfile::tempdir().unwrap();
        let auth_file = dir.path().join("authorized_keys");
        std::fs::write(&auth_file, "# initial\n").unwrap();
        let target = AuthKeysTarget {
            path: auth_file.clone(),
            is_default_home_ssh: false,
            windows_admin_acl: false,
        };

        // 1. Direct write_auth_keys_if_unchanged detects a concurrent edit without overwriting it.
        let (mut file, stamp) = read_auth_keys_stamped(&auth_file).unwrap();
        std::fs::write(&auth_file, "# initial\n# concurrent hand edit\n").unwrap();
        file.append_line(
            "restrict,command=\"/home/me/legion gate --as phone --device a\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000 legion-device:a",
        );
        let outcome =
            write_auth_keys_if_unchanged(&target, &auth_file, &file.to_bytes(), &stamp).unwrap();
        assert_eq!(outcome, WriteOutcome::ConcurrentEdit);
        assert_eq!(
            std::fs::read_to_string(&auth_file).unwrap(),
            "# initial\n# concurrent hand edit\n"
        );

        // 2. edit_auth_keys_with_hook retries on the first collision and preserves BOTH the
        //    concurrent hand edit and Legion's new line.
        let mut attempts = 0usize;
        edit_auth_keys_with_hook(
            &target,
            &auth_file,
            |mut auth| {
                attempts += 1;
                auth.append_line(
                    "restrict,command=\"/home/me/legion gate --as phone --device b\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI1111111111111111111111111111111111111111111 legion-device:b",
                );
                Ok((Some(auth), ()))
            },
            |attempt| {
                if attempt == 0 {
                    std::fs::write(
                        &auth_file,
                        "# initial\n# concurrent hand edit\n# second hand edit\n",
                    )
                    .unwrap();
                }
            },
        )
        .unwrap();
        assert_eq!(attempts, 2);
        let final_text = std::fs::read_to_string(&auth_file).unwrap();
        assert!(final_text.contains("# second hand edit\n"), "{final_text}");
        assert!(final_text.contains("legion-device:b"), "{final_text}");

        // 3. Persistent concurrent edits fail with Conflict without clobbering the last edit.
        let conflict = edit_auth_keys_with_hook(
            &target,
            &auth_file,
            |mut auth| {
                auth.append_line("# never lands");
                Ok((Some(auth), ()))
            },
            |attempt| {
                std::fs::write(&auth_file, format!("# hand edit {attempt}\n")).unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(conflict.code, ErrorCode::Conflict);
        assert_eq!(
            std::fs::read_to_string(&auth_file).unwrap(),
            format!("# hand edit {}\n", MAX_CONCURRENT_EDIT_RETRIES - 1)
        );
    }

    #[test]
    fn parses_invite_line_with_quoted_path_and_expiry() {
        let ts: Timestamp = "2026-11-24T12:15:00Z".parse().unwrap();
        let line = format_invite_line(
            Path::new("/Users/me/My Apps/legion"),
            "deadbeef01",
            ts,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000",
            &PinnedHome::Root(PathBuf::from("/Users/me/My Legion Home")),
            Some("Alice"),
            ForcedCommandShell::Posix,
        )
        .unwrap();
        // 12:15:00Z + 14h = next day 02:15 (12 digits, no trailing Z for OpenSSH 8.9 compatibility).
        assert!(line.contains("expiry-time=\"202611250215\""), "{line}");
        assert!(!line.contains("Z\""), "{line}");

        // Non-zero seconds round UP to the next whole minute.
        let with_secs: Timestamp = "2026-11-24T12:15:01Z".parse().unwrap();
        assert_eq!(format_sshd_expiry(with_secs), "202611250216");

        let parsed = AuthKeysFile::parse(format!("{line}\n").as_bytes());
        assert_eq!(parsed.lines.len(), 1);
        match &parsed.lines[0].kind {
            LineKind::Invite {
                id,
                key_b64,
                expiry_time,
                home,
                account,
            } => {
                assert_eq!(id, "deadbeef01");
                assert!(key_b64.starts_with("AAAAC3Nza"));
                assert_eq!(expiry_time.as_deref(), Some("202611250215"));
                assert_eq!(
                    home.as_ref(),
                    Some(&PinnedHome::Root(PathBuf::from("/Users/me/My Legion Home")))
                );
                assert_eq!(account.as_deref(), Some("Alice"));
            }
            other => panic!("unexpected {other:?}"),
        }

        // Case-insensitive matching for Windows shared administrators_authorized_keys.
        assert!(line_matches_account(
            Some("Alice"),
            Some("alice"),
            true,
            true
        ));
        assert!(!line_matches_account(
            Some("Alice"),
            Some("alice"),
            true,
            false
        ));
        assert!(!line_matches_account(
            Some("Alice"),
            Some("bob"),
            true,
            true
        ));
        assert!(!line_matches_account(None, Some("alice"), true, true));
        assert!(line_matches_account(None, Some("alice"), false, false));
    }

    #[cfg(unix)]
    #[test]
    fn verify_owner_refuses_mismatched_effective_uid_for_file_ssh_dir_and_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let actual_uid = legion_os::user::effective_uid().unwrap();
        let wrong_uid = if actual_uid == 0 { 65534 } else { 0 };

        let ssh_dir = home.join(".ssh");
        let auth_file = ssh_dir.join("authorized_keys");

        // 1. Neither authorized_keys nor ~/.ssh exists yet: falls back to home directory owner.
        assert!(verify_owner_matches_euid(&auth_file, Some(&home), true, Some(actual_uid)).is_ok());
        let err =
            verify_owner_matches_euid(&auth_file, Some(&home), true, Some(wrong_uid)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(err.message.contains("owned by"), "{}", err.message);
        assert!(
            err.hint.as_deref().is_some_and(|h| h.contains("sudo -u")),
            "{:?}",
            err.hint
        );

        // 2. ~/.ssh exists, authorized_keys missing: checks ~/.ssh owner.
        std::fs::create_dir_all(&ssh_dir).unwrap();
        assert!(verify_owner_matches_euid(&auth_file, Some(&home), true, Some(actual_uid)).is_ok());
        let err =
            verify_owner_matches_euid(&auth_file, Some(&home), true, Some(wrong_uid)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // 3. authorized_keys exists: checks file owner directly.
        std::fs::write(&auth_file, "# existing\n").unwrap();
        assert!(verify_owner_matches_euid(&auth_file, Some(&home), true, Some(actual_uid)).is_ok());
        let err =
            verify_owner_matches_euid(&auth_file, Some(&home), true, Some(wrong_uid)).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);

        // 4. Even when authorized_keys itself matches euid, a mismatched owner on ~/.ssh or home
        //    is still refused (never stops after the first existing path).
        let err_ssh =
            verify_owner_matches_euid_with(&auth_file, Some(&home), true, Some(1000), |path| {
                if path == auth_file || path == home {
                    Ok(Some(1000))
                } else if path == ssh_dir {
                    Ok(Some(0))
                } else {
                    Ok(None)
                }
            })
            .unwrap_err();
        assert_eq!(err_ssh.code, ErrorCode::Denied);
        assert!(err_ssh.message.contains(".ssh"), "{}", err_ssh.message);

        let err_home =
            verify_owner_matches_euid_with(&auth_file, Some(&home), true, Some(1000), |path| {
                if path == auth_file || path == ssh_dir {
                    Ok(Some(1000))
                } else if path == home {
                    Ok(Some(0))
                } else {
                    Ok(None)
                }
            })
            .unwrap_err();
        assert_eq!(err_home.code, ErrorCode::Denied);
        assert!(
            err_home.message.contains(&home.display().to_string()),
            "{}",
            err_home.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_exe_refuses_group_or_world_writable_binary_or_parent_dir() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("exec-dir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fake_exe = sub.join("legion");
        std::fs::write(&fake_exe, b"#!/bin/sh\n").unwrap();

        // 1. Group-writable binary is refused naming the binary itself.
        std::fs::set_permissions(&fake_exe, std::fs::Permissions::from_mode(0o775)).unwrap();
        let err = verify_exe_not_group_or_world_writable(&fake_exe).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(
            err.message.contains("the legion executable"),
            "{}",
            err.message
        );
        assert!(
            err.hint
                .as_deref()
                .is_some_and(|h| h.contains("chmod go-w")),
            "{:?}",
            err.hint
        );

        // 2. Clean 0755 binary inside a group-writable parent directory is refused naming the parent.
        std::fs::set_permissions(&fake_exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o775)).unwrap();
        let err = verify_exe_not_group_or_world_writable(&fake_exe).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(
            err.message.contains("ancestor directory"),
            "{}",
            err.message
        );
        assert!(err.message.contains("exec-dir"), "{}", err.message);

        // 3. The test binary itself in target/ has no group-/world-writable ancestors.
        let current = std::env::current_exe().unwrap();
        assert!(verify_exe_not_group_or_world_writable(&current).is_ok());
    }

    #[test]
    fn non_utf8_comment_on_foreign_line_still_blocks_duplicate_key_and_preserves_bytes() {
        use crate::device::keys::{ed25519_public_key_from_seed, encode_ed25519_wire_blob};

        let seed = [33_u8; 32];
        let pubkey = ed25519_public_key_from_seed(&seed);
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let blob = encode_ed25519_wire_blob(&signing.verifying_key().to_bytes());

        // Plain key with invalid UTF-8 bytes in comment, plus an option-prefixed key with invalid UTF-8 comment.
        let mut raw = Vec::new();
        raw.extend_from_slice(pubkey.as_bytes());
        raw.extend_from_slice(b" foreign-\xff\xfe-comment\n");
        let parsed = AuthKeysFile::parse(&raw);
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].kind, LineKind::Foreign);
        assert!(
            parsed.contains_key_blob(&blob),
            "non-UTF-8 comment must not hide the key from duplicate detection"
        );
        assert_eq!(parsed.to_bytes(), raw);

        // Also with leading options before the key and invalid UTF-8 in the comment.
        let mut raw_with_opts = Vec::new();
        raw_with_opts.extend_from_slice(b"from=\"192.0.2.1\",restrict ");
        raw_with_opts.extend_from_slice(pubkey.as_bytes());
        raw_with_opts.extend_from_slice(b" \x80\x81\n");
        let parsed_opts = AuthKeysFile::parse(&raw_with_opts);
        assert_eq!(parsed_opts.lines[0].kind, LineKind::Foreign);
        assert!(parsed_opts.contains_key_blob(&blob));
        assert_eq!(parsed_opts.to_bytes(), raw_with_opts);
    }

    #[test]
    fn strict_classification_requires_restrict_and_exact_gate_command_with_matching_id() {
        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000";
        let cases = [
            // Hand-written key carrying a legion-device comment without forced command / restrict.
            format!("{key} legion-device:handwritten\n"),
            // Hand-written forced command missing `restrict`.
            format!(
                "command=\"/home/me/legion gate --as phone --device pixel\" {key} legion-device:pixel\n"
            ),
            // `restrict` only inside a quoted option value, not as a bare option.
            format!(
                "environment=\"FOO=restrict\",command=\"/home/me/legion gate --as phone --device pixel\" {key} legion-device:pixel\n"
            ),
            // Earlier-branch line missing `--device <name>` is not recognised as a Legion line.
            format!(
                "restrict,command=\"/home/me/legion gate --as phone\" {key} legion-device:legacy\n"
            ),
            // Mismatched `--device` vs comment name.
            format!(
                "restrict,command=\"/home/me/legion gate --as phone --device other\" {key} legion-device:pixel\n"
            ),
            // Shell chaining / metacharacters appended to gate command.
            format!(
                "restrict,command=\"/home/me/legion gate --as phone --device pixel; id\" {key} legion-device:pixel\n"
            ),
            format!(
                "restrict,command=\"/home/me/legion gate --as phone --device pixel && id\" {key} legion-device:pixel\n"
            ),
            // Unknown flag or extra positional argument.
            format!(
                "restrict,command=\"/home/me/legion gate --as phone --device pixel --unknown\" {key} legion-device:pixel\n"
            ),
            format!(
                "restrict,command=\"/home/me/legion gate --as phone --device pixel extra\" {key} legion-device:pixel\n"
            ),
            // Duplicate `command=` option.
            format!(
                "restrict,command=\"/bin/sh\",command=\"/home/me/legion gate --as phone --device pixel\" {key} legion-device:pixel\n"
            ),
            // Wrong executable (not legion).
            format!(
                "restrict,command=\"/usr/bin/other gate --as phone --device pixel\" {key} legion-device:pixel\n"
            ),
            // Hand-written invite comment without restrict or with mismatched --enrol id.
            format!("{key} legion-invite:abc123\n"),
            format!("command=\"/home/me/legion gate --enrol abc123\" {key} legion-invite:abc123\n"),
            format!(
                "restrict,command=\"/home/me/legion gate --enrol other\" {key} legion-invite:abc123\n"
            ),
            // Device comment paired with --enrol command, or invite comment paired with --as command.
            format!(
                "restrict,command=\"/home/me/legion gate --enrol pixel\" {key} legion-device:pixel\n"
            ),
            format!(
                "restrict,command=\"/home/me/legion gate --as phone --device abc123\" {key} legion-invite:abc123\n"
            ),
        ];

        for raw in cases {
            let parsed = AuthKeysFile::parse(raw.as_bytes());
            assert_eq!(parsed.lines.len(), 1);
            assert_eq!(
                parsed.lines[0].kind,
                LineKind::Foreign,
                "line must be classified as Foreign: {raw}"
            );
        }

        // Exact valid device and invite lines ARE classified properly.
        let valid_device = format_device_line(
            Path::new("/home/me/.local/bin/legion"),
            "phone",
            key,
            "pixel",
            &PinnedHome::Root(PathBuf::from("/home/me/legion")),
            None,
            ForcedCommandShell::Posix,
        )
        .unwrap();
        let parsed_dev = AuthKeysFile::parse(format!("{valid_device}\n").as_bytes());
        assert!(matches!(
            &parsed_dev.lines[0].kind,
            LineKind::Device { name, .. } if name == "pixel"
        ));
    }

    #[test]
    fn verify_device_authorized_requires_matching_device_principal_home_and_account() {
        let dir = tempfile::tempdir().unwrap();
        let home_a = dir.path().join("legion-a");
        let home_b = dir.path().join("legion-b");
        let paths_a = Paths::from_explicit_home(&home_a).unwrap();
        let paths_b = Paths::from_explicit_home(&home_b).unwrap();
        let auth_path = dir.path().join("authorized_keys");

        let mut node_file = NodeFile::default();
        node_file.devices.authorized_keys = Some(auth_path.display().to_string());

        let key =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI0000000000000000000000000000000000000000000";
        let line_a = format_device_line(
            Path::new("/usr/local/bin/legion"),
            "phone",
            key,
            "pixel",
            &PinnedHome::from_paths(&paths_a),
            None,
            ForcedCommandShell::Posix,
        )
        .unwrap();
        std::fs::write(&auth_path, format!("{line_a}\n")).unwrap();

        // Exact match succeeds.
        assert!(verify_device_authorized(&paths_a, &node_file, "pixel", "phone", None).is_ok());

        // Mismatched device name, principal, home, or account all fail with Denied.
        assert_eq!(
            verify_device_authorized(&paths_a, &node_file, "other", "phone", None)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            verify_device_authorized(&paths_a, &node_file, "pixel", "owner", None)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            verify_device_authorized(&paths_b, &node_file, "pixel", "phone", None)
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );
        assert_eq!(
            verify_device_authorized(&paths_a, &node_file, "pixel", "phone", Some("alice"))
                .unwrap_err()
                .code,
            ErrorCode::Denied
        );

        // Removing the line immediately causes refusal.
        std::fs::write(&auth_path, "# empty\n").unwrap();
        let err =
            verify_device_authorized(&paths_a, &node_file, "pixel", "phone", None).unwrap_err();
        assert_eq!(err.code, ErrorCode::Denied);
        assert!(
            err.message.contains("no longer authorized"),
            "{}",
            err.message
        );
    }
}
