//! RFC 9110 single-range parsing, resolution, `ETag`, and `If-Range` matching.

use http::HeaderValue;
use serde::{Deserialize, Serialize};

/// File modification timestamp represented as signed Unix seconds plus sub-second nanoseconds.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ContentTime {
    /// Whole seconds since the Unix epoch (negative before 1970-01-01T00:00:00Z).
    pub secs: i64,
    /// Sub-second nanoseconds in `0..1_000_000_000`.
    pub nanos: u32,
}

impl ContentTime {
    /// Constructs a [`ContentTime`] from raw seconds and nanoseconds.
    pub const fn new(secs: i64, nanos: u32) -> Self {
        Self { secs, nanos }
    }
}

/// Builds a strong `ETag` from a file's `size` and modification time.
pub fn etag(size: u64, mtime: ContentTime) -> HeaderValue {
    let tag = format!("\"{size:x}-{:x}-{:x}\"", mtime.secs as u64, mtime.nanos);
    HeaderValue::from_str(&tag).unwrap_or_else(|_| HeaderValue::from_static("\"0-0-0\""))
}

/// Evaluates `If-Range` against `current_etag` using RFC 9110 strong comparison.
///
/// Only a strong ETag (`"..."`, never `W/"..."`) matching `current_etag` byte-for-byte returns
/// `true`; any mismatch (including weak tags or dates) returns `false` so the server falls back
/// to sending the full representation (`200 OK`).
pub fn if_range_matches(if_range: &str, current_etag: &str) -> bool {
    let trimmed = if_range.trim();
    trimmed.starts_with('"')
        && trimmed.ends_with('"')
        && trimmed.len() >= 2
        && trimmed == current_etag
}

/// Result of parsing a `Range` HTTP request header for single-range serving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParsedRange {
    /// No `Range` header was present.
    None,
    /// The `Range` header uses an unknown unit or requests multiple ranges; per RFC 9110 §14.2 a
    /// single-range server ignores it and returns the full representation (`200 OK`).
    Ignore,
    /// The `Range` header uses `bytes=` with an invalid or unsatisfiable specification (for
    /// example `bytes=10-5`, `bytes=-0`, or non-numeric bounds).
    Invalid,
    /// `bytes=start-` (`end: None`) or `bytes=start-end` (`end: Some(end)` with `start <= end`).
    FromStart {
        /// Inclusive starting byte offset.
        start: u64,
        /// Optional inclusive ending byte offset (`>= start`).
        end: Option<u64>,
    },
    /// `bytes=-suffix` (`suffix > 0`): the last `suffix` bytes of the representation.
    Suffix(u64),
}

/// Concrete inclusive byte range `[start, end]` resolved against a file's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedRange {
    /// First byte offset (inclusive).
    pub start: u64,
    /// Last byte offset (inclusive, always `< size` and `>= start`).
    pub end: u64,
    /// Exact byte count (`end - start + 1`).
    pub length: u64,
}

/// Outcome of resolving a [`ParsedRange`] against a file of `size` bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeDecision {
    /// Send the full representation with `200 OK`.
    Full,
    /// Send the resolved byte range with `206 Partial Content`.
    Partial(ResolvedRange),
    /// Reject with `416 Range Not Satisfiable` and `Content-Range: bytes */<size>`.
    Unsatisfiable,
}

/// Parses a single HTTP `Range` header value.
pub fn parse_range(raw: &str) -> ParsedRange {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ParsedRange::Invalid;
    }
    let Some((unit, spec)) = trimmed.split_once('=') else {
        return ParsedRange::Invalid;
    };
    if !unit.trim().eq_ignore_ascii_case("bytes") {
        // RFC 9110 §14.2: ignore range units the server does not understand.
        return ParsedRange::Ignore;
    }
    let spec = spec.trim();
    if spec.is_empty() {
        return ParsedRange::Invalid;
    }
    if spec.contains(',') {
        // Single-range server: fall back to full 200 OK rather than emitting multipart/byteranges.
        return ParsedRange::Ignore;
    }
    let Some((start_str, end_str)) = spec.split_once('-') else {
        return ParsedRange::Invalid;
    };
    let start_str = start_str.trim();
    let end_str = end_str.trim();

    if start_str.is_empty() {
        // Suffix range: `bytes=-N`
        if end_str.is_empty() || !end_str.bytes().all(|b| b.is_ascii_digit()) {
            return ParsedRange::Invalid;
        }
        let Ok(suffix) = end_str.parse::<u64>() else {
            return ParsedRange::Invalid;
        };
        if suffix == 0 {
            return ParsedRange::Invalid;
        }
        return ParsedRange::Suffix(suffix);
    }

    if !start_str.bytes().all(|b| b.is_ascii_digit()) {
        return ParsedRange::Invalid;
    }
    let Ok(start) = start_str.parse::<u64>() else {
        return ParsedRange::Invalid;
    };

    if end_str.is_empty() {
        // Open-ended range: `bytes=N-`
        return ParsedRange::FromStart { start, end: None };
    }

    if !end_str.bytes().all(|b| b.is_ascii_digit()) {
        return ParsedRange::Invalid;
    }
    let Ok(end) = end_str.parse::<u64>() else {
        return ParsedRange::Invalid;
    };
    if start > end {
        return ParsedRange::Invalid;
    }
    ParsedRange::FromStart {
        start,
        end: Some(end),
    }
}

