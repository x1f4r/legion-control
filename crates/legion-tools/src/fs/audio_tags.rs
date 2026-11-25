//! Pure-Rust, memory-bounded audio metadata and embedded cover art parser for `fs.tags` and
//! `fs.thumb`.
//!
//! Supported formats (parsed strictly from an already-opened seekable file handle — never by
//! opening paths):
//! - **MP3**: ID3v2 (2.2, 2.3, 2.4 including global/per-frame unsynchronisation, extended headers,
//!   data length indicators, and all four text encodings), ID3v1 / ID3v1.1 fallback, and container
//!   duration from MPEG frame headers + Xing/Info/VBRI headers without decoding audio.
//! - **WAV** (`RIFF`/`WAVE`): `ID3 ` / `id3 ` chunks, `LIST`/`INFO` fallback, and duration from
//!   `fmt ` + `data` chunk sizes (seeking past `data`).
//! - **AIFF / AIFC** (`FORM`/`AIFF`|`AIFC`): `ID3 ` / `id3 ` chunks and duration from `COMM`
//!   (including 80-bit IEEE 754 extended sample rate, seeking past `SSND`).
//! - **FLAC** (`fLaC`, with optional leading ID3v2): `STREAMINFO` duration, `VORBIS_COMMENT`, and
//!   `PICTURE` blocks (seeking past padding/seektable/audio).
//! - **Ogg Vorbis & Ogg Opus** (`OggS`): multi-page identification and comment header reassembly,
//!   Vorbis comments including base64 `METADATA_BLOCK_PICTURE`, and EOF last-page granule-position
//!   duration.
//! - **MP4 / M4A / M4B** (ISO BMFF): 32-bit and 64-bit box walking (seeking past `mdat`), `mvhd`
//!   v0/v1 duration, and `moov/udta/meta/ilst` items (`©nam`, `©ART`, `©alb`, `aART`, `trkn`,
//!   `disk`, `©day`, `covr`).
//!
//! # Hostile-input bounds
//!
//! Every allocation is checked against both the remaining bytes of the open file and a cumulative
//! per-file tag budget of [`MAX_AUDIO_TAG_BYTES`] (16 MiB) *before* allocating. Audio payload
//! chunks (`data`, `SSND`, `mdat`, audio frames) are skipped by seeking and never read into memory.
//! All arithmetic is overflow-checked and every loop has an explicit upper bound and monotonic
//! offset progression so crafted files can neither OOM, panic, nor hang.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use legion_core::media::{Category, classify_path};
use legion_core::proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Maximum cumulative bytes of tag/metadata/picture data read from a single audio file (16 MiB).
pub(crate) const MAX_AUDIO_TAG_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of Unicode scalar values kept in any text tag field.
pub(crate) const MAX_TAG_FIELD_CHARS: usize = 512;
/// Maximum raw input bytes decoded for any single text tag field before cleaning (8 KiB: 4x the
/// worst-case 512x4-byte UTF-8 or UTF-16 astral sequence with generous headroom for leading
/// stripped characters and multi-value separators, so a 16 MiB invalid-byte value can never
/// expand into tens of MiB before truncation).
const MAX_TEXT_DECODE_BYTES: usize = 8 * 1024;

const MAX_CONTAINER_CHUNKS: usize = 4096;
const MAX_ID3_FRAMES: usize = 4096;
const MAX_VORBIS_COMMENTS: u32 = 65_536;
const MAX_MP4_DEPTH: usize = 8;
const MAX_OGG_SCAN_PAGES: usize = 64;
const MAX_OGG_COMMENT_PAGES: usize = 2048;
const OGG_TAIL_SCAN_BYTES: u64 = 65_536;
const MP3_SYNC_SCAN_BYTES: usize = 64 * 1024;
const COVER_MAGIC_PEEK_BYTES: usize = 64;
const MAX_WAV_INFO_BYTES: u64 = 256 * 1024;
const MAX_WAV_INFO_ENTRIES: usize = 256;

/// Audio container family selected from a file's extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioFormat {
    Mp3,
    Wav,
    Aiff,
    Flac,
    Ogg,
    Mp4,
    /// Recognized `Category::Audio` extension outside the dedicated tag readers above (for
    /// example `.aac`, `.wma`, `.mka`); accepts an optional leading ID3v2 tag and still participates
    /// in folder cover lookup.
    GenericAudio,
}

/// Classifies `path` into an [`AudioFormat`] for `fs.tags` (returning `None` for non-audio files).
///
/// Includes `.mp4` alongside `.m4a` and `.m4b` so MP4 audio files can report their MP4 items.
pub(crate) fn audio_format_for_path(path: &Path) -> Option<AudioFormat> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("mp3" | "mpga") => Some(AudioFormat::Mp3),
        Some("wav" | "wave") => Some(AudioFormat::Wav),
        Some("aif" | "aiff" | "aifc") => Some(AudioFormat::Aiff),
        Some("flac") => Some(AudioFormat::Flac),
        Some("ogg" | "oga" | "opus" | "spx") => Some(AudioFormat::Ogg),
        Some("m4a" | "m4b" | "mp4" | "mp4a" | "alac") => Some(AudioFormat::Mp4),
        _ => (classify_path(path).category == Category::Audio).then_some(AudioFormat::GenericAudio),
    }
}

/// Whether the parser should only check for a valid embedded cover or also retain its bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoverExtractMode {
    /// Only check whether a valid embedded raster cover exists (avoids retaining cover bytes).
    CheckOnly,
    /// Extract the winning embedded cover bytes (front cover preferred, else first valid picture).
    ExtractBytes,
}

/// File-intrinsic audio metadata parsed from an audio file (excluding folder cover lookup).
///
/// Stored directly in `ThumbCache` keyed to the audio file's own stamp so changing the file
/// invalidates it while folder cover additions/removals and per-caller folder permissions are
/// always evaluated fresh.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ParsedAudioTags {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) track: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) disc: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) year: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) duration_s: Option<f64>,
    pub(crate) has_embedded_cover: bool,
}

/// Output of parsing an audio file in the worker.
#[derive(Debug)]
pub(crate) struct ExtractedAudio {
    pub(crate) tags: ParsedAudioTags,
    pub(crate) cover: Option<Vec<u8>>,
}

/// Parses audio tags (and optionally the winning embedded cover) from an already-opened seekable
/// reader of known byte length `file_len`.
pub(crate) fn parse_audio<R: Read + Seek>(
    reader: &mut R,
    file_len: u64,
    format: AudioFormat,
    cover_mode: CoverExtractMode,
    display_path: &Path,
) -> Result<ExtractedAudio, ErrorBody> {
    if file_len == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is empty and cannot be read as an audio file.",
                display_path.display()
            ),
        )
        .with_hint("The file may be truncated or still being written."));
    }

    let mut stream = BoundedStream::new(reader, file_len, display_path);
    let mut tags = ParsedAudioTags::default();
    let mut cover = CoverSlot::new(cover_mode);

    match format {
        AudioFormat::Mp3 => parse_mp3(&mut stream, &mut tags, &mut cover)?,
        AudioFormat::Wav => parse_wav(&mut stream, &mut tags, &mut cover)?,
        AudioFormat::Aiff => parse_aiff(&mut stream, &mut tags, &mut cover)?,
        AudioFormat::Flac => parse_flac(&mut stream, &mut tags, &mut cover)?,
        AudioFormat::Ogg => parse_ogg(&mut stream, &mut tags, &mut cover)?,
        AudioFormat::Mp4 => parse_mp4(&mut stream, &mut tags, &mut cover)?,
        AudioFormat::GenericAudio => parse_generic_audio(&mut stream, &mut tags, &mut cover)?,
    }

    tags.has_embedded_cover = cover.has_any;
    Ok(ExtractedAudio {
        tags,
        cover: cover.into_bytes(),
    })
}

/// Inspects an MP4 container's `moov` box (seeking past `mdat`, never decoding media) to determine
/// whether it contains a video track (`hdlr` handler type `vide`).
///
/// Returns `Some(true)` for video MP4s, `Some(false)` for audio-only MP4s (e.g. `.mp4` music with
/// embedded `covr` art), and `None` if `moov` cannot be read cleanly. Always rewinds `reader` to
/// byte 0 before returning.
pub(crate) fn mp4_has_video_track<R: Read + Seek>(reader: &mut R, file_len: u64) -> Option<bool> {
    let result = mp4_has_video_track_inner(reader, file_len);
    let _ = reader.seek(SeekFrom::Start(0));
    result
}

fn mp4_has_video_track_inner<R: Read + Seek>(reader: &mut R, file_len: u64) -> Option<bool> {
    if file_len < 8 || reader.seek(SeekFrom::Start(0)).is_err() {
        return None;
    }
    let mut pos = 0u64;
    let mut visited = 0usize;
    let mut saw_moov = false;
    let mut has_vide = false;

    while pos + 8 <= file_len && visited < 512 {
        visited += 1;
        let (box_type, payload_start, box_end) = read_mp4_box_header_raw(reader, pos, file_len)?;
        if &box_type == b"moov" {
            saw_moov = true;
            if mp4_container_has_vide(reader, payload_start, box_end, 1, &mut visited) {
                has_vide = true;
                break;
            }
        }
        if box_end <= pos {
            break;
        }
        pos = box_end;
    }
    saw_moov.then_some(has_vide)
}

fn mp4_container_has_vide<R: Read + Seek>(
    reader: &mut R,
    start: u64,
    end: u64,
    depth: usize,
    visited: &mut usize,
) -> bool {
    if depth > 4 || start >= end {
        return false;
    }
    let mut pos = start;
    while pos + 8 <= end && *visited < 512 {
        *visited += 1;
        let Some((box_type, payload_start, box_end)) = read_mp4_box_header_raw(reader, pos, end)
        else {
            return false;
        };
        match &box_type {
            b"trak" | b"mdia" => {
                if mp4_container_has_vide(reader, payload_start, box_end, depth + 1, visited) {
                    return true;
                }
            }
            b"hdlr" => {
                // FullBox: version/flags (4) + pre_defined (4) + handler_type (4) = 12 bytes.
                if box_end.saturating_sub(payload_start) >= 12
                    && reader.seek(SeekFrom::Start(payload_start)).is_ok()
                {
                    let mut buf = [0u8; 12];
                    if reader.read_exact(&mut buf).is_ok() && &buf[8..12] == b"vide" {
                        return true;
                    }
                }
            }
            _ => {}
        }
        if box_end <= pos {
            break;
        }
        pos = box_end;
    }
    false
}

fn read_mp4_box_header_raw<R: Read + Seek>(
    reader: &mut R,
    pos: u64,
    container_end: u64,
) -> Option<([u8; 4], u64, u64)> {
    if pos.checked_add(8)? > container_end {
        return None;
    }
    reader.seek(SeekFrom::Start(pos)).ok()?;
    let mut hdr = [0u8; 8];
    reader.read_exact(&mut hdr).ok()?;
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let box_type = [hdr[4], hdr[5], hdr[6], hdr[7]];
    let (header_len, total_size) = match size32 {
        0 => (8u64, container_end.checked_sub(pos)?),
        1 => {
            if pos.checked_add(16)? > container_end {
                return None;
            }
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext).ok()?;
            let size64 = u64::from_be_bytes(ext);
            if size64 < 16 {
                return None;
            }
            (16u64, size64)
        }
        2..=7 => return None,
        n => (8u64, u64::from(n)),
    };
    let box_end = pos.checked_add(total_size)?;
    if box_end > container_end {
        return None;
    }
    let payload_start = pos.checked_add(header_len)?;
    Some((box_type, payload_start, box_end))
}

/// Returns true for characters that must be removed from audio tag text: control characters,
/// bidirectional overrides/isolates/marks, invisible zero-width/BOM/soft-hyphen characters,
/// Unicode tag characters (`U+E0000..=U+E007F`), and all 66 Unicode noncharacters.
pub(crate) fn is_stripped_tag_char(ch: char) -> bool {
    let cp = ch as u32;
    ch.is_control()
        || matches!(
            ch,
            '\u{00AD}' // soft hyphen
            | '\u{061C}' // Arabic Letter Mark
            | '\u{200B}' // Zero Width Space
            | '\u{200E}'..='\u{200F}' // LRM, RLM
            | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
            | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
            | '\u{FEFF}' // BOM / ZWNBSP
            | '\u{FDD0}'..='\u{FDEF}' // noncharacters
            | '\u{E0000}'..='\u{E007F}' // Unicode tag characters
        )
        || (cp & 0xFFFF) >= 0xFFFE
}

fn is_joiner(ch: char) -> bool {
    matches!(ch, '\u{200C}' | '\u{200D}')
}

fn is_edge_trimmed(ch: char) -> bool {
    ch.is_whitespace() || is_joiner(ch)
}

fn is_variation_selector(ch: char) -> bool {
    matches!(ch, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Covers Unicode combining marks (`Mn`/`Mc`/`Me`) across common scripts plus emoji skin-tone
/// modifiers so the 512-character truncation boundary steps back to the last complete cluster
/// without adding a grapheme-table crate dependency.
fn is_combining_mark(ch: char) -> bool {
    let cp = ch as u32;
    matches!(
        cp,
        0x0300..=0x036F // Combining Diacritical Marks
        | 0x0483..=0x0489 // Cyrillic combining marks
        | 0x0591..=0x05BD
        | 0x05BF
        | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5
        | 0x05C7 // Hebrew marks
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED // Arabic marks
        | 0x0711
        | 0x0730..=0x074A // Syriac
        | 0x07A6..=0x07B0 // Thaana
        | 0x07EB..=0x07F3
        | 0x07FD // NKo
        | 0x0816..=0x0819
        | 0x081B..=0x0823
        | 0x0825..=0x0827
        | 0x0829..=0x082D
        | 0x0859..=0x085B
        | 0x0898..=0x089F
        | 0x08CA..=0x08E1
        | 0x08E3..=0x0903 // Extended Arabic / Devanagari signs
        | 0x093A..=0x094F
        | 0x0951..=0x0957
        | 0x0962..=0x0963 // Devanagari
        | 0x0981..=0x0983
        | 0x09BC
        | 0x09BE..=0x09CD
        | 0x09D7
        | 0x09E2..=0x09E3 // Bengali
        | 0x0A01..=0x0A03
        | 0x0A3C
        | 0x0A3E..=0x0A51
        | 0x0A70..=0x0A71
        | 0x0A75 // Gurmukhi
        | 0x0A81..=0x0A83
        | 0x0ABC
        | 0x0ABE..=0x0ACD
        | 0x0AE2..=0x0AE3
        | 0x0AFA..=0x0AFF // Gujarati
        | 0x0B01..=0x0B03
        | 0x0B3C
        | 0x0B3E..=0x0B57
        | 0x0B62..=0x0B63 // Oriya
        | 0x0B82
        | 0x0BBE..=0x0BD7 // Tamil
        | 0x0C00..=0x0C04
        | 0x0C3C
        | 0x0C3E..=0x0C56
        | 0x0C62..=0x0C63 // Telugu
        | 0x0C81..=0x0C83
        | 0x0CBC
        | 0x0CBE..=0x0CD6
        | 0x0CE2..=0x0CE3 // Kannada
        | 0x0D00..=0x0D03
        | 0x0D3B..=0x0D4E
        | 0x0D57
        | 0x0D62..=0x0D63 // Malayalam
        | 0x0D81..=0x0D83
        | 0x0DCA..=0x0DDF
        | 0x0DF2..=0x0DF3 // Sinhala
        | 0x0E31
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E // Thai
        | 0x0EB1
        | 0x0EB4..=0x0EBC
        | 0x0EC8..=0x0ECE // Lao
        | 0x0F18..=0x0F19
        | 0x0F35
        | 0x0F37
        | 0x0F39
        | 0x0F3E..=0x0F3F
        | 0x0F71..=0x0F84
        | 0x0F86..=0x0F87
        | 0x0F8D..=0x0FBC
        | 0x0FC6 // Tibetan
        | 0x102B..=0x103E
        | 0x1056..=0x1059
        | 0x105E..=0x1060
        | 0x1062..=0x1064
        | 0x1067..=0x106D
        | 0x1071..=0x1074
        | 0x1082..=0x108D
        | 0x108F
        | 0x109A..=0x109D // Myanmar
        | 0x1160..=0x11FF // Hangul Jamo medial/trailing
        | 0x1712..=0x1715
        | 0x1732..=0x1734
        | 0x1752..=0x1753
        | 0x1772..=0x1773
        | 0x17B4..=0x17D3
        | 0x17DD // Khmer & Philippine marks
        | 0x180B..=0x180D
        | 0x180F
        | 0x18A9 // Mongolian
        | 0x1920..=0x193B
        | 0x1A17..=0x1A1B
        | 0x1A55..=0x1A7F
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1B00..=0x1B04
        | 0x1B34..=0x1B44
        | 0x1B6B..=0x1B73
        | 0x1B80..=0x1B82
        | 0x1BA1..=0x1BAD
        | 0x1BE6..=0x1BF3
        | 0x1C24..=0x1C37
        | 0x1CD0..=0x1CD2
        | 0x1CD4..=0x1CE8
        | 0x1CED
        | 0x1CF4
        | 0x1CF7..=0x1CF9
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20FF // Combining Diacritical Marks for Symbols
        | 0x302A..=0x302F
        | 0x3099..=0x309A // CJK / Kana combining marks
        | 0xA8E0..=0xA8F1
        | 0xFB1E
        | 0xFE20..=0xFE2F // Combining Half Marks
        | 0x1F3FB..=0x1F3FF // Emoji skin-tone modifiers
    )
}

fn is_cluster_tail(ch: char) -> bool {
    is_joiner(ch) || is_variation_selector(ch) || is_combining_mark(ch)
}

fn is_regional_indicator(ch: char) -> bool {
    matches!(ch, '\u{1F1E6}'..='\u{1F1FF}')
}

/// Cleans a tag text value per the player contract: maps `U+2028`/`U+2029` to ASCII spaces,
/// removes control/bidirectional/invisible/tag/noncharacter code points, keeps internal
/// `U+200C`/`U+200D` while stripping them and whitespace at both edges, caps at
/// [`MAX_TAG_FIELD_CHARS`] (512) characters stepping back to the last complete cluster on
/// truncation, and returns `None` when empty.
#[cfg(test)]
pub(crate) fn clean_tag_text(raw: &str) -> Option<String> {
    clean_tag_chars(raw.chars())
}

fn clean_tag_chars(chars: impl Iterator<Item = char>) -> Option<String> {
    let mut out = String::with_capacity(MAX_TAG_FIELD_CHARS.min(64));
    let mut count = 0usize;
    let mut started = false;
    let mut truncated_next: Option<char> = None;

    for raw_ch in chars {
        let ch = match raw_ch {
            '\u{2028}' | '\u{2029}' => ' ',
            other if is_stripped_tag_char(other) => continue,
            other => other,
        };
        if !started {
            if is_edge_trimmed(ch) {
                continue;
            }
            started = true;
        }
        if count >= MAX_TAG_FIELD_CHARS {
            truncated_next = Some(ch);
            break;
        }
        out.push(ch);
        count += 1;
    }

    if let Some(next_ch) = truncated_next {
        let mut need_step_back =
            is_cluster_tail(next_ch) || out.chars().next_back().is_some_and(is_cluster_tail);
        while need_step_back {
            while out.chars().next_back().is_some_and(is_cluster_tail) {
                out.pop();
            }
            // Drop the base scalar whose cluster was cut off or ended with a tail modifier.
            out.pop();
            // If another joiner/modifier now sits at the end (for example a multi-part ZWJ emoji
            // sequence), keep stepping back to the last complete cluster.
            need_step_back = out.chars().next_back().is_some_and(is_cluster_tail);
        }
        // Regional indicator symbols pair up into two-scalar flag emojis (U+1F1E6..=U+1F1FF).
        // If truncation leaves an odd trailing run of regional indicators, drop the last scalar so
        // a flag pair is never split in half.
        let trailing_ri = out
            .chars()
            .rev()
            .take_while(|&ch| is_regional_indicator(ch))
            .count();
        if !trailing_ri.is_multiple_of(2) {
            out.pop();
        }
    }

    while out.chars().next_back().is_some_and(is_edge_trimmed) {
        out.pop();
    }
    (!out.is_empty()).then_some(out)
}

/// Returns a prefix of `data` bounded to `max_bytes`, stepping back at most 3 bytes if `max_bytes`
/// falls in the middle of a UTF-8 continuation sequence so valid multi-byte characters at the cut
/// boundary are not turned into `U+FFFD`.
fn bounded_utf8_prefix(data: &[u8], max_bytes: usize) -> &[u8] {
    if data.len() <= max_bytes {
        return data;
    }
    let mut end = max_bytes;
    let floor = max_bytes.saturating_sub(3);
    while end > floor && (data[end] & 0xC0) == 0x80 {
        end -= 1;
    }
    &data[..end]
}

/// Zero-allocation lossy UTF-8 character iterator over a byte slice.
struct LossyUtf8Chars<'a> {
    rest: &'a [u8],
    valid: std::str::Chars<'a>,
    pending_replacement: bool,
}

impl<'a> LossyUtf8Chars<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            valid: "".chars(),
            pending_replacement: false,
        }
    }
}

