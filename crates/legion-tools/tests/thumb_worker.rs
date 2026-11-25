//! End-to-end tests for out-of-process `__thumb-worker` isolation, lying-header defense,
//! zero-parent-read large batches, worker memory-limit kills + negative bad-file caching, worker
//! timeout enforcement, hardened `ffmpeg` limits/dref pinning, and `.ts` MPEG-TS sniffing.
//!
//! Uses `harness = false` so this test binary dispatches `__thumb-worker` before Tokio starts,
//! exercising the exact same worker code path as the `legion` binary.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "support/audio_fixtures.rs"]
mod audio_fixtures;

use std::fs;
use std::io::Write as _;
use std::process::{ExitCode, Stdio};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicU64;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use image::codecs::bmp::BmpEncoder;
use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, Frame, ImageBuffer, ImageEncoder, Luma, LumaA, Rgb, Rgba};
use legion_core::{Call, Door, Node, Paths};
use serde_json::json;

#[global_allocator]
static ALLOCATOR: legion_os::capped_alloc::CappedAlloc =
    legion_os::capped_alloc::CappedAlloc::new();

struct TimeoutGuard;

impl Drop for TimeoutGuard {
    fn drop(&mut self) {
        legion_tools::fs::set_thumb_decode_timeout_for_test(None);
        legion_tools::fs::set_thumb_timeout_cpu_busy_for_test(None);
    }
}

fn main() -> ExitCode {
    legion_core::record_current_exe();
    if let Some(code) = legion_tools::fs::try_thumb_worker_cli(std::env::args_os().skip(1)) {
        return code;
    }
    legion_tools::fs::enable_thumb_worker_self();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    runtime.block_on(async {
        test_measured_baseline_and_honest_12mp_never_false_killed().await;
        test_honest_corpus_all_formats_extremes_sizes_and_fits().await;
        test_lying_headers_and_sibling_survival().await;
        test_32_large_files_do_not_inflate_parent_rss().await;
        test_worker_memory_kill_caches_bad_file_and_keeps_siblings().await;
        test_worker_timeout_kills_slow_decode().await;
        test_mpeg_ts_sniffing_in_find_and_thumb().await;
        test_ffmpeg_blocks_dref_and_pixel_bomb().await;
        test_video_suite_1080p_4k_hevc10_8k_longgop_and_portrait_rotation().await;
        #[cfg(target_os = "linux")]
        test_external_sigkill_and_sigterm_do_not_poison_bad_cache().await;
        #[cfg(unix)]
        test_fifo_swap_race_never_hangs_any_fs_or_xfer_tool().await;
        test_gif_many_extension_blocks_and_32_parallel_workers_stay_within_budget().await;
        test_worker_cli_rejects_absurd_args_without_allocating().await;
        test_audio_tags_and_covers_through_real_worker_and_benchmarks().await;
        test_reply_budget_real_worker_node_memory_and_completion().await;
    });

    ExitCode::SUCCESS
}

fn encode_solid_jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(width, height, Rgb(rgb));
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 85)
        .encode(&img, width, height, ExtendedColorType::Rgb8)
        .expect("encode jpeg");
    out
}

fn minimal_progressive_444_jpeg_body(width: u16, height: u16) -> Vec<u8> {
    // Valid progressive 4:4:4 JPEG tables + SOF2 + minimal SOS/EOI (small file that makes
    // zune-jpeg allocate ~9 B/px of coefficient + RGB buffers when decoded).
    let mut out = Vec::new();
    // DQT
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 67, 0x00]);
    out.extend_from_slice(&[16u8; 64]);
    // SOF2 (progressive, 3 components, 1x1 sampling = 4:4:4)
    out.extend_from_slice(&[0xFF, 0xC2, 0x00, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(3);
    out.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
    // Minimal DHT (DC table 0 with 1 symbol = 0)
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x00]);
    out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    // Minimal DHT (AC table 0 with 1 symbol = 0)
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x10]);
    out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    // SOS (DC scan for all 3 components)
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 12, 3, 1, 0, 2, 0, 3, 0, 0, 0, 0]);
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn build_lie444_jpeg() -> Vec<u8> {
    let mut out = vec![
        0xFF, 0xD8, 0xFF, 0xD0, 0x00, 0x0F, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x10,
        0x01, 0x01, 0x11, 0x00,
    ];
    out.extend_from_slice(&minimal_progressive_444_jpeg_body(9000, 9000));
    out
}

fn build_lie_gif(frame_w: u16, frame_h: u16) -> Vec<u8> {
    let mut gif = Vec::new();
    gif.extend_from_slice(b"GIF89a");
    gif.extend_from_slice(&64u16.to_le_bytes());
    gif.extend_from_slice(&64u16.to_le_bytes());
    gif.extend_from_slice(&[0x80, 0x00, 0x00]);
    gif.extend_from_slice(&[0x00; 6]);
    gif.push(0x2C);
    gif.extend_from_slice(&0u16.to_le_bytes());
    gif.extend_from_slice(&0u16.to_le_bytes());
    gif.extend_from_slice(&frame_w.to_le_bytes());
    gif.extend_from_slice(&frame_h.to_le_bytes());
    gif.push(0x00);
    gif.extend_from_slice(&[0x02, 0x00, 0x3B]);
    gif
}

fn build_lie_webp(frame_w: u16, frame_h: u16) -> Vec<u8> {
    let mut vp8 = vec![0x90, 0x01, 0x00, 0x9D, 0x01, 0x2A];
    vp8.extend_from_slice(&frame_w.to_le_bytes());
    vp8.extend_from_slice(&frame_h.to_le_bytes());
    vp8.extend_from_slice(&[0u8; 16]);

    let mut body = Vec::new();
    body.extend_from_slice(b"WEBP");
    body.extend_from_slice(b"VP8X");
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(&[0, 0, 0, 0]);
    body.extend_from_slice(&[63, 0, 0]);
    body.extend_from_slice(&[63, 0, 0]);
    body.extend_from_slice(b"VP8 ");
    body.extend_from_slice(&(vp8.len() as u32).to_le_bytes());
    body.extend_from_slice(&vp8);

    let mut webp = Vec::new();
    webp.extend_from_slice(b"RIFF");
    webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
    webp.extend_from_slice(&body);
    webp
}

/// Builds a JPEG whose header intentionally exploits a parser differential between `scan_jpeg_metadata`
/// (which skips an APP1 segment hiding the real 9000x9000 progressive 4:4:4 SOF2, seeing only a
/// trailing 16x16 baseline SOF0) and `zune-jpeg` (which treats `FF D0 00 06` as a 6-byte skip,
/// landing directly on the real 9000x9000 progressive 4:4:4 SOF2).
///
/// The header probe therefore estimates ~247 MiB (baseline 9000x9000 <= 256 MiB cap) and launches
/// `__thumb-worker`, where `zune-jpeg` tries to allocate ~730 MiB and is immediately killed by the
/// OS memory limit / RSS watchdog!
fn build_parser_diff_oom_jpeg() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xFF, 0xD8]);
    // DQT + DHT before the differential so zune-jpeg has valid tables ready.
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 67, 0x00]);
    out.extend_from_slice(&[16u8; 64]);
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x00]);
    out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x10]);
    out.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

    // Real 9000x9000 progressive 4:4:4 SOF2 (19 bytes).
    let mut real_sof2 = vec![0xFF, 0xC2, 0x00, 17, 8];
    real_sof2.extend_from_slice(&9000u16.to_be_bytes());
    real_sof2.extend_from_slice(&9000u16.to_be_bytes());
    real_sof2.push(3);
    real_sof2.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
    assert_eq!(real_sof2.len(), 19);

    // `FF D0 00 06` makes zune-jpeg skip 6 bytes (`00 06` + `FF E1 00 15`) and land on `real_sof2`.
    // Meanwhile `scan_jpeg_metadata` treats `FF D0` as standalone, enters `FF E1` with length 21
    // (`2 + 19`), and skips right over `real_sof2` onto the decoy baseline SOF0 below.
    out.extend_from_slice(&[0xFF, 0xD0, 0x00, 0x06]);
    out.extend_from_slice(&[0xFF, 0xE1, 0x00, 21]);
    out.extend_from_slice(&real_sof2);

    // Decoy 16x16 baseline 1-comp SOF0 seen by `scan_jpeg_metadata`, skipped by zune-jpeg's SOS
    // only after we hide it inside a comment? Even simpler: put SOS right after a tiny APP0? Wait:
    // zune-jpeg will see the decoy SOF0 if it's plain! Put a 2-byte `FF D1 00 0F` before decoy SOF0
    // so zune-jpeg skips over the decoy SOF0 too and keeps the 9000x9000 SOF2 active!
    out.extend_from_slice(&[0xFF, 0xD1, 0x00, 0x0F]);
    out.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00,
    ]);

    // SOS for the 3-component progressive frame + EOI.
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 12, 3, 1, 0, 2, 0, 3, 0, 0, 0, 0]);
    out.extend_from_slice(&[0x00, 0x00, 0xFF, 0xD9]);
    out
}

async fn test_measured_baseline_and_honest_12mp_never_false_killed() {
    let baseline = legion_tools::fs::measured_worker_baseline_bytes_for_test();
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    eprintln!(
        "thumb_worker measured baseline ({}-{}-{profile}): {} bytes ({:.2} MiB)",
        std::env::consts::OS,
        std::env::consts::ARCH,
        baseline,
        baseline as f64 / (1024.0 * 1024.0)
    );
    assert!(
        (2 * 1024 * 1024..=64 * 1024 * 1024).contains(&baseline),
        "unexpected worker baseline RSS: {baseline} bytes"
    );

    // An honest 12 MP (4000x3000) baseline JPEG must always succeed on every OS and build profile
    // without tripping the RSS watchdog or OS limit backstops.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let honest_12mp = home.join("honest_12mp.jpg");
    fs::write(&honest_12mp, encode_solid_jpeg(4000, 3000, [90, 140, 210])).unwrap();

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );
    let res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [honest_12mp.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    assert!(
        res["results"][0].get("ok").is_some(),
        "honest 12 MP JPEG must never be killed by backstops: {}",
        res["results"][0]
    );
    assert_eq!(res["results"][0]["ok"]["width"], json!(256));
    assert_eq!(res["results"][0]["ok"]["height"], json!(192));
}

