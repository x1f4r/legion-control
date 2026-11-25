//! Safe `Content-Type` allow-list, sandbox CSP, and `Content-Disposition` formatting.
//!
//! File types are chosen strictly from the file name's extension (case-insensitive); file bytes
//! are never inspected to guess a media type. Every text file — including HTML, XHTML, XML, and
//! JavaScript — is served as `text/plain; charset=utf-8` (except WebVTT `.vtt`, which is served
//! as `text/vtt; charset=utf-8` for `<track>` subtitles). Anything outside the allow-list —
//! including PDF — is served as `application/octet-stream` with `Content-Disposition: attachment`.

use http::HeaderValue;

/// Sandbox CSP attached unconditionally to every content reply.
///
/// `sandbox` without `allow-scripts` or `allow-same-origin` places any rendered document in a
/// unique opaque origin with script execution disabled; `default-src 'none'` blocks all
/// subresources; `frame-ancestors 'none'` stops any page (even on the same origin or a third-party
/// origin holding a leaked ticket) from framing a content URL.
pub const CONTENT_SANDBOX_CSP: &str = "sandbox; default-src 'none'; frame-ancestors 'none'";

/// How a file name should be presented over HTTP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentPolicy {
    /// Exact `Content-Type` header value to emit.
    pub content_type: &'static str,
    /// Whether `Content-Disposition: attachment` is forced even without `?download=1`.
    pub attachment: bool,
}

/// Maps `file_name` to its [`ContentPolicy`] using only its ASCII extension (case-insensitive).
pub fn classify_filename(file_name: &str) -> ContentPolicy {
    let ext = file_name
        .rsplit_once('.')
        .filter(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
        .map(|(_, ext)| ext.to_ascii_lowercase());

    let Some(ext) = ext else {
        return ContentPolicy {
            content_type: "application/octet-stream",
            attachment: true,
        };
    };

    let inline = |content_type: &'static str| ContentPolicy {
        content_type,
        attachment: false,
    };

    match ext.as_str() {
        // Raster images
        "jpg" | "jpeg" => inline("image/jpeg"),
        "png" => inline("image/png"),
        "gif" => inline("image/gif"),
        "webp" => inline("image/webp"),
        "avif" => inline("image/avif"),
        "bmp" => inline("image/bmp"),
        "ico" => inline("image/x-icon"),
        "tif" | "tiff" => inline("image/tiff"),
        "heic" => inline("image/heic"),
        "heif" => inline("image/heif"),

        // SVG (inline, always carries `CONTENT_SANDBOX_CSP`)
        "svg" => inline("image/svg+xml"),

        // Video
        "mp4" | "m4v" => inline("video/mp4"),
        "webm" => inline("video/webm"),
        "mov" => inline("video/quicktime"),
        "mkv" => inline("video/x-matroska"),
        "avi" => inline("video/x-msvideo"),
        "ogv" => inline("video/ogg"),

        // Audio
        "mp3" => inline("audio/mpeg"),
        "m4a" => inline("audio/mp4"),
        "aac" => inline("audio/aac"),
        "flac" => inline("audio/flac"),
        "ogg" | "oga" => inline("audio/ogg"),
        "opus" => inline("audio/opus"),
        "wav" => inline("audio/wav"),
        "weba" => inline("audio/webm"),

        // WebVTT subtitles for `<track>` elements (browsers require `text/vtt`).
        "vtt" => inline("text/vtt; charset=utf-8"),

        // Text and source files (including HTML/XHTML/XML/JS): always plain text, never active.
        "txt" | "md" | "markdown" | "log" | "csv" | "tsv" | "json" | "jsonl" | "ndjson"
        | "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "env" | "xml" | "html" | "htm"
        | "xhtml" | "css" | "scss" | "less" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx"
        | "rs" | "py" | "rb" | "go" | "java" | "kt" | "kts" | "c" | "h" | "cc" | "cpp" | "cxx"
        | "hpp" | "hxx" | "cs" | "swift" | "m" | "mm" | "php" | "pl" | "sh" | "bash" | "zsh"
        | "fish" | "ps1" | "bat" | "cmd" | "sql" | "graphql" | "gql" | "proto" | "nix" | "lua"
        | "zig" | "ex" | "exs" | "erl" | "hs" | "ml" | "mli" | "r" | "scala" | "dart" | "vue"
        | "svelte" | "astro" | "diff" | "patch" | "rst" | "tex" | "rtf" | "srt" | "ics" => {
            inline("text/plain; charset=utf-8")
        }

        // Everything else (including PDF) forces download as octet-stream.
        _ => ContentPolicy {
            content_type: "application/octet-stream",
            attachment: true,
        },
    }
}