impl Iterator for LossyUtf8Chars<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        loop {
            if let Some(ch) = self.valid.next() {
                return Some(ch);
            }
            if self.pending_replacement {
                self.pending_replacement = false;
                return Some('\u{FFFD}');
            }
            if self.rest.is_empty() {
                return None;
            }
            match std::str::from_utf8(self.rest) {
                Ok(s) => {
                    self.valid = s.chars();
                    self.rest = &[];
                }
                Err(err) => {
                    let valid_len = err.valid_up_to();
                    if valid_len > 0
                        && let Ok(prefix) = std::str::from_utf8(&self.rest[..valid_len])
                    {
                        self.valid = prefix.chars();
                    }
                    let bad_len = err
                        .error_len()
                        .unwrap_or(self.rest.len() - valid_len)
                        .max(1);
                    self.rest = &self.rest[(valid_len + bad_len).min(self.rest.len())..];
                    self.pending_replacement = true;
                }
            }
        }
    }
}

fn clean_utf8_bytes(data: &[u8]) -> Option<String> {
    let prefix = bounded_utf8_prefix(data, MAX_TEXT_DECODE_BYTES);
    clean_tag_chars(LossyUtf8Chars::new(prefix))
}

fn clean_latin1_bytes(data: &[u8]) -> Option<String> {
    let prefix = &data[..data.len().min(MAX_TEXT_DECODE_BYTES)];
    clean_tag_chars(prefix.iter().map(|&b| char::from(b)))
}

fn clean_utf16_bytes(data: &[u8], little_endian: bool) -> Option<String> {
    let mut len = data.len().min(MAX_TEXT_DECODE_BYTES) & !1;
    // Avoid splitting a UTF-16 surrogate pair when truncating at `MAX_TEXT_DECODE_BYTES`.
    if data.len() > len && len >= 2 {
        let last = if little_endian {
            u16::from_le_bytes([data[len - 2], data[len - 1]])
        } else {
            u16::from_be_bytes([data[len - 2], data[len - 1]])
        };
        if (0xD800..=0xDBFF).contains(&last) {
            len -= 2;
        }
    }
    let units = data[..len].as_chunks::<2>().0.iter().map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    clean_tag_chars(char::decode_utf16(units).map(|res| res.unwrap_or('\u{FFFD}')))
}

/// Parses a positive track or disc number from tag text such as `"3"`, `"03"`, or `"3/12"`.
#[cfg(test)]
pub(crate) fn parse_track_or_disc(raw: &str) -> Option<u32> {
    let cleaned = clean_tag_text(raw)?;
    parse_track_or_disc_cleaned(&cleaned)
}

fn parse_track_or_disc_cleaned(cleaned: &str) -> Option<u32> {
    let head = cleaned.split('/').next()?.trim();
    let n = head.parse::<u32>().ok()?;
    (n > 0).then_some(n)
}

/// Parses a 4-digit year from tag text such as `"2024"`, `"2024-05-12"`, or `"2024-05-12T10:00Z"`.
#[cfg(test)]
pub(crate) fn parse_year(raw: &str) -> Option<u32> {
    let cleaned = clean_tag_text(raw)?;
    parse_year_cleaned(&cleaned)
}

fn parse_year_cleaned(cleaned: &str) -> Option<u32> {
    let bytes = cleaned.as_bytes();
    if bytes.len() >= 4 && bytes[..4].iter().all(u8::is_ascii_digit) {
        let y = cleaned.get(..4)?.parse::<u32>().ok()?;
        return (1..=9999).contains(&y).then_some(y);
    }
    None
}

/// Rounds a positive finite duration to millisecond precision (preserving tiny non-zero durations).
fn normalize_duration(secs: f64) -> Option<f64> {
    if !secs.is_finite() || secs <= 0.0 || secs > 1e9 {
        return None;
    }
    let rounded = (secs * 1000.0).round() / 1000.0;
    if rounded > 0.0 {
        Some(rounded)
    } else {
        Some(secs)
    }
}

/// Sniffs magic bytes to confirm embedded picture bytes belong to one of the five supported raster
/// formats (`JPEG`, `PNG`, `GIF`, `WebP`, `BMP`).
pub(crate) fn is_supported_cover_image(bytes: &[u8]) -> bool {
    if bytes.len() < 8 {
        return false;
    }
    // JPEG
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return true;
    }
    // PNG
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return true;
    }
    // GIF
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return true;
    }
    // WebP
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return true;
    }
    // BMP
    if bytes.starts_with(b"BM") && bytes.len() >= 14 {
        return true;
    }
    false
}

/// Tracks the winning embedded cover picture across a file without holding unnecessary duplicates.
struct CoverSlot {
    mode: CoverExtractMode,
    has_front: bool,
    has_any: bool,
    bytes: Option<Vec<u8>>,
}

impl CoverSlot {
    fn new(mode: CoverExtractMode) -> Self {
        Self {
            mode,
            has_front: false,
            has_any: false,
            bytes: None,
        }
    }

    fn wants_picture(&self, pic_type: u8) -> bool {
        let is_front = pic_type == 3;
        match self.mode {
            CoverExtractMode::CheckOnly => !self.has_any,
            CoverExtractMode::ExtractBytes => (is_front && !self.has_front) || !self.has_any,
        }
    }

    fn consider_slice(&mut self, pic_type: u8, data: &[u8]) {
        if !self.wants_picture(pic_type) || !is_supported_cover_image(data) {
            return;
        }
        let is_front = pic_type == 3;
        self.has_any = true;
        if is_front {
            self.has_front = true;
        }
        if self.mode == CoverExtractMode::ExtractBytes {
            self.bytes = Some(data.to_vec());
        }
    }

    fn consider_owned(&mut self, pic_type: u8, data: Vec<u8>) {
        if !self.wants_picture(pic_type) || !is_supported_cover_image(&data) {
            return;
        }
        let is_front = pic_type == 3;
        self.has_any = true;
        if is_front {
            self.has_front = true;
        }
        if self.mode == CoverExtractMode::ExtractBytes {
            self.bytes = Some(data);
        }
    }

    fn record_checked_cover(&mut self, pic_type: u8, magic_prefix: &[u8]) {
        if is_supported_cover_image(magic_prefix) {
            self.has_any = true;
            if pic_type == 3 {
                self.has_front = true;
            }
        }
    }

    fn into_bytes(self) -> Option<Vec<u8>> {
        self.bytes
    }
}

fn limit_error(display_path: &Path, reason: &'static str, limit: usize, detail: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "Metadata in {} exceeds parser limit ({detail}, limit {limit}).",
            display_path.display()
        ),
    )
    .with_hint("Legion caps metadata counts and nesting depth to bound parser work.")
    .with_details(json!({
        "reason": reason,
        "limit": limit,
    }))
}

/// Bounded seekable stream enforcing `MAX_AUDIO_TAG_BYTES` cumulative tag reads and never trusting
/// declared lengths past EOF or the remaining tag budget.
struct BoundedStream<'a, R: Read + Seek> {
    inner: &'a mut R,
    file_len: u64,
    pos: u64,
    tag_budget: usize,
    display_path: &'a Path,
}

impl<'a, R: Read + Seek> BoundedStream<'a, R> {
    fn new(inner: &'a mut R, file_len: u64, display_path: &'a Path) -> Self {
        Self {
            inner,
            file_len,
            pos: 0,
            tag_budget: MAX_AUDIO_TAG_BYTES,
            display_path,
        }
    }

    fn remaining_file(&self) -> u64 {
        self.file_len.saturating_sub(self.pos)
    }

    fn malformed(&self, detail: &str) -> ErrorBody {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot parse audio tags in {}: {detail}.",
                self.display_path.display()
            ),
        )
        .with_hint("The audio file may be truncated or carry malformed metadata.")
    }

    fn limit_exceeded(&self, reason: &'static str, limit: usize, detail: &str) -> ErrorBody {
        limit_error(self.display_path, reason, limit, detail)
    }

    fn tag_too_large(&self, detail: &str) -> ErrorBody {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Metadata in {} exceeds the 16 MiB tag limit ({detail}).",
                self.display_path.display()
            ),
        )
        .with_hint("Legion reads at most 16 MiB of tag data per audio file.")
        .with_details(json!({
            "reason": "tag_too_large",
            "limit_bytes": MAX_AUDIO_TAG_BYTES,
        }))
    }

    fn charge_tag_budget(&mut self, len: u64) -> Result<usize, ErrorBody> {
        let len_usize =
            usize::try_from(len).map_err(|_| self.tag_too_large("declared length overflows"))?;
        if len_usize > self.tag_budget {
            return Err(self.tag_too_large("declared metadata exceeds remaining 16 MiB budget"));
        }
        self.tag_budget -= len_usize;
        Ok(len_usize)
    }

    fn seek_to(&mut self, offset: u64) -> Result<(), ErrorBody> {
        if offset > self.file_len {
            return Err(self.malformed("seek offset exceeds file length"));
        }
        self.inner
            .seek(SeekFrom::Start(offset))
            .map_err(|err| self.malformed(&format!("seek failed: {err}")))?;
        self.pos = offset;
        Ok(())
    }

    fn skip_bytes(&mut self, count: u64) -> Result<(), ErrorBody> {
        let next = self
            .pos
            .checked_add(count)
            .ok_or_else(|| self.malformed("offset overflow"))?;
        self.seek_to(next)
    }

    fn read_fixed<const N: usize>(&mut self) -> Result<[u8; N], ErrorBody> {
        if self.remaining_file() < N as u64 {
            return Err(self.malformed("unexpected end of file"));
        }
        let mut buf = [0u8; N];
        self.inner
            .read_exact(&mut buf)
            .map_err(|err| self.malformed(&format!("read failed: {err}")))?;
        self.pos = self.pos.saturating_add(N as u64);
        Ok(buf)
    }

    fn read_tag_bytes(&mut self, declared_len: u64) -> Result<Vec<u8>, ErrorBody> {
        let len = self.charge_tag_budget(declared_len)?;
        if declared_len > self.remaining_file() {
            return Err(self.malformed("declared tag length exceeds remaining file size"));
        }
        let mut buf = vec![0u8; len];
        self.inner
            .read_exact(&mut buf)
            .map_err(|err| self.malformed(&format!("read failed: {err}")))?;
        self.pos = self.pos.saturating_add(declared_len);
        Ok(buf)
    }

    /// Reads up to `max_len` raw (non-tag) bytes for container probing (such as MP3 frame sync or
    /// Ogg EOF granule scan), bounded by `remaining_file()`.
    fn read_probe_bytes(&mut self, max_len: usize) -> Result<Vec<u8>, ErrorBody> {
        let to_read = usize::try_from(self.remaining_file())
            .unwrap_or(max_len)
            .min(max_len);
        let mut buf = vec![0u8; to_read];
        self.inner
            .read_exact(&mut buf)
            .map_err(|err| self.malformed(&format!("read failed: {err}")))?;
        self.pos = self.pos.saturating_add(to_read as u64);
        Ok(buf)
    }
}

// =================================================================================================
// MP3: ID3v2 + ID3v1 + MPEG duration (Xing / Info / VBRI / CBR)
// =================================================================================================

fn parse_mp3<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;
    let mut audio_start = 0u64;

    if stream.file_len >= 10 {
        let hdr = stream.read_fixed::<10>()?;
        if &hdr[0..3] == b"ID3" {
            let tag_end = parse_id3v2_from_stream(stream, 0, hdr, tags, cover)?;
            audio_start = tag_end;
        } else {
            stream.seek_to(0)?;
        }
    }

    // Check trailing ID3v1 / ID3v1.1 (last 128 bytes).
    let mut has_id3v1 = false;
    if stream.file_len >= 128 {
        let v1_offset = stream.file_len - 128;
        if v1_offset >= audio_start {
            stream.seek_to(v1_offset)?;
            let v1 = stream.read_fixed::<128>()?;
            if &v1[0..3] == b"TAG" {
                has_id3v1 = true;
                parse_id3v1_into(&v1, tags);
            }
        }
    }

    let audio_end = if has_id3v1 {
        stream.file_len.saturating_sub(128)
    } else {
        stream.file_len
    };
    if audio_end > audio_start {
        stream.seek_to(audio_start)?;
        let probe = stream.read_probe_bytes(MP3_SYNC_SCAN_BYTES)?;
        if let Some(secs) = estimate_mp3_duration(&probe, audio_end - audio_start) {
            tags.duration_s = normalize_duration(secs);
        }
    }

    Ok(())
}

fn parse_generic_audio<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;
    if stream.file_len >= 10 {
        let hdr = stream.read_fixed::<10>()?;
        if &hdr[0..3] == b"ID3" {
            let _ = parse_id3v2_from_stream(stream, 0, hdr, tags, cover)?;
        }
    }
    Ok(())
}

fn parse_id3v1_into(v1: &[u8; 128], tags: &mut ParsedAudioTags) {
    let latin1 = |slice: &[u8]| -> Option<String> {
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        clean_latin1_bytes(&slice[..end])
    };
    if tags.title.is_none() {
        tags.title = latin1(&v1[3..33]);
    }
    if tags.artist.is_none() {
        tags.artist = latin1(&v1[33..63]);
    }
    if tags.album.is_none() {
        tags.album = latin1(&v1[63..93]);
    }
    if tags.year.is_none()
        && let Some(yr) = latin1(&v1[93..97])
    {
        tags.year = parse_year_cleaned(&yr);
    }
    // ID3v1.1: comment byte 28 == 0 and byte 29 == non-zero track number.
    if tags.track.is_none() && v1[125] == 0 && v1[126] != 0 {
        tags.track = Some(u32::from(v1[126]));
    }
}

#[derive(Clone, Copy)]
struct MpegFrameHeader {
    version_bits: u8,
    layer_bits: u8,
    bitrate_bps: u32,
    sample_rate: u32,
    samples_per_frame: u32,
    frame_len: usize,
    is_mono: bool,
}

