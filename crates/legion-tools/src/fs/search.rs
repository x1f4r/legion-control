//! Search tools: `fs.find` and `fs.grep`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use legion_core::media::{Category, classify_path};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Registry, ToolError};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    EntryKind, MAX_REPLY_BYTES, OwnDirFilter, Progress, SortField, SortOrder, cancelled_error,
    check_fs_path, compare_by_sort, format_mtime, map_io_error, path_to_string, platform,
    run_blocking,
};

const DEFAULT_FIND_LIMIT: u32 = 200;
const MAX_FIND_LIMIT: u32 = 5000;
const DEFAULT_FIND_BUDGET_MS: u64 = 10_000;
const MAX_FIND_BUDGET_MS: u64 = 60_000;
const DEFAULT_GREP_LIMIT: u32 = 100;
const MAX_GREP_LIMIT: u32 = 1000;
const MAX_GREP_CONTEXT: u32 = 5;
const BINARY_PROBE_BYTES: usize = 8 * 1024;
const MAX_LINE_SCAN_BYTES: usize = 1024 * 1024;
const MAX_DISPLAY_CHARS: usize = 500;
const TRUNCATION_MARKER: &str = "... [truncated]";
/// Conservative per-line headroom reserved for pending `after` context lines in `fs.grep`.
const GREP_AFTER_LINE_RESERVE: usize = 600;

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.find",
        Effect::Read,
        "Find files and directories under a path matching optional name words, glob, kind, category, size, modification time and depth filters within a time budget, with optional sorting and separate hidden and .gitignore controls. Unsorted searches stream in directory-entry order so huge directories cut off promptly within budget_ms; pass `sort` when deterministic ordering is needed. With `on: \"all\"`, searches every machine in the fleet concurrently and reports unreachable machines per key.",
        find,
    );
    registry.add_fn(
        "fs.grep",
        Effect::Read,
        "Search text files for a regular expression with optional glob filter and context lines, skipping binary files and truncating very long lines.",
        grep,
    );
}

/// A compiled glob filter that matches relative paths when the pattern contains a path separator
/// and file names otherwise.
struct GlobFilter {
    matcher: GlobMatcher,
    match_full_relative: bool,
}

impl GlobFilter {
    fn compile(pattern: &str) -> Result<GlobFilter, ToolError> {
        let normalized = pattern.replace('\\', "/");
        let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
        if normalized.is_empty() {
            return Err(
                ErrorBody::new(ErrorCode::InvalidArgs, "glob pattern must not be empty.")
                    .with_hint(
                        "Omit glob to match every entry, or write a pattern such as \"**/*.rs\".",
                    ),
            );
        }
        let match_full_relative = normalized.contains('/');
        let glob = GlobBuilder::new(normalized)
            .literal_separator(true)
            .build()
            .map_err(|error| {
                ErrorBody::new(
                    ErrorCode::InvalidArgs,
                    format!("Invalid glob pattern {pattern:?}: {error}."),
                )
                .with_hint("Use standard glob syntax such as \"*.rs\" or \"src/**/*.toml\".")
            })?;
        Ok(GlobFilter {
            matcher: glob.compile_matcher(),
            match_full_relative,
        })
    }

    fn matches(&self, root: &Path, candidate: &Path) -> bool {
        if self.match_full_relative {
            let relative = candidate.strip_prefix(root).unwrap_or(candidate);
            self.matcher.is_match(relative)
        } else {
            candidate
                .file_name()
                .is_some_and(|name| self.matcher.is_match(name))
        }
    }
}

