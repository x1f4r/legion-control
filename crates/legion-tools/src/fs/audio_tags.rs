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

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use legion_core::media::{Category, classify_path};
use legion_core::proto::{ErrorBody, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Maximum cumulative bytes of tag/metadata/picture data read from a single audio file (16 MiB).
pub(crate) const MAX_AUDIO_TAG_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of Unicode scalar values kept in any text tag field.
pub(crate) const MAX_TAG_FIELD_CHARS: usize = 512;

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

/// Cleans a tag text value per the player contract: removes all control characters, trims
/// surrounding whitespace, caps at [`MAX_TAG_FIELD_CHARS`] (512) Unicode characters, re-trims, and
/// returns `None` when empty.
pub(crate) fn clean_tag_text(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len().min(MAX_TAG_FIELD_CHARS));
    let mut count = 0usize;
    let mut started = false;
    for ch in raw.chars() {
        if ch.is_control() {
            continue;
        }
        if !started {
            if ch.is_whitespace() {
                continue;
            }
            started = true;
        }
        if count >= MAX_TAG_FIELD_CHARS {
            break;
        }
        out.push(ch);
        count += 1;
    }
    let trimmed_end = out.trim_end().len();
    out.truncate(trimmed_end);
    (!out.is_empty()).then_some(out)
}

/// Parses a positive track or disc number from tag text such as `"3"`, `"03"`, or `"3/12"`.
pub(crate) fn parse_track_or_disc(raw: &str) -> Option<u32> {
    let cleaned = clean_tag_text(raw)?;
    let head = cleaned.split('/').next()?.trim();
    let n = head.parse::<u32>().ok()?;
    (n > 0).then_some(n)
}

/// Parses a 4-digit year from tag text such as `"2024"`, `"2024-05-12"`, or `"2024-05-12T10:00Z"`.
pub(crate) fn parse_year(raw: &str) -> Option<u32> {
    let cleaned = clean_tag_text(raw)?;
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
        let text: String = slice[..end].iter().map(|&b| char::from(b)).collect();
        clean_tag_text(&text)
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
        tags.year = parse_year(&yr);
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

fn deunsync(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0usize;
    while i < data.len() {
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
    raw_body: Vec<u8>,
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
    // Drop `raw_body` right after deunsyncing so peak heap does not hold both copies during frame
    // parsing and cover extraction.
    let body: Vec<u8> = if global_unsync && major < 4 {
        deunsync(&raw_body)
    } else {
        raw_body
    };

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

    while pos + frame_hdr_len <= body.len() && frames_seen < MAX_ID3_FRAMES {
        if body[pos] == 0 {
            // Reached zero-padding at the end of the ID3v2 tag.
            break;
        }
        frames_seen += 1;

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
        let owned_frame;
        if frame_unsync {
            owned_frame = deunsync(payload);
            payload = &owned_frame;
        }

        match id_bytes {
            b"TIT2" | b"TT2" => {
                if tags.title.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.title = clean_tag_text(&text);
                }
            }
            b"TPE1" | b"TP1" => {
                if tags.artist.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.artist = clean_tag_text(&text);
                }
            }
            b"TALB" | b"TAL" => {
                if tags.album.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.album = clean_tag_text(&text);
                }
            }
            b"TPE2" | b"TP2" => {
                if tags.album_artist.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.album_artist = clean_tag_text(&text);
                }
            }
            b"TRCK" | b"TRK" => {
                if tags.track.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.track = parse_track_or_disc(&text);
                }
            }
            b"TPOS" | b"TPA" => {
                if tags.disc.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.disc = parse_track_or_disc(&text);
                }
            }
            b"TDRC" | b"TYER" | b"TYE" => {
                if tags.year.is_none()
                    && let Some(text) = decode_id3_text_frame(payload)
                {
                    tags.year = parse_year(&text);
                }
            }
            b"APIC" => parse_id3_apic(payload, cover),
            b"PIC" => parse_id3_pic(payload, cover),
            _ => {}
        }
    }

    Ok(())
}