fn crc32_ieee_test(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32_test(bytes: &[u8]) -> u32 {
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &b in bytes {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    (s2 << 16) | s1
}

fn build_interlaced_png_test(width: u32, height: u32, color_type: u8) -> Vec<u8> {
    let ch: usize = if color_type == 6 { 4 } else { 3 };
    const PASSES: [(u32, u32, u32, u32); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];
    let mut raw = Vec::new();
    for (x0, y0, dx, dy) in PASSES {
        if x0 >= width || y0 >= height {
            continue;
        }
        let pw = (width - x0).div_ceil(dx) as usize;
        let ph = (height - y0).div_ceil(dy) as usize;
        raw.resize(raw.len() + ph * (1 + pw * ch), 0u8);
    }
    let mut zlib = Vec::with_capacity(raw.len() + 64);
    zlib.extend_from_slice(&[0x78, 0x01]);
    let mut pos = 0;
    loop {
        let rem = raw.len() - pos;
        let len = rem.min(65535);
        let fin = pos + len == raw.len();
        zlib.push(u8::from(fin));
        let len16 = len as u16;
        zlib.extend_from_slice(&len16.to_le_bytes());
        zlib.extend_from_slice(&(!len16).to_le_bytes());
        zlib.extend_from_slice(&raw[pos..pos + len]);
        pos += len;
        if fin {
            break;
        }
    }
    zlib.extend_from_slice(&adler32_test(&raw).to_be_bytes());
    let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let push = |out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let s = out.len();
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32_ieee_test(&out[s..]).to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, color_type, 0, 0, 1]);
    push(&mut out, b"IHDR", &ihdr);
    push(&mut out, b"IDAT", &zlib);
    push(&mut out, b"IEND", &[]);
    out
}

/// Wraps a simple `VP8L` lossless WebP into a libvips-style `VP8X` container with the EXIF flag
/// set and a trailing `EXIF` chunk placed after the pixel payload (past the 128 KiB header peek
/// when the payload exceeds 128 KiB).
fn wrap_vp8l_with_vips_trailing_exif(vp8l_file: &[u8], width: u32, height: u32) -> Vec<u8> {
    assert_eq!(&vp8l_file[12..16], b"VP8L");
    let vp8l_chunk = &vp8l_file[12..];
    let exif_payload = b"II\x2a\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00";
    let mut body = Vec::with_capacity(4 + 18 + vp8l_chunk.len() + 1 + 8 + exif_payload.len());
    body.extend_from_slice(b"WEBP");
    body.extend_from_slice(b"VP8X");
    body.extend_from_slice(&10u32.to_le_bytes());
    body.extend_from_slice(&[0x08, 0, 0, 0]); // EXIF flag
    let wm1 = width - 1;
    let hm1 = height - 1;
    body.extend_from_slice(&wm1.to_le_bytes()[..3]);
    body.extend_from_slice(&hm1.to_le_bytes()[..3]);
    body.extend_from_slice(vp8l_chunk);
    if !vp8l_chunk.len().is_multiple_of(2) {
        body.push(0);
    }
    body.extend_from_slice(b"EXIF");
    body.extend_from_slice(&(exif_payload.len() as u32).to_le_bytes());
    body.extend_from_slice(exif_payload);
    if !exif_payload.len().is_multiple_of(2) {
        body.push(0);
    }
    let mut riff = Vec::with_capacity(8 + body.len());
    riff.extend_from_slice(b"RIFF");
    riff.extend_from_slice(&(body.len() as u32).to_le_bytes());
    riff.extend_from_slice(&body);
    riff
}

async fn test_honest_corpus_all_formats_extremes_sizes_and_fits() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let mut corpus: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut add = |name: &str, bytes: Vec<u8>| {
        let p = home.join(name);
        fs::write(&p, bytes).unwrap();
        corpus.push((name.to_owned(), p));
    };

    // 1. Extreme aspect ratios (previously OOM'd or failed in resize_to_fill / Triangle float buffers)
    for (name, w, h) in [
        ("strip_1x20000.png", 1u32, 20_000u32),
        ("strip_20000x1.png", 20_000, 1),
        ("strip_8192x100.png", 8192, 100),
        ("strip_100x8192.png", 100, 8192),
        ("shot_1280x720.png", 1280, 720),
        ("edge_257x257.png", 257, 257),
        ("edge_511x511.png", 511, 511),
        ("edge_512x512.png", 512, 512),
        ("edge_513x513.png", 513, 513),
    ] {
        let img = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(w, h, Rgb([70, 130, 190]));
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf)
            .write_image(&img, w, h, ExtendedColorType::Rgb8)
            .unwrap();
        add(name, buf);
    }

    // 2. Wide JPEG strip (20000x200) and progressive JPEG (2000x1500)
    add(
        "strip_20000x200.jpg",
        encode_solid_jpeg(20_000, 200, [140, 90, 50]),
    );
    let mut prog_jpg = vec![0xFF, 0xD8];
    prog_jpg.extend_from_slice(&minimal_progressive_444_jpeg_body(2000, 1500));
    add("prog_444_2000x1500.jpg", prog_jpg);

    // 3. PNG subtypes: RGBA8, Luma8, LumaA8, RGB16, RGBA16, interlaced RGB8 & RGBA8
    {
        let mut rgba =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(1280, 720, Rgba([40, 160, 220, 200]));
        rgba.put_pixel(10, 10, Rgba([255, 0, 0, 128]));
        let mut b = Vec::new();
        PngEncoder::new(&mut b)
            .write_image(&rgba, 1280, 720, ExtendedColorType::Rgba8)
            .unwrap();
        add("png_rgba8_1280x720.png", b);

        let gray = ImageBuffer::<Luma<u8>, Vec<u8>>::from_pixel(1001, 601, Luma([140]));
        let mut b = Vec::new();
        PngEncoder::new(&mut b)
            .write_image(&gray, 1001, 601, ExtendedColorType::L8)
            .unwrap();
        add("png_luma8_1001x601.png", b);

        let graya = ImageBuffer::<LumaA<u8>, Vec<u8>>::from_pixel(1001, 601, LumaA([140, 180]));
        let mut b = Vec::new();
        PngEncoder::new(&mut b)
            .write_image(&graya, 1001, 601, ExtendedColorType::La8)
            .unwrap();
        add("png_lumaa8_1001x601.png", b);

        let rgb16 = vec![0x42u8; 1024 * 768 * 6];
        let mut b = Vec::new();
        PngEncoder::new(&mut b)
            .write_image(&rgb16, 1024, 768, ExtendedColorType::Rgb16)
            .unwrap();
        add("png_rgb16_1024x768.png", b);

        let rgba16 = vec![0x42u8; 1024 * 768 * 8];
        let mut b = Vec::new();
        PngEncoder::new(&mut b)
            .write_image(&rgba16, 1024, 768, ExtendedColorType::Rgba16)
            .unwrap();
        add("png_rgba16_1024x768.png", b);

        add(
            "png_interlaced_rgb8.png",
            build_interlaced_png_test(1000, 800, 2),
        );
        add(
            "png_interlaced_rgba8.png",
            build_interlaced_png_test(1000, 800, 6),
        );
    }

    // 4. GIF (multi-frame 800x600)
    {
        let f1 = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(800, 600, Rgba([20, 180, 90, 255]));
        let f2 = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(800, 600, Rgba([180, 40, 90, 255]));
        let mut b = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut b);
            enc.encode_frame(Frame::new(f1)).unwrap();
            enc.encode_frame(Frame::new(f2)).unwrap();
        }
        add("anim_800x600.gif", b);
    }

    // 5. WebP lossless RGB, lossless RGBA, and libvips-style VP8X + VP8L (>128 KiB) + trailing EXIF
    {
        let mut noisy = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(1280, 720);
        let mut state = 0x1234_5678u32;
        for px in noisy.pixels_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = (state >> 24) as u8;
            let g = (state >> 16) as u8;
            let b = (state >> 8) as u8;
            *px = Rgb([r, g, b]);
        }
        let mut webp_rgb = Vec::new();
        WebPEncoder::new_lossless(&mut webp_rgb)
            .encode(&noisy, 1280, 720, ExtendedColorType::Rgb8)
            .unwrap();
        assert!(
            webp_rgb.len() > 160 * 1024,
            "noisy lossless WebP must exceed 128 KiB header peek to test trailing EXIF"
        );
        let vips_exif = wrap_vp8l_with_vips_trailing_exif(&webp_rgb, 1280, 720);
        add("webp_lossless_rgb_1280x720.webp", webp_rgb);
        add("webp_vips_trailing_exif_1280x720.webp", vips_exif);

        let rgba =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(1280, 720, Rgba([30, 120, 210, 180]));
        let mut webp_rgba = Vec::new();
        WebPEncoder::new_lossless(&mut webp_rgba)
            .encode(&rgba, 1280, 720, ExtendedColorType::Rgba8)
            .unwrap();
        add("webp_lossless_rgba_1280x720.webp", webp_rgba);
    }

    // 6. BMP 24-bit RGB and 32-bit RGBA
    {
        let rgb = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(1280, 720, Rgb([200, 100, 50]));
        let mut b = Vec::new();
        BmpEncoder::new(&mut b)
            .encode(&rgb, 1280, 720, ExtendedColorType::Rgb8)
            .unwrap();
        add("bmp_24bit_1280x720.bmp", b);

        let rgba =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(1280, 720, Rgba([200, 100, 50, 200]));
        let mut b = Vec::new();
        BmpEncoder::new(&mut b)
            .encode(&rgba, 1280, 720, ExtendedColorType::Rgba8)
            .unwrap();
        add("bmp_32bit_1280x720.bmp", b);
    }

    // 7. When `magick` is installed, also test real lossy WebP, lossy WebP with alpha, and
    // animated WebP (the exact subtypes that tripped the old factors).
    let has_magick = std::process::Command::new("magick")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if has_magick {
        for (name, args) in [
            (
                "webp_lossy_1920x1080.webp",
                vec!["-size", "1920x1080", "plasma:fractal", "-quality", "80"],
            ),
            (
                "webp_lossy_alpha_1920x1080.webp",
                vec![
                    "-size",
                    "1920x1080",
                    "plasma:fractal",
                    "-alpha",
                    "set",
                    "-quality",
                    "80",
                ],
            ),
            (
                "webp_animated_1280x720.webp",
                vec![
                    "-size",
                    "1280x720",
                    "plasma:fractal",
                    "-size",
                    "1280x720",
                    "gradient:red-blue",
                    "-quality",
                    "75",
                ],
            ),
        ] {
            let p = home.join(name);
            let st = std::process::Command::new("magick")
                .args(&args)
                .arg(&p)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            if st.is_ok_and(|s| s.success()) && p.exists() {
                corpus.push((name.to_owned(), p));
            }
        }
    }

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );
    let paths: Vec<String> = corpus
        .iter()
        .map(|(_, p)| p.to_string_lossy().into_owned())
        .collect();

    // Run EVERY honest corpus file across ALL sizes (including 1024) and BOTH fits through the real worker!
    let mut total_calls = 0usize;
    for size in [96u32, 128, 256, 384, 512, 1024] {
        for fit in ["contain", "cover"] {
            let res = node
                .call(Call::new(
                    Door::Cli,
                    "fs.thumb",
                    json!({"paths": paths, "size": size, "fit": fit}),
                ))
                .await
                .unwrap();
            let arr = res["results"].as_array().unwrap();
            assert_eq!(arr.len(), corpus.len());
            for (idx, item) in arr.iter().enumerate() {
                let ok = item.get("ok").unwrap_or_else(|| {
                    panic!(
                        "honest corpus image {} failed at size={size} fit={fit}: {item}",
                        corpus[idx].0
                    )
                });
                let ow = ok["width"].as_u64().unwrap() as u32;
                let oh = ok["height"].as_u64().unwrap() as u32;
                assert!(ow >= 1 && oh >= 1);
                if fit == "cover" {
                    assert_eq!(
                        (ow, oh),
                        (size, size),
                        "cover thumbnail for {} at size={size} must be {size}x{size}, got {ow}x{oh}",
                        corpus[idx].0
                    );
                } else {
                    assert!(
                        ow <= size && oh <= size,
                        "contain thumbnail for {} at size={size} exceeded bounds: {ow}x{oh}",
                        corpus[idx].0
                    );
                }
                total_calls += 1;
            }
        }
    }
    eprintln!(
        "thumb_worker honest corpus passed: {} images x 12 (size, fit) combinations = {total_calls} worker decodes (0 failures)",
        corpus.len()
    );
}