/// Resolves a [`ParsedRange`] against a representation of `size` bytes.
pub fn resolve_range(parsed: ParsedRange, size: u64) -> RangeDecision {
    match parsed {
        ParsedRange::None | ParsedRange::Ignore => RangeDecision::Full,
        ParsedRange::Invalid => RangeDecision::Unsatisfiable,
        ParsedRange::FromStart { start, end } => {
            if size == 0 || start >= size {
                return RangeDecision::Unsatisfiable;
            }
            let last = size - 1;
            let resolved_end = end.map_or(last, |e| e.min(last));
            RangeDecision::Partial(ResolvedRange {
                start,
                end: resolved_end,
                length: resolved_end - start + 1,
            })
        }
        ParsedRange::Suffix(suffix) => {
            if size == 0 || suffix == 0 {
                return RangeDecision::Unsatisfiable;
            }
            let length = suffix.min(size);
            let start = size - length;
            RangeDecision::Partial(ResolvedRange {
                start,
                end: size - 1,
                length,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_resolve_covers_all_single_range_forms() {
        // First byte
        assert_eq!(
            resolve_range(parse_range("bytes=0-0"), 100),
            RangeDecision::Partial(ResolvedRange {
                start: 0,
                end: 0,
                length: 1,
            })
        );
        // Last byte
        assert_eq!(
            resolve_range(parse_range("bytes=99-99"), 100),
            RangeDecision::Partial(ResolvedRange {
                start: 99,
                end: 99,
                length: 1,
            })
        );
        // End past EOF clamps
        assert_eq!(
            resolve_range(parse_range("bytes=90-500"), 100),
            RangeDecision::Partial(ResolvedRange {
                start: 90,
                end: 99,
                length: 10,
            })
        );
        // Open-ended
        assert_eq!(
            resolve_range(parse_range("bytes=80-"), 100),
            RangeDecision::Partial(ResolvedRange {
                start: 80,
                end: 99,
                length: 20,
            })
        );
        // Suffix smaller than size
        assert_eq!(
            resolve_range(parse_range("bytes=-10"), 100),
            RangeDecision::Partial(ResolvedRange {
                start: 90,
                end: 99,
                length: 10,
            })
        );
        // Suffix larger than size clamps to full file
        assert_eq!(
            resolve_range(parse_range("bytes=-500"), 100),
            RangeDecision::Partial(ResolvedRange {
                start: 0,
                end: 99,
                length: 100,
            })
        );
        // Unsatisfiable cases
        for raw in [
            "bytes=100-100",
            "bytes=100-",
            "bytes=500-600",
            "bytes=-0",
            "bytes=10-5",
            "bytes=abc-def",
            "bytes=-",
        ] {
            assert_eq!(
                resolve_range(parse_range(raw), 100),
                RangeDecision::Unsatisfiable,
                "{raw}"
            );
        }
        // Empty file rejects any byte range
        assert_eq!(
            resolve_range(parse_range("bytes=0-0"), 0),
            RangeDecision::Unsatisfiable
        );
        assert_eq!(
            resolve_range(parse_range("bytes=-1"), 0),
            RangeDecision::Unsatisfiable
        );
        // Multi-range and unknown units fall back to full 200 OK
        assert_eq!(
            resolve_range(parse_range("bytes=0-1, 2-3"), 100),
            RangeDecision::Full
        );
        assert_eq!(
            resolve_range(parse_range("items=0-5"), 100),
            RangeDecision::Full
        );
    }

    #[test]
    fn etag_and_if_range_require_strong_match() {
        let tag = etag(4096, ContentTime::new(1_700_000_000, 123_456_789));
        let tag_str = tag.to_str().unwrap();
        assert!(if_range_matches(tag_str, tag_str));
        assert!(if_range_matches(&format!(" {tag_str} "), tag_str));
        assert!(!if_range_matches(&format!("W/{tag_str}"), tag_str));
        assert!(!if_range_matches("\"other\"", tag_str));
        assert!(!if_range_matches("Wed, 21 Oct 2015 07:28:00 GMT", tag_str));
    }
}
