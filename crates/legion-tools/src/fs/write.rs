//! File creation and modification tools: `fs.write`, `fs.edit` and `fs.mkdir`.

use std::borrow::Cow;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, CreateKind, Ctx, Grant, Registry, ToolError};
use legion_os::atomic::{WriteOptions, write_atomic_from_reader};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Encoding, acquire_write_lock, cancelled_error, check_fs_path, check_regular_file,
    format_blake3, map_io_error, map_os_error, parse_unix_mode, path_to_string, platform,
    run_blocking, validate_windows_path,
};

const HASH_CHUNK_BYTES: usize = 64 * 1024;
/// Maximum byte length of inline `content` accepted by `fs.write` (8 MiB).
const MAX_WRITE_CONTENT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum file size `fs.edit` will read into memory (16 MiB).
const MAX_EDIT_FILE_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.write",
        Effect::Write,
        "Write UTF-8 text or base64 bytes to a file atomically, supporting overwrite, create-only and append modes, optional expect_hash conflict checks, parent creation and Unix mode.",
        write,
    );
    registry.add_fn(
        "fs.edit",
        Effect::Write,
        "Replace an exact string in a UTF-8 text file atomically while preserving line endings and permissions, with optional expect_hash conflict checking.",
        edit,
    );
    registry.add_fn(
        "fs.mkdir",
        Effect::Write,
        "Create a directory and, by default, any missing parent directories.",
        mkdir,
    );
}

/// How `fs.write` treats an existing file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WriteMode {
    /// Create the file or atomically replace its contents.
    #[default]
    Overwrite,
    /// Create a new file and fail with `exists` if anything is already at the path.
    Create,
    /// Append to an existing file in OS append mode or create it if missing.
    Append,
}

/// Arguments of `fs.write`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    /// File to write (absolute or starting with `~`).
    path: String,
    /// Content to write, as UTF-8 text or standard base64 according to `encoding`.
    content: String,
    /// Encoding of `content` (`utf8` or `base64`, default `utf8`).
    #[serde(default = "default_utf8")]
    encoding: Encoding,
    /// Write mode (`overwrite`, `create` or `append`, default `overwrite`).
    #[serde(default)]
    mode: WriteMode,
    /// Require the existing file's BLAKE3 hash (`blake3:<hex>`) to match before writing.
    #[serde(default)]
    expect_hash: Option<String>,
    /// Create missing parent directories before writing.
    #[serde(default)]
    parents: bool,
    /// Unix permissions as an octal string, such as `"0600"` or `"0644"`. Ignored on Windows.
    #[serde(default)]
    unix_mode: Option<String>,
}

fn default_utf8() -> Encoding {
    Encoding::Utf8
}

/// Result of `fs.write`.
#[derive(Debug, Serialize)]
struct WriteResult {
    path: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    created: bool,
}