async fn test_lying_headers_and_sibling_survival() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let lie444 = home.join("lie444.jpg");
    let liegif = home.join("liegif.gif");
    let liewebp = home.join("liewebp.webp");
    let liewebp2 = home.join("liewebp2.webp");
    let good = home.join("good.jpg");

    fs::write(&lie444, build_lie444_jpeg()).unwrap();
    fs::write(&liegif, build_lie_gif(8000, 8000)).unwrap();
    fs::write(&liewebp, build_lie_webp(12_000, 12_000)).unwrap();
    fs::write(&liewebp2, build_lie_webp(16_383, 16_383)).unwrap();
    fs::write(&good, encode_solid_jpeg(240, 180, [40, 180, 90])).unwrap();

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({
                "paths": [
                    lie444.to_string_lossy(),
                    liegif.to_string_lossy(),
                    liewebp.to_string_lossy(),
                    liewebp2.to_string_lossy(),
                    good.to_string_lossy(),
                ],
                "size": 256
            }),
        ))
        .await
        .unwrap();

    let results = res["results"].as_array().unwrap();
    assert_eq!(results.len(), 5);
    for (idx, name) in ["lie444", "liegif", "liewebp", "liewebp2"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            results[idx]["error"]["code"],
            json!("failed"),
            "{name}: {}",
            results[idx]
        );
        assert_eq!(
            results[idx]["error"]["details"]["reason"],
            json!("too_large"),
            "{name}: {}",
            results[idx]
        );
    }
    assert!(
        results[4].get("ok").is_some(),
        "valid sibling must succeed: {}",
        results[4]
    );
    assert_eq!(results[4]["ok"]["width"], json!(240));
    assert_eq!(results[4]["ok"]["height"], json!(180));

    // 16 lying GIFs in a single call must all return per-path errors quickly with low parent RSS.
    let mut gif_paths = Vec::new();
    for i in 0..16 {
        let p = home.join(format!("liegif_{i}.gif"));
        fs::write(&p, build_lie_gif(8000, 8000)).unwrap();
        gif_paths.push(p.to_string_lossy().into_owned());
    }
    let res16 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": gif_paths, "size": 256}),
        ))
        .await
        .unwrap();
    let arr16 = res16["results"].as_array().unwrap();
    assert_eq!(arr16.len(), 16);
    for item in arr16 {
        assert_eq!(item["error"]["details"]["reason"], json!("too_large"));
    }
    let parent_rss = legion_os::mem_limit::process_rss_bytes(std::process::id()).unwrap_or(0);
    assert!(
        parent_rss < 256 * 1024 * 1024,
        "parent RSS too high after lying files: {} MiB",
        parent_rss / (1024 * 1024)
    );
}

async fn test_32_large_files_do_not_inflate_parent_rss() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // 32 x 63 MiB valid small JPEGs padded with trailing zero bytes: previously read all 2 GiB
    // into the parent process before reserving budget; now the parent peeks <= 128 KiB and hands
    // each open file handle to a bounded worker one permit at a time.
    let small_jpg = encode_solid_jpeg(128, 96, [20, 120, 220]);
    let target_len = 63 * 1024 * 1024u64;
    let mut paths = Vec::with_capacity(32);
    for i in 0..32 {
        let p = home.join(format!("padded_{i:02}.jpg"));
        let f = fs::File::create(&p).unwrap();
        f.set_len(target_len).unwrap();
        drop(f);
        let mut rw = fs::OpenOptions::new().write(true).open(&p).unwrap();
        use std::io::Write as _;
        rw.write_all(&small_jpg).unwrap();
        paths.push(p.to_string_lossy().into_owned());
    }

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let rss_before = legion_os::mem_limit::process_rss_bytes(std::process::id()).unwrap_or(0);
    let res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": paths, "size": 128}),
        ))
        .await
        .unwrap();
    let rss_after = legion_os::mem_limit::process_rss_bytes(std::process::id()).unwrap_or(0);
    let rss_delta = rss_after.saturating_sub(rss_before);

    let results = res["results"].as_array().unwrap();
    assert_eq!(results.len(), 32);
    for (i, item) in results.iter().enumerate() {
        assert!(item.get("ok").is_some(), "file {i} failed: {item}");
    }
    assert!(
        rss_delta < 128 * 1024 * 1024,
        "parent RSS grew by {} MiB on 32x63 MiB batch (must stay << 2 GiB)",
        rss_delta / (1024 * 1024)
    );
}

async fn test_worker_memory_kill_caches_bad_file_and_keeps_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let oom_jpg = home.join("parser_diff_oom.jpg");
    let good_jpg = home.join("sibling_ok.jpg");
    fs::write(&oom_jpg, build_parser_diff_oom_jpeg()).unwrap();
    fs::write(&good_jpg, encode_solid_jpeg(200, 150, [200, 60, 40])).unwrap();

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    // First call: worker hits the hard OS memory limit / RSS watchdog, is killed, records a
    // negative bad-file entry in ThumbCache, and returns a per-path error while sibling succeeds.
    let res1 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({
                "paths": [oom_jpg.to_string_lossy(), good_jpg.to_string_lossy()],
                "size": 256
            }),
        ))
        .await
        .unwrap();

    let arr1 = res1["results"].as_array().unwrap();
    assert_eq!(arr1[0]["error"]["code"], json!("failed"), "{}", arr1[0]);
    assert_eq!(
        arr1[0]["error"]["details"]["killed_for_memory"],
        json!(true),
        "expected killed_for_memory=true: {}",
        arr1[0]
    );
    assert_eq!(
        arr1[0]["error"]["details"]["enforced_by"],
        json!("allocator_cap"),
        "expected CappedAlloc (exit 107) to enforce exact heap cap: {}",
        arr1[0]
    );
    assert!(
        arr1[1].get("ok").is_some(),
        "sibling must survive worker kill: {}",
        arr1[1]
    );

    // Verify a negative bad-file cache entry was written alongside the positive sibling entry.
    let cache_dir = Paths::under(dir.path()).with_home(&home).thumb_cache_dir();
    let thumb_files: Vec<_> = fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
        .collect();
    assert!(
        thumb_files.len() >= 2,
        "expected both positive sibling cache entry and negative bad-file cache entry, found {}",
        thumb_files.len()
    );

    // Second call with the SAME `(size=256, fit=contain)`: served directly from the negative cache
    // without re-spawning or re-killing a worker.
    let t0 = Instant::now();
    let res2 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [oom_jpg.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let elapsed = t0.elapsed();
    assert_eq!(
        res2["results"][0]["error"]["code"],
        json!("failed"),
        "{}",
        res2["results"][0]
    );
    assert_eq!(
        res2["results"][0]["error"]["message"], arr1[0]["error"]["message"],
        "second call at the same size+fit must return the cached bad-file error"
    );
    assert!(
        !arr1[0]["error"]["hint"]
            .as_str()
            .unwrap_or("")
            .contains("header"),
        "memory-limit error must not claim the header lied: {}",
        arr1[0]
    );
    assert!(
        elapsed < Duration::from_millis(200),
        "cached bad-file hit should be fast, took {elapsed:?}"
    );

    // A call at a DIFFERENT `size` or `fit` must NOT reuse the `(256, contain)` negative record
    // (it runs independently and writes its own per-`(size, fit)` record if it also hits the cap).
    let count_before_other = fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
        .count();
    let res_other_size = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [oom_jpg.to_string_lossy()], "size": 128, "fit": "cover"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        res_other_size["results"][0]["error"]["code"],
        json!("failed")
    );
    let count_after_other = fs::read_dir(&cache_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
        .count();
    assert_eq!(
        count_after_other,
        count_before_other + 1,
        "different size+fit must have its own bad-file record rather than sharing (256, contain)"
    );
}