fn parse_mpeg_header(b: [u8; 4]) -> Option<MpegFrameHeader> {
    let hdr = u32::from_be_bytes(b);
    if (hdr & 0xFFE0_0000) != 0xFFE0_0000 {
        return None;
    }
    let version_bits = ((hdr >> 19) & 0x03) as u8;
    let layer_bits = ((hdr >> 17) & 0x03) as u8;
    let bitrate_idx = ((hdr >> 12) & 0x0F) as usize;
    let sr_idx = ((hdr >> 10) & 0x03) as usize;
    let padding = ((hdr >> 9) & 0x01) as usize;
    let channel_mode = ((hdr >> 6) & 0x03) as u8;

    if version_bits == 1 || layer_bits == 0 || bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3
    {
        return None;
    }

    let sample_rate = match version_bits {
        3 => [44_100, 48_000, 32_000][sr_idx],
        2 => [22_050, 24_000, 16_000][sr_idx],
        0 => [11_025, 12_000, 8_000][sr_idx],
        _ => return None,
    };

    const M1_L1: [u32; 15] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const M1_L2: [u32; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const M1_L3: [u32; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const M2_L1: [u32; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const M2_L23: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

    let kbps = match (version_bits == 3, layer_bits) {
        (true, 3) => M1_L1[bitrate_idx],
        (true, 2) => M1_L2[bitrate_idx],
        (true, 1) => M1_L3[bitrate_idx],
        (false, 3) => M2_L1[bitrate_idx],
        (false, 2 | 1) => M2_L23[bitrate_idx],
        _ => return None,
    };
    let bitrate_bps = kbps.checked_mul(1000)?;

    let samples_per_frame = match (version_bits == 3, layer_bits) {
        (_, 3) => 384,
        (_, 2) => 1152,
        (true, 1) => 1152,
        (false, 1) => 576,
        _ => return None,
    };

    let frame_len = if layer_bits == 3 {
        ((12 * usize::try_from(bitrate_bps).ok()? / usize::try_from(sample_rate).ok()?) + padding)
            .checked_mul(4)?
    } else {
        (usize::try_from(samples_per_frame / 8).ok()? * usize::try_from(bitrate_bps).ok()?
            / usize::try_from(sample_rate).ok()?)
            + padding
    };
    if frame_len < 4 {
        return None;
    }

    Some(MpegFrameHeader {
        version_bits,
        layer_bits,
        bitrate_bps,
        sample_rate,
        samples_per_frame,
        frame_len,
        is_mono: channel_mode == 3,
    })
}

fn estimate_mp3_duration(probe: &[u8], total_audio_bytes: u64) -> Option<f64> {
    if probe.len() < 4 {
        return None;
    }
    for i in 0..probe.len().saturating_sub(3) {
        if probe[i] != 0xFF || (probe[i + 1] & 0xE0) != 0xE0 {
            continue;
        }
        let Some(first) = parse_mpeg_header([probe[i], probe[i + 1], probe[i + 2], probe[i + 3]])
        else {
            continue;
        };

        // Check for Xing / Info VBR header inside this first frame.
        let xing_rel = match (first.version_bits == 3, first.is_mono) {
            (true, false) => 36usize,
            (true, true) => 21,
            (false, false) => 21,
            (false, true) => 13,
        };
        if let Some(xing_pos) = i.checked_add(xing_rel)
            && let Some(xh) = probe.get(xing_pos..xing_pos + 12)
            && (&xh[0..4] == b"Xing" || &xh[0..4] == b"Info")
        {
            let flags = u32::from_be_bytes([xh[4], xh[5], xh[6], xh[7]]);
            if (flags & 0x01) != 0 {
                let total_frames = u32::from_be_bytes([xh[8], xh[9], xh[10], xh[11]]);
                if total_frames > 0 {
                    let total_samples =
                        f64::from(total_frames) * f64::from(first.samples_per_frame);
                    return Some(total_samples / f64::from(first.sample_rate));
                }
            }
        }

        // Check for Fraunhofer VBRI header at offset 36 inside the first frame.
        if let Some(vbri_pos) = i.checked_add(36)
            && let Some(vh) = probe.get(vbri_pos..vbri_pos + 18)
            && &vh[0..4] == b"VBRI"
        {
            let total_frames = u32::from_be_bytes([vh[14], vh[15], vh[16], vh[17]]);
            if total_frames > 0 {
                let total_samples = f64::from(total_frames) * f64::from(first.samples_per_frame);
                return Some(total_samples / f64::from(first.sample_rate));
            }
        }

        // Verify a second consecutive MPEG frame with matching version/layer when the probe holds
        // it, so random `0xFF 0xFB` bytes in non-MP3 data are not mistaken for MPEG frames.
        let confirmed = if let Some(next_off) = i.checked_add(first.frame_len) {
            if let Some(next_hdr) = probe.get(next_off..next_off + 4) {
                parse_mpeg_header([next_hdr[0], next_hdr[1], next_hdr[2], next_hdr[3]]).is_some_and(
                    |second| {
                        second.version_bits == first.version_bits
                            && second.layer_bits == first.layer_bits
                    },
                )
            } else {
                // Single-frame short clip where the entire audio region is one valid frame.
                (first.frame_len as u64) <= total_audio_bytes.saturating_sub(i as u64)
            }
        } else {
            false
        };

        if confirmed {
            let audio_bytes = total_audio_bytes.saturating_sub(i as u64);
            if audio_bytes > 0 {
                return Some((audio_bytes as f64 * 8.0) / f64::from(first.bitrate_bps));
            }
        }
    }
    None
}

// =================================================================================================
// ID3v2 (2.2, 2.3, 2.4)
// =================================================================================================

fn decode_synchsafe(b: [u8; 4]) -> Option<u32> {
    if b.iter().any(|&byte| byte >= 0x80) {
        return None;
    }
    Some(
        (u32::from(b[0]) << 21)
            | (u32::from(b[1]) << 14)
            | (u32::from(b[2]) << 7)
            | u32::from(b[3]),
    )
}

fn deunsync_in_place(data: &mut Vec<u8>) {
    let mut read = 0usize;
    let mut write = 0usize;
    let len = data.len();
    while read < len {
        let b = data[read];
        data[write] = b;
        write += 1;
        if b == 0xFF && read + 1 < len && data[read + 1] == 0x00 {
            read += 2;
        } else {
            read += 1;
        }
    }
    data.truncate(write);
}

fn deunsync_prefix(data: &[u8], max_out: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len().min(max_out));
    let mut i = 0usize;
    while i < data.len() && out.len() < max_out {
        let b = data[i];
        out.push(b);
        if b == 0xFF && data.get(i + 1) == Some(&0x00) {
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// Zero-allocation streaming reader over an unsynchronised ID3v2.4 frame payload.
struct DeunsyncReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> DeunsyncReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn next_byte(&mut self) -> Option<u8> {
        let &b = self.data.get(self.pos)?;
        if b == 0xFF && self.data.get(self.pos + 1) == Some(&0x00) {
            self.pos += 2;
        } else {
            self.pos += 1;
        }
        Some(b)
    }

    fn skip_terminated_text(&mut self, encoding: u8) -> bool {
        match encoding {
            0 | 3 => {
                while let Some(b) = self.next_byte() {
                    if b == 0 {
                        return true;
                    }
                }
                false
            }
            1 | 2 => loop {
                let (Some(b0), Some(b1)) = (self.next_byte(), self.next_byte()) else {
                    return false;
                };
                if b0 == 0 && b1 == 0 {
                    return true;
                }
            },
            _ => false,
        }
    }
}

fn parse_id3v2_from_stream<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tag_start: u64,
    hdr: [u8; 10],
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<u64, ErrorBody> {
    let major = hdr[3];
    let minor = hdr[4];
    let flags = hdr[5];
    if !matches!(major, 2..=4) || minor == 0xFF {
        return Err(stream.malformed("unsupported ID3v2 version"));
    }
    let body_size = decode_synchsafe([hdr[6], hdr[7], hdr[8], hdr[9]])
        .ok_or_else(|| stream.malformed("invalid ID3v2 synchsafe size"))?;
    let footer_len = if major == 4 && (flags & 0x10) != 0 {
        10u64
    } else {
        0u64
    };
    let body = stream.read_tag_bytes(u64::from(body_size))?;
    if footer_len > 0 {
        stream.skip_bytes(footer_len)?;
    }
    parse_id3v2_body(major, flags, body, tags, cover, stream.display_path)?;
    tag_start
        .checked_add(10)
        .and_then(|v| v.checked_add(u64::from(body_size)))
        .and_then(|v| v.checked_add(footer_len))
        .ok_or_else(|| stream.malformed("ID3v2 size overflow"))
}

fn parse_id3v2_slice(
    raw: Vec<u8>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
    display_path: &Path,
) -> Result<(), ErrorBody> {
    let bad = |detail: &str| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot parse audio tags in {}: {detail}.",
                display_path.display()
            ),
        )
        .with_hint("The audio file may be truncated or carry malformed metadata.")
    };
    if raw.len() < 10 || &raw[0..3] != b"ID3" {
        return Err(bad("invalid ID3v2 chunk header"));
    }
    let major = raw[3];
    let minor = raw[4];
    let flags = raw[5];
    if !matches!(major, 2..=4) || minor == 0xFF {
        return Err(bad("unsupported ID3v2 version"));
    }
    let body_size = decode_synchsafe([raw[6], raw[7], raw[8], raw[9]])
        .ok_or_else(|| bad("invalid ID3v2 synchsafe size"))? as usize;
    let body_end = 10usize
        .checked_add(body_size)
        .ok_or_else(|| bad("ID3v2 size overflow"))?;
    if body_end > raw.len() {
        return Err(bad("ID3v2 chunk is truncated"));
    }
    let mut body = raw;
    body.truncate(body_end);
    body.drain(..10);
    parse_id3v2_body(major, flags, body, tags, cover, display_path)
}

fn parse_id3v2_body(
    major: u8,
    header_flags: u8,
    mut body: Vec<u8>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
    display_path: &Path,
) -> Result<(), ErrorBody> {
    let bad = |detail: &str| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot parse audio tags in {}: {detail}.",
                display_path.display()
            ),
        )
        .with_hint("The audio file may be truncated or carry malformed metadata.")
    };
    let global_unsync = (header_flags & 0x80) != 0;
    let has_ext_header = (header_flags & 0x40) != 0;

    // ID3v2.2 and ID3v2.3 apply global unsynchronisation to the entire tag body after the header.
    // Deunsync in place because removing `0x00` after `0xFF` only shrinks the buffer, avoiding any
    // duplicate multi-megabyte allocation.
    if global_unsync && major < 4 {
        deunsync_in_place(&mut body);
    }

    let mut pos = 0usize;
    if has_ext_header && major >= 3 {
        let Some(ext_hdr) = body.get(pos..pos + 4) else {
            return Err(bad("truncated ID3v2 extended header"));
        };
        let skip = if major == 4 {
            let sz = decode_synchsafe([ext_hdr[0], ext_hdr[1], ext_hdr[2], ext_hdr[3]])
                .ok_or_else(|| bad("invalid ID3v2.4 extended header size"))?
                as usize;
            if sz < 6 {
                return Err(bad("invalid ID3v2.4 extended header size"));
            }
            sz
        } else {
            let sz = u32::from_be_bytes([ext_hdr[0], ext_hdr[1], ext_hdr[2], ext_hdr[3]]) as usize;
            4usize
                .checked_add(sz)
                .ok_or_else(|| bad("ID3v2.3 extended header overflow"))?
        };
        if pos.checked_add(skip).is_none_or(|end| end > body.len()) {
            return Err(bad("ID3v2 extended header exceeds tag bounds"));
        }
        pos += skip;
    }

    let mut frames_seen = 0usize;
    let frame_hdr_len = if major == 2 { 6usize } else { 10usize };

    while pos + frame_hdr_len <= body.len() {
        if body[pos] == 0 {
            // Reached zero-padding at the end of the ID3v2 tag.
            break;
        }

        let (id_bytes, frame_size, flags_lo) = if major == 2 {
            let id = &body[pos..pos + 3];
            if !id
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                break;
            }
            let sz = u32::from_be_bytes([0, body[pos + 3], body[pos + 4], body[pos + 5]]) as usize;
            (id, sz, 0u8)
        } else {
            let id = &body[pos..pos + 4];
            if !id
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                break;
            }
            let sz_bytes = [body[pos + 4], body[pos + 5], body[pos + 6], body[pos + 7]];
            let sz = if major == 4 {
                decode_synchsafe(sz_bytes)
                    .ok_or_else(|| bad("invalid ID3v2.4 synchsafe frame size"))?
                    as usize
            } else {
                u32::from_be_bytes(sz_bytes) as usize
            };
            (id, sz, body[pos + 9])
        };

        if frames_seen >= MAX_ID3_FRAMES {
            return Err(limit_error(
                display_path,
                "too_many_id3_frames",
                MAX_ID3_FRAMES,
                "too many ID3v2 frames",
            ));
        }
        frames_seen += 1;

        if frame_size == 0 {
            return Err(bad("zero-length ID3v2 frame"));
        }
        let data_start = pos + frame_hdr_len;
        let Some(data_end) = data_start.checked_add(frame_size) else {
            return Err(bad("ID3v2 frame size overflow"));
        };
        if data_end > body.len() {
            return Err(bad("ID3v2 frame length exceeds tag bounds"));
        }
        let raw_payload = &body[data_start..data_end];
        pos = data_end;

        // Skip compressed or encrypted frames safely without attempting decompression.
        let is_compressed =
            (major == 3 && (flags_lo & 0x80) != 0) || (major == 4 && (flags_lo & 0x08) != 0);
        let is_encrypted =
            (major == 3 && (flags_lo & 0x40) != 0) || (major == 4 && (flags_lo & 0x04) != 0);
        if is_compressed || is_encrypted {
            continue;
        }

        let has_group =
            (major == 3 && (flags_lo & 0x20) != 0) || (major == 4 && (flags_lo & 0x40) != 0);
        let has_dli = major == 4 && (flags_lo & 0x01) != 0;
        let frame_unsync = major == 4 && ((flags_lo & 0x02) != 0 || global_unsync);

        let mut payload = raw_payload;
        if has_group {
            let Some(rest) = payload.get(1..) else {
                continue;
            };
            payload = rest;
        }
        if has_dli {
            let Some(rest) = payload.get(4..) else {
                continue;
            };
            payload = rest;
        }

        let decode_text = |payload: &[u8]| -> Option<String> {
            if frame_unsync {
                let prefix = deunsync_prefix(payload, MAX_TEXT_DECODE_BYTES + 1);
                clean_id3_text_frame(major, &prefix)
            } else {
                clean_id3_text_frame(major, payload)
            }
        };
        let decode_first_text = |payload: &[u8]| -> Option<String> {
            if frame_unsync {
                let prefix = deunsync_prefix(payload, MAX_TEXT_DECODE_BYTES + 1);
                clean_id3_first_text_value(major, &prefix)
            } else {
                clean_id3_first_text_value(major, payload)
            }
        };

        match id_bytes {
            b"TIT2" | b"TT2" => {
                if tags.title.is_none() {
                    tags.title = decode_text(payload);
                }
            }
            b"TPE1" | b"TP1" => {
                if tags.artist.is_none() {
                    tags.artist = decode_text(payload);
                }
            }
            b"TALB" | b"TAL" => {
                if tags.album.is_none() {
                    tags.album = decode_text(payload);
                }
            }
            b"TPE2" | b"TP2" => {
                if tags.album_artist.is_none() {
                    tags.album_artist = decode_text(payload);
                }
            }
            b"TRCK" | b"TRK" => {
                if tags.track.is_none()
                    && let Some(text) = decode_first_text(payload)
                {
                    tags.track = parse_track_or_disc_cleaned(&text);
                }
            }
            b"TPOS" | b"TPA" => {
                if tags.disc.is_none()
                    && let Some(text) = decode_first_text(payload)
                {
                    tags.disc = parse_track_or_disc_cleaned(&text);
                }
            }
            b"TDRC" | b"TYER" | b"TYE" => {
                if tags.year.is_none()
                    && let Some(text) = decode_first_text(payload)
                {
                    tags.year = parse_year_cleaned(&text);
                }
            }
            b"APIC" => {
                if frame_unsync {
                    parse_id3_apic_unsynced(payload, cover);
                } else {
                    parse_id3_apic(payload, cover);
                }
            }
            b"PIC" => parse_id3_pic(payload, cover),
            _ => {}
        }
    }

    Ok(())
}

/// Splits an ID3v2.4 text payload (after the encoding byte) into NUL-separated raw segments
/// bounded to `MAX_TEXT_DECODE_BYTES`, using 2-byte aligned `0x00 0x00` for UTF-16 encodings and
/// single `0x00` for Latin-1/UTF-8.
fn split_id3v24_segments(encoding: u8, data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    match encoding {
        0 | 3 => {
            let bounded = if encoding == 3 {
                bounded_utf8_prefix(data, MAX_TEXT_DECODE_BYTES)
            } else {
                &data[..data.len().min(MAX_TEXT_DECODE_BYTES)]
            };
            for seg in bounded.split(|&b| b == 0) {
                if !seg.is_empty() {
                    out.push(seg);
                }
            }
        }
        1 | 2 => {
            let bounded = &data[..data.len().min(MAX_TEXT_DECODE_BYTES) & !1];
            let mut seg_start = 0usize;
            let mut i = 0usize;
            while i + 1 < bounded.len() {
                if bounded[i] == 0 && bounded[i + 1] == 0 {
                    if i > seg_start {
                        out.push(&bounded[seg_start..i]);
                    }
                    i += 2;
                    seg_start = i;
                } else {
                    i += 2;
                }
            }
            if seg_start < bounded.len() {
                out.push(&bounded[seg_start..]);
            }
        }
        _ => {}
    }
    out
}

fn clean_id3_text_frame(major: u8, payload: &[u8]) -> Option<String> {
    let (&enc, data) = payload.split_first()?;
    if major < 4 {
        return clean_id3_string(enc, data);
    }
    let segments = split_id3v24_segments(enc, data);
    let mut joined = String::new();
    let mut joined_chars = 0usize;
    let mut value_count = 0usize;
    for seg in segments {
        // Each ID3v2.4 NUL-separated value is decoded independently with its own BOM.
        let Some(cleaned) = clean_id3_string(enc, seg) else {
            continue;
        };
        if value_count > 0 {
            joined.push_str(", ");
            joined_chars += 2;
        }
        joined_chars += cleaned.chars().count();
        joined.push_str(&cleaned);
        value_count += 1;
        // Stop collecting once we already exceed the 512-char field cap with cluster headroom.
        if joined_chars > MAX_TAG_FIELD_CHARS + 32 {
            break;
        }
    }
    match value_count {
        0 => None,
        1 => Some(joined),
        _ => clean_tag_chars(joined.chars()),
    }
}

fn clean_id3_first_text_value(major: u8, payload: &[u8]) -> Option<String> {
    let (&enc, data) = payload.split_first()?;
    if major < 4 {
        return clean_id3_string(enc, data);
    }
    for seg in split_id3v24_segments(enc, data) {
        if let Some(cleaned) = clean_id3_string(enc, seg) {
            return Some(cleaned);
        }
    }
    None
}

fn clean_id3_string(encoding: u8, data: &[u8]) -> Option<String> {
    match encoding {
        // ISO-8859-1 (Latin-1)
        0 => clean_latin1_bytes(data),
        // UTF-16 with BOM
        1 => {
            let (le, body) = if data.starts_with(&[0xFF, 0xFE]) {
                (true, &data[2..])
            } else if data.starts_with(&[0xFE, 0xFF]) {
                (false, &data[2..])
            } else {
                (true, data)
            };
            clean_utf16_bytes(body, le)
        }
        // UTF-16BE without BOM
        2 => clean_utf16_bytes(data, false),
        // UTF-8
        3 => clean_utf8_bytes(data),
        _ => None,
    }
}

fn parse_id3_apic_unsynced(payload: &[u8], cover: &mut CoverSlot) {
    if (cover.mode == CoverExtractMode::CheckOnly && cover.has_any) || cover.has_front {
        return;
    }
    let mut r = DeunsyncReader::new(payload);
    let Some(enc) = r.next_byte() else {
        return;
    };
    if enc > 3 {
        return;
    }
    // Read MIME up to NUL, checking for external URL link (`-->`).
    let mut mime_len = 0usize;
    let mut mime_head = [0u8; 3];
    loop {
        let Some(b) = r.next_byte() else {
            return;
        };
        if b == 0 {
            break;
        }
        if mime_len < 3 {
            mime_head[mime_len] = b;
        }
        mime_len += 1;
    }
    if mime_len == 3 && &mime_head == b"-->" {
        return;
    }
    let Some(pic_type) = r.next_byte() else {
        return;
    };
    if !cover.wants_picture(pic_type) {
        return;
    }
    if !r.skip_terminated_text(enc) {
        return;
    }
    match cover.mode {
        CoverExtractMode::CheckOnly => {
            let mut peek = [0u8; COVER_MAGIC_PEEK_BYTES];
            let mut n = 0usize;
            while n < COVER_MAGIC_PEEK_BYTES {
                let Some(b) = r.next_byte() else {
                    break;
                };
                peek[n] = b;
                n += 1;
            }
            cover.record_checked_cover(pic_type, &peek[..n]);
        }
        CoverExtractMode::ExtractBytes => {
            let remaining_upper = payload.len().saturating_sub(r.pos);
            let mut img = Vec::with_capacity(remaining_upper);
            while let Some(b) = r.next_byte() {
                img.push(b);
            }
            cover.consider_owned(pic_type, img);
        }
    }
}

fn skip_id3_terminated_text(encoding: u8, data: &[u8]) -> Option<&[u8]> {
    match encoding {
        0 | 3 => {
            let nul = data.iter().position(|&b| b == 0)?;
            data.get(nul + 1..)
        }
        1 | 2 => {
            let mut i = 0usize;
            while i + 1 < data.len() {
                if data[i] == 0 && data[i + 1] == 0 {
                    return data.get(i + 2..);
                }
                i += 2;
            }
            None
        }
        _ => None,
    }
}

fn parse_id3_apic(payload: &[u8], cover: &mut CoverSlot) {
    let Some((&enc, rest)) = payload.split_first() else {
        return;
    };
    if enc > 3 {
        return;
    }
    let Some(mime_end) = rest.iter().position(|&b| b == 0) else {
        return;
    };
    let mime = &rest[..mime_end];
    // External URL link (`-->`) is never embedded image data and must never be fetched.
    if mime == b"-->" {
        return;
    }
    let Some(after_mime) = rest.get(mime_end + 1..) else {
        return;
    };
    let Some((&pic_type, desc_and_img)) = after_mime.split_first() else {
        return;
    };
    let Some(img_bytes) = skip_id3_terminated_text(enc, desc_and_img) else {
        return;
    };
    cover.consider_slice(pic_type, img_bytes);
}

fn parse_id3_pic(payload: &[u8], cover: &mut CoverSlot) {
    if payload.len() < 6 {
        return;
    }
    let enc = payload[0];
    if enc > 3 {
        return;
    }
    let img_fmt = &payload[1..4];
    if img_fmt == b"-->" {
        return;
    }
    let pic_type = payload[4];
    let Some(img_bytes) = skip_id3_terminated_text(enc, &payload[5..]) else {
        return;
    };
    cover.consider_slice(pic_type, img_bytes);
}

// =================================================================================================
// WAV (RIFF / WAVE)
// =================================================================================================

fn parse_wav<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;
    let hdr = stream.read_fixed::<12>()?;
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return Err(stream.malformed("not a RIFF WAVE file"));
    }

    let mut byte_rate = 0u32;
    let mut data_bytes = 0u64;
    let mut info_fallback = ParsedAudioTags::default();
    let mut chunks = 0usize;

    while stream.remaining_file() >= 8 {
        if chunks >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many WAV chunks",
            ));
        }
        chunks += 1;
        let ch = stream.read_fixed::<8>()?;
        let fourcc = [ch[0], ch[1], ch[2], ch[3]];
        let declared_size = u64::from(u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]));
        let remaining = stream.remaining_file();
        // Streamed and in-progress WAV recordings commonly declare `0xFFFFFFFF` (or more than
        // currently written bytes) on the final `data` chunk. Clamp payload/trailing chunks to EOF
        // so earlier metadata and current duration are preserved, while still rejecting truncated
        // header or tag chunks.
        let chunk_size = if declared_size > remaining {
            match &fourcc {
                b"fmt " | b"ID3 " | b"id3 " | b"LIST" => {
                    return Err(stream.malformed("WAV chunk length exceeds file size"));
                }
                _ => remaining,
            }
        } else {
            declared_size
        };
        let pad = chunk_size & 1;
        let chunk_start = stream.pos;
        let next_pos = chunk_start
            .checked_add(chunk_size)
            .and_then(|v| v.checked_add(pad))
            .ok_or_else(|| stream.malformed("WAV chunk offset overflow"))?
            .min(stream.file_len);

        match &fourcc {
            b"fmt " => {
                if chunk_size >= 16 {
                    let fmt = stream.read_fixed::<16>()?;
                    let channels = u32::from(u16::from_le_bytes([fmt[2], fmt[3]]));
                    let sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                    let br = u32::from_le_bytes([fmt[8], fmt[9], fmt[10], fmt[11]]);
                    let bits_per_sample = u32::from(u16::from_le_bytes([fmt[14], fmt[15]]));
                    byte_rate = if br > 0 {
                        br
                    } else {
                        channels
                            .saturating_mul(sample_rate)
                            .saturating_mul(bits_per_sample / 8)
                    };
                }
            }
            b"data" => {
                data_bytes = chunk_size;
            }
            b"ID3 " | b"id3 " => {
                let raw = stream.read_tag_bytes(chunk_size)?;
                parse_id3v2_slice(raw, tags, cover, stream.display_path)?;
            }
            b"LIST" if (4..=MAX_WAV_INFO_BYTES).contains(&chunk_size) => {
                let list_type = stream.read_fixed::<4>()?;
                if &list_type == b"INFO" {
                    let raw = stream.read_tag_bytes(chunk_size - 4)?;
                    parse_riff_info(&raw, &mut info_fallback, stream.display_path)?;
                }
            }
            _ => {}
        }

        stream.seek_to(next_pos)?;
    }

    merge_fallback_tags(tags, info_fallback);
    if byte_rate > 0 && data_bytes > 0 {
        tags.duration_s = normalize_duration(data_bytes as f64 / f64::from(byte_rate));
    }
    Ok(())
}