/// Configures `ignore::WalkBuilder` consistently for `fs.find` and `fs.grep`: never follows
/// symbolic links, stays on one file system unless `cross_filesystems` is set, optionally sorts
/// entries per directory, skips Legion's own configuration and state directories for non-admin
/// roles, keeps hidden-file filtering separate from `.gitignore` / `.ignore`, and respects
/// `.gitignore` even outside a `.git` repository when `respect_ignore` is true.
#[allow(clippy::too_many_arguments)]
fn build_walker(
    root: &Path,
    respect_ignore: bool,
    include_hidden: bool,
    sort_per_dir: bool,
    max_depth: Option<usize>,
    cross_filesystems: bool,
    own_dirs: OwnDirFilter,
    skipped: Arc<Mutex<Vec<String>>>,
) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.follow_links(false);
    builder.same_file_system(!cross_filesystems);
    if sort_per_dir {
        builder.sort_by_file_name(|a, b| a.cmp(b));
    }
    builder.standard_filters(false);
    builder.hidden(!include_hidden);
    builder.parents(respect_ignore);
    builder.ignore(respect_ignore);
    builder.git_ignore(respect_ignore);
    builder.git_global(respect_ignore);
    builder.git_exclude(respect_ignore);
    if respect_ignore {
        builder.require_git(false);
    }
    builder.max_depth(max_depth);
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        if own_dirs.is_blocked(entry.path()) {
            let mut list = skipped
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            list.push(path_to_string(entry.path()));
            return false;
        }
        // On Windows, `ignore`'s built-in hidden filter only checks leading dots; honor the
        // Windows FILE_ATTRIBUTE_HIDDEN bit too so hidden directories are pruned consistently.
        !(!include_hidden
            && platform::IS_WINDOWS
            && entry
                .metadata()
                .ok()
                .is_some_and(|meta| platform::is_hidden_attr(&meta)))
    });
    builder
}

/// Why `fs.find` stopped before visiting the entire tree or returning every match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum TruncateReason {
    /// The search reached `budget_ms`.
    Time,
    /// The search reached `limit` or the 1 MiB reply byte budget.
    Limit,
}

/// Arguments of `fs.find`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    /// Root directory to search (absolute or starting with `~`).
    path: String,
    /// Optional space-separated words that must all appear in the entry name (case-insensitive,
    /// Unicode-aware).
    #[serde(default)]
    name: Option<String>,
    /// Optional glob pattern (for example `"**/*.rs"` or `"*.toml"`).
    #[serde(default)]
    glob: Option<String>,
    /// Optional entry kind filter (`file`, `dir`, `symlink` or `other`).
    #[serde(default)]
    kind: Option<EntryKind>,
    /// Optional list of regular-file categories to match (`image`, `video`, `audio`, `document`,
    /// `text`, `archive`, `other`).
    #[serde(default)]
    categories: Option<Vec<Category>>,
    /// Minimum size in bytes (inclusive). Matches regular files unless `kind` selects another kind.
    #[serde(default)]
    min_size: Option<u64>,
    /// Maximum size in bytes (inclusive). Matches regular files unless `kind` selects another kind.
    #[serde(default)]
    max_size: Option<u64>,
    /// Only include entries modified at or after this RFC 3339 timestamp (for example
    /// `"2026-11-24T02:40:18.942Z"`).
    #[serde(default)]
    modified_after: Option<String>,
    /// Only include entries modified at or before this RFC 3339 timestamp (for example
    /// `"2026-11-24T02:40:18.942Z"`).
    #[serde(default)]
    modified_before: Option<String>,
    /// Optional field to sort matches by (`name`, `modified`, or `size`).
    #[serde(default)]
    sort: Option<SortField>,
    /// Sort direction (`asc` or `desc`); defaults to `asc` when `sort` is set, and sorts by
    /// `name` when given without `sort`.
    #[serde(default)]
    order: Option<SortOrder>,
    /// Maximum directory depth to descend (1 means direct children only).
    #[serde(default)]
    #[schemars(range(min = 1))]
    max_depth: Option<u32>,
    /// Maximum number of matches to return, from 1 to 5000.
    #[serde(default = "default_find_limit")]
    #[schemars(range(min = 1, max = 5000))]
    limit: u32,
    /// Respect `.gitignore` and `.ignore` rules.
    #[serde(default = "default_true")]
    ignore: bool,
    /// Include hidden files and directories (dotfiles, and Windows hidden entries). Defaults to
    /// `false` when `ignore` is `true`, and `true` when `ignore` is `false`.
    #[serde(default)]
    hidden: Option<bool>,
    /// Time budget for the walk in milliseconds, from 1 to 60000 (default 10000).
    #[serde(default = "default_find_budget_ms")]
    #[schemars(range(min = 1, max = 60000))]
    budget_ms: u64,
    /// Descend into directories mounted from other file systems (false stays on one file system).
    #[serde(default)]
    cross_filesystems: bool,
}

