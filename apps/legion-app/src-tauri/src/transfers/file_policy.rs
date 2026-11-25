//! Strict cross-platform filename sanitization, device-side MIME/open safety policy,
//! atomic no-overwrite download placement, OS download security marks (Windows MotW and
//! macOS quarantine), and bounded cache eviction.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::{AppError, ErrorCode};

/// Maximum UTF-8 byte length of a sanitized filename, leaving headroom under the 255-byte
/// filesystem component limit for `" (9999)"` collision suffixes and cache prefixes.
pub const MAX_SAFE_FILENAME_BYTES: usize = 200;

/// Maximum UTF-8 byte length preserved for a file extension when truncating long names.
const MAX_EXTENSION_BYTES: usize = 32;

/// Copy chunk size for cross-filesystem exclusive placement (64 KiB).
const PLACE_COPY_BUF_BYTES: usize = 64 * 1024;

/// Default maximum files retained in the `open-externally` cache.
pub const MAX_OPEN_EXTERNALLY_CACHE_FILES: usize = 24;

/// Default maximum age of files retained in the `open-externally` cache (24 hours).
pub const MAX_OPEN_EXTERNALLY_CACHE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Default maximum total bytes retained in the `open-externally` cache (256 MB).
pub const MAX_OPEN_EXTERNALLY_CACHE_BYTES: u64 = 256 * 1000 * 1000;

/// Plain explanation surfaced when a downloaded file's extension is on the refused open list.
pub const REFUSED_OPEN_MESSAGE: &str =
    "Saved to Downloads. Legion does not open executable or installer files directly.";

/// Actionable hint surfaced when opening a refused file type is blocked.
pub const REFUSED_OPEN_HINT: &str =
    "Show the file in its folder if you trust it and want to inspect or run it yourself.";

/// Returns `true` for every Unicode General Category `Cf` (Format) code point (Unicode 16),
/// including bidirectional overrides/isolates (`U+202A..=U+202E`, `U+2066..=U+2069`, `U+200E`,
/// `U+200F`, `U+061C`), zero-width characters, soft hyphens, and BOM.
pub fn is_unicode_format_char(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

fn is_forbidden_filename_char(c: char) -> bool {
    matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        || c.is_control()
        || is_unicode_format_char(c)
}

/// Returns `true` if `name` (or its stem before the first `.`) matches a Windows reserved DOS
/// device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0`–`COM9`, `LPT0`–`LPT9`, and superscript variants
/// `COM¹`–`COM³`, `LPT¹`–`LPT³`).
pub fn is_reserved_windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end();
    if base.is_empty() {
        return false;
    }
    let upper = base.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    if let Some(rest) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    {
        let mut chars = rest.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            return ch.is_ascii_digit() || matches!(ch, '\u{00B9}' | '\u{00B2}' | '\u{00B3}');
        }
    }
    false
}

/// Splits `name` into `(stem, Some(ext))` when it has a safe non-empty ASCII extension up to
/// [`MAX_EXTENSION_BYTES`], or `(name, None)` otherwise.
fn split_stem_and_ext(name: &str) -> (&str, Option<&str>) {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return (name, None);
    };
    let ext_ok = !stem.is_empty()
        && !ext.is_empty()
        && ext.len() <= MAX_EXTENSION_BYTES
        && ext
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ext_ok {
        (stem, Some(ext))
    } else {
        (name, None)
    }
}

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn cap_filename_bytes(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_owned();
    }
    let (stem, ext) = split_stem_and_ext(name);
    if let Some(ext) = ext
        && ext.len() + 2 <= max_bytes
    {
        let stem_budget = max_bytes - ext.len() - 1;
        let truncated_stem = truncate_utf8_bytes(stem, stem_budget)
            .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
        if !truncated_stem.is_empty() {
            return format!("{truncated_stem}.{ext}");
        }
        return format!("file.{ext}");
    }
    truncate_utf8_bytes(name, max_bytes)
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_owned()
}

