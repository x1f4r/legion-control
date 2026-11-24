//! Cross-platform validation for relative transfer paths and filenames.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Validates a relative transfer path and returns its normalized forward-slash form.
///
/// Refuses empty paths, `.`, absolute paths, `..`, backslashes, NUL bytes, non-UTF-8, colons in
/// any segment on every platform (preventing Windows mid-path drive prefixes and NTFS alternate
/// data streams), and on Windows also refuses reserved device names and segments ending in a dot
/// or space.
pub fn validate_relative_path(raw: &str) -> Result<String> {
    match parse_relative_path_bytes(raw.as_bytes())? {
        Some((normalized, _)) => Ok(normalized),
        None => Err(Error::UnsafeArchiveEntry {
            entry: raw.to_string(),
            reason: "empty or current-directory path is not allowed".to_string(),
        }),
    }
}

/// Validates a relative transfer path and joins its segments onto `root`.
///
/// Use this whenever a remote-supplied manifest or plan path (such as an entry in
/// [`crate::Plan::delete`]) is turned into a local filesystem path.
pub fn resolve_relative_path(root: &Path, raw: &str) -> Result<PathBuf> {
    let Some((_, segments)) = parse_relative_path_bytes(raw.as_bytes())? else {
        return Err(Error::UnsafeArchiveEntry {
            entry: raw.to_string(),
            reason: "empty or current-directory path is not allowed".to_string(),
        });
    };
    let mut out = root.to_path_buf();
    for seg in segments {
        out.push(seg);
    }
    Ok(out)
}

/// Parses and validates raw path bytes into a normalized forward-slash string and segments.
///
/// Returns `Ok(None)` only for `.` / `./`.
pub(crate) fn parse_relative_path_bytes(raw: &[u8]) -> Result<Option<(String, Vec<String>)>> {
    let display = String::from_utf8_lossy(raw).into_owned();
    if raw.contains(&0) {
        return Err(Error::UnsafeArchiveEntry {
            entry: display,
            reason: "path contains NUL byte".to_string(),
        });
    }
    let s = std::str::from_utf8(raw).map_err(|_| Error::UnsafeArchiveEntry {
        entry: display.clone(),
        reason: "path is not valid UTF-8".to_string(),
    })?;
    if s.starts_with('/') {
        return Err(Error::UnsafeArchiveEntry {
            entry: display,
            reason: "absolute path is not allowed".to_string(),
        });
    }
    if s.contains('\\') {
        return Err(Error::UnsafeArchiveEntry {
            entry: display,
            reason: "backslash separator is not allowed in transfer paths".to_string(),
        });
    }

    let mut segments = Vec::new();
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(Error::UnsafeArchiveEntry {
                entry: display,
                reason: "parent directory component '..' is not allowed".to_string(),
            });
        }
        validate_segment(seg, &display)?;
        segments.push(seg.to_string());
    }

    if segments.is_empty() {
        return Ok(None);
    }

    let normalized = segments.join("/");
    Ok(Some((normalized, segments)))
}

/// Validates the final filename component of a local target path used by single-file operations.
pub(crate) fn validate_target_filename(path: &Path) -> Result<&OsStr> {
    let file_name = path.file_name().ok_or_else(|| Error::Conflict {
        path: path.to_path_buf(),
        reason: "target path has no file name".to_string(),
    })?;
    if let Some(name) = file_name.to_str() {
        validate_segment(name, &path.display().to_string())?;
    }
    Ok(file_name)
}

fn validate_segment(seg: &str, display: &str) -> Result<()> {
    if seg.contains(':') {
        return Err(Error::UnsafeArchiveEntry {
            entry: display.to_string(),
            reason: "colon is not allowed in transfer path segments".to_string(),
        });
    }
    #[cfg(windows)]
    if let Err(reason) = validate_windows_segment(seg) {
        return Err(Error::UnsafeArchiveEntry {
            entry: display.to_string(),
            reason: reason.to_string(),
        });
    }
    Ok(())
}

/// Checks Windows-specific filename restrictions (reserved DOS device names and trailing dots or
/// spaces). Compiled on every platform so its rules can be unit-tested everywhere.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn validate_windows_segment(seg: &str) -> std::result::Result<(), &'static str> {
    if seg.ends_with('.') || seg.ends_with(' ') {
        return Err("path segment cannot end with a dot or space on Windows");
    }
    let stem = seg.split('.').next().unwrap_or(seg).trim_end_matches(' ');
    if is_windows_reserved_device(stem) {
        return Err("reserved Windows device name is not allowed");
    }
    Ok(())
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_colon_in_any_segment_on_every_platform() {
        for bad in [
            "C:evil.txt",
            "sub/C:evil.txt",
            "a/b:stream",
            "a/b/c:zone.identifier",
            ":hidden",
        ] {
            let err = validate_relative_path(bad).unwrap_err();
            assert!(
                matches!(err, Error::UnsafeArchiveEntry { .. }),
                "expected UnsafeArchiveEntry for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn windows_segment_rules_catch_reserved_names_and_trailing_chars() {
        for bad in [
            "CON",
            "con",
            "Con.txt",
            "PRN",
            "aux.tar.gz",
            "NUL",
            "nul.json",
            "CONIN$",
            "conin$.txt",
            "CONOUT$",
            "ConOut$.log",
            "COM0",
            "com0.txt",
            "COM1",
            "com9.log",
            "COM\u{00B9}",
            "com\u{00B2}.txt",
            "COM\u{00B3}.log",
            "LPT0",
            "lpt0.bin",
            "LPT1",
            "lpt9.bin",
            "LPT\u{00B9}",
            "lpt\u{00B2}.txt",
            "LPT\u{00B3}.tar.gz",
            "CON .txt",
            "trailing.",
            "trailing ",
            "dots...",
        ] {
            assert!(
                validate_windows_segment(bad).is_err(),
                "expected Windows rejection for {bad:?}"
            );
        }

        for good in [
            "console.txt",
            "COM10",
            "LPT10",
            "null.txt",
            ".hidden",
            " leading-space.txt",
            "a.b.txt",
        ] {
            assert!(
                validate_windows_segment(good).is_ok(),
                "unexpected Windows rejection for {good:?}"
            );
        }
    }

    #[test]
    fn resolve_relative_path_rejects_traversal_and_colons() {
        let root = Path::new("/home/me/project");
        assert_eq!(
            resolve_relative_path(root, "sub/file.txt").unwrap(),
            PathBuf::from("/home/me/project/sub/file.txt")
        );
        assert!(resolve_relative_path(root, "../escape").is_err());
        assert!(resolve_relative_path(root, "sub/C:evil.txt").is_err());
        assert!(resolve_relative_path(root, "a/b:stream").is_err());
        assert!(resolve_relative_path(root, ".").is_err());
    }
}
