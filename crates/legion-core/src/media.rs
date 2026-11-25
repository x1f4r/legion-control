//! File categories and media types, classified strictly by file extension.
//!
//! Nothing on disk is opened to guess a file's type: [`classify_extension`] and [`classify_path`]
//! look up the extension case-insensitively in one table and fall back to [`Category::Other`] with
//! `application/octet-stream`.
//!
//! # Security: never serve inline `Content-Type` directly from this table
//!
//! This table is for display, filtering, and client hints — **not** an HTTP inline-serving
//! allow-list. Several media types returned here execute script, load external resources, or
//! interpret active content if served inline in a browser, and **must never** be served inline
//! from this table without a dedicated content server allow-list and sandboxing headers:
//!
//! - `image/svg+xml` (`.svg`)
//! - `text/html` (`.html`, `.htm`, `.xhtml`)
//! - `application/xml` (`.xml`, `.xsl`, `.xsd`, `.plist`)
//! - `text/javascript` (`.js`, `.mjs`, `.cjs`, `.jsx`)
//! - `text/css` (`.css`, `.scss`, `.sass`, `.less`)
//! - `application/pdf` (`.pdf`)
//!
//! Any content-serving endpoint must apply its own strict allow-list and security headers
//! (`X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox; default-src 'none'`,
//! `Content-Disposition: attachment` for non-allow-listed types) rather than echoing `mime`.

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
    /// Raster and vector pictures.
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

const OTHER: MediaInfo = MediaInfo {
    category: Category::Other,
    mime: "application/octet-stream",
};