async fn write(ctx: &Ctx, args: WriteArgs) -> Result<WriteResult, ToolError> {
    if args.mode == WriteMode::Create && args.expect_hash.is_some() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "fs.write cannot combine mode \"create\" with expect_hash.",
        )
        .with_hint(
            "Use mode \"overwrite\" or fs.edit with expect_hash, or omit expect_hash to create a new file.",
        ));
    }
    if args.mode == WriteMode::Append && args.expect_hash.is_some() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "fs.write cannot combine mode \"append\" with expect_hash.",
        )
        .with_hint(
            "Append uses the OS append flag so concurrent writers never lose data; use mode \"overwrite\" or fs.edit when you need expect_hash.",
        ));
    }
    if args.content.len() > MAX_WRITE_CONTENT_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.write accepts at most {MAX_WRITE_CONTENT_BYTES} bytes (8 MiB) in `content`, and {} were sent.",
                args.content.len()
            ),
        )
        .with_hint(
            "Stream larger files with fs.transfer instead of sending them inline in fs.write.",
        ));
    }
    let unix_mode = match &args.unix_mode {
        Some(text) => Some(parse_unix_mode(text)?),
        None => None,
    };
    let payload = match args.encoding {
        Encoding::Utf8 => args.content.into_bytes(),
        Encoding::Base64 => BASE64.decode(&args.content).map_err(|error| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("content is not valid base64: {error}."),
            )
            .with_hint("Provide standard base64, or use encoding \"utf8\".")
        })?,
    };

    let checked = check_fs_path(ctx.grant(), &args.path, Access::Write)?;
    ctx.grant()
        .check_create(&checked, CreateKind::File, args.parents)?;
    let grant = ctx.grant().clone();
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let mode = args.mode;
    let expect_hash = args.expect_hash;
    let parents = args.parents;

    run_blocking(ctx, "fs.write", move |cancel, _progress| {
        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.write"));
        }

        // Hold the node-wide file-write lock across read-check-replace for `overwrite` (and
        // `expect_hash`) so concurrent Legion calls cannot interleave. This guards against other
        // Legion calls on this node, not against unrelated programs.
        let _write_lock = if mode == WriteMode::Overwrite {
            Some(acquire_write_lock(&state_dir, &cancel, "fs.write")?)
        } else {
            None
        };

        let path = resolve_write_target(&grant, checked, mode == WriteMode::Create)?;

        if mode == WriteMode::Append {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && !parent.exists()
            {
                if parents {
                    fs::create_dir_all(parent).map_err(|error| {
                        map_io_error("create parent directories for", &path, error)
                    })?;
                } else {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("Parent directory {} does not exist.", parent.display()),
                    )
                    .with_hint(
                        "Pass parents: true to create missing parent directories, or call fs.mkdir first.",
                    ));
                }
            }
            let (mut file, created) = match platform::open_append_create_new(&path, unix_mode) {
                Ok(file) => (file, true),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    check_regular_file(&path, "append to", "Pass a file path to fs.write.")?;
                    let file = OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .map_err(|err| map_io_error("open for append", &path, err))?;
                    (file, false)
                }
                Err(error) => return Err(map_io_error("open for append", &path, error)),
            };
            file.write_all(&payload)
                .map_err(|error| map_io_error("append to", &path, error))?;
            file.flush()
                .map_err(|error| map_io_error("flush", &path, error))?;
            file.sync_all()
                .map_err(|error| map_io_error("sync", &path, error))?;
            if !created && let Some(bits) = unix_mode {
                platform::set_unix_mode(&path, bits)
                    .map_err(|error| map_io_error("set permissions on", &path, error))?;
            }
            let size = file.metadata().map_or(payload.len() as u64, |m| m.len());
            return Ok(WriteResult {
                path: path_to_string(&path),
                size,
                hash: None,
                created,
            });
        }

        let existed = match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.is_dir() {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!("{} is a directory, not a file.", path.display()),
                    )
                    .with_hint("Pass a file path to fs.write."));
                }
                if mode == WriteMode::Create {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("{} already exists.", path.display()),
                    )
                    .with_hint(
                        "Use mode \"overwrite\" or \"append\" to modify an existing file.",
                    ));
                }
                if !meta.is_file() {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        format!(
                            "{} is not a regular file (it is a named pipe, socket or device).",
                            path.display()
                        ),
                    )
                    .with_hint("Pass a regular file path to fs.write."));
                }
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(map_io_error("inspect", &path, error)),
        };

        if expect_hash.is_some() && !existed {
            return Err(ErrorBody::new(
                ErrorCode::Conflict,
                format!(
                    "{} does not exist, so it cannot match expect_hash.",
                    path.display()
                ),
            )
            .with_hint("Read the file again, or omit expect_hash to create a new file."));
        }

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && !parent.exists()
        {
            if parents {
                fs::create_dir_all(parent)
                    .map_err(|error| map_io_error("create parent directories for", &path, error))?;
            } else {
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("Parent directory {} does not exist.", parent.display()),
                )
                .with_hint(
                    "Pass parents: true to create missing parent directories, or call fs.mkdir first.",
                ));
            }
        }

        if let Some(expected) = expect_hash.as_deref() {
            let current = hash_file(&path, &cancel, "fs.write")?;
            check_expected_hash(&path, expected, &current)?;
        }

        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.write"));
        }

        let hash = hash_bytes_cancellable(&payload, &cancel, "fs.write")?;
        let size = payload.len() as u64;
        let options = WriteOptions {
            mode: unix_mode,
            force_mode: false,
            no_replace: mode == WriteMode::Create,
        };
        write_atomic_cancellable(&path, &payload, options, &cancel, "fs.write")?;
        // `legion-os` preserves the existing file's mode on overwrite; honor an explicit
        // `unix_mode` when the caller asked for one.
        if existed && let Some(bits) = unix_mode {
            platform::set_unix_mode(&path, bits)
                .map_err(|error| map_io_error("set permissions on", &path, error))?;
        }

        Ok(WriteResult {
            path: path_to_string(&path),
            size,
            hash: Some(hash),
            created: !existed,
        })
    })
    .await
}