fn decode_id3_text_frame(payload: &[u8]) -> Option<String> {
    let (&enc, data) = payload.split_first()?;
    decode_id3_string(enc, data)
}

fn decode_id3_string(encoding: u8, data: &[u8]) -> Option<String> {
    match encoding {
        // ISO-8859-1 (Latin-1)
        0 => Some(data.iter().map(|&b| char::from(b)).collect()),
        // UTF-16 with BOM
        1 => {
            let (le, body) = if data.starts_with(&[0xFF, 0xFE]) {
                (true, &data[2..])
            } else if data.starts_with(&[0xFE, 0xFF]) {
                (false, &data[2..])
            } else {
                (true, data)
            };
            Some(decode_utf16_bytes(body, le))
        }
        // UTF-16BE without BOM
        2 => Some(decode_utf16_bytes(data, false)),
        // UTF-8
        3 => Some(String::from_utf8_lossy(data).into_owned()),
        _ => None,
    }
}

fn decode_utf16_bytes(data: &[u8], little_endian: bool) -> String {
    let units = data.as_chunks::<2>().0.iter().map(|pair| {
        if little_endian {
            u16::from_le_bytes([pair[0], pair[1]])
        } else {
            u16::from_be_bytes([pair[0], pair[1]])
        }
    });
    char::decode_utf16(units)
        .map(|res| res.unwrap_or('\u{FFFD}'))
        .collect()
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

    while stream.remaining_file() >= 8 && chunks < MAX_CONTAINER_CHUNKS {
        chunks += 1;
        let ch = stream.read_fixed::<8>()?;
        let fourcc = [ch[0], ch[1], ch[2], ch[3]];
        let chunk_size = u64::from(u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]));
        if chunk_size > stream.remaining_file() {
            return Err(stream.malformed("WAV chunk length exceeds file size"));
        }
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
                    parse_riff_info(&raw, &mut info_fallback);
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

