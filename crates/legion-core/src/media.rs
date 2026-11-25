//! File categories, media types, content-serving policies, and viewer hints, classified strictly
//! by file extension from one canonical table ([`MEDIA_TABLE`]).
//!
//! Nothing on disk is opened to guess a file's type: [`classify_extension`], [`classify_path`],
//! and [`lookup_extension`] look up the extension case-insensitively in [`MEDIA_TABLE`] and fall
//! back to [`Category::Other`] with `"application/octet-stream"`.
//!
//! [`MEDIA_TABLE`] is the single source of truth for file-type metadata across the workspace.
//! Downstream mirrors (`crates/legion-content/src/media.rs`,
//! `apps/legion-app/src-tauri/src/transfers/file_policy.rs`,
//! `apps/legion-app/src-tauri/gen/android/app/src/main/java/com/x1f4r/legion/TransferPathPolicy.kt`,
//! and `apps/ui/lib/files.js`) are generated from this table by `node scripts/sync-media-table.mjs`
//! and checked for drift by automated unit tests.
//!
//! # Security: never serve inline `Content-Type` directly from `mime`
//!
//! [`MediaInfo::mime`] is for display, filtering, and client hints — **not** an HTTP
//! inline-serving allow-list. Several media types returned here execute script, load external
//! resources, or interpret active content if served inline in a browser:
//!
//! - `image/svg+xml` (`.svg`)
//! - `text/html` (`.html`, `.htm`, `.xhtml`)
//! - `application/xml` (`.xml`, `.xsl`, `.xsd`, `.plist`)
//! - `text/javascript` (`.js`, `.mjs`, `.cjs`, `.jsx`)
//! - `text/css` (`.css`, `.scss`, `.sass`, `.less`)
//! - `application/pdf` (`.pdf`)
//!
//! HTTP content endpoints must use [`MediaEntry::content_serve`] (which flattens every text/code/
//! markup type to `text/plain; charset=utf-8`, forces `Content-Disposition: attachment` on
//! non-allow-listed types including PDF, and pairs every reply with `X-Content-Type-Options:
//! nosniff` and `Content-Security-Policy: sandbox; default-src 'none'; frame-ancestors 'none'`).
//! Direct external opening on devices must use [`MediaEntry::open_externally_mime`], which is a
//! separate per-type allow-list restricted to safe non-executable data formats.

use std::fmt;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Coarse kind of a regular file, derived solely from its file extension.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Raster, vector, and camera raw pictures.
    Image,
    /// Video containers and streams.
    Video,
    /// Audio files and music.
    Audio,
    /// Office documents, PDFs and e-books.
    Document,
    /// Plain text, markup, configuration files and source code.
    Text,
    /// Compressed archives and disk or package containers.
    Archive,
    /// Everything without a recognized extension.
    Other,
}

impl Category {
    /// The category as JSON and tools spell it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Category::Image => "image",
            Category::Video => "video",
            Category::Audio => "audio",
            Category::Document => "document",
            Category::Text => "text",
            Category::Archive => "archive",
            Category::Other => "other",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The category and media type of a file, derived from its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaInfo {
    /// Coarse file category.
    pub category: Category,
    /// MIME media type (without parameters).
    pub mime: &'static str,
}

/// How `legion-content` serves a file extension over HTTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentServe {
    /// Serve inline with the specified safe media `Content-Type` and unconditional sandbox CSP.
    InlineMedia(&'static str),
    /// Serve inline as `text/plain; charset=utf-8` (never as active HTML/XML/JS/CSS).
    InlineText,
    /// Serve inline as `text/vtt; charset=utf-8` for `<track>` subtitle elements.
    InlineVtt,
    /// Serve as `application/octet-stream` with `Content-Disposition: attachment`.
    Attachment,
}

/// How the Legion UI viewer should open a file extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewerMode {
    /// Try the platform's inline `<img>`, `<video>`, or `<audio>` element directly.
    Inline,
    /// Picture format known not to decode directly in WebView (HEIC on non-Apple WebView, TIFF,
    /// camera raw): request a machine-made `fs.thumb` preview first, otherwise show an honest
    /// fallback card without wasting bandwidth on an `<img>` fetch.
    ThumbOrFallback,
    /// Open in the bounded `fs.read` plain-text viewer (never rendered as HTML/XML).
    Text,
    /// Skip the media player/viewer and go straight to the kind icon, honest sentence, and
    /// available actions (`Open in another app` only when allowed, plus `Download`).
    Fallback,
}

impl ViewerMode {
    /// Stable wire/JS spelling of this viewer mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            ViewerMode::Inline => "inline",
            ViewerMode::ThumbOrFallback => "thumb_or_fallback",
            ViewerMode::Text => "text",
            ViewerMode::Fallback => "fallback",
        }
    }
}

/// One canonical entry in [`MEDIA_TABLE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaEntry {
    /// Lowercase ASCII alphanumeric extension without leading dot.
    pub ext: &'static str,
    /// Coarse category used for listings, search filters, and UI grouping.
    pub category: Category,
    /// Canonical MIME type reported by the node in listings.
    pub mime: &'static str,
    /// Safe HTTP content-serving rule mirrored into `legion-content`.
    pub content_serve: ContentServe,
    /// Deliberate per-type allow-list for "Open in another app" / notification-tap chooser on
    /// devices. `Some(device_mime)` ONLY for safe non-executable data formats; `None` for
    /// executables, scripts, HTML, XML, SVG, archives, legacy/macro Office, and code.
    pub open_externally_mime: Option<&'static str>,
    /// Optional device-side MIME used when saving a non-openable file to Downloads (e.g.
    /// `application/zip` for `.zip`, `image/svg+xml` for `.svg`), never set for refused
    /// executable/installer/script extensions.
    pub download_mime: Option<&'static str>,
    /// Viewer presentation rule mirrored into `apps/ui/lib/files.js`.
    pub viewer_mode: ViewerMode,
}

const OTHER: MediaInfo = MediaInfo {
    category: Category::Other,
    mime: "application/octet-stream",
};

const fn entry(
    ext: &'static str,
    category: Category,
    mime: &'static str,
    content_serve: ContentServe,
    open_externally_mime: Option<&'static str>,
    download_mime: Option<&'static str>,
    viewer_mode: ViewerMode,
) -> MediaEntry {
    MediaEntry {
        ext,
        category,
        mime,
        content_serve,
        open_externally_mime,
        download_mime,
        viewer_mode,
    }
}