/// Sanitizes an untrusted filename by the strict union of Windows, macOS, Linux, and Android
/// filename safety rules so a downloaded file can never escape its target directory, spoof its
/// extension via Unicode BiDi controls, create NTFS alternate data streams, or collide with
/// reserved Windows device names.
///
/// Always returns a non-empty single path component (`<= MAX_SAFE_FILENAME_BYTES` UTF-8 bytes),
/// falling back to `"download"` when nothing safe remains.
pub fn sanitize_file_name(raw: &str) -> String {
    // 1. Discard any directory prefix across both POSIX and Windows path separators.
    let mut segment = raw.rsplit(['/', '\\']).next().unwrap_or("");

    // 2. Strip leading Windows drive designators (e.g. `C:version.dll`, `C:D:foo.txt`).
    loop {
        let bytes = segment.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            segment = &segment[2..];
        } else {
            break;
        }
    }

    // 3. Strip NTFS alternate data stream syntax (`file.jpg:evil.exe`, `file.txt:$DATA`).
    if let Some((before_stream, _)) = segment.split_once(':') {
        segment = before_stream;
    }

    // 4. Remove forbidden characters, Unicode control (Cc), and Unicode format (Cf) characters.
    let filtered: String = segment
        .chars()
        .filter(|&c| !is_forbidden_filename_char(c))
        .collect();

    // 5. Trim leading and trailing dots and whitespace (Windows rejects trailing dots/spaces and
    //    stripping leading dots prevents dotfiles and `.` / `..`).
    let trimmed = filtered.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        return "download".to_owned();
    }

    // 6. Cap UTF-8 byte length while preserving the real extension.
    let mut bounded = cap_filename_bytes(trimmed, MAX_SAFE_FILENAME_BYTES);
    let cleaned = bounded
        .trim_matches(|c: char| c == '.' || c.is_whitespace())
        .to_owned();
    if cleaned.is_empty() {
        return "download".to_owned();
    }
    bounded = cleaned;

    // 7. Rename reserved Windows device names (including `CON.txt`, `COM1.jpg`, `LPT¹.bin`).
    if is_reserved_windows_device_name(&bounded) {
        let prefixed = format!("_{bounded}");
        bounded = cap_filename_bytes(&prefixed, MAX_SAFE_FILENAME_BYTES);
    }

    if bounded.is_empty() || bounded == "." || bounded == ".." {
        "download".to_owned()
    } else {
        bounded
    }
}

/// Validates that `candidate_name` is a single normal path component and that joining it onto `dir`
/// stays strictly inside `dir` (`dst.parent() == Some(dir)`).
pub fn validate_child_in_dir(dir: &Path, candidate_name: &str) -> Result<PathBuf, AppError> {
    if candidate_name.is_empty()
        || candidate_name.len() > 255
        || candidate_name.chars().any(is_forbidden_filename_char)
    {
        return Err(AppError::new(
            ErrorCode::Denied,
            "Refusing unsafe destination file name.",
        ));
    }
    let mut components = Path::new(candidate_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => {
            return Err(AppError::new(
                ErrorCode::Denied,
                "Refusing destination file name that is not a single path component.",
            ));
        }
    }
    if is_reserved_windows_device_name(candidate_name) {
        return Err(AppError::new(
            ErrorCode::Denied,
            "Refusing reserved Windows device file name.",
        ));
    }
    let dst = dir.join(candidate_name);
    if dst.parent() != Some(dir) {
        return Err(AppError::new(
            ErrorCode::Denied,
            "Refusing destination path outside the intended target folder.",
        ));
    }
    Ok(dst)
}

/// Returns the lowercase real final extension of `file_name` after sanitization, if any.
pub fn real_extension(file_name: &str) -> Option<String> {
    let safe = sanitize_file_name(file_name);
    let (_, ext) = split_stem_and_ext(&safe);
    ext.map(str::to_ascii_lowercase)
}