async fn test_worker_timeout_kills_slow_decode() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // Build a valid 7000x5000 8-bit RGB PNG (35 MP, estimated ~116 MiB <= 256 MiB cap). Even at
    // opt-level=3, inflating + filtering 105 MB of PNG scanlines takes >120 ms of solid CPU, so a
    // 50 ms test timeout deterministically triggers both timeout and `cpu_busy` accounting.
    let width = 7000u32;
    let height = 5000u32;
    let mut raw = vec![0u8; (width * height * 3) as usize];
    for (i, b) in raw.iter_mut().enumerate().step_by(97) {
        *b = (i % 251) as u8;
    }
    let mut png_bytes = Vec::new();
    PngEncoder::new(&mut png_bytes)
        .write_image(&raw, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    let slow_png = home.join("slow_valid.png");
    fs::write(&slow_png, &png_bytes).unwrap();

    let paths = Paths::under(dir.path()).with_home(&home);
    let cache_dir = paths.thumb_cache_dir();
    let node = Node::new(paths, legion_tools::registry());

    // 1. Non-CPU-busy timeouts (e.g., transient host load starvation) must NEVER trigger back-off.
    legion_tools::fs::set_thumb_decode_timeout_for_test(Some(Duration::from_millis(50)));
    legion_tools::fs::set_thumb_timeout_cpu_busy_for_test(Some(false));
    let _reset = TimeoutGuard;

    for attempt in 1..=2 {
        let res_load = node
            .call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": [slow_png.to_string_lossy()], "size": 256}),
            ))
            .await
            .unwrap();
        assert_eq!(
            res_load["results"][0]["error"]["code"],
            json!("timeout"),
            "non-cpu-busy attempt {attempt} must time out: {}",
            res_load["results"][0]
        );
        assert!(
            !res_load["results"][0]["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("back-off"),
            "non-cpu-busy timeouts must not enter back-off: {}",
            res_load["results"][0]
        );
    }

    // 2. Deterministic CPU-busy timeouts (via test hook rather than host load) enter back-off
    // after 2 hits on EVERY OS while NEVER writing a permanent bad-file entry on disk.
    legion_tools::fs::set_thumb_timeout_cpu_busy_for_test(Some(true));

    let res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [slow_png.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();

    assert_eq!(
        res["results"][0]["error"]["code"],
        json!("timeout"),
        "expected worker timeout error: {}",
        res["results"][0]
    );

    // Timeouts must NEVER write a permanent bad-file entry to disk.
    let disk_entries = fs::read_dir(&cache_dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        disk_entries, 0,
        "a worker timeout must not write a bad-file entry to thumb-cache"
    );

    // Trigger a second CPU-busy timeout (reaches repeated back-off threshold).
    let res2 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [slow_png.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    assert_eq!(
        res2["results"][0]["error"]["code"],
        json!("timeout"),
        "second slow attempt must time out: {}",
        res2["results"][0]
    );
    let disk_entries_after = fs::read_dir(&cache_dir)
        .map(|it| {
            it.filter_map(Result::ok)
                .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(
        disk_entries_after, 0,
        "repeated timeouts must still not write a permanent bad-file entry on disk"
    );

    // Third call immediately after restoring the normal timeout hits the short in-memory back-off
    // without re-spawning a worker (works deterministically on Linux, macOS, and Windows).
    legion_tools::fs::set_thumb_decode_timeout_for_test(None);
    legion_tools::fs::set_thumb_timeout_cpu_busy_for_test(None);
    let t0 = Instant::now();
    let res3 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [slow_png.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    assert_eq!(
        res3["results"][0]["error"]["code"],
        json!("timeout"),
        "third call after 2 CPU-busy timeouts must hit short in-memory back-off: {}",
        res3["results"][0]
    );
    assert!(
        res3["results"][0]["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("back-off"),
        "expected back-off message: {}",
        res3["results"][0]
    );
    assert!(
        t0.elapsed() < Duration::from_millis(200),
        "in-memory timeout back-off should return immediately, took {:?}",
        t0.elapsed()
    );
}

async fn test_mpeg_ts_sniffing_in_find_and_thumb() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // TypeScript source file.
    let ts_code = home.join("app.ts");
    fs::write(
        &ts_code,
        "export interface Photo { width: number; height: number; }\n".repeat(10),
    )
    .unwrap();

    // Synthetic 188-byte MPEG-TS stream (or real MPEG-TS if ffmpeg is installed).
    let ts_video = home.join("recording.ts");
    let has_ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if has_ffmpeg {
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=320x240:d=1",
                "-f",
                "mpegts",
                ts_video.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg failed to generate recording.ts");
    } else {
        let mut fake_ts = vec![0u8; 188 * 4];
        for i in 0..4 {
            fake_ts[i * 188] = 0x47;
            fake_ts[i * 188 + 1] = 0x40;
            fake_ts[i * 188 + 3] = 0x10;
        }
        fs::write(&ts_video, &fake_ts).unwrap();
    }

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let find_video = node
        .call(Call::new(
            Door::Cli,
            "fs.find",
            json!({"path": home.to_string_lossy(), "categories": ["video"]}),
        ))
        .await
        .unwrap();
    let v_matches = find_video["matches"].as_array().unwrap();
    assert_eq!(
        v_matches.len(),
        1,
        "expected only recording.ts: {find_video}"
    );
    assert!(
        v_matches[0]["path"]
            .as_str()
            .unwrap()
            .ends_with("recording.ts")
    );
    assert_eq!(v_matches[0]["category"], json!("video"));
    assert_eq!(v_matches[0]["mime"], json!("video/mp2t"));

    let find_text = node
        .call(Call::new(
            Door::Cli,
            "fs.find",
            json!({"path": home.to_string_lossy(), "categories": ["text"]}),
        ))
        .await
        .unwrap();
    let t_matches = find_text["matches"].as_array().unwrap();
    assert_eq!(t_matches.len(), 1, "expected only app.ts: {find_text}");
    assert!(t_matches[0]["path"].as_str().unwrap().ends_with("app.ts"));
    assert_eq!(t_matches[0]["category"], json!("text"));
    assert_eq!(t_matches[0]["mime"], json!("text/typescript"));

    let thumb_res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [ts_code.to_string_lossy(), ts_video.to_string_lossy()], "size": 128}),
        ))
        .await
        .unwrap();
    assert_eq!(
        thumb_res["results"][0]["error"]["code"],
        json!("unsupported"),
        "TypeScript .ts must stay unsupported: {}",
        thumb_res["results"][0]
    );
    if has_ffmpeg {
        assert!(
            thumb_res["results"][1].get("ok").is_some(),
            "MPEG-TS recording.ts must produce a video thumbnail: {}",
            thumb_res["results"][1]
        );
    }
}

fn mp4_box(fourcc: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(fourcc);
    out.extend_from_slice(payload);
    out
}

