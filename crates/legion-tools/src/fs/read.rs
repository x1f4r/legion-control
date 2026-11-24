//! Read-only inspection tools: `fs.list`, `fs.stat`, `fs.read` and `fs.hash`.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Encoding, EntryKind, Progress, cancelled_error, format_blake3, format_mtime, is_hidden,
    map_io_error, path_to_string, platform, run_blocking,
};

const MAX_LIST_DEPTH: u32 = 8;
const DEFAULT_LIST_LIMIT: u32 = 500;
const MAX_LIST_LIMIT: u32 = 5000;
const MAX_READ_LENGTH: u64 = 1024 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.list",
        Effect::Read,
        "List entries in a directory up to a bounded depth and count, directories first then by name, reporting symbolic links without following them.",
        list,
    );
    registry.add_fn(
        "fs.stat",
        Effect::Read,
        "Return metadata for a path (kind, size, modification time, Unix mode, readonly flag, and symbolic link target) without following a symbolic link.",
        stat,
    );
    registry.add_fn(
        "fs.read",
        Effect::Read,
        "Read up to 1 MiB of a file from an offset as UTF-8 or base64, returning the whole-file BLAKE3 hash when the entire file fit in this reply.",
        read,
    );
    registry.add_fn(
        "fs.hash",
        Effect::Read,
        "Stream a file and return its size and BLAKE3 digest as blake3:<hex>.",
        hash,
    );
}

/// Arguments of `fs.list`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    /// Directory to list (absolute or starting with `~`).
    path: String,
    /// How many directory levels to descend, from 1 to 8.
    #[serde(default = "default_list_depth")]
    #[schemars(range(min = 1, max = 8))]
    depth: u32,
    /// Include hidden files and directories (dotfiles, and Windows hidden entries).
    #[serde(default = "default_true")]
    hidden: bool,
    /// Maximum number of entries to return, from 1 to 5000.
    #[serde(default = "default_list_limit")]
    #[schemars(range(min = 1, max = 5000))]
    limit: u32,
}

fn default_list_depth() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn default_list_limit() -> u32 {
    DEFAULT_LIST_LIMIT
}

/// One entry returned by `fs.list`.
#[derive(Debug, Serialize)]
struct ListEntry {
    name: String,
    path: String,
    kind: EntryKind,
    size: u64,
    mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}

/// Result of `fs.list`.
#[derive(Debug, Serialize)]
struct ListResult {
    path: String,
    entries: Vec<ListEntry>,
    truncated: bool,
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<ListResult, ToolError> {
    if !(1..=MAX_LIST_DEPTH).contains(&args.depth) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.list depth must be between 1 and {MAX_LIST_DEPTH}, and {} was given.",
                args.depth
            ),
        )
        .with_hint("Use fs.find for deeper directory searches."));
    }
    if !(1..=MAX_LIST_LIMIT).contains(&args.limit) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.list limit must be between 1 and {MAX_LIST_LIMIT}, and {} was given.",
                args.limit
            ),
        ));
    }
    let root = ctx.grant().check_path(&args.path, Access::Read)?;
    let max_depth = args.depth;
    let include_hidden = args.hidden;
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);

    run_blocking(ctx, "fs.list", move |cancel, mut progress| {
        let meta = fs::metadata(&root).map_err(|error| map_io_error("list", &root, error))?;
        if !meta.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{} is not a directory.", root.display()),
            )
            .with_hint("Use fs.stat or fs.read for a file, or pass its parent directory."));
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut state = ListWalk {
            max_depth,
            include_hidden,
            limit,
            cancel: &cancel,
            progress: &mut progress,
            entries: &mut entries,
            truncated: &mut truncated,
        };
        list_dir(&root, 1, &mut state)?;
        Ok(ListResult {
            path: path_to_string(&root),
            entries,
            truncated,
        })
    })
    .await
}

struct ListWalk<'a> {
    max_depth: u32,
    include_hidden: bool,
    limit: usize,
    cancel: &'a CancellationToken,
    progress: &'a mut Progress,
    entries: &'a mut Vec<ListEntry>,
    truncated: &'a mut bool,
}