/// Fixed list of executable, installer, script, shortcut, and disk-image extensions that Legion
/// refuses to open directly on any platform.
pub fn is_refused_open_extension(ext_lower: &str) -> bool {
    matches!(
        ext_lower,
        // Android packages and native binaries
        "apk"
            | "apks"
            | "apkm"
            | "xapk"
            | "aab"
            | "dex"
            | "odex"
            | "so"
            // Windows executables, installers, scripts, and shortcuts
            | "exe"
            | "dll"
            | "sys"
            | "scr"
            | "com"
            | "msi"
            | "msp"
            | "msu"
            | "mst"
            | "bat"
            | "cmd"
            | "ps1"
            | "psm1"
            | "psd1"
            | "vbs"
            | "vbe"
            | "js"
            | "jse"
            | "mjs"
            | "cjs"
            | "wsf"
            | "wsh"
            | "hta"
            | "cpl"
            | "inf"
            | "reg"
            | "lnk"
            | "url"
            | "pif"
            | "scf"
            | "settingcontent-ms"
            | "appref-ms"
            | "msix"
            | "msixbundle"
            | "appx"
            | "appxbundle"
            | "jar"
            | "chm"
            // macOS executables, bundles, installers, scripts, and disk images
            | "app"
            | "pkg"
            | "mpkg"
            | "dmg"
            | "command"
            | "tool"
            | "workflow"
            | "action"
            | "osascript"
            | "scpt"
            | "scptd"
            | "mobileconfig"
            | "terminal"
            | "inetloc"
            | "webloc"
            | "macho"
            | "dylib"
            // Linux / cross-platform scripts, executables, packages, and disk images
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "csh"
            | "ksh"
            | "run"
            | "bin"
            | "elf"
            | "appimage"
            | "deb"
            | "rpm"
            | "flatpak"
            | "flatpakref"
            | "snap"
            | "desktop"
            | "iso"
            | "img"
            | "vhd"
            | "vhdx"
            | "vmdk"
            | "py"
            | "pyc"
            | "pyw"
            | "rb"
            | "pl"
            | "php"
    )
}

/// Returns `true` when `file_name` has an extension on the refused executable/installer list.
pub fn is_refused_open_file_name(file_name: &str) -> bool {
    real_extension(file_name)
        .as_deref()
        .is_some_and(is_refused_open_extension)
}

/// Single authoritative device-side MIME table derived strictly from the file's real extension
/// (never from untrusted remote listings).
///
/// Refused executable/installer types always map to `"application/octet-stream"`.
pub fn mime_for_file_name(file_name: &str) -> &'static str {
    let Some(ext) = real_extension(file_name) else {
        return "application/octet-stream";
    };
    if is_refused_open_extension(&ext) {
        return "application/octet-stream";
    }
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "tif" | "tiff" => "image/tiff",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" | "csv" | "toml" | "yaml" | "yml" | "ini" | "conf" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "rtf" => "application/rtf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "bz2" => "application/x-bzip2",
        "xz" => "application/x-xz",
        "zst" => "application/zstd",
        "7z" => "application/x-7z-compressed",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odt" => "application/vnd.oasis.opendocument.text",
        "ods" => "application/vnd.oasis.opendocument.spreadsheet",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "epub" => "application/epub+zip",
        _ => "application/octet-stream",
    }
}

/// Formats the Windows Mark-of-the-Web (`Zone.Identifier`) NTFS alternate data stream contents
/// (`ZoneId=3` = Internet zone).
pub fn windows_zone_identifier_payload() -> &'static str {
    "[ZoneTransfer]\r\nZoneId=3\r\n"
}

/// Formats the macOS LaunchServices quarantine extended attribute (`com.apple.quarantine`) value
/// (`0081;<unix_hex>;Legion;`).
pub fn macos_quarantine_value(unix_secs: u64) -> String {
    format!("0081;{unix_secs:08x};Legion;")
}

/// Applies OS download safety attributes to a newly saved download (`0o644` permissions on Unix,
/// `Zone.Identifier` on Windows, `com.apple.quarantine` on macOS). Best-effort on volumes that do
/// not support extended attributes or alternate data streams.
pub fn apply_download_marks(path: &Path, is_cache_file: bool) {
    #[cfg(unix)]
    if !is_cache_file {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    }
    #[cfg(not(unix))]
    let _ = is_cache_file;

    #[cfg(target_os = "windows")]
    {
        let mut ads = path.as_os_str().to_os_string();
        ads.push(":Zone.Identifier");
        let _ = fs::write(PathBuf::from(ads), windows_zone_identifier_payload());
    }

    #[cfg(target_os = "macos")]
    {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let val = macos_quarantine_value(secs);
        let _ = rustix::fs::setxattr(
            path,
            "com.apple.quarantine",
            val.as_bytes(),
            rustix::fs::XattrFlags::empty(),
        );
    }
}