fn parse_riff_info(
    raw: &[u8],
    out: &mut ParsedAudioTags,
    display_path: &Path,
) -> Result<(), ErrorBody> {
    let mut pos = 0usize;
    let mut count = 0usize;
    while pos + 8 <= raw.len() {
        if count >= MAX_WAV_INFO_ENTRIES {
            return Err(limit_error(
                display_path,
                "too_many_riff_info_entries",
                MAX_WAV_INFO_ENTRIES,
                "too many RIFF INFO entries",
            ));
        }
        count += 1;
        let id = &raw[pos..pos + 4];
        let len =
            u32::from_le_bytes([raw[pos + 4], raw[pos + 5], raw[pos + 6], raw[pos + 7]]) as usize;
        let start = pos + 8;
        let Some(end) = start.checked_add(len) else {
            break;
        };
        if end > raw.len() {
            break;
        }
        let val_slice = &raw[start..end];
        match id {
            b"INAM" if out.title.is_none() => out.title = clean_utf8_bytes(val_slice),
            b"IART" if out.artist.is_none() => out.artist = clean_utf8_bytes(val_slice),
            b"IPRD" if out.album.is_none() => out.album = clean_utf8_bytes(val_slice),
            b"ICRD" if out.year.is_none() => {
                out.year = clean_utf8_bytes(val_slice).and_then(|s| parse_year_cleaned(&s));
            }
            b"ITRK" if out.track.is_none() => {
                out.track =
                    clean_utf8_bytes(val_slice).and_then(|s| parse_track_or_disc_cleaned(&s));
            }
            _ => {}
        }
        let padded = len.saturating_add(len & 1);
        let Some(next) = start.checked_add(padded) else {
            break;
        };
        pos = next;
    }
    Ok(())
}

fn merge_fallback_tags(dst: &mut ParsedAudioTags, fallback: ParsedAudioTags) {
    if dst.title.is_none() {
        dst.title = fallback.title;
    }
    if dst.artist.is_none() {
        dst.artist = fallback.artist;
    }
    if dst.album.is_none() {
        dst.album = fallback.album;
    }
    if dst.album_artist.is_none() {
        dst.album_artist = fallback.album_artist;
    }
    if dst.track.is_none() {
        dst.track = fallback.track;
    }
    if dst.disc.is_none() {
        dst.disc = fallback.disc;
    }
    if dst.year.is_none() {
        dst.year = fallback.year;
    }
}

// =================================================================================================
// AIFF / AIFC (FORM / AIFF | AIFC)
// =================================================================================================

fn decode_ieee80_be(b: [u8; 10]) -> f64 {
    let sign = (b[0] & 0x80) != 0;
    let exp = (u16::from(b[0] & 0x7F) << 8) | u16::from(b[1]);
    let mantissa = u64::from_be_bytes([b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9]]);
    if exp == 0 && mantissa == 0 {
        return 0.0;
    }
    if exp == 0x7FFF {
        return f64::NAN;
    }
    let val = (mantissa as f64) * 2.0f64.powi(i32::from(exp) - 16_383 - 63);
    if sign { -val } else { val }
}

fn parse_aiff<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;
    let hdr = stream.read_fixed::<12>()?;
    if &hdr[0..4] != b"FORM" || (&hdr[8..12] != b"AIFF" && &hdr[8..12] != b"AIFC") {
        return Err(stream.malformed("not an AIFF FORM file"));
    }

    let mut channels = 0u16;
    let mut sample_size_bits = 0u16;
    let mut num_sample_frames = 0u32;
    let mut sample_rate = 0.0f64;
    let mut ssnd_bytes = 0u64;
    let mut chunks = 0usize;

    while stream.remaining_file() >= 8 {
        if chunks >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many AIFF chunks",
            ));
        }
        chunks += 1;
        let ch = stream.read_fixed::<8>()?;
        let fourcc = [ch[0], ch[1], ch[2], ch[3]];
        let declared_size = u64::from(u32::from_be_bytes([ch[4], ch[5], ch[6], ch[7]]));
        let remaining = stream.remaining_file();
        // Streamed and in-progress AIFF recordings commonly declare `0xFFFFFFFF` (or more than
        // currently written bytes) on the final `SSND` chunk. Clamp payload/trailing chunks to EOF
        // so earlier metadata and current duration are preserved, while still rejecting truncated
        // header or tag chunks.
        let chunk_size = if declared_size > remaining {
            match &fourcc {
                b"COMM" | b"ID3 " | b"id3 " => {
                    return Err(stream.malformed("AIFF chunk length exceeds file size"));
                }
                _ => remaining,
            }
        } else {
            declared_size
        };
        let pad = chunk_size & 1;
        let chunk_start = stream.pos;
        let next_pos = chunk_start
            .checked_add(chunk_size)
            .and_then(|v| v.checked_add(pad))
            .ok_or_else(|| stream.malformed("AIFF chunk offset overflow"))?
            .min(stream.file_len);

        match &fourcc {
            b"COMM" => {
                if chunk_size >= 18 {
                    let comm = stream.read_fixed::<18>()?;
                    channels = u16::from_be_bytes([comm[0], comm[1]]);
                    num_sample_frames = u32::from_be_bytes([comm[2], comm[3], comm[4], comm[5]]);
                    sample_size_bits = u16::from_be_bytes([comm[6], comm[7]]);
                    sample_rate = decode_ieee80_be([
                        comm[8], comm[9], comm[10], comm[11], comm[12], comm[13], comm[14],
                        comm[15], comm[16], comm[17],
                    ]);
                }
            }
            b"SSND" => {
                ssnd_bytes = chunk_size;
            }
            b"ID3 " | b"id3 " => {
                let raw = stream.read_tag_bytes(chunk_size)?;
                parse_id3v2_slice(raw, tags, cover, stream.display_path)?;
            }
            _ => {}
        }

        stream.seek_to(next_pos)?;
    }

    if sample_rate.is_finite() && sample_rate > 0.0 {
        if num_sample_frames > 0 && num_sample_frames != u32::MAX {
            tags.duration_s = normalize_duration(f64::from(num_sample_frames) / sample_rate);
        } else if channels > 0 && sample_size_bits > 0 && ssnd_bytes > 8 {
            let bytes_per_frame =
                u64::from(channels).saturating_mul(u64::from(sample_size_bits.div_ceil(8)));
            if let Some(frames) = (ssnd_bytes - 8).checked_div(bytes_per_frame)
                && frames > 0
            {
                tags.duration_s = normalize_duration(frames as f64 / sample_rate);
            }
        }
    }
    Ok(())
}

// =================================================================================================
// FLAC metadata blocks
// =================================================================================================

fn parse_flac<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;
    let mut id3_fallback = ParsedAudioTags::default();
    if stream.file_len >= 10 {
        let peek = stream.read_fixed::<10>()?;
        if &peek[0..3] == b"ID3" {
            let _ = parse_id3v2_from_stream(stream, 0, peek, &mut id3_fallback, cover)?;
        } else {
            stream.seek_to(0)?;
        }
    }

    let magic = stream.read_fixed::<4>()?;
    if &magic != b"fLaC" {
        return Err(stream.malformed("missing fLaC signature"));
    }

    let mut blocks = 0usize;
    loop {
        if blocks >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many FLAC metadata blocks",
            ));
        }
        blocks += 1;

        let hdr = stream.read_fixed::<4>()?;
        let is_last = (hdr[0] & 0x80) != 0;
        let block_type = hdr[0] & 0x7F;
        if block_type == 127 {
            return Err(stream.malformed("invalid FLAC metadata block type 127"));
        }
        let block_len = u64::from(u32::from_be_bytes([0, hdr[1], hdr[2], hdr[3]]));
        if block_len > stream.remaining_file() {
            return Err(stream.malformed("FLAC metadata block length exceeds file size"));
        }
        let block_start = stream.pos;
        let block_end = block_start
            .checked_add(block_len)
            .ok_or_else(|| stream.malformed("FLAC block offset overflow"))?;

        match block_type {
            // STREAMINFO
            0 => {
                if block_len < 34 {
                    return Err(stream.malformed("truncated FLAC STREAMINFO block"));
                }
                let info = stream.read_fixed::<34>()?;
                let sample_rate = (u32::from(info[10]) << 12)
                    | (u32::from(info[11]) << 4)
                    | (u32::from(info[12]) >> 4);
                let total_samples = (u64::from(info[13] & 0x0F) << 32)
                    | u64::from(u32::from_be_bytes([info[14], info[15], info[16], info[17]]));
                if sample_rate > 0 && total_samples > 0 {
                    tags.duration_s =
                        normalize_duration(total_samples as f64 / f64::from(sample_rate));
                }
            }
            // VORBIS_COMMENT
            4 => {
                let raw = stream.read_tag_bytes(block_len)?;
                parse_vorbis_comments(&raw, tags, cover, stream.display_path)?;
            }
            // PICTURE
            6 => {
                parse_flac_picture_from_stream(stream, block_len, cover)?;
            }
            // PADDING, APPLICATION, SEEKTABLE, CUESHEET, etc.: seek past without allocating.
            _ => {}
        }

        stream.seek_to(block_end)?;
        if is_last {
            break;
        }
    }

    merge_fallback_tags(tags, id3_fallback);
    Ok(())
}

fn parse_flac_picture_from_stream<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    block_len: u64,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    // Charge the full picture block length against the 16 MiB cumulative tag budget before reading.
    let _ = stream.charge_tag_budget(block_len)?;
    let block_start = stream.pos;
    let block_end = block_start
        .checked_add(block_len)
        .ok_or_else(|| stream.malformed("FLAC picture block overflow"))?;

    if block_len < 32 {
        return Err(stream.malformed("truncated FLAC PICTURE block"));
    }
    let pt_bytes = stream.read_fixed::<4>()?;
    let pic_type_u32 = u32::from_be_bytes(pt_bytes);
    let pic_type = u8::try_from(pic_type_u32).unwrap_or(0);

    let mime_len = u64::from(u32::from_be_bytes(stream.read_fixed::<4>()?));
    if stream
        .pos
        .checked_add(mime_len)
        .is_none_or(|e| e > block_end)
    {
        return Err(stream.malformed("FLAC PICTURE MIME length exceeds block bounds"));
    }
    let is_url_link = if mime_len == 3 {
        let m = stream.read_fixed::<3>()?;
        &m == b"-->"
    } else {
        stream.skip_bytes(mime_len)?;
        false
    };
    if is_url_link {
        return Ok(());
    }

    let desc_len = u64::from(u32::from_be_bytes(stream.read_fixed::<4>()?));
    if stream
        .pos
        .checked_add(desc_len)
        .is_none_or(|e| e > block_end)
    {
        return Err(stream.malformed("FLAC PICTURE description length exceeds block bounds"));
    }
    stream.skip_bytes(desc_len)?;

    // width(4) + height(4) + depth(4) + colors(4) + data_len(4) = 20 bytes
    if stream.pos.checked_add(20).is_none_or(|e| e > block_end) {
        return Err(stream.malformed("truncated FLAC PICTURE metadata fields"));
    }
    stream.skip_bytes(16)?;
    let data_len = u64::from(u32::from_be_bytes(stream.read_fixed::<4>()?));
    if stream
        .pos
        .checked_add(data_len)
        .is_none_or(|e| e > block_end)
    {
        return Err(stream.malformed("FLAC PICTURE data length exceeds block bounds"));
    }
    if data_len == 0 || !cover.wants_picture(pic_type) {
        return Ok(());
    }

    match cover.mode {
        CoverExtractMode::CheckOnly => {
            let peek_len = usize::try_from(data_len)
                .unwrap_or(COVER_MAGIC_PEEK_BYTES)
                .min(COVER_MAGIC_PEEK_BYTES);
            let prefix = stream.read_probe_bytes(peek_len)?;
            cover.record_checked_cover(pic_type, &prefix);
        }
        CoverExtractMode::ExtractBytes => {
            let data_usize = usize::try_from(data_len)
                .map_err(|_| stream.tag_too_large("picture length overflows"))?;
            let mut img = vec![0u8; data_usize];
            stream
                .inner
                .read_exact(&mut img)
                .map_err(|err| stream.malformed(&format!("read failed: {err}")))?;
            stream.pos = stream.pos.saturating_add(data_len);
            cover.consider_owned(pic_type, img);
        }
    }
    Ok(())
}

/// Zero-copy streaming RFC 4648 base64 decoder that skips ASCII whitespace in-place so line-wrapped
/// `METADATA_BLOCK_PICTURE` comments neither copy into a compacted buffer nor decode the full image
/// when only checking cover presence.
struct Base64StreamReader<'a> {
    input: &'a [u8],
    pos: usize,
    buf: [u8; 3],
    buf_pos: usize,
    buf_len: usize,
    finished: bool,
}

impl<'a> Base64StreamReader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            buf: [0; 3],
            buf_pos: 0,
            buf_len: 0,
            finished: false,
        }
    }

    fn decode_sym(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    fn next_non_ws(&mut self) -> Option<u8> {
        while let Some(&b) = self.input.get(self.pos) {
            self.pos += 1;
            if !b.is_ascii_whitespace() {
                return Some(b);
            }
        }
        None
    }

    fn fill_buf(&mut self) -> Result<bool, ()> {
        if self.finished {
            return Ok(false);
        }
        let Some(c0) = self.next_non_ws() else {
            self.finished = true;
            return Ok(false);
        };
        let (Some(c1), Some(c2), Some(c3)) =
            (self.next_non_ws(), self.next_non_ws(), self.next_non_ws())
        else {
            return Err(());
        };
        let v0 = Self::decode_sym(c0).ok_or(())?;
        let v1 = Self::decode_sym(c1).ok_or(())?;
        self.buf_pos = 0;
        if c2 == b'=' {
            if c3 != b'=' || (v1 & 0x0F) != 0 {
                return Err(());
            }
            self.buf[0] = (v0 << 2) | (v1 >> 4);
            self.buf_len = 1;
            self.finished = true;
            if self.next_non_ws().is_some() {
                return Err(());
            }
        } else if c3 == b'=' {
            let v2 = Self::decode_sym(c2).ok_or(())?;
            if (v2 & 0x03) != 0 {
                return Err(());
            }
            self.buf[0] = (v0 << 2) | (v1 >> 4);
            self.buf[1] = (v1 << 4) | (v2 >> 2);
            self.buf_len = 2;
            self.finished = true;
            if self.next_non_ws().is_some() {
                return Err(());
            }
        } else {
            let v2 = Self::decode_sym(c2).ok_or(())?;
            let v3 = Self::decode_sym(c3).ok_or(())?;
            self.buf[0] = (v0 << 2) | (v1 >> 4);
            self.buf[1] = (v1 << 4) | (v2 >> 2);
            self.buf[2] = (v2 << 6) | v3;
            self.buf_len = 3;
        }
        Ok(true)
    }

    fn read_exact(&mut self, mut dst: &mut [u8]) -> Result<(), ()> {
        while !dst.is_empty() {
            if self.buf_pos >= self.buf_len && !self.fill_buf()? {
                return Err(());
            }
            let avail = self.buf_len - self.buf_pos;
            let take = avail.min(dst.len());
            dst[..take].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + take]);
            self.buf_pos += take;
            dst = &mut dst[take..];
        }
        Ok(())
    }

    fn read_u32_be(&mut self) -> Result<u32, ()> {
        let mut b = [0u8; 4];
        self.read_exact(&mut b)?;
        Ok(u32::from_be_bytes(b))
    }

    fn skip_bytes(&mut self, mut n: usize) -> Result<(), ()> {
        while n > 0 {
            if self.buf_pos >= self.buf_len && !self.fill_buf()? {
                return Err(());
            }
            let avail = self.buf_len - self.buf_pos;
            let take = avail.min(n);
            self.buf_pos += take;
            n -= take;
        }
        Ok(())
    }

    /// Computes the exact number of decoded bytes still available (without allocating) by counting
    /// non-whitespace base64 symbols in the unread tail.
    fn remaining_decoded_len(&self) -> Result<usize, ()> {
        let buffered = self.buf_len - self.buf_pos;
        if self.finished {
            return Ok(buffered);
        }
        let mut non_ws = 0usize;
        let mut pad = 0usize;
        for &b in &self.input[self.pos..] {
            if b.is_ascii_whitespace() {
                continue;
            }
            non_ws += 1;
            if b == b'=' {
                pad += 1;
                if pad > 2 {
                    return Err(());
                }
            } else if pad > 0 {
                return Err(());
            }
        }
        if !non_ws.is_multiple_of(4) {
            return Err(());
        }
        let tail_bytes = (non_ws / 4)
            .checked_mul(3)
            .and_then(|v| v.checked_sub(pad))
            .ok_or(())?;
        buffered.checked_add(tail_bytes).ok_or(())
    }
}

fn parse_vorbis_metadata_block_picture(val_bytes: &[u8], cover: &mut CoverSlot) -> Result<(), ()> {
    if (cover.mode == CoverExtractMode::CheckOnly && cover.has_any) || cover.has_front {
        return Ok(());
    }
    let mut b64 = Base64StreamReader::new(val_bytes);
    let pic_type_u32 = b64.read_u32_be()?;
    let pic_type = u8::try_from(pic_type_u32).unwrap_or(0);
    if !cover.wants_picture(pic_type) {
        return Ok(());
    }
    let mime_len = b64.read_u32_be()? as usize;
    if mime_len > val_bytes.len() {
        return Err(());
    }
    if mime_len == 3 {
        let mut mime = [0u8; 3];
        b64.read_exact(&mut mime)?;
        if &mime == b"-->" {
            return Ok(());
        }
    } else {
        b64.skip_bytes(mime_len)?;
    }
    let desc_len = b64.read_u32_be()? as usize;
    if desc_len > val_bytes.len() {
        return Err(());
    }
    b64.skip_bytes(desc_len)?;
    // width(4) + height(4) + depth(4) + colors(4) = 16 bytes.
    b64.skip_bytes(16)?;
    let data_len = b64.read_u32_be()? as usize;
    if data_len == 0 || data_len > MAX_AUDIO_TAG_BYTES {
        return Err(());
    }
    if b64.remaining_decoded_len()? < data_len {
        return Err(());
    }
    match cover.mode {
        CoverExtractMode::CheckOnly => {
            let peek_len = data_len.min(COVER_MAGIC_PEEK_BYTES);
            let mut peek = [0u8; COVER_MAGIC_PEEK_BYTES];
            b64.read_exact(&mut peek[..peek_len])?;
            cover.record_checked_cover(pic_type, &peek[..peek_len]);
        }
        CoverExtractMode::ExtractBytes => {
            let mut img = vec![0u8; data_len];
            b64.read_exact(&mut img)?;
            cover.consider_owned(pic_type, img);
        }
    }
    Ok(())
}

// =================================================================================================
// Vorbis Comments (shared by FLAC, Ogg Vorbis, Ogg Opus)
// =================================================================================================

