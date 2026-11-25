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

/// Returns the lowercase ASCII alphanumeric real final extension of `file_name` after
/// sanitization, or `None` if there is no extension or if any character lies outside ASCII letters
/// and digits (`A-Z`, `a-z`, `0-9`).
///
/// Treating any non-ASCII-alphanumeric extension as unknown guarantees that Unicode lookalikes
/// such as `x.ap\u{212A}` (Kelvin sign) are handled identically across Rust and Kotlin and are
/// never treated as openable.
pub fn real_extension(file_name: &str) -> Option<String> {
    let safe = sanitize_file_name(file_name);
    let (stem, ext) = safe.rsplit_once('.')?;
    if stem.is_empty()
        || ext.is_empty()
        || ext.len() > MAX_EXTENSION_BYTES
        || !ext.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Returns the lowercase ASCII extension (allowing `-` for Windows compound extensions such as
/// `.search-ms` and `.settingcontent-ms`) used by the defense-in-depth refusal check.
fn refusal_extension(file_name: &str) -> Option<String> {
    let safe = sanitize_file_name(file_name);
    let (stem, ext) = safe.rsplit_once('.')?;
    if stem.is_empty()
        || ext.is_empty()
        || ext.len() > MAX_EXTENSION_BYTES
        || !ext.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Explicit cross-platform allow-list of data types (and their device-derived MIME types) that
/// Legion may offer to open directly via the OS viewer/chooser.
///
/// Only non-executable data formats belong here: raster pictures (explicitly NOT SVG), audio,
/// video, PDF, plain text and the plain-text family (NOT HTML, XML, or SVG), and common
/// macro-free document formats. Every other extension — including archives, HTML/SVG/XML, legacy
/// macro-capable Office formats, scripts, shortcuts, installers, and unknown extensions — is
/// saved to Downloads with `canOpen = false` and "Show in folder".
///
/// Mirrored 1-to-1 from `crates/legion-core/src/media.rs` and into `TransferPathPolicy.kt`;
/// automated drift tests fail on any mismatch (`node scripts/sync-media-table.mjs`).
// BEGIN_ALLOWED_OPEN_MIME_BY_EXT
pub const ALLOWED_OPEN_MIME_BY_EXT: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("jpe", "image/jpeg"),
    ("jfif", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("dib", "image/bmp"),
    ("ico", "image/x-icon"),
    ("cur", "image/x-icon"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("jxl", "image/jxl"),
    ("dng", "image/x-adobe-dng"),
    ("cr2", "image/x-canon-cr2"),
    ("cr3", "image/x-canon-cr3"),
    ("nef", "image/x-nikon-nef"),
    ("arw", "image/x-sony-arw"),
    ("raf", "image/x-fuji-raf"),
    ("orf", "image/x-olympus-orf"),
    ("rw2", "image/x-panasonic-rw2"),
    ("pef", "image/x-pentax-pef"),
    ("srw", "image/x-samsung-srw"),
    ("dcr", "image/x-kodak-dcr"),
    ("kdc", "image/x-kodak-kdc"),
    ("mrw", "image/x-minolta-mrw"),
    ("x3f", "image/x-sigma-x3f"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("qt", "video/quicktime"),
    ("webm", "video/webm"),
    ("mkv", "video/x-matroska"),
    ("3gp", "video/3gpp"),
    ("3g2", "video/3gpp2"),
    ("ogv", "video/ogg"),
    ("mpg", "video/mpeg"),
    ("mpeg", "video/mpeg"),
    ("mpe", "video/mpeg"),
    ("mts", "video/mp2t"),
    ("m2ts", "video/mp2t"),
    ("avi", "video/x-msvideo"),
    ("wmv", "video/x-ms-wmv"),
    ("flv", "video/x-flv"),
    ("asf", "video/x-ms-asf"),
    ("rm", "video/vnd.rn-realvideo"),
    ("rmvb", "video/vnd.rn-realvideo"),
    ("vob", "video/dvd"),
    ("flac", "audio/flac"),
    ("mp3", "audio/mpeg"),
    ("mpga", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("mp4a", "audio/mp4"),
    ("alac", "audio/mp4"),
    ("aac", "audio/mp4"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("spx", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("wav", "audio/wav"),
    ("wave", "audio/wav"),
    ("weba", "audio/webm"),
    ("aif", "audio/aiff"),
    ("aiff", "audio/aiff"),
    ("aifc", "audio/aiff"),
    ("mka", "audio/x-matroska"),
    ("amr", "audio/amr"),
    ("wma", "audio/x-ms-wma"),
    ("mid", "audio/midi"),
    ("midi", "audio/midi"),
    ("ape", "audio/x-ape"),
    ("wv", "audio/x-wavpack"),
    ("caf", "audio/x-caf"),
    ("pdf", "application/pdf"),
    ("epub", "application/epub+zip"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("txt", "text/plain"),
    ("md", "text/plain"),
    ("log", "text/plain"),
    ("toml", "text/plain"),
    ("yaml", "text/plain"),
    ("yml", "text/plain"),
    ("ini", "text/plain"),
    ("conf", "text/plain"),
    ("vtt", "text/plain"),
    ("csv", "text/plain"),
    ("json", "application/json"),
];
// END_ALLOWED_OPEN_MIME_BY_EXT

/// Device-derived MIME mappings for known non-executable download types (archives, web documents,
/// legacy Office formats) that are NOT allowed to open directly (`canOpen = false`).
///
/// Mirrored 1-to-1 from `crates/legion-core/src/media.rs` and into `TransferPathPolicy.kt`;
/// automated drift tests fail on any mismatch (`node scripts/sync-media-table.mjs`).
// BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT
pub const NON_OPEN_DOWNLOAD_MIME_BY_EXT: &[(&str, &str)] = &[
    ("svg", "image/svg+xml"),
    ("rtf", "application/rtf"),
    ("doc", "application/msword"),
    ("xls", "application/vnd.ms-excel"),
    ("ppt", "application/vnd.ms-powerpoint"),
    ("xml", "application/xml"),
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("zip", "application/zip"),
    ("tar", "application/x-tar"),
    ("gz", "application/gzip"),
    ("bz2", "application/x-bzip2"),
    ("xz", "application/x-xz"),
    ("zst", "application/zstd"),
    ("7z", "application/x-7z-compressed"),
    ("rar", "application/vnd.rar"),
];
// END_NON_OPEN_DOWNLOAD_MIME_BY_EXT

/// Defense-in-depth list of executable, installer, script, shortcut, macro-carrier, and disk-image
/// extensions refused behind the allow-list on every platform. If an extension appears on both
/// lists, it is refused and its MIME type is forced to `"application/octet-stream"`.
///
/// Mirrored 1-to-1 in `TransferPathPolicy.kt`; a unit test in this module fails on any drift.
pub const REFUSED_OPEN_EXTENSIONS: &[&str] = &[
    // Android packages and native binaries
    "apk",
    "apks",
    "apkm",
    "xapk",
    "aab",
    "dex",
    "odex",
    "so",
    // Windows executables, installers, scripts, shortcuts, profiles, and Office macro carriers
    "exe",
    "dll",
    "sys",
    "scr",
    "com",
    "msi",
    "msp",
    "msu",
    "mst",
    "bat",
    "cmd",
    "ps1",
    "psm1",
    "psd1",
    "psc1",
    "ps1xml",
    "msh",
    "vbs",
    "vbe",
    "js",
    "jse",
    "mjs",
    "cjs",
    "ws",
    "wsc",
    "wsf",
    "wsh",
    "sct",
    "hta",
    "cpl",
    "msc",
    "inf",
    "reg",
    "lnk",
    "url",
    "pif",
    "scf",
    "gadget",
    "application",
    "rdp",
    "diagcab",
    "settingcontent-ms",
    "appref-ms",
    "library-ms",
    "search-ms",
    "searchconnector-ms",
    "theme",
    "themepack",
    "website",
    "xbap",
    "vsto",
    "cer",
    "msix",
    "msixbundle",
    "appx",
    "appxbundle",
    "jar",
    "jnlp",
    "chm",
    "xll",
    "xla",
    "xlam",
    "docm",
    "xlsm",
    "xlsb",
    "pptm",
    "ppsm",
    "potm",
    "xltm",
    "ppam",
    "dotm",
    "mde",
    "accde",
    "mdb",
    "iqy",
    "slk",
    // macOS executables, bundles, installers, scripts, shortcuts, and disk images
    "app",
    "pkg",
    "mpkg",
    "dmg",
    "command",
    "tool",
    "workflow",
    "action",
    "applescript",
    "osascript",
    "scpt",
    "scptd",
    "mobileconfig",
    "terminal",
    "inetloc",
    "webloc",
    "fileloc",
    "shortcut",
    "macho",
    "dylib",
    // Linux / cross-platform scripts, executables, packages, launchers, and disk images
    "sh",
    "bash",
    "zsh",
    "fish",
    "csh",
    "ksh",
    "run",
    "bin",
    "elf",
    "appimage",
    "deb",
    "rpm",
    "flatpak",
    "flatpakref",
    "flatpakrepo",
    "snap",
    "desktop",
    "kdelnk",
    "ymp",
    "iso",
    "img",
    "vhd",
    "vhdx",
    "vmdk",
    "py",
    "pyc",
    "pyw",
    "pyz",
    "pyzw",
    "rb",
    "pl",
    "php",
];

/// Returns `true` when `ext_lower` is on the defense-in-depth refused extension list.
pub fn is_refused_open_extension(ext_lower: &str) -> bool {
    REFUSED_OPEN_EXTENSIONS.contains(&ext_lower)
}

/// Returns `true` when `file_name` has an extension on the defense-in-depth refused list.
pub fn is_refused_open_file_name(file_name: &str) -> bool {
    refusal_extension(file_name)
        .as_deref()
        .is_some_and(is_refused_open_extension)
}

/// Returns `true` when `ext_lower` is on the explicit direct-open allow-list and NOT refused.
pub fn is_allowed_open_extension(ext_lower: &str) -> bool {
    if is_refused_open_extension(ext_lower) {
        return false;
    }
    ALLOWED_OPEN_MIME_BY_EXT
        .iter()
        .any(|&(ext, _)| ext == ext_lower)
}

/// Returns `true` ONLY when `file_name` has a pure ASCII-alphanumeric extension on the explicit
/// direct-open allow-list and is NOT on the defense-in-depth refused list.
pub fn can_open_file_name(file_name: &str) -> bool {
    if is_refused_open_file_name(file_name) {
        return false;
    }
    real_extension(file_name)
        .as_deref()
        .is_some_and(is_allowed_open_extension)
}

/// Single authoritative device-side MIME table derived strictly from the file's real ASCII
/// extension (never from untrusted remote listings).
///
/// Unknown, non-ASCII, and refused executable/installer extensions always map to
/// `"application/octet-stream"`.
pub fn mime_for_file_name(file_name: &str) -> &'static str {
    if is_refused_open_file_name(file_name) {
        return "application/octet-stream";
    }
    let Some(ext) = real_extension(file_name) else {
        return "application/octet-stream";
    };
    if is_refused_open_extension(&ext) {
        return "application/octet-stream";
    }
    for &(allowed_ext, mime) in ALLOWED_OPEN_MIME_BY_EXT {
        if allowed_ext == ext {
            return mime;
        }
    }
    for &(other_ext, mime) in NON_OPEN_DOWNLOAD_MIME_BY_EXT {
        if other_ext == ext {
            return mime;
        }
    }
    "application/octet-stream"
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
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

    #[test]
    fn open_allow_list_blocks_dangerous_web_archives_unknown_and_kelvin_sign() {
        // Every allow-listed data extension is openable (case-insensitive and with `(2)` suffix).
        for &(ext, expected_mime) in ALLOWED_OPEN_MIME_BY_EXT {
            let lower = format!("sample.{ext}");
            let upper = format!("SAMPLE.{}", ext.to_ascii_uppercase());
            let numbered = format!("sample (2).{ext}");
            assert!(
                can_open_file_name(&lower),
                "expected {lower} to be allowed for direct open"
            );
            assert!(
                can_open_file_name(&upper),
                "expected {upper} to be allowed for direct open"
            );
            assert!(
                can_open_file_name(&numbered),
                "expected {numbered} to be allowed for direct open"
            );
            assert_eq!(mime_for_file_name(&lower), expected_mime);
        }

        // All dangerous desktop/mobile extensions named in the security check must be blocked AND
        // on the defense-in-depth refused list with application/octet-stream MIME.
        for dangerous_ext in [
            "pyz",
            "pyzw",
            "ws",
            "wsc",
            "sct",
            "application",
            "rdp",
            "diagcab",
            "library-ms",
            "search-ms",
            "searchConnector-ms",
            "theme",
            "themepack",
            "website",
            "xbap",
            "vsto",
            "psc1",
            "ps1xml",
            "msh",
            "cer",
            "xlsb",
            "ppsm",
            "potm",
            "xltm",
            "ppam",
            "xla",
            "mde",
            "accde",
            "mdb",
            "iqy",
            "slk",
            "fileloc",
            "shortcut",
            "kdelnk",
            "ymp",
            "flatpakrepo",
        ] {
            let name = format!("payload.{dangerous_ext}");
            assert!(
                !can_open_file_name(&name),
                "expected {name} to be blocked from direct open"
            );
            assert!(
                is_refused_open_file_name(&name),
                "expected {name} to be on the defense-in-depth refused list"
            );
            assert_eq!(
                mime_for_file_name(&name),
                "application/octet-stream",
                "expected refused type {name} to map to application/octet-stream"
            );
        }

        // Archives, HTML/SVG/XML, CSS, legacy Office, unknown extensions, and extensionless files
        // are NEVER openable directly.
        for blocked in [
            "page.html",
            "page.HTM",
            "diagram.svg",
            "feed.xml",
            "style.css",
            "archive.zip",
            "backup.tar",
            "backup.tar.gz",
            "data.gz",
            "data.bz2",
            "data.xz",
            "data.zst",
            "data.7z",
            "data.rar",
            "legacy.doc",
            "legacy.xls",
            "legacy.ppt",
            "legacy.rtf",
            "blob.unknown",
            "blob.xyz123",
            "README",
        ] {
            assert!(
                !can_open_file_name(blocked),
                "expected {blocked} to be blocked by the strict open allow-list"
            );
        }

        // Non-ASCII Kelvin sign (`x.ap\u{212A}`) is an unknown extension on BOTH Rust and Kotlin.
        let kelvin = "x.ap\u{212A}";
        assert_eq!(real_extension(kelvin), None);
        assert!(!can_open_file_name(kelvin));
        assert!(!is_refused_open_file_name(kelvin));
        assert_eq!(mime_for_file_name(kelvin), "application/octet-stream");
    }

    #[test]
    fn open_policy_tables_match_kotlin_without_drift() {
        use std::collections::BTreeMap;

        // First verify internal invariants on Rust tables.
        let rust_allowed: BTreeMap<String, String> = ALLOWED_OPEN_MIME_BY_EXT
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let rust_non_open: BTreeMap<String, String> = NON_OPEN_DOWNLOAD_MIME_BY_EXT
            .iter()
            .map(|&(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let rust_refused: HashSet<String> = REFUSED_OPEN_EXTENSIONS
            .iter()
            .map(|&s| s.to_owned())
            .collect();

        assert_eq!(
            rust_allowed.len(),
            ALLOWED_OPEN_MIME_BY_EXT.len(),
            "duplicate key in Rust ALLOWED_OPEN_MIME_BY_EXT"
        );
        assert_eq!(
            rust_non_open.len(),
            NON_OPEN_DOWNLOAD_MIME_BY_EXT.len(),
            "duplicate key in Rust NON_OPEN_DOWNLOAD_MIME_BY_EXT"
        );
        assert_eq!(
            rust_refused.len(),
            REFUSED_OPEN_EXTENSIONS.len(),
            "duplicate entry in Rust REFUSED_OPEN_EXTENSIONS"
        );

        for ext in rust_allowed.keys() {
            assert!(
                !ext.is_empty()
                    && ext
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "allowed extension {ext:?} must be lowercase ASCII alphanumeric"
            );
            assert!(
                !rust_refused.contains(ext),
                "extension {ext:?} appears in both ALLOWED and REFUSED lists"
            );
            assert!(
                !rust_non_open.contains_key(ext),
                "extension {ext:?} appears in both ALLOWED and NON_OPEN lists"
            );
        }

        // Parse Kotlin TransferPathPolicy.kt tables directly so any drift fails cargo test.
        let kt_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("gen/android/app/src/main/java/com/x1f4r/legion/TransferPathPolicy.kt");
        let kt_src = fs::read_to_string(&kt_path)
            .expect("TransferPathPolicy.kt must exist beside Rust crate")
            .replace("\r\n", "\n");

        let extract_block = |begin: &str, end: &str| -> String {
            let start_idx = kt_src
                .find(begin)
                .unwrap_or_else(|| panic!("missing marker {begin} in TransferPathPolicy.kt"));
            let rest = &kt_src[start_idx + begin.len()..];
            let end_idx = rest
                .find(end)
                .unwrap_or_else(|| panic!("missing marker {end} in TransferPathPolicy.kt"));
            rest[..end_idx].to_owned()
        };

        let parse_kotlin_map = |block: &str| -> BTreeMap<String, String> {
            let mut map = BTreeMap::new();
            for line in block.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with("//") {
                    continue;
                }
                if let Some((lhs, rhs)) = trimmed.split_once(" to ") {
                    let key = lhs.trim().trim_matches('"');
                    let val = rhs.trim().trim_end_matches(',').trim().trim_matches('"');
                    if !key.is_empty() && !val.is_empty() {
                        let prev = map.insert(key.to_owned(), val.to_owned());
                        assert!(prev.is_none(), "duplicate Kotlin map key {key:?}");
                    }
                }
            }
            map
        };

        let parse_kotlin_set = |block: &str| -> HashSet<String> {
            let mut set = HashSet::new();
            for line in block.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty()
                    || trimmed.starts_with("//")
                    || trimmed.contains("setOf(")
                    || trimmed == ")"
                {
                    continue;
                }
                let mut in_str = false;
                let mut cur = String::new();
                for ch in trimmed.chars() {
                    if ch == '"' {
                        if in_str {
                            assert!(
                                set.insert(cur.clone()),
                                "duplicate Kotlin refused extension {cur:?}"
                            );
                            cur.clear();
                            in_str = false;
                        } else {
                            in_str = true;
                        }
                    } else if in_str {
                        cur.push(ch);
                    }
                }
            }
            set
        };

        let kt_allowed = parse_kotlin_map(&extract_block(
            "BEGIN_ALLOWED_OPEN_MIME_BY_EXT",
            "END_ALLOWED_OPEN_MIME_BY_EXT",
        ));
        let kt_non_open = parse_kotlin_map(&extract_block(
            "BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT",
            "END_NON_OPEN_DOWNLOAD_MIME_BY_EXT",
        ));
        let kt_refused = parse_kotlin_set(&extract_block(
            "BEGIN_REFUSED_OPEN_EXTENSIONS",
            "END_REFUSED_OPEN_EXTENSIONS",
        ));

        assert_eq!(
            rust_allowed, kt_allowed,
            "Rust ALLOWED_OPEN_MIME_BY_EXT drifted from Kotlin TransferPathPolicy.kt"
        );
        assert_eq!(
            rust_non_open, kt_non_open,
            "Rust NON_OPEN_DOWNLOAD_MIME_BY_EXT drifted from Kotlin TransferPathPolicy.kt"
        );
        assert_eq!(
            rust_refused, kt_refused,
            "Rust REFUSED_OPEN_EXTENSIONS drifted from Kotlin TransferPathPolicy.kt"
        );
    }

    #[test]
    fn open_allow_list_property_test_10000_generated_names() {
        let allowed_set: HashSet<&str> = ALLOWED_OPEN_MIME_BY_EXT
            .iter()
            .map(|&(ext, _)| ext)
            .collect();
        let refused_set: HashSet<&str> = REFUSED_OPEN_EXTENSIONS.iter().copied().collect();

        // Deterministic xorshift64 PRNG so test failures are 100% reproducible.
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next_u64 = || -> u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let prefixes = [
            "",
            "dir/",
            "C:\\Users\\me\\",
            "../../",
            "C:",
            "\u{202E}",
            "  .",
        ];
        let stems = [
            "file",
            "holiday (2)",
            "CON",
            "archive.tar",
            "photo.jpg",
            "doc\u{202E}gpj",
            "report_2026",
            "日本語",
            "x",
        ];
        let exotic_chars: &[char] = &[
            'a', 'z', 'A', 'Z', '0', '9', '-', '_', '.', ' ', '\u{212A}', '\u{0130}', '\u{0131}',
            '\u{0430}', '\u{202E}', 'é',
        ];
        let extra_exts = [
            "html",
            "svg",
            "xml",
            "zip",
            "tar",
            "gz",
            "doc",
            "xls",
            "ppt",
            "rtf",
            "apk",
            "exe",
            "rdp",
            "xlsb",
            "search-ms",
            "ap\u{212A}",
            "jp\u{0430}",
            "pd\u{017F}",
            "unknown",
            "",
        ];

        let mut openable_count = 0usize;
        let mut blocked_count = 0usize;
        const TOTAL: usize = 10_500;

        for i in 0..TOTAL {
            let prefix = prefixes[(next_u64() as usize) % prefixes.len()];
            let stem = stems[(next_u64() as usize) % stems.len()];
            let candidate = match i % 5 {
                // Mix of allow-listed extensions (various casings and trailing spaces/dots).
                0 => {
                    let &(ext, _) = ALLOWED_OPEN_MIME_BY_EXT
                        .get((next_u64() as usize) % ALLOWED_OPEN_MIME_BY_EXT.len())
                        .unwrap();
                    let ext_cased = if next_u64().is_multiple_of(2) {
                        ext.to_ascii_uppercase()
                    } else {
                        ext.to_owned()
                    };
                    let trail = if next_u64().is_multiple_of(4) {
                        ". "
                    } else {
                        ""
                    };
                    format!("{prefix}{stem}.{ext_cased}{trail}")
                }
                // Refused extensions (with optional BiDi or ADS suffix).
                1 => {
                    let ext = REFUSED_OPEN_EXTENSIONS
                        [(next_u64() as usize) % REFUSED_OPEN_EXTENSIONS.len()];
                    let ads = if next_u64().is_multiple_of(3) {
                        ":$DATA"
                    } else {
                        ""
                    };
                    format!("{prefix}{stem}.{ext}{ads}")
                }
                // Explicitly blocked non-allowed & Unicode-lookalike extensions.
                2 => {
                    let ext = extra_exts[(next_u64() as usize) % extra_exts.len()];
                    if ext.is_empty() {
                        format!("{prefix}{stem}")
                    } else {
                        format!("{prefix}{stem}.{ext}")
                    }
                }
                // Random pure ASCII-alphanumeric extension of length 1..=8.
                3 => {
                    let len = 1 + ((next_u64() as usize) % 8);
                    let mut ext = String::with_capacity(len);
                    for _ in 0..len {
                        let idx = (next_u64() as usize) % 62;
                        let ch = match idx {
                            0..=25 => (b'a' + idx as u8) as char,
                            26..=51 => (b'A' + (idx - 26) as u8) as char,
                            _ => (b'0' + (idx - 52) as u8) as char,
                        };
                        ext.push(ch);
                    }
                    format!("{prefix}{stem}.{ext}")
                }
                // Random exotic/Unicode/punctuation extension of length 1..=10.
                _ => {
                    let len = 1 + ((next_u64() as usize) % 10);
                    let mut ext = String::with_capacity(len);
                    for _ in 0..len {
                        ext.push(exotic_chars[(next_u64() as usize) % exotic_chars.len()]);
                    }
                    format!("{prefix}{stem}.{ext}")
                }
            };

            if can_open_file_name(&candidate) {
                openable_count += 1;
                let ext = real_extension(&candidate).unwrap_or_else(|| {
                    panic!(
                        "can_open_file_name returned true without real_extension for {candidate:?}"
                    )
                });
                assert!(
                    ext.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                    "openable name {candidate:?} had non-ASCII-alphanumeric ext {ext:?}"
                );
                assert!(
                    allowed_set.contains(ext.as_str()),
                    "openable name {candidate:?} had ext {ext:?} NOT on allow-list"
                );
                assert!(
                    !refused_set.contains(ext.as_str()) && !is_refused_open_file_name(&candidate),
                    "openable name {candidate:?} matched refused list"
                );
            } else {
                blocked_count += 1;
            }
        }

        assert!(
            openable_count >= 1000 && blocked_count >= 5000,
            "property test must exercise both openable ({openable_count}) and blocked ({blocked_count}) cases"
        );
    }
}
