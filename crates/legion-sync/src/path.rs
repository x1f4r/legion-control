//! Path validation, platform compatibility checks, ignore matching, and conflict naming.

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use legion_xfer::{EntryKind, Timestamp};
use unicode_normalization::UnicodeNormalization;

use crate::error::{Result, SyncError};
use crate::model::{ProblemKind, validate_member_id};
use crate::traits::FsCapabilities;

/// Reserved directory name holding sync temporary files, bin contents, and folder marker.
pub const SYNC_META_DIR: &str = ".legion-sync";

/// Relative path of the folder identity marker inside `.legion-sync/`.
pub const SYNC_MARKER_FILE: &str = ".legion-sync/marker.json";

/// Relative path of the folder's durable sync index inside `.legion-sync/`.
pub const SYNC_INDEX_FILE: &str = ".legion-sync/index.redb";

/// Relative directory inside the sync folder for deleted and replaced copies.
pub const SYNC_BIN_DIR: &str = ".legion-sync/bin";

/// Relative directory inside the sync folder for in-flight staged downloads.
pub const SYNC_TMP_DIR: &str = ".legion-sync/tmp";

/// Maximum universal relative sync path byte length accepted from peers.
pub const MAX_SYNC_PATH_BYTES: usize = 4_096;

/// Maximum directory nesting depth accepted from peers.
pub const MAX_SYNC_PATH_DEPTH: usize = 128;

/// Returns `true` if `seg` matches `.legion-sync` under case folding and Unicode compatibility
/// normalization (NFKC).
pub fn is_dot_legion_sync_segment(seg: &str) -> bool {
    if seg.is_ascii() {
        return seg.eq_ignore_ascii_case(SYNC_META_DIR);
    }
    let normalized: String = seg.nfkc().collect();
    normalized.to_lowercase() == SYNC_META_DIR
}

/// Returns `true` if any path segment in `rel` is `.legion-sync` under case folding and NFKC
/// Unicode compatibility normalization.
pub fn is_internal_sync_path(rel: &str) -> bool {
    rel.split('/').any(is_dot_legion_sync_segment)
}

/// Returns `true` if `seg` starts with a Windows drive-relative prefix such as `C:foo`.
pub fn has_windows_drive_prefix(seg: &str) -> bool {
    let bytes = seg.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Validates a relative path used in the live sync tree and returns its normalized forward-slash
/// form.
///
/// Refuses empty paths, `.`, `..`, leading or trailing slashes, empty segments (`//`),
/// backslashes (including `\\?\` and `\\.\`), NUL bytes, ASCII/Unicode control characters,
/// Windows drive-relative prefixes (`C:x`), paths exceeding universal byte/depth bounds, and any
/// segment matching `.legion-sync` under case folding and NFKC normalization at any depth.
///
/// Does *not* refuse platform-only filename limits such as multi-character colons (`foo:bar.txt`)
/// or reserved device names (`CON.txt`); those are checked against [`FsCapabilities`] so
/// Windows holds them back as problems while Unix syncs them normally (and restore/conflict
/// operations check [`check_fs_compatibility`] too).
pub fn validate_sync_path(raw: &str) -> Result<String> {
    if raw.is_empty() {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "empty path is not allowed".to_string(),
        });
    }
    if raw.len() > MAX_SYNC_PATH_BYTES {
        return Err(SyncError::InvalidPath {
            path: raw.chars().take(128).collect(),
            reason: format!("path exceeds maximum length of {MAX_SYNC_PATH_BYTES} bytes"),
        });
    }
    if raw.as_bytes().contains(&0) {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "path contains a NUL byte".to_string(),
        });
    }
    if raw.chars().any(char::is_control) {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "path contains a control character".to_string(),
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

    let mut seg_count: usize = 0;
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
        if is_dot_legion_sync_segment(seg) {
            return Err(SyncError::InvalidPath {
                path: raw.to_string(),
                reason: "'.legion-sync' is reserved for sync metadata and never synced".to_string(),
            });
        }
        if has_windows_drive_prefix(seg) {
            return Err(SyncError::InvalidPath {
                path: raw.to_string(),
                reason: format!("Windows drive-relative segment {seg:?} is not allowed"),
            });
        }
        seg_count += 1;
    }

    if seg_count == 0 {
        return Err(SyncError::InvalidPath {
            path: raw.to_string(),
            reason: "path has no segments".to_string(),
        });
    }
    if seg_count.saturating_sub(1) > MAX_SYNC_PATH_DEPTH {
        return Err(SyncError::InvalidPath {
            path: raw.chars().take(128).collect(),
            reason: format!("path exceeds maximum depth of {MAX_SYNC_PATH_DEPTH}"),
        });
    }

    Ok(raw.to_string())
}

