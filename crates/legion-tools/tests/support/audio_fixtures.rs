//! Deterministic, small audio fixture builders for `fs.tags` and audio `fs.thumb` tests.
//!
//! Every generated fixture is a few kilobytes of silent container framing plus placeholder
//! metadata (`"First song"`, `"Someone"`, `"First album"`, `"Album artist"`) and tiny solid-color
//! covers so tests can verify tag fields, cover selection priority, hostile inputs, and worker
//! memory caps without checking in binary media files.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "shared across multiple integration test binaries"
)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgb, Rgba};

pub fn encode_solid_jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(width, height, Rgb(rgb));
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 85)
        .encode(&img, width, height, ExtendedColorType::Rgb8)
        .expect("encode solid jpeg");
    out
}

pub fn encode_solid_png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(width, height, Rgb(rgb));
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(&img, width, height, ExtendedColorType::Rgb8)
        .expect("encode solid png");
    out
}

pub fn encode_solid_rgba_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
    let img = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(width, height, Rgba(rgba));
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(&img, width, height, ExtendedColorType::Rgba8)
        .expect("encode solid rgba png");
    out
}

/// Builds a real, valid, deterministic ~10.2 MiB RGB PNG cover (1900x1900 pseudo-random pixels,
/// ~3.61 MP, incompressible so the PNG file itself is just over 10 MiB while decoding quickly).
pub fn build_10mib_png_cover() -> Vec<u8> {
    let (w, h) = (1900u32, 1900u32);
    let mut raw = vec![0u8; (w as usize) * (h as usize) * 3];
    let mut state = 0x5EED_CAFE_1234_5678u64;
    for chunk in raw.as_chunks_mut::<8>().0 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    let mut out = Vec::with_capacity(11 * 1024 * 1024);
    PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(&raw, w, h, ExtendedColorType::Rgb8)
    .expect("encode 10 MiB png");
    assert!(
        out.len() >= 10 * 1024 * 1024 && out.len() <= 12 * 1024 * 1024,
        "expected ~10 MiB PNG, got {} bytes",
        out.len()
    );
    out
}

/// Builds a deterministic, high-entropy noisy RGB JPEG of roughly `~10 MiB` (high-frequency noise
/// so re-encoding at `size = 1024` produces a ~0.8–1.0 MiB JPEG / ~1.1–1.3 MiB base64 output).
pub fn build_noisy_10mib_jpeg_cover() -> Vec<u8> {
    let (w, h) = (1900u32, 1900u32);
    let mut raw = vec![0u8; (w as usize) * (h as usize) * 3];
    let mut state = 0x1234_5678_9ABC_DEF0u64;
    for chunk in raw.as_chunks_mut::<8>().0 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        chunk.copy_from_slice(&state.to_le_bytes());
    }
    let mut out = Vec::with_capacity(11 * 1024 * 1024);
    JpegEncoder::new_with_quality(&mut out, 97)
        .encode(&raw, w, h, ExtendedColorType::Rgb8)
        .expect("encode 10 MiB noisy jpeg");
    assert!(
        out.len() >= 9 * 1024 * 1024 && out.len() <= 12 * 1024 * 1024,
        "expected ~10 MiB noisy JPEG, got {} bytes",
        out.len()
    );
    out
}

/// Builds a deterministic transparent noise RGBA8 PNG of `width x height` pixels with varying
/// non-opaque alpha so `fs.thumb` keeps it as an incompressible PNG.
pub fn build_transparent_noise_png(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let mut raw = vec![0u8; (width as usize) * (height as usize) * 4];
    let mut state = seed ^ 0xA5A5_5A5A_1357_2468u64;
    for px in raw.as_chunks_mut::<4>().0 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let b = state.to_le_bytes();
        px[0] = b[4];
        px[1] = b[5];
        px[2] = b[6];
        px[3] = 64 + (b[7] & 0x7F); // strictly < 255 so alpha is preserved
    }
    let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4 + 1024);
    PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    )
    .write_image(&raw, width, height, ExtendedColorType::Rgba8)
    .expect("encode transparent noise png");
    out
}

pub fn encode_syncsafe(n: u32) -> [u8; 4] {
    [
        ((n >> 21) & 0x7F) as u8,
        ((n >> 14) & 0x7F) as u8,
        ((n >> 7) & 0x7F) as u8,
        (n & 0x7F) as u8,
    ]
}

fn unsync_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 16);
    for (i, &b) in input.iter().enumerate() {
        out.push(b);
        if b == 0xFF {
            let next = input.get(i + 1).copied();
            if next.is_none() || next.is_some_and(|n| n == 0x00 || (n & 0xE0) == 0xE0) {
                out.push(0x00);
            }
        }
    }
    out
}

pub fn make_id3v23_text_frame(id: &[u8; 4], encoding: u8, text: &str) -> Vec<u8> {
    let mut payload = vec![encoding];
    match encoding {
        0 => payload.extend(text.chars().map(|c| c as u8)),
        1 => {
            payload.extend_from_slice(&[0xFF, 0xFE]);
            for u in text.encode_utf16() {
                payload.extend_from_slice(&u.to_le_bytes());
            }
        }
        2 => {
            for u in text.encode_utf16() {
                payload.extend_from_slice(&u.to_be_bytes());
            }
        }
        3 => payload.extend_from_slice(text.as_bytes()),
        _ => unreachable!(),
    }
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.extend_from_slice(id);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&payload);
    frame
}

pub fn make_id3v23_apic_frame(mime: &str, pic_type: u8, desc: &str, img: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4 + mime.len() + desc.len() + img.len());
    payload.push(0); // ISO-8859-1 description
    payload.extend_from_slice(mime.as_bytes());
    payload.push(0);
    payload.push(pic_type);
    payload.extend_from_slice(desc.as_bytes());
    payload.push(0);
    payload.extend_from_slice(img);

    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.extend_from_slice(b"APIC");
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&payload);
    frame
}

pub fn wrap_id3v23(frames: &[u8], global_unsync: bool, with_ext_header: bool) -> Vec<u8> {
    let mut raw_body = Vec::new();
    if with_ext_header {
        // ID3v2.3 extended header: size=6 (excludes itself), 2 flags bytes, 4 padding bytes.
        raw_body.extend_from_slice(&6u32.to_be_bytes());
        raw_body.extend_from_slice(&[0u8; 6]);
    }
    raw_body.extend_from_slice(frames);
    let body = if global_unsync {
        unsync_bytes(&raw_body)
    } else {
        raw_body
    };
    let flags = if global_unsync { 0x80 } else { 0x00 } | if with_ext_header { 0x40 } else { 0x00 };
    let mut tag = Vec::with_capacity(10 + body.len());
    tag.extend_from_slice(b"ID3");
    tag.extend_from_slice(&[3, 0, flags]);
    tag.extend_from_slice(&encode_syncsafe(body.len() as u32));
    tag.extend_from_slice(&body);
    tag
}