fn parse_vorbis_comments(
    raw: &[u8],
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
    display_path: &Path,
) -> Result<(), ErrorBody> {
    let bad = |detail: &str| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Cannot parse audio tags in {}: {detail}.",
                display_path.display()
            ),
        )
        .with_hint("The audio file may be truncated or carry malformed metadata.")
    };
    if raw.len() < 8 {
        return Err(bad("truncated Vorbis comment block"));
    }
    let vendor_len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    let count_off = 4usize
        .checked_add(vendor_len)
        .ok_or_else(|| bad("Vorbis vendor length overflow"))?;
    let mut pos = count_off
        .checked_add(4)
        .ok_or_else(|| bad("Vorbis comment list offset overflow"))?;
    if pos > raw.len() {
        return Err(bad("Vorbis vendor string exceeds comment block"));
    }
    let comment_count = u32::from_le_bytes([
        raw[count_off],
        raw[count_off + 1],
        raw[count_off + 2],
        raw[count_off + 3],
    ]);
    if comment_count > MAX_VORBIS_COMMENTS {
        return Err(limit_error(
            display_path,
            "too_many_vorbis_comments",
            MAX_VORBIS_COMMENTS as usize,
            "too many Vorbis comment entries",
        ));
    }

    for _ in 0..comment_count {
        let len_end = pos
            .checked_add(4)
            .ok_or_else(|| bad("Vorbis comment offset overflow"))?;
        if len_end > raw.len() {
            return Err(bad("truncated Vorbis comment entry length"));
        }
        let entry_len =
            u32::from_le_bytes([raw[pos], raw[pos + 1], raw[pos + 2], raw[pos + 3]]) as usize;
        let entry_end = len_end
            .checked_add(entry_len)
            .ok_or_else(|| bad("Vorbis comment entry length overflow"))?;
        if entry_end > raw.len() {
            return Err(bad("Vorbis comment entry exceeds block bounds"));
        }
        let entry = &raw[len_end..entry_end];
        pos = entry_end;

        let Some(eq) = entry.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key_bytes = &entry[..eq];
        let val_bytes = &entry[eq + 1..];

        if key_bytes.eq_ignore_ascii_case(b"METADATA_BLOCK_PICTURE") {
            let _ = parse_vorbis_metadata_block_picture(val_bytes, cover);
            continue;
        }

        if key_bytes.eq_ignore_ascii_case(b"TITLE") {
            if tags.title.is_none() {
                tags.title = clean_utf8_bytes(val_bytes);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"ARTIST") {
            if tags.artist.is_none() {
                tags.artist = clean_utf8_bytes(val_bytes);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"ALBUM") {
            if tags.album.is_none() {
                tags.album = clean_utf8_bytes(val_bytes);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"ALBUMARTIST")
            || key_bytes.eq_ignore_ascii_case(b"ALBUM ARTIST")
        {
            if tags.album_artist.is_none() {
                tags.album_artist = clean_utf8_bytes(val_bytes);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"TRACKNUMBER") {
            if tags.track.is_none() {
                tags.track =
                    clean_utf8_bytes(val_bytes).and_then(|s| parse_track_or_disc_cleaned(&s));
            }
        } else if key_bytes.eq_ignore_ascii_case(b"DISCNUMBER") {
            if tags.disc.is_none() {
                tags.disc =
                    clean_utf8_bytes(val_bytes).and_then(|s| parse_track_or_disc_cleaned(&s));
            }
        } else if (key_bytes.eq_ignore_ascii_case(b"DATE")
            || key_bytes.eq_ignore_ascii_case(b"YEAR"))
            && tags.year.is_none()
        {
            tags.year = clean_utf8_bytes(val_bytes).and_then(|s| parse_year_cleaned(&s));
        }
    }

    Ok(())
}

// =================================================================================================
// Ogg Vorbis & Ogg Opus
// =================================================================================================

#[derive(Clone, Copy)]
enum OggCodec {
    Vorbis { sample_rate: u32 },
    Opus { pre_skip: u16 },
}

struct OggPageHeader {
    header_type: u8,
    serial: u32,
    segments: [u8; 255],
    num_segments: usize,
    body_len: usize,
}

fn read_ogg_page_header<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
) -> Result<OggPageHeader, ErrorBody> {
    let hdr = stream.read_fixed::<27>()?;
    if &hdr[0..4] != b"OggS" || hdr[4] != 0 {
        return Err(stream.malformed("invalid Ogg page header"));
    }
    let header_type = hdr[5];
    let serial = u32::from_le_bytes([hdr[14], hdr[15], hdr[16], hdr[17]]);
    let num_segments = usize::from(hdr[26]);
    if stream.remaining_file() < num_segments as u64 {
        return Err(stream.malformed("truncated Ogg segment table"));
    }
    let mut segments = [0u8; 255];
    stream
        .inner
        .read_exact(&mut segments[..num_segments])
        .map_err(|err| stream.malformed(&format!("read failed: {err}")))?;
    stream.pos = stream.pos.saturating_add(num_segments as u64);

    let body_len: usize = segments[..num_segments]
        .iter()
        .map(|&s| usize::from(s))
        .sum();
    if body_len as u64 > stream.remaining_file() {
        return Err(stream.malformed("Ogg page payload exceeds file size"));
    }
    Ok(OggPageHeader {
        header_type,
        serial,
        segments,
        num_segments,
        body_len,
    })
}

fn parse_ogg<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;

    // 1. Locate the first Vorbis or Opus BOS identification page.
    let mut found: Option<(u32, OggCodec, Vec<u8>, bool)> = None;
    let mut scan_pages = 0usize;
    while stream.remaining_file() >= 27 {
        if scan_pages >= MAX_OGG_SCAN_PAGES {
            return Err(stream.limit_exceeded(
                "too_many_ogg_scan_pages",
                MAX_OGG_SCAN_PAGES,
                "too many Ogg pages before audio stream header",
            ));
        }
        scan_pages += 1;
        let page = read_ogg_page_header(stream)?;
        let is_bos = (page.header_type & 0x02) != 0;
        let page_end = stream.pos.saturating_add(page.body_len as u64);

        if is_bos && found.is_none() && page.num_segments > 0 {
            let body = stream.read_tag_bytes(page.body_len as u64)?;
            let mut pkt0_len = 0usize;
            let mut pkt0_complete = false;
            let mut seg_idx = 0usize;
            while seg_idx < page.num_segments {
                let s = usize::from(page.segments[seg_idx]);
                pkt0_len += s;
                seg_idx += 1;
                if s < 255 {
                    pkt0_complete = true;
                    break;
                }
            }
            if pkt0_complete && let Some(pkt0) = body.get(..pkt0_len) {
                let codec = if pkt0.len() >= 30 && pkt0.starts_with(b"\x01vorbis") {
                    let sr = u32::from_le_bytes([pkt0[12], pkt0[13], pkt0[14], pkt0[15]]);
                    Some(OggCodec::Vorbis { sample_rate: sr })
                } else if pkt0.len() >= 19 && pkt0.starts_with(b"OpusHead") {
                    let pre_skip = u16::from_le_bytes([pkt0[10], pkt0[11]]);
                    Some(OggCodec::Opus { pre_skip })
                } else {
                    None
                };
                if let Some(codec) = codec {
                    // Check whether packet 1 already begins on this BOS page (unusual, but valid).
                    let mut pkt1 = Vec::new();
                    let mut pkt1_done = false;
                    let mut off = pkt0_len;
                    while seg_idx < page.num_segments {
                        let s = usize::from(page.segments[seg_idx]);
                        if let Some(sl) = body.get(off..off + s) {
                            pkt1.extend_from_slice(sl);
                        }
                        off += s;
                        seg_idx += 1;
                        if s < 255 {
                            pkt1_done = true;
                            break;
                        }
                    }
                    found = Some((page.serial, codec, pkt1, pkt1_done));
                    if pkt1_done {
                        break;
                    }
                }
            }
        } else {
            stream.seek_to(page_end)?;
            if found.is_some() {
                // Rewind back to this page so the comment packet loop below reads it.
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }

    let Some((serial, codec, mut comment_packet, mut comment_done)) = found else {
        return Err(stream.malformed("no Ogg Vorbis or Opus stream found"));
    };

    // 2. Reassemble packet 1 (comment header) across pages for `serial`, bounded by the 16 MiB tag
    //    budget. Cap `comment_packet` capacity growth at `MAX_AUDIO_TAG_BYTES` so a 15.9 MiB
    //    comment packet never triggers a 32 MiB Vec doubling spike.
    let mut pages_read = 0usize;
    let mut page_buf = Vec::new();
    while !comment_done && stream.remaining_file() >= 27 {
        if pages_read >= MAX_OGG_COMMENT_PAGES {
            return Err(stream.limit_exceeded(
                "too_many_ogg_comment_pages",
                MAX_OGG_COMMENT_PAGES,
                "too many Ogg comment header pages",
            ));
        }
        pages_read += 1;
        let page = read_ogg_page_header(stream)?;
        let page_end = stream.pos.saturating_add(page.body_len as u64);
        if page.serial != serial {
            stream.seek_to(page_end)?;
            continue;
        }
        if !comment_packet.is_empty() && (page.header_type & 0x01) == 0 {
            return Err(stream.malformed("Ogg comment packet missing continuation flag"));
        }

        let mut want_bytes = 0usize;
        for &seg in &page.segments[..page.num_segments] {
            want_bytes += usize::from(seg);
            if seg < 255 {
                comment_done = true;
                break;
            }
        }

        let len = stream.charge_tag_budget(want_bytes as u64)?;
        if (len as u64) > stream.remaining_file() {
            return Err(stream.malformed("declared tag length exceeds remaining file size"));
        }
        page_buf.resize(len, 0);
        stream
            .inner
            .read_exact(&mut page_buf)
            .map_err(|err| stream.malformed(&format!("read failed: {err}")))?;
        stream.pos = stream.pos.saturating_add(len as u64);

        let needed = comment_packet.len().saturating_add(len);
        if needed > comment_packet.capacity() {
            let grown = comment_packet
                .capacity()
                .max(64 * 1024)
                .saturating_mul(2)
                .min(MAX_AUDIO_TAG_BYTES)
                .max(needed);
            comment_packet.reserve_exact(grown - comment_packet.len());
        }
        comment_packet.extend_from_slice(&page_buf);
        stream.seek_to(page_end)?;
    }
    drop(page_buf);

    if !comment_done {
        return Err(stream.malformed("truncated Ogg comment header packet"));
    }

    let comment_body = match codec {
        OggCodec::Vorbis { .. } => comment_packet
            .strip_prefix(b"\x03vorbis")
            .ok_or_else(|| stream.malformed("missing Vorbis comment packet header"))?,
        OggCodec::Opus { .. } => comment_packet
            .strip_prefix(b"OpusTags")
            .ok_or_else(|| stream.malformed("missing OpusTags comment packet header"))?,
    };
    parse_vorbis_comments(comment_body, tags, cover, stream.display_path)?;
    drop(comment_packet);

    // 3. Read the last <= 64 KiB of the file to get the final granule position for `serial`.
    let tail_start = stream.file_len.saturating_sub(OGG_TAIL_SCAN_BYTES);
    stream.seek_to(tail_start)?;
    let tail = stream.read_probe_bytes(OGG_TAIL_SCAN_BYTES as usize)?;
    if let Some(last_granule) = find_last_ogg_granule(&tail, serial) {
        let secs = match codec {
            OggCodec::Vorbis { sample_rate } if sample_rate > 0 => {
                last_granule as f64 / f64::from(sample_rate)
            }
            OggCodec::Opus { pre_skip } => {
                last_granule.saturating_sub(u64::from(pre_skip)) as f64 / 48_000.0
            }
            _ => 0.0,
        };
        tags.duration_s = normalize_duration(secs);
    }

    Ok(())
}

fn find_last_ogg_granule(buf: &[u8], target_serial: u32) -> Option<u64> {
    let mut last = None;
    let mut i = 0usize;
    while i + 27 <= buf.len() {
        if &buf[i..i + 4] == b"OggS" && buf[i + 4] == 0 {
            let granule = u64::from_le_bytes([
                buf[i + 6],
                buf[i + 7],
                buf[i + 8],
                buf[i + 9],
                buf[i + 10],
                buf[i + 11],
                buf[i + 12],
                buf[i + 13],
            ]);
            let serial = u32::from_le_bytes([buf[i + 14], buf[i + 15], buf[i + 16], buf[i + 17]]);
            let nseg = usize::from(buf[i + 26]);
            if let Some(seg_table) = buf.get(i + 27..i + 27 + nseg) {
                let body_len: usize = seg_table.iter().map(|&s| usize::from(s)).sum();
                if let Some(page_end) = (i + 27 + nseg).checked_add(body_len)
                    && page_end <= buf.len()
                {
                    if serial == target_serial && granule != u64::MAX && granule > 0 {
                        last = Some(granule);
                    }
                    i = page_end;
                    continue;
                }
            }
        }
        i += 1;
    }
    last
}

// =================================================================================================
// MP4 / M4A / M4B (ISO BMFF)
// =================================================================================================

fn parse_mp4<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    stream.seek_to(0)?;
    let mut visited = 0usize;
    let mut saw_known_top = false;
    let mut pos = 0u64;
    let end = stream.file_len;

    while pos + 8 <= end {
        if visited >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many MP4 boxes",
            ));
        }
        visited += 1;
        let (box_type, payload_start, box_end) = read_mp4_box_header(stream, pos, end)?;
        if matches!(
            &box_type,
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"meta" | b"moof"
        ) {
            saw_known_top = true;
        }
        if &box_type == b"moov" {
            parse_mp4_moov(stream, payload_start, box_end, 1, &mut visited, tags, cover)?;
        }
        if box_end <= pos {
            return Err(stream.malformed("MP4 box did not advance"));
        }
        pos = box_end;
    }

    if !saw_known_top {
        return Err(stream.malformed("not an ISO BMFF / MP4 file"));
    }
    Ok(())
}

fn read_mp4_box_header<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    pos: u64,
    container_end: u64,
) -> Result<([u8; 4], u64, u64), ErrorBody> {
    if pos.checked_add(8).is_none_or(|e| e > container_end) {
        return Err(stream.malformed("truncated MP4 box header"));
    }
    stream.seek_to(pos)?;
    let hdr = stream.read_fixed::<8>()?;
    let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
    let box_type = [hdr[4], hdr[5], hdr[6], hdr[7]];
    let (header_len, total_size) = match size32 {
        0 => (8u64, container_end.saturating_sub(pos)),
        1 => {
            if pos.checked_add(16).is_none_or(|e| e > container_end) {
                return Err(stream.malformed("truncated MP4 extended box size"));
            }
            let ext = stream.read_fixed::<8>()?;
            let size64 = u64::from_be_bytes(ext);
            if size64 < 16 {
                return Err(stream.malformed("invalid MP4 extended box size"));
            }
            (16u64, size64)
        }
        2..=7 => return Err(stream.malformed("invalid MP4 box size")),
        n => (8u64, u64::from(n)),
    };
    let box_end = pos
        .checked_add(total_size)
        .ok_or_else(|| stream.malformed("MP4 box size overflow"))?;
    if box_end > container_end {
        return Err(stream.malformed("MP4 box length exceeds container bounds"));
    }
    let payload_start = pos
        .checked_add(header_len)
        .ok_or_else(|| stream.malformed("MP4 header offset overflow"))?;
    Ok((box_type, payload_start, box_end))
}

fn parse_mp4_moov<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    start: u64,
    end: u64,
    depth: usize,
    visited: &mut usize,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    if depth > MAX_MP4_DEPTH {
        return Err(stream.limit_exceeded(
            "mp4_nesting_too_deep",
            MAX_MP4_DEPTH,
            "MP4 box nesting too deep",
        ));
    }
    let mut pos = start;
    while pos + 8 <= end {
        if *visited >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many MP4 boxes",
            ));
        }
        *visited += 1;
        let (box_type, payload_start, box_end) = read_mp4_box_header(stream, pos, end)?;
        match &box_type {
            b"mvhd" => parse_mp4_mvhd(stream, payload_start, box_end, tags)?,
            b"udta" => {
                parse_mp4_moov(
                    stream,
                    payload_start,
                    box_end,
                    depth + 1,
                    visited,
                    tags,
                    cover,
                )?;
            }
            b"meta" => {
                // `meta` is normally a FullBox with 4 bytes of version/flags before child boxes.
                let child_start = mp4_meta_children_start(stream, payload_start, box_end)?;
                parse_mp4_moov(
                    stream,
                    child_start,
                    box_end,
                    depth + 1,
                    visited,
                    tags,
                    cover,
                )?;
            }
            b"ilst" => parse_mp4_ilst(stream, payload_start, box_end, visited, tags, cover)?,
            _ => {}
        }
        if box_end <= pos {
            return Err(stream.malformed("MP4 box did not advance"));
        }
        pos = box_end;
    }
    Ok(())
}

fn mp4_meta_children_start<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    payload_start: u64,
    box_end: u64,
) -> Result<u64, ErrorBody> {
    let payload_len = box_end.saturating_sub(payload_start);
    if payload_len < 8 {
        return Ok(box_end);
    }
    stream.seek_to(payload_start)?;
    let probe = stream.read_fixed::<8>()?;
    // If bytes 4..8 are already a recognized child box name (`hdlr`, `ilst`, `free`), this non-
    // standard encoder omitted the 4-byte FullBox version/flags prefix.
    if matches!(&probe[4..8], b"hdlr" | b"ilst" | b"free")
        && u32::from_be_bytes([probe[0], probe[1], probe[2], probe[3]]) >= 8
    {
        return Ok(payload_start);
    }
    Ok(payload_start.saturating_add(4))
}

fn parse_mp4_mvhd<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    payload_start: u64,
    box_end: u64,
    tags: &mut ParsedAudioTags,
) -> Result<(), ErrorBody> {
    let len = box_end.saturating_sub(payload_start);
    if len < 20 {
        return Ok(());
    }
    stream.seek_to(payload_start)?;
    let first = stream.read_fixed::<20>()?;
    let version = first[0];
    if version == 0 {
        let timescale = u32::from_be_bytes([first[12], first[13], first[14], first[15]]);
        let duration = u32::from_be_bytes([first[16], first[17], first[18], first[19]]);
        if timescale > 0 && duration > 0 && duration != u32::MAX {
            tags.duration_s = normalize_duration(f64::from(duration) / f64::from(timescale));
        }
    } else if version == 1 && len >= 32 {
        let rest = stream.read_fixed::<12>()?;
        let timescale = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let duration = u64::from_be_bytes([
            rest[4], rest[5], rest[6], rest[7], rest[8], rest[9], rest[10], rest[11],
        ]);
        if timescale > 0 && duration > 0 && duration != u64::MAX {
            tags.duration_s = normalize_duration(duration as f64 / f64::from(timescale));
        }
    }
    Ok(())
}

fn parse_mp4_ilst<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    start: u64,
    end: u64,
    visited: &mut usize,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    let mut pos = start;
    while pos + 8 <= end {
        if *visited >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many MP4 boxes",
            ));
        }
        *visited += 1;
        let (item_type, item_payload_start, item_end) = read_mp4_box_header(stream, pos, end)?;
        let is_known = matches!(
            &item_type,
            b"\xa9nam"
                | b"\xa9ART"
                | b"\xa9alb"
                | b"aART"
                | b"trkn"
                | b"disk"
                | b"\xa9day"
                | b"covr"
        );
        if is_known {
            parse_mp4_item_data_boxes(
                stream,
                item_type,
                item_payload_start,
                item_end,
                visited,
                tags,
                cover,
            )?;
        }
        if item_end <= pos {
            return Err(stream.malformed("MP4 ilst item did not advance"));
        }
        pos = item_end;
    }
    Ok(())
}