/// Verifies that symbolic link target `target` located at sync-relative path `link_rel_path` is a
/// relative path that lexically resolves strictly inside the sync folder and never enters
/// `.legion-sync`.
///
/// Absolute targets, targets containing backslashes or drive prefixes, targets pointing into
/// `.legion-sync`, and relative targets whose `..` segments escape above the sync folder root are
/// refused so they are held back and listed instead of synced.
pub fn check_symlink_target_contained(
    link_rel_path: &str,
    target: &str,
) -> std::result::Result<(), String> {
    if target.is_empty() {
        return Err("its target is empty".to_string());
    }
    if target.len() > crate::model::MAX_LINK_TARGET_BYTES {
        return Err(format!(
            "its target exceeds {} bytes",
            crate::model::MAX_LINK_TARGET_BYTES
        ));
    }
    if target.as_bytes().contains(&0) {
        return Err("its target contains a NUL byte".to_string());
    }
    if target.starts_with('/') {
        return Err("absolute symbolic links are not synced".to_string());
    }
    if target.contains('\\') {
        return Err("symbolic links containing backslashes are not synced".to_string());
    }

    let mut stack: Vec<&str> = match parent_path(link_rel_path) {
        Some(parent) => parent.split('/').collect(),
        None => Vec::new(),
    };
    for seg in target.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if has_windows_drive_prefix(seg) {
            return Err("symbolic links with drive prefixes are not synced".to_string());
        }
        if seg == ".." {
            if stack.pop().is_none() {
                return Err("it points outside the sync folder".to_string());
            }
        } else {
            if is_dot_legion_sync_segment(seg) {
                return Err("it points into '.legion-sync'".to_string());
            }
            stack.push(seg);
        }
    }
    Ok(())
}

/// Validates a flat filename used inside `.legion-sync/tmp/` or `.legion-sync/bin/`.
pub fn validate_flat_internal_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 255
        || name.contains('/')
        || name.contains('\\')
        || name.as_bytes().contains(&0)
        || name.chars().any(char::is_control)
        || has_windows_drive_prefix(name)
    {
        return Err(SyncError::InvalidPath {
            path: name.to_string(),
            reason: "internal filename must be a single safe non-empty path segment".to_string(),
        });
    }
    Ok(())
}