/// Builds a single valid silent MPEG-1 Layer 3 128 kbps 44.1 kHz stereo frame (417 bytes).
pub fn silent_mp3_frame() -> Vec<u8> {
    // 0xFF 0xFB = MPEG1 Layer3 no-CRC; 0x90 = 128 kbps, 44100 Hz, no padding; 0x00 = stereo.
    let mut frame = vec![0u8; 417];
    frame[0] = 0xFF;
    frame[1] = 0xFB;
    frame[2] = 0x90;
    frame[3] = 0x00;
    frame
}

/// Builds an MPEG-1 Layer 3 Xing header frame reporting `total_frames` frames (duration =
/// `total_frames * 1152 / 44100` seconds).
pub fn silent_mp3_xing_frame(total_frames: u32) -> Vec<u8> {
    let mut frame = silent_mp3_frame();
    // MPEG1 stereo side-info is 32 bytes -> Xing starts at offset 4 + 32 = 36.
    frame[36..40].copy_from_slice(b"Xing");
    frame[40..44].copy_from_slice(&1u32.to_be_bytes()); // frames flag
    frame[44..48].copy_from_slice(&total_frames.to_be_bytes());
    frame
}

/// Builds a complete ID3v2.3 MP3 with mixed text encodings (UTF-8, UTF-16 BOM, UTF-16BE, Latin-1),
/// optional unsynchronisation/extended header, covers, and Xing duration (~5.0 s).
pub fn build_mp3_id3v23(
    covers: &[(&str, u8, &[u8])],
    global_unsync: bool,
    with_ext_header: bool,
) -> Vec<u8> {
    let mut frames = Vec::new();
    frames.extend_from_slice(&make_id3v23_text_frame(b"TIT2", 1, "First song"));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TPE1", 2, "Someone"));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TALB", 0, "First album"));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TPE2", 1, "Album artist"));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TRCK", 0, "3/10"));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TPOS", 0, "1/2"));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TYER", 0, "2025"));
    for &(mime, pic_type, img) in covers {
        frames.extend_from_slice(&make_id3v23_apic_frame(mime, pic_type, "cover", img));
    }
    let mut mp3 = wrap_id3v23(&frames, global_unsync, with_ext_header);
    // 191 frames * 1152 / 44100 ~= 4.989 s
    mp3.extend_from_slice(&silent_mp3_xing_frame(191));
    mp3.extend_from_slice(&silent_mp3_frame());
    mp3
}

/// Builds an ID3v2.2 MP3 (`TT2`, `TP1`, `TAL`, `TP2`, `TRK`, `TPA`, `TYE`, `PIC`).
pub fn build_mp3_id3v22(cover: Option<&[u8]>) -> Vec<u8> {
    let push_v22 = |out: &mut Vec<u8>, id: &[u8; 3], payload: &[u8]| {
        let len = payload.len() as u32;
        out.extend_from_slice(id);
        out.extend_from_slice(&[
            ((len >> 16) & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            (len & 0xFF) as u8,
        ]);
        out.extend_from_slice(payload);
    };
    let mut frames = Vec::new();
    push_v22(&mut frames, b"TT2", b"\x00Second song");
    push_v22(&mut frames, b"TP1", b"\x00Someone");
    push_v22(&mut frames, b"TAL", b"\x00Legacy album");
    push_v22(&mut frames, b"TP2", b"\x00Album artist");
    push_v22(&mut frames, b"TRK", b"\x002/8");
    push_v22(&mut frames, b"TPA", b"\x001");
    push_v22(&mut frames, b"TYE", b"\x002022");
    if let Some(img) = cover {
        let mut pic = vec![0u8];
        pic.extend_from_slice(b"JPG");
        pic.push(3); // Front cover
        pic.extend_from_slice(b"front\0");
        pic.extend_from_slice(img);
        push_v22(&mut frames, b"PIC", &pic);
    }
    let mut mp3 = Vec::with_capacity(10 + frames.len() + 834);
    mp3.extend_from_slice(b"ID3");
    mp3.extend_from_slice(&[2, 0, 0]);
    mp3.extend_from_slice(&encode_syncsafe(frames.len() as u32));
    mp3.extend_from_slice(&frames);
    mp3.extend_from_slice(&silent_mp3_frame());
    mp3.extend_from_slice(&silent_mp3_frame());
    mp3
}

/// Builds an ID3v2.4 MP3 with UTF-8 frames, `TDRC`, per-frame unsynchronisation, and Data Length
/// Indicator (DLI) on the APIC frame.
pub fn build_mp3_id3v24(cover: Option<&[u8]>) -> Vec<u8> {
    let push_v24 = |out: &mut Vec<u8>, id: &[u8; 4], payload: &[u8], unsync_with_dli: bool| {
        if unsync_with_dli {
            let unsynced = unsync_bytes(payload);
            let total = 4 + unsynced.len();
            out.extend_from_slice(id);
            out.extend_from_slice(&encode_syncsafe(total as u32));
            out.extend_from_slice(&[0x00, 0x03]); // unsync (0x02) + DLI (0x01)
            out.extend_from_slice(&encode_syncsafe(payload.len() as u32));
            out.extend_from_slice(&unsynced);
        } else {
            out.extend_from_slice(id);
            out.extend_from_slice(&encode_syncsafe(payload.len() as u32));
            out.extend_from_slice(&[0x00, 0x00]);
            out.extend_from_slice(payload);
        }
    };
    let mut frames = Vec::new();
    push_v24(&mut frames, b"TIT2", b"\x03Fourth song", false);
    push_v24(&mut frames, b"TPE1", b"\x03Someone", false);
    push_v24(&mut frames, b"TALB", b"\x03Modern album", false);
    push_v24(&mut frames, b"TPE2", b"\x03Album artist", false);
    push_v24(&mut frames, b"TRCK", b"\x034", false);
    push_v24(&mut frames, b"TPOS", b"\x032/2", false);
    push_v24(&mut frames, b"TDRC", b"\x032024-07-15", false);
    if let Some(img) = cover {
        let mut apic = vec![3u8]; // UTF-8
        apic.extend_from_slice(b"image/jpeg\0");
        apic.push(3);
        apic.extend_from_slice(b"front\0");
        apic.extend_from_slice(img);
        push_v24(&mut frames, b"APIC", &apic, true);
    }
    let mut mp3 = Vec::with_capacity(10 + frames.len() + 834);
    mp3.extend_from_slice(b"ID3");
    mp3.extend_from_slice(&[4, 0, 0]);
    mp3.extend_from_slice(&encode_syncsafe(frames.len() as u32));
    mp3.extend_from_slice(&frames);
    mp3.extend_from_slice(&silent_mp3_xing_frame(382)); // ~9.98 s
    mp3
}

/// Builds an ID3v1.1-only MP3 (no ID3v2 header, 128-byte TAG footer).
pub fn build_mp3_id3v1() -> Vec<u8> {
    let mut mp3 = Vec::new();
    for _ in 0..8 {
        mp3.extend_from_slice(&silent_mp3_frame());
    }
    let mut tag = [0u8; 128];
    tag[0..3].copy_from_slice(b"TAG");
    tag[3..13].copy_from_slice(b"Old track ");
    tag[33..40].copy_from_slice(b"Someone");
    tag[63..72].copy_from_slice(b"Old album");
    tag[93..97].copy_from_slice(b"1999");
    tag[125] = 0;
    tag[126] = 7; // track 7
    tag[127] = 12;
    mp3.extend_from_slice(&tag);
    mp3
}

/// Builds an untagged silent MP3 (no ID3v2, no ID3v1, valid CBR frames for duration).
pub fn build_mp3_no_tags() -> Vec<u8> {
    let mut mp3 = Vec::new();
    for _ in 0..10 {
        mp3.extend_from_slice(&silent_mp3_frame());
    }
    mp3
}

/// Builds an MP3 whose ID3v2.3 tag embeds `raw_cover` bytes as a front cover `APIC` frame.
pub fn build_mp3_with_raw_cover(title: &str, mime: &str, raw_cover: &[u8]) -> Vec<u8> {
    let mut frames = Vec::new();
    frames.extend_from_slice(&make_id3v23_text_frame(b"TIT2", 3, title));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TPE1", 3, "Someone"));
    frames.extend_from_slice(&make_id3v23_apic_frame(mime, 3, "cover", raw_cover));
    let mut mp3 = wrap_id3v23(&frames, false, false);
    mp3.extend_from_slice(&silent_mp3_xing_frame(191));
    mp3
}

/// Builds a WAV file (`fmt ` + silent `data` + optional `ID3 ` chunk).
pub fn build_wav(with_tags: bool, cover: Option<&[u8]>, duration_secs: u32) -> Vec<u8> {
    let sample_rate = 8000u32;
    let byte_rate = sample_rate * 2; // 16-bit mono
    let data_len = byte_rate * duration_secs;

    let mut body = Vec::new();
    body.extend_from_slice(b"WAVE");

    // `fmt ` chunk (16 bytes: PCM, 1ch, 8000 Hz, 16-bit)
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes()); // PCM
    body.extend_from_slice(&1u16.to_le_bytes()); // mono
    body.extend_from_slice(&sample_rate.to_le_bytes());
    body.extend_from_slice(&byte_rate.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes()); // block align
    body.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // `data` chunk (silence)
    body.extend_from_slice(b"data");
    body.extend_from_slice(&data_len.to_le_bytes());
    body.resize(body.len() + data_len as usize, 0);

    if with_tags {
        let mut frames = Vec::new();
        frames.extend_from_slice(&make_id3v23_text_frame(b"TIT2", 3, "Wave song"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TPE1", 3, "Someone"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TALB", 3, "Wave album"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TRCK", 3, "5"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TYER", 3, "2025"));
        if let Some(img) = cover {
            frames.extend_from_slice(&make_id3v23_apic_frame("image/jpeg", 3, "cover", img));
        }
        let id3 = wrap_id3v23(&frames, false, false);
        body.extend_from_slice(b"ID3 ");
        body.extend_from_slice(&(id3.len() as u32).to_le_bytes());
        body.extend_from_slice(&id3);
        if !id3.len().is_multiple_of(2) {
            body.push(0);
        }
    }

    let mut riff = Vec::with_capacity(8 + body.len());
    riff.extend_from_slice(b"RIFF");
    riff.extend_from_slice(&(body.len() as u32).to_le_bytes());
    riff.extend_from_slice(&body);
    riff
}

