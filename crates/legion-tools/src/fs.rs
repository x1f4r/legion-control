//! `fs`: inspect, read, write, edit, organize and search files on this node.
//!
//! Every path argument goes through [`Grant::check_path`](legion_core::Grant::check_path) before
//! the file system is touched, and the resolved path it returns is the one used. Writes go through
//! `legion-os::atomic`, and long walks or reads run on Tokio's blocking pool while watching
//! cancellation and emitting `progress` events.

mod audio_tags;
mod manage;
mod platform;
mod read;
mod search;
mod tags;
mod thumb;
mod thumb_cache;
mod thumb_limit;
mod thumb_probe;
mod thumb_worker;
mod write;

pub use thumb_worker::{
    THUMB_WORKER_MODE, enable_thumb_worker_self, measured_worker_baseline_bytes_for_test,
    peak_worker_rss_for_test, reset_peak_worker_rss_for_test, set_thumb_decode_timeout_for_test,
    set_thumb_timeout_cpu_busy_for_test, try_thumb_worker_cli,
};

use std::cmp::Ordering;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use legion_core::proto::{ErrorBody, ErrorCode, EventKind};
use legion_core::{Access, CancellationToken, Ctx, Grant, Place, Registry, ToolError};
use legion_os::OsError;
use legion_os::lock::FileLock;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How often long-running filesystem operations emit a `progress` event.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Soft-to-hard JSON reply byte budget (1 MiB) shared by listing, reading and search tools.
pub(crate) const MAX_REPLY_BYTES: usize = 1024 * 1024;

/// Adds the `fs` tools.
pub fn register(registry: &mut Registry) {
    read::register(registry);
    write::register(registry);
    manage::register(registry);
    search::register(registry);
    tags::register(registry);
    thumb::register(registry);
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

/// Field used to order directory listings and search matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    /// Natural, case-insensitive comparison of the file name (`file2` before `file10`).
    #[default]
    Name,
    /// Last modification timestamp.
    Modified,
    /// File size in bytes.
    Size,
}

/// Sort direction for directory listings and search matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// Smallest / earliest / A-Z first.
    #[default]
    Asc,
    /// Largest / newest / Z-A first.
    Desc,
}