/// Formats `Content-Disposition: attachment; filename="..."; filename*=UTF-8''...` safely.
///
/// Strips control characters and path separators so arbitrary file names can never inject headers
/// or break quoting.
pub fn content_disposition_attachment(file_name: &str) -> HeaderValue {
    let cleaned: String = file_name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .collect();
    let cleaned = if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "download".to_owned()
    } else {
        cleaned
    };

    let mut ascii_fallback = String::with_capacity(cleaned.len());
    let mut needs_rfc5987 = false;
    for ch in cleaned.chars() {
        if ch.is_ascii() && (0x20..=0x7E).contains(&(ch as u8)) {
            if ch == '"' || ch == '\\' {
                ascii_fallback.push('_');
                needs_rfc5987 = true;
            } else {
                ascii_fallback.push(ch);
            }
        } else {
            ascii_fallback.push('_');
            needs_rfc5987 = true;
        }
    }

    let value = if needs_rfc5987 {
        let encoded = rfc5987_encode(&cleaned);
        format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{encoded}")
    } else {
        format!("attachment; filename=\"{ascii_fallback}\"")
    };

    HeaderValue::from_str(&value)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download\""))
}

fn rfc5987_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            out.push(byte as char);
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0F) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_list_matches_design_rules() {
        assert_eq!(
            classify_filename("photo.JPG"),
            ContentPolicy {
                content_type: "image/jpeg",
                attachment: false,
            }
        );
        assert_eq!(
            classify_filename("diagram.svg"),
            ContentPolicy {
                content_type: "image/svg+xml",
                attachment: false,
            }
        );
        assert_eq!(
            classify_filename("clip.mp4"),
            ContentPolicy {
                content_type: "video/mp4",
                attachment: false,
            }
        );
        assert_eq!(
            classify_filename("song.flac"),
            ContentPolicy {
                content_type: "audio/flac",
                attachment: false,
            }
        );
        assert_eq!(
            classify_filename("captions.vtt"),
            ContentPolicy {
                content_type: "text/vtt; charset=utf-8",
                attachment: false,
            }
        );
        for text_name in ["index.html", "page.HTM", "doc.xhtml", "app.js", "notes.txt"] {
            assert_eq!(
                classify_filename(text_name),
                ContentPolicy {
                    content_type: "text/plain; charset=utf-8",
                    attachment: false,
                },
                "{text_name}"
            );
        }
        for other in [
            "manual.pdf",
            "MANUAL.PDF",
            "archive.zip",
            "program.exe",
            "README",
            ".bashrc",
            "a.wasm",
            "trailing.",
        ] {
            assert_eq!(
                classify_filename(other),
                ContentPolicy {
                    content_type: "application/octet-stream",
                    attachment: true,
                },
                "{other}"
            );
        }
    }

    #[test]
    fn content_disposition_sanitizes_control_and_unicode_names() {
        assert_eq!(
            content_disposition_attachment("notes.txt")
                .to_str()
                .unwrap(),
            "attachment; filename=\"notes.txt\""
        );
        let injected = content_disposition_attachment("bad\"\r\nname.txt");
        let text = injected.to_str().unwrap();
        assert!(!text.contains('\r') && !text.contains('\n'));
        assert!(text.contains("filename=\"bad_name.txt\""), "{text}");
        assert!(text.contains("filename*=UTF-8''bad%22name.txt"), "{text}");

        let unicode = content_disposition_attachment("café.png");
        let text = unicode.to_str().unwrap();
        assert!(text.contains("filename=\"caf_.png\""), "{text}");
        assert!(text.contains("filename*=UTF-8''caf%C3%A9.png"), "{text}");
    }
}