/// Arguments of `fs.edit`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    /// UTF-8 text file to edit (absolute or starting with `~`).
    path: String,
    /// Exact text to find and replace.
    old: String,
    /// Replacement text.
    new: String,
    /// Replace every match instead of requiring a single unambiguous match.
    #[serde(default)]
    all: bool,
    /// Require the current file's BLAKE3 hash (`blake3:<hex>`) to match before editing.
    #[serde(default)]
    expect_hash: Option<String>,
}

/// Result of `fs.edit`.
#[derive(Debug, Serialize)]
struct EditResult {
    path: String,
    replacements: usize,
    hash: String,
}

async fn edit(ctx: &Ctx, args: EditArgs) -> Result<EditResult, ToolError> {
    if args.old.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "fs.edit requires a non-empty `old` string.",
        )
        .with_hint(
            "Provide the exact text to replace, or use fs.write to replace the whole file.",
        ));
    }
    let checked = check_fs_path(ctx.grant(), &args.path, Access::Write)?;
    ctx.grant()
        .check_create(&checked, CreateKind::File, false)?;
    let grant = ctx.grant().clone();
    let state_dir = ctx.paths().state_dir().to_path_buf();
    let old = args.old;
    let new = args.new;
    let all = args.all;
    let expect_hash = args.expect_hash;

    run_blocking(ctx, "fs.edit", move |cancel, _progress| {
        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.edit"));
        }
        // Serialize read-check-replace across concurrent Legion calls on this node (guards against
        // other Legion calls, not against external programs).
        let _write_lock = acquire_write_lock(&state_dir, &cancel, "fs.edit")?;
        let path = resolve_write_target(&grant, checked, false)?;
        let meta =
            check_regular_file(&path, "read", "Pass a file path to fs.edit.").map_err(|err| {
                if err.code == ErrorCode::NotFound {
                    err.with_hint("Check the path or create the file with fs.write.")
                } else {
                    err
                }
            })?;
        if meta.len() > MAX_EDIT_FILE_BYTES as u64 {
            return Err(file_too_large_for_edit(&path, Some(meta.len())));
        }

        let bytes = read_file_cancellable(&path, MAX_EDIT_FILE_BYTES, &cancel, "fs.edit")?;
        if let Some(expected) = expect_hash.as_deref() {
            let current = format_blake3(blake3::hash(&bytes));
            check_expected_hash(&path, expected, &current)?;
        }

        let text = String::from_utf8(bytes).map_err(|_| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("{} is not valid UTF-8 text.", path.display()),
            )
            .with_hint("fs.edit only edits UTF-8 text files; use fs.read and fs.write with base64 for binary files.")
        })?;

        let (updated, replacements) = apply_edit(&path, &text, &old, &new, all)?;
        if cancel.is_cancelled() {
            return Err(cancelled_error("fs.edit"));
        }

        let updated_bytes = updated.as_bytes();
        let hash = hash_bytes_cancellable(updated_bytes, &cancel, "fs.edit")?;
        write_atomic_cancellable(
            &path,
            updated_bytes,
            WriteOptions::default(),
            &cancel,
            "fs.edit",
        )?;

        Ok(EditResult {
            path: path_to_string(&path),
            replacements,
            hash,
        })
    })
    .await
}