fn parse_mp4_item_data_boxes<R: Read + Seek>(
    stream: &mut BoundedStream<'_, R>,
    item_type: [u8; 4],
    start: u64,
    end: u64,
    visited: &mut usize,
    tags: &mut ParsedAudioTags,
    cover: &mut CoverSlot,
) -> Result<(), ErrorBody> {
    let mut pos = start;
    while pos + 8 <= end {
        if *visited >= MAX_CONTAINER_CHUNKS {
            return Err(stream.limit_exceeded(
                "too_many_container_chunks",
                MAX_CONTAINER_CHUNKS,
                "too many MP4 boxes",
            ));
        }
        *visited += 1;
        let (sub_type, sub_start, sub_end) = read_mp4_box_header(stream, pos, end)?;
        if &sub_type == b"data" && sub_end.saturating_sub(sub_start) >= 8 {
            stream.seek_to(sub_start)?;
            let data_hdr = stream.read_fixed::<8>()?;
            let type_indicator =
                u32::from_be_bytes([data_hdr[0], data_hdr[1], data_hdr[2], data_hdr[3]])
                    & 0x00FF_FFFF;
            let value_len = sub_end.saturating_sub(sub_start + 8);

            if &item_type == b"covr" {
                let _ = stream.charge_tag_budget(value_len)?;
                if value_len > 0 && cover.wants_picture(0) {
                    // MP4 `covr` carries type 13 (JPEG), 14 (PNG), 27 (BMP), or 0 (implicit).
                    if matches!(type_indicator, 0 | 13 | 14 | 27) {
                        match cover.mode {
                            CoverExtractMode::CheckOnly => {
                                let peek_len = usize::try_from(value_len)
                                    .unwrap_or(COVER_MAGIC_PEEK_BYTES)
                                    .min(COVER_MAGIC_PEEK_BYTES);
                                let prefix = stream.read_probe_bytes(peek_len)?;
                                cover.record_checked_cover(0, &prefix);
                            }
                            CoverExtractMode::ExtractBytes => {
                                let val_usize = usize::try_from(value_len)
                                    .map_err(|_| stream.tag_too_large("cover length overflows"))?;
                                let mut img = vec![0u8; val_usize];
                                stream.inner.read_exact(&mut img).map_err(|err| {
                                    stream.malformed(&format!("read failed: {err}"))
                                })?;
                                stream.pos = stream.pos.saturating_add(value_len);
                                cover.consider_owned(0, img);
                            }
                        }
                    }
                }
            } else {
                // Charge the full declared item value against the 16 MiB cumulative tag budget,
                // verify it fits within the file, then read ONLY the bounded text prefix so a
                // multi-megabyte MP4 text item is never materialized in memory.
                let _ = stream.charge_tag_budget(value_len)?;
                if value_len > stream.remaining_file() {
                    return Err(stream.malformed("MP4 item length exceeds remaining file size"));
                }
                let to_read = usize::try_from(value_len)
                    .unwrap_or(MAX_TEXT_DECODE_BYTES)
                    .min(MAX_TEXT_DECODE_BYTES);
                let mut payload = vec![0u8; to_read];
                stream
                    .inner
                    .read_exact(&mut payload)
                    .map_err(|err| stream.malformed(&format!("read failed: {err}")))?;
                stream.pos = stream.pos.saturating_add(to_read as u64);
                apply_mp4_item(item_type, type_indicator, &payload, tags);
            }
        }
        if sub_end <= pos {
            return Err(stream.malformed("MP4 data sub-box did not advance"));
        }
        pos = sub_end;
    }
    Ok(())
}

fn clean_mp4_text(type_indicator: u32, payload: &[u8]) -> Option<String> {
    match type_indicator {
        1 => clean_utf8_bytes(payload),
        2 => clean_utf16_bytes(payload, false),
        _ => None,
    }
}