/// Compares two names in natural order (`file2` before `file10`), case-insensitively across
/// Unicode, with a deterministic raw-string tie-breaker so the result is a strict total order.
pub(crate) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a;
    let mut bi = b;

    loop {
        match (next_natural_chunk(ai), next_natural_chunk(bi)) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((ca, rest_a)), Some((cb, rest_b))) => {
                let ord = compare_natural_chunks(ca, cb);
                if ord != Ordering::Equal {
                    return ord;
                }
                ai = rest_a;
                bi = rest_b;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum NaturalChunk<'a> {
    Number(&'a str),
    Text(&'a str),
}

fn next_natural_chunk(input: &str) -> Option<(NaturalChunk<'_>, &str)> {
    let first = input.as_bytes().first()?;
    if first.is_ascii_digit() {
        let end = input
            .bytes()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(input.len());
        Some((NaturalChunk::Number(&input[..end]), &input[end..]))
    } else {
        let end = input
            .bytes()
            .position(|b| b.is_ascii_digit())
            .unwrap_or(input.len());
        Some((NaturalChunk::Text(&input[..end]), &input[end..]))
    }
}

fn compare_natural_chunks(a: NaturalChunk<'_>, b: NaturalChunk<'_>) -> Ordering {
    match (a, b) {
        (NaturalChunk::Number(na), NaturalChunk::Number(nb)) => {
            let sa = na.trim_start_matches('0');
            let sb = nb.trim_start_matches('0');
            sa.len().cmp(&sb.len()).then_with(|| sa.cmp(sb))
        }
        (NaturalChunk::Text(ta), NaturalChunk::Text(tb)) => compare_text_case_insensitive(ta, tb),
        // Every ASCII digit lies in the contiguous U+0030..=U+0039 block with no non-digits
        // inside, so comparing any digit against any non-digit character yields a transitive order
        // matching standard code-point ordering (`.` / `-` < digits < letters / `_`).
        (NaturalChunk::Number(_), NaturalChunk::Text(tb)) => {
            let Some(ch) = tb.chars().next() else {
                return Ordering::Greater;
            };
            let lower = ch.to_lowercase().next().unwrap_or(ch);
            '0'.cmp(&lower)
        }
        (NaturalChunk::Text(ta), NaturalChunk::Number(_)) => {
            let Some(ch) = ta.chars().next() else {
                return Ordering::Less;
            };
            let lower = ch.to_lowercase().next().unwrap_or(ch);
            lower.cmp(&'0')
        }
    }
}

fn compare_text_case_insensitive(a: &str, b: &str) -> Ordering {
    let mut ca = a.chars().flat_map(char::to_lowercase);
    let mut cb = b.chars().flat_map(char::to_lowercase);
    loop {
        match (ca.next(), cb.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = x.cmp(&y);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

/// Compares two entries by `sort` and `order`, breaking ties in `modified` or `size` by natural
/// ascending name so ordering is always deterministic.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compare_by_sort(
    sort: SortField,
    order: SortOrder,
    a_name: &str,
    b_name: &str,
    a_mtime: Option<SystemTime>,
    b_mtime: Option<SystemTime>,
    a_size: u64,
    b_size: u64,
) -> Ordering {
    let primary = match sort {
        SortField::Name => natural_cmp(a_name, b_name),
        SortField::Modified => a_mtime.cmp(&b_mtime),
        SortField::Size => a_size.cmp(&b_size),
    };
    let ordered = match order {
        SortOrder::Asc => primary,
        SortOrder::Desc => primary.reverse(),
    };
    ordered.then_with(|| natural_cmp(a_name, b_name))
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

    #[cfg(test)]
    pub(crate) fn for_test(tx: tokio::sync::mpsc::UnboundedSender<Value>) -> Progress {
        Progress::new(tx)
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
///
/// Under the node's cancellation contract (`CANCEL_GRACE`), this deliberately waits for the
/// blocking worker to observe `cancel`, clean up and return before the tool call finishes, so no
/// worker thread keeps modifying files after `cancelled` is reported.
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

/// Acquires the node-wide file-write lock in `state_dir` while watching `cancel`.
///
/// Holding this lock from reading the current file until after its atomic replacement prevents two
/// Legion `fs.write` / `fs.edit` calls on this node from interleaving across an `expect_hash`
/// check or read-modify-write cycle. It serializes Legion calls only and does not guard against
/// other programs outside Legion modifying the same file concurrently.
pub(crate) fn acquire_write_lock(
    state_dir: &Path,
    cancel: &CancellationToken,
    tool: &str,
) -> Result<FileLock, ToolError> {
    if let Err(error) = std::fs::create_dir_all(state_dir) {
        return Err(map_io_error("prepare state directory", state_dir, error));
    }
    let lock_path = state_dir.join("fs-write.lock");
    let mut pause = Duration::from_millis(5);
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        match FileLock::try_lock_exclusive(&lock_path) {
            Ok(guard) => return Ok(guard),
            Err(OsError::WouldBlock { .. }) => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(25));
            }
            Err(error) => return Err(map_os_error("lock", &lock_path, error)),
        }
    }
}

/// Maps a [`legion_os::OsError`] on `path` into the nearest closed [`ErrorCode`].
pub(crate) fn map_os_error(action: &str, path: &Path, error: OsError) -> ErrorBody {
    match error {
        OsError::NotFound { detail } => ErrorBody::new(
            ErrorCode::NotFound,
            format!("{} does not exist ({detail}).", path.display()),
        ),
        OsError::AlreadyExists { path: existing } => ErrorBody::new(
            ErrorCode::Exists,
            format!("{} already exists.", existing.display()),
        )
        .with_hint("Use mode \"overwrite\" or \"append\" to modify an existing file."),
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

/// Refuses whole-tree mutation (remove, move or directory replacement) on any protected place
/// reported by [`Grant::protected`]: a file-system root, the home directory, Legion's own
/// configuration or state directories, or any ancestor holding one of them.
pub fn check_not_protected_tree(grant: &Grant, path: &Path) -> Result<(), ToolError> {
    let Some(place) = grant.protected(path)? else {
        return Ok(());
    };
    let (what, hint) = match place {
        Place::Root => (
            "the root of a file system",
            "Name a specific file or subdirectory instead.",
        ),
        Place::Home => (
            "the user's home directory",
            "Name a specific file or subdirectory inside the home directory instead.",
        ),
        Place::Config => (
            "Legion's configuration directory",
            "Legion's configuration directory cannot be removed, moved or replaced as a whole.",
        ),
        Place::State => (
            "Legion's state directory",
            "Legion's state directory holds the audit log and runtime state and cannot be removed, moved or replaced as a whole.",
        ),
        Place::Secret => (
            "a secret location",
            "Only a role with effect:admin, or one whose `paths` names the secret location explicitly, may remove, move or replace it.",
        ),
    };
    Err(ErrorBody::new(
        ErrorCode::Denied,
        format!(
            "Refusing to remove, move or replace {}: it is or contains {what}.",
            path.display()
        ),
    )
    .with_hint(hint)
    .with_details(json!({"path": path, "place": format!("{place:?}").to_lowercase()})))
}

/// Identifies Legion's own configuration/state directories and well-known secret locations during
/// recursive directory walks, backed directly by [`legion_core::ProtectedFilter`].
#[derive(Debug)]
pub(crate) struct OwnDirFilter {
    inner: legion_core::ProtectedFilter,
}

impl OwnDirFilter {
    /// Builds a filter for `ctx`: empty for roles with `effect:admin`, and matching Legion's
    /// configuration/state directories plus secret locations not explicitly opted into via `paths`.
    pub(crate) fn for_ctx(ctx: &Ctx) -> OwnDirFilter {
        OwnDirFilter {
            inner: ctx.grant().protected_filter(),
        }
    }

    /// Whether `path` refers to Legion's own directories or a refused secret location.
    pub(crate) fn is_blocked(&self, path: &Path) -> bool {
        self.inner.is_blocked(path)
    }

    #[cfg(test)]
    pub(crate) fn allow_all_for_test() -> OwnDirFilter {
        OwnDirFilter {
            inner: legion_core::ProtectedFilter::allow_all(),
        }
    }
}

/// Whether `a` and `b` refer to the same path, accounting for platform filename casing.
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
pub(crate) fn is_strict_ancestor(ancestor: &Path, descendant: &Path) -> bool {
    is_same_or_descendant(descendant, ancestor) && !paths_equal(ancestor, descendant)
}

/// Validates `from` and `to` for a move or rename with the exact same platform-path, `paths`
/// confinement, and protected-tree checks enforced by `fs.move`.
pub fn check_move_paths(
    grant: &Grant,
    from: &str,
    to: &str,
) -> Result<(PathBuf, PathBuf), ToolError> {
    let from = check_fs_path(grant, from, Access::Write)?;
    let to = check_fs_path(grant, to, Access::Write)?;
    check_not_protected_tree(grant, &from)?;
    check_not_protected_tree(grant, &to)?;
    Ok((from, to))
}

/// Validates platform path rules (on Windows: reserved device names and alternate data streams)
/// before running [`Grant::check_path`].
pub(crate) fn check_fs_path(
    grant: &Grant,
    raw: &str,
    access: Access,
) -> Result<PathBuf, ToolError> {
    if platform::IS_WINDOWS {
        validate_windows_path(raw)?;
    }
    let resolved = grant.check_path(raw, access)?;
    if platform::IS_WINDOWS {
        validate_windows_path(&path_to_string(&resolved))?;
    }
    Ok(resolved)
}

/// Opens `path` for reading without ever blocking on a FIFO or acquiring a controlling terminal,
/// verifying on the open handle itself that it is a regular file.
pub(crate) fn open_regular_file(
    path: &Path,
    action: &str,
    dir_hint: &str,
) -> Result<(std::fs::File, Metadata), ToolError> {
    legion_os::open_regular::open_regular_read(path).map_err(|err| match err {
        legion_os::open_regular::OpenRegularError::IsDirectory => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{} is a directory, not a file.", path.display()),
        )
        .with_hint(dir_hint),
        legion_os::open_regular::OpenRegularError::NotRegularFile => ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} is not a regular file (it is a named pipe, socket or device).",
                path.display()
            ),
        )
        .with_hint("Pass a regular file path; special files are refused without blocking."),
        legion_os::open_regular::OpenRegularError::Io(error) => map_io_error(action, path, error),
    })
}

/// Confirms `path` exists and refers to a regular file via `stat` before opening it, so named
/// pipes, sockets and device nodes are refused immediately instead of blocking on `File::open`.
pub(crate) fn check_regular_file(
    path: &Path,
    action: &str,
    dir_hint: &str,
) -> Result<Metadata, ToolError> {
    let meta = fs::metadata(path).map_err(|error| map_io_error(action, path, error))?;
    if meta.is_dir() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{} is a directory, not a file.", path.display()),
        )
        .with_hint(dir_hint));
    }
    if !meta.is_file() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "{} is not a regular file (it is a named pipe, socket or device).",
                path.display()
            ),
        )
        .with_hint("Pass a regular file path; special files are refused before opening so calls cannot block."));
    }
    Ok(meta)
}