/// Applies an exact replacement while preserving a pure-CRLF file's line endings when the caller
/// sends LF newlines.
fn apply_edit(
    path: &Path,
    text: &str,
    old: &str,
    new: &str,
    all: bool,
) -> Result<(String, usize), ToolError> {
    let adapt_crlf = is_pure_crlf(text) && !old.contains('\r') && !new.contains('\r');
    let effective_old: Cow<'_, str> = if adapt_crlf && old.contains('\n') {
        Cow::Owned(old.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(old)
    };
    let effective_new: Cow<'_, str> = if adapt_crlf && new.contains('\n') {
        Cow::Owned(new.replace('\n', "\r\n"))
    } else {
        Cow::Borrowed(new)
    };

    let count = text.matches(effective_old.as_ref()).count();
    if count == 0 {
        return Err(ErrorBody::new(
            ErrorCode::NotFound,
            format!("The text to replace was not found in {}.", path.display()),
        )
        .with_hint(
            "Read the file with fs.read to check the exact text, whitespace and indentation.",
        ));
    }
    if count > 1 && !all {
        return Err(ErrorBody::new(
            ErrorCode::Conflict,
            format!(
                "Found {count} matches in {}, and `all` is false.",
                path.display()
            ),
        )
        .with_hint(format!(
            "Include more surrounding context in `old` so it matches once, or set all: true to replace all {count} matches."
        ))
        .with_details(json!({"matches": count})));
    }

    let updated = if all {
        text.replace(effective_old.as_ref(), effective_new.as_ref())
    } else {
        text.replacen(effective_old.as_ref(), effective_new.as_ref(), 1)
    };
    Ok((updated, count))
}

/// Whether `text` uses `\r\n` everywhere and has no bare `\n`.
fn is_pure_crlf(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut saw_crlf = false;
    for (idx, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            if idx > 0 && bytes[idx - 1] == b'\r' {
                saw_crlf = true;
            } else {
                return false;
            }
        }
    }
    saw_crlf
}

/// Arguments of `fs.mkdir`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    /// Directory to create (absolute or starting with `~`).
    path: String,
    /// Create missing parent directories and succeed if the directory already exists.
    #[serde(default = "default_true")]
    parents: bool,
}

fn default_true() -> bool {
    true
}

/// Result of `fs.mkdir`.
#[derive(Debug, Serialize)]
struct MkdirResult {
    path: String,
    created: bool,
}

async fn mkdir(ctx: &Ctx, args: MkdirArgs) -> Result<MkdirResult, ToolError> {
    let path = check_fs_path(ctx.grant(), &args.path, Access::Write)?;
    ctx.grant()
        .check_create(&path, CreateKind::Dir, args.parents)?;
    let parents = args.parents;

    run_blocking(ctx, "fs.mkdir", move |_cancel, _progress| {
        let created = if parents {
            match fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => false,
                Ok(meta) if meta.file_type().is_symlink() => match fs::metadata(&path) {
                    Ok(target_meta) if target_meta.is_dir() => false,
                    _ => {
                        return Err(ErrorBody::new(
                            ErrorCode::Exists,
                            format!("{} already exists and is not a directory.", path.display()),
                        ));
                    }
                },
                Ok(_) => {
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("{} already exists and is not a directory.", path.display()),
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    fs::create_dir_all(&path)
                        .map_err(|error| map_io_error("create directory", &path, error))?;
                    true
                }
                Err(error) => return Err(map_io_error("inspect", &path, error)),
            }
        } else {
            match fs::create_dir(&path) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let hint = if fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                        "Pass parents: true to succeed when the directory already exists."
                    } else {
                        "A file already exists at that path."
                    };
                    return Err(ErrorBody::new(
                        ErrorCode::Exists,
                        format!("{} already exists.", path.display()),
                    )
                    .with_hint(hint));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!("Parent directory of {} does not exist.", path.display()),
                    )
                    .with_hint("Pass parents: true to create missing parent directories."));
                }
                Err(error) => return Err(map_io_error("create directory", &path, error)),
            }
        };

        Ok(MkdirResult {
            path: path_to_string(&path),
            created,
        })
    })
    .await
}

