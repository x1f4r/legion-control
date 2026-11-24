//! Streaming log reader for `job.logs`.
//!
//! Reads at most `limit` bytes (plus a few peek bytes for UTF-8 boundary checks) from `output.log`,
//! whether polling forward from `offset` or reading the last `tail` lines, so a multi-gigabyte log
//! never loads into memory. Never splits a valid multi-byte UTF-8 character across chunks, and
//! replaces genuinely invalid UTF-8 bytes with U+FFFD.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use legion_core::proto::{ErrorBody, ErrorCode};

/// Default number of trailing lines returned when `offset` is omitted.
pub(super) const DEFAULT_TAIL_LINES: u32 = 100;
/// Maximum number of trailing lines accepted.
pub(super) const MAX_TAIL_LINES: u32 = 5_000;
/// Default byte limit per `job.logs` call (64 KiB).
pub(super) const DEFAULT_LOG_LIMIT: u64 = 65_536;
/// Maximum byte limit per `job.logs` call (1 MiB).
pub(super) const MAX_LOG_LIMIT: u64 = 1_048_576;

const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Decoded slice of a job's log file.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct LogSlice {
    pub content: String,
    pub offset: u64,
    pub next_offset: u64,
    pub eof: bool,
    pub truncated: bool,
}

/// Reads forward from byte `offset`, returning at most `limit` bytes (or a single leading
/// multi-byte UTF-8 character if `limit` is smaller than 4 bytes so `next_offset` always moves).
pub(super) fn read_offset(
    path: &Path,
    offset: u64,
    limit: usize,
    job_active: bool,
) -> Result<LogSlice, ErrorBody> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(LogSlice {
                content: String::new(),
                offset,
                next_offset: offset,
                eof: true,
                truncated: false,
            });
        }
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot open job log {}: {err}.", path.display()),
            ));
        }
    };

    file.seek(SeekFrom::Start(offset)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot seek job log {}: {err}.", path.display()),
        )
    })?;

    // Read up to `limit + 4` bytes: up to 3 bytes beyond `limit` distinguish a split multi-byte
    // UTF-8 character from invalid bytes, and the 4th byte tells whether more data follows.
    let want = limit.saturating_add(4);
    let raw = read_up_to(&mut file, want, path)?;

    if raw.is_empty() {
        return Ok(LogSlice {
            content: String::new(),
            offset,
            next_offset: offset,
            eof: true,
            truncated: false,
        });
    }

    let hit_eof = raw.len() <= limit;
    let (used, eof) = if hit_eof {
        let hold = if job_active {
            incomplete_utf8_tail(&raw)
        } else {
            0
        };
        let used = raw.len() - hold;
        (used, hold == 0)
    } else {
        let slice = &raw[..limit];
        let peek = &raw[limit..];
        let split = split_utf8_tail(slice, peek);
        let used = if split > 0 && split == slice.len() {
            // Only possible when `limit < 4` and byte 0 starts a multi-byte character wider than
            // `limit`; consume that single character whole so polling never stalls.
            first_utf8_char_len(&raw).min(raw.len())
        } else {
            slice.len() - split
        };
        let eof = used >= raw.len() && read_up_to(&mut file, 1, path)?.is_empty();
        (used, eof)
    };

    let content = String::from_utf8_lossy(&raw[..used]).into_owned();
    let next_offset = offset.saturating_add(u64::try_from(used).unwrap_or(u64::MAX));
    Ok(LogSlice {
        content,
        offset,
        next_offset,
        eof,
        truncated: !eof,
    })
}

/// Reads up to the last `tail_lines` lines capped at `limit` bytes from the end of `path` without
/// loading the earlier part of the file into memory.
pub(super) fn read_tail(
    path: &Path,
    tail_lines: usize,
    limit: usize,
    job_active: bool,
) -> Result<LogSlice, ErrorBody> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Ok(LogSlice {
                content: String::new(),
                offset: 0,
                next_offset: 0,
                eof: true,
                truncated: false,
            });
        }
        Err(err) => {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot open job log {}: {err}.", path.display()),
            ));
        }
    };

    let file_len = file
        .metadata()
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot inspect job log {}: {err}.", path.display()),
            )
        })?
        .len();
    if file_len == 0 {
        return Ok(LogSlice {
            content: String::new(),
            offset: 0,
            next_offset: 0,
            eof: true,
            truncated: false,
        });
    }

    // Any byte earlier than `file_len - limit` cannot fit into the `limit`-byte suffix. Read up to
    // 3 extra leading bytes so we can detect whether `file_len - limit` lands mid-character.
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    let window_start = file_len.saturating_sub(limit_u64);
    let read_start = window_start.saturating_sub(3);
    let target_bytes = usize::try_from(file_len.saturating_sub(read_start)).unwrap_or(usize::MAX);

    file.seek(SeekFrom::Start(read_start)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot seek job log {}: {err}.", path.display()),
        )
    })?;
    let raw = read_up_to(&mut file, target_bytes, path)?;
    let more_appended = !read_up_to(&mut file, 1, path)?.is_empty();

    let end_hold = if job_active && !more_appended {
        incomplete_utf8_tail(&raw)
    } else {
        0
    };
    let effective_end = raw.len() - end_hold;
    let slice = &raw[..effective_end];

    let line_start = find_tail_lines_start(slice, tail_lines);
    let limit_start = effective_end.saturating_sub(limit);
    let mut cut = line_start.max(limit_start);

    if read_start.saturating_add(u64::try_from(cut).unwrap_or(u64::MAX)) > 0 {
        cut = advance_past_split_utf8_start(slice, cut);
    }

    let actual_offset = read_start.saturating_add(u64::try_from(cut).unwrap_or(u64::MAX));
    let next_offset = read_start.saturating_add(u64::try_from(effective_end).unwrap_or(u64::MAX));
    let content = String::from_utf8_lossy(&slice[cut..effective_end]).into_owned();
    let eof = end_hold == 0 && !more_appended;
    let truncated = actual_offset > 0;

    Ok(LogSlice {
        content,
        offset: actual_offset,
        next_offset,
        eof,
        truncated,
    })
}