fn build_mov_with_dref(secret_name: &str) -> Vec<u8> {
    // Minimal MOV/MP4 containing a `dref` box with an external `url ` entry referencing
    // `secret_name` (non-self-contained flags = 0), testing that `-enable_drefs 0
    // -use_absolute_path 0` blocks external data references.
    let mut url_payload = vec![0u8; 4]; // version=0, flags=0 (external reference!)
    url_payload.extend_from_slice(secret_name.as_bytes());
    url_payload.push(0);
    let url_box = mp4_box(b"url ", &url_payload);

    let mut dref_payload = vec![0u8; 4];
    dref_payload.extend_from_slice(&1u32.to_be_bytes());
    dref_payload.extend_from_slice(&url_box);
    let dref = mp4_box(b"dref", &dref_payload);
    let dinf = mp4_box(b"dinf", &dref);

    let mut stsd_payload = vec![0u8; 4];
    stsd_payload.extend_from_slice(&1u32.to_be_bytes());
    let mut jpeg_entry = vec![0u8; 78];
    jpeg_entry[6..8].copy_from_slice(&1u16.to_be_bytes()); // data_reference_index = 1
    jpeg_entry[24..26].copy_from_slice(&64u16.to_be_bytes());
    jpeg_entry[26..28].copy_from_slice(&64u16.to_be_bytes());
    stsd_payload.extend_from_slice(&mp4_box(b"jpeg", &jpeg_entry));
    let stsd = mp4_box(b"stsd", &stsd_payload);

    let mut stts_p = vec![0u8; 4];
    stts_p.extend_from_slice(&1u32.to_be_bytes());
    stts_p.extend_from_slice(&1u32.to_be_bytes());
    stts_p.extend_from_slice(&1000u32.to_be_bytes());
    let stts = mp4_box(b"stts", &stts_p);

    let mut stsc_p = vec![0u8; 4];
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    let stsc = mp4_box(b"stsc", &stsc_p);

    let mut stsz_p = vec![0u8; 4];
    stsz_p.extend_from_slice(&0u32.to_be_bytes());
    stsz_p.extend_from_slice(&1u32.to_be_bytes());
    stsz_p.extend_from_slice(&4096u32.to_be_bytes());
    let stsz = mp4_box(b"stsz", &stsz_p);

    let mut stco_p = vec![0u8; 4];
    stco_p.extend_from_slice(&1u32.to_be_bytes());
    stco_p.extend_from_slice(&0u32.to_be_bytes());
    let stco = mp4_box(b"stco", &stco_p);

    let mut stbl_p = Vec::new();
    stbl_p.extend_from_slice(&stsd);
    stbl_p.extend_from_slice(&stts);
    stbl_p.extend_from_slice(&stsc);
    stbl_p.extend_from_slice(&stsz);
    stbl_p.extend_from_slice(&stco);
    let stbl = mp4_box(b"stbl", &stbl_p);

    let vmhd = mp4_box(b"vmhd", &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut minf_p = Vec::new();
    minf_p.extend_from_slice(&vmhd);
    minf_p.extend_from_slice(&dinf);
    minf_p.extend_from_slice(&stbl);
    let minf = mp4_box(b"minf", &minf_p);

    let mut mdhd_p = vec![0u8; 24];
    mdhd_p[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mdhd_p[16..20].copy_from_slice(&1000u32.to_be_bytes());
    let mdhd = mp4_box(b"mdhd", &mdhd_p);

    let mut hdlr_p = vec![0u8; 25];
    hdlr_p[8..12].copy_from_slice(b"vide");
    let hdlr = mp4_box(b"hdlr", &hdlr_p);

    let mut mdia_p = Vec::new();
    mdia_p.extend_from_slice(&mdhd);
    mdia_p.extend_from_slice(&hdlr);
    mdia_p.extend_from_slice(&minf);
    let mdia = mp4_box(b"mdia", &mdia_p);

    let mut tkhd_p = vec![0u8; 84];
    tkhd_p[3] = 3;
    tkhd_p[12..16].copy_from_slice(&1u32.to_be_bytes());
    tkhd_p[20..24].copy_from_slice(&1000u32.to_be_bytes());
    tkhd_p[76..80].copy_from_slice(&(64u32 << 16).to_be_bytes());
    tkhd_p[80..84].copy_from_slice(&(64u32 << 16).to_be_bytes());
    let tkhd = mp4_box(b"tkhd", &tkhd_p);

    let mut trak_p = Vec::new();
    trak_p.extend_from_slice(&tkhd);
    trak_p.extend_from_slice(&mdia);
    let trak = mp4_box(b"trak", &trak_p);

    let mut mvhd_p = vec![0u8; 100];
    mvhd_p[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd_p[16..20].copy_from_slice(&1000u32.to_be_bytes());
    mvhd_p[96..100].copy_from_slice(&2u32.to_be_bytes());
    let mvhd = mp4_box(b"mvhd", &mvhd_p);

    let mut moov_p = Vec::new();
    moov_p.extend_from_slice(&mvhd);
    moov_p.extend_from_slice(&trak);
    let moov = mp4_box(b"moov", &moov_p);

    let ftyp = mp4_box(b"ftyp", b"qt  \x00\x00\x00\x00qt  ");
    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&moov);
    mov
}

async fn test_ffmpeg_blocks_dref_and_pixel_bomb() {
    let has_ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !has_ffmpeg {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // 1. MOV/MP4 with external `dref` pointing to a valid JPEG next to it: must be blocked!
    let secret = home.join("secret_frame.jpg");
    fs::write(&secret, encode_solid_jpeg(64, 64, [255, 0, 0])).unwrap();
    let dref_mp4 = home.join("dref_rel.mp4");
    fs::write(&dref_mp4, build_mov_with_dref("secret_frame.jpg")).unwrap();

    // 2. MKV containing a 10000x10000 MJPEG frame (100 MP > 64 MP `-max_pixels`): must be blocked!
    let huge_frame = home.join("huge.jpg");
    fs::write(&huge_frame, encode_solid_jpeg(10_000, 10_000, [10, 20, 30])).unwrap();
    let bomb_mkv = home.join("bomb.mkv");
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-i",
            huge_frame.to_str().unwrap(),
            "-c:v",
            "copy",
            bomb_mkv.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg failed to mux bomb.mkv");
    let _ = fs::remove_file(&huge_frame);

    // 3. Normal MP4 video to confirm valid videos still thumbnail cleanly.
    let legit_mp4 = home.join("legit.mp4");
    let status2 = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=green:s=320x240:d=1",
            "-pix_fmt",
            "yuv420p",
            legit_mp4.to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status2.success(), "ffmpeg failed to create legit.mp4");

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({
                "paths": [
                    dref_mp4.to_string_lossy(),
                    bomb_mkv.to_string_lossy(),
                    legit_mp4.to_string_lossy(),
                ],
                "size": 256
            }),
        ))
        .await
        .unwrap();

    let arr = res["results"].as_array().unwrap();
    assert_eq!(
        arr[0]["error"]["code"],
        json!("failed"),
        "dref must fail: {}",
        arr[0]
    );
    assert_eq!(
        arr[1]["error"]["code"],
        json!("failed"),
        "bomb.mkv must fail: {}",
        arr[1]
    );
    assert!(
        arr[2].get("ok").is_some(),
        "legit.mp4 must succeed: {}",
        arr[2]
    );
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn ffmpeg_has_encoder(name: &str) -> bool {
    std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains(name)
        })
}

async fn test_video_suite_1080p_4k_hevc10_8k_longgop_and_portrait_rotation() {
    if !ffmpeg_available() {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let has_x265 = ffmpeg_has_encoder("libx265");

    let run_ffmpeg = |args: &[&str]| {
        let status = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args(args)
            .status()
            .expect("spawn ffmpeg");
        assert!(status.success(), "ffmpeg failed with args {args:?}");
    };

    // 1. 1080p H.264
    let v_1080p = home.join("clip_1080p_h264.mp4");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=1920x1080:r=24:d=0.5",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        v_1080p.to_str().unwrap(),
    ]);

    // 2. 4K H.264
    let v_4k_h264 = home.join("clip_4k_h264.mp4");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=navy:s=3840x2160:r=10:d=0.3",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        v_4k_h264.to_str().unwrap(),
    ]);

    // 3. 4K long-GOP H.264 (240-frame GOP)
    let v_4k_longgop = home.join("clip_4k_longgop.mp4");
    run_ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "color=c=teal:s=3840x2160:r=24:d=1.2",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-g",
        "240",
        "-pix_fmt",
        "yuv420p",
        v_4k_longgop.to_str().unwrap(),
    ]);

    // 4. Portrait phone clip with 90-degree display rotation metadata
    let v_portrait = home.join("clip_portrait_rot90.mov");
    run_ffmpeg(&[
        "-display_rotation:v:0",
        "90",
        "-i",
        v_1080p.to_str().unwrap(),
        "-c",
        "copy",
        v_portrait.to_str().unwrap(),
    ]);

    let mut paths = vec![
        v_1080p.to_string_lossy().into_owned(),
        v_4k_h264.to_string_lossy().into_owned(),
        v_4k_longgop.to_string_lossy().into_owned(),
        v_portrait.to_string_lossy().into_owned(),
    ];

    // 5. 4K HEVC 10-bit (ordinary iPhone HDR clip) & 6. 8K HEVC (when libx265 is installed)
    let v_4k_hevc10 = home.join("clip_4k_hevc10.mov");
    let v_8k_hevc = home.join("clip_8k_hevc.mp4");
    if has_x265 {
        run_ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "color=c=maroon:s=3840x2160:r=10:d=0.3",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error",
            "-pix_fmt",
            "yuv420p10le",
            "-tag:v",
            "hvc1",
            v_4k_hevc10.to_str().unwrap(),
        ]);
        run_ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "color=c=olive:s=7680x4320:r=5:d=0.2",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error",
            "-pix_fmt",
            "yuv420p",
            v_8k_hevc.to_str().unwrap(),
        ]);
        paths.push(v_4k_hevc10.to_string_lossy().into_owned());
        paths.push(v_8k_hevc.to_string_lossy().into_owned());
    }

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": paths, "size": 256}),
        ))
        .await
        .unwrap();

    let arr = res["results"].as_array().unwrap();
    assert_eq!(arr.len(), paths.len());
    for (i, item) in arr.iter().enumerate() {
        assert!(
            item.get("ok").is_some(),
            "video clip {i} ({}) must produce a poster: {item}",
            paths[i]
        );
    }

    // Verify rotation metadata was honored on the portrait phone clip (index 3).
    let pw = arr[3]["ok"]["width"].as_u64().unwrap();
    let ph = arr[3]["ok"]["height"].as_u64().unwrap();
    assert!(
        ph > pw,
        "portrait clip with 90-deg display rotation must produce portrait poster (height > width), got {pw}x{ph}"
    );
    // And landscape 1080p (index 0) stays landscape.
    let lw = arr[0]["ok"]["width"].as_u64().unwrap();
    let lh = arr[0]["ok"]["height"].as_u64().unwrap();
    assert!(
        lw > lh,
        "landscape 1080p clip must stay landscape, got {lw}x{lh}"
    );
}