/// Builds an AIFF file (`COMM` with 44100 Hz IEEE-80 + silent `SSND` + optional `ID3 ` chunk).
pub fn build_aiff(with_tags: bool, cover: Option<&[u8]>, duration_secs: u32) -> Vec<u8> {
    let sample_rate = 44_100u32;
    let num_frames = sample_rate * duration_secs;
    // IEEE 754 80-bit extended representation of 44100.0:
    // exponent = 16383 + 15 = 16398 = 0x400E, mantissa = 44100 << 48 = 0xAC44_0000_0000_0000
    let sr_ieee80: [u8; 10] = [0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0];

    let mut body = Vec::new();
    body.extend_from_slice(b"AIFF");

    // `COMM` chunk (18 bytes)
    body.extend_from_slice(b"COMM");
    body.extend_from_slice(&18u32.to_be_bytes());
    body.extend_from_slice(&1u16.to_be_bytes()); // 1 channel
    body.extend_from_slice(&num_frames.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes()); // 16-bit
    body.extend_from_slice(&sr_ieee80);

    // Tiny `SSND` chunk (8 header bytes + 16 silence bytes)
    body.extend_from_slice(b"SSND");
    body.extend_from_slice(&24u32.to_be_bytes());
    body.extend_from_slice(&[0u8; 24]);

    if with_tags {
        let mut frames = Vec::new();
        frames.extend_from_slice(&make_id3v23_text_frame(b"TIT2", 3, "Aiff song"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TPE1", 3, "Someone"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TALB", 3, "Aiff album"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TRCK", 3, "6"));
        frames.extend_from_slice(&make_id3v23_text_frame(b"TYER", 3, "2023"));
        if let Some(img) = cover {
            frames.extend_from_slice(&make_id3v23_apic_frame("image/jpeg", 3, "cover", img));
        }
        let id3 = wrap_id3v23(&frames, false, false);
        body.extend_from_slice(b"ID3 ");
        body.extend_from_slice(&(id3.len() as u32).to_be_bytes());
        body.extend_from_slice(&id3);
        if !id3.len().is_multiple_of(2) {
            body.push(0);
        }
    }

    let mut form = Vec::with_capacity(8 + body.len());
    form.extend_from_slice(b"FORM");
    form.extend_from_slice(&(body.len() as u32).to_be_bytes());
    form.extend_from_slice(&body);
    form
}

pub fn build_flac_picture_payload(pic_type: u32, mime: &str, desc: &str, img: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + mime.len() + desc.len() + img.len());
    out.extend_from_slice(&pic_type.to_be_bytes());
    out.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    out.extend_from_slice(mime.as_bytes());
    out.extend_from_slice(&(desc.len() as u32).to_be_bytes());
    out.extend_from_slice(desc.as_bytes());
    out.extend_from_slice(&[0u8; 16]); // width, height, depth, colors
    out.extend_from_slice(&(img.len() as u32).to_be_bytes());
    out.extend_from_slice(img);
    out
}

pub fn build_vorbis_comment_payload(
    comments: &[(&str, &str)],
    extra_owned: &[(String, String)],
) -> Vec<u8> {
    let vendor = b"legion-test";
    let mut out = Vec::new();
    out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vendor);
    let total = (comments.len() + extra_owned.len()) as u32;
    out.extend_from_slice(&total.to_le_bytes());
    for &(k, v) in comments {
        let entry = format!("{k}={v}");
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.as_bytes());
    }
    for (k, v) in extra_owned {
        let entry = format!("{k}={v}");
        out.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        out.extend_from_slice(entry.as_bytes());
    }
    out
}