/// Single canonical media-type table for Legion.
///
/// Downstream mirrors are generated from the rows between `BEGIN_MEDIA_TABLE` and
/// `END_MEDIA_TABLE` by `node scripts/sync-media-table.mjs`.
// BEGIN_MEDIA_TABLE
#[rustfmt::skip]
pub const MEDIA_TABLE: &[MediaEntry] = &[
    // ── Pictures (inline-decodable raster) ───────────────────────────────────────────────────────
    entry("jpg", Category::Image, "image/jpeg", ContentServe::InlineMedia("image/jpeg"), Some("image/jpeg"), None, ViewerMode::Inline),
    entry("jpeg", Category::Image, "image/jpeg", ContentServe::InlineMedia("image/jpeg"), Some("image/jpeg"), None, ViewerMode::Inline),
    entry("jpe", Category::Image, "image/jpeg", ContentServe::InlineMedia("image/jpeg"), Some("image/jpeg"), None, ViewerMode::Inline),
    entry("jfif", Category::Image, "image/jpeg", ContentServe::InlineMedia("image/jpeg"), Some("image/jpeg"), None, ViewerMode::Inline),
    entry("png", Category::Image, "image/png", ContentServe::InlineMedia("image/png"), Some("image/png"), None, ViewerMode::Inline),
    entry("gif", Category::Image, "image/gif", ContentServe::InlineMedia("image/gif"), Some("image/gif"), None, ViewerMode::Inline),
    entry("webp", Category::Image, "image/webp", ContentServe::InlineMedia("image/webp"), Some("image/webp"), None, ViewerMode::Inline),
    entry("avif", Category::Image, "image/avif", ContentServe::InlineMedia("image/avif"), Some("image/avif"), None, ViewerMode::Inline),
    entry("bmp", Category::Image, "image/bmp", ContentServe::InlineMedia("image/bmp"), Some("image/bmp"), None, ViewerMode::Inline),
    entry("dib", Category::Image, "image/bmp", ContentServe::InlineMedia("image/bmp"), Some("image/bmp"), None, ViewerMode::Inline),
    entry("ico", Category::Image, "image/x-icon", ContentServe::InlineMedia("image/x-icon"), Some("image/x-icon"), None, ViewerMode::Inline),
    entry("cur", Category::Image, "image/x-icon", ContentServe::InlineMedia("image/x-icon"), Some("image/x-icon"), None, ViewerMode::Inline),
    // SVG: rendered only inside `<img>` (never as a document, always sandbox CSP) and NEVER opened externally.
    // `.svgz` is gzip-compressed SVG and intentionally falls through to `Other`.
    entry("svg", Category::Image, "image/svg+xml", ContentServe::InlineMedia("image/svg+xml"), None, Some("image/svg+xml"), ViewerMode::Inline),
    // ── Pictures (not directly decodable by WebView -> fs.thumb preview or fallback) ─────────────
    entry("heic", Category::Image, "image/heic", ContentServe::InlineMedia("image/heic"), Some("image/heic"), None, ViewerMode::ThumbOrFallback),
    entry("heif", Category::Image, "image/heif", ContentServe::InlineMedia("image/heif"), Some("image/heif"), None, ViewerMode::ThumbOrFallback),
    entry("tif", Category::Image, "image/tiff", ContentServe::InlineMedia("image/tiff"), Some("image/tiff"), None, ViewerMode::ThumbOrFallback),
    entry("tiff", Category::Image, "image/tiff", ContentServe::InlineMedia("image/tiff"), Some("image/tiff"), None, ViewerMode::ThumbOrFallback),
    entry("jxl", Category::Image, "image/jxl", ContentServe::Attachment, Some("image/jxl"), None, ViewerMode::ThumbOrFallback),
    entry("psd", Category::Image, "image/vnd.adobe.photoshop", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    // Camera raw formats (picture category, never decoded directly by WebView, allowed to open in another app)
    entry("dng", Category::Image, "image/x-adobe-dng", ContentServe::Attachment, Some("image/x-adobe-dng"), None, ViewerMode::ThumbOrFallback),
    entry("cr2", Category::Image, "image/x-canon-cr2", ContentServe::Attachment, Some("image/x-canon-cr2"), None, ViewerMode::ThumbOrFallback),
    entry("cr3", Category::Image, "image/x-canon-cr3", ContentServe::Attachment, Some("image/x-canon-cr3"), None, ViewerMode::ThumbOrFallback),
    entry("nef", Category::Image, "image/x-nikon-nef", ContentServe::Attachment, Some("image/x-nikon-nef"), None, ViewerMode::ThumbOrFallback),
    entry("arw", Category::Image, "image/x-sony-arw", ContentServe::Attachment, Some("image/x-sony-arw"), None, ViewerMode::ThumbOrFallback),
    entry("raf", Category::Image, "image/x-fuji-raf", ContentServe::Attachment, Some("image/x-fuji-raf"), None, ViewerMode::ThumbOrFallback),
    entry("orf", Category::Image, "image/x-olympus-orf", ContentServe::Attachment, Some("image/x-olympus-orf"), None, ViewerMode::ThumbOrFallback),
    entry("rw2", Category::Image, "image/x-panasonic-rw2", ContentServe::Attachment, Some("image/x-panasonic-rw2"), None, ViewerMode::ThumbOrFallback),
    entry("pef", Category::Image, "image/x-pentax-pef", ContentServe::Attachment, Some("image/x-pentax-pef"), None, ViewerMode::ThumbOrFallback),
    entry("srw", Category::Image, "image/x-samsung-srw", ContentServe::Attachment, Some("image/x-samsung-srw"), None, ViewerMode::ThumbOrFallback),
    entry("dcr", Category::Image, "image/x-kodak-dcr", ContentServe::Attachment, Some("image/x-kodak-dcr"), None, ViewerMode::ThumbOrFallback),
    entry("kdc", Category::Image, "image/x-kodak-kdc", ContentServe::Attachment, Some("image/x-kodak-kdc"), None, ViewerMode::ThumbOrFallback),
    entry("mrw", Category::Image, "image/x-minolta-mrw", ContentServe::Attachment, Some("image/x-minolta-mrw"), None, ViewerMode::ThumbOrFallback),
    entry("x3f", Category::Image, "image/x-sigma-x3f", ContentServe::Attachment, Some("image/x-sigma-x3f"), None, ViewerMode::ThumbOrFallback),
    entry("tga", Category::Image, "image/x-tga", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("ppm", Category::Image, "image/x-portable-pixmap", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("pgm", Category::Image, "image/x-portable-graymap", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("pbm", Category::Image, "image/x-portable-bitmap", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("pnm", Category::Image, "image/x-portable-anymap", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("qoi", Category::Image, "image/qoi", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("dds", Category::Image, "image/vnd.ms-dds", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("exr", Category::Image, "image/x-exr", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),
    entry("hdr", Category::Image, "image/vnd.radiance", ContentServe::Attachment, None, None, ViewerMode::ThumbOrFallback),

    // ── Video (containers WebView may decode -> try player) ──────────────────────────────────────
    entry("mp4", Category::Video, "video/mp4", ContentServe::InlineMedia("video/mp4"), Some("video/mp4"), None, ViewerMode::Inline),
    entry("m4v", Category::Video, "video/x-m4v", ContentServe::InlineMedia("video/mp4"), Some("video/mp4"), None, ViewerMode::Inline),
    entry("mov", Category::Video, "video/quicktime", ContentServe::InlineMedia("video/quicktime"), Some("video/quicktime"), None, ViewerMode::Inline),
    entry("qt", Category::Video, "video/quicktime", ContentServe::InlineMedia("video/quicktime"), Some("video/quicktime"), None, ViewerMode::Inline),
    entry("webm", Category::Video, "video/webm", ContentServe::InlineMedia("video/webm"), Some("video/webm"), None, ViewerMode::Inline),
    entry("mkv", Category::Video, "video/x-matroska", ContentServe::InlineMedia("video/x-matroska"), Some("video/x-matroska"), None, ViewerMode::Inline),
    entry("3gp", Category::Video, "video/3gpp", ContentServe::InlineMedia("video/3gpp"), Some("video/3gpp"), None, ViewerMode::Inline),
    entry("3g2", Category::Video, "video/3gpp2", ContentServe::InlineMedia("video/3gpp2"), Some("video/3gpp2"), None, ViewerMode::Inline),
    entry("ogv", Category::Video, "video/ogg", ContentServe::InlineMedia("video/ogg"), Some("video/ogg"), None, ViewerMode::Inline),
    entry("mpg", Category::Video, "video/mpeg", ContentServe::InlineMedia("video/mpeg"), Some("video/mpeg"), None, ViewerMode::Inline),
    entry("mpeg", Category::Video, "video/mpeg", ContentServe::InlineMedia("video/mpeg"), Some("video/mpeg"), None, ViewerMode::Inline),
    entry("mpe", Category::Video, "video/mpeg", ContentServe::InlineMedia("video/mpeg"), Some("video/mpeg"), None, ViewerMode::Inline),
    // AVCHD / Blu-ray MPEG transport streams. Plain `.ts` stays mapped to TypeScript below so
    // `fs.thumb` never spawns `ffmpeg` on TypeScript source trees; the viewer probes binary `.ts`
    // files separately.
    entry("mts", Category::Video, "video/mp2t", ContentServe::InlineMedia("video/mp2t"), Some("video/mp2t"), None, ViewerMode::Inline),
    entry("m2ts", Category::Video, "video/mp2t", ContentServe::InlineMedia("video/mp2t"), Some("video/mp2t"), None, ViewerMode::Inline),
    // ── Video (known not to decode in WebView -> skip player, open in another app) ───────────────
    entry("avi", Category::Video, "video/x-msvideo", ContentServe::InlineMedia("video/x-msvideo"), Some("video/x-msvideo"), None, ViewerMode::Fallback),
    entry("wmv", Category::Video, "video/x-ms-wmv", ContentServe::InlineMedia("video/x-ms-wmv"), Some("video/x-ms-wmv"), None, ViewerMode::Fallback),
    entry("flv", Category::Video, "video/x-flv", ContentServe::InlineMedia("video/x-flv"), Some("video/x-flv"), None, ViewerMode::Fallback),
    entry("asf", Category::Video, "video/x-ms-asf", ContentServe::InlineMedia("video/x-ms-asf"), Some("video/x-ms-asf"), None, ViewerMode::Fallback),
    entry("rm", Category::Video, "video/vnd.rn-realvideo", ContentServe::Attachment, Some("video/vnd.rn-realvideo"), None, ViewerMode::Fallback),
    entry("rmvb", Category::Video, "video/vnd.rn-realvideo", ContentServe::Attachment, Some("video/vnd.rn-realvideo"), None, ViewerMode::Fallback),
    entry("vob", Category::Video, "video/dvd", ContentServe::Attachment, Some("video/dvd"), None, ViewerMode::Fallback),

    // ── Audio (containers WebView may decode -> try player) ──────────────────────────────────────
    entry("flac", Category::Audio, "audio/flac", ContentServe::InlineMedia("audio/flac"), Some("audio/flac"), None, ViewerMode::Inline),
    entry("mp3", Category::Audio, "audio/mpeg", ContentServe::InlineMedia("audio/mpeg"), Some("audio/mpeg"), None, ViewerMode::Inline),
    entry("mpga", Category::Audio, "audio/mpeg", ContentServe::InlineMedia("audio/mpeg"), Some("audio/mpeg"), None, ViewerMode::Inline),
    entry("m4a", Category::Audio, "audio/mp4", ContentServe::InlineMedia("audio/mp4"), Some("audio/mp4"), None, ViewerMode::Inline),
    entry("m4b", Category::Audio, "audio/mp4", ContentServe::InlineMedia("audio/mp4"), Some("audio/mp4"), None, ViewerMode::Inline),
    entry("mp4a", Category::Audio, "audio/mp4", ContentServe::InlineMedia("audio/mp4"), Some("audio/mp4"), None, ViewerMode::Inline),
    entry("alac", Category::Audio, "audio/mp4", ContentServe::InlineMedia("audio/mp4"), Some("audio/mp4"), None, ViewerMode::Inline),
    entry("aac", Category::Audio, "audio/aac", ContentServe::InlineMedia("audio/aac"), Some("audio/mp4"), None, ViewerMode::Inline),
    entry("ogg", Category::Audio, "audio/ogg", ContentServe::InlineMedia("audio/ogg"), Some("audio/ogg"), None, ViewerMode::Inline),
    entry("oga", Category::Audio, "audio/ogg", ContentServe::InlineMedia("audio/ogg"), Some("audio/ogg"), None, ViewerMode::Inline),
    entry("spx", Category::Audio, "audio/ogg", ContentServe::InlineMedia("audio/ogg"), Some("audio/ogg"), None, ViewerMode::Inline),
    entry("opus", Category::Audio, "audio/opus", ContentServe::InlineMedia("audio/opus"), Some("audio/ogg"), None, ViewerMode::Inline),
    entry("wav", Category::Audio, "audio/wav", ContentServe::InlineMedia("audio/wav"), Some("audio/wav"), None, ViewerMode::Inline),
    entry("wave", Category::Audio, "audio/wav", ContentServe::InlineMedia("audio/wav"), Some("audio/wav"), None, ViewerMode::Inline),
    entry("weba", Category::Audio, "audio/webm", ContentServe::InlineMedia("audio/webm"), Some("audio/webm"), None, ViewerMode::Inline),
    entry("aif", Category::Audio, "audio/aiff", ContentServe::InlineMedia("audio/aiff"), Some("audio/aiff"), None, ViewerMode::Inline),
    entry("aiff", Category::Audio, "audio/aiff", ContentServe::InlineMedia("audio/aiff"), Some("audio/aiff"), None, ViewerMode::Inline),
    entry("aifc", Category::Audio, "audio/aiff", ContentServe::InlineMedia("audio/aiff"), Some("audio/aiff"), None, ViewerMode::Inline),
    entry("mka", Category::Audio, "audio/x-matroska", ContentServe::InlineMedia("audio/x-matroska"), Some("audio/x-matroska"), None, ViewerMode::Inline),
    entry("amr", Category::Audio, "audio/amr", ContentServe::InlineMedia("audio/amr"), Some("audio/amr"), None, ViewerMode::Inline),
    // ── Audio (known not to decode in WebView -> skip player, open in another app) ───────────────
    entry("wma", Category::Audio, "audio/x-ms-wma", ContentServe::Attachment, Some("audio/x-ms-wma"), None, ViewerMode::Fallback),
    entry("mid", Category::Audio, "audio/midi", ContentServe::Attachment, Some("audio/midi"), None, ViewerMode::Fallback),
    entry("midi", Category::Audio, "audio/midi", ContentServe::Attachment, Some("audio/midi"), None, ViewerMode::Fallback),
    entry("ape", Category::Audio, "audio/x-ape", ContentServe::Attachment, Some("audio/x-ape"), None, ViewerMode::Fallback),
    entry("wv", Category::Audio, "audio/x-wavpack", ContentServe::Attachment, Some("audio/x-wavpack"), None, ViewerMode::Fallback),
    entry("caf", Category::Audio, "audio/x-caf", ContentServe::Attachment, Some("audio/x-caf"), None, ViewerMode::Fallback),

    // ── Documents ────────────────────────────────────────────────────────────────────────────────
    entry("pdf", Category::Document, "application/pdf", ContentServe::Attachment, Some("application/pdf"), None, ViewerMode::Fallback),
    entry("epub", Category::Document, "application/epub+zip", ContentServe::Attachment, Some("application/epub+zip"), None, ViewerMode::Fallback),
    entry("docx", Category::Document, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", ContentServe::Attachment, Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"), None, ViewerMode::Fallback),
    entry("xlsx", Category::Document, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", ContentServe::Attachment, Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"), None, ViewerMode::Fallback),
    entry("pptx", Category::Document, "application/vnd.openxmlformats-officedocument.presentationml.presentation", ContentServe::Attachment, Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"), None, ViewerMode::Fallback),
    entry("odt", Category::Document, "application/vnd.oasis.opendocument.text", ContentServe::Attachment, Some("application/vnd.oasis.opendocument.text"), None, ViewerMode::Fallback),
    entry("ods", Category::Document, "application/vnd.oasis.opendocument.spreadsheet", ContentServe::Attachment, Some("application/vnd.oasis.opendocument.spreadsheet"), None, ViewerMode::Fallback),
    entry("odp", Category::Document, "application/vnd.oasis.opendocument.presentation", ContentServe::Attachment, Some("application/vnd.oasis.opendocument.presentation"), None, ViewerMode::Fallback),
    // Legacy or macro-capable document formats: never offered for direct external open.
    entry("rtf", Category::Document, "application/rtf", ContentServe::Attachment, None, Some("application/rtf"), ViewerMode::Fallback),
    entry("doc", Category::Document, "application/msword", ContentServe::Attachment, None, Some("application/msword"), ViewerMode::Fallback),
    entry("xls", Category::Document, "application/vnd.ms-excel", ContentServe::Attachment, None, Some("application/vnd.ms-excel"), ViewerMode::Fallback),
    entry("ppt", Category::Document, "application/vnd.ms-powerpoint", ContentServe::Attachment, None, Some("application/vnd.ms-powerpoint"), ViewerMode::Fallback),
    entry("mobi", Category::Document, "application/x-mobipocket-ebook", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("azw", Category::Document, "application/vnd.amazon.ebook", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("azw3", Category::Document, "application/vnd.amazon.ebook", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("djvu", Category::Document, "image/vnd.djvu", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("djv", Category::Document, "image/vnd.djvu", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("odg", Category::Document, "application/vnd.oasis.opendocument.graphics", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("pages", Category::Document, "application/vnd.apple.pages", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("numbers", Category::Document, "application/vnd.apple.numbers", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("key", Category::Document, "application/vnd.apple.keynote", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("ps", Category::Document, "application/postscript", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("eps", Category::Document, "application/postscript", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("xps", Category::Document, "application/vnd.ms-xpsdocument", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("cbz", Category::Document, "application/vnd.comicbook+zip", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("cbr", Category::Document, "application/vnd.comicbook-rar", ContentServe::Attachment, None, None, ViewerMode::Fallback),

    // ── Plain text, config, markup, and source code (never rendered as active HTML/XML) ──────────
    // Safe plain-text family allowed for direct external open
    entry("txt", Category::Text, "text/plain", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("md", Category::Text, "text/markdown", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("log", Category::Text, "text/plain", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("toml", Category::Text, "application/toml", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("yaml", Category::Text, "application/yaml", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("yml", Category::Text, "application/yaml", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("ini", Category::Text, "text/plain", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("conf", Category::Text, "text/plain", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("vtt", Category::Text, "text/vtt", ContentServe::InlineVtt, Some("text/plain"), None, ViewerMode::Text),
    entry("csv", Category::Text, "text/csv", ContentServe::InlineText, Some("text/plain"), None, ViewerMode::Text),
    entry("json", Category::Text, "application/json", ContentServe::InlineText, Some("application/json"), None, ViewerMode::Text),
    // Markup, web documents, structured text, config, and code (viewed as plain text, never opened externally)
    entry("text", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("srt", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("markdown", Category::Text, "text/markdown", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("mdx", Category::Text, "text/markdown", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("rst", Category::Text, "text/x-rst", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("adoc", Category::Text, "text/asciidoc", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("asciidoc", Category::Text, "text/asciidoc", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("org", Category::Text, "text/org", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("tex", Category::Text, "text/x-tex", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("latex", Category::Text, "text/x-tex", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("tsv", Category::Text, "text/tab-separated-values", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("ics", Category::Text, "text/calendar", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("vcf", Category::Text, "text/vcard", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("diff", Category::Text, "text/x-diff", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("patch", Category::Text, "text/x-diff", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("jsonc", Category::Text, "application/json", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("json5", Category::Text, "application/json", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("jsonl", Category::Text, "application/json", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("ndjson", Category::Text, "application/json", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("xml", Category::Text, "application/xml", ContentServe::InlineText, None, Some("application/xml"), ViewerMode::Text),
    entry("xsl", Category::Text, "application/xml", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("xsd", Category::Text, "application/xml", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("plist", Category::Text, "application/xml", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("html", Category::Text, "text/html", ContentServe::InlineText, None, Some("text/html"), ViewerMode::Text),
    entry("htm", Category::Text, "text/html", ContentServe::InlineText, None, Some("text/html"), ViewerMode::Text),
    entry("xhtml", Category::Text, "text/html", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("css", Category::Text, "text/css", ContentServe::InlineText, None, Some("text/css"), ViewerMode::Text),
    entry("scss", Category::Text, "text/css", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("sass", Category::Text, "text/css", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("less", Category::Text, "text/css", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cfg", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("env", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("lock", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("properties", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("editorconfig", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cmake", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("mk", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("make", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("dockerfile", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("tf", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("tfvars", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hcl", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("vue", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("svelte", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("astro", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("qml", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("glsl", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("wgsl", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hlsl", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("vert", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("frag", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("sv", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("vhd", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("vhdl", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("sol", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("prisma", Category::Text, "text/plain", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("graphql", Category::Text, "application/graphql", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("gql", Category::Text, "application/graphql", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("sql", Category::Text, "application/sql", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("rs", Category::Text, "text/x-rust", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("c", Category::Text, "text/x-c", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("h", Category::Text, "text/x-c", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cc", Category::Text, "text/x-c++", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cpp", Category::Text, "text/x-c++", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cxx", Category::Text, "text/x-c++", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hh", Category::Text, "text/x-c++", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hpp", Category::Text, "text/x-c++", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hxx", Category::Text, "text/x-c++", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cs", Category::Text, "text/x-csharp", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("go", Category::Text, "text/x-go", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("java", Category::Text, "text/x-java-source", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("kt", Category::Text, "text/x-kotlin", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("kts", Category::Text, "text/x-kotlin", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("scala", Category::Text, "text/x-scala", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("sc", Category::Text, "text/x-scala", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("swift", Category::Text, "text/x-swift", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("m", Category::Text, "text/x-objective-c", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("mm", Category::Text, "text/x-objective-c", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("py", Category::Text, "text/x-python", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("pyi", Category::Text, "text/x-python", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("pyw", Category::Text, "text/x-python", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("rb", Category::Text, "text/x-ruby", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("rake", Category::Text, "text/x-ruby", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("gemspec", Category::Text, "text/x-ruby", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("php", Category::Text, "text/x-php", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("pl", Category::Text, "text/x-perl", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("pm", Category::Text, "text/x-perl", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("lua", Category::Text, "text/x-lua", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("js", Category::Text, "text/javascript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("mjs", Category::Text, "text/javascript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cjs", Category::Text, "text/javascript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("jsx", Category::Text, "text/javascript", ContentServe::InlineText, None, None, ViewerMode::Text),
    // `.ts` stays TypeScript (see note under `mts`/`m2ts` above).
    entry("ts", Category::Text, "text/typescript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cts", Category::Text, "text/typescript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("tsx", Category::Text, "text/typescript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("sh", Category::Text, "text/x-shellscript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("bash", Category::Text, "text/x-shellscript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("zsh", Category::Text, "text/x-shellscript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("fish", Category::Text, "text/x-shellscript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("ksh", Category::Text, "text/x-shellscript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("csh", Category::Text, "text/x-shellscript", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("ps1", Category::Text, "text/x-powershell", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("psm1", Category::Text, "text/x-powershell", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("psd1", Category::Text, "text/x-powershell", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("bat", Category::Text, "text/x-bat", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cmd", Category::Text, "text/x-bat", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("zig", Category::Text, "text/x-zig", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("nim", Category::Text, "text/x-nim", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("ex", Category::Text, "text/x-elixir", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("exs", Category::Text, "text/x-elixir", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("erl", Category::Text, "text/x-erlang", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hrl", Category::Text, "text/x-erlang", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("hs", Category::Text, "text/x-haskell", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("lhs", Category::Text, "text/x-haskell", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("ml", Category::Text, "text/x-ocaml", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("mli", Category::Text, "text/x-ocaml", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("clj", Category::Text, "text/x-clojure", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cljs", Category::Text, "text/x-clojure", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("cljc", Category::Text, "text/x-clojure", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("edn", Category::Text, "text/x-clojure", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("el", Category::Text, "text/x-lisp", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("lisp", Category::Text, "text/x-lisp", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("scm", Category::Text, "text/x-lisp", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("rkt", Category::Text, "text/x-lisp", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("dart", Category::Text, "text/x-dart", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("r", Category::Text, "text/x-r", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("jl", Category::Text, "text/x-julia", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("groovy", Category::Text, "text/x-groovy", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("gradle", Category::Text, "text/x-groovy", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("nix", Category::Text, "text/x-nix", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("proto", Category::Text, "text/x-protobuf", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("asm", Category::Text, "text/x-asm", ContentServe::InlineText, None, None, ViewerMode::Text),
    entry("s", Category::Text, "text/x-asm", ContentServe::InlineText, None, None, ViewerMode::Text),

    // ── Archives & packages (never opened externally) ────────────────────────────────────────────
    entry("zip", Category::Archive, "application/zip", ContentServe::Attachment, None, Some("application/zip"), ViewerMode::Fallback),
    entry("whl", Category::Archive, "application/zip", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("tar", Category::Archive, "application/x-tar", ContentServe::Attachment, None, Some("application/x-tar"), ViewerMode::Fallback),
    entry("gz", Category::Archive, "application/gzip", ContentServe::Attachment, None, Some("application/gzip"), ViewerMode::Fallback),
    entry("gzip", Category::Archive, "application/gzip", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("tgz", Category::Archive, "application/gzip", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("bz2", Category::Archive, "application/x-bzip2", ContentServe::Attachment, None, Some("application/x-bzip2"), ViewerMode::Fallback),
    entry("bzip2", Category::Archive, "application/x-bzip2", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("tbz2", Category::Archive, "application/x-bzip2", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("xz", Category::Archive, "application/x-xz", ContentServe::Attachment, None, Some("application/x-xz"), ViewerMode::Fallback),
    entry("txz", Category::Archive, "application/x-xz", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("zst", Category::Archive, "application/zstd", ContentServe::Attachment, None, Some("application/zstd"), ViewerMode::Fallback),
    entry("zstd", Category::Archive, "application/zstd", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("tzst", Category::Archive, "application/zstd", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("lz4", Category::Archive, "application/x-lz4", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("lz", Category::Archive, "application/x-lzip", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("lzma", Category::Archive, "application/x-lzma", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("7z", Category::Archive, "application/x-7z-compressed", ContentServe::Attachment, None, Some("application/x-7z-compressed"), ViewerMode::Fallback),
    entry("rar", Category::Archive, "application/vnd.rar", ContentServe::Attachment, None, Some("application/vnd.rar"), ViewerMode::Fallback),
    entry("cab", Category::Archive, "application/vnd.ms-cab-compressed", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("iso", Category::Archive, "application/x-iso9660-image", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("dmg", Category::Archive, "application/x-apple-diskimage", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("deb", Category::Archive, "application/vnd.debian.binary-package", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("rpm", Category::Archive, "application/x-rpm", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("apk", Category::Archive, "application/vnd.android.package-archive", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("jar", Category::Archive, "application/java-archive", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("war", Category::Archive, "application/java-archive", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("ear", Category::Archive, "application/java-archive", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("cpio", Category::Archive, "application/x-cpio", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("ar", Category::Archive, "application/x-archive", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("br", Category::Archive, "application/x-brotli", ContentServe::Attachment, None, None, ViewerMode::Fallback),
    entry("sz", Category::Archive, "application/x-snappy-framed", ContentServe::Attachment, None, None, ViewerMode::Fallback),
];
// END_MEDIA_TABLE

/// Looks up the full [`MediaEntry`] for a file extension (with or without leading `.`),
/// case-insensitively.
pub fn lookup_extension(ext: &str) -> Option<&'static MediaEntry> {
    let trimmed = ext.strip_prefix('.').unwrap_or(ext);
    if trimmed.is_empty() || trimmed.len() > 16 || !trimmed.is_ascii() {
        return None;
    }
    let mut lower = [0u8; 16];
    for (dst, &src) in lower.iter_mut().zip(trimmed.as_bytes()) {
        *dst = src.to_ascii_lowercase();
    }
    let key = std::str::from_utf8(&lower[..trimmed.len()]).ok()?;
    MEDIA_TABLE.iter().find(|item| item.ext == key)
}

/// Classifies a file extension (with or without a leading `.`), case-insensitively.
///
/// Extensions not present in [`MEDIA_TABLE`] map to [`Category::Other`] and
/// `"application/octet-stream"`.
pub fn classify_extension(ext: &str) -> MediaInfo {
    match lookup_extension(ext) {
        Some(item) => MediaInfo {
            category: item.category,
            mime: item.mime,
        },
        None => OTHER,
    }
}

/// Classifies a path strictly by its file extension without touching the file system.
pub fn classify_path(path: &Path) -> MediaInfo {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => classify_extension(ext),
        None => OTHER,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::collections::{BTreeMap, HashSet};

    use super::*;

    #[test]
    fn media_table_invariants_hold() {
        let mut seen = HashSet::new();
        for item in MEDIA_TABLE {
            assert!(
                !item.ext.is_empty()
                    && item.ext.len() <= 16
                    && item
                        .ext
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "extension {:?} must be 1..=16 lowercase ASCII alphanumeric chars",
                item.ext
            );
            assert!(
                seen.insert(item.ext),
                "duplicate extension {:?} in MEDIA_TABLE",
                item.ext
            );
            assert!(
                !(item.open_externally_mime.is_some() && item.download_mime.is_some()),
                "extension {:?} cannot set both open_externally_mime and download_mime",
                item.ext
            );
            // Active web documents, SVG, archives, scripts, and executables must never be openable externally.
            if matches!(
                item.ext,
                "svg"
                    | "html"
                    | "htm"
                    | "xhtml"
                    | "xml"
                    | "css"
                    | "js"
                    | "mjs"
                    | "cjs"
                    | "py"
                    | "sh"
                    | "ps1"
                    | "bat"
                    | "cmd"
                    | "apk"
                    | "dmg"
                    | "iso"
                    | "deb"
                    | "rpm"
                    | "jar"
                    | "zip"
                    | "tar"
                    | "gz"
                    | "7z"
                    | "rar"
                    | "doc"
                    | "xls"
                    | "ppt"
                    | "rtf"
            ) {
                assert!(
                    item.open_externally_mime.is_none(),
                    "security-sensitive extension {:?} must never have open_externally_mime",
                    item.ext
                );
            }
            // HTML, XML, and JS must always be served as plain text over legion-content, never active.
            if matches!(
                item.ext,
                "html" | "htm" | "xhtml" | "xml" | "js" | "mjs" | "cjs" | "css"
            ) {
                assert_eq!(
                    item.content_serve,
                    ContentServe::InlineText,
                    "active web/script type {:?} must be served as InlineText",
                    item.ext
                );
            }
        }
    }

    #[test]
    fn classifies_every_documented_category() {
        let cases = [
            // Image
            ("photo.jpg", Category::Image, "image/jpeg"),
            ("photo.JPEG", Category::Image, "image/jpeg"),
            ("icon.png", Category::Image, "image/png"),
            ("anim.gif", Category::Image, "image/gif"),
            ("pic.webp", Category::Image, "image/webp"),
            ("pic.avif", Category::Image, "image/avif"),
            ("IMG_0001.HEIC", Category::Image, "image/heic"),
            ("scan.bmp", Category::Image, "image/bmp"),
            ("logo.svg", Category::Image, "image/svg+xml"),
            ("scan.tif", Category::Image, "image/tiff"),
            ("raw.dng", Category::Image, "image/x-adobe-dng"),
            ("raw.cr3", Category::Image, "image/x-canon-cr3"),
            ("raw.nef", Category::Image, "image/x-nikon-nef"),
            ("raw.arw", Category::Image, "image/x-sony-arw"),
            // Video
            ("clip.mp4", Category::Video, "video/mp4"),
            ("movie.m4v", Category::Video, "video/x-m4v"),
            ("camera.MOV", Category::Video, "video/quicktime"),
            ("web.webm", Category::Video, "video/webm"),
            ("film.mkv", Category::Video, "video/x-matroska"),
            ("old.avi", Category::Video, "video/x-msvideo"),
            ("old.wmv", Category::Video, "video/x-ms-wmv"),
            ("old.flv", Category::Video, "video/x-flv"),
            ("phone.3gp", Category::Video, "video/3gpp"),
            ("cam.mts", Category::Video, "video/mp2t"),
            ("bluray.M2TS", Category::Video, "video/mp2t"),
            // Audio
            ("song.mp3", Category::Audio, "audio/mpeg"),
            ("track.m4a", Category::Audio, "audio/mp4"),
            ("book.m4b", Category::Audio, "audio/mp4"),
            ("stream.aac", Category::Audio, "audio/aac"),
            ("album.FLAC", Category::Audio, "audio/flac"),
            ("voice.ogg", Category::Audio, "audio/ogg"),
            ("voice.oga", Category::Audio, "audio/ogg"),
            ("call.opus", Category::Audio, "audio/opus"),
            ("sample.wav", Category::Audio, "audio/wav"),
            ("clip.weba", Category::Audio, "audio/webm"),
            ("studio.aif", Category::Audio, "audio/aiff"),
            ("studio.aiff", Category::Audio, "audio/aiff"),
            ("legacy.wma", Category::Audio, "audio/x-ms-wma"),
            ("note.amr", Category::Audio, "audio/amr"),
            ("synth.mid", Category::Audio, "audio/midi"),
            // Document
            ("report.pdf", Category::Document, "application/pdf"),
            ("book.epub", Category::Document, "application/epub+zip"),
            (
                "memo.docx",
                Category::Document,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "sheet.xlsx",
                Category::Document,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "deck.pptx",
                Category::Document,
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            (
                "letter.odt",
                Category::Document,
                "application/vnd.oasis.opendocument.text",
            ),
            ("notes.rtf", Category::Document, "application/rtf"),
            // Text & code
            ("notes.txt", Category::Text, "text/plain"),
            ("README.md", Category::Text, "text/markdown"),
            ("server.log", Category::Text, "text/plain"),
            ("data.csv", Category::Text, "text/csv"),
            ("pkg.json", Category::Text, "application/json"),
            ("Cargo.toml", Category::Text, "application/toml"),
            ("config.yaml", Category::Text, "application/yaml"),
            ("feed.xml", Category::Text, "application/xml"),
            ("index.html", Category::Text, "text/html"),
            ("style.css", Category::Text, "text/css"),
            ("main.rs", Category::Text, "text/x-rust"),
            ("app.ts", Category::Text, "text/typescript"),
            ("script.py", Category::Text, "text/x-python"),
            ("build.sh", Category::Text, "text/x-shellscript"),
            // Archive
            ("bundle.zip", Category::Archive, "application/zip"),
            ("backup.tar", Category::Archive, "application/x-tar"),
            ("backup.tar.gz", Category::Archive, "application/gzip"),
            ("backup.tar.xz", Category::Archive, "application/x-xz"),
            ("backup.tar.zst", Category::Archive, "application/zstd"),
            (
                "packed.7z",
                Category::Archive,
                "application/x-7z-compressed",
            ),
            ("packed.rar", Category::Archive, "application/vnd.rar"),
            // Other
            ("README", Category::Other, "application/octet-stream"),
            (".gitignore", Category::Other, "application/octet-stream"),
            ("blob.bin", Category::Other, "application/octet-stream"),
            ("vector.svgz", Category::Other, "application/octet-stream"),
            (
                "unknown.xyz123",
                Category::Other,
                "application/octet-stream",
            ),
        ];

        for (file, want_category, want_mime) in cases {
            let info = classify_path(Path::new(file));
            assert_eq!(
                info,
                MediaInfo {
                    category: want_category,
                    mime: want_mime,
                },
                "misclassified {file}"
            );
        }
    }

    #[test]
    fn classify_extension_handles_leading_dot_case_unicode_and_long_inputs() {
        assert_eq!(classify_extension(".PNG"), classify_extension("png"));
        assert_eq!(classify_extension("JpEg"), classify_extension("jpg"));
        assert_eq!(classify_extension(""), OTHER);
        assert_eq!(classify_extension("."), OTHER);
        assert_eq!(classify_extension("🦀"), OTHER);
        assert_eq!(classify_extension("averylongunknownextensionname"), OTHER);
        assert_eq!(
            classify_path(Path::new("/home/me/Übersicht 2026.PDF")),
            MediaInfo {
                category: Category::Document,
                mime: "application/pdf",
            }
        );
    }

    #[test]
    fn category_serializes_as_snake_case() {
        let all = [
            (Category::Image, "image"),
            (Category::Video, "video"),
            (Category::Audio, "audio"),
            (Category::Document, "document"),
            (Category::Text, "text"),
            (Category::Archive, "archive"),
            (Category::Other, "other"),
        ];
        for (cat, expected) in all {
            assert_eq!(cat.as_str(), expected);
            assert_eq!(cat.to_string(), expected);
            assert_eq!(
                serde_json::to_value(cat).unwrap(),
                serde_json::Value::String(expected.to_owned())
            );
            let parsed: Category = serde_json::from_str(&format!("\"{expected}\"")).unwrap();
            assert_eq!(parsed, cat);
        }
    }

    /// Verifies that every downstream mirror table across the workspace matches [`MEDIA_TABLE`]
    /// without drift, regardless of LF or CRLF line endings. Run `node scripts/sync-media-table.mjs`
    /// after editing `MEDIA_TABLE`.
    #[test]
    fn downstream_media_mirrors_match_canonical_table() {
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("legion-core lives under <repo>/crates/legion-core");

        let normalize_newlines = |src: &str| -> String { src.replace("\r\n", "\n") };
        let to_crlf = |src: &str| -> String { src.replace("\r\n", "\n").replace('\n', "\r\n") };

        let assert_mirrors_match = |content_raw: &str,
                                    policy_raw: &str,
                                    kt_raw: &str,
                                    js_raw: &str| {
            let content_src = normalize_newlines(content_raw);
            let policy_src = normalize_newlines(policy_raw);
            let kt_src = normalize_newlines(kt_raw);
            let js_src = normalize_newlines(js_raw);

            let extract = |src: &str, begin: &str, end: &str, label: &str| -> String {
                let start = src
                    .find(begin)
                    .unwrap_or_else(|| panic!("missing {begin} marker in {label}"));
                let rest = &src[start + begin.len()..];
                let stop = rest
                    .find(end)
                    .unwrap_or_else(|| panic!("missing {end} marker in {label}"));
                rest[..stop].to_owned()
            };

            // 1. Check `crates/legion-content/src/media.rs`
            let content_block = extract(
                &content_src,
                "// BEGIN_CONTENT_MEDIA_TABLE",
                "// END_CONTENT_MEDIA_TABLE",
                "crates/legion-content/src/media.rs",
            );
            let mut content_map: BTreeMap<String, String> = BTreeMap::new();
            for line in content_block.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("(\"")
                    && let Some((ext, tail)) = rest.split_once("\", \"")
                    && let Some((ctype, _)) = tail.split_once("\")")
                {
                    assert!(
                        content_map
                            .insert(ext.to_owned(), ctype.to_owned())
                            .is_none(),
                        "duplicate ext {ext:?} in legion-content/src/media.rs"
                    );
                }
            }
            let mut expected_content: BTreeMap<String, String> = BTreeMap::new();
            for item in MEDIA_TABLE {
                match item.content_serve {
                    ContentServe::InlineMedia(ct) => {
                        expected_content.insert(item.ext.to_owned(), ct.to_owned());
                    }
                    ContentServe::InlineText => {
                        expected_content
                            .insert(item.ext.to_owned(), "text/plain; charset=utf-8".to_owned());
                    }
                    ContentServe::InlineVtt => {
                        expected_content
                            .insert(item.ext.to_owned(), "text/vtt; charset=utf-8".to_owned());
                    }
                    ContentServe::Attachment => {}
                }
            }
            assert_eq!(
                content_map, expected_content,
                "crates/legion-content/src/media.rs drifted from crates/legion-core/src/media.rs (run `node scripts/sync-media-table.mjs`)"
            );

            // 2. Check `apps/legion-app/src-tauri/src/transfers/file_policy.rs`
            let parse_rust_pairs = |block: &str, label: &str| -> BTreeMap<String, String> {
                let mut map = BTreeMap::new();
                let mut joined = String::new();
                for line in block.lines() {
                    let t = line.trim();
                    if t.starts_with("//") {
                        continue;
                    }
                    joined.push_str(t);
                    joined.push(' ');
                }
                let mut rest = joined.as_str();
                while let Some(idx) = rest.find('(') {
                    rest = rest[idx + 1..].trim_start();
                    let Some(after_open) = rest.strip_prefix('"') else {
                        continue;
                    };
                    let Some((ext, after_ext)) = after_open.split_once('"') else {
                        break;
                    };
                    let Some((_, after_comma)) = after_ext.split_once('"') else {
                        break;
                    };
                    let Some((mime, after_mime)) = after_comma.split_once('"') else {
                        break;
                    };
                    assert!(
                        map.insert(ext.to_owned(), mime.to_owned()).is_none(),
                        "duplicate ext {ext:?} in {label}"
                    );
                    rest = after_mime;
                }
                map
            };
            let policy_allowed = parse_rust_pairs(
                &extract(
                    &policy_src,
                    "// BEGIN_ALLOWED_OPEN_MIME_BY_EXT",
                    "// END_ALLOWED_OPEN_MIME_BY_EXT",
                    "file_policy.rs",
                ),
                "file_policy.rs::ALLOWED_OPEN_MIME_BY_EXT",
            );
            let policy_non_open = parse_rust_pairs(
                &extract(
                    &policy_src,
                    "// BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT",
                    "// END_NON_OPEN_DOWNLOAD_MIME_BY_EXT",
                    "file_policy.rs",
                ),
                "file_policy.rs::NON_OPEN_DOWNLOAD_MIME_BY_EXT",
            );
            let expected_allowed: BTreeMap<String, String> = MEDIA_TABLE
                .iter()
                .filter_map(|i| {
                    i.open_externally_mime
                        .map(|m| (i.ext.to_owned(), m.to_owned()))
                })
                .collect();
            let expected_non_open: BTreeMap<String, String> = MEDIA_TABLE
                .iter()
                .filter_map(|i| i.download_mime.map(|m| (i.ext.to_owned(), m.to_owned())))
                .collect();
            assert_eq!(
                policy_allowed, expected_allowed,
                "file_policy.rs::ALLOWED_OPEN_MIME_BY_EXT drifted from crates/legion-core/src/media.rs (run `node scripts/sync-media-table.mjs`)"
            );
            assert_eq!(
                policy_non_open, expected_non_open,
                "file_policy.rs::NON_OPEN_DOWNLOAD_MIME_BY_EXT drifted from crates/legion-core/src/media.rs (run `node scripts/sync-media-table.mjs`)"
            );

            // 3. Check `TransferPathPolicy.kt`
            let parse_kt_map = |block: &str| -> BTreeMap<String, String> {
                let mut map = BTreeMap::new();
                for line in block.lines() {
                    let t = line.trim();
                    if t.is_empty() || t.starts_with("//") {
                        continue;
                    }
                    if let Some((lhs, rhs)) = t.split_once(" to ") {
                        let k = lhs.trim().trim_matches('"');
                        let v = rhs.trim().trim_end_matches(',').trim().trim_matches('"');
                        if !k.is_empty() && !v.is_empty() {
                            assert!(
                                map.insert(k.to_owned(), v.to_owned()).is_none(),
                                "duplicate Kotlin key {k:?}"
                            );
                        }
                    }
                }
                map
            };
            let kt_allowed = parse_kt_map(&extract(
                &kt_src,
                "// BEGIN_ALLOWED_OPEN_MIME_BY_EXT",
                "// END_ALLOWED_OPEN_MIME_BY_EXT",
                "TransferPathPolicy.kt",
            ));
            let kt_non_open = parse_kt_map(&extract(
                &kt_src,
                "// BEGIN_NON_OPEN_DOWNLOAD_MIME_BY_EXT",
                "// END_NON_OPEN_DOWNLOAD_MIME_BY_EXT",
                "TransferPathPolicy.kt",
            ));
            assert_eq!(
                kt_allowed, expected_allowed,
                "TransferPathPolicy.kt::ALLOWED_OPEN_MIME_BY_EXT drifted from crates/legion-core/src/media.rs"
            );
            assert_eq!(
                kt_non_open, expected_non_open,
                "TransferPathPolicy.kt::NON_OPEN_DOWNLOAD_MIME_BY_EXT drifted from crates/legion-core/src/media.rs"
            );

            // 4. Check `apps/ui/lib/files.js`
            let js_block = extract(
                &js_src,
                "// BEGIN_MEDIA_TABLE",
                "// END_MEDIA_TABLE",
                "apps/ui/lib/files.js",
            );
            // Each generated row in files.js has the form:
            //   ext: ['category', 'mime', 'viewer_mode', openBool],
            let mut js_rows: BTreeMap<String, (String, String, String, bool)> = BTreeMap::new();
            for line in js_block.lines() {
                let t = line.trim();
                if t.is_empty() || t.starts_with("//") || !t.contains(": [") {
                    continue;
                }
                let Some((raw_key, rest)) = t.split_once(": [") else {
                    continue;
                };
                let ext = raw_key.trim().trim_matches('\'');
                let inner = rest.trim_end_matches("],").trim_end_matches(']');
                let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
                assert_eq!(parts.len(), 4, "malformed files.js media row: {t}");
                let cat = parts[0].trim_matches('\'').to_owned();
                let mime = parts[1].trim_matches('\'').to_owned();
                let mode = parts[2].trim_matches('\'').to_owned();
                let open = match parts[3] {
                    "true" => true,
                    "false" => false,
                    other => panic!("unexpected open flag {other:?} in files.js row: {t}"),
                };
                assert!(
                    js_rows
                        .insert(ext.to_owned(), (cat, mime, mode, open))
                        .is_none(),
                    "duplicate ext {ext:?} in apps/ui/lib/files.js"
                );
            }
            let expected_js: BTreeMap<String, (String, String, String, bool)> = MEDIA_TABLE
                .iter()
                .map(|i| {
                    (
                        i.ext.to_owned(),
                        (
                            i.category.as_str().to_owned(),
                            i.mime.to_owned(),
                            i.viewer_mode.as_str().to_owned(),
                            i.open_externally_mime.is_some(),
                        ),
                    )
                })
                .collect();
            assert_eq!(
                js_rows, expected_js,
                "apps/ui/lib/files.js drifted from crates/legion-core/src/media.rs (run `node scripts/sync-media-table.mjs`)"
            );
        };

        let content_src =
            std::fs::read_to_string(repo_root.join("crates/legion-content/src/media.rs"))
                .expect("legion-content media.rs must exist");
        let policy_src = std::fs::read_to_string(
            repo_root.join("apps/legion-app/src-tauri/src/transfers/file_policy.rs"),
        )
        .expect("file_policy.rs must exist");
        let kt_src = std::fs::read_to_string(repo_root.join(
            "apps/legion-app/src-tauri/gen/android/app/src/main/java/com/x1f4r/legion/TransferPathPolicy.kt",
        ))
        .expect("TransferPathPolicy.kt must exist");
        let js_src = std::fs::read_to_string(repo_root.join("apps/ui/lib/files.js"))
            .expect("apps/ui/lib/files.js must exist");

        // Verify the checked-out files directly, and verify CRLF-converted copies explicitly so
        // Windows autocrlf checkouts are always covered on every host OS.
        assert_mirrors_match(&content_src, &policy_src, &kt_src, &js_src);
        assert_mirrors_match(
            &to_crlf(&content_src),
            &to_crlf(&policy_src),
            &to_crlf(&kt_src),
            &to_crlf(&js_src),
        );
    }
}