/// Generates collision-avoiding candidate names (`file.ext`, `file (2).ext`, `file (3).ext`, ...)
/// keeping each candidate sanitized and within the byte limit.
pub fn numbered_candidate_name(safe_name: &str, attempt: u32) -> String {
    if attempt <= 1 {
        return safe_name.to_owned();
    }
    let suffix = format!(" ({attempt})");
    let (stem, ext) = split_stem_and_ext(safe_name);
    match ext {
        Some(ext) => {
            let max_stem = MAX_SAFE_FILENAME_BYTES.saturating_sub(suffix.len() + ext.len() + 1);
            let trimmed_stem = truncate_utf8_bytes(stem, max_stem)
                .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
            let base = if trimmed_stem.is_empty() {
                "download"
            } else {
                trimmed_stem
            };
            format!("{base}{suffix}.{ext}")
        }
        None => {
            let max_stem = MAX_SAFE_FILENAME_BYTES.saturating_sub(suffix.len());
            let trimmed_stem = truncate_utf8_bytes(safe_name, max_stem)
                .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
            let base = if trimmed_stem.is_empty() {
                "download"
            } else {
                trimmed_stem
            };
            format!("{base}{suffix}")
        }
    }
}

fn copy_into_exclusive_file(src: &Path, dst: &Path) -> io::Result<()> {
    let mut output = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let copy_res = (|| -> io::Result<()> {
        let mut input = File::open(src)?;
        let mut buf = vec![0u8; PLACE_COPY_BUF_BYTES];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
        }
        output.flush()?;
        let _ = output.sync_all();
        Ok(())
    })();
    if let Err(err) = copy_res {
        drop(output);
        let _ = fs::remove_file(dst);
        return Err(err);
    }
    Ok(())
}