fn read_up_to(file: &mut File, want: usize, path: &Path) -> Result<Vec<u8>, ErrorBody> {
    let mut buf = Vec::with_capacity(want.min(READ_CHUNK_BYTES));
    let mut scratch = [0u8; READ_CHUNK_BYTES];
    while buf.len() < want {
        let remaining = want - buf.len();
        let take = remaining.min(scratch.len());
        let n = file.read(&mut scratch[..take]).map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot read job log {}: {err}.", path.display()),
            )
        })?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
    }
    Ok(buf)
}

/// Finds the byte index in `bytes` where the last `tail_lines` lines begin.
///
/// A single trailing newline terminates the final line rather than counting as an extra empty line.
fn find_tail_lines_start(bytes: &[u8], tail_lines: usize) -> usize {
    if bytes.is_empty() || tail_lines == 0 {
        return bytes.len();
    }
    let scan_end = if bytes.ends_with(b"\n") {
        bytes.len() - 1
    } else {
        bytes.len()
    };
    let mut found = 0usize;
    for idx in (0..scan_end).rev() {
        if bytes[idx] == b'\n' {
            found += 1;
            if found == tail_lines {
                return idx + 1;
            }
        }
    }
    0
}

/// Number of trailing bytes in `bytes` that could be the prefix of an unfinished valid UTF-8
/// character (used at EOF while a job is still writing).
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needed = utf8_width(byte);
        return if needed > back { back } else { 0 };
    }
    0
}

/// Number of trailing bytes in `slice` that begin a valid UTF-8 character completed by `peek`.
///
/// Invalid byte sequences at the cut are not held back: they are decoded as U+FFFD immediately.
fn split_utf8_tail(slice: &[u8], peek: &[u8]) -> usize {
    let tail = incomplete_utf8_tail(slice);
    if tail == 0 {
        return 0;
    }
    let start = slice.len() - tail;
    let needed = utf8_width(slice[start]);
    let missing = needed.saturating_sub(tail);
    if missing == 0 || peek.len() < missing {
        return 0;
    }
    let mut combined = [0u8; 4];
    combined[..tail].copy_from_slice(&slice[start..]);
    combined[tail..needed].copy_from_slice(&peek[..missing]);
    if std::str::from_utf8(&combined[..needed]).is_ok() {
        tail
    } else {
        0
    }
}

/// Byte length of the first UTF-8 character in `bytes`, or 1 when byte 0 is not a valid sequence.
fn first_utf8_char_len(bytes: &[u8]) -> usize {
    let Some(&first) = bytes.first() else {
        return 0;
    };
    let needed = utf8_width(first);
    if needed > 1 && bytes.len() >= needed && std::str::from_utf8(&bytes[..needed]).is_ok() {
        needed
    } else {
        1
    }
}

/// When `cut` lands inside a valid multi-byte UTF-8 character in `buf`, advances `cut` to the
/// first byte after that character so the returned text never begins mid-character.
fn advance_past_split_utf8_start(buf: &[u8], cut: usize) -> usize {
    if cut >= buf.len() || (buf[cut] & 0b1100_0000) != 0b1000_0000 {
        return cut;
    }
    for back in 1..=cut.min(3) {
        let lead_idx = cut - back;
        let lead = buf[lead_idx];
        if lead & 0b1100_0000 == 0b1000_0000 {
            continue;
        }
        let needed = utf8_width(lead);
        let char_end = lead_idx + needed;
        if needed > back
            && char_end <= buf.len()
            && std::str::from_utf8(&buf[lead_idx..char_end]).is_ok()
        {
            return char_end;
        }
        break;
    }
    cut
}

