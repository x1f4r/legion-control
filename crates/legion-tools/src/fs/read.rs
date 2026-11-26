//! Read-only inspection tools: `fs.list`, `fs.stat`, `fs.read` and `fs.hash`.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_core::media::{Category, classify_path};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Encoding, EntryKind, MAX_REPLY_BYTES, OwnDirFilter, Progress, SortField, SortOrder,
    cancelled_error, check_fs_path, compare_by_sort, format_blake3, format_mtime, is_hidden,
    map_io_error, natural_cmp, path_to_string, platform, run_blocking,
};

const MAX_LIST_DEPTH: u32 = 8;
const DEFAULT_LIST_LIMIT: u32 = 500;
const MAX_LIST_LIMIT: u32 = 5000;
/// Maximum matching entries counted during a multi-level (`depth > 1`) walk before stopping with
/// `total_exact: false`. Single-directory (`depth == 1`) listings always read and count every
/// direct child accurately.
const MAX_RECURSIVE_LIST_COUNT: usize = 10_000;
/// Wall-clock budget for recursive (`depth > 1`) listing walks so deep trees stay bounded.
const RECURSIVE_LIST_BUDGET: Duration = Duration::from_secs(2);
const MAX_READ_LENGTH: u64 = 1024 * 1024;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.list",
        Effect::Read,
        "List entries in a directory up to a bounded depth and count, directories first then ordered by name (natural case-insensitive), modified time, or size, with optional category filtering, paging, total/total_exact counts, staying on one file system for depth > 1, and reporting symbolic links without following them.",
        list,
    );
    registry.add_fn(
        "fs.stat",
        Effect::Read,
        "Return metadata for a path (kind, size, modification time, Unix mode, readonly flag, symbolic link target, and file category and MIME type) without following a symbolic link.",
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
    /// Number of matching entries to skip before returning up to `limit`.
    #[serde(default)]
    offset: u64,
    /// Field to sort by within directories and within non-directories (`name`, `modified`, or `size`).
    #[serde(default)]
    sort: SortField,
    /// Sort direction (`asc` or `desc`). Directories always come first regardless of order.
    #[serde(default)]
    order: SortOrder,
    /// Optional list of file categories to include; directories are always kept.
    #[serde(default)]
    categories: Option<Vec<Category>>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<Category>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
}

/// Result of `fs.list`.
#[derive(Debug, Serialize)]
struct ListResult {
    path: String,
    entries: Vec<ListEntry>,
    total: usize,
    total_exact: bool,
    truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<String>,
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
    let root = check_fs_path(ctx.grant(), &args.path, Access::Read)?;
    let own_dirs = OwnDirFilter::for_ctx(ctx);
    let max_depth = args.depth;
    let include_hidden = args.hidden;
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let offset = usize::try_from(args.offset).unwrap_or(usize::MAX);
    let sort = args.sort;
    let order = args.order;
    let categories = args.categories;