/// Pure validator for Windows path hazards, tested on every platform:
/// - refuses `:` anywhere in a path component after the leading drive/UNC prefix (NTFS alternate
///   data streams), and
/// - refuses reserved Win32 device names (`CON`, `PRN`, `AUX`, `NUL`, `CONIN$`, `CONOUT$`,
///   `COM1`..`COM9`, `COM¹`..`COM³`, `LPT1`..`LPT9`, `LPT¹`..`LPT³`) in any path component,
///   case-insensitively and ignoring extensions or trailing dots/spaces.
pub(crate) fn validate_windows_path(raw: &str) -> Result<(), ErrorBody> {
    let remainder = strip_windows_prefix(raw)?;
    for component in remainder.split(['\\', '/']) {
        if component.is_empty() {
            continue;
        }
        if component.contains(':') {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{raw:?} contains ':' in path component {component:?}, which names a Windows alternate data stream."
                ),
            )
            .with_hint(
                "Name the file itself without an alternate data stream suffix; on Windows ':' is only allowed in a leading drive letter.",
            ));
        }
        if is_windows_reserved_device_name(component) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{raw:?} uses the reserved Windows device name {component:?}."),
            )
            .with_hint(
                "Avoid reserved Windows device names (CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM1-COM9, LPT1-LPT9) in path components.",
            ));
        }
    }
    Ok(())
}