/// Atomically places `src` into `dir` under `desired_name` without EVER overwriting an existing
/// file, retrying with `"name (2).ext"`..`"name (9999).ext"` on collision.
///
/// Uses atomic `hard_link` + unlink on the same filesystem (fails atomically with `AlreadyExists`
/// if another download or program created the destination in between) and falls back to
/// `OpenOptions::create_new(true)` (`O_CREAT | O_EXCL` / `CREATE_NEW`) across filesystems.
pub fn place_file_without_overwrite_sync(
    src: &Path,
    dir: &Path,
    desired_name: &str,
    is_cache_file: bool,
) -> Result<PathBuf, AppError> {
    fs::create_dir_all(dir).map_err(|err| {
        AppError::new(
            ErrorCode::Failed,
            format!("Could not create download folder {}: {err}.", dir.display()),
        )
    })?;
    let safe = sanitize_file_name(desired_name);

    for attempt in 1..=9999u32 {
        let candidate = numbered_candidate_name(&safe, attempt);
        let dst = validate_child_in_dir(dir, &candidate)?;

        // Fast path: atomic hard link never replaces an existing entry.
        match fs::hard_link(src, &dst) {
            Ok(()) => {
                let _ = fs::remove_file(src);
                apply_download_marks(&dst, is_cache_file);
                return Ok(dst);
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => {
                // Cross-filesystem or link-disallowed fallback: exclusive create (`O_EXCL`).
                match copy_into_exclusive_file(src, &dst) {
                    Ok(()) => {
                        let _ = fs::remove_file(src);
                        apply_download_marks(&dst, is_cache_file);
                        return Ok(dst);
                    }
                    Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
                    Err(err) => {
                        return Err(AppError::new(
                            ErrorCode::Failed,
                            format!(
                                "Could not save downloaded file to {}: {err}.",
                                dst.display()
                            ),
                        ));
                    }
                }
            }
        }
    }

    // Final unique fallback using random hex suffix if 9999 numbered collisions exist.
    let mut rand = [0u8; 4];
    let _ = getrandom::fill(&mut rand);
    let suffix = format!(
        "-{:02x}{:02x}{:02x}{:02x}",
        rand[0], rand[1], rand[2], rand[3]
    );
    let (stem, ext) = split_stem_and_ext(&safe);
    let fallback = match ext {
        Some(ext) => format!(
            "{}{suffix}.{ext}",
            truncate_utf8_bytes(
                stem,
                MAX_SAFE_FILENAME_BYTES.saturating_sub(suffix.len() + ext.len() + 1)
            )
        ),
        None => format!(
            "{}{suffix}",
            truncate_utf8_bytes(&safe, MAX_SAFE_FILENAME_BYTES.saturating_sub(suffix.len()))
        ),
    };
    let dst = validate_child_in_dir(dir, &fallback)?;
    match fs::hard_link(src, &dst) {
        Ok(()) => {
            let _ = fs::remove_file(src);
        }
        Err(_) => {
            copy_into_exclusive_file(src, &dst).map_err(|err| {
                AppError::new(
                    ErrorCode::Failed,
                    format!(
                        "Could not save downloaded file to {}: {err}.",
                        dst.display()
                    ),
                )
            })?;
            let _ = fs::remove_file(src);
        }
    }
    apply_download_marks(&dst, is_cache_file);
    Ok(dst)
}

/// Async wrapper around [`place_file_without_overwrite_sync`].
pub async fn place_file_without_overwrite(
    src: &Path,
    dir: &Path,
    desired_name: &str,
    is_cache_file: bool,
) -> Result<PathBuf, AppError> {
    let src = src.to_path_buf();
    let dir = dir.to_path_buf();
    let name = desired_name.to_owned();
    tokio::task::spawn_blocking(move || {
        place_file_without_overwrite_sync(&src, &dir, &name, is_cache_file)
    })
    .await
    .map_err(|err| {
        AppError::new(
            ErrorCode::Internal,
            format!("Download save worker failed: {err}."),
        )
    })?
}

/// Evicts stale or excess files from an `open-externally` cache directory by age, total size, and
/// file count (oldest files removed first).
pub fn prune_cache_dir(dir: &Path, max_files: usize, max_age: Duration, max_total_bytes: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    let mut live: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        let expired = now.duration_since(modified).is_ok_and(|age| age > max_age);
        if expired {
            let _ = fs::remove_file(&path);
            continue;
        }
        live.push((path, modified, meta.len()));
    }
    // Keep newest first; remove oldest entries exceeding file count or byte budget.
    live.sort_by_key(|a| std::cmp::Reverse(a.1));
    let mut kept_count = 0usize;
    let mut kept_bytes = 0u64;
    for (path, _modified, size) in live {
        if kept_count >= max_files || kept_bytes.saturating_add(size) > max_total_bytes {
            let _ = fs::remove_file(&path);
        } else {
            kept_count += 1;
            kept_bytes = kept_bytes.saturating_add(size);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;

    fn assert_strictly_safe_filename(raw: &str, out: &str, dir: &Path) {
        assert!(!out.is_empty(), "empty output for {raw:?}");
        assert!(
            out.len() <= MAX_SAFE_FILENAME_BYTES,
            "output exceeds byte limit ({} > {MAX_SAFE_FILENAME_BYTES}) for {raw:?}: {out:?}",
            out.len()
        );
        assert!(
            !out.starts_with('.') && !out.ends_with('.'),
            "leading/trailing dot in {out:?} for {raw:?}"
        );
        assert!(
            !out.chars().next().unwrap().is_whitespace()
                && !out.chars().last().unwrap().is_whitespace(),
            "leading/trailing whitespace in {out:?} for {raw:?}"
        );
        for c in out.chars() {
            assert!(
                !is_forbidden_filename_char(c),
                "forbidden char {c:?} (U+{:04X}) in {out:?} for {raw:?}",
                c as u32
            );
        }
        assert!(
            !is_reserved_windows_device_name(out),
            "reserved Windows device name {out:?} for {raw:?}"
        );
        let validated = validate_child_in_dir(dir, out).expect("must stay inside dir");
        assert_eq!(validated.parent(), Some(dir));
    }

    #[test]
    fn sanitizer_neutralizes_windows_hostile_names_ads_reserved_devices_and_bidi_spoofing() {
        let dir = tempfile::tempdir().unwrap();

        // Windows drive-relative path (`C:version.dll` would discard base dir in `dir.join` on Windows).
        let drive_rel = sanitize_file_name("C:version.dll");
        assert_eq!(drive_rel, "version.dll");
        assert_strictly_safe_filename("C:version.dll", &drive_rel, dir.path());

        let nested_drive = sanitize_file_name("C:D:payload.exe");
        assert_eq!(nested_drive, "payload.exe");
        assert_strictly_safe_filename("C:D:payload.exe", &nested_drive, dir.path());

        // Alternate data streams.
        assert_eq!(sanitize_file_name("holiday.jpg:evil.exe"), "holiday.jpg");
        assert_eq!(sanitize_file_name("doc.pdf:$DATA"), "doc.pdf");
        assert_eq!(sanitize_file_name(":hidden"), "download");

        // Forbidden characters and traversal.
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_file_name(r"\\server\share\notes?.txt"),
            "notes.txt"
        );
        assert_eq!(sanitize_file_name("a<b>c\"d|e?f*g.png"), "abcdefg.png");

        // Unicode BiDi override (`photo\u{202E}gpj.exe` visually spoofs `photoexe.jpg`).
        let bidi = sanitize_file_name("photo\u{202E}gpj.exe");
        assert_eq!(bidi, "photogpj.exe");
        assert!(is_refused_open_file_name(&bidi));
        assert_strictly_safe_filename("photo\u{202E}gpj.exe", &bidi, dir.path());

        // Reserved Windows device names (with and without extensions, case-insensitive, superscripts).
        for reserved in [
            "CON",
            "con",
            "PRN.txt",
            "aux.jpg",
            "NUL.tar.gz",
            "COM1",
            "com9.png",
            "LPT0.log",
            "LPT9.pdf",
            "COM\u{00B9}.txt",
            "LPT\u{00B3}.jpg",
        ] {
            let out = sanitize_file_name(reserved);
            assert!(
                out.starts_with('_'),
                "expected renamed device for {reserved}: got {out}"
            );
            assert_strictly_safe_filename(reserved, &out, dir.path());
        }

        // Trailing dots, spaces, empty fallbacks.
        assert_eq!(sanitize_file_name("report.pdf.   "), "report.pdf");
        assert_eq!(sanitize_file_name("...   ..."), "download");
        assert_eq!(sanitize_file_name("\u{200B}\u{202E}\u{FEFF}"), "download");
        assert_eq!(sanitize_file_name(""), "download");

        // Long multibyte filename preserves extension and valid UTF-8 under byte cap.
        let long_raw = format!("{}.mp4", "😀".repeat(100));
        let long_out = sanitize_file_name(&long_raw);
        assert!(long_out.ends_with(".mp4"));
        assert_strictly_safe_filename(&long_raw, &long_out, dir.path());
    }

    #[test]
    fn sanitizer_property_test_across_generated_hostile_combinations() {
        let dir = tempfile::tempdir().unwrap();
        let prefixes = [
            "",
            "C:",
            "z:",
            "C:D:",
            "../",
            "..\\",
            "/",
            "\\\\host\\share\\",
            "...",
            "   ",
            "\u{202E}",
            "\u{2066}",
            "\0",
        ];
        let cores = [
            "",
            ".",
            "..",
            "CON",
            "nul",
            "COM1",
            "LPT\u{00B2}",
            "photo",
            "a<>|?*\"b",
            "file\x07\x1F\u{200F}\u{FEFF}name",
            "日本語ファイル名",
            "🚀🎉🔥",
        ];
        let suffixes = [
            "",
            ".jpg",
            ".exe",
            ".apk",
            ":evil.exe",
            ":$DATA",
            ".   ",
            "...",
            "\u{202E}gpj.exe",
            &format!(".{}", "a".repeat(260)),
            &format!("{}.webm", "x".repeat(300)),
        ];

        for p in prefixes {
            for c in cores {
                for s in suffixes {
                    let candidate = format!("{p}{c}{s}");
                    let sanitized = sanitize_file_name(&candidate);
                    assert_strictly_safe_filename(&candidate, &sanitized, dir.path());
                }
            }
        }
    }

    #[test]
    fn validate_child_in_dir_rejects_escapes_and_reserved_names() {
        let dir = tempfile::tempdir().unwrap();
        assert!(validate_child_in_dir(dir.path(), "ok.jpg").is_ok());
        for bad in [
            "",
            ".",
            "..",
            "../escape.txt",
            "sub/file.txt",
            r"sub\file.txt",
            "C:version.dll",
            "CON.txt",
            "file:ads",
            "file\u{202E}gpj.exe",
        ] {
            assert!(
                validate_child_in_dir(dir.path(), bad).is_err(),
                "expected validate_child_in_dir to refuse {bad:?}"
            );
        }
    }

    #[test]
    fn device_mime_table_and_refused_open_list_ignore_remote_claims() {
        // Safe viewer types derive exact device MIME from real extension.
        assert_eq!(mime_for_file_name("holiday.jpg"), "image/jpeg");
        assert_eq!(mime_for_file_name("clip.WEBM"), "video/webm");
        assert_eq!(mime_for_file_name("tickets.pdf"), "application/pdf");
        assert!(!is_refused_open_file_name("holiday.jpg"));
        assert!(!is_refused_open_file_name("tickets.pdf"));

        // Refused executable/installer/script/shortcut/disk-image extensions are blocked and
        // forced to application/octet-stream.
        for dangerous in [
            "update.apk",
            "bundle.apks",
            "setup.exe",
            "install.msi",
            "run.bat",
            "run.cmd",
            "evil.ps1",
            "link.lnk",
            "installer.pkg",
            "disk.dmg",
            "launch.command",
            "script.sh",
            "tool.AppImage",
            "pkg.deb",
            "photo\u{202E}gpj.exe",
            "archive.jar",
            "image.iso",
            "profile.mobileconfig",
        ] {
            assert!(
                is_refused_open_file_name(dangerous),
                "expected {dangerous} to be refused for direct open"
            );
            assert_eq!(mime_for_file_name(dangerous), "application/octet-stream");
        }
    }

    #[test]
    fn place_file_without_overwrite_never_clobbers_under_concurrent_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("parts");
        let dst_dir = dir.path().join("Downloads");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&dst_dir).unwrap();

        // Pre-existing file must survive untouched.
        fs::write(dst_dir.join("photo.jpg"), b"existing-original").unwrap();

        let n = 12usize;
        let mut handles = Vec::new();
        let dst_arc = Arc::new(dst_dir.clone());
        for i in 0..n {
            let part = src_dir.join(format!("p-{i}.part"));
            fs::write(&part, format!("payload-{i}")).unwrap();
            let dst_ref = Arc::clone(&dst_arc);
            handles.push(std::thread::spawn(move || {
                place_file_without_overwrite_sync(&part, &dst_ref, "photo.jpg", false).unwrap()
            }));
        }

        let mut placed_paths = HashSet::new();
        for h in handles {
            let p = h.join().unwrap();
            assert_eq!(p.parent(), Some(dst_dir.as_path()));
            assert!(placed_paths.insert(p), "duplicate path returned under race");
        }
        assert_eq!(placed_paths.len(), n);
        assert_eq!(
            fs::read(dst_dir.join("photo.jpg")).unwrap(),
            b"existing-original"
        );

        let mut seen_payloads: HashSet<String> = HashSet::new();
        for p in &placed_paths {
            seen_payloads.insert(fs::read_to_string(p).unwrap());
        }
        for i in 0..n {
            assert!(
                seen_payloads.contains(&format!("payload-{i}")),
                "missing payload-{i}"
            );
        }
    }

    #[test]
    fn download_marks_payloads_and_cache_pruning_by_age_size_and_count() {
        assert_eq!(
            windows_zone_identifier_payload(),
            "[ZoneTransfer]\r\nZoneId=3\r\n"
        );
        assert_eq!(macos_quarantine_value(0x6500_0001), "0081;65000001;Legion;");

        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            let p = dir.path().join(format!("f-{i}.bin"));
            fs::write(&p, vec![b'x'; 1000]).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        // Size cap of 2500 bytes keeps at most 2 newest files (2000 bytes).
        prune_cache_dir(dir.path(), 10, Duration::from_secs(3600), 2500);
        let remaining: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&"f-4.bin".to_owned()));
        assert!(remaining.contains(&"f-3.bin".to_owned()));

        // Zero max_age prunes all existing files immediately.
        std::thread::sleep(Duration::from_millis(5));
        prune_cache_dir(dir.path(), 10, Duration::ZERO, 10_000);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