/// Builds a FLAC file (`fLaC` + `STREAMINFO` + optional `VORBIS_COMMENT` + `PICTURE` blocks).
pub fn build_flac(
    comments: &[(&str, &str)],
    pictures: &[(u32, &str, &[u8])],
    duration_secs: u32,
) -> Vec<u8> {
    let sample_rate = 44_100u32;
    let total_samples = u64::from(sample_rate) * u64::from(duration_secs);
    let mut streaminfo = [0u8; 34];
    streaminfo[10] = ((sample_rate >> 12) & 0xFF) as u8;
    streaminfo[11] = ((sample_rate >> 4) & 0xFF) as u8;
    streaminfo[12] = (((sample_rate & 0x0F) << 4) as u8) | (((total_samples >> 32) & 0x0F) as u8);
    streaminfo[13] = 0; // wait: let's match exact FLAC bit layout!
    // FLAC STREAMINFO bytes 10..18:
    // sample_rate (20 bits): bits 0..20  -> byte[10]<<12 | byte[11]<<4 | byte[12]>>4
    // channels-1  (3 bits):  bits 20..23 -> (byte[12]>>1) & 7
    // bps-1       (5 bits):  bits 23..28 -> ((byte[12]&1)<<4) | (byte[13]>>4)
    // total_samples (36 bits): bits 28..64 -> (byte[13]&0x0F)<<32 | u32::from_be_bytes(byte[14..18])
    streaminfo[12] = (((sample_rate & 0x0F) << 4) as u8) | 0x02; // 2ch, bps top bit 0
    streaminfo[13] = 0xF0 | (((total_samples >> 32) & 0x0F) as u8); // 16-bit + top 4 bits of samples
    streaminfo[14..18].copy_from_slice(&(total_samples as u32).to_be_bytes());

    let mut blocks: Vec<(u8, Vec<u8>)> = vec![(0, streaminfo.to_vec())];
    if !comments.is_empty() {
        blocks.push((4, build_vorbis_comment_payload(comments, &[])));
    }
    for &(pic_type, mime, img) in pictures {
        blocks.push((6, build_flac_picture_payload(pic_type, mime, "cover", img)));
    }

    let mut flac = b"fLaC".to_vec();
    let last_idx = blocks.len() - 1;
    for (idx, (btype, payload)) in blocks.into_iter().enumerate() {
        let len = payload.len() as u32;
        let first = if idx == last_idx { btype | 0x80 } else { btype };
        flac.push(first);
        flac.push(((len >> 16) & 0xFF) as u8);
        flac.push(((len >> 8) & 0xFF) as u8);
        flac.push((len & 0xFF) as u8);
        flac.extend_from_slice(&payload);
    }
    flac
}

fn push_ogg_packet_pages(
    out: &mut Vec<u8>,
    serial: u32,
    seq: &mut u32,
    header_type: u8,
    granule: u64,
    packet: &[u8],
) {
    // Split large packets across multiple Ogg pages (255 * 255 = 65,025 bytes, an exact multiple of
    // 255 so non-final pages contain only 255-byte lacing segments and never prematurely terminate
    // the multi-page packet).
    let max_page_payload = 255 * 255usize;
    let mut offset = 0usize;
    let mut first_page = true;
    loop {
        let remaining = packet.len() - offset;
        let chunk_len = remaining.min(max_page_payload);
        let is_final_chunk = offset + chunk_len == packet.len();
        let chunk = &packet[offset..offset + chunk_len];

        let mut lacing = Vec::new();
        let mut pos = 0usize;
        while pos + 255 <= chunk.len() {
            lacing.push(255u8);
            pos += 255;
        }
        if pos < chunk.len() {
            lacing.push((chunk.len() - pos) as u8);
        } else if is_final_chunk {
            // Packet ends cleanly on a 255-byte boundary -> terminating 0-byte lace.
            lacing.push(0u8);
        }

        let page_flags = if first_page { header_type } else { 0x01 }; // continuation
        let page_granule = if is_final_chunk { granule } else { u64::MAX };

        out.extend_from_slice(b"OggS");
        out.push(0); // version
        out.push(page_flags);
        out.extend_from_slice(&page_granule.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&(*seq).to_le_bytes());
        *seq += 1;
        out.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
        out.push(lacing.len() as u8);
        out.extend_from_slice(&lacing);
        out.extend_from_slice(chunk);

        offset += chunk_len;
        first_page = false;
        if is_final_chunk {
            break;
        }
    }
}

/// Builds an Ogg Vorbis stream with Vorbis comments, optional base64 `METADATA_BLOCK_PICTURE`
/// entries, and an EOS granule page yielding `duration_secs`.
pub fn build_ogg_vorbis(
    comments: &[(&str, &str)],
    pictures: &[(u32, &str, &[u8])],
    duration_secs: u32,
) -> Vec<u8> {
    let serial = 0x1122_3344u32;
    let mut seq = 0u32;
    let mut out = Vec::new();

    // Identification header (30 bytes): `\x01vorbis`, version 0, 2ch, 44100 Hz, bitrates, blocksize, framing=1.
    let mut id_pkt = vec![0x01];
    id_pkt.extend_from_slice(b"vorbis");
    id_pkt.extend_from_slice(&0u32.to_le_bytes());
    id_pkt.push(2);
    id_pkt.extend_from_slice(&44_100u32.to_le_bytes());
    id_pkt.extend_from_slice(&[0u8; 13]);
    id_pkt.push(1); // framing flag (byte 29 -> total 30 bytes)
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x02, 0, &id_pkt);

    // Comment header: `\x03vorbis` + Vorbis comments + framing bit.
    let extra: Vec<(String, String)> = pictures
        .iter()
        .map(|&(pic_type, mime, img)| {
            let block = build_flac_picture_payload(pic_type, mime, "cover", img);
            ("METADATA_BLOCK_PICTURE".to_owned(), BASE64.encode(&block))
        })
        .collect();
    let mut comment_pkt = vec![0x03];
    comment_pkt.extend_from_slice(b"vorbis");
    comment_pkt.extend_from_slice(&build_vorbis_comment_payload(comments, &extra));
    comment_pkt.push(1); // framing bit
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x00, 0, &comment_pkt);

    // Final audio EOS page with granule position = duration_secs * 44100.
    let granule = u64::from(duration_secs) * 44_100;
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x04, granule, &[0u8; 32]);
    out
}

/// Builds an Ogg Opus stream (`OpusHead` + `OpusTags` + EOS granule page).
pub fn build_ogg_opus(
    comments: &[(&str, &str)],
    pictures: &[(u32, &str, &[u8])],
    duration_secs: u32,
) -> Vec<u8> {
    let serial = 0x5566_7788u32;
    let pre_skip = 312u16;
    let mut seq = 0u32;
    let mut out = Vec::new();

    // `OpusHead` (19 bytes): magic(8), version(1), ch(1), pre_skip(2), sample_rate(4), gain(2), map(1)
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(2);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes());
    head.push(0);
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x02, 0, &head);

    let extra: Vec<(String, String)> = pictures
        .iter()
        .map(|&(pic_type, mime, img)| {
            let block = build_flac_picture_payload(pic_type, mime, "cover", img);
            ("METADATA_BLOCK_PICTURE".to_owned(), BASE64.encode(&block))
        })
        .collect();
    let mut tags_pkt = b"OpusTags".to_vec();
    tags_pkt.extend_from_slice(&build_vorbis_comment_payload(comments, &extra));
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x00, 0, &tags_pkt);

    let granule = u64::from(duration_secs) * 48_000 + u64::from(pre_skip);
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x04, granule, &[0u8; 32]);
    out
}