fn default_find_limit() -> u32 {
    DEFAULT_FIND_LIMIT
}

fn default_find_budget_ms() -> u64 {
    DEFAULT_FIND_BUDGET_MS
}

fn default_true() -> bool {
    true
}

/// One entry returned by `fs.find`.
#[derive(Debug, Serialize)]
struct FindMatch {
    path: String,
    kind: EntryKind,
    size: u64,
    mtime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<Category>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime: Option<String>,
}

/// Result of `fs.find`.
#[derive(Debug, Serialize)]
struct FindResult {
    machine: String,
    matches: Vec<FindMatch>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<TruncateReason>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<String>,
}

/// Parses an RFC 3339 timestamp argument for `modified_after` / `modified_before`.
fn parse_timestamp_arg(field: &str, raw: &str) -> Result<jiff::Timestamp, ErrorBody> {
    raw.trim().parse::<jiff::Timestamp>().map_err(|error| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Invalid {field} timestamp {raw:?}: {error}."),
        )
        .with_hint(
            "Use an RFC 3339 UTC timestamp such as \"2026-11-24T02:40:18.942Z\", the same format fs.stat and fs.list return.",
        )
    })
}

/// Candidate kept in the bounded top-K heap when `fs.find` is asked to sort matches.
struct SortedCandidate {
    found: FindMatch,
    name: String,
    mtime_raw: Option<SystemTime>,
    sort: SortField,
    order: SortOrder,
}

impl SortedCandidate {
    /// Desired presentation order (`Less` means this candidate should appear earlier).
    fn cmp_desired(&self, other: &Self) -> Ordering {
        compare_by_sort(
            self.sort,
            self.order,
            &self.name,
            &other.name,
            self.mtime_raw,
            other.mtime_raw,
            self.found.size,
            other.found.size,
        )
        .then_with(|| self.found.path.cmp(&other.found.path))
    }
}

impl PartialEq for SortedCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.cmp_desired(other) == Ordering::Equal
    }
}

impl Eq for SortedCandidate {}

/// Orders the *worst* candidate (the one that sorts last in the requested order) as `Greater` so
/// `BinaryHeap::peek()` always points to the item to evict when a better match arrives.
impl Ord for SortedCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_desired(other)
    }
}