fn apply_mp4_item(
    item_type: [u8; 4],
    type_indicator: u32,
    payload: &[u8],
    tags: &mut ParsedAudioTags,
) {
    match &item_type {
        b"\xa9nam" => {
            if tags.title.is_none() {
                tags.title = clean_mp4_text(type_indicator, payload);
            }
        }
        b"\xa9ART" => {
            if tags.artist.is_none() {
                tags.artist = clean_mp4_text(type_indicator, payload);
            }
        }
        b"\xa9alb" => {
            if tags.album.is_none() {
                tags.album = clean_mp4_text(type_indicator, payload);
            }
        }
        b"aART" => {
            if tags.album_artist.is_none() {
                tags.album_artist = clean_mp4_text(type_indicator, payload);
            }
        }
        b"\xa9day" => {
            if tags.year.is_none()
                && let Some(text) = clean_mp4_text(type_indicator, payload)
            {
                tags.year = parse_year_cleaned(&text);
            }
        }
        b"trkn" => {
            if tags.track.is_none() && payload.len() >= 4 {
                let n = u32::from(u16::from_be_bytes([payload[2], payload[3]]));
                if n > 0 {
                    tags.track = Some(n);
                }
            }
        }
        b"disk" if tags.disc.is_none() && payload.len() >= 4 => {
            let n = u32::from(u16::from_be_bytes([payload[2], payload[3]]));
            if n > 0 {
                tags.disc = Some(n);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn clean_tag_text_strips_controls_trims_caps_at_512_and_drops_empty() {
        assert_eq!(
            clean_tag_text("  \x00\x1bFirst\r\n song\t  "),
            Some("First song".to_owned())
        );
        assert_eq!(clean_tag_text("  \x00\x07\n\r\t "), None);
        assert_eq!(clean_tag_text(""), None);

        let long_unicode = "é".repeat(600);
        let cleaned = clean_tag_text(&long_unicode).unwrap();
        assert_eq!(cleaned.chars().count(), 512);

        // Cutting at 512 chars that lands on trailing spaces trims cleanly.
        let mut spaced = "a".repeat(510);
        spaced.push_str("    tail");
        assert_eq!(clean_tag_text(&spaced).unwrap(), "a".repeat(510));
    }

    #[test]
    fn clean_tag_text_strips_bidi_invisible_tags_nonchars_maps_separators_and_guards_clusters() {
        // Every bidirectional control (U+061C, U+200E, U+200F, U+202A..=U+202E, U+2066..=U+2069).
        let bidi = "A\u{061C}\u{200E}\u{200F}\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}\u{2066}\u{2067}\u{2068}\u{2069}B";
        assert_eq!(clean_tag_text(bidi).as_deref(), Some("AB"));

        // U+200B (ZWSP), U+FEFF (BOM), U+00AD (soft hyphen).
        assert_eq!(
            clean_tag_text("\u{FEFF}Hel\u{00AD}lo\u{200B}World\u{FEFF}").as_deref(),
            Some("HelloWorld")
        );

        // Unicode tag characters U+E0000..=U+E007F (smuggled prompt-injection plane).
        let tagged = "Song\u{E0000}\u{E0001}\u{E0041}\u{E007F}Title";
        assert_eq!(clean_tag_text(tagged).as_deref(), Some("SongTitle"));

        // Noncharacters: U+FDD0..=U+FDEF and plane-end xxFFFE/xxFFFF.
        let nonchars =
            "Track\u{FDD0}\u{FDEF}\u{FFFE}\u{FFFF}\u{1FFFE}\u{1FFFF}\u{10FFFE}\u{10FFFF}One";
        assert_eq!(clean_tag_text(nonchars).as_deref(), Some("TrackOne"));

        // U+2028 (Line Separator) and U+2029 (Paragraph Separator) become ASCII spaces (trimmed at edges).
        assert_eq!(
            clean_tag_text("\u{2028}First\u{2028}line\u{2029}second\u{2029}").as_deref(),
            Some("First line second")
        );
        assert_eq!(clean_tag_text("\u{2028}\u{2029}"), None);

        // U+200C (ZWNJ) and U+200D (ZWJ) are preserved inside text, never at edges.
        assert_eq!(
            clean_tag_text("\u{200D}می\u{200C}خواهم\u{200C}").as_deref(),
            Some("می\u{200C}خواهم")
        );
        assert_eq!(
            clean_tag_text(" \u{200C}\u{200D}👩\u{200D}🚀\u{200D}\u{200C} ").as_deref(),
            Some("👩\u{200D}🚀")
        );
        assert_eq!(clean_tag_text("\u{200C}\u{200D}"), None);

        // Non-truncated combining marks & variation selectors at end are preserved intact.
        assert_eq!(
            clean_tag_text("Caf\u{0065}\u{0301}").as_deref(),
            Some("Caf\u{0065}\u{0301}")
        );
        assert_eq!(
            clean_tag_text("Love ❤\u{FE0F}").as_deref(),
            Some("Love ❤\u{FE0F}")
        );

        // 512-char truncation must never end on a joiner, variation selector, or combining mark,
        // nor split a cluster across index 512:
        // 1) Combining mark straddling the 512 boundary: 511 'a' + 'e' + U+0301 -> 511 'a'
        let cut_comb_next = format!("{}e\u{0301}", "a".repeat(511));
        assert_eq!(clean_tag_text(&cut_comb_next).unwrap(), "a".repeat(511));
        // 2) Combining mark sitting on index 512 with more chars after -> step back past 'e'
        let cut_comb_end = format!("{}e\u{0301}z", "a".repeat(510));
        assert_eq!(clean_tag_text(&cut_comb_end).unwrap(), "a".repeat(510));
        // 3) Variation selector straddling 512 or landing on 512
        let cut_vs_next = format!("{}❤\u{FE0F}", "a".repeat(511));
        assert_eq!(clean_tag_text(&cut_vs_next).unwrap(), "a".repeat(511));
        let cut_vs_end = format!("{}❤\u{FE0F}z", "a".repeat(510));
        assert_eq!(clean_tag_text(&cut_vs_end).unwrap(), "a".repeat(510));
        // 4) ZWJ emoji sequence cut after ZWJ or before ZWJ -> step back over entire joined cluster
        let cut_zwj_end = format!("{}👩\u{200D}🚀", "a".repeat(510));
        assert_eq!(clean_tag_text(&cut_zwj_end).unwrap(), "a".repeat(510));
        let cut_zwj_chain = format!("{}👩\u{200D}👩\u{200D}👧", "a".repeat(509));
        assert_eq!(clean_tag_text(&cut_zwj_chain).unwrap(), "a".repeat(509));
        // 5) Regional indicator flag pair straddling 512 -> drop the lone leading regional indicator
        let cut_flag_split = format!("{}\u{1F1E9}\u{1F1EA}", "a".repeat(511));
        assert_eq!(clean_tag_text(&cut_flag_split).unwrap(), "a".repeat(511));
        // Complete flag pair fitting before 512 stays intact when later chars are truncated
        let cut_flag_complete = format!("{}\u{1F1E9}\u{1F1EA}z", "a".repeat(510));
        assert_eq!(
            clean_tag_text(&cut_flag_complete).unwrap(),
            format!("{}\u{1F1E9}\u{1F1EA}", "a".repeat(510))
        );
        // Second flag straddling 512 keeps the first flag pair intact and drops the lone third RI
        let cut_two_flags = format!("{}\u{1F1E9}\u{1F1EA}\u{1F1FA}\u{1F1F8}", "a".repeat(509));
        assert_eq!(
            clean_tag_text(&cut_two_flags).unwrap(),
            format!("{}\u{1F1E9}\u{1F1EA}", "a".repeat(509))
        );
        // Non-truncated flag remains intact
        assert_eq!(
            clean_tag_text("Berlin \u{1F1E9}\u{1F1EA}").as_deref(),
            Some("Berlin \u{1F1E9}\u{1F1EA}")
        );
    }

    #[test]
    fn id3v24_nul_separated_values_join_with_comma_and_per_value_bom() {
        // UTF-8 multi-value with empty/trailing NULs -> joined with ", "
        let utf8_payload = b"\x03First artist\x00\x00Second artist\x00Third artist\x00";
        assert_eq!(
            clean_id3_text_frame(4, utf8_payload).as_deref(),
            Some("First artist, Second artist, Third artist")
        );

        // Latin-1 multi-value
        let latin1_payload = b"\x00Alpha\x00Beta";
        assert_eq!(
            clean_id3_text_frame(4, latin1_payload).as_deref(),
            Some("Alpha, Beta")
        );

        // UTF-16 with BOM: first segment UTF-16LE (FF FE), second segment UTF-16BE (FE FF)
        let mut utf16_payload = vec![1u8]; // encoding 1 = UTF-16 with BOM
        utf16_payload.extend_from_slice(&[0xFF, 0xFE]); // LE BOM
        for u in "Ada".encode_utf16() {
            utf16_payload.extend_from_slice(&u.to_le_bytes());
        }
        utf16_payload.extend_from_slice(&[0x00, 0x00]); // NUL separator
        utf16_payload.extend_from_slice(&[0xFE, 0xFF]); // BE BOM
        for u in "Grace".encode_utf16() {
            utf16_payload.extend_from_slice(&u.to_be_bytes());
        }
        utf16_payload.extend_from_slice(&[0x00, 0x00]); // trailing NUL
        assert_eq!(
            clean_id3_text_frame(4, &utf16_payload).as_deref(),
            Some("Ada, Grace")
        );

        // Full ID3v2.4 MP3 parse verifying TPE1 multi-value UTF-16 mixed BOM & TRCK first value.
        let mut frames = Vec::new();
        let push_v24 = |out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8]| {
            let sz = payload.len() as u32;
            out.extend_from_slice(id);
            out.extend_from_slice(&[
                ((sz >> 21) & 0x7F) as u8,
                ((sz >> 14) & 0x7F) as u8,
                ((sz >> 7) & 0x7F) as u8,
                (sz & 0x7F) as u8,
            ]);
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(payload);
        };
        push_v24(&mut frames, b"TIT2", b"\x03Part One\x00Part Two");
        push_v24(&mut frames, b"TPE1", &utf16_payload);
        push_v24(&mut frames, b"TRCK", b"\x033/10\x004/10");
        let sz = frames.len() as u32;
        let mut mp3 = vec![
            b'I',
            b'D',
            b'3',
            4,
            0,
            0,
            ((sz >> 21) & 0x7F) as u8,
            ((sz >> 14) & 0x7F) as u8,
            ((sz >> 7) & 0x7F) as u8,
            (sz & 0x7F) as u8,
        ];
        mp3.extend_from_slice(&frames);
        let mut cur = Cursor::new(mp3.as_slice());
        let extracted = parse_audio(
            &mut cur,
            mp3.len() as u64,
            AudioFormat::Mp3,
            CoverExtractMode::CheckOnly,
            Path::new("/v24_multi.mp3"),
        )
        .unwrap();
        assert_eq!(extracted.tags.title.as_deref(), Some("Part One, Part Two"));
        assert_eq!(extracted.tags.artist.as_deref(), Some("Ada, Grace"));
        assert_eq!(extracted.tags.track, Some(3));
    }

    #[test]
    fn wav_and_aiff_clamp_oversized_trailing_audio_chunks_for_in_progress_recordings() {
        // 1. In-progress WAV: RIFF size=0xFFFFFFFF, LIST/INFO title + fmt + data size=0xFFFFFFFF
        //    with 2 seconds of 8 kHz 16-bit mono PCM (32,000 bytes) written so far.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        // LIST/INFO chunk before `data`
        let inam = b"Live WAV take\0";
        let list_payload_len = 4 + 8 + inam.len(); // "INFO" + "INAM" + len + value (14 is even)
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&(list_payload_len as u32).to_le_bytes());
        wav.extend_from_slice(b"INFO");
        wav.extend_from_slice(b"INAM");
        wav.extend_from_slice(&(inam.len() as u32).to_le_bytes());
        wav.extend_from_slice(inam);
        // `fmt ` chunk: PCM, 1ch, 8000 Hz, byte_rate=16000, block_align=2, 16-bit
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&8000u32.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        // `data` chunk declaring 0xFFFFFFFF with 32,000 bytes actually present
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        wav.resize(wav.len() + 32_000, 0);

        let mut cur_w = Cursor::new(wav.as_slice());
        let res_w = parse_audio(
            &mut cur_w,
            wav.len() as u64,
            AudioFormat::Wav,
            CoverExtractMode::CheckOnly,
            Path::new("/recording.wav"),
        )
        .unwrap();
        assert_eq!(res_w.tags.title.as_deref(), Some("Live WAV take"));
        assert!((res_w.tags.duration_s.unwrap() - 2.0).abs() < 0.01);

        // 2. In-progress AIFF: FORM size=0xFFFFFFFF, ID3 title + COMM (num_frames=0xFFFFFFFF,
        //    1ch 16-bit 44100 Hz) + SSND size=0xFFFFFFFF with 8-byte header + 1 s of PCM (88,200 B).
        let mut aiff = Vec::new();
        aiff.extend_from_slice(b"FORM");
        aiff.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        aiff.extend_from_slice(b"AIFF");
        // ID3 chunk before SSND
        let id3_frame = b"TIT2\x00\x00\x00\x0f\x00\x00\x03Live AIFF take";
        let mut id3 = vec![b'I', b'D', b'3', 4, 0, 0, 0, 0, 0, id3_frame.len() as u8];
        id3.extend_from_slice(id3_frame);
        aiff.extend_from_slice(b"ID3 ");
        aiff.extend_from_slice(&(id3.len() as u32).to_be_bytes());
        aiff.extend_from_slice(&id3);
        if !id3.len().is_multiple_of(2) {
            aiff.push(0);
        }
        // COMM chunk: 1ch, num_frames=0xFFFFFFFF, 16-bit, 44100 Hz
        aiff.extend_from_slice(b"COMM");
        aiff.extend_from_slice(&18u32.to_be_bytes());
        aiff.extend_from_slice(&1u16.to_be_bytes());
        aiff.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        aiff.extend_from_slice(&16u16.to_be_bytes());
        aiff.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        // SSND chunk declaring 0xFFFFFFFF with 8-byte header + 88,200 bytes (1.0 s)
        aiff.extend_from_slice(b"SSND");
        aiff.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        aiff.resize(aiff.len() + 8 + 88_200, 0);

        let mut cur_a = Cursor::new(aiff.as_slice());
        let res_a = parse_audio(
            &mut cur_a,
            aiff.len() as u64,
            AudioFormat::Aiff,
            CoverExtractMode::CheckOnly,
            Path::new("/recording.aiff"),
        )
        .unwrap();
        assert_eq!(res_a.tags.title.as_deref(), Some("Live AIFF take"));
        assert!((res_a.tags.duration_s.unwrap() - 1.0).abs() < 0.01);
    }

    #[test]
    fn parses_track_disc_and_year_cleanly() {
        assert_eq!(parse_track_or_disc("3/12"), Some(3));
        assert_eq!(parse_track_or_disc("07"), Some(7));
        assert_eq!(parse_track_or_disc("0/10"), None);
        assert_eq!(parse_track_or_disc("not a number"), None);

        assert_eq!(parse_year("2024"), Some(2024));
        assert_eq!(parse_year("2024-05-12T08:00:00Z"), Some(2024));
        assert_eq!(parse_year("0000"), None);
        assert_eq!(parse_year("99"), None);
    }

    #[test]
    fn decodes_ieee80_sample_rates_accurately() {
        // 44100.0 Hz: exp = 16383 + 15 = 0x400E, mantissa = 0xAC44 << 48
        let r441 = decode_ieee80_be([0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
        assert!((r441 - 44_100.0).abs() < 1e-6);
        // 48000.0 Hz: mantissa = 0xBB80 << 48
        let r48 = decode_ieee80_be([0x40, 0x0E, 0xBB, 0x80, 0, 0, 0, 0, 0, 0]);
        assert!((r48 - 48_000.0).abs() < 1e-6);
    }

    #[test]
    fn deunsync_handles_ff_00_sequences() {
        let mut buf = vec![0xFF, 0x00, 0xD8, 0xFF, 0x00, 0x00, 0x01, 0xFF];
        deunsync_in_place(&mut buf);
        assert_eq!(buf, vec![0xFF, 0xD8, 0xFF, 0x00, 0x01, 0xFF]);
    }

    #[test]
    fn streaming_base64_reader_skips_whitespace_and_validates_padding() {
        // "Hello, world!" -> "SGVsbG8sIHdvcmxkIQ==" wrapped with CRLF and spaces
        let wrapped = b"  SGVs\r\nbG8s \nIHdv\tcmxkIQ==  \n";
        let mut r = Base64StreamReader::new(wrapped);
        assert_eq!(r.remaining_decoded_len(), Ok(13));
        let mut out = [0u8; 13];
        r.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"Hello, world!");
        assert_eq!(r.remaining_decoded_len(), Ok(0));
        assert!(r.read_exact(&mut [0u8; 1]).is_err());

        // Single padding `=` ("Hello" -> "SGVsbG8=")
        let mut r1 = Base64StreamReader::new(b"SGVs\nbG8=");
        assert_eq!(r1.remaining_decoded_len(), Ok(5));
        let mut out1 = [0u8; 5];
        r1.read_exact(&mut out1).unwrap();
        assert_eq!(&out1, b"Hello");

        // Rejects invalid base64 character and non-canonical non-zero pad bits.
        let mut bad = Base64StreamReader::new(b"SGVs!G8=");
        let mut tmp = [0u8; 5];
        assert!(bad.read_exact(&mut tmp).is_err());
        let mut bad_pad = Base64StreamReader::new(b"SGVsbG9=");
        assert!(bad_pad.read_exact(&mut tmp).is_err());
    }

    fn assert_valid_audio_outcome(res: &Result<ExtractedAudio, ErrorBody>, mode: CoverExtractMode) {
        match res {
            Ok(extracted) => {
                for text in [
                    &extracted.tags.title,
                    &extracted.tags.artist,
                    &extracted.tags.album,
                    &extracted.tags.album_artist,
                ]
                .into_iter()
                .flatten()
                {
                    assert!(!text.is_empty(), "parsed text field must not be empty");
                    assert!(
                        text.chars().count() <= MAX_TAG_FIELD_CHARS,
                        "parsed text exceeded {MAX_TAG_FIELD_CHARS} chars"
                    );
                    let first = text.chars().next().unwrap();
                    let last = text.chars().next_back().unwrap();
                    assert!(!is_edge_trimmed(first), "leading edge char {first:?}");
                    assert!(!is_edge_trimmed(last), "trailing edge char {last:?}");
                    for ch in text.chars() {
                        assert!(
                            !is_stripped_tag_char(ch) && ch != '\u{2028}' && ch != '\u{2029}',
                            "disallowed char U+{:04X} survived cleaning",
                            ch as u32
                        );
                    }
                }
                if let Some(n) = extracted.tags.track {
                    assert!(n > 0);
                }
                if let Some(n) = extracted.tags.disc {
                    assert!(n > 0);
                }
                if let Some(y) = extracted.tags.year {
                    assert!((1..=9999).contains(&y));
                }
                if let Some(d) = extracted.tags.duration_s {
                    assert!(d.is_finite() && d > 0.0);
                }
                match mode {
                    CoverExtractMode::CheckOnly => {
                        assert!(extracted.cover.is_none());
                    }
                    CoverExtractMode::ExtractBytes => {
                        if let Some(ref bytes) = extracted.cover {
                            assert!(extracted.tags.has_embedded_cover);
                            assert!(bytes.len() <= MAX_AUDIO_TAG_BYTES);
                            assert!(is_supported_cover_image(bytes));
                        }
                    }
                }
            }
            Err(err) => {
                assert_eq!(err.code, ErrorCode::Failed, "unexpected error: {err:?}");
                assert!(!err.message.is_empty());
            }
        }
    }

    #[test]
    fn property_truncated_and_mutated_inputs_never_panic_or_exceed_bounds() {
        // Build seeds across every supported format, every iteration cap, and the 16 MiB tag
        // budget boundary, then run > 3,500 truncations and mutations asserting valid Ok or clean
        // structured Err within a strict time bound.
        let t0 = std::time::Instant::now();
        let tiny_png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let small_seeds: Vec<(AudioFormat, Vec<u8>)> = vec![
            // MP3 ID3v2.4 + frame + ID3v1
            (AudioFormat::Mp3, {
                let mut f =
                    b"ID3\x04\x00\x00\x00\x00\x00\x16TIT2\x00\x00\x00\x0c\x00\x00\x03First song!"
                        .to_vec();
                f.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
                f.resize(f.len() + 413, 0);
                f.extend_from_slice(b"TAG");
                f.resize(f.len() + 125, b' ');
                f
            }),
            // WAV with fmt + data + ID3
            (AudioFormat::Wav, {
                let mut f = b"RIFF\x38\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x44\xAC\x00\x00\x88\x58\x01\x00\x02\x00\x10\x00data\x04\x00\x00\x00\x00\x00\x00\x00".to_vec();
                f.extend_from_slice(b"ID3 \x0a\x00\x00\x00ID3\x03\x00\x00\x00\x00\x00\x00");
                f
            }),
            // AIFF with COMM + ID3
            (AudioFormat::Aiff, {
                let mut f = b"FORM\x00\x00\x00\x30AIFFCOMM\x00\x00\x00\x12\x00\x01\x00\x00\xAC\x44\x00\x10\x40\x0E\xAC\x44\x00\x00\x00\x00\x00\x00".to_vec();
                f.extend_from_slice(b"ID3 \x00\x00\x00\x0aID3\x03\x00\x00\x00\x00\x00\x00");
                f
            }),
            // FLAC with STREAMINFO + VORBIS_COMMENT + PICTURE
            (AudioFormat::Flac, {
                let mut f = b"fLaC\x00\x00\x00\x22".to_vec();
                let mut si = [0u8; 34];
                si[10] = 0x0A;
                si[11] = 0xC4;
                si[12] = 0x40;
                si[17] = 100;
                f.extend_from_slice(&si);
                let vc = b"\x04\x00\x00\x00test\x01\x00\x00\x00\x10\x00\x00\x00TITLE=First song";
                f.extend_from_slice(&[0x04, 0, 0, vc.len() as u8]);
                f.extend_from_slice(vc);
                let mut pic = Vec::new();
                pic.extend_from_slice(&3u32.to_be_bytes());
                pic.extend_from_slice(&9u32.to_be_bytes());
                pic.extend_from_slice(b"image/png");
                pic.extend_from_slice(&0u32.to_be_bytes());
                pic.extend_from_slice(&[0u8; 16]);
                pic.extend_from_slice(&(tiny_png.len() as u32).to_be_bytes());
                pic.extend_from_slice(tiny_png);
                f.extend_from_slice(&[0x86, 0, 0, pic.len() as u8]);
                f.extend_from_slice(&pic);
                f
            }),
            // Ogg Opus
            (AudioFormat::Ogg, {
                let mut f = Vec::new();
                let mut p0 = b"OpusHead\x01\x02\x38\x01\x80\xBB\x00\x00\x00\x00\x00".to_vec();
                f.extend_from_slice(b"OggS\x00\x02");
                f.extend_from_slice(&0u64.to_le_bytes());
                f.extend_from_slice(&1u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.push(1);
                f.push(p0.len() as u8);
                f.append(&mut p0);
                let p1 =
                    b"OpusTags\x04\x00\x00\x00test\x01\x00\x00\x00\x10\x00\x00\x00TITLE=First song";
                f.extend_from_slice(b"OggS\x00\x00");
                f.extend_from_slice(&48000u64.to_le_bytes());
                f.extend_from_slice(&1u32.to_le_bytes());
                f.extend_from_slice(&1u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.push(1);
                f.push(p1.len() as u8);
                f.extend_from_slice(p1);
                f
            }),
            // MP4 with ftyp + moov/mvhd
            (AudioFormat::Mp4, {
                let mut f = b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00M4A ".to_vec();
                f.extend_from_slice(&36u32.to_be_bytes());
                f.extend_from_slice(b"moov");
                f.extend_from_slice(&28u32.to_be_bytes());
                f.extend_from_slice(b"mvhd");
                f.extend_from_slice(&[0u8; 12]);
                f.extend_from_slice(&1000u32.to_be_bytes());
                f.extend_from_slice(&2500u32.to_be_bytes());
                f
            }),
        ];

        // Iteration-cap hostile seeds covering every loop bound in this module.
        let cap_seeds: Vec<(AudioFormat, Vec<u8>)> = vec![
            // > MAX_ID3_FRAMES (4096)
            (AudioFormat::Mp3, {
                let count = MAX_ID3_FRAMES + 64;
                let body_len = (count * 12) as u32;
                let mut f = Vec::with_capacity(10 + body_len as usize);
                f.extend_from_slice(b"ID3\x03\x00\x00");
                f.extend_from_slice(&[
                    ((body_len >> 21) & 0x7F) as u8,
                    ((body_len >> 14) & 0x7F) as u8,
                    ((body_len >> 7) & 0x7F) as u8,
                    (body_len & 0x7F) as u8,
                ]);
                for _ in 0..count {
                    f.extend_from_slice(b"TXXX\x00\x00\x00\x02\x00\x00\x03\x00");
                }
                f
            }),
            // > MAX_CONTAINER_CHUNKS (4096) in WAV
            (AudioFormat::Wav, {
                let count = MAX_CONTAINER_CHUNKS + 64;
                let body_len = (4 + count * 8) as u32;
                let mut f = Vec::with_capacity(8 + body_len as usize);
                f.extend_from_slice(b"RIFF");
                f.extend_from_slice(&body_len.to_le_bytes());
                f.extend_from_slice(b"WAVE");
                for _ in 0..count {
                    f.extend_from_slice(b"JUNK\x00\x00\x00\x00");
                }
                f
            }),
            // > MAX_CONTAINER_CHUNKS (4096) in AIFF
            (AudioFormat::Aiff, {
                let count = MAX_CONTAINER_CHUNKS + 64;
                let body_len = (4 + count * 8) as u32;
                let mut f = Vec::with_capacity(8 + body_len as usize);
                f.extend_from_slice(b"FORM");
                f.extend_from_slice(&body_len.to_be_bytes());
                f.extend_from_slice(b"AIFF");
                for _ in 0..count {
                    f.extend_from_slice(b"JUNK\x00\x00\x00\x00");
                }
                f
            }),
            // > MAX_CONTAINER_CHUNKS (4096) in FLAC
            (AudioFormat::Flac, {
                let count = MAX_CONTAINER_CHUNKS + 64;
                let mut f = Vec::with_capacity(4 + 38 + count * 4);
                f.extend_from_slice(b"fLaC\x00\x00\x00\x22");
                f.extend_from_slice(&[0u8; 34]);
                for i in 0..count {
                    let hdr = if i + 1 == count { 0x81 } else { 0x01 };
                    f.extend_from_slice(&[hdr, 0, 0, 0]);
                }
                f
            }),
            // > MAX_VORBIS_COMMENTS (65536) in FLAC
            (AudioFormat::Flac, {
                let count = (MAX_VORBIS_COMMENTS as usize) + 128;
                let mut vc = Vec::with_capacity(12 + count * 7);
                vc.extend_from_slice(&4u32.to_le_bytes());
                vc.extend_from_slice(b"test");
                vc.extend_from_slice(&(count as u32).to_le_bytes());
                for _ in 0..count {
                    vc.extend_from_slice(&3u32.to_le_bytes());
                    vc.extend_from_slice(b"A=1");
                }
                let mut f = Vec::with_capacity(4 + 38 + 4 + vc.len());
                f.extend_from_slice(b"fLaC\x00\x00\x00\x22");
                f.extend_from_slice(&[0u8; 34]);
                let len = vc.len() as u32;
                f.extend_from_slice(&[
                    0x84,
                    ((len >> 16) & 0xFF) as u8,
                    ((len >> 8) & 0xFF) as u8,
                    (len & 0xFF) as u8,
                ]);
                f.extend_from_slice(&vc);
                f
            }),
            // > MAX_OGG_SCAN_PAGES (64)
            (AudioFormat::Ogg, {
                let count = MAX_OGG_SCAN_PAGES + 16;
                let mut f = Vec::with_capacity(count * 36);
                for i in 0..count {
                    f.extend_from_slice(b"OggS\x00\x02");
                    f.extend_from_slice(&0u64.to_le_bytes());
                    f.extend_from_slice(&(100 + i as u32).to_le_bytes());
                    f.extend_from_slice(&0u32.to_le_bytes());
                    f.extend_from_slice(&0u32.to_le_bytes());
                    f.extend_from_slice(&[1, 8]);
                    f.extend_from_slice(b"NOTAUDIO");
                }
                f
            }),
            // > MAX_OGG_COMMENT_PAGES (2048)
            (AudioFormat::Ogg, {
                let count = MAX_OGG_COMMENT_PAGES + 32;
                let mut f = Vec::with_capacity(64 + count * (28 + 255));
                let p0 = b"OpusHead\x01\x02\x38\x01\x80\xBB\x00\x00\x00\x00\x00";
                f.extend_from_slice(b"OggS\x00\x02");
                f.extend_from_slice(&0u64.to_le_bytes());
                f.extend_from_slice(&1u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.push(1);
                f.push(p0.len() as u8);
                f.extend_from_slice(p0);
                let mut chunk = [b'B'; 255];
                for page_idx in 0..count {
                    if page_idx == 0 {
                        chunk[..8].copy_from_slice(b"OpusTags");
                    } else if page_idx == 1 {
                        chunk[..8].copy_from_slice(b"BBBBBBBB");
                    }
                    f.extend_from_slice(b"OggS\x00");
                    f.push(if page_idx == 0 { 0x00 } else { 0x01 });
                    f.extend_from_slice(&u64::MAX.to_le_bytes());
                    f.extend_from_slice(&1u32.to_le_bytes());
                    f.extend_from_slice(&((page_idx + 1) as u32).to_le_bytes());
                    f.extend_from_slice(&0u32.to_le_bytes());
                    f.extend_from_slice(&[1, 255]);
                    f.extend_from_slice(&chunk);
                }
                f
            }),
            // > MAX_MP4_DEPTH (8) nested udta boxes
            (AudioFormat::Mp4, {
                let mut inner = b"\x00\x00\x00\x08free".to_vec();
                for _ in 0..(MAX_MP4_DEPTH + 6) {
                    let mut wrapped = Vec::with_capacity(8 + inner.len());
                    wrapped.extend_from_slice(&((8 + inner.len()) as u32).to_be_bytes());
                    wrapped.extend_from_slice(b"udta");
                    wrapped.extend_from_slice(&inner);
                    inner = wrapped;
                }
                let mut f = b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00M4A ".to_vec();
                f.extend_from_slice(&((8 + inner.len()) as u32).to_be_bytes());
                f.extend_from_slice(b"moov");
                f.extend_from_slice(&inner);
                f
            }),
            // > MAX_CONTAINER_CHUNKS (4096) in MP4
            (AudioFormat::Mp4, {
                let count = MAX_CONTAINER_CHUNKS + 64;
                let mut f = Vec::with_capacity(20 + count * 8);
                f.extend_from_slice(b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00M4A ");
                for _ in 0..count {
                    f.extend_from_slice(b"\x00\x00\x00\x08free");
                }
                f
            }),
        ];

        // Deterministic xorshift64 PRNG so mutations are reproducible across runs.
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next_u64 = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let label = Path::new("/fuzz.bin");
        let mut cases = 0usize;

        for (fmt, seed) in &small_seeds {
            // 1. Every prefix truncation 0..=seed.len()
            for cut in 0..=seed.len() {
                let slice = &seed[..cut];
                for mode in [CoverExtractMode::CheckOnly, CoverExtractMode::ExtractBytes] {
                    let mut cur = Cursor::new(slice);
                    let res = parse_audio(&mut cur, slice.len() as u64, *fmt, mode, label);
                    assert_valid_audio_outcome(&res, mode);
                    cases += 1;
                }
            }
            // 2. 400 random byte/length mutations per format (including 0xFF/0x7F oversized lengths)
            for _ in 0..400 {
                let mut buf = seed.clone();
                let edits = 1 + (next_u64() as usize % 6);
                for _ in 0..edits {
                    if buf.is_empty() {
                        break;
                    }
                    let idx = next_u64() as usize % buf.len();
                    match next_u64() % 5 {
                        0 => buf[idx] = 0xFF,
                        1 => buf[idx] = 0x7F,
                        2 => buf[idx] = 0x00,
                        3 => buf[idx] ^= 1u8 << (next_u64() % 8),
                        _ => {
                            let end = (idx + 4).min(buf.len());
                            for b in &mut buf[idx..end] {
                                *b = (next_u64() & 0xFF) as u8;
                            }
                        }
                    }
                }
                for mode in [CoverExtractMode::CheckOnly, CoverExtractMode::ExtractBytes] {
                    let mut cur = Cursor::new(buf.as_slice());
                    let res = parse_audio(&mut cur, buf.len() as u64, *fmt, mode, label);
                    assert_valid_audio_outcome(&res, mode);
                    cases += 1;
                }
            }
        }

        // 3. Exercise every iteration-cap seed intact, truncated, and mutated.
        for (fmt, seed) in &cap_seeds {
            for mode in [CoverExtractMode::CheckOnly, CoverExtractMode::ExtractBytes] {
                let mut cur = Cursor::new(seed.as_slice());
                let res = parse_audio(&mut cur, seed.len() as u64, *fmt, mode, label);
                assert_valid_audio_outcome(&res, mode);
                cases += 1;
            }
            for step in 0..24usize {
                let mut buf = seed.clone();
                if step % 3 == 0 {
                    let cut = (next_u64() as usize) % buf.len().max(1);
                    buf.truncate(cut);
                } else if !buf.is_empty() {
                    let window = buf.len().clamp(1, 256);
                    for _ in 0..4 {
                        let off = (next_u64() as usize) % window;
                        buf[off] ^= (next_u64() & 0xFF) as u8;
                    }
                }
                let mut cur = Cursor::new(buf.as_slice());
                let res = parse_audio(
                    &mut cur,
                    buf.len() as u64,
                    *fmt,
                    CoverExtractMode::CheckOnly,
                    label,
                );
                assert_valid_audio_outcome(&res, CoverExtractMode::CheckOnly);
                cases += 1;
            }
        }

        // 4. Exercise the 16 MiB cumulative tag budget (just under and just over across MP3
        //    global-unsync, FLAC cumulative PICTURE blocks, and MP4 `covr`) plus header mutations.
        {
            // 4a. MP3 global-unsynced tag at 15.8 MiB (Ok) and 16 MiB + 128 B (tag_too_large).
            let near_len = MAX_AUDIO_TAG_BYTES - 256 * 1024;
            let mut mp3_near = Vec::with_capacity(10 + near_len);
            mp3_near.extend_from_slice(b"ID3\x03\x00\x80");
            let sz = near_len as u32;
            mp3_near.extend_from_slice(&[
                ((sz >> 21) & 0x7F) as u8,
                ((sz >> 14) & 0x7F) as u8,
                ((sz >> 7) & 0x7F) as u8,
                (sz & 0x7F) as u8,
            ]);
            mp3_near.extend_from_slice(b"TIT2\x00\x00\x00\x0a\x00\x00\x03Near 16M!");
            mp3_near.extend_from_slice(b"PRIV");
            mp3_near.extend_from_slice(&((near_len - 30) as u32).to_be_bytes());
            mp3_near.extend_from_slice(&[0, 0]);
            mp3_near.resize(10 + near_len, b'A');
            let mut cur = Cursor::new(mp3_near.as_slice());
            let ok_near = parse_audio(
                &mut cur,
                mp3_near.len() as u64,
                AudioFormat::Mp3,
                CoverExtractMode::CheckOnly,
                label,
            );
            assert!(
                ok_near.is_ok(),
                "15.75 MiB unsync MP3 should succeed: {ok_near:?}"
            );
            assert_valid_audio_outcome(&ok_near, CoverExtractMode::CheckOnly);
            cases += 1;

            let over_len = MAX_AUDIO_TAG_BYTES + 128;
            let sz_over = over_len as u32;
            mp3_near[6..10].copy_from_slice(&[
                ((sz_over >> 21) & 0x7F) as u8,
                ((sz_over >> 14) & 0x7F) as u8,
                ((sz_over >> 7) & 0x7F) as u8,
                (sz_over & 0x7F) as u8,
            ]);
            mp3_near.resize(10 + over_len, b'A');
            let mut cur_over = Cursor::new(mp3_near.as_slice());
            let err_over = parse_audio(
                &mut cur_over,
                mp3_near.len() as u64,
                AudioFormat::Mp3,
                CoverExtractMode::CheckOnly,
                label,
            )
            .unwrap_err();
            assert_eq!(
                err_over.details.as_ref().and_then(|d| d.get("reason")),
                Some(&json!("tag_too_large"))
            );
            cases += 1;

            // In-place header mutations on the 16 MiB MP3 buffer (zero extra 16 MiB clones).
            let orig_hdr: [u8; 64] = mp3_near[..64].try_into().unwrap();
            for _ in 0..32 {
                mp3_near[..64].copy_from_slice(&orig_hdr);
                for _ in 0..4 {
                    let off = (next_u64() as usize) % 64;
                    mp3_near[off] = match next_u64() % 3 {
                        0 => 0xFF,
                        1 => 0x7F,
                        _ => (next_u64() & 0xFF) as u8,
                    };
                }
                let mut cur_m = Cursor::new(mp3_near.as_slice());
                let res_m = parse_audio(
                    &mut cur_m,
                    mp3_near.len() as u64,
                    AudioFormat::Mp3,
                    CoverExtractMode::CheckOnly,
                    label,
                );
                assert_valid_audio_outcome(&res_m, CoverExtractMode::CheckOnly);
                cases += 1;
            }
            drop(mp3_near);

            // 4b. FLAC cumulative 4 x 4.1 MiB PICTURE blocks (> 16 MiB budget -> tag_too_large).
            let each_pic = 4_200_000usize;
            let mut pic_body = Vec::with_capacity(32 + 9 + each_pic);
            pic_body.extend_from_slice(&3u32.to_be_bytes());
            pic_body.extend_from_slice(&9u32.to_be_bytes());
            pic_body.extend_from_slice(b"image/png");
            pic_body.extend_from_slice(&0u32.to_be_bytes());
            pic_body.extend_from_slice(&[0u8; 16]);
            pic_body.extend_from_slice(&(each_pic as u32).to_be_bytes());
            pic_body.extend_from_slice(tiny_png);
            pic_body.resize(32 + 9 + each_pic, 0);
            let mut flac_over = Vec::with_capacity(4 + 38 + 4 * (4 + pic_body.len()));
            flac_over.extend_from_slice(b"fLaC\x00\x00\x00\x22");
            flac_over.extend_from_slice(&[0u8; 34]);
            let plen = pic_body.len() as u32;
            for i in 0..4 {
                flac_over.extend_from_slice(&[
                    if i == 3 { 0x86 } else { 0x06 },
                    ((plen >> 16) & 0xFF) as u8,
                    ((plen >> 8) & 0xFF) as u8,
                    (plen & 0xFF) as u8,
                ]);
                flac_over.extend_from_slice(&pic_body);
            }
            let mut cur_f = Cursor::new(flac_over.as_slice());
            let err_f = parse_audio(
                &mut cur_f,
                flac_over.len() as u64,
                AudioFormat::Flac,
                CoverExtractMode::CheckOnly,
                label,
            )
            .unwrap_err();
            assert_eq!(
                err_f.details.as_ref().and_then(|d| d.get("reason")),
                Some(&json!("tag_too_large"))
            );
            cases += 1;
        }

        let elapsed = t0.elapsed();
        assert!(
            cases >= 3_500,
            "expected >= 3,500 property cases, ran {cases}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "audio parser property suite exceeded 15 s time bound: {elapsed:?} ({cases} cases)"
        );
    }

    #[test]
    fn iteration_and_size_caps_accept_just_under_and_refuse_just_over_with_specific_reasons() {
        let label = Path::new("/cap_boundary.bin");
        let parse = |fmt: AudioFormat, data: &[u8]| {
            let mut cur = Cursor::new(data);
            parse_audio(
                &mut cur,
                data.len() as u64,
                fmt,
                CoverExtractMode::CheckOnly,
                label,
            )
        };
        let assert_limit_err = |res: Result<ExtractedAudio, ErrorBody>,
                                expected_reason: &str,
                                expected_key: &str,
                                expected_val: usize| {
            let err = res.expect_err(&format!(
                "expected refusal with reason {expected_reason}, got Ok"
            ));
            assert_eq!(err.code, ErrorCode::Failed, "err: {err:?}");
            let details = err.details.as_ref().expect("limit error must set details");
            assert_eq!(
                details.get("reason"),
                Some(&json!(expected_reason)),
                "wrong reason: {err:?}"
            );
            assert_eq!(
                details.get(expected_key),
                Some(&json!(expected_val)),
                "wrong {expected_key}: {err:?}"
            );
        };

        // 1. MAX_ID3_FRAMES (4096): TIT2 + (n - 2) TXXX + TPE1
        let make_mp3_frames = |total_frames: usize| -> Vec<u8> {
            let mut body = Vec::with_capacity(64 + total_frames * 12);
            body.extend_from_slice(b"TIT2\x00\x00\x00\x0c\x00\x00\x03Start title");
            for _ in 0..(total_frames - 2) {
                body.extend_from_slice(b"TXXX\x00\x00\x00\x02\x00\x00\x03\x00");
            }
            body.extend_from_slice(b"TPE1\x00\x00\x00\x0b\x00\x00\x03End artist");
            let sz = body.len() as u32;
            let mut f = vec![
                b'I',
                b'D',
                b'3',
                3,
                0,
                0,
                ((sz >> 21) & 0x7F) as u8,
                ((sz >> 14) & 0x7F) as u8,
                ((sz >> 7) & 0x7F) as u8,
                (sz & 0x7F) as u8,
            ];
            f.extend_from_slice(&body);
            f
        };
        let ok_id3 = parse(AudioFormat::Mp3, &make_mp3_frames(MAX_ID3_FRAMES)).unwrap();
        assert_eq!(ok_id3.tags.title.as_deref(), Some("Start title"));
        assert_eq!(ok_id3.tags.artist.as_deref(), Some("End artist"));
        assert_limit_err(
            parse(AudioFormat::Mp3, &make_mp3_frames(MAX_ID3_FRAMES + 1)),
            "too_many_id3_frames",
            "limit",
            MAX_ID3_FRAMES,
        );

        // 2. MAX_CONTAINER_CHUNKS (4096) in WAV: fmt + (n - 1) JUNK
        let make_wav_chunks = |total_chunks: usize| -> Vec<u8> {
            let junk = total_chunks - 1;
            let body_len = (4 + 24 + junk * 8) as u32;
            let mut f = Vec::with_capacity(8 + body_len as usize);
            f.extend_from_slice(b"RIFF");
            f.extend_from_slice(&body_len.to_le_bytes());
            f.extend_from_slice(b"WAVE");
            f.extend_from_slice(b"fmt \x10\x00\x00\x00\x01\x00\x01\x00\x40\x1F\x00\x00\x80\x3E\x00\x00\x02\x00\x10\x00");
            for _ in 0..junk {
                f.extend_from_slice(b"JUNK\x00\x00\x00\x00");
            }
            f
        };
        assert!(parse(AudioFormat::Wav, &make_wav_chunks(MAX_CONTAINER_CHUNKS)).is_ok());
        assert_limit_err(
            parse(AudioFormat::Wav, &make_wav_chunks(MAX_CONTAINER_CHUNKS + 1)),
            "too_many_container_chunks",
            "limit",
            MAX_CONTAINER_CHUNKS,
        );

        // 3. MAX_WAV_INFO_ENTRIES (256) in WAV LIST/INFO
        let make_wav_info = |total_entries: usize| -> Vec<u8> {
            let mut info = b"INFO".to_vec();
            for _ in 0..(total_entries - 1) {
                info.extend_from_slice(b"ICMT\x02\x00\x00\x00x\x00");
            }
            info.extend_from_slice(b"INAM\x0a\x00\x00\x00Info song\x00");
            let body_len = (4 + 8 + info.len()) as u32;
            let mut f = Vec::with_capacity(8 + body_len as usize);
            f.extend_from_slice(b"RIFF");
            f.extend_from_slice(&body_len.to_le_bytes());
            f.extend_from_slice(b"WAVE");
            f.extend_from_slice(b"LIST");
            f.extend_from_slice(&(info.len() as u32).to_le_bytes());
            f.extend_from_slice(&info);
            f
        };
        let ok_info = parse(AudioFormat::Wav, &make_wav_info(MAX_WAV_INFO_ENTRIES)).unwrap();
        assert_eq!(ok_info.tags.title.as_deref(), Some("Info song"));
        assert_limit_err(
            parse(AudioFormat::Wav, &make_wav_info(MAX_WAV_INFO_ENTRIES + 1)),
            "too_many_riff_info_entries",
            "limit",
            MAX_WAV_INFO_ENTRIES,
        );

        // 4. MAX_CONTAINER_CHUNKS (4096) in AIFF: COMM + (n - 1) JUNK
        let make_aiff_chunks = |total_chunks: usize| -> Vec<u8> {
            let junk = total_chunks - 1;
            let body_len = (4 + 26 + junk * 8) as u32;
            let mut f = Vec::with_capacity(8 + body_len as usize);
            f.extend_from_slice(b"FORM");
            f.extend_from_slice(&body_len.to_be_bytes());
            f.extend_from_slice(b"AIFF");
            f.extend_from_slice(b"COMM\x00\x00\x00\x12\x00\x01\x00\x00\xAC\x44\x00\x10\x40\x0E\xAC\x44\x00\x00\x00\x00\x00\x00");
            for _ in 0..junk {
                f.extend_from_slice(b"JUNK\x00\x00\x00\x00");
            }
            f
        };
        let ok_aiff = parse(AudioFormat::Aiff, &make_aiff_chunks(MAX_CONTAINER_CHUNKS)).unwrap();
        assert!((ok_aiff.tags.duration_s.unwrap() - 1.0).abs() < 0.01);
        assert_limit_err(
            parse(
                AudioFormat::Aiff,
                &make_aiff_chunks(MAX_CONTAINER_CHUNKS + 1),
            ),
            "too_many_container_chunks",
            "limit",
            MAX_CONTAINER_CHUNKS,
        );

        // 5. MAX_CONTAINER_CHUNKS (4096) in FLAC: STREAMINFO + (n - 1) PADDING
        let make_flac_blocks = |total_blocks: usize| -> Vec<u8> {
            let pad = total_blocks - 1;
            let mut f = Vec::with_capacity(4 + 38 + pad * 4);
            f.extend_from_slice(b"fLaC\x00\x00\x00\x22");
            let mut si = [0u8; 34];
            si[10] = 0x0A;
            si[11] = 0xC4;
            si[12] = 0x42;
            si[13] = 0xF0;
            si[14..18].copy_from_slice(&44_100u32.to_be_bytes());
            f.extend_from_slice(&si);
            for i in 0..pad {
                f.extend_from_slice(&[if i + 1 == pad { 0x81 } else { 0x01 }, 0, 0, 0]);
            }
            f
        };
        let ok_flac = parse(AudioFormat::Flac, &make_flac_blocks(MAX_CONTAINER_CHUNKS)).unwrap();
        assert!((ok_flac.tags.duration_s.unwrap() - 1.0).abs() < 0.01);
        assert_limit_err(
            parse(
                AudioFormat::Flac,
                &make_flac_blocks(MAX_CONTAINER_CHUNKS + 1),
            ),
            "too_many_container_chunks",
            "limit",
            MAX_CONTAINER_CHUNKS,
        );

        // 6. MAX_CONTAINER_CHUNKS (4096) in MP4: ftyp + (n - 1) free
        let make_mp4_boxes = |total_boxes: usize| -> Vec<u8> {
            let free = total_boxes - 1;
            let mut f = Vec::with_capacity(20 + free * 8);
            f.extend_from_slice(b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00M4A ");
            for _ in 0..free {
                f.extend_from_slice(b"\x00\x00\x00\x08free");
            }
            f
        };
        assert!(parse(AudioFormat::Mp4, &make_mp4_boxes(MAX_CONTAINER_CHUNKS)).is_ok());
        assert_limit_err(
            parse(AudioFormat::Mp4, &make_mp4_boxes(MAX_CONTAINER_CHUNKS + 1)),
            "too_many_container_chunks",
            "limit",
            MAX_CONTAINER_CHUNKS,
        );

        // 7. MAX_VORBIS_COMMENTS (65536) in FLAC
        let make_flac_comments = |total_comments: usize| -> Vec<u8> {
            let mut vc = Vec::with_capacity(32 + total_comments * 7);
            vc.extend_from_slice(&4u32.to_le_bytes());
            vc.extend_from_slice(b"test");
            vc.extend_from_slice(&(total_comments as u32).to_le_bytes());
            let title = b"TITLE=Many comments";
            vc.extend_from_slice(&(title.len() as u32).to_le_bytes());
            vc.extend_from_slice(title);
            for _ in 0..(total_comments - 1) {
                vc.extend_from_slice(&3u32.to_le_bytes());
                vc.extend_from_slice(b"X=1");
            }
            let mut f = Vec::with_capacity(4 + 38 + 4 + vc.len());
            f.extend_from_slice(b"fLaC\x00\x00\x00\x22");
            f.extend_from_slice(&[0u8; 34]);
            let len = vc.len() as u32;
            f.extend_from_slice(&[
                0x84,
                ((len >> 16) & 0xFF) as u8,
                ((len >> 8) & 0xFF) as u8,
                (len & 0xFF) as u8,
            ]);
            f.extend_from_slice(&vc);
            f
        };
        let ok_vc = parse(
            AudioFormat::Flac,
            &make_flac_comments(MAX_VORBIS_COMMENTS as usize),
        )
        .unwrap();
        assert_eq!(ok_vc.tags.title.as_deref(), Some("Many comments"));
        assert_limit_err(
            parse(
                AudioFormat::Flac,
                &make_flac_comments((MAX_VORBIS_COMMENTS as usize) + 1),
            ),
            "too_many_vorbis_comments",
            "limit",
            MAX_VORBIS_COMMENTS as usize,
        );

        // 8. MAX_MP4_DEPTH (8): moov (depth 1) + udta_layers * udta + meta (depth = udta_layers + 2)
        let make_mp4_depth = |udta_layers: usize| -> Vec<u8> {
            let wrap = |tag: &[u8; 4], payload: &[u8]| -> Vec<u8> {
                let mut out = Vec::with_capacity(8 + payload.len());
                out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
                out.extend_from_slice(tag);
                out.extend_from_slice(payload);
                out
            };
            let mut data_payload = Vec::new();
            data_payload.extend_from_slice(&1u32.to_be_bytes());
            data_payload.extend_from_slice(&0u32.to_be_bytes());
            data_payload.extend_from_slice(b"Deep title");
            let nam = wrap(b"\xA9nam", &wrap(b"data", &data_payload));
            let ilst = wrap(b"ilst", &nam);
            let mut meta_body = vec![0u8; 4];
            meta_body.extend_from_slice(&ilst);
            let mut inner = wrap(b"meta", &meta_body);
            for _ in 0..udta_layers {
                inner = wrap(b"udta", &inner);
            }
            let mut f = b"\x00\x00\x00\x14ftypM4A \x00\x00\x00\x00M4A ".to_vec();
            f.extend_from_slice(&wrap(b"moov", &inner));
            f
        };
        let ok_depth = parse(AudioFormat::Mp4, &make_mp4_depth(MAX_MP4_DEPTH - 2)).unwrap();
        assert_eq!(ok_depth.tags.title.as_deref(), Some("Deep title"));
        assert_limit_err(
            parse(AudioFormat::Mp4, &make_mp4_depth(MAX_MP4_DEPTH - 1)),
            "mp4_nesting_too_deep",
            "limit",
            MAX_MP4_DEPTH,
        );

        // 9. MAX_OGG_SCAN_PAGES (64): dummy non-audio BOS pages before a valid OpusHead BOS page
        let make_ogg_scan = |dummy_pages: usize| -> Vec<u8> {
            let mut f = Vec::with_capacity((dummy_pages + 2) * 48);
            for i in 0..dummy_pages {
                f.extend_from_slice(b"OggS\x00\x02");
                f.extend_from_slice(&0u64.to_le_bytes());
                f.extend_from_slice(&(100 + i as u32).to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&[1, 8]);
                f.extend_from_slice(b"NOTAUDIO");
            }
            let head = b"OpusHead\x01\x02\x38\x01\x80\xBB\x00\x00\x00\x00\x00";
            f.extend_from_slice(b"OggS\x00\x02");
            f.extend_from_slice(&0u64.to_le_bytes());
            f.extend_from_slice(&999u32.to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&[1, head.len() as u8]);
            f.extend_from_slice(head);
            let tags_pkt =
                b"OpusTags\x04\x00\x00\x00test\x01\x00\x00\x00\x16\x00\x00\x00TITLE=After scan pages";
            f.extend_from_slice(b"OggS\x00\x00");
            f.extend_from_slice(&48_000u64.to_le_bytes());
            f.extend_from_slice(&999u32.to_le_bytes());
            f.extend_from_slice(&1u32.to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&[1, tags_pkt.len() as u8]);
            f.extend_from_slice(tags_pkt);
            f
        };
        let ok_scan = parse(AudioFormat::Ogg, &make_ogg_scan(MAX_OGG_SCAN_PAGES - 1)).unwrap();
        assert_eq!(ok_scan.tags.title.as_deref(), Some("After scan pages"));
        assert_limit_err(
            parse(AudioFormat::Ogg, &make_ogg_scan(MAX_OGG_SCAN_PAGES)),
            "too_many_ogg_scan_pages",
            "limit",
            MAX_OGG_SCAN_PAGES,
        );

        // 10. MAX_OGG_COMMENT_PAGES (2048): valid OpusTags packet split across exact N pages
        let make_ogg_comment_pages = |page_count: usize| -> Vec<u8> {
            let final_len = 100usize;
            let total_pkt_len = (page_count - 1) * 255 + final_len;
            let title_entry = b"TITLE=Split comment";
            let fixed_overhead = 8 + 4 + 4 + 4 + title_entry.len();
            let vendor_len = total_pkt_len - fixed_overhead;
            let mut pkt = Vec::with_capacity(total_pkt_len);
            pkt.extend_from_slice(b"OpusTags");
            pkt.extend_from_slice(&(vendor_len as u32).to_le_bytes());
            pkt.resize(pkt.len() + vendor_len, b'v');
            pkt.extend_from_slice(&1u32.to_le_bytes());
            pkt.extend_from_slice(&(title_entry.len() as u32).to_le_bytes());
            pkt.extend_from_slice(title_entry);
            assert_eq!(pkt.len(), total_pkt_len);

            let mut f = Vec::with_capacity(64 + page_count * (28 + 255));
            let head = b"OpusHead\x01\x02\x38\x01\x80\xBB\x00\x00\x00\x00\x00";
            f.extend_from_slice(b"OggS\x00\x02");
            f.extend_from_slice(&0u64.to_le_bytes());
            f.extend_from_slice(&77u32.to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&0u32.to_le_bytes());
            f.extend_from_slice(&[1, head.len() as u8]);
            f.extend_from_slice(head);

            let mut off = 0usize;
            for page_idx in 0..page_count {
                let is_last = page_idx + 1 == page_count;
                let seg_len = if is_last { final_len } else { 255 };
                f.extend_from_slice(b"OggS\x00");
                f.push(if page_idx == 0 { 0x00 } else { 0x01 });
                f.extend_from_slice(&if is_last { 0u64 } else { u64::MAX }.to_le_bytes());
                f.extend_from_slice(&77u32.to_le_bytes());
                f.extend_from_slice(&((page_idx + 1) as u32).to_le_bytes());
                f.extend_from_slice(&0u32.to_le_bytes());
                f.extend_from_slice(&[1, seg_len as u8]);
                f.extend_from_slice(&pkt[off..off + seg_len]);
                off += seg_len;
            }
            assert_eq!(off, pkt.len());
            f
        };
        let ok_cp = parse(
            AudioFormat::Ogg,
            &make_ogg_comment_pages(MAX_OGG_COMMENT_PAGES),
        )
        .unwrap();
        assert_eq!(ok_cp.tags.title.as_deref(), Some("Split comment"));
        assert_limit_err(
            parse(
                AudioFormat::Ogg,
                &make_ogg_comment_pages(MAX_OGG_COMMENT_PAGES + 1),
            ),
            "too_many_ogg_comment_pages",
            "limit",
            MAX_OGG_COMMENT_PAGES,
        );

        // 11. MAX_AUDIO_TAG_BYTES (16 MiB): exact 16 MiB succeeds, 16 MiB + 1 refuses
        let make_sized_mp3 = |body_len: usize| -> Vec<u8> {
            let title = b"TIT2\x00\x00\x00\x0a\x00\x00\x03Exact 16M";
            let priv_payload = body_len - title.len() - 10;
            let sz = body_len as u32;
            let mut f = Vec::with_capacity(10 + body_len);
            f.extend_from_slice(b"ID3\x03\x00\x00");
            f.extend_from_slice(&[
                ((sz >> 21) & 0x7F) as u8,
                ((sz >> 14) & 0x7F) as u8,
                ((sz >> 7) & 0x7F) as u8,
                (sz & 0x7F) as u8,
            ]);
            f.extend_from_slice(title);
            f.extend_from_slice(b"PRIV");
            f.extend_from_slice(&(priv_payload as u32).to_be_bytes());
            f.extend_from_slice(&[0, 0]);
            f.resize(10 + body_len, b'A');
            f
        };
        let ok_size = parse(AudioFormat::Mp3, &make_sized_mp3(MAX_AUDIO_TAG_BYTES)).unwrap();
        assert_eq!(ok_size.tags.title.as_deref(), Some("Exact 16M"));
        assert_limit_err(
            parse(AudioFormat::Mp3, &make_sized_mp3(MAX_AUDIO_TAG_BYTES + 1)),
            "tag_too_large",
            "limit_bytes",
            MAX_AUDIO_TAG_BYTES,
        );
    }
}