/// Validates a staged temporary token, which is either:
/// - a flat filename inside `.legion-sync/tmp/` (returns `Ok(None)`), or
/// - a beside-destination relative path `<dir>/.legion-*.tmp` used when `<dir>` sits on a
///   different filesystem/subvolume from `.legion-sync/tmp/` (returns `Ok(Some(clean_rel))`).
pub fn validate_staged_tmp_token(token: &str) -> Result<Option<String>> {
    if !token.contains('/') {
        validate_flat_internal_name(token)?;
        return Ok(None);
    }
    let clean = validate_sync_path(token)?;
    let name = basename(&clean);
    if !is_legion_inflight_name(name) {
        return Err(SyncError::InvalidPath {
            path: token.to_string(),
            reason: "beside-destination staging filename must use Legion's fixed-ignored '.legion-*.tmp' name".to_string(),
        });
    }
    Ok(Some(clean))
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

/// Case-folds and NFC-normalizes a relative path for case-insensitive / Unicode-normalizing
/// collision detection (Windows NTFS and macOS APFS/HFS+).
pub fn fold_case(path: &str) -> String {
    if path.is_ascii() {
        return path.to_ascii_lowercase();
    }
    path.nfc().collect::<String>().to_lowercase()
}

/// Normalizes a path for a filesystem with `caps` (applying case folding and/or Unicode
/// canonical equivalence as needed).
pub fn fs_equivalence_key(path: &str, caps: &FsCapabilities) -> String {
    if caps.case_insensitive {
        fold_case(path)
    } else if caps.unicode_normalizing {
        if path.is_ascii() {
            path.to_string()
        } else {
            path.nfc().collect()
        }
    } else {
        path.to_string()
    }
}

/// Returns `true` when `name` is one of the operating-system litter filenames from the fixed
/// ignore list (`.DS_Store`, `Thumbs.db`, `desktop.ini`).
pub fn is_os_litter_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(".DS_Store")
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

/// Returns `true` when `name` is an in-flight temporary file written by Legion itself
/// (`*.legion-part` or `.legion-*.tmp`).
pub fn is_legion_inflight_name(name: &str) -> bool {
    name.ends_with(legion_xfer::PART_SUFFIX)
        || (name.starts_with(".legion-") && name.ends_with(".tmp"))
}

/// Returns `true` when `rel` / `name` matches one of the built-in fixed ignore rules from the
/// design: `.legion-sync` at any depth, OS litter (`.DS_Store`, `Thumbs.db`, `desktop.ini`), or
/// files Legion itself is writing (`*.legion-part`, `.legion-*.tmp`).
///
/// `.git` and editor/download temporary files are intentionally *not* in the fixed list; they
/// belong in per-folder `ignore` globs.
pub fn is_fixed_ignored(rel: &str, name: &str) -> bool {
    is_internal_sync_path(rel) || is_os_litter_name(name) || is_legion_inflight_name(name)
}

#[derive(Debug)]
struct CustomGlobs {
    any_path: GlobSet,
    any_name: GlobSet,
    dir_path: GlobSet,
    dir_name: GlobSet,
}

/// Compiled ignore matcher combining fixed design rules with per-folder `ignore` globs.
#[derive(Debug, Clone, Default)]
pub struct IgnoreMatcher {
    custom: Option<std::sync::Arc<CustomGlobs>>,
}

impl IgnoreMatcher {
    /// Compiles per-folder ignore glob patterns using the same semantics as `legion-xfer`:
    /// trailing `/` restricts to directories, patterns without `/` match any basename or root
    /// path, and patterns with `/` match relative paths from the folder root.
    pub fn compile(patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self::default());
        }
        let mut any_path = GlobSetBuilder::new();
        let mut any_name = GlobSetBuilder::new();
        let mut dir_path = GlobSetBuilder::new();
        let mut dir_name = GlobSetBuilder::new();
        let mut added_any = false;

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

            added_any = true;
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

        if !added_any {
            return Ok(Self::default());
        }

        let build_set = |builder: GlobSetBuilder| -> Result<GlobSet> {
            builder.build().map_err(|source| SyncError::InvalidPattern {
                pattern: patterns.join(","),
                reason: source.to_string(),
            })
        };

        Ok(Self {
            custom: Some(std::sync::Arc::new(CustomGlobs {
                any_path: build_set(any_path)?,
                any_name: build_set(any_name)?,
                dir_path: build_set(dir_path)?,
                dir_name: build_set(dir_name)?,
            })),
        })
    }

    /// Returns `true` if `rel` matches only the folder's custom `ignore` globs (excluding fixed
    /// rules).
    pub fn is_custom_ignored(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        let Some(custom) = &self.custom else {
            return false;
        };
        if custom.any_path.is_match(rel) || custom.any_name.is_match(name) {
            return true;
        }
        if is_dir && (custom.dir_path.is_match(rel) || custom.dir_name.is_match(name)) {
            return true;
        }
        false
    }

    /// Returns `true` if `rel` (with basename `name` and directory flag `is_dir`) is excluded by
    /// either the fixed rules or the folder's custom `ignore` globs.
    pub fn is_ignored(&self, rel: &str, name: &str, is_dir: bool) -> bool {
        is_fixed_ignored(rel, name) || self.is_custom_ignored(rel, name, is_dir)
    }

    /// Returns `true` if `rel` or any of its ancestor directories is excluded.
    pub fn is_path_or_ancestor_ignored(&self, rel: &str, is_dir: bool) -> bool {
        if self.custom.is_none() {
            return rel.split('/').any(|seg| {
                is_dot_legion_sync_segment(seg)
                    || is_os_litter_name(seg)
                    || is_legion_inflight_name(seg)
            });
        }
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

/// Returns `true` if `stem` matches a reserved Windows DOS device name (case-insensitively).
pub fn is_windows_reserved_device(stem: &str) -> bool {
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

/// Builds the deterministic conflict-copy path for `original_path`:
/// `name (changed on <member> <YYYY-MM-DD>, <hash6>).ext`.
///
/// Always includes a 6-hex-character content hash so two separate conflicts on the same file lost
/// by the same machine on the same day produce distinct filenames while the same conflict derived
/// independently on two members produces the exact same filename and content.
pub fn conflict_copy_path(
    original_path: &str,
    member: &str,
    mtime: Timestamp,
    hash6: &str,
    disambiguator: Option<&str>,
) -> String {
    let date = format_utc_date(mtime);
    let suffix = match disambiguator {
        Some(extra) if !extra.is_empty() => {
            format!(" (changed on {member} {date}, {hash6}-{extra})")
        }
        _ => format!(" (changed on {member} {date}, {hash6})"),
    };
    insert_before_extension(original_path, &suffix)
}

/// Parsed components of a canonical conflict copy filename derived locally from the path itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedConflictPath {
    /// Derived original path before the ` (changed on ...)` suffix.
    pub original_path: String,
    /// Member name embedded in the conflict copy filename.
    pub member: String,
    /// UTC date string (`YYYY-MM-DD`).
    pub date: String,
    /// 6-character content hash tag (excluding any optional `-N` disambiguator).
    pub hash6: String,
}

/// Parses a conflict-copy path `dir/name (changed on <member> <YYYY-MM-DD>, <hash6>[-N]).ext`
/// locally without trusting peer-asserted metadata.
pub fn parse_conflict_copy_path(path: &str) -> Option<ParsedConflictPath> {
    let (dir_prefix, file_name) = match path.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, path),
    };

    // Split extension the exact same way `insert_before_extension` does.
    let (stem_with_suffix, ext) = split_name_and_ext(file_name);
    let marker = " (changed on ";
    let idx = stem_with_suffix.rfind(marker)?;
    let orig_stem = &stem_with_suffix[..idx];
    if orig_stem.is_empty() {
        return None;
    }
    let inside = stem_with_suffix[idx + marker.len()..].strip_suffix(')')?;
    let (member_and_date, hash_part) = inside.rsplit_once(", ")?;
    let (member, date) = member_and_date.rsplit_once(' ')?;

    if validate_member_id(member).is_err() {
        return None;
    }
    if !is_valid_iso_date(date) {
        return None;
    }
    let hash6 = hash_part.split('-').next().unwrap_or(hash_part);
    if hash6.len() != 6
        || !hash6
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }

    let orig_name = format!("{orig_stem}{ext}");
    let original_path = match dir_prefix {
        Some(d) => format!("{d}/{orig_name}"),
        None => orig_name,
    };
    if validate_sync_path(&original_path).is_err() {
        return None;
    }

    Some(ParsedConflictPath {
        original_path,
        member: member.to_string(),
        date: date.to_string(),
        hash6: hash6.to_string(),
    })
}