impl PartialOrd for SortedCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Checks whether every whitespace-separated word in `words` appears in `name` (case-insensitive,
/// Unicode-aware).
fn name_matches_words(name: &str, lower_words: &[String]) -> bool {
    if lower_words.is_empty() {
        return true;
    }
    let lowered = name.to_lowercase();
    lower_words
        .iter()
        .all(|word| lowered.contains(word.as_str()))
}

async fn find(ctx: &Ctx, args: FindArgs) -> Result<FindResult, ToolError> {
    if !(1..=MAX_FIND_LIMIT).contains(&args.limit) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.find limit must be between 1 and {MAX_FIND_LIMIT}, and {} was given.",
                args.limit
            ),
        ));
    }
    if !(1..=MAX_FIND_BUDGET_MS).contains(&args.budget_ms) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.find budget_ms must be between 1 and {MAX_FIND_BUDGET_MS}, and {} was given.",
                args.budget_ms
            ),
        )
        .with_hint("Pass a budget_ms between 1 and 60000 milliseconds."));
    }
    if args.max_depth == Some(0) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "fs.find max_depth must be at least 1.",
        )
        .with_hint(
            "Use max_depth: 1 for direct children, or omit max_depth for unlimited depth.",
        ));
    }
    if let (Some(min), Some(max)) = (args.min_size, args.max_size)
        && min > max
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("fs.find min_size ({min}) is greater than max_size ({max})."),
        )
        .with_hint("Set min_size to a value less than or equal to max_size."));
    }
    let modified_after = args
        .modified_after
        .as_deref()
        .map(|raw| parse_timestamp_arg("modified_after", raw))
        .transpose()?;
    let modified_before = args
        .modified_before
        .as_deref()
        .map(|raw| parse_timestamp_arg("modified_before", raw))
        .transpose()?;
    if let (Some(after), Some(before)) = (modified_after, modified_before)
        && after > before
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "fs.find modified_after is later than modified_before.",
        )
        .with_hint("Set modified_after to a timestamp at or before modified_before."));
    }
    let glob = args.glob.as_deref().map(GlobFilter::compile).transpose()?;
    let name_words: Vec<String> = args
        .name
        .as_deref()
        .map(|text| {
            text.split_whitespace()
                .map(str::to_lowercase)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let sort_spec: Option<(SortField, SortOrder)> = match (args.sort, args.order) {
        (Some(field), order) => Some((field, order.unwrap_or_default())),
        (None, Some(order)) => Some((SortField::Name, order)),
        (None, None) => None,
    };

    let root = check_fs_path(ctx.grant(), &args.path, Access::Read)?;
    let own_dirs = OwnDirFilter::for_ctx(ctx);
    let machine = ctx.node_name().to_owned();
    let kind_filter = args.kind;
    let categories = args.categories;
    let min_size = args.min_size;
    let max_size = args.max_size;
    let max_depth = args
        .max_depth
        .map(|d| usize::try_from(d).unwrap_or(usize::MAX));
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let respect_ignore = args.ignore;
    let include_hidden = args.hidden.unwrap_or(!args.ignore);
    let budget = Duration::from_millis(args.budget_ms);
    let cross_filesystems = args.cross_filesystems;

    run_blocking(ctx, "fs.find", move |cancel, mut progress| {
        let meta = fs::metadata(&root).map_err(|error| map_io_error("search", &root, error))?;
        if !meta.is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("{} is not a directory.", root.display()),
            )
            .with_hint("Pass a directory to fs.find."));
        }

        let deadline = Instant::now() + budget;
        let mut iterations: u64 = 0;
        let mut visited: u64 = 0;
        let mut reason: Option<TruncateReason> = None;
        let skipped_buf = Arc::new(Mutex::new(Vec::new()));
        let envelope_bytes = serde_json::to_vec(&FindResult {
            machine: machine.clone(),
            matches: Vec::new(),
            truncated: true,
            reason: Some(TruncateReason::Limit),
            skipped: Vec::new(),
        })
        .map_or(128, |v| v.len());

        let mut unsorted_matches: Vec<FindMatch> = Vec::new();
        let mut reply_bytes = envelope_bytes;
        let mut top_heap: BinaryHeap<SortedCandidate> = BinaryHeap::new();
        let mut overflowed_limit = false;

        // Unsorted walks deliberately stream entries lazily without `sort_by_file_name`, because
        // buffering and sorting a 200,000-entry directory before yielding entry #1 would blow past
        // short `budget_ms` limits. Pass `sort` for deterministic ordering.
        let walker = build_walker(
            &root,
            respect_ignore,
            include_hidden,
            false,
            max_depth,
            cross_filesystems,
            own_dirs,
            Arc::clone(&skipped_buf),
        )
        .build();

        for item in walker {
            // Check cancellation and the time budget on EVERY iteration — including entries that
            // return I/O errors — so a tree full of unreadable directories cannot escape the budget.
            if iterations.is_multiple_of(32) {
                if cancel.is_cancelled() {
                    return Err(cancelled_error("fs.find"));
                }
                if Instant::now() >= deadline {
                    reason = Some(TruncateReason::Time);
                    break;
                }
            }
            iterations = iterations.saturating_add(1);
            let Ok(entry) = item else {
                continue;
            };
            if entry.depth() == 0 {
                continue;
            }
            visited += 1;
            let matched_count = if sort_spec.is_some() {
                top_heap.len()
            } else {
                unsorted_matches.len()
            };
            progress.maybe_emit(|| json!({"visited": visited, "matches": matched_count}));

            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let kind = EntryKind::from_file_type(file_type);
            if kind_filter.is_some_and(|want| kind != want) {
                continue;
            }
            if (min_size.is_some() || max_size.is_some())
                && kind_filter.is_none()
                && kind != EntryKind::File
            {
                continue;
            }
            let entry_path = entry.path();
            let (category, mime) = if kind == EntryKind::File {
                let info = classify_path(entry_path);
                (Some(info.category), Some(info.mime))
            } else {
                (None, None)
            };
            if let Some(wanted) = categories.as_deref()
                && !category.is_some_and(|cat| wanted.contains(&cat))
            {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy();
            if !name_words.is_empty() && !name_matches_words(&file_name, &name_words) {
                continue;
            }
            if glob
                .as_ref()
                .is_some_and(|filter| !filter.matches(&root, entry_path))
            {
                continue;
            }

            let Ok(entry_meta) = fs::symlink_metadata(entry_path) else {
                continue;
            };
            let size = entry_meta.len();
            if min_size.is_some_and(|min| size < min) || max_size.is_some_and(|max| size > max) {
                continue;
            }
            let mtime_raw = entry_meta.modified().ok();
            if modified_after.is_some() || modified_before.is_some() {
                let Some(ts) = mtime_raw.and_then(|t| jiff::Timestamp::try_from(t).ok()) else {
                    continue;
                };
                if modified_after.is_some_and(|after| ts < after)
                    || modified_before.is_some_and(|before| ts > before)
                {
                    continue;
                }
            }

            let found = FindMatch {
                path: path_to_string(entry_path),
                kind,
                size,
                mtime: format_mtime(&entry_meta),
                category,
                mime: mime.map(str::to_owned),
            };

            if let Some((sort_field, sort_order)) = sort_spec {
                let candidate = SortedCandidate {
                    found,
                    name: file_name.into_owned(),
                    mtime_raw,
                    sort: sort_field,
                    order: sort_order,
                };
                if top_heap.len() < limit {
                    top_heap.push(candidate);
                } else {
                    overflowed_limit = true;
                    if let Some(worst) = top_heap.peek()
                        && candidate.cmp_desired(worst) == Ordering::Less
                    {
                        top_heap.pop();
                        top_heap.push(candidate);
                    }
                }
            } else {
                if unsorted_matches.len() == limit {
                    reason = Some(TruncateReason::Limit);
                    break;
                }
                let item_bytes = serde_json::to_vec(&found).map_or(0, |v| v.len())
                    + usize::from(!unsorted_matches.is_empty());
                if !unsorted_matches.is_empty()
                    && reply_bytes.saturating_add(item_bytes) > MAX_REPLY_BYTES
                {
                    reason = Some(TruncateReason::Limit);
                    break;
                }
                reply_bytes = reply_bytes.saturating_add(item_bytes);
                unsorted_matches.push(found);
            }
        }

        let matches = if sort_spec.is_some() {
            if reason.is_none() && overflowed_limit {
                reason = Some(TruncateReason::Limit);
            }
            let mut sorted: Vec<SortedCandidate> = top_heap.into_vec();
            sorted.sort_by(SortedCandidate::cmp_desired);
            let mut out = Vec::with_capacity(sorted.len());
            let mut used_bytes = envelope_bytes;
            for item in sorted {
                let item_bytes = serde_json::to_vec(&item.found).map_or(0, |v| v.len())
                    + usize::from(!out.is_empty());
                if !out.is_empty() && used_bytes.saturating_add(item_bytes) > MAX_REPLY_BYTES {
                    if reason.is_none() {
                        reason = Some(TruncateReason::Limit);
                    }
                    break;
                }
                used_bytes = used_bytes.saturating_add(item_bytes);
                out.push(item.found);
            }
            out
        } else {
            unsorted_matches
        };

        let mut skipped = skipped_buf
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        skipped.sort();
        let truncated = reason.is_some();
        Ok(FindResult {
            machine,
            matches,
            truncated,
            reason,
            skipped,
        })
    })
    .await
}

