//! Path validation, platform compatibility checks, ignore matching, and conflict naming.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use legion_xfer::{EntryKind, Timestamp};

use crate::error::{Result, SyncError};
use crate::model::ProblemKind;
use crate::traits::FsCapabilities;

/// Reserved top-level directory holding sync temporary files, bin contents, and metadata.
pub const SYNC_META_DIR: &str = ".legion-sync";

/// Relative directory inside the sync folder for deleted and replaced copies.
pub const SYNC_BIN_DIR: &str = ".legion-sync/bin";

/// Relative directory inside the sync folder for in-flight staged downloads.
pub const SYNC_TMP_DIR: &str = ".legion-sync/tmp";

/// Returns `true` if `rel` is `.legion-sync` or sits inside `.legion-sync/` (matched
/// case-insensitively so Windows and macOS cannot collide with the metadata directory).
pub fn is_internal_sync_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    first.eq_ignore_ascii_case(SYNC_META_DIR)
}

/// Validates a relative path used in the live sync tree and returns its normalized forward-slash
/// form.
///
/// Refuses empty paths, `.`, `..`, leading or trailing slashes, empty segments (`//`),
/// backslashes, NUL bytes, and anything under `.legion-sync/`. Does *not* refuse platform-specific
/// names such as Windows-forbidden characters; those are checked separately against the local
/// filesystem's [`FsCapabilities`] so unsupported names are held back and reported as problems
/// rather than rejected globally.
pub fn validate_sync_path(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "empty path is not allowed".to_string(),
        });
    }
    if raw.as_bytes().contains(&0) {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "path contains a NUL byte".to_string(),
        });
    }
    if raw.starts_with('/') || raw.ends_with('/') {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "leading or trailing slash is not allowed".to_string(),
        });
    }
    if raw.contains('\\') {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "backslash separator is not allowed in sync paths".to_string(),
        });
    }

    let mut segments = Vec::new();
    for seg in raw.split('/') {
        if seg.is_empty() {
            return Err(SyncError::InvalidPath {
                path: raw.to_string(),
                reason: "empty path segment is not allowed".to_string(),
            });
        }
        if seg == "." || seg == ".." {
            return Err(SyncError::InvalidPath {
                path: raw.to_string(),
                reason: format!("path segment {seg:?} is not allowed"),
            });
        }
        segments.push(seg);
    }

    if segments.is_empty() {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "path has no segments".to_string(),
        });
    }

    if segments[0].eq_ignore_ascii_case(SYNC_META_DIR) {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "'.legion-sync' is reserved for sync metadata and never synced".to_string(),
        });
    }

    Ok(segments.join("/"))
}

/// Validates a flat filename used inside `.legion-sync/tmp/` or `.legion-sync/bin/`.
pub fn validate_flat_internal_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.as_bytes().contains(&0)
    {
        return Err(SyncError::InvalidPath {
            path: name.to_string(),
            reason: "internal filename must be a single non-empty path segment".to_string(),
        });
    }
    Ok(())
}

/// Returns all strict ancestor directory paths of `rel`, ordered shallowest to deepest.
///
/// For `"a/b/c.txt"`, returns `["a", "a/b"]`. For `"file.txt"`, returns an empty `Vec`.
pub fn ancestors(rel: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for (idx, ch) in rel.char_indices() {
        if ch == '/' && idx > 0 {
            out.push(&rel[..idx]);
        }
    }
    out
}

/// Returns the final filename component of a normalized relative path.
pub fn basename(rel: &str) -> &str {
    match rel.rsplit_once('/') {
        Some((_, name)) => name,
        None => rel,
    }
}

/// Returns the parent directory portion of a normalized relative path, if any.
pub fn parent_path(rel: &str) -> Option<&str> {
    rel.rsplit_once('/').map(|(parent, _)| parent)
}

/// Returns the depth (number of `/` separators) of a normalized relative path.
pub fn path_depth(rel: &str) -> usize {
    rel.bytes().filter(|&b| b == b'/').count()
}