fn is_win_sep(b: u8) -> bool {
    b == b'\\' || b == b'/'
}

/// Strips a valid Windows volume or UNC prefix (`\\?\UNC\server\share`, `\\?\C:`, `\\.\C:`,
/// `\\server\share`, or `C:`) and returns the remaining path components to validate.
fn strip_windows_prefix(raw: &str) -> Result<&str, ErrorBody> {
    let bytes = raw.as_bytes();
    // Verbatim or Win32 device prefix: `\\?\...`, `\\.\...` (with either separator).
    if bytes.len() >= 4
        && is_win_sep(bytes[0])
        && is_win_sep(bytes[1])
        && (bytes[2] == b'?' || bytes[2] == b'.')
        && is_win_sep(bytes[3])
    {
        let after = &raw[4..];
        let after_bytes = after.as_bytes();
        if after_bytes.len() >= 4
            && after_bytes[..3].eq_ignore_ascii_case(b"UNC")
            && is_win_sep(after_bytes[3])
        {
            return strip_unc_host_and_share(raw, &after[4..]);
        }
        if after_bytes.len() >= 2 && after_bytes[0].is_ascii_alphabetic() && after_bytes[1] == b':'
        {
            return Ok(&after[2..]);
        }
        return Ok(after);
    }

    // Standard UNC prefix: `\\server\share\...`.
    if bytes.len() >= 2 && is_win_sep(bytes[0]) && is_win_sep(bytes[1]) {
        return strip_unc_host_and_share(raw, &raw[2..]);
    }

    // Drive letter prefix: `C:\...` or `C:...`.
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Ok(&raw[2..]);
    }

    Ok(raw)
}

fn strip_unc_host_and_share<'a>(raw: &str, after_prefix: &'a str) -> Result<&'a str, ErrorBody> {
    let mut parts = after_prefix.splitn(3, ['\\', '/']);
    let server = parts.next().unwrap_or("");
    let share = parts.next().unwrap_or("");
    for part in [server, share] {
        if part.contains(':') {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{raw:?} contains ':' in UNC component {part:?}, which is not allowed on Windows."
                ),
            )
            .with_hint("Name a plain Windows path or UNC share without ':' after the volume."));
        }
    }
    Ok(parts.next().unwrap_or(""))
}

