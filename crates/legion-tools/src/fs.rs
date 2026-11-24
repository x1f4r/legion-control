//! `fs`: inspect, read, write, edit, organize and search files on this node.
//!
//! Every path argument goes through [`Grant::check_path`](legion_core::Grant::check_path) before
//! the file system is touched, and the resolved path it returns is the one used. Writes go through
//! `legion-os::atomic`, and long walks or reads run on Tokio's blocking pool while watching
//! cancellation and emitting `progress` events.

mod platform;
mod read;

use std::ffi::OsString;
use std::fs::{FileType, Metadata};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use legion_core::proto::{ErrorBody, ErrorCode, EventKind};
use legion_core::{CancellationToken, Ctx, Registry, ToolError};
use legion_os::OsError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How often long-running filesystem operations emit a `progress` event.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Adds the `fs` tools.
pub fn register(registry: &mut Registry) {
    read::register(registry);
}

/// The kind of a filesystem entry, reported without following symbolic links.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// Something else, such as a named pipe, socket or device node.
    Other,
}

impl EntryKind {
    pub(crate) fn from_file_type(file_type: FileType) -> EntryKind {
        if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        }
    }
}

/// How file bytes are represented in JSON arguments and results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    /// UTF-8 text.
    Utf8,
    /// Standard base64-encoded bytes.
    Base64,
}

/// Sends periodic `progress` events from a blocking worker back to the calling [`Ctx`].
pub(crate) struct Progress {
    tx: tokio::sync::mpsc::UnboundedSender<Value>,
    last: Instant,
    interval: Duration,
}

impl Progress {
    fn new(tx: tokio::sync::mpsc::UnboundedSender<Value>) -> Progress {
        Progress {
            tx,
            last: Instant::now(),
            interval: PROGRESS_INTERVAL,
        }
    }

    /// Emits a progress payload when enough time has passed since the previous one.
    pub(crate) fn maybe_emit(&mut self, build: impl FnOnce() -> Value) {
        if self.last.elapsed() >= self.interval {
            self.last = Instant::now();
            let _ = self.tx.send(build());
        }
    }
}

/// Runs a blocking filesystem operation off the async runtime while forwarding its `progress`
/// events live to `ctx` and passing `ctx`'s cancellation token.
pub(crate) async fn run_blocking<T, F>(
    ctx: &Ctx,
    tool: &'static str,
    work: F,
) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce(CancellationToken, Progress) -> Result<T, ToolError> + Send + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cancel = ctx.cancel_token().clone();
    let handle = tokio::task::spawn_blocking(move || work(cancel, Progress::new(tx)));
    while let Some(data) = rx.recv().await {
        ctx.emit(EventKind::Progress, data);
    }
    handle.await.map_err(|error| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("{tool} stopped unexpectedly: {error}."),
        )
    })?
}

/// Renders a path as text for JSON fields.
pub(crate) fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Formats a metadata modification time as RFC 3339 UTC to the millisecond.
pub(crate) fn format_mtime(meta: &Metadata) -> Option<String> {
    let modified = meta.modified().ok()?;
    let timestamp = jiff::Timestamp::try_from(modified).ok()?;
    Some(timestamp.strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
}

/// Formats a BLAKE3 digest in Legion's wire spelling (`blake3:<hex>`).
pub(crate) fn format_blake3(hash: blake3::Hash) -> String {
    format!("blake3:{}", hash.to_hex())
}

/// Whether an entry counts as hidden: a leading dot on every platform, plus the OS hidden
/// attribute on Windows.
pub(crate) fn is_hidden(name: &str, meta: &Metadata) -> bool {
    name.starts_with('.') || platform::is_hidden_attr(meta)
}

/// Parses an octal Unix permission string such as `"0600"`, `"644"` or `"0o755"`.
#[allow(dead_code)]
pub(crate) fn parse_unix_mode(text: &str) -> Result<u32, ErrorBody> {
    let trimmed = text.trim();
    let digits = trimmed
        .strip_prefix("0o")
        .or_else(|| trimmed.strip_prefix("0O"))
        .unwrap_or(trimmed);
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| (b'0'..=b'7').contains(&b))
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text:?} is not a Unix permission mode."),
        )
        .with_hint("Write an octal string of up to four digits, such as \"0600\" or \"0644\"."));
    }
    u32::from_str_radix(digits, 8).map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{text:?} is not a Unix permission mode."),
        )
        .with_hint("Write an octal string of up to four digits, such as \"0600\" or \"0644\".")
    })
}

/// The error returned when a tool notices its call was cancelled.
pub(crate) fn cancelled_error(tool: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Cancelled,
        format!("The call to {tool} was cancelled before it finished."),
    )
}