/// Returns `true` if `candidate` is a strict descendant of `dir` (`candidate` starts with
/// `"{dir}/"`).
pub fn is_descendant_of(candidate: &str, dir: &str) -> bool {
    candidate
        .strip_prefix(dir)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Case-folds a normalized path for case-insensitive collision detection.
pub fn fold_case(path: &str) -> String {
    path.to_lowercase()
}

/// Returns `true` when `rel` / `name` matches one of the built-in fixed ignore rules from the
/// design: `.legion-sync/`, system litter (`.DS_Store`, `Thumbs.db`, `desktop.ini`), or files
/// Legion itself is still writing (`*.legion-part`, `.legion-*.tmp`).
pub fn is_fixed_ignored(rel: &str, name: &str) -> bool {
    if is_internal_sync_path(rel) {
        return true;
    }
    if name.eq_ignore_ascii_case(".DS_Store")
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
    {
        return true;
    }
    if name.ends_with(legion_xfer::PART_SUFFIX) {
        return true;
    }
    if name.starts_with(".legion-") && name.ends_with(".tmp") {
        return true;
    }
    false
}

/// Compiled ignore matcher combining fixed design rules with per-folder `ignore` globs.
#[derive(Debug, Clone)]
pub struct IgnoreMatcher {
    any_path: GlobSet,
    any_name: GlobSet,
    dir_path: GlobSet,
    dir_name: GlobSet,
}

impl Default for IgnoreMatcher {
    fn default() -> Self {
        Self {
            any_path: GlobSet::empty(),
            any_name: GlobSet::empty(),
            dir_path: GlobSet::empty(),
            dir_name: GlobSet::empty(),
        }
    }
}

impl IgnoreMatcher {
    /// Compiles per-folder ignore glob patterns using the same semantics as `legion-xfer`:
    /// trailing `/` restricts to directories, patterns without `/` match any basename or root
    /// path, and patterns with `/` match relative paths from the folder root.
    pub fn compile(patterns: &[String]) -> Result<Self> {
        let mut any_path = GlobSetBuilder::new();
        let mut any_name = GlobSetBuilder::new();
        let mut dir_path = GlobSetBuilder::new();
        let mut dir_name = GlobSetBuilder::new();

        for raw in patterns {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                continue;
            }
            let (dir_only, body) = match trimmed.strip_suffix('/') {
                Some(stripped) => (true, stripped),
                None => (false, trimmed),
            };
            if body.is_empty() {
                continue;
            }
            let glob = GlobBuilder::new(body)
                .literal_separator(true)
                .build()
                .map_err(|source| SyncError::InvalidPattern {
                    pattern: raw.clone(),
                    reason: source.to_string(),
                })?;

            let has_slash = body.contains('/');
            match (dir_only, has_slash) {
                (false, true) => {
                    any_path.add(glob);
                }
                (false, false) => {
                    any_path.add(glob.clone());
                    any_name.add(glob);
                }
                (true, true) => {
                    dir_path.add(glob);
                }
                (true, false) => {
                    dir_path.add(glob.clone());
                    dir_name.add(glob);
                }
            }
        }

        let build_set = |builder: GlobSetBuilder| -> Result<GlobSet> {
            builder.build().map_err(|source| SyncError::InvalidPattern {
                pattern: patterns.join(","),
                reason: source.to_string(),
            })
        };

        Ok(Self {
            any_path: build_set(any_path)?,
            any_name: build_set(any_name)?,
            dir_path: build_set(dir_path)?,
            dir_name: build_set(dir_name)?,
        })
    }

    /// Returns `true` if `rel` (with basename `name` and directory flag `is_dir`) is excluded by
    /// either the fixed rules or the folder's custom `ignore` globs.
    pub fn is_ignored(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        if is_fixed_ignored(rel, name) {
            return true;
        }
        if self.any_path.is_match(rel) || self.any_name.is_match(name) {
            return true;
        }
        if is_dir && (self.dir_path.is_match(rel) || self.dir_name.is_match(name)) {
            return true;
        }
        false
    }

    /// Returns `true` if `rel` or any of its ancestor directories is excluded.
    pub fn is_path_or_ancestor_ignored(&self, rel: &str, is_dir: bool) -> bool {
        for anc in ancestors(rel) {
            if self.is_ignored(anc, basename(anc), true) {
                return true;
            }
        }
        self.is_ignored(rel, basename(rel), is_dir)
    }
}