fn is_valid_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// Builds the sibling restore path when restoring a bin entry with [`crate::RestoreStrategy::Rename`].
pub fn restore_copy_path(
    original_path: &str,
    now: Timestamp,
    disambiguator: Option<&str>,
) -> String {
    let date = format_utc_date(now);
    let suffix = match disambiguator {
        Some(tag) if !tag.is_empty() => format!(" (restored {date} {tag})"),
        _ => format!(" (restored {date})"),
    };
    insert_before_extension(original_path, &suffix)
}

fn split_name_and_ext(file_name: &str) -> (&str, &str) {
    match file_name.rfind('.') {
        // Dotfiles like `.bashrc` have no extension; append after the name.
        Some(dot) if dot > 0 && dot + 1 < file_name.len() => (&file_name[..dot], &file_name[dot..]),
        _ => (file_name, ""),
    }
}

fn insert_before_extension(path: &str, suffix: &str) -> String {
    let (dir_prefix, file_name) = match path.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, path),
    };

    let (stem, ext) = split_name_and_ext(file_name);
    let new_name = format!("{stem}{suffix}{ext}");

    match dir_prefix {
        Some(d) => format!("{d}/{new_name}"),
        None => new_name,
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
            "\\\\?\\C:\\evil",
            "C:evil.txt",
            "sub/D:rel",
            "a\0b",
            "ctrl\x07.txt",
            ".legion-sync",
            ".legion-sync/bin/x",
            ".LEGION-SYNC/tmp/y",
            "sub/.legion-sync/file.txt",
            "sub/.LEGION-SYNC/file.txt",
            // Fullwidth compatibility equivalents of `.legion-sync` (NFKC)
            "\u{FF0E}\u{FF4C}\u{FF45}\u{FF47}\u{FF49}\u{FF4F}\u{FF4E}\u{FF0D}\u{FF53}\u{FF59}\u{FF4E}\u{FF43}/x",
        ] {
            assert!(
                validate_sync_path(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn formats_and_parses_conflict_paths_with_hash6() {
        let ts = Timestamp::new(1_795_608_000, 0); // 2026-11-25 UTC
        let c1 = conflict_copy_path("docs/report.pdf", "laptop", ts, "a1b2c3", None);
        assert_eq!(c1, "docs/report (changed on laptop 2026-11-25, a1b2c3).pdf");
        let parsed = parse_conflict_copy_path(&c1).unwrap();
        assert_eq!(parsed.original_path, "docs/report.pdf");
        assert_eq!(parsed.member, "laptop");
        assert_eq!(parsed.date, "2026-11-25");
        assert_eq!(parsed.hash6, "a1b2c3");

        let c2 = conflict_copy_path("Makefile", "tower", ts, "00ff99", None);
        assert_eq!(c2, "Makefile (changed on tower 2026-11-25, 00ff99)");
        assert_eq!(
            parse_conflict_copy_path(&c2).unwrap().original_path,
            "Makefile"
        );

        let c3 = conflict_copy_path(".env", "pi", ts, "deadbe", Some("2"));
        assert_eq!(c3, ".env (changed on pi 2026-11-25, deadbe-2)");
        assert_eq!(parse_conflict_copy_path(&c3).unwrap().original_path, ".env");
    }

    #[test]
    fn fixed_ignores_match_design_and_do_not_ignore_git() {
        let matcher = IgnoreMatcher::compile(&["*.log".into(), "build/".into()]).unwrap();
        assert!(matcher.is_ignored(".legion-sync/bin/a", "a", false));
        assert!(matcher.is_ignored("sub/.legion-sync/x", "x", false));
        assert!(matcher.is_ignored("sub/.DS_Store", ".DS_Store", false));
        assert!(matcher.is_ignored("Thumbs.db", "Thumbs.db", false));
        assert!(matcher.is_ignored("desktop.ini", "desktop.ini", false));
        assert!(matcher.is_ignored("big.bin.legion-part", "big.bin.legion-part", false));
        assert!(matcher.is_ignored(".legion-123.tmp", ".legion-123.tmp", false));
        // `.git` and editor swap files are NOT in the fixed list (Item I).
        assert!(!matcher.is_ignored(".git", ".git", true));
        assert!(!matcher.is_ignored("file.swp", "file.swp", false));
        assert!(!matcher.is_ignored("draft.tmp", "draft.tmp", false));
        // Per-folder custom globs still work.
        assert!(matcher.is_ignored("sub/debug.log", "debug.log", false));
        assert!(matcher.is_ignored("build", "build", true));
        assert!(!matcher.is_ignored("build", "build", false));
    }

    #[test]
    fn checks_windows_and_length_rules() {
        let win = FsCapabilities::windows();
        assert!(check_fs_compatibility("ok/report.txt", EntryKind::File, &win).is_ok());
        assert_eq!(
            check_fs_compatibility("CON.txt", EntryKind::File, &win)
                .unwrap_err()
                .0,
            ProblemKind::ReservedWindowsName
        );
        assert_eq!(
            check_fs_compatibility("sub/foo:bar.txt", EntryKind::File, &win)
                .unwrap_err()
                .0,
            ProblemKind::ForbiddenWindowsChar
        );
        assert_eq!(
            check_fs_compatibility("trailing.", EntryKind::File, &win)
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
    }

    #[test]
    fn checks_symlink_target_containment() {
        assert!(check_symlink_target_contained("link", "target.txt").is_ok());
        assert!(check_symlink_target_contained("sub/link", "../target.txt").is_ok());
        assert!(check_symlink_target_contained("a/b/link", "./../c/../d.txt").is_ok());
        assert!(check_symlink_target_contained("sub/link", "..").is_ok());

        for (path, target) in [
            ("link", ""),
            ("link", "/etc/passwd"),
            ("sub/link", "/abs/path"),
            ("link", "../outside.txt"),
            ("sub/link", "../../outside.txt"),
            ("sub/link", "a/../../../escape"),
            ("link", ".legion-sync/marker.json"),
            ("sub/link", "../.LEGION-SYNC/bin/x"),
            ("link", "sub\\file.txt"),
            ("link", "C:evil.txt"),
            ("link", "bad\0target"),
        ] {
            assert!(
                check_symlink_target_contained(path, target).is_err(),
                "expected ({path:?} -> {target:?}) to be refused"
            );
        }
    }
}