fn list_dir(dir: &Path, depth: u32, walk: &mut ListWalk<'_>) -> Result<(), ToolError> {
    if walk.cancel.is_cancelled() {
        return Err(cancelled_error("fs.list"));
    }
    let read_dir = match fs::read_dir(dir) {
        Ok(iter) => iter,
        // Failing to read the root directory is an error; an unreadable nested directory during a
        // recursive listing is skipped so the rest of the tree is still reported.
        Err(error) if depth == 1 => return Err(map_io_error("list", dir, error)),
        Err(_) => return Ok(()),
    };

    let mut children: Vec<(String, PathBuf, fs::Metadata, EntryKind)> = Vec::new();
    for item in read_dir {
        if walk.cancel.is_cancelled() {
            return Err(cancelled_error("fs.list"));
        }
        let Ok(entry) = item else {
            continue;
        };
        let child_path = entry.path();
        let meta = match fs::symlink_metadata(&child_path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !walk.include_hidden && is_hidden(&name, &meta) {
            continue;
        }
        let kind = EntryKind::from_file_type(meta.file_type());
        children.push((name, child_path, meta, kind));
    }

    children.sort_by(|a, b| {
        let a_rank = usize::from(a.3 != EntryKind::Dir);
        let b_rank = usize::from(b.3 != EntryKind::Dir);
        a_rank.cmp(&b_rank).then_with(|| a.0.cmp(&b.0))
    });

    for (name, child_path, meta, kind) in children {
        if walk.cancel.is_cancelled() {
            return Err(cancelled_error("fs.list"));
        }
        if walk.entries.len() == walk.limit {
            *walk.truncated = true;
            return Ok(());
        }
        let target = if kind == EntryKind::Symlink {
            fs::read_link(&child_path)
                .ok()
                .map(|link| path_to_string(&link))
        } else {
            None
        };
        walk.entries.push(ListEntry {
            name,
            path: path_to_string(&child_path),
            kind,
            size: meta.len(),
            mtime: format_mtime(&meta),
            mode: platform::unix_mode(&meta),
            target,
        });
        let count = walk.entries.len();
        walk.progress.maybe_emit(|| json!({"entries": count}));

        if kind == EntryKind::Dir && depth < walk.max_depth {
            list_dir(&child_path, depth + 1, walk)?;
            if *walk.truncated {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Arguments of `fs.stat`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatArgs {
    /// Path to inspect (absolute or starting with `~`).
    path: String,
}

/// Result of `fs.stat`.
#[derive(Debug, Serialize)]
struct StatResult {
    path: String,
    kind: EntryKind,
    size: u64,
    mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    readonly: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}

async fn stat(ctx: &Ctx, args: StatArgs) -> Result<StatResult, ToolError> {
    let path = ctx.grant().check_path(&args.path, Access::Read)?;
    run_blocking(ctx, "fs.stat", move |_cancel, _progress| {
        let meta =
            fs::symlink_metadata(&path).map_err(|error| map_io_error("stat", &path, error))?;
        let kind = EntryKind::from_file_type(meta.file_type());
        let target = if kind == EntryKind::Symlink {
            fs::read_link(&path).ok().map(|link| path_to_string(&link))
        } else {
            None
        };
        Ok(StatResult {
            path: path_to_string(&path),
            kind,
            size: meta.len(),
            mtime: format_mtime(&meta),
            mode: platform::unix_mode(&meta),
            readonly: meta.permissions().readonly(),
            target,
        })
    })
    .await
}

/// Arguments of `fs.read`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    /// File to read (absolute or starting with `~`).
    path: String,
    /// Byte offset to start reading from.
    #[serde(default)]
    offset: u64,
    /// Maximum number of bytes to read, up to 1 MiB (1048576).
    #[serde(default = "default_read_length")]
    #[schemars(range(min = 0, max = 1048576))]
    length: u64,
    /// Output encoding (`utf8` or `base64`); omitted selects `utf8` when valid and `base64`
    /// otherwise.
    #[serde(default)]
    encoding: Option<Encoding>,
}

fn default_read_length() -> u64 {
    MAX_READ_LENGTH
}

/// Result of `fs.read`.
#[derive(Debug, Serialize)]
struct ReadResult {
    path: String,
    size: u64,
    offset: u64,
    length: u64,
    encoding: Encoding,
    content: String,
    eof: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

async fn read(ctx: &Ctx, args: ReadArgs) -> Result<ReadResult, ToolError> {
    if args.length > MAX_READ_LENGTH {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.read returns at most {MAX_READ_LENGTH} bytes (1 MiB), and {} were asked for.",
                args.length
            ),
        )
        .with_hint("Page through larger files with offset, or stream them with fs.transfer."));
    }
    let path = ctx.grant().check_path(&args.path, Access::Read)?;
    let offset = args.offset;
    let length = usize::try_from(args.length).unwrap_or(usize::MAX);
    let requested_encoding = args.encoding;

    run_blocking(ctx, "fs.read", move |cancel, _progress| {
        let mut file = File::open(&path).map_err(|error| map_io_error("read", &path, error))?;
        let meta = file
            .metadata()
            .map_err(|error| map_io_error("read", &path, error))?;
        if meta.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{} is a directory, not a file.", path.display()),
            )
            .with_hint("Use fs.list to inspect a directory."));
        }
        let size = meta.len();
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| map_io_error("seek", &path, error))?;
        }

        // Read up to length + 1 bytes so we know definitively whether more bytes remain past the
        // requested slice.
        let want = length.saturating_add(1);
        let mut raw = Vec::with_capacity(want.min(STREAM_CHUNK_BYTES));
        let mut scratch = [0u8; STREAM_CHUNK_BYTES];
        while raw.len() < want {
            if cancel.is_cancelled() {
                return Err(cancelled_error("fs.read"));
            }
            let remaining = want - raw.len();
            let to_read = remaining.min(scratch.len());
            let read_now = file
                .read(&mut scratch[..to_read])
                .map_err(|error| map_io_error("read", &path, error))?;
            if read_now == 0 {
                break;
            }
            raw.extend_from_slice(&scratch[..read_now]);
        }

        let reached_eof = raw.len() <= length;
        let mut peek = [0u8; 3];
        let mut peek_len = 0usize;
        if !reached_eof {
            peek[0] = raw[length];
            peek_len = 1;
            raw.truncate(length);
        }

        // If the slice ends mid-character, peek up to 2 further bytes (3 total past the cut) so we
        // can tell a split multi-byte UTF-8 character apart from invalid binary bytes.
        if !reached_eof
            && requested_encoding != Some(Encoding::Base64)
            && let Err(err) = std::str::from_utf8(&raw)
            && err.error_len().is_none()
        {
            while peek_len < peek.len() {
                let n = file
                    .read(&mut peek[peek_len..])
                    .map_err(|error| map_io_error("read", &path, error))?;
                if n == 0 {
                    break;
                }
                peek_len += n;
            }
        }

        let decoded = decode_chunk(
            &path,
            &raw,
            requested_encoding,
            reached_eof,
            &peek[..peek_len],
        )?;
        let hash =
            (offset == 0 && decoded.eof).then(|| format_blake3(blake3::hash(&raw[..decoded.used])));

        Ok(ReadResult {
            path: path_to_string(&path),
            size,
            offset,
            length: decoded.used as u64,
            encoding: decoded.encoding,
            content: decoded.content,
            eof: decoded.eof,
            hash,
        })
    })
    .await
}