/// Arguments of `fs.grep`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    /// File or directory to search (absolute or starting with `~`).
    path: String,
    /// Regular expression pattern to search for.
    pattern: String,
    /// Optional glob filter restricting which files are searched (for example `"**/*.rs"`).
    #[serde(default)]
    glob: Option<String>,
    /// Match without distinguishing ASCII/Unicode letter case.
    #[serde(default)]
    ignore_case: bool,
    /// Number of surrounding lines to include before and after each match, from 0 to 5.
    #[serde(default)]
    #[schemars(range(min = 0, max = 5))]
    context: u32,
    /// Maximum number of matches to return, from 1 to 1000.
    #[serde(default = "default_grep_limit")]
    #[schemars(range(min = 1, max = 1000))]
    limit: u32,
    /// Respect `.gitignore` / `.ignore` rules and skip hidden files when searching a directory.
    #[serde(default = "default_true")]
    ignore: bool,
    /// Descend into directories mounted from other file systems (false stays on one file system).
    #[serde(default)]
    cross_filesystems: bool,
}

fn default_grep_limit() -> u32 {
    DEFAULT_GREP_LIMIT
}

/// One matching line returned by `fs.grep`.
#[derive(Debug, Serialize)]
struct GrepMatch {
    path: String,
    line: u64,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    before: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<Vec<String>>,
}

