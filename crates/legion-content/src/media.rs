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

/// Inline `Content-Type` allow-list generated from `crates/legion-core/src/media.rs`
/// (`node scripts/sync-media-table.mjs`).
// BEGIN_CONTENT_MEDIA_TABLE
const INLINE_CONTENT_TYPES: &[(&str, &str)] = &[
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
    ("svg", "image/svg+xml"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
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
    ("flac", "audio/flac"),
    ("mp3", "audio/mpeg"),
    ("mpga", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("mp4a", "audio/mp4"),
    ("alac", "audio/mp4"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("spx", "audio/ogg"),
    ("opus", "audio/opus"),
    ("wav", "audio/wav"),
    ("wave", "audio/wav"),
    ("weba", "audio/webm"),
    ("aif", "audio/aiff"),
    ("aiff", "audio/aiff"),
    ("aifc", "audio/aiff"),
    ("mka", "audio/x-matroska"),
    ("amr", "audio/amr"),
    ("txt", "text/plain; charset=utf-8"),
    ("md", "text/plain; charset=utf-8"),
    ("log", "text/plain; charset=utf-8"),
    ("toml", "text/plain; charset=utf-8"),
    ("yaml", "text/plain; charset=utf-8"),
    ("yml", "text/plain; charset=utf-8"),
    ("ini", "text/plain; charset=utf-8"),
    ("conf", "text/plain; charset=utf-8"),
    ("vtt", "text/vtt; charset=utf-8"),
    ("csv", "text/plain; charset=utf-8"),
    ("json", "text/plain; charset=utf-8"),
    ("text", "text/plain; charset=utf-8"),
    ("srt", "text/plain; charset=utf-8"),
    ("markdown", "text/plain; charset=utf-8"),
    ("mdx", "text/plain; charset=utf-8"),
    ("rst", "text/plain; charset=utf-8"),
    ("adoc", "text/plain; charset=utf-8"),
    ("asciidoc", "text/plain; charset=utf-8"),
    ("org", "text/plain; charset=utf-8"),
    ("tex", "text/plain; charset=utf-8"),
    ("latex", "text/plain; charset=utf-8"),
    ("tsv", "text/plain; charset=utf-8"),
    ("ics", "text/plain; charset=utf-8"),
    ("vcf", "text/plain; charset=utf-8"),
    ("diff", "text/plain; charset=utf-8"),
    ("patch", "text/plain; charset=utf-8"),
    ("jsonc", "text/plain; charset=utf-8"),
    ("json5", "text/plain; charset=utf-8"),
    ("jsonl", "text/plain; charset=utf-8"),
    ("ndjson", "text/plain; charset=utf-8"),
    ("xml", "text/plain; charset=utf-8"),
    ("xsl", "text/plain; charset=utf-8"),
    ("xsd", "text/plain; charset=utf-8"),
    ("plist", "text/plain; charset=utf-8"),
    ("html", "text/plain; charset=utf-8"),
    ("htm", "text/plain; charset=utf-8"),
    ("xhtml", "text/plain; charset=utf-8"),
    ("css", "text/plain; charset=utf-8"),
    ("scss", "text/plain; charset=utf-8"),
    ("sass", "text/plain; charset=utf-8"),
    ("less", "text/plain; charset=utf-8"),
    ("cfg", "text/plain; charset=utf-8"),
    ("env", "text/plain; charset=utf-8"),
    ("lock", "text/plain; charset=utf-8"),
    ("properties", "text/plain; charset=utf-8"),
    ("editorconfig", "text/plain; charset=utf-8"),
    ("cmake", "text/plain; charset=utf-8"),
    ("mk", "text/plain; charset=utf-8"),
    ("make", "text/plain; charset=utf-8"),
    ("dockerfile", "text/plain; charset=utf-8"),
    ("tf", "text/plain; charset=utf-8"),
    ("tfvars", "text/plain; charset=utf-8"),
    ("hcl", "text/plain; charset=utf-8"),
    ("vue", "text/plain; charset=utf-8"),
    ("svelte", "text/plain; charset=utf-8"),
    ("astro", "text/plain; charset=utf-8"),
    ("qml", "text/plain; charset=utf-8"),
    ("glsl", "text/plain; charset=utf-8"),
    ("wgsl", "text/plain; charset=utf-8"),
    ("hlsl", "text/plain; charset=utf-8"),
    ("vert", "text/plain; charset=utf-8"),
    ("frag", "text/plain; charset=utf-8"),
    ("sv", "text/plain; charset=utf-8"),
    ("vhd", "text/plain; charset=utf-8"),
    ("vhdl", "text/plain; charset=utf-8"),
    ("sol", "text/plain; charset=utf-8"),
    ("prisma", "text/plain; charset=utf-8"),
    ("graphql", "text/plain; charset=utf-8"),
    ("gql", "text/plain; charset=utf-8"),
    ("sql", "text/plain; charset=utf-8"),
    ("rs", "text/plain; charset=utf-8"),
    ("c", "text/plain; charset=utf-8"),
    ("h", "text/plain; charset=utf-8"),
    ("cc", "text/plain; charset=utf-8"),
    ("cpp", "text/plain; charset=utf-8"),
    ("cxx", "text/plain; charset=utf-8"),
    ("hh", "text/plain; charset=utf-8"),
    ("hpp", "text/plain; charset=utf-8"),
    ("hxx", "text/plain; charset=utf-8"),
    ("cs", "text/plain; charset=utf-8"),
    ("go", "text/plain; charset=utf-8"),
    ("java", "text/plain; charset=utf-8"),
    ("kt", "text/plain; charset=utf-8"),
    ("kts", "text/plain; charset=utf-8"),
    ("scala", "text/plain; charset=utf-8"),
    ("sc", "text/plain; charset=utf-8"),
    ("swift", "text/plain; charset=utf-8"),
    ("m", "text/plain; charset=utf-8"),
    ("mm", "text/plain; charset=utf-8"),
    ("py", "text/plain; charset=utf-8"),
    ("pyi", "text/plain; charset=utf-8"),
    ("pyw", "text/plain; charset=utf-8"),
    ("rb", "text/plain; charset=utf-8"),
    ("rake", "text/plain; charset=utf-8"),
    ("gemspec", "text/plain; charset=utf-8"),
    ("php", "text/plain; charset=utf-8"),
    ("pl", "text/plain; charset=utf-8"),
    ("pm", "text/plain; charset=utf-8"),
    ("lua", "text/plain; charset=utf-8"),
    ("js", "text/plain; charset=utf-8"),
    ("mjs", "text/plain; charset=utf-8"),
    ("cjs", "text/plain; charset=utf-8"),
    ("jsx", "text/plain; charset=utf-8"),
    ("ts", "text/plain; charset=utf-8"),
    ("cts", "text/plain; charset=utf-8"),
    ("tsx", "text/plain; charset=utf-8"),
    ("sh", "text/plain; charset=utf-8"),
    ("bash", "text/plain; charset=utf-8"),
    ("zsh", "text/plain; charset=utf-8"),
    ("fish", "text/plain; charset=utf-8"),
    ("ksh", "text/plain; charset=utf-8"),
    ("csh", "text/plain; charset=utf-8"),
    ("ps1", "text/plain; charset=utf-8"),
    ("psm1", "text/plain; charset=utf-8"),
    ("psd1", "text/plain; charset=utf-8"),
    ("bat", "text/plain; charset=utf-8"),
    ("cmd", "text/plain; charset=utf-8"),
    ("zig", "text/plain; charset=utf-8"),
    ("nim", "text/plain; charset=utf-8"),
    ("ex", "text/plain; charset=utf-8"),
    ("exs", "text/plain; charset=utf-8"),
    ("erl", "text/plain; charset=utf-8"),
    ("hrl", "text/plain; charset=utf-8"),
    ("hs", "text/plain; charset=utf-8"),
    ("lhs", "text/plain; charset=utf-8"),
    ("ml", "text/plain; charset=utf-8"),
    ("mli", "text/plain; charset=utf-8"),
    ("clj", "text/plain; charset=utf-8"),
    ("cljs", "text/plain; charset=utf-8"),
    ("cljc", "text/plain; charset=utf-8"),
    ("edn", "text/plain; charset=utf-8"),
    ("el", "text/plain; charset=utf-8"),
    ("lisp", "text/plain; charset=utf-8"),
    ("scm", "text/plain; charset=utf-8"),
    ("rkt", "text/plain; charset=utf-8"),
    ("dart", "text/plain; charset=utf-8"),
    ("r", "text/plain; charset=utf-8"),
    ("jl", "text/plain; charset=utf-8"),
    ("groovy", "text/plain; charset=utf-8"),
    ("gradle", "text/plain; charset=utf-8"),
    ("nix", "text/plain; charset=utf-8"),
    ("proto", "text/plain; charset=utf-8"),
    ("asm", "text/plain; charset=utf-8"),
    ("s", "text/plain; charset=utf-8"),
];
// END_CONTENT_MEDIA_TABLE

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

    if let Some(&(_, content_type)) = INLINE_CONTENT_TYPES
        .iter()
        .find(|&&(entry_ext, _)| entry_ext == ext)
    {
        return ContentPolicy {
            content_type,
            attachment: false,
        };
    }

    // Everything else (including PDF, documents, archives, camera raw) forces download as octet-stream.
    ContentPolicy {
        content_type: "application/octet-stream",
        attachment: true,
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