/// Maps an I/O error on `path` into the nearest closed [`ErrorCode`].
pub(crate) fn map_io_error(action: &str, path: &Path, error: io::Error) -> ErrorBody {
    match error.kind() {
        io::ErrorKind::NotFound => ErrorBody::new(
            ErrorCode::NotFound,
            format!("{} does not exist.", path.display()),
        ),
        io::ErrorKind::AlreadyExists => ErrorBody::new(
            ErrorCode::Exists,
            format!("{} already exists.", path.display()),
        ),
        io::ErrorKind::PermissionDenied => ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "Cannot {action} {}: permission denied ({error}).",
                path.display()
            ),
        )
        .with_hint("Check the file permissions for the account legion runs as."),
        io::ErrorKind::DirectoryNotEmpty => ErrorBody::new(
            ErrorCode::Conflict,
            format!("{} is not empty.", path.display()),
        ),
        io::ErrorKind::Unsupported => ErrorBody::new(
            ErrorCode::Unsupported,
            format!(
                "Cannot {action} {} on this platform: {error}.",
                path.display()
            ),
        ),
        _ => ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot {action} {}: {error}.", path.display()),
        ),
    }
}

/// Maps a [`legion_os::OsError`] on `path` into the nearest closed [`ErrorCode`].
#[allow(dead_code)]
pub(crate) fn map_os_error(action: &str, path: &Path, error: OsError) -> ErrorBody {
    match error {
        OsError::NotFound { detail } => ErrorBody::new(
            ErrorCode::NotFound,
            format!("{} does not exist ({detail}).", path.display()),
        ),
        OsError::PermissionDenied { detail, .. } => ErrorBody::new(
            ErrorCode::Denied,
            format!("Cannot {action} {}: {detail}.", path.display()),
        )
        .with_hint("Check the file permissions for the account legion runs as."),
        OsError::InvalidArgument { detail } => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Cannot {action} {}: {detail}.", path.display()),
        ),
        OsError::TimedOut { detail } => ErrorBody::new(
            ErrorCode::Timeout,
            format!("Timed out trying to {action} {}: {detail}.", path.display()),
        ),
        OsError::Unsupported { detail } => ErrorBody::new(
            ErrorCode::Unsupported,
            format!("Cannot {action} {}: {detail}.", path.display()),
        ),
        OsError::Io { source, .. } => map_io_error(action, path, source),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot {action} {}: {other}.", path.display()),
        ),
    }
}

/// Resolves every existing prefix of `path` (following symlinks and folding `..`) and appends any
/// trailing non-existent components, so paths can be compared even before a directory exists.
#[allow(dead_code)]
pub(crate) fn resolve_best_effort(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match dunce::canonicalize(&existing) {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return resolved;
            }
            Err(_) => {
                let Some(name) = existing.file_name().map(OsString::from) else {
                    return path.to_path_buf();
                };
                missing.push(name);
                if !existing.pop() {
                    return path.to_path_buf();
                }
            }
        }
    }
}

/// Whether `path` names a filesystem root (such as `/` on Unix or `C:\` on Windows).
#[allow(dead_code)]
pub(crate) fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
        || path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::CurDir
            )
        })
}

/// Whether `a` and `b` refer to the same path, accounting for platform filename casing.
#[allow(dead_code)]
pub(crate) fn paths_equal(a: &Path, b: &Path) -> bool {
    let mut ca = a.components();
    let mut cb = b.components();
    loop {
        match (ca.next(), cb.next()) {
            (None, None) => return true,
            (Some(pa), Some(pb)) => {
                if !platform::path_component_eq(pa.as_os_str(), pb.as_os_str()) {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

/// Whether `candidate` is equal to `base` or lives anywhere beneath `base`.
#[allow(dead_code)]
pub(crate) fn is_same_or_descendant(candidate: &Path, base: &Path) -> bool {
    let mut cc = candidate.components();
    for bc in base.components() {
        let Some(part) = cc.next() else {
            return false;
        };
        if !platform::path_component_eq(part.as_os_str(), bc.as_os_str()) {
            return false;
        }
    }
    true
}

/// Whether `ancestor` is a strict ancestor of `descendant` (not equal to it).
#[allow(dead_code)]
pub(crate) fn is_strict_ancestor(ancestor: &Path, descendant: &Path) -> bool {
    is_same_or_descendant(descendant, ancestor) && !paths_equal(ancestor, descendant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_mode_parses_octal_strings_and_refuses_bad_ones() {
        assert_eq!(parse_unix_mode("0600").unwrap(), 0o600);
        assert_eq!(parse_unix_mode("644").unwrap(), 0o644);
        assert_eq!(parse_unix_mode("0o755").unwrap(), 0o755);
        assert_eq!(parse_unix_mode("1777").unwrap(), 0o1777);
        assert_eq!(parse_unix_mode("0").unwrap(), 0);
        for bad in ["", "0800", "999", "077777", "rwxr-xr-x", "-1", "0x644"] {
            assert_eq!(
                parse_unix_mode(bad).unwrap_err().code,
                ErrorCode::InvalidArgs,
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn path_relationship_helpers_handle_roots_and_ancestors() {
        let root = Path::new("/");
        assert!(is_filesystem_root(root));
        assert!(!is_filesystem_root(Path::new("/home/me")));
        assert!(paths_equal(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!paths_equal(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(is_same_or_descendant(
            Path::new("/a/b/c"),
            Path::new("/a/b")
        ));
        assert!(is_same_or_descendant(Path::new("/a/b"), Path::new("/a/b")));
        assert!(!is_same_or_descendant(
            Path::new("/a/bc"),
            Path::new("/a/b")
        ));
        assert!(is_strict_ancestor(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(!is_strict_ancestor(Path::new("/a/b"), Path::new("/a/b")));
    }
}