/// Result of `fs.grep`.
#[derive(Debug, Serialize)]
struct GrepResult {
    matches: Vec<GrepMatch>,
    files_searched: u64,
    truncated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<String>,
}

async fn grep(ctx: &Ctx, args: GrepArgs) -> Result<GrepResult, ToolError> {
    if args.context > MAX_GREP_CONTEXT {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.grep context is at most {MAX_GREP_CONTEXT} lines, and {} were asked for.",
                args.context
            ),
        ));
    }
    if !(1..=MAX_GREP_LIMIT).contains(&args.limit) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.grep limit must be between 1 and {MAX_GREP_LIMIT}, and {} was given.",
                args.limit
            ),
        ));
    }
    let regex = RegexBuilder::new(&args.pattern)
        .case_insensitive(args.ignore_case)
        .build()
        .map_err(|error| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!("Invalid regular expression {:?}: {error}.", args.pattern),
            )
            .with_hint("Check the regex syntax; escape characters such as ., (, or [ with \\.")
        })?;
    let glob = args.glob.as_deref().map(GlobFilter::compile).transpose()?;
    let root = check_fs_path(ctx.grant(), &args.path, Access::Read)?;
    let own_dirs = OwnDirFilter::for_ctx(ctx);
    let context = usize::try_from(args.context).unwrap_or(0);
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let respect_ignore = args.ignore;
    let cross_filesystems = args.cross_filesystems;

    run_blocking(ctx, "fs.grep", move |cancel, mut progress| {
        let meta = fs::metadata(&root).map_err(|error| map_io_error("search", &root, error))?;
        let skipped_buf = Arc::new(Mutex::new(Vec::new()));
        let reply_bytes = serde_json::to_vec(&GrepResult {
            matches: Vec::new(),
            files_searched: u64::MAX,
            truncated: false,
            skipped: Vec::new(),
        })
        .map_or(96, |v| v.len());
        let mut state = GrepState {
            regex: &regex,
            context,
            limit,
            reply_bytes,
            cancel: &cancel,
            progress: &mut progress,
            matches: Vec::new(),
            files_searched: 0,
            truncated: false,
        };

        if meta.is_file() {
            if glob
                .as_ref()
                .is_none_or(|filter| filter.matches(root.parent().unwrap_or(&root), &root))
            {
                grep_file(&root, true, &mut state)?;
            }
        } else if meta.is_dir() {
            for item in build_walker(
                &root,
                respect_ignore,
                !respect_ignore,
                true,
                None,
                cross_filesystems,
                own_dirs,
                Arc::clone(&skipped_buf),
            )
            .build()
            {
                if state.cancel.is_cancelled() {
                    return Err(cancelled_error("fs.grep"));
                }
                let Ok(entry) = item else {
                    continue;
                };
                if entry.depth() == 0 {
                    continue;
                }
                let Some(file_type) = entry.file_type() else {
                    continue;
                };
                // Never follow symbolic links during a directory grep so a confined role cannot
                // read files outside its allowed roots through a link inside the directory.
                if file_type.is_symlink() || !file_type.is_file() {
                    continue;
                }
                let file_path = entry.path();
                if glob
                    .as_ref()
                    .is_some_and(|filter| !filter.matches(&root, file_path))
                {
                    continue;
                }
                grep_file(file_path, false, &mut state)?;
                let searched = state.files_searched;
                let found = state.matches.len();
                state
                    .progress
                    .maybe_emit(|| json!({"files_searched": searched, "matches": found}));
                if state.truncated {
                    break;
                }
            }
        } else {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} is not a regular file or directory (it is a named pipe, socket or device).",
                    root.display()
                ),
            )
            .with_hint(
                "Pass a regular file or directory to fs.grep; special files are refused before opening so calls cannot block.",
            ));
        }

        let mut skipped = skipped_buf
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        skipped.sort();
        Ok(GrepResult {
            matches: state.matches,
            files_searched: state.files_searched,
            truncated: state.truncated,
            skipped,
        })
    })
    .await
}