fn utf8_width(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn offset_reads_incrementally_without_splitting_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.log");
        let text = "aé€😀b";
        fs::write(&path, text.as_bytes()).unwrap();

        for limit in 1..=text.len() + 2 {
            let mut offset = 0u64;
            let mut assembled = String::new();
            let mut steps = 0;
            loop {
                steps += 1;
                assert!(steps < 32, "stalled at offset {offset} for limit {limit}");
                let slice = read_offset(&path, offset, limit, false).unwrap();
                assert_eq!(slice.offset, offset);
                assert!(!slice.content.contains('\u{fffd}'), "limit={limit}");
                assembled.push_str(&slice.content);
                offset = slice.next_offset;
                if slice.eof {
                    assert!(!slice.truncated);
                    break;
                }
                assert!(slice.truncated);
            }
            assert_eq!(assembled, text, "limit={limit}");
        }
    }

    #[test]
    fn active_job_holds_back_partial_trailing_utf8_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.log");
        // First 2 bytes of '€' (0xE2 0x82 0xAC).
        fs::write(&path, [b'o', b'k', 0xE2, 0x82]).unwrap();

        let active = read_offset(&path, 0, 100, true).unwrap();
        assert_eq!(active.content, "ok");
        assert_eq!(active.next_offset, 2);
        assert!(!active.eof);

        // Once the remaining byte arrives, polling from next_offset decodes '€' cleanly.
        fs::write(&path, [b'o', b'k', 0xE2, 0x82, 0xAC]).unwrap();
        let next = read_offset(&path, active.next_offset, 100, true).unwrap();
        assert_eq!(next.content, "€");
        assert_eq!(next.next_offset, 5);
        assert!(next.eof);

        // When finished with dangling incomplete bytes at EOF, they become U+FFFD.
        fs::write(&path, [b'o', b'k', 0xE2, 0x82]).unwrap();
        let done = read_offset(&path, 0, 100, false).unwrap();
        assert_eq!(done.content, "ok\u{fffd}");
        assert_eq!(done.next_offset, 4);
        assert!(done.eof);
    }

    #[test]
    fn invalid_utf8_is_replaced_without_stalling() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.log");
        fs::write(&path, [b'a', 0xFF, 0xFE, b'b']).unwrap();

        let slice = read_offset(&path, 0, 2, false).unwrap();
        assert_eq!(slice.content, "a\u{fffd}");
        assert_eq!(slice.next_offset, 2);
        assert!(!slice.eof);

        let rest = read_offset(&path, slice.next_offset, 10, false).unwrap();
        assert_eq!(rest.content, "\u{fffd}b");
        assert_eq!(rest.next_offset, 4);
        assert!(rest.eof);
    }

    #[test]
    fn tail_respects_line_count_byte_limit_and_utf8_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.log");
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        let last2 = read_tail(&path, 2, 1024, false).unwrap();
        assert_eq!(last2.content, "three\nfour\n");
        assert_eq!(last2.offset, 8);
        assert_eq!(last2.next_offset, 19);
        assert!(last2.eof);
        assert!(last2.truncated);

        let all = read_tail(&path, 10, 1024, false).unwrap();
        assert_eq!(all.content, "one\ntwo\nthree\nfour\n");
        assert_eq!(all.offset, 0);
        assert!(!all.truncated);

        // Byte cap landing mid-character skips past the split character cleanly.
        fs::write(&path, "éééé").unwrap();
        let capped = read_tail(&path, 100, 5, false).unwrap();
        assert_eq!(capped.content, "éé");
        assert_eq!(capped.offset, 4);
        assert_eq!(capped.next_offset, 8);
        assert!(capped.truncated);
    }

    #[test]
    fn missing_or_empty_log_returns_empty_eof() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.log");
        let tail = read_tail(&missing, 100, 1024, false).unwrap();
        assert_eq!(tail.content, "");
        assert_eq!(tail.offset, 0);
        assert_eq!(tail.next_offset, 0);
        assert!(tail.eof);
        assert!(!tail.truncated);

        let off = read_offset(&missing, 42, 1024, false).unwrap();
        assert_eq!(off.offset, 42);
        assert_eq!(off.next_offset, 42);
        assert!(off.eof);
    }

    #[test]
    fn tail_and_offset_handle_multi_megabyte_logs_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.log");
        let mut data = vec![b'a'; 2 * 1024 * 1024];
        data.extend_from_slice(b"\npenultimate\nfinal-line\n");
        fs::write(&path, &data).unwrap();

        let tail = read_tail(&path, 2, 1024, false).unwrap();
        assert_eq!(tail.content, "penultimate\nfinal-line\n");
        assert!(tail.eof);
        assert!(tail.truncated);
        assert_eq!(tail.next_offset, data.len() as u64);

        let mid = read_offset(&path, 1024 * 1024, 64, false).unwrap();
        assert_eq!(mid.content.len(), 64);
        assert_eq!(mid.offset, 1024 * 1024);
        assert_eq!(mid.next_offset, 1024 * 1024 + 64);
        assert!(!mid.eof);
        assert!(mid.truncated);
    }
}