/// Classifies a file extension (with or without a leading `.`), case-insensitively.
///
/// Extensions not present in the table map to [`Category::Other`] and
/// `"application/octet-stream"`.
pub fn classify_extension(ext: &str) -> MediaInfo {
    let trimmed = ext.strip_prefix('.').unwrap_or(ext);
    if trimmed.is_empty() || trimmed.len() > 16 || !trimmed.is_ascii() {
        return OTHER;
    }
    let mut lower = [0u8; 16];
    for (dst, &src) in lower.iter_mut().zip(trimmed.as_bytes()) {
        *dst = src.to_ascii_lowercase();
    }
    let key = std::str::from_utf8(&lower[..trimmed.len()]).unwrap_or_default();

    let (category, mime) = match key {
        // Image
        "jpg" | "jpeg" | "jpe" | "jfif" => (Category::Image, "image/jpeg"),
        "png" => (Category::Image, "image/png"),
        "gif" => (Category::Image, "image/gif"),
        "webp" => (Category::Image, "image/webp"),
        "avif" => (Category::Image, "image/avif"),
        "heic" => (Category::Image, "image/heic"),
        "heif" => (Category::Image, "image/heif"),
        "bmp" | "dib" => (Category::Image, "image/bmp"),
        // `.svgz` is gzip-compressed SVG and must not be labelled `image/svg+xml` (which would
        // mislabel compressed bytes as plain XML); it falls through to `Other`.
        "svg" => (Category::Image, "image/svg+xml"),
        "tif" | "tiff" => (Category::Image, "image/tiff"),
        "ico" | "cur" => (Category::Image, "image/x-icon"),
        "jxl" => (Category::Image, "image/jxl"),
        "psd" => (Category::Image, "image/vnd.adobe.photoshop"),
        "dng" => (Category::Image, "image/x-adobe-dng"),
        "cr2" => (Category::Image, "image/x-canon-cr2"),
        "cr3" => (Category::Image, "image/x-canon-cr3"),
        "nef" => (Category::Image, "image/x-nikon-nef"),
        "arw" => (Category::Image, "image/x-sony-arw"),
        "raf" => (Category::Image, "image/x-fuji-raf"),
        "orf" => (Category::Image, "image/x-olympus-orf"),
        "rw2" => (Category::Image, "image/x-panasonic-rw2"),
        "tga" => (Category::Image, "image/x-tga"),
        "ppm" => (Category::Image, "image/x-portable-pixmap"),
        "pgm" => (Category::Image, "image/x-portable-graymap"),
        "pbm" => (Category::Image, "image/x-portable-bitmap"),
        "pnm" => (Category::Image, "image/x-portable-anymap"),
        "qoi" => (Category::Image, "image/qoi"),
        "dds" => (Category::Image, "image/vnd.ms-dds"),
        "exr" => (Category::Image, "image/x-exr"),
        "hdr" => (Category::Image, "image/vnd.radiance"),

        // Video
        "mp4" => (Category::Video, "video/mp4"),
        "m4v" => (Category::Video, "video/x-m4v"),
        "mov" | "qt" => (Category::Video, "video/quicktime"),
        "webm" => (Category::Video, "video/webm"),
        "mkv" => (Category::Video, "video/x-matroska"),
        "avi" => (Category::Video, "video/x-msvideo"),
        "wmv" => (Category::Video, "video/x-ms-wmv"),
        "flv" => (Category::Video, "video/x-flv"),
        "mpg" | "mpeg" | "mpe" => (Category::Video, "video/mpeg"),
        // AVCHD / Blu-ray MPEG transport streams. Plain `.ts` stays mapped to TypeScript below
        // because classifying `.ts` as video would make `fs.thumb` spawn `ffmpeg` on TypeScript
        // source trees when browsing code projects.
        "mts" | "m2ts" => (Category::Video, "video/mp2t"),
        "3gp" => (Category::Video, "video/3gpp"),
        "3g2" => (Category::Video, "video/3gpp2"),
        "ogv" => (Category::Video, "video/ogg"),
        "vob" => (Category::Video, "video/dvd"),
        "asf" => (Category::Video, "video/x-ms-asf"),
        "rm" | "rmvb" => (Category::Video, "video/vnd.rn-realvideo"),

        // Audio
        "mp3" | "mpga" => (Category::Audio, "audio/mpeg"),
        "m4a" | "mp4a" | "alac" => (Category::Audio, "audio/mp4"),
        "aac" => (Category::Audio, "audio/aac"),
        "flac" => (Category::Audio, "audio/flac"),
        "ogg" | "oga" | "spx" => (Category::Audio, "audio/ogg"),
        "opus" => (Category::Audio, "audio/opus"),
        "wav" | "wave" => (Category::Audio, "audio/wav"),
        "weba" => (Category::Audio, "audio/webm"),
        "mka" => (Category::Audio, "audio/x-matroska"),
        "aif" | "aiff" | "aifc" => (Category::Audio, "audio/aiff"),
        "wma" => (Category::Audio, "audio/x-ms-wma"),
        "mid" | "midi" => (Category::Audio, "audio/midi"),
        "amr" => (Category::Audio, "audio/amr"),
        "ape" => (Category::Audio, "audio/x-ape"),
        "wv" => (Category::Audio, "audio/x-wavpack"),
        "caf" => (Category::Audio, "audio/x-caf"),

        // Document
        "pdf" => (Category::Document, "application/pdf"),
        "epub" => (Category::Document, "application/epub+zip"),
        "mobi" => (Category::Document, "application/x-mobipocket-ebook"),
        "azw" | "azw3" => (Category::Document, "application/vnd.amazon.ebook"),
        "djvu" | "djv" => (Category::Document, "image/vnd.djvu"),
        "doc" => (Category::Document, "application/msword"),
        "docx" => (
            Category::Document,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        "xls" => (Category::Document, "application/vnd.ms-excel"),
        "xlsx" => (
            Category::Document,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        "ppt" => (Category::Document, "application/vnd.ms-powerpoint"),
        "pptx" => (
            Category::Document,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        "odt" => (
            Category::Document,
            "application/vnd.oasis.opendocument.text",
        ),
        "ods" => (
            Category::Document,
            "application/vnd.oasis.opendocument.spreadsheet",
        ),
        "odp" => (
            Category::Document,
            "application/vnd.oasis.opendocument.presentation",
        ),
        "odg" => (
            Category::Document,
            "application/vnd.oasis.opendocument.graphics",
        ),
        "rtf" => (Category::Document, "application/rtf"),
        "pages" => (Category::Document, "application/vnd.apple.pages"),
        "numbers" => (Category::Document, "application/vnd.apple.numbers"),
        "ps" | "eps" => (Category::Document, "application/postscript"),
        "xps" => (Category::Document, "application/vnd.ms-xpsdocument"),
        "cbz" => (Category::Document, "application/vnd.comicbook+zip"),
        "cbr" => (Category::Document, "application/vnd.comicbook-rar"),

        // Text, markup, structured data, configuration and source code
        "txt" | "text" | "log" | "srt" => (Category::Text, "text/plain"),
        "md" | "markdown" | "mdx" => (Category::Text, "text/markdown"),
        "rst" => (Category::Text, "text/x-rst"),
        "adoc" | "asciidoc" => (Category::Text, "text/asciidoc"),
        "org" => (Category::Text, "text/org"),
        "tex" | "latex" => (Category::Text, "text/x-tex"),
        "csv" => (Category::Text, "text/csv"),
        "tsv" => (Category::Text, "text/tab-separated-values"),
        "ics" => (Category::Text, "text/calendar"),
        "vcf" => (Category::Text, "text/vcard"),
        "vtt" => (Category::Text, "text/vtt"),
        "diff" | "patch" => (Category::Text, "text/x-diff"),
        "json" | "jsonc" | "json5" | "jsonl" | "ndjson" => (Category::Text, "application/json"),
        "toml" => (Category::Text, "application/toml"),
        "yaml" | "yml" => (Category::Text, "application/yaml"),
        "xml" | "xsl" | "xsd" | "plist" => (Category::Text, "application/xml"),
        "html" | "htm" | "xhtml" => (Category::Text, "text/html"),
        "css" | "scss" | "sass" | "less" => (Category::Text, "text/css"),
        "ini" | "cfg" | "conf" | "env" | "properties" | "editorconfig" | "cmake" | "mk"
        | "make" | "dockerfile" | "tf" | "tfvars" | "hcl" | "vue" | "svelte" | "astro" | "qml"
        | "glsl" | "wgsl" | "hlsl" | "vert" | "frag" | "sv" | "vhd" | "vhdl" | "sol" | "prisma" => {
            (Category::Text, "text/plain")
        }
        "graphql" | "gql" => (Category::Text, "application/graphql"),
        "sql" => (Category::Text, "application/sql"),
        "rs" => (Category::Text, "text/x-rust"),
        "c" | "h" => (Category::Text, "text/x-c"),
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => (Category::Text, "text/x-c++"),
        "cs" => (Category::Text, "text/x-csharp"),
        "go" => (Category::Text, "text/x-go"),
        "java" => (Category::Text, "text/x-java-source"),
        "kt" | "kts" => (Category::Text, "text/x-kotlin"),
        "scala" | "sc" => (Category::Text, "text/x-scala"),
        "swift" => (Category::Text, "text/x-swift"),
        "m" | "mm" => (Category::Text, "text/x-objective-c"),
        "py" | "pyi" | "pyw" => (Category::Text, "text/x-python"),
        "rb" | "rake" | "gemspec" => (Category::Text, "text/x-ruby"),
        "php" => (Category::Text, "text/x-php"),
        "pl" | "pm" => (Category::Text, "text/x-perl"),
        "lua" => (Category::Text, "text/x-lua"),
        "js" | "mjs" | "cjs" | "jsx" => (Category::Text, "text/javascript"),
        // `.ts` stays TypeScript (see note under `mts`/`m2ts` above).
        "ts" | "cts" | "tsx" => (Category::Text, "text/typescript"),
        "sh" | "bash" | "zsh" | "fish" | "ksh" | "csh" => (Category::Text, "text/x-shellscript"),
        "ps1" | "psm1" | "psd1" => (Category::Text, "text/x-powershell"),
        "bat" | "cmd" => (Category::Text, "text/x-bat"),
        "zig" => (Category::Text, "text/x-zig"),
        "nim" => (Category::Text, "text/x-nim"),
        "ex" | "exs" => (Category::Text, "text/x-elixir"),
        "erl" | "hrl" => (Category::Text, "text/x-erlang"),
        "hs" | "lhs" => (Category::Text, "text/x-haskell"),
        "ml" | "mli" => (Category::Text, "text/x-ocaml"),
        "clj" | "cljs" | "cljc" | "edn" => (Category::Text, "text/x-clojure"),
        "el" | "lisp" | "scm" | "rkt" => (Category::Text, "text/x-lisp"),
        "dart" => (Category::Text, "text/x-dart"),
        "r" => (Category::Text, "text/x-r"),
        "jl" => (Category::Text, "text/x-julia"),
        "groovy" | "gradle" => (Category::Text, "text/x-groovy"),
        "nix" => (Category::Text, "text/x-nix"),
        "proto" => (Category::Text, "text/x-protobuf"),
        "asm" | "s" => (Category::Text, "text/x-asm"),

        // Archive
        "zip" | "whl" => (Category::Archive, "application/zip"),
        "tar" => (Category::Archive, "application/x-tar"),
        "gz" | "gzip" | "tgz" => (Category::Archive, "application/gzip"),
        "bz2" | "bzip2" | "tbz2" => (Category::Archive, "application/x-bzip2"),
        "xz" | "txz" => (Category::Archive, "application/x-xz"),
        "zst" | "zstd" | "tzst" => (Category::Archive, "application/zstd"),
        "lz4" => (Category::Archive, "application/x-lz4"),
        "lz" => (Category::Archive, "application/x-lzip"),
        "lzma" => (Category::Archive, "application/x-lzma"),
        "7z" => (Category::Archive, "application/x-7z-compressed"),
        "rar" => (Category::Archive, "application/vnd.rar"),
        "cab" => (Category::Archive, "application/vnd.ms-cab-compressed"),
        "iso" => (Category::Archive, "application/x-iso9660-image"),
        "dmg" => (Category::Archive, "application/x-apple-diskimage"),
        "deb" => (Category::Archive, "application/vnd.debian.binary-package"),
        "rpm" => (Category::Archive, "application/x-rpm"),
        "apk" => (Category::Archive, "application/vnd.android.package-archive"),
        "jar" | "war" | "ear" => (Category::Archive, "application/java-archive"),
        "cpio" => (Category::Archive, "application/x-cpio"),
        "ar" => (Category::Archive, "application/x-archive"),
        "br" => (Category::Archive, "application/x-brotli"),
        "sz" => (Category::Archive, "application/x-snappy-framed"),

        _ => return OTHER,
    };

    MediaInfo { category, mime }
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
    use super::*;

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
            // Video
            ("clip.mp4", Category::Video, "video/mp4"),
            ("movie.m4v", Category::Video, "video/x-m4v"),
            ("camera.MOV", Category::Video, "video/quicktime"),
            ("web.webm", Category::Video, "video/webm"),
            ("film.mkv", Category::Video, "video/x-matroska"),
            ("old.avi", Category::Video, "video/x-msvideo"),
            ("cam.mts", Category::Video, "video/mp2t"),
            ("bluray.M2TS", Category::Video, "video/mp2t"),
            // Audio
            ("song.mp3", Category::Audio, "audio/mpeg"),
            ("track.m4a", Category::Audio, "audio/mp4"),
            ("stream.aac", Category::Audio, "audio/aac"),
            ("album.FLAC", Category::Audio, "audio/flac"),
            ("voice.ogg", Category::Audio, "audio/ogg"),
            ("call.opus", Category::Audio, "audio/opus"),
            ("sample.wav", Category::Audio, "audio/wav"),
            // Document
            ("report.pdf", Category::Document, "application/pdf"),
            ("book.epub", Category::Document, "application/epub+zip"),
            ("memo.doc", Category::Document, "application/msword"),
            (
                "memo.docx",
                Category::Document,
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            ("sheet.xls", Category::Document, "application/vnd.ms-excel"),
            (
                "sheet.xlsx",
                Category::Document,
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "deck.ppt",
                Category::Document,
                "application/vnd.ms-powerpoint",
            ),
            (
                "deck.pptx",
                Category::Document,
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            (
                "doc.odt",
                Category::Document,
                "application/vnd.oasis.opendocument.text",
            ),
            (
                "calc.ods",
                Category::Document,
                "application/vnd.oasis.opendocument.spreadsheet",
            ),
            (
                "slides.odp",
                Category::Document,
                "application/vnd.oasis.opendocument.presentation",
            ),
            // Text
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
}