#[cfg(target_os = "linux")]
fn find_child_thumb_workers(parent_pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(rparen) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[rparen + 1..].split_whitespace();
        let _state = fields.next();
        let Some(ppid_str) = fields.next() else {
            continue;
        };
        if ppid_str.parse::<u32>().ok() != Some(parent_pid) {
            continue;
        }
        let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if cmdline
            .windows(b"__thumb-worker".len())
            .any(|w| w == b"__thumb-worker")
        {
            out.push(pid);
        }
    }
    out
}

#[cfg(target_os = "linux")]
async fn test_external_sigkill_and_sigterm_do_not_poison_bad_cache() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // Build a valid 7000x5000 PNG (~150-300 ms decode) so an external signal reliably hits the
    // running `__thumb-worker` child.
    let width = 7000u32;
    let height = 5000u32;
    let mut raw = vec![0u8; (width * height * 3) as usize];
    for (i, b) in raw.iter_mut().enumerate().step_by(97) {
        *b = (i % 251) as u8;
    }
    let mut png_bytes = Vec::new();
    PngEncoder::new(&mut png_bytes)
        .write_image(&raw, width, height, ExtendedColorType::Rgb8)
        .unwrap();

    let paths = Paths::under(dir.path()).with_home(&home);
    let cache_dir = paths.thumb_cache_dir();
    let node = Arc::new(Node::new(paths, legion_tools::registry()));
    let my_pid = std::process::id();

    for (label, sig) in [("sigkill", Signal::SIGKILL), ("sigterm", Signal::SIGTERM)] {
        let img = home.join(format!("ext_{label}.png"));
        fs::write(&img, &png_bytes).unwrap();

        let stop_killer = Arc::new(AtomicBool::new(false));
        let killed_flag = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop_killer);
        let killed_bg = Arc::clone(&killed_flag);

        let killer = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !stop_bg.load(Ordering::Relaxed) && Instant::now() < deadline {
                let workers = find_child_thumb_workers(my_pid);
                if let Some(&wpid) = workers.first()
                    && kill(Pid::from_raw(wpid as i32), sig).is_ok()
                {
                    killed_bg.store(true, Ordering::Release);
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        let res1 = node
            .call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": [img.to_string_lossy()], "size": 256}),
            ))
            .await
            .unwrap();
        stop_killer.store(true, Ordering::Release);
        killer.await.unwrap();

        assert!(
            killed_flag.load(Ordering::Acquire),
            "test must have delivered external {label} to __thumb-worker"
        );
        let err = &res1["results"][0]["error"];
        assert_eq!(
            err["code"],
            json!("failed"),
            "external {label} must return failed: {}",
            res1["results"][0]
        );
        assert!(
            err["details"]["killed_for_memory"].is_null(),
            "external {label} must NEVER be misattributed to memory limit: {err}"
        );
        assert!(
            !err["message"]
                .as_str()
                .unwrap_or("")
                .contains("memory limit"),
            "external {label} message must not blame memory limit: {err}"
        );

        // No bad-file entry may exist on disk for this file.
        let disk_entries = fs::read_dir(&cache_dir)
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
                    .count()
            })
            .unwrap_or(0);
        // Only earlier successful positive entries (0 on first iter, 1 on second iter) may exist.
        let expected_before_retry = usize::from(label == "sigterm");
        assert_eq!(
            disk_entries, expected_before_retry,
            "external {label} must not poison thumb-cache (found {disk_entries} entries)"
        );

        // Second call without external interference must succeed!
        let res2 = node
            .call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": [img.to_string_lossy()], "size": 256}),
            ))
            .await
            .unwrap();
        assert!(
            res2["results"][0].get("ok").is_some(),
            "second call after external {label} must succeed: {}",
            res2["results"][0]
        );
    }
}

#[cfg(unix)]
async fn test_fifo_swap_race_never_hangs_any_fs_or_xfer_tool() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // 1. Stationary FIFO: every read/thumb/xfer path must reject immediately without blocking.
    let stationary_fifo = home.join("stuck.jpg");
    let mk_status = std::process::Command::new("mkfifo")
        .arg(&stationary_fifo)
        .status()
        .expect("run mkfifo");
    assert!(mk_status.success(), "mkfifo failed");

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let fifo_str = stationary_fifo.to_string_lossy().into_owned();
    let copy_dst = home.join("copied.jpg").to_string_lossy().into_owned();

    let t0 = Instant::now();
    let r_thumb = tokio::time::timeout(
        Duration::from_secs(2),
        node.call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [&fifo_str], "size": 128}),
        )),
    )
    .await
    .expect("fs.thumb hung on FIFO")
    .unwrap();
    assert!(r_thumb["results"][0].get("error").is_some());

    for (tool, args) in [
        ("fs.read", json!({"path": &fifo_str})),
        ("fs.hash", json!({"path": &fifo_str})),
        ("fs.grep", json!({"path": &fifo_str, "pattern": "hello"})),
        (
            "fs.edit",
            json!({"path": &fifo_str, "old": "hello", "new": "world"}),
        ),
        ("fs.copy", json!({"from": &fifo_str, "to": &copy_dst})),
    ] {
        let res = tokio::time::timeout(
            Duration::from_secs(2),
            node.call(Call::new(Door::Cli, tool, args)),
        )
        .await
        .unwrap_or_else(|_| panic!("{tool} hung on stationary FIFO"));
        assert!(res.is_err(), "{tool} must reject FIFO, got {res:?}");
    }

    // legion-xfer direct open & hash on FIFO must fail immediately too.
    assert!(legion_xfer::file::GetStream::open(&stationary_fifo, 0).is_err());
    assert!(legion_xfer::hash::hash_file(&stationary_fifo).is_err());
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "FIFO rejections should be instant, took {:?}",
        t0.elapsed()
    );

    // 2. Active FIFO swap race (reproducing race.py): rapidly atomically swap a regular file and
    // a FIFO at the same path while calling every fs.* read tool and legion-xfer.
    let race_jpg = home.join("race.jpg");
    let race_txt = home.join("race.txt");
    let valid_jpg = encode_solid_jpeg(96, 96, [30, 140, 210]);
    fs::write(&race_jpg, &valid_jpg).unwrap();
    fs::write(&race_txt, "hello world\nsecond line\n").unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_swap = Arc::clone(&stop);
    let swap_dir = home.clone();
    let jpg_bytes = valid_jpg.clone();
    let swapper = std::thread::spawn(move || {
        let mut iter = 0u64;
        while !stop_swap.load(Ordering::Relaxed) {
            iter += 1;
            let stage_jpg = swap_dir.join(format!(".stage_jpg_{iter}"));
            let stage_txt = swap_dir.join(format!(".stage_txt_{iter}"));
            if iter.is_multiple_of(2) {
                let _ = std::process::Command::new("mkfifo")
                    .arg(&stage_jpg)
                    .status();
                let _ = std::process::Command::new("mkfifo")
                    .arg(&stage_txt)
                    .status();
            } else {
                let _ = fs::write(&stage_jpg, &jpg_bytes);
                let _ = fs::write(&stage_txt, "hello world\nsecond line\n");
            }
            let _ = fs::rename(&stage_jpg, swap_dir.join("race.jpg"));
            let _ = fs::rename(&stage_txt, swap_dir.join("race.txt"));
        }
    });

    let jpg_s = race_jpg.to_string_lossy().into_owned();
    let txt_s = race_txt.to_string_lossy().into_owned();
    let copy_out = home.join("race_copy.txt");
    let copy_out_s = copy_out.to_string_lossy().into_owned();

    for i in 0..16 {
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            node.call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": [&jpg_s], "size": 96}),
            )),
        )
        .await
        .unwrap_or_else(|_| panic!("fs.thumb hung during FIFO swap race iter {i}"));

        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            node.call(Call::new(Door::Cli, "fs.read", json!({"path": &txt_s}))),
        )
        .await
        .unwrap_or_else(|_| panic!("fs.read hung during FIFO swap race iter {i}"));

        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            node.call(Call::new(Door::Cli, "fs.hash", json!({"path": &txt_s}))),
        )
        .await
        .unwrap_or_else(|_| panic!("fs.hash hung during FIFO swap race iter {i}"));

        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            node.call(Call::new(
                Door::Cli,
                "fs.grep",
                json!({"path": &txt_s, "pattern": "hello"}),
            )),
        )
        .await
        .unwrap_or_else(|_| panic!("fs.grep hung during FIFO swap race iter {i}"));

        let _ = fs::remove_file(&copy_out);
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            node.call(Call::new(
                Door::Cli,
                "fs.copy",
                json!({"from": &txt_s, "to": &copy_out_s, "overwrite": true}),
            )),
        )
        .await
        .unwrap_or_else(|_| panic!("fs.copy hung during FIFO swap race iter {i}"));

        // Also exercise legion-xfer open/hash directly while racing.
        let p_get = race_txt.clone();
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || {
                let _ = legion_xfer::file::GetStream::open(&p_get, 0);
                let _ = legion_xfer::hash::hash_file(&p_get);
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("legion-xfer hung during FIFO swap race iter {i}"));
    }

    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();
}

fn build_gif_with_n_comments(n_comments: usize, frame_w: u16, frame_h: u16) -> Vec<u8> {
    let mut gif = Vec::with_capacity(64 + n_comments * 5);
    gif.extend_from_slice(b"GIF89a");
    gif.extend_from_slice(&16u16.to_le_bytes());
    gif.extend_from_slice(&16u16.to_le_bytes());
    gif.extend_from_slice(&[0x80, 0x00, 0x00]);
    gif.extend_from_slice(&[0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF]);
    for _ in 0..n_comments {
        gif.extend_from_slice(&[0x21, 0xFE, 0x01, b'x', 0x00]);
    }
    gif.push(0x2C);
    gif.extend_from_slice(&0u16.to_le_bytes());
    gif.extend_from_slice(&0u16.to_le_bytes());
    gif.extend_from_slice(&frame_w.to_le_bytes());
    gif.extend_from_slice(&frame_h.to_le_bytes());
    gif.push(0x00);
    // Minimal valid LZW stream for small frames (16x16 = 256 zero pixels with clear+EOI).
    gif.push(0x02); // LZW min code size = 2
    // Sub-block with valid LZW encoding of 256 zero indices or minimal stub.
    if frame_w == 16 && frame_h == 16 {
        // Encode a real 16x16 GIF frame via the `image` crate? Even simpler: clear(4), 0..0, EOI(5).
        // Let's write a tiny valid 1x1-friendly frame or encode a real 16x16 GIF below.
        gif.extend_from_slice(&[0x02, 0x4C, 0x01, 0x00, 0x3B]);
    } else {
        gif.extend_from_slice(&[0x02, 0x00, 0x3B]);
    }
    gif
}