#[derive(Debug)]
struct DecodedChunk {
    encoding: Encoding,
    content: String,
    used: usize,
    eof: bool,
}

/// Decodes a read byte slice according to `requested`, trimming an incomplete trailing UTF-8
/// character at a chunk boundary when `peek` confirms it completes cleanly.
fn decode_chunk(
    path: &Path,
    chunk: &[u8],
    requested: Option<Encoding>,
    reached_eof: bool,
    peek: &[u8],
) -> Result<DecodedChunk, ToolError> {
    match requested {
        Some(Encoding::Base64) => Ok(DecodedChunk {
            encoding: Encoding::Base64,
            content: BASE64.encode(chunk),
            used: chunk.len(),
            eof: reached_eof,
        }),
        Some(Encoding::Utf8) => match std::str::from_utf8(chunk) {
            Ok(text) => Ok(DecodedChunk {
                encoding: Encoding::Utf8,
                content: text.to_owned(),
                used: chunk.len(),
                eof: reached_eof,
            }),
            Err(err)
                if err.error_len().is_none()
                    && !reached_eof
                    && tail_completes_utf8(&chunk[err.valid_up_to()..], peek) =>
            {
                let cut = err.valid_up_to();
                if cut == 0 {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidArgs,
                        "length is too small to hold the UTF-8 character at this offset.",
                    )
                    .with_hint(
                        "Use a length of at least 4 bytes, or read with encoding \"base64\".",
                    ));
                }
                let text = std::str::from_utf8(&chunk[..cut])
                    .map_err(|_| {
                        ErrorBody::new(
                            ErrorCode::Internal,
                            "UTF-8 boundary calculation produced an invalid slice.",
                        )
                    })?
                    .to_owned();
                Ok(DecodedChunk {
                    encoding: Encoding::Utf8,
                    content: text,
                    used: cut,
                    eof: false,
                })
            }
            Err(err) => Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} is not valid UTF-8 (invalid byte at offset {}).",
                    path.display(),
                    err.valid_up_to()
                ),
            )
            .with_hint("Pass encoding \"base64\" or omit encoding to read binary data.")),
        },
        None => match std::str::from_utf8(chunk) {
            Ok(text) => Ok(DecodedChunk {
                encoding: Encoding::Utf8,
                content: text.to_owned(),
                used: chunk.len(),
                eof: reached_eof,
            }),
            Err(err)
                if err.error_len().is_none()
                    && !reached_eof
                    && err.valid_up_to() > 0
                    && tail_completes_utf8(&chunk[err.valid_up_to()..], peek) =>
            {
                let cut = err.valid_up_to();
                let text = std::str::from_utf8(&chunk[..cut])
                    .map_err(|_| {
                        ErrorBody::new(
                            ErrorCode::Internal,
                            "UTF-8 boundary calculation produced an invalid slice.",
                        )
                    })?
                    .to_owned();
                Ok(DecodedChunk {
                    encoding: Encoding::Utf8,
                    content: text,
                    used: cut,
                    eof: false,
                })
            }
            Err(_) => Ok(DecodedChunk {
                encoding: Encoding::Base64,
                content: BASE64.encode(chunk),
                used: chunk.len(),
                eof: reached_eof,
            }),
        },
    }
}