/// Builds an Ogg Opus file from raw byte `(key, value)` Vorbis comments so tests can embed
/// arbitrarily large line-wrapped base64 pictures or raw invalid UTF-8 bytes.
pub fn build_ogg_opus_raw_comments(entries: &[(&[u8], &[u8])], duration_secs: u32) -> Vec<u8> {
    let serial = 0x5566_7799u32;
    let pre_skip = 312u16;
    let mut seq = 0u32;
    let mut out = Vec::new();

    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(2);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes());
    head.push(0);
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x02, 0, &head);

    let vendor = b"legion-test";
    let total_cap: usize = 16
        + vendor.len()
        + entries
            .iter()
            .map(|(k, v)| 4 + k.len() + 1 + v.len())
            .sum::<usize>();
    let mut tags_pkt = Vec::with_capacity(total_cap);
    tags_pkt.extend_from_slice(b"OpusTags");
    tags_pkt.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    tags_pkt.extend_from_slice(vendor);
    tags_pkt.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for &(k, v) in entries {
        let len = (k.len() + 1 + v.len()) as u32;
        tags_pkt.extend_from_slice(&len.to_le_bytes());
        tags_pkt.extend_from_slice(k);
        tags_pkt.push(b'=');
        tags_pkt.extend_from_slice(v);
    }
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x00, 0, &tags_pkt);

    let granule = u64::from(duration_secs) * 48_000 + u64::from(pre_skip);
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x04, granule, &[0u8; 32]);
    out
}

fn mp4_box(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let len = (8 + payload.len()) as u32;
    let mut out = Vec::with_capacity(len as usize);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(payload);
    out
}

fn mp4_ilst_text(atom: &[u8; 4], text: &str) -> Vec<u8> {
    let mut data_payload = Vec::with_capacity(8 + text.len());
    data_payload.extend_from_slice(&1u32.to_be_bytes()); // UTF-8
    data_payload.extend_from_slice(&0u32.to_be_bytes());
    data_payload.extend_from_slice(text.as_bytes());
    let data = mp4_box(b"data", &data_payload);
    mp4_box(atom, &data)
}

fn mp4_ilst_pair(atom: &[u8; 4], num: u16, total: u16) -> Vec<u8> {
    let mut data_payload = Vec::with_capacity(16);
    data_payload.extend_from_slice(&0u32.to_be_bytes());
    data_payload.extend_from_slice(&0u32.to_be_bytes());
    data_payload.extend_from_slice(&0u16.to_be_bytes());
    data_payload.extend_from_slice(&num.to_be_bytes());
    data_payload.extend_from_slice(&total.to_be_bytes());
    data_payload.extend_from_slice(&0u16.to_be_bytes());
    let data = mp4_box(b"data", &data_payload);
    mp4_box(atom, &data)
}

/// Builds an MP4 / M4A / M4B container with `ftyp`, `moov` (`mvhd`, optional audio/video `trak`,
/// optional `udta/meta/ilst` items & `covr`), and a tiny silent `mdat`.
#[allow(clippy::too_many_arguments)]
pub fn build_mp4(
    with_tags: bool,
    cover: Option<(u32, &[u8])>,
    duration_secs: u32,
    mvhd_v1: bool,
    with_video_track: bool,
) -> Vec<u8> {
    let ftyp = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A isom");

    let mvhd = if mvhd_v1 {
        let mut p = Vec::new();
        p.extend_from_slice(&[1, 0, 0, 0]); // version 1
        p.extend_from_slice(&0u64.to_be_bytes()); // creation
        p.extend_from_slice(&0u64.to_be_bytes()); // modification
        p.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        p.extend_from_slice(&(u64::from(duration_secs) * 1000).to_be_bytes());
        p.resize(p.len() + 80, 0);
        mp4_box(b"mvhd", &p)
    } else {
        let mut p = Vec::new();
        p.extend_from_slice(&[0, 0, 0, 0]); // version 0
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&0u32.to_be_bytes());
        p.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        p.extend_from_slice(&(duration_secs * 1000).to_be_bytes());
        p.resize(p.len() + 80, 0);
        mp4_box(b"mvhd", &p)
    };

    let handler = if with_video_track { b"vide" } else { b"soun" };
    let mut hdlr_payload = vec![0u8; 24];
    hdlr_payload[8..12].copy_from_slice(handler);
    let hdlr = mp4_box(b"hdlr", &hdlr_payload);
    let mdia = mp4_box(b"mdia", &hdlr);
    let trak = mp4_box(b"trak", &mdia);

    let mut moov_body = Vec::new();
    moov_body.extend_from_slice(&mvhd);
    moov_body.extend_from_slice(&trak);

    if with_tags || cover.is_some() {
        let mut ilst_body = Vec::new();
        if with_tags {
            ilst_body.extend_from_slice(&mp4_ilst_text(b"\xA9nam", "First song"));
            ilst_body.extend_from_slice(&mp4_ilst_text(b"\xA9ART", "Someone"));
            ilst_body.extend_from_slice(&mp4_ilst_text(b"\xA9alb", "First album"));
            ilst_body.extend_from_slice(&mp4_ilst_text(b"aART", "Album artist"));
            ilst_body.extend_from_slice(&mp4_ilst_pair(b"trkn", 1, 12));
            ilst_body.extend_from_slice(&mp4_ilst_pair(b"disk", 1, 1));
            ilst_body.extend_from_slice(&mp4_ilst_text(b"\xA9day", "2025-03-01"));
        }
        if let Some((type_code, img)) = cover {
            let mut dp = Vec::with_capacity(8 + img.len());
            dp.extend_from_slice(&type_code.to_be_bytes()); // 13=JPEG, 14=PNG
            dp.extend_from_slice(&0u32.to_be_bytes());
            dp.extend_from_slice(img);
            let data = mp4_box(b"data", &dp);
            ilst_body.extend_from_slice(&mp4_box(b"covr", &data));
        }
        let ilst = mp4_box(b"ilst", &ilst_body);
        let mut meta_body = vec![0u8; 4]; // FullBox version+flags
        meta_body.extend_from_slice(&ilst);
        let meta = mp4_box(b"meta", &meta_body);
        let udta = mp4_box(b"udta", &meta);
        moov_body.extend_from_slice(&udta);
    }

    let moov = mp4_box(b"moov", &moov_body);
    let mdat = mp4_box(b"mdat", &[0u8; 64]);

    let mut out = Vec::with_capacity(ftyp.len() + moov.len() + mdat.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out.extend_from_slice(&mdat);
    out
}