fn build_real_valid_16x16_gif_with_n_comments(n_comments: usize) -> Vec<u8> {
    let img = ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_pixel(
        16,
        16,
        image::Rgba([20, 120, 220, 255]),
    );
    let mut base = Vec::new();
    let mut enc = image::codecs::gif::GifEncoder::new(&mut base);
    enc.encode_frame(image::Frame::new(img)).unwrap();
    drop(enc);

    // Insert `n_comments` comment extension blocks right before the first Image Descriptor (`0x2C`).
    let pos = base
        .windows(1)
        .enumerate()
        .skip(13)
        .find_map(|(i, w)| (w[0] == 0x2C).then_some(i))
        .expect("find GIF image descriptor");
    let mut out = Vec::with_capacity(base.len() + n_comments * 5);
    out.extend_from_slice(&base[..pos]);
    for _ in 0..n_comments {
        out.extend_from_slice(&[0x21, 0xFE, 0x01, b'x', 0x00]);
    }
    out.extend_from_slice(&base[pos..]);
    out
}

async fn test_gif_many_extension_blocks_and_32_parallel_workers_stay_within_budget() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    // 1. mkgif2-style liar: 250 comment extension blocks before an 8000x8000 frame descriptor
    // (~336 MiB > 256 MiB cap). Previously stopped scanning at 128 blocks; now caught and
    // rejected at header peek (`too_large`).
    let liar = home.join("mkgif2_liar_8000.gif");
    let mut liar_bytes = build_gif_with_n_comments(250, 8000, 8000);
    liar_bytes.resize(2 * 1024 * 1024, 0);
    fs::write(&liar, &liar_bytes).unwrap();

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );
    let res_liar = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [liar.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    assert_eq!(
        res_liar["results"][0]["error"]["details"]["reason"],
        json!("too_large"),
        "mkgif2 250-comment 8000x8000 GIF must be rejected at header peek: {}",
        res_liar["results"][0]
    );

    // 2. 32 padded 31 MiB valid GIFs (with 250 comment extension blocks each) processed in one
    // batch: all 32 must succeed AND combined live child worker RSS must stay <= 512 MiB!
    let valid_gif = build_real_valid_16x16_gif_with_n_comments(250);
    let target_len = 31 * 1024 * 1024u64;
    let mut paths = Vec::with_capacity(32);
    for i in 0..32 {
        let p = home.join(format!("padded_gif_{i:02}.gif"));
        let f = fs::File::create(&p).unwrap();
        f.set_len(target_len).unwrap();
        drop(f);
        let mut rw = fs::OpenOptions::new().write(true).open(&p).unwrap();
        rw.write_all(&valid_gif).unwrap();
        paths.push(p.to_string_lossy().into_owned());
    }

    #[cfg(target_os = "linux")]
    let (stop_mon, peak_sum, mon_handle) = {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let stop_bg = Arc::clone(&stop);
        let peak_bg = Arc::clone(&peak);
        let my_pid = std::process::id();
        let h = std::thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                let mut sum = 0u64;
                for wpid in find_child_thumb_workers(my_pid) {
                    if let Some(rss) = legion_os::mem_limit::process_rss_bytes(wpid) {
                        sum = sum.saturating_add(rss);
                    }
                }
                peak_bg.fetch_max(sum, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        (stop, peak, h)
    };

    let res32 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": paths, "size": 128}),
        ))
        .await
        .unwrap();

    #[cfg(target_os = "linux")]
    {
        stop_mon.store(true, Ordering::Relaxed);
        mon_handle.join().unwrap();
        let peak_bytes = peak_sum.load(Ordering::Relaxed);
        assert!(
            peak_bytes <= 512 * 1024 * 1024,
            "combined live worker RSS across 32x31 MiB padded GIFs exceeded 512 MiB: {} MiB",
            peak_bytes / (1024 * 1024)
        );
    }

    let arr32 = res32["results"].as_array().unwrap();
    assert_eq!(arr32.len(), 32);
    for (i, item) in arr32.iter().enumerate() {
        assert!(
            item.get("ok").is_some(),
            "padded valid GIF {i} failed: {item}"
        );
    }
}

async fn test_worker_cli_rejects_absurd_args_without_allocating() {
    let exe = std::env::current_exe().expect("current exe");
    let t0 = Instant::now();
    let out = std::process::Command::new(&exe)
        .args([
            "__thumb-worker",
            "4294967295",
            "contain",
            "4294967295",
            "67108864",
            "67108864",
            "/x.jpg",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn __thumb-worker");
    assert_eq!(
        out.status.code(),
        Some(2),
        "absurd CLI args must exit 2 immediately, got {:?}",
        out.status
    );
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "absurd CLI arg rejection should be immediate, took {:?}",
        t0.elapsed()
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("invalid_args"),
        "expected invalid_args error frame on stdout"
    );
}

async fn test_audio_tags_and_covers_through_real_worker_and_benchmarks() {
    use audio_fixtures as af;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    // 1. Embedded parser-differential OOM JPEG inside an MP3: worker must tighten CappedAlloc down
    //    to the cover's probe cap and exit 107 (enforced_by = allocator_cap), while a valid sibling
    //    MP3 in the exact same call succeeds cleanly.
    let small_cover = af::encode_solid_jpeg(160, 160, [210, 40, 40]);
    let oom_mp3 = home.join("embedded_oom.mp3");
    let good_mp3 = home.join("good_sibling.mp3");
    fs::write(
        &oom_mp3,
        af::build_mp3_with_raw_cover("Bad cover", "image/jpeg", &build_parser_diff_oom_jpeg()),
    )
    .unwrap();
    fs::write(
        &good_mp3,
        af::build_mp3_id3v23(&[("image/jpeg", 3, &small_cover)], true, false),
    )
    .unwrap();

    let oom_res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({
                "paths": [oom_mp3.to_string_lossy(), good_mp3.to_string_lossy()],
                "size": 256,
                "fit": "cover"
            }),
        ))
        .await
        .unwrap();
    let r_oom = &oom_res["results"][0];
    assert_eq!(r_oom["error"]["code"], json!("failed"), "oom: {r_oom}");
    assert_eq!(
        r_oom["error"]["details"]["enforced_by"],
        json!("allocator_cap"),
        "embedded OOM cover must trip CappedAlloc after tightening: {r_oom}"
    );
    assert_eq!(
        r_oom["error"]["details"]["killed_for_memory"],
        json!(true),
        "oom: {r_oom}"
    );
    assert!(
        oom_res["results"][1].get("ok").is_some(),
        "valid sibling MP3 must succeed: {}",
        oom_res["results"][1]
    );

    // 2. Audio worker CLI rejects absurd arguments immediately without allocating.
    let exe = std::env::current_exe().expect("current exe");
    for bad_args in [
        vec![
            "__thumb-worker",
            "--audio-tags",
            "mp3",
            "999999999999",
            "/x.mp3",
        ],
        vec![
            "__thumb-worker",
            "--audio-cover",
            "mp3",
            "2048",
            "contain",
            "67108864",
            "1",
            "/x.mp3",
        ],
    ] {
        let out = std::process::Command::new(&exe)
            .args(&bad_args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn audio worker");
        assert_eq!(out.status.code(), Some(2), "{bad_args:?}");
    }

    // 3. Benchmark: `fs.tags` on 32 diverse audio paths (cold cache, real out-of-process worker).
    let bench32_dir = tempfile::tempdir().unwrap();
    let bench32_home = bench32_dir.path().join("home");
    fs::create_dir_all(&bench32_home).unwrap();
    let bench32_node = Node::new(
        Paths::under(bench32_dir.path()).with_home(&bench32_home),
        legion_tools::registry(),
    );

    let mut paths32 = Vec::with_capacity(32);
    for i in 0..32usize {
        let (ext, bytes) = match i % 8 {
            0 => (
                "mp3",
                af::build_mp3_id3v23(&[("image/jpeg", 3, &small_cover)], true, false),
            ),
            1 => ("mp3", af::build_mp3_id3v24(Some(&small_cover))),
            2 => ("wav", af::build_wav(true, Some(&small_cover), 4)),
            3 => ("aiff", af::build_aiff(true, Some(&small_cover), 5)),
            4 => (
                "flac",
                af::build_flac(
                    &[("TITLE", "First song"), ("ARTIST", "Someone")],
                    &[(3, "image/jpeg", &small_cover)],
                    6,
                ),
            ),
            5 => (
                "ogg",
                af::build_ogg_vorbis(
                    &[("TITLE", "First song"), ("ARTIST", "Someone")],
                    &[(3, "image/jpeg", &small_cover)],
                    7,
                ),
            ),
            6 => (
                "opus",
                af::build_ogg_opus(
                    &[("TITLE", "First song"), ("ARTIST", "Someone")],
                    &[(3, "image/jpeg", &small_cover)],
                    8,
                ),
            ),
            _ => (
                "m4a",
                af::build_mp4(true, Some((13, &small_cover)), 9, false, false),
            ),
        };
        let p = bench32_home.join(format!("song_{i:02}.{ext}"));
        fs::write(&p, bytes).unwrap();
        paths32.push(p.to_string_lossy().into_owned());
    }

    legion_tools::fs::reset_peak_worker_rss_for_test();
    let t_tags = Instant::now();
    let res32 = bench32_node
        .call(Call::new(Door::Cli, "fs.tags", json!({"paths": paths32})))
        .await
        .unwrap();
    let tags_ms = t_tags.elapsed().as_secs_f64() * 1000.0;
    let tags_peak_rss = legion_tools::fs::peak_worker_rss_for_test();
    let arr32 = res32["results"].as_array().unwrap();
    assert_eq!(arr32.len(), 32);
    for (i, item) in arr32.iter().enumerate() {
        assert!(item.get("ok").is_some(), "track {i} failed: {item}");
        assert_eq!(item["ok"]["cover"], json!(true), "track {i}: {item}");
    }
    eprintln!(
        "BENCH fs.tags 32 paths (cold, real worker): {:.1} ms | peak worker RSS = {} bytes ({:.2} MiB)",
        tags_ms,
        tags_peak_rss,
        tags_peak_rss as f64 / (1024.0 * 1024.0)
    );
    assert!(
        tags_peak_rss > 0 && tags_peak_rss < 128 * 1024 * 1024,
        "unexpected peak worker RSS for fs.tags: {tags_peak_rss} bytes"
    );

    // 4. Benchmark: `fs.thumb` at size=1024 on an audio file with a real ~10 MiB embedded cover.
    let bench10_dir = tempfile::tempdir().unwrap();
    let bench10_home = bench10_dir.path().join("home");
    fs::create_dir_all(&bench10_home).unwrap();
    let bench10_node = Node::new(
        Paths::under(bench10_dir.path()).with_home(&bench10_home),
        legion_tools::registry(),
    );
    let big_cover_png = af::build_10mib_png_cover();
    let big_cover_len = big_cover_png.len();
    let big_mp3 = bench10_home.join("big_cover_10mib.mp3");
    fs::write(
        &big_mp3,
        af::build_mp3_with_raw_cover("Large cover song", "image/png", &big_cover_png),
    )
    .unwrap();

    legion_tools::fs::reset_peak_worker_rss_for_test();
    let t_big = Instant::now();
    let res_big = bench10_node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({
                "paths": [big_mp3.to_string_lossy()],
                "size": 1024,
                "fit": "cover"
            }),
        ))
        .await
        .unwrap();
    let big_ms = t_big.elapsed().as_secs_f64() * 1000.0;
    let big_peak_rss = legion_tools::fs::peak_worker_rss_for_test();
    let ok_big = res_big["results"][0]
        .get("ok")
        .unwrap_or_else(|| panic!("10 MiB embedded cover failed: {}", res_big["results"][0]));
    assert_eq!(ok_big["width"], json!(1024));
    assert_eq!(ok_big["height"], json!(1024));
    eprintln!(
        "BENCH fs.thumb 1024 cover from {:.2} MiB embedded picture (cold, real worker): {:.1} ms | peak worker RSS = {} bytes ({:.2} MiB)",
        big_cover_len as f64 / (1024.0 * 1024.0),
        big_ms,
        big_peak_rss,
        big_peak_rss as f64 / (1024.0 * 1024.0)
    );
    assert!(
        big_peak_rss > 0 && big_peak_rss < 256 * 1024 * 1024,
        "10 MiB embedded cover worker exceeded 256 MiB RSS: {big_peak_rss} bytes"
    );
}