    run_blocking(ctx, "fs.list", move |cancel, mut progress| {
        let meta = fs::metadata(&root).map_err(|error| map_io_error("list", &root, error))?;
        if !meta.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{} is not a directory.", root.display()),
            )
            .with_hint("Use fs.stat or fs.read for a file, or pass its parent directory."));
        }
        let root_device = platform::device_id(&root, &meta);
        let root_str = path_to_string(&root);
        let reply_bytes = serde_json::to_vec(&ListResult {
            path: root_str.clone(),
            entries: Vec::new(),
            total: usize::MAX,
            total_exact: false,
            truncated: true,
            skipped: Vec::new(),
        })
        .map_or(112, |v| v.len());

        // For `depth > 1`, bound work proportionally to `offset + limit` (with a hard ceiling and
        // wall-clock budget) so `fs.list /usr depth=8 limit=1` stops promptly rather than walking
        // hundreds of thousands of entries just to compute `total`.
        let page_end = offset.saturating_add(limit);
        let count_cap = if max_depth > 1 {
            Some(
                page_end
                    .saturating_mul(2)
                    .max(page_end.saturating_add(1))
                    .min(MAX_RECURSIVE_LIST_COUNT),
            )
        } else {
            None
        };
        let visit_cap = count_cap.map(|c| c.saturating_mul(4).clamp(256, 40_000));
        let deadline = (max_depth > 1).then(|| Instant::now() + RECURSIVE_LIST_BUDGET);

        let mut entries = Vec::new();
        let mut skipped = Vec::new();
        let mut total: usize = 0;
        let mut visited: usize = 0;
        let mut page_full = false;
        let mut stopped_early = false;
        let mut state = ListWalk {
            max_depth,
            include_hidden,
            limit,
            offset,
            sort,
            order,
            categories: categories.as_deref(),
            root_device,
            count_cap,
            visit_cap,
            deadline,
            reply_bytes,
            own_dirs: &own_dirs,
            cancel: &cancel,
            progress: &mut progress,
            entries: &mut entries,
            skipped: &mut skipped,
            total: &mut total,
            visited: &mut visited,
            page_full: &mut page_full,
            stopped_early: &mut stopped_early,
        };
        list_dir(&root, 1, &mut state)?;
        skipped.sort();
        let total_exact = !stopped_early;
        let truncated = stopped_early || offset.saturating_add(entries.len()) < total;
        Ok(ListResult {
            path: root_str,
            entries,
            total,
            total_exact,
            truncated,
            skipped,
        })
    })
    .await
}

struct ListWalk<'a> {
    max_depth: u32,
    include_hidden: bool,
    limit: usize,
    offset: usize,
    sort: SortField,
    order: SortOrder,
    categories: Option<&'a [Category]>,
    root_device: Option<platform::DeviceId>,
    count_cap: Option<usize>,
    visit_cap: Option<usize>,
    deadline: Option<Instant>,
    reply_bytes: usize,
    own_dirs: &'a OwnDirFilter,
    cancel: &'a CancellationToken,
    progress: &'a mut Progress,
    entries: &'a mut Vec<ListEntry>,
    skipped: &'a mut Vec<String>,
    total: &'a mut usize,
    visited: &'a mut usize,
    page_full: &'a mut bool,
    stopped_early: &'a mut bool,
}

impl ListWalk<'_> {
    fn hit_recursive_budget(&self) -> bool {
        if let Some(deadline) = self.deadline
            && self.visited.is_multiple_of(32)
            && Instant::now() >= deadline
        {
            return true;
        }
        if *self.page_full {
            if self.count_cap.is_some_and(|cap| *self.total >= cap) {
                return true;
            }
            if self.visit_cap.is_some_and(|cap| *self.visited >= cap) {
                return true;
            }
        }
        false
    }
}

/// Partitions `items` in linear time so the `[offset..offset+limit]` page sits in exact sorted
/// order under `cmp` without sorting the rest of the directory.
fn select_and_sort_page<T, F>(items: &mut [T], offset: usize, limit: usize, mut cmp: F) -> &mut [T]
where
    F: FnMut(&T, &T) -> Ordering,
{
    let len = items.len();
    if offset >= len || limit == 0 {
        return &mut [];
    }
    let page_end = offset.saturating_add(limit).min(len);
    if offset > 0 {
        items.select_nth_unstable_by(offset, &mut cmp);
    }
    let rest = &mut items[offset..];
    let take = page_end - offset;
    if take < rest.len() {
        rest.select_nth_unstable_by(take, &mut cmp);
    }
    let page = &mut rest[..take];
    page.sort_by(&mut cmp);
    page
}