/// Checks whether a validated relative path and entry kind can be represented on a filesystem with
/// `caps`.
///
/// Returns `Ok(())` when representable, or `Err((ProblemKind, plain_sentence))` when the entry
/// must be held back and reported as a problem instead of being mangled.
pub fn check_fs_compatibility(
    rel: &str,
    kind: EntryKind,
    caps: &FsCapabilities,
) -> std::result::Result<(), (ProblemKind, String)> {
    if kind == EntryKind::Symlink && !caps.supports_symlinks {
        return Err((
            ProblemKind::UnsupportedSymlink,
            format!("'{rel}' was held back because this system does not sync symbolic links."),
        ));
    }

    if caps.max_path_len > 0 && rel.len() > caps.max_path_len {
        return Err((
            ProblemKind::PathTooLong,
            format!(
                "'{rel}' was held back because its path is longer than this file system allows ({} bytes).",
                caps.max_path_len
            ),
        ));
    }

    for seg in rel.split('/') {
        if caps.max_segment_len > 0 && seg.len() > caps.max_segment_len {
            return Err((
                ProblemKind::SegmentTooLong,
                format!(
                    "'{rel}' was held back because the name '{seg}' is longer than this file system allows ({} bytes).",
                    caps.max_segment_len
                ),
            ));
        }

        if caps.windows_naming {
            if let Some(ch) = seg.chars().find(|&c| {
                (c as u32) < 0x20 || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            }) {
                return Err((
                    ProblemKind::ForbiddenWindowsChar,
                    format!(
                        "'{rel}' was held back because Windows does not allow '{}' in file names.",
                        ch.escape_default()
                    ),
                ));
            }
            if seg.ends_with('.') || seg.ends_with(' ') {
                return Err((
                    ProblemKind::TrailingDotOrSpace,
                    format!(
                        "'{rel}' was held back because Windows does not allow names ending with a dot or space."
                    ),
                ));
            }
            let stem = seg.split('.').next().unwrap_or(seg).trim_end_matches(' ');
            if is_windows_reserved_device(stem) {
                return Err((
                    ProblemKind::ReservedWindowsName,
                    format!(
                        "'{rel}' was held back because '{stem}' is a reserved name on Windows."
                    ),
                ));
            }
        }
    }

    Ok(())
}

fn is_windows_reserved_device(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "CONIN$"
            | "CONOUT$"
            | "COM0"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM\u{00B9}"
            | "COM\u{00B2}"
            | "COM\u{00B3}"
            | "LPT0"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT\u{00B9}"
            | "LPT\u{00B2}"
            | "LPT\u{00B3}"
    )
}

/// Formats a UTC `YYYY-MM-DD` date string from a [`Timestamp`].
///
/// Uses Howard Hinnant's Euclidean civil-date conversion over whole UTC days (`secs.div_euclid(86400)`),
/// which handles leap years and pre-1970 timestamps accurately with no timezone database dependency.
pub fn format_utc_date(ts: Timestamp) -> String {
    let days = ts.secs.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d as u32)
}

/// Formats the deterministic conflict copy path for `original_path`.
///
/// Follows the design format: `name (changed on <machine> <date>).ext` (or without `.ext` when
/// there is no extension or the filename is a dotfile such as `.bashrc`). When `disambiguator` is
/// supplied (only if the primary name already holds different content), inserts it inside the
/// parentheses: `name (changed on <machine> <date> <disambiguator>).ext`.
pub fn conflict_copy_path(
    original_path: &str,
    machine: &str,
    mtime: Timestamp,
    disambiguator: Option<&str>,
) -> String {
    let date = format_utc_date(mtime);
    let tag = match disambiguator {
        Some(extra) => format!(" (changed on {machine} {date} {extra})"),
        None => format!(" (changed on {machine} {date})"),
    };
    insert_suffix_before_ext(original_path, &tag)
}

/// Formats a non-colliding restore path: `name (restored <date>).ext`.
pub fn restore_copy_path(
    original_path: &str,
    now: Timestamp,
    disambiguator: Option<&str>,
) -> String {
    let date = format_utc_date(now);
    let tag = match disambiguator {
        Some(extra) => format!(" (restored {date} {extra})"),
        None => format!(" (restored {date})"),
    };
    insert_suffix_before_ext(original_path, &tag)
}

fn insert_suffix_before_ext(path: &str, suffix: &str) -> String {
    let (parent, name) = match path.rsplit_once('/') {
        Some((p, n)) => (Some(p), n),
        None => (None, path),
    };

    let renamed = match split_stem_ext(name) {
        (stem, Some(ext)) => format!("{stem}{suffix}.{ext}"),
        (stem, None) => format!("{stem}{suffix}"),
    };

    match parent {
        Some(p) => format!("{p}/{renamed}"),
        None => renamed,
    }
}