/// Resolves a terminal symbolic link so writing or editing through it updates the target file
/// instead of replacing the link with a regular file. The resolved target is passed through
/// `Grant::check_path` for write as well, and a dangling link is refused cleanly.
fn resolve_write_target(
    grant: &Grant,
    path: PathBuf,
    create_only: bool,
) -> Result<PathBuf, ToolError> {
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if create_only {
                return Err(ErrorBody::new(
                    ErrorCode::Exists,
                    format!("{} already exists.", path.display()),
                )
                .with_hint("Use mode \"overwrite\" or \"append\" to modify an existing file."));
            }
            let target = match dunce::canonicalize(&path) {
                Ok(target) => target,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(ErrorBody::new(
                        ErrorCode::NotFound,
                        format!(
                            "{} is a dangling symbolic link whose target does not exist.",
                            path.display()
                        ),
                    )
                    .with_hint(
                        "Write to the target path directly, or remove the broken symbolic link first.",
                    ));
                }
                Err(error) => return Err(map_io_error("resolve symbolic link", &path, error)),
            };
            if platform::IS_WINDOWS {
                validate_windows_path(&path_to_string(&target))?;
            }
            let resolved = grant.check_path(&target, Access::Write)?;
            grant.check_create(&resolved, CreateKind::File, false)?;
            Ok(resolved)
        }
        Ok(_) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(map_io_error("inspect", &path, error)),
    }
}

/// Streams `bytes` into `write_atomic_from_reader` in bounded chunks, aborting promptly when
/// `cancel` fires so `legion-os` cleans up its temporary file and leaves `path` untouched.
fn write_atomic_cancellable(
    path: &Path,
    bytes: &[u8],
    options: WriteOptions,
    cancel: &CancellationToken,
    tool: &str,
) -> Result<(), ToolError> {
    let mut reader = CancelSliceReader {
        data: bytes,
        offset: 0,
        cancel,
        cancelled: false,
    };
    match write_atomic_from_reader(path, &mut reader, options) {
        Ok(_) => Ok(()),
        Err(_) if reader.cancelled => Err(cancelled_error(tool)),
        Err(error) => Err(map_os_error("write", path, error)),
    }
}

struct CancelSliceReader<'a> {
    data: &'a [u8],
    offset: usize,
    cancel: &'a CancellationToken,
    cancelled: bool,
}

impl Read for CancelSliceReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.cancel.is_cancelled() {
            self.cancelled = true;
            // Must not be `ErrorKind::Interrupted`, which `std::io::copy` silently retries.
            return Err(io::Error::other("cancelled"));
        }
        let remaining = &self.data[self.offset..];
        if remaining.is_empty() {
            return Ok(0);
        }
        let n = remaining.len().min(buf.len()).min(HASH_CHUNK_BYTES);
        buf[..n].copy_from_slice(&remaining[..n]);
        self.offset += n;
        Ok(n)
    }
}

fn hash_bytes_cancellable(
    bytes: &[u8],
    cancel: &CancellationToken,
    tool: &str,
) -> Result<String, ToolError> {
    let mut hasher = blake3::Hasher::new();
    for chunk in bytes.chunks(HASH_CHUNK_BYTES) {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        hasher.update(chunk);
    }
    Ok(format_blake3(hasher.finalize()))
}

fn check_expected_hash(path: &Path, expected: &str, actual: &str) -> Result<(), ToolError> {
    if actual == expected {
        return Ok(());
    }
    Err(ErrorBody::new(
        ErrorCode::Conflict,
        format!(
            "{} changed since it was read (expected {expected}, found {actual}).",
            path.display()
        ),
    )
    .with_hint("Read the file again with fs.read or fs.hash and retry with the new hash.")
    .with_details(json!({"expected": expected, "actual": actual})))
}

fn hash_file(path: &Path, cancel: &CancellationToken, tool: &str) -> Result<String, ToolError> {
    let (mut file, _meta) = super::open_regular_file(
        path,
        "read",
        "Pass a regular file path; directories cannot be hashed.",
    )?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; HASH_CHUNK_BYTES];
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        let n = file
            .read(&mut buf)
            .map_err(|error| map_io_error("read", path, error))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format_blake3(hasher.finalize()))
}

fn file_too_large_for_edit(path: &Path, size: Option<u64>) -> ErrorBody {
    let message = match size {
        Some(len) => format!(
            "{} is {len} bytes, above fs.edit's {MAX_EDIT_FILE_BYTES}-byte (16 MiB) limit.",
            path.display()
        ),
        None => format!(
            "{} exceeds fs.edit's {MAX_EDIT_FILE_BYTES}-byte (16 MiB) limit.",
            path.display()
        ),
    };
    ErrorBody::new(ErrorCode::InvalidArgs, message)
        .with_hint("Use fs.transfer for larger files instead of editing them inline with fs.edit.")
}