fn is_windows_reserved_device_name(component: &str) -> bool {
    let trimmed = component.trim_end_matches([' ', '.']);
    let stem = trimmed
        .split_once('.')
        .map_or(trimmed, |(head, _)| head)
        .trim_end_matches(' ');
    if stem.is_empty() {
        return false;
    }
    if stem.eq_ignore_ascii_case("CON")
        || stem.eq_ignore_ascii_case("PRN")
        || stem.eq_ignore_ascii_case("AUX")
        || stem.eq_ignore_ascii_case("NUL")
        || stem.eq_ignore_ascii_case("CONIN$")
        || stem.eq_ignore_ascii_case("CONOUT$")
    {
        return true;
    }
    let rest = stem
        .get(..3)
        .filter(|prefix| prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
        .and_then(|_| stem.get(3..));
    if let Some(suffix) = rest {
        let mut chars = suffix.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            return matches!(ch, '1'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}');
        }
    }
    false
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
    fn path_relationship_helpers_handle_ancestors() {
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

    #[test]
    fn windows_path_validator_refuses_reserved_devices_and_ads_colons() {
        // Reserved Win32 device names across casing, extensions, trailing dots/spaces,
        // nested directories, UNC/device prefixes and superscript ports.
        let reserved = [
            r"C:\Users\me\CON",
            r"C:\Users\me\con.txt",
            r"C:\Users\me\Con.tar.gz",
            r"C:\Users\me\PRN .log",
            r"C:\Users\me\aux...",
            r"C:\Users\me\NUL\child.txt",
            r"C:\Users\me\NUL   ",
            r"C:\Users\me\CONIN$",
            r"C:\Users\me\conout$.txt",
            r"C:\Users\me\COM1",
            r"C:\Users\me\com9.dat",
            r"C:\Users\me\COM²",
            r"C:\Users\me\LPT1",
            r"C:\Users\me\lpt9.prn",
            r"C:\Users\me\lpt³.txt",
            r"~/projects/NUL.json",
            r"\\.\COM1",
            r"\\?\NUL",
            r"\\?\C:\Users\me\aux.txt",
            r"\\tower\share\dir\prn",
            r"\\?\UNC\tower\share\com4.bin",
        ];
        for path in reserved {
            let err = validate_windows_path(path).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{path}: {err}");
            assert!(err.hint.is_some(), "{path}");
        }

        // Alternate data streams (any `:` after the leading drive/UNC prefix).
        let ads = [
            r"C:\Users\me\notes.txt:hidden",
            r"C:\Users\me\notes.txt::$DATA",
            r"C:\Users\me\dir:stream\file.txt",
            r"C::\Users\me\file.txt",
            r"~/projects/file.txt:ads",
            r"\\?\C:\Users\me\key:secret",
            r"\\tower\share\key:secret",
            r"\\tower:80\share\file.txt",
            r"\\?\UNC\tower\share\file:stream",
        ];
        for path in ads {
            let err = validate_windows_path(path).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgs, "{path}: {err}");
            assert!(err.hint.is_some(), "{path}");
        }

        // Valid Windows paths and non-reserved lookalike names are accepted.
        let allowed = [
            r"C:\Users\me\projects\config.toml",
            r"C:/Users/me/projects/console.txt",
            r"C:\Users\me\null.rs",
            r"C:\Users\me\auxiliary.json",
            r"C:\Users\me\com0.txt",
            r"C:\Users\me\com10.txt",
            r"C:\Users\me\lpt0.txt",
            r"C:\Users\me\lpt10.txt",
            r"C:\Users\me\.nul",
            r"C:\Users\me\file.nul",
            r"~/projects/notes.txt",
            r"\\?\C:\Users\me\ok.txt",
            r"\\tower\share\projects\ok.txt",
            r"\\?\UNC\tower\share\ok.txt",
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\ok.txt",
        ];
        for path in allowed {
            assert!(validate_windows_path(path).is_ok(), "{path} was rejected");
        }
    }

    #[test]
    fn natural_cmp_orders_numbers_case_and_unicode_transitively() {
        let mut names = vec![
            "file10.txt",
            "file2.txt",
            "file1.txt",
            "File20.txt",
            "file02.txt",
            "B.txt",
            "a.txt",
            ".hidden",
            "über10.txt",
            "Über2.txt",
            "big999999999999999999999999999999.txt",
            "big1000000000000000000000000000000.txt",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec![
                ".hidden",
                "a.txt",
                "B.txt",
                "big999999999999999999999999999999.txt",
                "big1000000000000000000000000000000.txt",
                "file1.txt",
                "file02.txt",
                "file2.txt",
                "file10.txt",
                "File20.txt",
                "Über2.txt",
                "über10.txt",
            ]
        );

        // Verify strict weak ordering (antisymmetry and transitivity) across all pairs/triples.
        for &x in &names {
            assert_eq!(natural_cmp(x, x), Ordering::Equal);
            for &y in &names {
                assert_eq!(natural_cmp(x, y), natural_cmp(y, x).reverse());
                for &z in &names {
                    if natural_cmp(x, y) == Ordering::Less && natural_cmp(y, z) == Ordering::Less {
                        assert_eq!(natural_cmp(x, z), Ordering::Less, "{x} < {y} < {z}");
                    }
                }
            }
        }
    }
}