fn push_list_entry(
    walk: &mut ListWalk<'_>,
    name: String,
    child_path: &Path,
    meta: &fs::Metadata,
    kind: EntryKind,
    precomputed_media: Option<(Option<Category>, Option<&'static str>)>,
) -> bool {
    if walk.entries.len() == walk.limit {
        *walk.page_full = true;
        return false;
    }
    let (category, mime) = precomputed_media.unwrap_or_else(|| {
        if kind == EntryKind::File {
            let info = classify_path(child_path);
            (Some(info.category), Some(info.mime))
        } else {
            (None, None)
        }
    });
    let target = if kind == EntryKind::Symlink {
        fs::read_link(child_path)
            .ok()
            .map(|link| path_to_string(&link))
    } else {
        None
    };
    let entry = ListEntry {
        name,
        path: path_to_string(child_path),
        kind,
        size: meta.len(),
        mtime: format_mtime(meta),
        mode: platform::unix_mode(meta),
        target,
        category,
        mime: mime.map(str::to_owned),
    };
    let entry_bytes =
        serde_json::to_vec(&entry).map_or(0, |v| v.len()) + usize::from(!walk.entries.is_empty());
    if !walk.entries.is_empty() && walk.reply_bytes.saturating_add(entry_bytes) > MAX_REPLY_BYTES {
        *walk.page_full = true;
        return false;
    }
    walk.reply_bytes = walk.reply_bytes.saturating_add(entry_bytes);
    walk.entries.push(entry);
    let count = walk.entries.len();
    walk.progress.maybe_emit(|| json!({"entries": count}));
    true
}

/// Fast single-level listing when `depth == 1`, sorting by name, on platforms where hiddenness
/// depends only on the filename, and with no protected-directory filter entries to inspect:
/// uses `DirEntry::file_type()` (`d_type`) during the scan, partitions only the requested page
/// with `select_nth_unstable_by`, and runs `symlink_metadata` only for the returned page entries.
fn list_dir_shallow_by_name(
    dir: &Path,
    read_dir: fs::ReadDir,
    walk: &mut ListWalk<'_>,
) -> Result<(), ToolError> {
    struct ShallowChild {
        name: String,
        raw_name: Option<OsString>,
        kind: EntryKind,
    }

    let mut children: Vec<ShallowChild> = Vec::new();
    for item in read_dir {
        *walk.visited = walk.visited.saturating_add(1);
        if walk.visited.is_multiple_of(32) && walk.cancel.is_cancelled() {
            return Err(cancelled_error("fs.list"));
        }
        let Ok(entry) = item else {
            continue;
        };
        let file_name = entry.file_name();
        let (name, raw_name) = match file_name.into_string() {
            Ok(s) => (s, None),
            Err(os) => (os.to_string_lossy().into_owned(), Some(os)),
        };
        if !walk.include_hidden && name.starts_with('.') {
            continue;
        }
        let kind = match entry.file_type() {
            Ok(ft) => EntryKind::from_file_type(ft),
            Err(_) => {
                let child_path = dir.join(raw_name.as_deref().unwrap_or_else(|| OsStr::new(&name)));
                let Ok(meta) = fs::symlink_metadata(&child_path) else {
                    continue;
                };
                EntryKind::from_file_type(meta.file_type())
            }
        };
        if let Some(wanted) = walk.categories
            && kind != EntryKind::Dir
        {
            if kind != EntryKind::File {
                continue;
            }
            let name_path = Path::new(raw_name.as_deref().unwrap_or_else(|| OsStr::new(&name)));
            let info = classify_path(name_path);
            if !wanted.contains(&info.category) {
                continue;
            }
        }
        children.push(ShallowChild {
            name,
            raw_name,
            kind,
        });
    }

    *walk.total = children.len();
    if walk.offset >= children.len() || walk.limit == 0 {
        return Ok(());
    }

    let order = walk.order;
    let page = select_and_sort_page(&mut children, walk.offset, walk.limit, |a, b| {
        let a_rank = usize::from(a.kind != EntryKind::Dir);
        let b_rank = usize::from(b.kind != EntryKind::Dir);
        a_rank.cmp(&b_rank).then_with(|| {
            let primary = natural_cmp(&a.name, &b.name);
            let ordered = match order {
                SortOrder::Asc => primary,
                SortOrder::Desc => primary.reverse(),
            };
            ordered.then_with(|| a.raw_name.cmp(&b.raw_name))
        })
    });

    for child in page {
        if walk.cancel.is_cancelled() {
            return Err(cancelled_error("fs.list"));
        }
        let child_path = dir.join(
            child
                .raw_name
                .as_deref()
                .unwrap_or_else(|| OsStr::new(&child.name)),
        );
        let Ok(meta) = fs::symlink_metadata(&child_path) else {
            *walk.total = walk.total.saturating_sub(1);
            continue;
        };
        let kind = EntryKind::from_file_type(meta.file_type());
        if !push_list_entry(
            walk,
            std::mem::take(&mut child.name),
            &child_path,
            &meta,
            kind,
            None,
        ) {
            break;
        }
    }
    Ok(())
}

fn list_dir(dir: &Path, depth: u32, walk: &mut ListWalk<'_>) -> Result<(), ToolError> {
    if walk.cancel.is_cancelled() {
        return Err(cancelled_error("fs.list"));
    }
    if *walk.stopped_early {
        return Ok(());
    }
    let read_dir = match fs::read_dir(dir) {
        Ok(iter) => iter,
        // Failing to read the root directory is an error; an unreadable nested directory during a
        // recursive listing is skipped so the rest of the tree is still reported.
        Err(error) if depth == 1 => return Err(map_io_error("list", dir, error)),
        Err(_) => return Ok(()),
    };

    if depth == 1
        && walk.max_depth == 1
        && walk.sort == SortField::Name
        && !platform::IS_WINDOWS
        && walk.own_dirs.is_empty()
    {
        return list_dir_shallow_by_name(dir, read_dir, walk);
    }

    struct ChildItem {
        name: String,
        path: PathBuf,
        meta: fs::Metadata,
        mtime: Option<SystemTime>,
        size: u64,
        kind: EntryKind,
        media: Option<(Option<Category>, Option<&'static str>)>,
    }

    let need_mtime_for_sort = walk.sort == SortField::Modified;
    let need_media_for_filter = walk.categories.is_some();
    let mut children: Vec<ChildItem> = Vec::new();
    for item in read_dir {
        *walk.visited = walk.visited.saturating_add(1);
        if walk.visited.is_multiple_of(32) && walk.cancel.is_cancelled() {
            return Err(cancelled_error("fs.list"));
        }
        if depth > 1 && walk.hit_recursive_budget() {
            *walk.stopped_early = true;
            return Ok(());
        }
        let Ok(entry) = item else {
            continue;
        };
        let child_path = entry.path();
        if walk.own_dirs.is_blocked(&child_path) {
            walk.skipped.push(path_to_string(&child_path));
            continue;
        }
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
        let media = if need_media_for_filter {
            let pair = if kind == EntryKind::File {
                let info = classify_path(&child_path);
                (Some(info.category), Some(info.mime))
            } else {
                (None, None)
            };
            if let Some(wanted) = walk.categories
                && kind != EntryKind::Dir
                && !pair.0.is_some_and(|cat| wanted.contains(&cat))
            {
                continue;
            }
            Some(pair)
        } else {
            None
        };
        let mtime = need_mtime_for_sort.then(|| meta.modified().ok()).flatten();
        let size = meta.len();
        children.push(ChildItem {
            name,
            path: child_path,
            meta,
            mtime,
            size,
            kind,
            media,
        });
    }

    let sort = walk.sort;
    let order = walk.order;
    let cmp_children = |a: &ChildItem, b: &ChildItem| {
        let a_rank = usize::from(a.kind != EntryKind::Dir);
        let b_rank = usize::from(b.kind != EntryKind::Dir);
        a_rank
            .cmp(&b_rank)
            .then_with(|| {
                compare_by_sort(
                    sort, order, &a.name, &b.name, a.mtime, b.mtime, a.size, b.size,
                )
            })
            .then_with(|| a.path.cmp(&b.path))
    };

    if depth == 1 && walk.max_depth == 1 {
        *walk.total = children.len();
        let page = select_and_sort_page(&mut children, walk.offset, walk.limit, cmp_children);
        for child in page {
            if walk.cancel.is_cancelled() {
                return Err(cancelled_error("fs.list"));
            }
            if !push_list_entry(
                walk,
                std::mem::take(&mut child.name),
                &child.path,
                &child.meta,
                child.kind,
                child.media.take(),
            ) {
                break;
            }
        }
        return Ok(());
    }

    children.sort_by(cmp_children);

    for child in children {
        let ChildItem {
            name,
            path: child_path,
            meta,
            mtime: _,
            size: _,
            kind,
            media,
        } = child;
        if walk.cancel.is_cancelled() {
            return Err(cancelled_error("fs.list"));
        }
        if *walk.stopped_early {
            return Ok(());
        }
        if walk.max_depth > 1 && walk.hit_recursive_budget() {
            *walk.stopped_early = true;
            return Ok(());
        }
        let index = *walk.total;
        *walk.total = walk.total.saturating_add(1);

        if index >= walk.offset && !*walk.page_full {
            let _ = push_list_entry(walk, name, &child_path, &meta, kind, media);
        }

        if kind == EntryKind::Dir
            && depth < walk.max_depth
            && platform::same_device(&walk.root_device, &child_path, &meta)
        {
            list_dir(&child_path, depth + 1, walk)?;
            if *walk.stopped_early {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<Category>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
}

async fn stat(ctx: &Ctx, args: StatArgs) -> Result<StatResult, ToolError> {
    let path = check_fs_path(ctx.grant(), &args.path, Access::Read)?;
    run_blocking(ctx, "fs.stat", move |_cancel, _progress| {
        let meta =
            fs::symlink_metadata(&path).map_err(|error| map_io_error("stat", &path, error))?;
        let kind = EntryKind::from_file_type(meta.file_type());
        let target = if kind == EntryKind::Symlink {
            fs::read_link(&path).ok().map(|link| path_to_string(&link))
        } else {
            None
        };
        let (category, mime) = if kind == EntryKind::File {
            let info = classify_path(&path);
            (Some(info.category), Some(info.mime.to_owned()))
        } else {
            (None, None)
        };
        Ok(StatResult {
            path: path_to_string(&path),
            kind,
            size: meta.len(),
            mtime: format_mtime(&meta),
            mode: platform::unix_mode(&meta),
            readonly: meta.permissions().readonly(),
            target,
            category,
            mime,
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
    let path = check_fs_path(ctx.grant(), &args.path, Access::Read)?;
    let offset = args.offset;
    let length = usize::try_from(args.length).unwrap_or(usize::MAX);
    let requested_encoding = args.encoding;

    run_blocking(ctx, "fs.read", move |cancel, _progress| {
        let (mut file, meta) =
            super::open_regular_file(&path, "read", "Use fs.list to inspect a directory.")?;
        let size = meta.len();
        let base64_raw_limit = max_base64_raw_bytes(&path, size, offset);
        let effective_length = if requested_encoding == Some(Encoding::Base64) {
            length.min(base64_raw_limit)
        } else {
            length
        };
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| map_io_error("seek", &path, error))?;
        }

        // Read up to effective_length + 1 bytes so we know definitively whether more bytes remain
        // past the requested slice.
        let want = effective_length.saturating_add(1);
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

        let reached_eof = raw.len() <= effective_length;
        let mut peek = [0u8; 3];
        let mut peek_len = 0usize;
        if !reached_eof {
            peek[0] = raw[effective_length];
            peek_len = 1;
            raw.truncate(effective_length);
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
            base64_raw_limit,
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

/// Maximum raw bytes that fit within [`MAX_REPLY_BYTES`] once base64-encoded inside a
/// [`ReadResult`] JSON object. Always a multiple of 3 so base64 emits no wasted padding.
fn max_base64_raw_bytes(path: &Path, size: u64, offset: u64) -> usize {
    let envelope = serde_json::to_vec(&ReadResult {
        path: path_to_string(path),
        size,
        offset,
        length: MAX_READ_LENGTH,
        encoding: Encoding::Base64,
        content: String::new(),
        eof: false,
        hash: Some(format!("blake3:{}", "0".repeat(64))),
    })
    .map_or(512, |v| v.len());
    let encoded_budget = MAX_REPLY_BYTES.saturating_sub(envelope);
    (encoded_budget / 4) * 3
}

/// Decodes a read byte slice according to `requested`, trimming an incomplete trailing UTF-8
/// character at a chunk boundary when `peek` confirms it completes cleanly and capping base64
/// slices to `base64_raw_limit` so the encoded JSON reply stays within [`MAX_REPLY_BYTES`].
fn decode_chunk(
    path: &Path,
    chunk: &[u8],
    requested: Option<Encoding>,
    reached_eof: bool,
    peek: &[u8],
    base64_raw_limit: usize,
) -> Result<DecodedChunk, ToolError> {
    match requested {
        Some(Encoding::Base64) => {
            let take = chunk.len().min(base64_raw_limit);
            Ok(DecodedChunk {
                encoding: Encoding::Base64,
                content: BASE64.encode(&chunk[..take]),
                used: take,
                eof: reached_eof && take == chunk.len(),
            })
        }
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
            Err(_) => {
                let take = chunk.len().min(base64_raw_limit);
                Ok(DecodedChunk {
                    encoding: Encoding::Base64,
                    content: BASE64.encode(&chunk[..take]),
                    used: take,
                    eof: reached_eof && take == chunk.len(),
                })
            }
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
    let path = check_fs_path(ctx.grant(), &args.path, Access::Read)?;
    run_blocking(ctx, "fs.hash", move |cancel, mut progress| {
        let (mut file, meta) =
            super::open_regular_file(&path, "hash", "Pass a file path to fs.hash.")?;
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
        let decoded =
            decode_chunk(Path::new("/x"), &bytes[..4], None, false, &bytes[4..], 1024).unwrap();
        assert_eq!(decoded.encoding, Encoding::Utf8);
        assert_eq!(decoded.content, "hi");
        assert_eq!(decoded.used, 2);
        assert!(!decoded.eof);

        // Next page starting at offset 2 finishes the file cleanly.
        let rest = decode_chunk(Path::new("/x"), &bytes[2..], None, true, &[], 1024).unwrap();
        assert_eq!(rest.encoding, Encoding::Utf8);
        assert_eq!(rest.content, "€!");
        assert_eq!(rest.used, 4);
        assert!(rest.eof);
    }

    #[test]
    fn decode_chunk_falls_back_to_base64_for_invalid_bytes_even_at_end() {
        let bytes = [b'a', b'b', 0xE2, 0x82, 0x00];
        let decoded =
            decode_chunk(Path::new("/x"), &bytes[..4], None, false, &bytes[4..], 1024).unwrap();
        assert_eq!(decoded.encoding, Encoding::Base64);
        assert_eq!(decoded.used, 4);
        assert_eq!(BASE64.decode(&decoded.content).unwrap(), &bytes[..4]);

        // Truncated UTF-8 at true EOF is also invalid UTF-8 -> base64 when auto-detecting.
        let eof_bad = decode_chunk(Path::new("/x"), &bytes[..4], None, true, &[], 1024).unwrap();
        assert_eq!(eof_bad.encoding, Encoding::Base64);
        assert!(eof_bad.eof);

        // Tight base64 raw limit truncates the fallback slice and clears `eof`.
        let capped = decode_chunk(Path::new("/x"), &bytes[..4], None, true, &[], 3).unwrap();
        assert_eq!(capped.encoding, Encoding::Base64);
        assert_eq!(capped.used, 3);
        assert!(!capped.eof);
        assert_eq!(BASE64.decode(&capped.content).unwrap(), &bytes[..3]);
    }

    #[test]
    fn decode_chunk_rejects_invalid_utf8_when_utf8_was_requested() {
        let err = decode_chunk(
            Path::new("/x"),
            &[0xFF, 0xFE],
            Some(Encoding::Utf8),
            true,
            &[],
            1024,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(err.hint.is_some());
    }

    #[test]
    fn select_and_sort_page_matches_full_sort_across_every_slice() {
        let base: Vec<i32> = (0..100).map(|i| (i * 37) % 100).collect();
        let mut full = base.clone();
        full.sort();

        for (offset, limit) in [(0, 10), (10, 15), (90, 20), (100, 10), (0, 0), (0, 200)] {
            let mut copy = base.clone();
            let got = select_and_sort_page(&mut copy, offset, limit, Ord::cmp).to_vec();
            let expected: Vec<i32> = full.iter().copied().skip(offset).take(limit).collect();
            assert_eq!(got, expected, "offset={offset} limit={limit}");
        }
    }

    #[test]
    fn list_dir_shallow_fast_path_matches_general_path_across_pages_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["dir10", "dir2", "AlphaDir", ".hidden_dir"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        let files = [
            "file1.jpg",
            "file2.png",
            "file02.jpg",
            "file10.txt",
            "Beta.md",
            "alpha.mp4",
            "Über2.txt",
            "über10.jpg",
            ".hidden.jpg",
            ".dotfile",
        ];
        for (i, name) in files.iter().enumerate() {
            fs::write(root.join(name), vec![b'x'; (i + 1) * 7]).unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("file1.jpg"), root.join("link.jpg")).unwrap();

        let run_walk = |sort: SortField,
                        order: SortOrder,
                        hidden: bool,
                        categories: Option<&[Category]>,
                        offset: usize,
                        limit: usize|
         -> (Vec<(String, EntryKind, u64)>, usize) {
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let own_dirs = OwnDirFilter::allow_all_for_test();
            let cancel = CancellationToken::new();
            let mut progress = Progress::for_test(tx);
            let mut entries = Vec::new();
            let mut skipped = Vec::new();
            let mut total = 0usize;
            let mut visited = 0usize;
            let mut page_full = false;
            let mut stopped_early = false;
            let mut state = ListWalk {
                max_depth: 1,
                include_hidden: hidden,
                limit,
                offset,
                sort,
                order,
                categories,
                root_device: None,
                count_cap: None,
                visit_cap: None,
                deadline: None,
                reply_bytes: 112,
                own_dirs: &own_dirs,
                cancel: &cancel,
                progress: &mut progress,
                entries: &mut entries,
                skipped: &mut skipped,
                total: &mut total,
                visited: &mut visited,
                page_full: &mut page_full,
                stopped_early: &mut stopped_early,
            };
            list_dir(root, 1, &mut state).unwrap();
            let summary = entries
                .into_iter()
                .map(|e| (e.name, e.kind, e.size))
                .collect();
            (summary, total)
        };

        let img_cats = [Category::Image];
        for order in [SortOrder::Asc, SortOrder::Desc] {
            for hidden in [true, false] {
                for cats in [None, Some(img_cats.as_slice())] {
                    let (full, full_total) = run_walk(SortField::Name, order, hidden, cats, 0, 100);
                    assert_eq!(full.len(), full_total);
                    // Verify paging slices reconstruct the exact full listing.
                    let mut paged = Vec::new();
                    let mut off = 0;
                    while off < full_total {
                        let (chunk, total) = run_walk(SortField::Name, order, hidden, cats, off, 3);
                        assert_eq!(total, full_total);
                        paged.extend(chunk);
                        off += 3;
                    }
                    assert_eq!(paged, full);

                    // Verify past-end offset returns empty with exact total.
                    let (empty, total) =
                        run_walk(SortField::Name, order, hidden, cats, full_total + 5, 3);
                    assert!(empty.is_empty());
                    assert_eq!(total, full_total);
                }
            }
        }
    }
}
