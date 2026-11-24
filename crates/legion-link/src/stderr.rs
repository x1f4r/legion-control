//! Bounded ring buffer holding recent stderr lines from a transport for error diagnostics.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Maximum number of recent stderr lines retained for diagnostics.
const MAX_STDERR_LINES: usize = 20;
/// Maximum bytes kept from any single stderr line.
const MAX_STDERR_LINE_BYTES: usize = 1024;
/// Maximum total bytes kept across all retained stderr lines.
const MAX_STDERR_TOTAL_BYTES: usize = 8 * 1024;

/// Bounded ring buffer holding the last lines written to a transport's stderr.
#[derive(Debug, Default)]
pub struct StderrTail {
    inner: Mutex<StderrBuf>,
}

#[derive(Debug, Default)]
struct StderrBuf {
    lines: VecDeque<String>,
    total_bytes: usize,
}

impl StderrTail {
    /// Appends one line (stripping trailing `\r`/`\n` and skipping blank lines).
    pub fn push_line(&self, raw: &str) {
        let trimmed = raw.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            return;
        }
        let line = truncate_utf8(trimmed, MAX_STDERR_LINE_BYTES);
        let mut buf = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        buf.total_bytes += line.len();
        buf.lines.push_back(line);
        while buf.lines.len() > MAX_STDERR_LINES || buf.total_bytes > MAX_STDERR_TOTAL_BYTES {
            let Some(dropped) = buf.lines.pop_front() else {
                break;
            };
            buf.total_bytes = buf.total_bytes.saturating_sub(dropped.len());
        }
    }

    /// Joins the retained stderr lines for inclusion in error hints.
    pub fn text(&self) -> String {
        let buf = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        buf.lines.iter().cloned().collect::<Vec<_>>().join(" | ")
    }

    /// Whether any non-empty stderr line has been collected.
    pub fn is_empty(&self) -> bool {
        let buf = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        buf.lines.is_empty()
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_is_bounded_and_keeps_the_latest_lines() {
        let tail = StderrTail::default();
        for i in 0..50 {
            tail.push_line(&format!("line-{i}\n"));
        }
        let text = tail.text();
        assert!(!text.contains("line-0"), "{text}");
        assert!(text.contains("line-49"), "{text}");
        assert!(text.len() <= MAX_STDERR_TOTAL_BYTES + 200);
    }
}