/// Builds a tiny 25000x25000 PNG decompression bomb (> 256 MiB estimated decode peak).
pub fn build_png_decompression_bomb() -> Vec<u8> {
    let crc32 = |bytes: &[u8]| -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    };
    let mut data = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut ihdr = b"IHDR".to_vec();
    ihdr.extend_from_slice(&25_000u32.to_be_bytes());
    ihdr.extend_from_slice(&25_000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    data.extend_from_slice(&13u32.to_be_bytes());
    data.extend_from_slice(&ihdr);
    data.extend_from_slice(&crc32(&ihdr).to_be_bytes());
    let idat_payload = [
        0x78u8, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01,
    ];
    let mut idat = b"IDAT".to_vec();
    idat.extend_from_slice(&idat_payload);
    data.extend_from_slice(&(idat_payload.len() as u32).to_be_bytes());
    data.extend_from_slice(&idat);
    data.extend_from_slice(&crc32(&idat).to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(b"IEND");
    data.extend_from_slice(&crc32(b"IEND").to_be_bytes());
    data
}

/// Builds an MP3 whose ID3v2 header declares 20 MiB of tag data (> 16 MiB cap).
pub fn build_oversized_id3v2_mp3() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"ID3");
    out.extend_from_slice(&[3, 0, 0]);
    out.extend_from_slice(&encode_syncsafe(20 * 1024 * 1024));
    out.extend_from_slice(&[0u8; 64]);
    out
}

/// Builds an MP3 with hostile text fields: control characters, ANSI escapes, NULs, > 512 chars,
/// and a whitespace+control-only artist that must be omitted rather than returned empty.
pub fn build_hostile_text_mp3() -> Vec<u8> {
    let long_title = format!("  \x1b[31mFirst\x00\x07 song\n\t  {}", "a".repeat(600));
    let mut frames = Vec::new();
    frames.extend_from_slice(&make_id3v23_text_frame(b"TIT2", 3, &long_title));
    frames.extend_from_slice(&make_id3v23_text_frame(b"TPE1", 3, " \x00\x07\x1b\r\n\t "));
    frames.extend_from_slice(&make_id3v23_text_frame(
        b"TALB",
        3,
        "  Clean\x0bAlbum\x7f  ",
    ));
    let mut mp3 = wrap_id3v23(&frames, false, false);
    mp3.extend_from_slice(&silent_mp3_frame());
    mp3
}

/// Builds an MP3 ID3v2.3 file with `count` tiny frames (used to exceed `MAX_ID3_FRAMES`).
pub fn build_mp3_many_id3_frames(count: usize) -> Vec<u8> {
    let mut frames = Vec::with_capacity(64 + count * 12);
    frames.extend_from_slice(&make_id3v23_text_frame(b"TIT2", 3, "Before frame cap"));
    for _ in 0..count {
        frames.extend_from_slice(b"TXXX");
        frames.extend_from_slice(&2u32.to_be_bytes());
        frames.extend_from_slice(&[0, 0, 3, 0]);
    }
    frames.extend_from_slice(&make_id3v23_text_frame(b"TPE1", 3, "After frame cap"));
    let mut mp3 = wrap_id3v23(&frames, false, false);
    mp3.extend_from_slice(&silent_mp3_frame());
    mp3
}

/// Builds a WAV file with `count` empty `JUNK` chunks (used to exceed `MAX_CONTAINER_CHUNKS`).
pub fn build_wav_many_chunks(count: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + 24 + count * 8);
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&16_000u32.to_le_bytes());
    body.extend_from_slice(&2u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    for _ in 0..count {
        body.extend_from_slice(b"JUNK");
        body.extend_from_slice(&0u32.to_le_bytes());
    }
    let mut wav = Vec::with_capacity(8 + body.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wav.extend_from_slice(&body);
    wav
}

/// Builds an AIFF file with `count` empty `JUNK` chunks (used to exceed `MAX_CONTAINER_CHUNKS`).
pub fn build_aiff_many_chunks(count: usize) -> Vec<u8> {
    let mut body = Vec::with_capacity(4 + 26 + count * 8);
    body.extend_from_slice(b"AIFF");
    body.extend_from_slice(b"COMM");
    body.extend_from_slice(&18u32.to_be_bytes());
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&44_100u32.to_be_bytes());
    body.extend_from_slice(&16u16.to_be_bytes());
    body.extend_from_slice(&[0x40, 0x0E, 0xAC, 0x44, 0, 0, 0, 0, 0, 0]);
    for _ in 0..count {
        body.extend_from_slice(b"JUNK");
        body.extend_from_slice(&0u32.to_be_bytes());
    }
    let mut aiff = Vec::with_capacity(8 + body.len());
    aiff.extend_from_slice(b"FORM");
    aiff.extend_from_slice(&(body.len() as u32).to_be_bytes());
    aiff.extend_from_slice(&body);
    aiff
}

/// Builds a FLAC file with `count` zero-length `PADDING` blocks (exceeds `MAX_CONTAINER_CHUNKS`).
pub fn build_flac_many_blocks(count: usize) -> Vec<u8> {
    let mut flac = Vec::with_capacity(4 + 38 + count * 4);
    flac.extend_from_slice(b"fLaC");
    let mut streaminfo = [0u8; 34];
    streaminfo[10] = 0x0A;
    streaminfo[11] = 0xC4;
    streaminfo[12] = 0x42;
    streaminfo[13] = 0xF0;
    streaminfo[14..18].copy_from_slice(&44_100u32.to_be_bytes());
    flac.extend_from_slice(&[0x00, 0, 0, 34]);
    flac.extend_from_slice(&streaminfo);
    for i in 0..count {
        let header = if i + 1 == count { 0x81 } else { 0x01 };
        flac.extend_from_slice(&[header, 0, 0, 0]);
    }
    flac
}

/// Builds a FLAC file whose `VORBIS_COMMENT` block contains `count` tiny entries (exceeds
/// `MAX_VORBIS_COMMENTS`).
pub fn build_flac_many_vorbis_comments(count: usize) -> Vec<u8> {
    let mut vc = Vec::with_capacity(32 + count * 7);
    let vendor = b"legion-test";
    vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    vc.extend_from_slice(vendor);
    vc.extend_from_slice(&((count + 1) as u32).to_le_bytes());
    let title = b"TITLE=Many comments";
    vc.extend_from_slice(&(title.len() as u32).to_le_bytes());
    vc.extend_from_slice(title);
    for _ in 0..count {
        vc.extend_from_slice(&3u32.to_le_bytes());
        vc.extend_from_slice(b"X=1");
    }

    let mut flac = Vec::with_capacity(4 + 38 + 4 + vc.len());
    flac.extend_from_slice(b"fLaC");
    let mut streaminfo = [0u8; 34];
    streaminfo[10] = 0x0A;
    streaminfo[11] = 0xC4;
    streaminfo[12] = 0x42;
    streaminfo[13] = 0xF0;
    streaminfo[14..18].copy_from_slice(&44_100u32.to_be_bytes());
    flac.extend_from_slice(&[0x00, 0, 0, 34]);
    flac.extend_from_slice(&streaminfo);
    let len = vc.len() as u32;
    flac.extend_from_slice(&[
        0x84,
        ((len >> 16) & 0xFF) as u8,
        ((len >> 8) & 0xFF) as u8,
        (len & 0xFF) as u8,
    ]);
    flac.extend_from_slice(&vc);
    flac
}