fn split_stem_ext(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        // Leading dot with no other dot (e.g. `.bashrc`) has no extension.
        Some(("", _)) => (name, None),
        // Trailing dot (e.g. `foo.`) has no non-empty extension.
        Some((_, "")) => (name, None),
        Some((stem, ext)) => (stem, Some(ext)),
        None => (name, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_and_unsafe_paths() {
        assert_eq!(validate_sync_path("a/b/c.txt").unwrap(), "a/b/c.txt");
        assert_eq!(validate_sync_path("foo:bar.txt").unwrap(), "foo:bar.txt");

        for bad in [
            "",
            ".",
            "..",
            "/abs.txt",
            "dir/",
            "a//b",
            "a/../b",
            "a/./b",
            "a\\b",
            "a\0b",
            ".legion-sync",
            ".legion-sync/bin/x",
            ".LEGION-SYNC/tmp/y",
        ] {
            assert!(
                validate_sync_path(bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }
    }

    #[test]
    fn formats_conflict_paths_matching_design() {
        // 2026-11-25T12:00:00Z
        let ts = Timestamp::new(1_795_608_000, 0);
        assert_eq!(
            conflict_copy_path("notes.txt", "laptop", ts, None),
            "notes (changed on laptop 2026-11-25).txt"
        );
        assert_eq!(
            conflict_copy_path("docs/report.pdf", "tower", ts, None),
            "docs/report (changed on tower 2026-11-25).pdf"
        );
        assert_eq!(
            conflict_copy_path("Makefile", "pi", ts, None),
            "Makefile (changed on pi 2026-11-25)"
        );
        assert_eq!(
            conflict_copy_path(".bashrc", "pi", ts, None),
            ".bashrc (changed on pi 2026-11-25)"
        );
        assert_eq!(
            conflict_copy_path("notes.txt", "laptop", ts, Some("deadbeef")),
            "notes (changed on laptop 2026-11-25 deadbeef).txt"
        );
    }

    #[test]
    fn checks_windows_and_length_rules() {
        let win = FsCapabilities::windows();
        assert_eq!(
            check_fs_compatibility("CON.txt", EntryKind::File, &win)
                .unwrap_err()
                .0,
            ProblemKind::ReservedWindowsName
        );
        assert_eq!(
            check_fs_compatibility("sub/a:b.txt", EntryKind::File, &win)
                .unwrap_err()
                .0,
            ProblemKind::ForbiddenWindowsChar
        );
        assert_eq!(
            check_fs_compatibility("trailing ", EntryKind::File, &win)
                .unwrap_err()
                .0,
            ProblemKind::TrailingDotOrSpace
        );
        assert_eq!(
            check_fs_compatibility("link", EntryKind::Symlink, &win)
                .unwrap_err()
                .0,
            ProblemKind::UnsupportedSymlink
        );

        let long_seg = "a".repeat(256);
        assert_eq!(
            check_fs_compatibility(&long_seg, EntryKind::File, &FsCapabilities::unix())
                .unwrap_err()
                .0,
            ProblemKind::SegmentTooLong
        );
    }

    #[test]
    fn ignore_matcher_handles_fixed_and_custom_globs() {
        let matcher = IgnoreMatcher::compile(&[
            "target/".to_string(),
            "*.log".to_string(),
            "build/output".to_string(),
        ])
        .unwrap();

        assert!(matcher.is_ignored(".legion-sync", ".legion-sync", true));
        assert!(matcher.is_ignored(".DS_Store", ".DS_Store", false));
        assert!(matcher.is_ignored("sub/Thumbs.db", "Thumbs.db", false));
        assert!(matcher.is_ignored("sub/desktop.ini", "desktop.ini", false));
        assert!(matcher.is_ignored("file.txt.legion-part", "file.txt.legion-part", false));
        assert!(matcher.is_ignored(".legion-sync-1.tmp", ".legion-sync-1.tmp", false));
        assert!(matcher.is_ignored("target", "target", true));
        assert!(!matcher.is_ignored("target", "target", false));
        assert!(matcher.is_ignored("a/b/debug.log", "debug.log", false));
        assert!(matcher.is_ignored("build/output", "output", false));
        assert!(!matcher.is_ignored("src/main.rs", "main.rs", false));
    }
}