struct GrepState<'a> {
    regex: &'a regex::Regex,
    context: usize,
    limit: usize,
    reply_bytes: usize,
    cancel: &'a CancellationToken,
    progress: &'a mut Progress,
    matches: Vec<GrepMatch>,
    files_searched: u64,
    truncated: bool,
}

fn grep_file(path: &Path, fail_on_open: bool, state: &mut GrepState<'_>) -> Result<(), ToolError> {
    if state.cancel.is_cancelled() {
        return Err(cancelled_error("fs.grep"));
    }
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) if fail_on_open => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                format!(
                    "{} is not a regular file (it is a named pipe, socket or device).",
                    path.display()
                ),
            )
            .with_hint("Pass a regular file or directory to fs.grep."));
        }
        Ok(_) => return Ok(()),
        Err(error) if fail_on_open => return Err(map_io_error("read", path, error)),
        Err(_) => return Ok(()),
    }
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if fail_on_open => return Err(map_io_error("read", path, error)),
        Err(_) => return Ok(()),
    };
    let mut reader = BufReader::with_capacity(BINARY_PROBE_BYTES, file);
    let probe = match reader.fill_buf() {
        Ok(bytes) => bytes,
        Err(error) if fail_on_open => return Err(map_io_error("read", path, error)),
        Err(_) => return Ok(()),
    };
    if is_binary_probe(probe) {
        return Ok(());
    }
    state.files_searched += 1;

    let path_str = path_to_string(path);
    let mut before: VecDeque<String> = VecDeque::with_capacity(state.context);
    let mut pending_after: Vec<(usize, usize)> = Vec::new();
    let mut raw = Vec::new();
    let mut line_no: u64 = 0;

    loop {
        if line_no.is_multiple_of(64) && state.cancel.is_cancelled() {
            return Err(cancelled_error("fs.grep"));
        }
        raw.clear();
        let line_read =
            match read_bounded_line(&mut reader, &mut raw, MAX_LINE_SCAN_BYTES, state.cancel) {
                Ok(Some(overflowed)) => overflowed,
                Ok(None) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        && state.cancel.is_cancelled() =>
                {
                    return Err(cancelled_error("fs.grep"));
                }
                Err(error) if fail_on_open => return Err(map_io_error("read", path, error)),
                Err(_) => break,
            };
        line_no += 1;

        let trimmed = strip_line_ending(&raw);
        let text = String::from_utf8_lossy(trimmed);
        let display = truncate_line(&text, MAX_DISPLAY_CHARS, line_read);

        for (match_idx, remaining) in &mut pending_after {
            if *remaining > 0 {
                if let Some(after) = state
                    .matches
                    .get_mut(*match_idx)
                    .and_then(|item| item.after.as_mut())
                {
                    let after_bytes = serde_json::to_vec(&display)
                        .map_or(display.len() + 2, |v| v.len())
                        + usize::from(!after.is_empty());
                    if state.reply_bytes.saturating_add(after_bytes) <= MAX_REPLY_BYTES {
                        state.reply_bytes = state.reply_bytes.saturating_add(after_bytes);
                        after.push(display.clone());
                    } else {
                        state.truncated = true;
                        *remaining = 0;
                        continue;
                    }
                }
                *remaining -= 1;
            }
        }
        pending_after.retain(|(_, remaining)| *remaining > 0);

        if state.regex.is_match(&text) {
            if !state.truncated && state.matches.len() < state.limit {
                let candidate = GrepMatch {
                    path: path_str.clone(),
                    line: line_no,
                    text: display.clone(),
                    before: (state.context > 0).then(|| before.iter().cloned().collect()),
                    after: (state.context > 0).then(Vec::new),
                };
                let item_bytes = serde_json::to_vec(&candidate).map_or(0, |v| v.len())
                    + usize::from(!state.matches.is_empty());
                let pending_slots = pending_after
                    .iter()
                    .map(|(_, rem)| *rem)
                    .sum::<usize>()
                    .saturating_add(state.context);
                let reserve = pending_slots.saturating_mul(GREP_AFTER_LINE_RESERVE);
                if !state.matches.is_empty()
                    && state
                        .reply_bytes
                        .saturating_add(item_bytes)
                        .saturating_add(reserve)
                        > MAX_REPLY_BYTES
                {
                    state.truncated = true;
                } else {
                    let idx = state.matches.len();
                    state.reply_bytes = state.reply_bytes.saturating_add(item_bytes);
                    state.matches.push(candidate);
                    if state.context > 0 {
                        pending_after.push((idx, state.context));
                    }
                }
            } else {
                state.truncated = true;
            }
        }

        if state.truncated && pending_after.is_empty() {
            break;
        }

        if state.context > 0 {
            if before.len() == state.context {
                before.pop_front();
            }
            before.push_back(display);
        }
    }

    Ok(())
}