fn parse_riff_info(raw: &[u8], out: &mut ParsedAudioTags) {
    let mut pos = 0usize;
    let mut count = 0usize;
    while pos + 8 <= raw.len() && count < 256 {
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
        let text = String::from_utf8_lossy(val_slice);
        match id {
            b"INAM" if out.title.is_none() => out.title = clean_tag_text(&text),
            b"IART" if out.artist.is_none() => out.artist = clean_tag_text(&text),
            b"IPRD" if out.album.is_none() => out.album = clean_tag_text(&text),
            b"ICRD" if out.year.is_none() => out.year = parse_year(&text),
            b"ITRK" if out.track.is_none() => out.track = parse_track_or_disc(&text),
            _ => {}
        }
        let padded = len.saturating_add(len & 1);
        let Some(next) = start.checked_add(padded) else {
            break;
        };
        pos = next;
    }
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

    let mut num_sample_frames = 0u32;
    let mut sample_rate = 0.0f64;
    let mut chunks = 0usize;

    while stream.remaining_file() >= 8 && chunks < MAX_CONTAINER_CHUNKS {
        chunks += 1;
        let ch = stream.read_fixed::<8>()?;
        let fourcc = [ch[0], ch[1], ch[2], ch[3]];
        let chunk_size = u64::from(u32::from_be_bytes([ch[4], ch[5], ch[6], ch[7]]));
        if chunk_size > stream.remaining_file() {
            return Err(stream.malformed("AIFF chunk length exceeds file size"));
        }
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
                    num_sample_frames = u32::from_be_bytes([comm[2], comm[3], comm[4], comm[5]]);
                    sample_rate = decode_ieee80_be([
                        comm[8], comm[9], comm[10], comm[11], comm[12], comm[13], comm[14],
                        comm[15], comm[16], comm[17],
                    ]);
                }
            }
            b"ID3 " | b"id3 " => {
                let raw = stream.read_tag_bytes(chunk_size)?;
                parse_id3v2_slice(raw, tags, cover, stream.display_path)?;
            }
            _ => {}
        }

        stream.seek_to(next_pos)?;
    }

    if sample_rate.is_finite() && sample_rate > 0.0 && num_sample_frames > 0 {
        tags.duration_s = normalize_duration(f64::from(num_sample_frames) / sample_rate);
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
            return Err(stream.malformed("too many FLAC metadata blocks"));
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

fn parse_flac_picture_owned(mut raw: Vec<u8>, cover: &mut CoverSlot) -> Result<(), &'static str> {
    if raw.len() < 32 {
        return Err("truncated METADATA_BLOCK_PICTURE");
    }
    let pic_type_u32 = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let pic_type = u8::try_from(pic_type_u32).unwrap_or(0);
    if !cover.wants_picture(pic_type) {
        return Ok(());
    }
    let mime_len = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
    let mime_end = 8usize
        .checked_add(mime_len)
        .ok_or("METADATA_BLOCK_PICTURE MIME overflow")?;
    if mime_end > raw.len() {
        return Err("METADATA_BLOCK_PICTURE MIME exceeds block");
    }
    if &raw[8..mime_end] == b"-->" {
        return Ok(());
    }
    let desc_len_end = mime_end
        .checked_add(4)
        .ok_or("METADATA_BLOCK_PICTURE offset overflow")?;
    if desc_len_end > raw.len() {
        return Err("truncated METADATA_BLOCK_PICTURE description length");
    }
    let desc_len = u32::from_be_bytes([
        raw[mime_end],
        raw[mime_end + 1],
        raw[mime_end + 2],
        raw[mime_end + 3],
    ]) as usize;
    let desc_end = desc_len_end
        .checked_add(desc_len)
        .ok_or("METADATA_BLOCK_PICTURE description overflow")?;
    let data_len_off = desc_end
        .checked_add(16)
        .ok_or("METADATA_BLOCK_PICTURE header overflow")?;
    let data_start = data_len_off
        .checked_add(4)
        .ok_or("METADATA_BLOCK_PICTURE data length overflow")?;
    if data_start > raw.len() {
        return Err("truncated METADATA_BLOCK_PICTURE header");
    }
    let data_len = u32::from_be_bytes([
        raw[data_len_off],
        raw[data_len_off + 1],
        raw[data_len_off + 2],
        raw[data_len_off + 3],
    ]) as usize;
    let data_end = data_start
        .checked_add(data_len)
        .ok_or("METADATA_BLOCK_PICTURE data overflow")?;
    if data_end > raw.len() {
        return Err("METADATA_BLOCK_PICTURE image data truncated");
    }
    if cover.mode == CoverExtractMode::CheckOnly {
        cover.consider_slice(pic_type, &raw[data_start..data_end]);
    } else {
        raw.truncate(data_end);
        raw.drain(..data_start);
        cover.consider_owned(pic_type, raw);
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
        return Err(bad("too many Vorbis comment entries"));
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
            if cover.mode == CoverExtractMode::CheckOnly && cover.has_any {
                continue;
            }
            if cover.has_front {
                continue;
            }
            let decoded = if val_bytes.iter().any(u8::is_ascii_whitespace) {
                let compact: Vec<u8> = val_bytes
                    .iter()
                    .copied()
                    .filter(|b| !b.is_ascii_whitespace())
                    .collect();
                BASE64.decode(&compact).ok()
            } else {
                BASE64.decode(val_bytes).ok()
            };
            if let Some(decoded) = decoded {
                let _ = parse_flac_picture_owned(decoded, cover);
            }
            continue;
        }

        let value = String::from_utf8_lossy(val_bytes);
        if key_bytes.eq_ignore_ascii_case(b"TITLE") {
            if tags.title.is_none() {
                tags.title = clean_tag_text(&value);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"ARTIST") {
            if tags.artist.is_none() {
                tags.artist = clean_tag_text(&value);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"ALBUM") {
            if tags.album.is_none() {
                tags.album = clean_tag_text(&value);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"ALBUMARTIST")
            || key_bytes.eq_ignore_ascii_case(b"ALBUM ARTIST")
        {
            if tags.album_artist.is_none() {
                tags.album_artist = clean_tag_text(&value);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"TRACKNUMBER") {
            if tags.track.is_none() {
                tags.track = parse_track_or_disc(&value);
            }
        } else if key_bytes.eq_ignore_ascii_case(b"DISCNUMBER") {
            if tags.disc.is_none() {
                tags.disc = parse_track_or_disc(&value);
            }
        } else if (key_bytes.eq_ignore_ascii_case(b"DATE")
            || key_bytes.eq_ignore_ascii_case(b"YEAR"))
            && tags.year.is_none()
        {
            tags.year = parse_year(&value);
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
    for _ in 0..MAX_OGG_SCAN_PAGES {
        if stream.remaining_file() < 27 {
            break;
        }
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
    //    budget.
    let mut pages_read = 0usize;
    while !comment_done && stream.remaining_file() >= 27 && pages_read < MAX_OGG_COMMENT_PAGES {
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

        let chunk = stream.read_tag_bytes(want_bytes as u64)?;
        comment_packet.extend_from_slice(&chunk);
        stream.seek_to(page_end)?;
    }

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

    while pos + 8 <= end && visited < MAX_CONTAINER_CHUNKS {
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
        return Ok(());
    }
    let mut pos = start;
    while pos + 8 <= end && *visited < MAX_CONTAINER_CHUNKS {
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
    while pos + 8 <= end && *visited < MAX_CONTAINER_CHUNKS {
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
    while pos + 8 <= end && *visited < MAX_CONTAINER_CHUNKS {
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
                let payload = stream.read_tag_bytes(value_len)?;
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

fn decode_mp4_text(type_indicator: u32, payload: &[u8]) -> Option<String> {
    match type_indicator {
        1 => Some(String::from_utf8_lossy(payload).into_owned()),
        2 => Some(decode_utf16_bytes(payload, false)),
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
            if tags.title.is_none()
                && let Some(text) = decode_mp4_text(type_indicator, payload)
            {
                tags.title = clean_tag_text(&text);
            }
        }
        b"\xa9ART" => {
            if tags.artist.is_none()
                && let Some(text) = decode_mp4_text(type_indicator, payload)
            {
                tags.artist = clean_tag_text(&text);
            }
        }
        b"\xa9alb" => {
            if tags.album.is_none()
                && let Some(text) = decode_mp4_text(type_indicator, payload)
            {
                tags.album = clean_tag_text(&text);
            }
        }
        b"aART" => {
            if tags.album_artist.is_none()
                && let Some(text) = decode_mp4_text(type_indicator, payload)
            {
                tags.album_artist = clean_tag_text(&text);
            }
        }
        b"\xa9day" => {
            if tags.year.is_none()
                && let Some(text) = decode_mp4_text(type_indicator, payload)
            {
                tags.year = parse_year(&text);
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
        assert_eq!(
            deunsync(&[0xFF, 0x00, 0xD8, 0xFF, 0x00, 0x00, 0x01, 0xFF]),
            vec![0xFF, 0xD8, 0xFF, 0x00, 0x01, 0xFF]
        );
    }

    #[test]
    fn property_truncated_and_mutated_inputs_never_panic_or_exceed_bounds() {
        // Build minimal valid seeds for every supported format and feed every truncation plus
        // deterministic mutations (including oversized declared lengths) through `parse_audio`.
        let tiny_png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        let seeds: Vec<(AudioFormat, Vec<u8>)> = vec![
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

        // Deterministic xorshift64 PRNG so mutations are reproducible across runs.
        let mut rng = 0x9E37_79B9_7F4A_7C15u64;
        let mut next_u64 = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let label = Path::new("/fuzz.bin");
        for (fmt, seed) in &seeds {
            // 1. Every prefix truncation 0..=seed.len()
            for cut in 0..=seed.len() {
                let slice = &seed[..cut];
                for mode in [CoverExtractMode::CheckOnly, CoverExtractMode::ExtractBytes] {
                    let mut cur = Cursor::new(slice);
                    let _ = parse_audio(&mut cur, slice.len() as u64, *fmt, mode, label);
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
                        3 => buf[idx] ^= (1u8) << (next_u64() % 8),
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
                    let _ = parse_audio(&mut cur, buf.len() as u64, *fmt, mode, label);
                }
            }
        }
    }
}