/// Builds an Ogg file with `count` non-Vorbis/Opus BOS pages (exceeds `MAX_OGG_SCAN_PAGES`).
pub fn build_ogg_many_scan_pages(count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count * 36);
    for i in 0..count {
        let mut seq = 0u32;
        push_ogg_packet_pages(&mut out, 100 + i as u32, &mut seq, 0x02, 0, b"NOTAUDIO");
    }
    out
}

/// Builds an Ogg Opus file whose comment packet spans `count` 255-byte continuation pages
/// (exceeds `MAX_OGG_COMMENT_PAGES` while staying well under the 16 MiB tag budget).
pub fn build_ogg_many_comment_pages(count: usize) -> Vec<u8> {
    let serial = 0x2468_1357u32;
    let mut seq = 0u32;
    let mut out = Vec::with_capacity(128 + count * (28 + 255));

    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(2);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0u16.to_le_bytes());
    head.push(0);
    push_ogg_packet_pages(&mut out, serial, &mut seq, 0x02, 0, &head);

    let mut chunk = [b'A'; 255];
    for page_idx in 0..count {
        if page_idx == 0 {
            chunk[..8].copy_from_slice(b"OpusTags");
        } else if page_idx == 1 {
            chunk[..8].copy_from_slice(b"AAAAAAAA");
        }
        let page_flags = if page_idx == 0 { 0x00 } else { 0x01 };
        out.extend_from_slice(b"OggS");
        out.push(0);
        out.push(page_flags);
        out.extend_from_slice(&u64::MAX.to_le_bytes());
        out.extend_from_slice(&serial.to_le_bytes());
        out.extend_from_slice(&seq.to_le_bytes());
        seq += 1;
        out.extend_from_slice(&0u32.to_le_bytes());
        out.push(1);
        out.push(255); // continues on next page
        out.extend_from_slice(&chunk);
    }
    out
}