fn read_file_cancellable(
    path: &Path,
    max_bytes: usize,
    cancel: &CancellationToken,
    tool: &str,
) -> Result<Vec<u8>, ToolError> {
    let (mut file, _meta) = super::open_regular_file(
        path,
        "read",
        "Pass a regular file path; directories cannot be edited.",
    )?;
    let mut out = Vec::new();
    let mut buf = [0u8; HASH_CHUNK_BYTES];
    loop {
        if cancel.is_cancelled() {
            return Err(cancelled_error(tool));
        }
        let remaining = max_bytes.saturating_add(1).saturating_sub(out.len());
        if remaining == 0 {
            return Err(file_too_large_for_edit(path, None));
        }
        let to_read = remaining.min(buf.len());
        let n = file
            .read(&mut buf[..to_read])
            .map_err(|error| map_io_error("read", path, error))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.len() > max_bytes {
            return Err(file_too_large_for_edit(path, None));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edit_handles_single_all_missing_and_ambiguous_matches() {
        let p = Path::new("/notes.txt");
        let (out, n) = apply_edit(p, "alpha beta gamma\n", "beta", "BETA", false).unwrap();
        assert_eq!(out, "alpha BETA gamma\n");
        assert_eq!(n, 1);

        let missing = apply_edit(p, "alpha\n", "omega", "x", false).unwrap_err();
        assert_eq!(missing.code, ErrorCode::NotFound);
        assert!(missing.hint.is_some());

        let ambig = apply_edit(p, "foo bar foo\n", "foo", "baz", false).unwrap_err();
        assert_eq!(ambig.code, ErrorCode::Conflict);
        assert!(ambig.message.contains("2 matches"), "{}", ambig.message);

        let (all_out, all_n) = apply_edit(p, "foo bar foo\n", "foo", "baz", true).unwrap();
        assert_eq!(all_out, "baz bar baz\n");
        assert_eq!(all_n, 2);
    }

    #[test]
    fn apply_edit_preserves_crlf_line_endings() {
        let p = Path::new("/win.txt");
        let crlf = "one\r\ntwo\r\nthree\r\n";

        let (single, _) = apply_edit(p, crlf, "two", "TWO", false).unwrap();
        assert_eq!(single, "one\r\nTWO\r\nthree\r\n");

        // Multi-line edit supplied with LF adapts to a pure-CRLF file.
        let (multi, _) = apply_edit(p, crlf, "one\ntwo", "ONE\nTWO", false).unwrap();
        assert_eq!(multi, "ONE\r\nTWO\r\nthree\r\n");

        // Inserting new lines into a pure-CRLF file keeps CRLF.
        let (inserted, _) = apply_edit(p, crlf, "two", "two-a\ntwo-b", false).unwrap();
        assert_eq!(inserted, "one\r\ntwo-a\r\ntwo-b\r\nthree\r\n");

        // Explicitly targeting \r\n leaves the caller's exact line endings alone.
        let (explicit, _) = apply_edit(p, crlf, "one\r\n", "one\n", false).unwrap();
        assert_eq!(explicit, "one\ntwo\r\nthree\r\n");

        // Mixed files are never rewritten implicitly.
        let mixed = "one\r\ntwo\n";
        let (kept, _) = apply_edit(p, mixed, "two", "a\nb", false).unwrap();
        assert_eq!(kept, "one\r\na\nb\n");
    }

    #[test]
    fn cancelled_atomic_write_leaves_target_untouched_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data.bin");
        fs::write(&target, b"original").unwrap();

        let cancel = CancellationToken::new();
        let payload = vec![b'x'; HASH_CHUNK_BYTES * 4];
        let mut reader = CancelAfterFirstChunk {
            inner: CancelSliceReader {
                data: &payload,
                offset: 0,
                cancel: &cancel,
                cancelled: false,
            },
            reads: 0,
        };
        let err =
            write_atomic_from_reader(&target, &mut reader, WriteOptions::default()).unwrap_err();
        assert!(reader.inner.cancelled, "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"original");
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec!["data.bin"]);
    }

    struct CancelAfterFirstChunk<'a> {
        inner: CancelSliceReader<'a>,
        reads: usize,
    }

    impl Read for CancelAfterFirstChunk<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            if self.reads == 2 {
                self.inner.cancel.cancel();
            }
            self.inner.read(buf)
        }
    }
}