/// Checks whether the incomplete UTF-8 prefix at the end of a chunk forms a valid UTF-8 character
/// when joined with the bytes immediately following it in the file.
fn tail_completes_utf8(incomplete: &[u8], peek: &[u8]) -> bool {
    if incomplete.is_empty() || incomplete.len() > 3 {
        return false;
    }
    let mut combined = [0u8; 4];
    let take_peek = peek.len().min(combined.len() - incomplete.len());
    combined[..incomplete.len()].copy_from_slice(incomplete);
    combined[incomplete.len()..incomplete.len() + take_peek].copy_from_slice(&peek[..take_peek]);
    let total = incomplete.len() + take_peek;
    match std::str::from_utf8(&combined[..total]) {
        Ok(_) => true,
        Err(err) => err.valid_up_to() > 0,
    }
}

/// Arguments of `fs.hash`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HashArgs {
    /// File to hash (absolute or starting with `~`).
    path: String,
}

/// Result of `fs.hash`.
#[derive(Debug, Serialize)]
struct HashResult {
    path: String,
    size: u64,
    hash: String,
}

async fn hash(ctx: &Ctx, args: HashArgs) -> Result<HashResult, ToolError> {
    let path = ctx.grant().check_path(&args.path, Access::Read)?;
    run_blocking(ctx, "fs.hash", move |cancel, mut progress| {
        let mut file = File::open(&path).map_err(|error| map_io_error("hash", &path, error))?;
        let meta = file
            .metadata()
            .map_err(|error| map_io_error("hash", &path, error))?;
        if meta.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{} is a directory, not a file.", path.display()),
            )
            .with_hint("Pass a file path to fs.hash."));
        }
        let total = meta.len();
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; STREAM_CHUNK_BYTES];
        let mut hashed: u64 = 0;
        loop {
            if cancel.is_cancelled() {
                return Err(cancelled_error("fs.hash"));
            }
            let n = file
                .read(&mut buf)
                .map_err(|error| map_io_error("hash", &path, error))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            hashed += n as u64;
            progress.maybe_emit(|| json!({"bytes": hashed, "total": total}));
        }
        Ok(HashResult {
            path: path_to_string(&path),
            size: hashed,
            hash: format_blake3(hasher.finalize()),
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_chunk_cuts_at_utf8_character_boundary() {
        // "hi€!" is 6 bytes: b"hi" (2), 0xE2 0x82 0xAC (3), b"!" (1).
        let bytes = "hi€!".as_bytes();
        let decoded = decode_chunk(Path::new("/x"), &bytes[..4], None, false, &bytes[4..]).unwrap();
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(decoded.content, "hi");
        assert_eq!(decoded.used, 2);
        assert!(!decoded.eof);

        // Next page starting at offset 2 finishes the file cleanly.
        let rest = decode_chunk(Path::new("/x"), &bytes[2..], None, true, &[]).unwrap();
        assert_eq!(rest.encoding, Encoding::Utf8);
        assert_eq!(rest.content, "€!");
        assert_eq!(rest.used, 4);
        assert!(rest.eof);
    }

    #[test]
    fn decode_chunk_falls_back_to_base64_for_invalid_bytes_even_at_end() {
        let bytes = [b'a', b'b', 0xE2, 0x82, 0x00];
        let decoded = decode_chunk(Path::new("/x"), &bytes[..4], None, false, &bytes[4..]).unwrap();
        assert_eq!(decoded.encoding, Encoding::Base64);
        assert_eq!(decoded.used, 4);
        assert_eq!(BASE64.decode(&decoded.content).unwrap(), &bytes[..4]);

        // Truncated UTF-8 at true EOF is also invalid UTF-8 -> base64 when auto-detecting.
        let eof_bad = decode_chunk(Path::new("/x"), &bytes[..4], None, true, &[]).unwrap();
        assert_eq!(eof_bad.encoding, Encoding::Base64);
        assert!(eof_bad.eof);
    }

    #[test]
    fn decode_chunk_rejects_invalid_utf8_when_utf8_was_requested() {
        let err = decode_chunk(
            Path::new("/x"),
            &[0xFF, 0xFE],
            Some(Encoding::Utf8),
            true,
            &[],
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(err.hint.is_some());
    }
}