async fn test_reply_budget_real_worker_node_memory_and_completion() {
    use audio_fixtures as af;

    const LINK_LINE_LIMIT: usize = 16 * 1024 * 1024;
    const REPLY_B64_LIMIT: usize = 12 * 1024 * 1024;

    async fn drain_with_rss(
        node: &Node,
        all_paths: &[String],
        size: u32,
        fit: &str,
        label: &str,
    ) -> usize {
        let mut remaining: Vec<String> = all_paths.to_vec();
        let mut done = 0usize;
        let mut calls = 0usize;
        let mut max_line = 0usize;
        let rss_before = legion_os::mem_limit::process_rss_bytes(std::process::id()).unwrap_or(0);
        let mut max_rss = rss_before;

        while !remaining.is_empty() {
            calls += 1;
            assert!(calls <= all_paths.len(), "{label}: must make progress");
            let res = node
                .call(Call::new(
                    Door::Cli,
                    "fs.thumb",
                    json!({"paths": remaining, "size": size, "fit": fit}),
                ))
                .await
                .unwrap();
            let line = serde_json::to_string(&res).unwrap();
            let rss_now = legion_os::mem_limit::process_rss_bytes(std::process::id()).unwrap_or(0);
            max_rss = max_rss.max(rss_now);
            max_line = max_line.max(line.len());
            assert!(
                line.len() < LINK_LINE_LIMIT,
                "{label}: reply line {} bytes >= 16 MiB",
                line.len()
            );

            let items = res["results"].as_array().unwrap();
            assert_eq!(items.len(), remaining.len());
            let mut b64_total = 0usize;
            let mut next = Vec::new();
            let mut saw_cutoff = false;
            for (idx, item) in items.iter().enumerate() {
                if let Some(ok) = item.get("ok") {
                    assert!(!saw_cutoff, "{label}: ok after reply_budget cutoff");
                    b64_total += ok["content"].as_str().unwrap().len();
                    done += 1;
                } else {
                    assert_eq!(item["error"]["code"], json!("busy"), "{label}: {item}");
                    assert_eq!(
                        item["error"]["details"]["reason"],
                        json!("reply_budget"),
                        "{label}: {item}"
                    );
                    saw_cutoff = true;
                    next.push(remaining[idx].clone());
                }
            }
            assert!(b64_total <= REPLY_B64_LIMIT);
            assert!(next.len() < remaining.len());
            drop(line);
            drop(res);
            remaining = next;
        }

        assert_eq!(done, all_paths.len());
        let rss_delta = max_rss.saturating_sub(rss_before);
        eprintln!(
            "WORKER_REPLY_BUDGET {label}: calls={calls}, max_line={:.2} MiB, parent_rss_peak={:.2} MiB (delta={:.2} MiB)",
            max_line as f64 / (1024.0 * 1024.0),
            max_rss as f64 / (1024.0 * 1024.0),
            rss_delta as f64 / (1024.0 * 1024.0),
        );
        assert!(
            rss_delta < 256 * 1024 * 1024,
            "{label}: parent RSS grew by {:.1} MiB (expected << 622 MiB)",
            rss_delta as f64 / (1024.0 * 1024.0)
        );
        calls
    }

    // 1. 32 songs with ~10 MiB noisy JPEG covers at size=1024 (real out-of-process workers).
    {
        let dir1 = tempfile::tempdir().unwrap();
        let home1 = dir1.path().join("home");
        fs::create_dir_all(&home1).unwrap();
        let node1 = Node::new(
            Paths::under(dir1.path()).with_home(&home1),
            legion_tools::registry(),
        );
        let song_paths = {
            let noisy_jpg = af::build_noisy_10mib_jpeg_cover();
            let base_mp3 = af::build_mp3_with_raw_cover("Noisy cover", "image/jpeg", &noisy_jpg);
            let mut paths = Vec::with_capacity(32);
            for i in 0..32usize {
                let p = home1.join(format!("song_{i:02}.mp3"));
                fs::write(&p, &base_mp3).unwrap();
                paths.push(p.to_string_lossy().into_owned());
            }
            paths
        };
        let song_calls = drain_with_rss(
            &node1,
            &song_paths,
            1024,
            "cover",
            "32x 10 MiB noisy JPEG song covers @1024",
        )
        .await;
        assert!(song_calls >= 2);
    }

    // 2. 32 transparent noise PNGs at size=512 (real out-of-process workers).
    {
        let dir2 = tempfile::tempdir().unwrap();
        let home2 = dir2.path().join("home");
        fs::create_dir_all(&home2).unwrap();
        let node2 = Node::new(
            Paths::under(dir2.path()).with_home(&home2),
            legion_tools::registry(),
        );
        let png512_paths = {
            let noise_png512 = af::build_transparent_noise_png(512, 512, 11);
            let mut paths = Vec::with_capacity(32);
            for i in 0..32usize {
                let p = home2.join(format!("alpha512_{i:02}.png"));
                fs::write(&p, &noise_png512).unwrap();
                paths.push(p.to_string_lossy().into_owned());
            }
            paths
        };
        let png512_calls = drain_with_rss(
            &node2,
            &png512_paths,
            512,
            "contain",
            "32x transparent noise PNGs @512",
        )
        .await;
        assert!(png512_calls >= 3);
    }

    // 3. 32 transparent noise PNGs at size=1024 (real out-of-process workers; single worst-case
    //    reply previously hit 179 MB / 622 MiB node peak).
    {
        let dir3 = tempfile::tempdir().unwrap();
        let home3 = dir3.path().join("home");
        fs::create_dir_all(&home3).unwrap();
        let node3 = Node::new(
            Paths::under(dir3.path()).with_home(&home3),
            legion_tools::registry(),
        );
        let png1024_paths = {
            let noise_png1024 = af::build_transparent_noise_png(1024, 1024, 22);
            let mut paths = Vec::with_capacity(32);
            for i in 0..32usize {
                let p = home3.join(format!("alpha1024_{i:02}.png"));
                fs::write(&p, &noise_png1024).unwrap();
                paths.push(p.to_string_lossy().into_owned());
            }
            paths
        };
        let png1024_calls = drain_with_rss(
            &node3,
            &png1024_paths,
            1024,
            "contain",
            "32x transparent noise PNGs @1024",
        )
        .await;
        assert!(png1024_calls >= 16);
    }
}