/// Reads one line into `out` up to `max_bytes`, discarding any remainder until `\n` so a single
/// enormous line cannot exhaust memory. Returns `Ok(None)` at EOF, or `Ok(Some(overflowed))`.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> io::Result<Option<bool>> {
    let mut saw_any = false;
    let mut overflowed = false;
    loop {
        if cancel.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_any.then_some(overflowed));
        }
        saw_any = true;
        let (consume, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(pos) => (pos + 1, true),
            None => (available.len(), false),
        };
        if out.len() < max_bytes {
            let room = max_bytes - out.len();
            let take = consume.min(room);
            out.extend_from_slice(&available[..take]);
            if take < consume {
                overflowed = true;
            }
        } else {
            overflowed = true;
        }
        reader.consume(consume);
        if found_newline {
            return Ok(Some(overflowed));
        }
    }
}

fn is_binary_probe(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

fn strip_line_ending(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

/// Cuts a line longer than `max_chars` Unicode scalar values (or already cut at the byte cap) and
/// appends a visible marker.
fn truncate_line(line: &str, max_chars: usize, already_cut: bool) -> String {
    let mut chars = line.char_indices();
    if let Some((byte_idx, _)) = chars.nth(max_chars) {
        format!("{}{TRUNCATION_MARKER}", &line[..byte_idx])
    } else if already_cut {
        format!("{line}{TRUNCATION_MARKER}")
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_filter_matches_filenames_and_relative_paths() {
        let root = Path::new("/proj");
        let ext = GlobFilter::compile("*.rs").unwrap();
        assert!(ext.matches(root, Path::new("/proj/main.rs")));
        assert!(ext.matches(root, Path::new("/proj/src/fs/read.rs")));
        assert!(!ext.matches(root, Path::new("/proj/Cargo.toml")));

        let deep = GlobFilter::compile("**/*.rs").unwrap();
        assert!(deep.matches(root, Path::new("/proj/main.rs")));
        assert!(deep.matches(root, Path::new("/proj/src/fs/read.rs")));

        let scoped = GlobFilter::compile("src/*.rs").unwrap();
        assert!(scoped.matches(root, Path::new("/proj/src/lib.rs")));
        assert!(!scoped.matches(root, Path::new("/proj/src/fs/read.rs")));

        let bad = GlobFilter::compile("[unclosed").err().unwrap();
        assert_eq!(bad.code, ErrorCode::InvalidArgs);
    }

    #[test]
    fn truncate_line_cuts_at_character_boundary_and_marks_long_lines() {
        assert_eq!(truncate_line("short", 10, false), "short");
        assert_eq!(truncate_line("0123456789", 10, false), "0123456789");
        assert_eq!(
            truncate_line("0123456789extra", 10, false),
            "0123456789... [truncated]"
        );
        // Multi-byte UTF-8 characters are counted by char, never split mid-scalar.
        assert_eq!(truncate_line("€€€€€!", 5, false), "€€€€€... [truncated]");
        assert_eq!(truncate_line("abc", 10, true), "abc... [truncated]");
    }

    #[test]
    fn binary_probe_detects_nul_bytes() {
        assert!(!is_binary_probe(b"hello world\n"));
        assert!(is_binary_probe(b"ELF\x00binary"));
    }
}