/// Builds an MP4 file with `depth` nested `udta` boxes inside `moov` (exceeds `MAX_MP4_DEPTH`).
pub fn build_mp4_deep_udta(depth: usize) -> Vec<u8> {
    let ftyp = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A isom");
    let ilst = mp4_box(b"ilst", &mp4_ilst_text(b"\xA9nam", "Deep title"));
    let mut meta_body = vec![0u8; 4];
    meta_body.extend_from_slice(&ilst);
    let mut inner = mp4_box(b"meta", &meta_body);
    for _ in 0..depth {
        inner = mp4_box(b"udta", &inner);
    }
    let moov = mp4_box(b"moov", &inner);
    let mut out = Vec::with_capacity(ftyp.len() + moov.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out
}

/// Builds an MP4 file with `count` 8-byte `free` boxes (exceeds `MAX_CONTAINER_CHUNKS`).
pub fn build_mp4_many_boxes(count: usize) -> Vec<u8> {
    let ftyp = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A isom");
    let mut out = Vec::with_capacity(ftyp.len() + count * 8);
    out.extend_from_slice(&ftyp);
    for _ in 0..count {
        out.extend_from_slice(&8u32.to_be_bytes());
        out.extend_from_slice(b"free");
    }
    out
}

/// Builds an Ogg Opus file with a ~15.9 MiB line-wrapped base64 `METADATA_BLOCK_PICTURE` strictly
/// within the 16 MiB tag budget.
pub fn build_opus_15_9mib_wrapped_base64_picture() -> Vec<u8> {
    let mut padded_png = encode_solid_png(128, 128, [30, 170, 80]);
    padded_png.resize(11_600_000, 0x00);
    let flac_pic = build_flac_picture_payload(3, "image/png", "cover", &padded_png);
    let raw_b64 = BASE64.encode(&flac_pic);
    let mut wrapped_b64 = Vec::with_capacity(raw_b64.len() + raw_b64.len() / 76 * 2 + 4);
    for chunk in raw_b64.as_bytes().chunks(76) {
        wrapped_b64.extend_from_slice(chunk);
        wrapped_b64.extend_from_slice(b"\r\n");
    }
    assert!(
        wrapped_b64.len() > 15_800_000 && wrapped_b64.len() < 16 * 1024 * 1024 - 1024,
        "expected ~15.9 MiB line-wrapped base64, got {} bytes",
        wrapped_b64.len()
    );
    build_ogg_opus_raw_comments(
        &[
            (b"TITLE", b"Wrapped cover song"),
            (b"ARTIST", b"Someone"),
            (b"METADATA_BLOCK_PICTURE", &wrapped_b64),
        ],
        5,
    )
}

/// Builds an ID3v2.4 MP3 carrying raw `raw_title` bytes in a UTF-8 `TIT2` frame plus `"Someone"`
/// in `TPE1`.
pub fn build_mp3_id3v24_raw_title(raw_title: &[u8]) -> Vec<u8> {
    let mut frames = Vec::with_capacity(raw_title.len() + 64);
    frames.extend_from_slice(b"TIT2");
    frames.extend_from_slice(&encode_syncsafe((1 + raw_title.len()) as u32));
    frames.extend_from_slice(&[0x00, 0x00, 0x03]); // UTF-8
    frames.extend_from_slice(raw_title);
    frames.extend_from_slice(b"TPE1");
    frames.extend_from_slice(&encode_syncsafe(8));
    frames.extend_from_slice(&[0x00, 0x00, 0x03]);
    frames.extend_from_slice(b"Someone");

    let mut mp3 = Vec::with_capacity(10 + frames.len() + 417);
    mp3.extend_from_slice(b"ID3");
    mp3.extend_from_slice(&[4, 0, 0]);
    mp3.extend_from_slice(&encode_syncsafe(frames.len() as u32));
    mp3.extend_from_slice(&frames);
    mp3.extend_from_slice(&silent_mp3_xing_frame(191));
    mp3
}

/// Builds an M4A carrying raw `raw_title` bytes in a UTF-8 `©nam` atom plus `"Someone"` in `©ART`.
pub fn build_m4a_raw_title(raw_title: &[u8]) -> Vec<u8> {
    let ftyp = mp4_box(b"ftyp", b"M4A \x00\x00\x00\x00M4A isom");
    let mut nam_data = Vec::with_capacity(8 + raw_title.len());
    nam_data.extend_from_slice(&1u32.to_be_bytes());
    nam_data.extend_from_slice(&0u32.to_be_bytes());
    nam_data.extend_from_slice(raw_title);
    let nam = mp4_box(b"\xA9nam", &mp4_box(b"data", &nam_data));
    let art = mp4_ilst_text(b"\xA9ART", "Someone");
    let mut ilst_body = Vec::with_capacity(nam.len() + art.len());
    ilst_body.extend_from_slice(&nam);
    ilst_body.extend_from_slice(&art);
    let ilst = mp4_box(b"ilst", &ilst_body);
    let mut meta_body = vec![0u8; 4];
    meta_body.extend_from_slice(&ilst);
    let moov = mp4_box(b"moov", &mp4_box(b"udta", &mp4_box(b"meta", &meta_body)));
    let mut out = Vec::with_capacity(ftyp.len() + moov.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    out
}

/// Builds a global-unsynced ID3v2.3 MP3 whose tag body has exact size `tag_bytes`.
pub fn build_mp3_global_unsync_sized(tag_bytes: usize) -> Vec<u8> {
    let title = make_id3v23_text_frame(b"TIT2", 3, "Sized unsync song");
    assert!(tag_bytes > title.len() + 26);
    let priv_raw_len = tag_bytes - title.len() - 10;
    // Inject two `0xFF 0x00` unsync pairs so `deunsync_in_place` compacts 2 bytes; declare the
    // post-deunsync payload length in the PRIV frame header so the de-unsynced tag parses cleanly.
    let priv_deunsynced_len = priv_raw_len - 2;
    let mut body = Vec::with_capacity(tag_bytes);
    body.extend_from_slice(&title);
    body.extend_from_slice(b"PRIV");
    body.extend_from_slice(&(priv_deunsynced_len as u32).to_be_bytes());
    body.extend_from_slice(&[0, 0]);
    body.resize(tag_bytes, b'Z');
    let base = title.len() + 10;
    body[base..base + 6].copy_from_slice(&[0xFF, 0x00, 0xE0, 0xFF, 0x00, 0x00]);

    let mut mp3 = Vec::with_capacity(10 + body.len() + 417);
    mp3.extend_from_slice(b"ID3");
    mp3.extend_from_slice(&[3, 0, 0x80]); // global unsync flag
    mp3.extend_from_slice(&encode_syncsafe(body.len() as u32));
    mp3.extend_from_slice(&body);
    mp3.extend_from_slice(&silent_mp3_frame());
    mp3
}

/// Builds a FLAC file with `pic_count` `PICTURE` blocks each carrying `each_bytes` of PNG-prefixed
/// payload (used to reach just below and just above the 16 MiB cumulative tag budget).
pub fn build_flac_cumulative_pictures(pic_count: usize, each_bytes: usize) -> Vec<u8> {
    let mut img = encode_solid_png(32, 32, [40, 120, 210]);
    img.resize(each_bytes.max(img.len()), 0);
    let pic_payload = build_flac_picture_payload(3, "image/png", "cover", &img);
    assert!(pic_payload.len() <= 0x00FF_FFFF);

    let mut flac = Vec::with_capacity(4 + 38 + pic_count * (4 + pic_payload.len()));
    flac.extend_from_slice(b"fLaC");
    let mut streaminfo = [0u8; 34];
    streaminfo[10] = 0x0A;
    streaminfo[11] = 0xC4;
    streaminfo[12] = 0x42;
    streaminfo[13] = 0xF0;
    streaminfo[14..18].copy_from_slice(&44_100u32.to_be_bytes());
    flac.extend_from_slice(&[0x00, 0, 0, 34]);
    flac.extend_from_slice(&streaminfo);

    let len = pic_payload.len() as u32;
    for i in 0..pic_count {
        let hdr = if i + 1 == pic_count { 0x86 } else { 0x06 };
        flac.extend_from_slice(&[
            hdr,
            ((len >> 16) & 0xFF) as u8,
            ((len >> 8) & 0xFF) as u8,
            (len & 0xFF) as u8,
        ]);
        flac.extend_from_slice(&pic_payload);
    }
    flac
}

/// Builds an MP4 file with a `covr` atom carrying `cover_bytes` of PNG-prefixed data.
pub fn build_mp4_sized_covr(cover_bytes: usize) -> Vec<u8> {
    let mut img = encode_solid_png(32, 32, [210, 80, 40]);
    img.resize(cover_bytes.max(img.len()), 0);
    build_mp4(true, Some((14, &img)), 4, false, false)
}

/// Verifies that a single `fs.tags` batch result item is either a valid cleaned `ok` object or a
/// clean structured `error` object.
pub fn assert_clean_tags_item(item: &serde_json::Value) {
    let has_ok = item.get("ok").is_some();
    let has_err = item.get("error").is_some();
    assert!(
        has_ok ^ has_err,
        "result item must have exactly one of ok/error: {item}"
    );
    if let Some(ok) = item.get("ok") {
        for field in ["title", "artist", "album", "album_artist"] {
            if let Some(val) = ok.get(field) {
                let text = val
                    .as_str()
                    .unwrap_or_else(|| panic!("{field} must be string: {ok}"));
                assert!(!text.is_empty(), "{field} must not be empty: {ok}");
                assert!(
                    text.chars().count() <= 512,
                    "{field} exceeded 512 chars ({}): {ok}",
                    text.chars().count()
                );
                let first = text.chars().next().unwrap();
                let last = text.chars().next_back().unwrap();
                assert!(
                    !first.is_whitespace() && first != '\u{200C}' && first != '\u{200D}',
                    "{field} has leading edge char {first:?}: {ok}"
                );
                assert!(
                    !last.is_whitespace() && last != '\u{200C}' && last != '\u{200D}',
                    "{field} has trailing edge char {last:?}: {ok}"
                );
                for ch in text.chars() {
                    let cp = ch as u32;
                    let stripped = ch.is_control()
                        || matches!(
                            ch,
                            '\u{00AD}'
                                | '\u{061C}'
                                | '\u{200B}'
                                | '\u{200E}'..='\u{200F}'
                                | '\u{2028}'..='\u{202E}'
                                | '\u{2066}'..='\u{2069}'
                                | '\u{FEFF}'
                                | '\u{FDD0}'..='\u{FDEF}'
                                | '\u{E0000}'..='\u{E007F}'
                        )
                        || (cp & 0xFFFF) >= 0xFFFE;
                    assert!(
                        !stripped,
                        "{field} leaked disallowed character U+{cp:04X}: {ok}"
                    );
                }
            }
        }
        if let Some(dur) = ok.get("duration_s") {
            let d = dur.as_f64().unwrap();
            assert!(d.is_finite() && d > 0.0, "invalid duration_s: {ok}");
        }
        if let Some(year) = ok.get("year") {
            let y = year.as_u64().unwrap();
            assert!((1..=9999).contains(&y), "invalid year: {ok}");
        }
    } else if let Some(err) = item.get("error") {
        let code = err["code"].as_str().unwrap_or("");
        assert_eq!(code, "failed", "unexpected fs.tags error code: {item}");
        assert!(
            !err["message"].as_str().unwrap_or("").is_empty(),
            "empty error message: {item}"
        );
    }
}
