//! Dedicated test binary with a counting global allocator that measures real peak heap
//! allocation across every supported image format/subtype and verifies `fs.thumb`'s 256 MiB
//! per-image cap, 50 MP baseline JPEG admission, embedded EXIF/MPF fallback, and hostile metadata
//! handling.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/audio_fixtures.rs"]
mod audio_fixtures;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::bmp::BmpEncoder;
use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{
    DynamicImage, ExtendedColorType, Frame, ImageBuffer, ImageDecoder, ImageEncoder, ImageFormat,
    ImageReader, Limits, Rgb, Rgba,
};
use legion_core::{Call, Door, Node, Paths};
use serde_json::json;

struct CountingAlloc;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Serialize tests in this binary because `CountingAlloc` tracks process-wide allocations.
static MEASURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn reset_peak() {
    let live = LIVE_BYTES.load(Ordering::SeqCst);
    PEAK_BYTES.store(live, Ordering::SeqCst);
}

fn peak_delta_bytes() -> usize {
    let live = LIVE_BYTES.load(Ordering::SeqCst);
    let peak = PEAK_BYTES.load(Ordering::SeqCst);
    peak.saturating_sub(live)
}

fn peak_total_bytes() -> usize {
    PEAK_BYTES.load(Ordering::SeqCst)
}

#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: delegates directly to the system allocator with the caller's layout.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            let live = LIVE_BYTES.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        // SAFETY: delegates directly to the system allocator for a pointer it allocated.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: delegates directly to the system allocator with the caller's layout and size.
        let out = unsafe { System.realloc(ptr, layout, new_size) };
        if !out.is_null() {
            if new_size >= layout.size() {
                let diff = new_size - layout.size();
                let live = LIVE_BYTES.fetch_add(diff, Ordering::Relaxed) + diff;
                PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        out
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

/// Builds a minimal valid 3-component JPEG (baseline SOF0 or progressive SOF2, 4:2:0 or 4:4:4) of
/// `width x height` pixels with all-zero coefficients.
fn build_jpeg(width: u16, height: u16, progressive: bool, subsampling_420: bool) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 67, 0x00]);
    out.extend_from_slice(&[16u8; 64]);

    let sof = if progressive { 0xC2 } else { 0xC0 };
    out.extend_from_slice(&[0xFF, sof, 0x00, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(3);
    let y_samp = if subsampling_420 { 0x22 } else { 0x11 };
    out.extend_from_slice(&[1, y_samp, 0]);
    out.extend_from_slice(&[2, 0x11, 0]);
    out.extend_from_slice(&[3, 0x11, 0]);

    // DHT DC table 0: symbol 0 -> 1-bit `0`
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x00]);
    let mut dc_counts = [0u8; 16];
    dc_counts[0] = 1;
    out.extend_from_slice(&dc_counts);
    out.push(0x00);

    if !progressive {
        // DHT AC table 0: symbol 0 (EOB) -> 1-bit `0`
        out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x10]);
        let mut ac_counts = [0u8; 16];
        ac_counts[0] = 1;
        out.extend_from_slice(&ac_counts);
        out.push(0x00);
    }

    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 12, 3]);
    for id in 1u8..=3 {
        out.extend_from_slice(&[id, 0x00]);
    }
    if progressive {
        out.extend_from_slice(&[0x00, 0x00, 0x00]);
    } else {
        out.extend_from_slice(&[0x00, 0x3F, 0x00]);
    }

    let (mcu_w, mcu_h, blocks_per_mcu) = if subsampling_420 {
        (16usize, 16usize, 6usize)
    } else {
        (8usize, 8usize, 3usize)
    };
    let mcus = usize::from(width).div_ceil(mcu_w) * usize::from(height).div_ceil(mcu_h);
    let bits_per_block = if progressive { 1 } else { 2 };
    let zero_bytes = (mcus * blocks_per_mcu * bits_per_block).div_ceil(8);
    out.resize(out.len() + zero_bytes, 0x00);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

fn encode_quadrant_jpeg(width: u32, height: u32, corner_rgb: [u8; 3]) -> Vec<u8> {
    let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let color = if x < width / 4 && y < height / 4 {
                Rgb(corner_rgb)
            } else {
                Rgb([20, 40, 230])
            };
            img.put_pixel(x, y, color);
        }
    }
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 92)
        .encode(&img, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    jpeg
}

fn build_exif_app1(orientation: u16, thumb_offset: u32, thumb_len: u32, extra: &[u8]) -> Vec<u8> {
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42u16.to_le_bytes());
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&0u16.to_le_bytes());
    tiff.extend_from_slice(&26u32.to_le_bytes());
    tiff.extend_from_slice(&2u16.to_le_bytes());
    tiff.extend_from_slice(&0x0201u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&thumb_offset.to_le_bytes());
    tiff.extend_from_slice(&0x0202u16.to_le_bytes());
    tiff.extend_from_slice(&4u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&thumb_len.to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(tiff.len(), 56);
    tiff.extend_from_slice(extra);

    let mut seg = Vec::new();
    seg.extend_from_slice(&[0xFF, 0xE1]);
    let seg_len = (2 + 6 + tiff.len()) as u16;
    seg.extend_from_slice(&seg_len.to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &b in bytes {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    (s2 << 16) | s1
}

fn build_interlaced_png(width: u32, height: u32, bit_depth: u8, color_type: u8) -> Vec<u8> {
    let channels: usize = if color_type == 6 { 4 } else { 3 };
    let bpp = channels * usize::from(bit_depth / 8);
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
        raw.resize(raw.len() + ph * (1 + pw * bpp), 0u8);
    }

    let mut zlib = Vec::with_capacity(raw.len() + (raw.len() / 65535 + 1) * 5 + 6);
    zlib.extend_from_slice(&[0x78, 0x01]);
    let mut pos = 0;
    loop {
        let remaining = raw.len() - pos;
        let chunk_len = remaining.min(65535);
        let is_final = pos + chunk_len == raw.len();
        zlib.push(if is_final { 0x01 } else { 0x00 });
        let len16 = chunk_len as u16;
        zlib.extend_from_slice(&len16.to_le_bytes());
        zlib.extend_from_slice(&(!len16).to_le_bytes());
        zlib.extend_from_slice(&raw[pos..pos + chunk_len]);
        pos += chunk_len;
        if is_final {
            break;
        }
    }
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut out = Vec::new();
    out.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
    let push_chunk = |out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(tag);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc32_ieee(&out[start..]).to_be_bytes());
    };
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, color_type, 0, 0, 1]);
    push_chunk(&mut out, b"IHDR", &ihdr);
    push_chunk(&mut out, b"IDAT", &zlib);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

fn measure_decode_bpp(label: &str, bytes: Vec<u8>, format: ImageFormat, pixels: u64) -> f64 {
    let input_len = bytes.len();
    reset_peak();
    let t0 = Instant::now();

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(65_535);
    limits.max_image_height = Some(65_535);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().unwrap();
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let img = DynamicImage::from_decoder(decoder).unwrap();
    let mut resized = if img.width() >= 512 && img.height() >= 512 {
        img.thumbnail(256, 256)
    } else {
        img.resize(256, 256, FilterType::Triangle)
    };
    drop(img);
    resized.apply_orientation(orientation);
    let rgb = resized.to_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 82)
        .encode(&rgb, rgb.width(), rgb.height(), ExtendedColorType::Rgb8)
        .unwrap();
    drop(out);
    drop(rgb);
    drop(resized);

    let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let delta = peak_delta_bytes();
    let decode_only = delta.saturating_sub(input_len);
    let decode_bpp = decode_only as f64 / pixels as f64;
    eprintln!(
        "{label:<28} | {:>4.1} MP | peak={:>6.2} MiB (decode={:>5.2} B/px) | {:>6.1} ms",
        pixels as f64 / 1_000_000.0,
        delta as f64 / (1024.0 * 1024.0),
        decode_bpp,
        elapsed_ms
    );
    decode_bpp
}

#[tokio::test]
async fn baseline_50mp_is_admitted_under_cap_and_over_cap_progressive_falls_back_or_rejects() {
    let _guard = MEASURE_LOCK.lock().await;
    // Memory/admission tests must not flake on wall-clock time on slow shared runners.
    struct TimeoutReset;
    impl Drop for TimeoutReset {
        fn drop(&mut self) {
            legion_tools::fs::set_thumb_decode_timeout_for_test(None);
        }
    }
    legion_tools::fs::set_thumb_decode_timeout_for_test(Some(std::time::Duration::from_secs(120)));
    let _reset = TimeoutReset;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // 1. 50 MP (8000 x 6250) baseline 4:2:0 JPEG: admitted and stays well below 256 MiB.
    let base_50_path = home.join("base_50mp.jpg");
    std::fs::write(&base_50_path, build_jpeg(8000, 6250, false, true)).unwrap();

    // 2. 25 MP (5000 x 5000) progressive 4:4:4 JPEG: admitted and stays below 256 MiB.
    let prog_25_path = home.join("prog_25mp.jpg");
    std::fs::write(&prog_25_path, build_jpeg(5000, 5000, true, false)).unwrap();

    // 3. 36 MP (6000 x 6000) progressive 4:4:4 JPEG without embedded preview: rejected as
    // `too_large` with negligible allocation (< 4 MiB).
    let prog_36_path = home.join("prog_36mp.jpg");
    let prog_36_bytes = build_jpeg(6000, 6000, true, false);
    std::fs::write(&prog_36_path, &prog_36_bytes).unwrap();

    // 4. 36 MP progressive 4:4:4 JPEG WITH an embedded EXIF IFD1 thumbnail and main Orientation=6
    // (Rotate90): returns the rotated thumbnail without decoding the 36 MP main image!
    let embedded_thumb = encode_quadrant_jpeg(200, 100, [240, 20, 20]);
    let app1 = build_exif_app1(6, 56, embedded_thumb.len() as u32, &embedded_thumb);
    let mut prog_36_with_exif = Vec::new();
    prog_36_with_exif.extend_from_slice(&prog_36_bytes[..2]);
    prog_36_with_exif.extend_from_slice(&app1);
    prog_36_with_exif.extend_from_slice(&prog_36_bytes[2..]);
    let prog_36_exif_path = home.join("prog_36mp_exif.jpg");
    std::fs::write(&prog_36_exif_path, &prog_36_with_exif).unwrap();

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    // 50 MP baseline JPEG
    reset_peak();
    let res_50 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [base_50_path.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let peak_50_mib = peak_total_bytes() as f64 / (1024.0 * 1024.0);
    let delta_50_mib = peak_delta_bytes() as f64 / (1024.0 * 1024.0);
    eprintln!(
        "50 MP baseline 4:2:0 JPEG: peak_total={peak_50_mib:.1} MiB, peak_delta={delta_50_mib:.1} MiB"
    );
    assert!(
        res_50["results"][0].get("ok").is_some(),
        "50 MP baseline JPEG must succeed: {res_50}"
    );
    assert_eq!(res_50["results"][0]["ok"]["width"], json!(256));
    assert_eq!(res_50["results"][0]["ok"]["height"], json!(200));
    assert!(
        peak_total_bytes() < 256 * 1024 * 1024,
        "50 MP baseline JPEG peaked at {peak_50_mib:.1} MiB (must stay < 256 MiB)"
    );

    // 25 MP progressive 4:4:4 JPEG
    reset_peak();
    let res_25 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [prog_25_path.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let peak_25_mib = peak_total_bytes() as f64 / (1024.0 * 1024.0);
    eprintln!("25 MP progressive 4:4:4 JPEG: peak_total={peak_25_mib:.1} MiB");
    assert!(
        res_25["results"][0].get("ok").is_some(),
        "25 MP progressive JPEG must succeed: {res_25}"
    );
    assert!(
        peak_total_bytes() < 256 * 1024 * 1024,
        "25 MP progressive JPEG peaked at {peak_25_mib:.1} MiB (must stay < 256 MiB)"
    );

    // 36 MP progressive 4:4:4 JPEG without embedded thumbnail -> `too_large`
    reset_peak();
    let res_36 = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [prog_36_path.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let delta_36_mib = peak_delta_bytes() as f64 / (1024.0 * 1024.0);
    eprintln!("36 MP progressive 4:4:4 JPEG (rejected): peak_delta={delta_36_mib:.2} MiB");
    assert_eq!(res_36["results"][0]["error"]["code"], json!("failed"));
    assert_eq!(
        res_36["results"][0]["error"]["details"]["reason"],
        json!("too_large"),
        "{res_36}"
    );
    assert!(
        peak_delta_bytes() < 4 * 1024 * 1024,
        "rejected 36 MP progressive JPEG allocated {delta_36_mib:.2} MiB"
    );

    // 36 MP progressive 4:4:4 JPEG WITH EXIF IFD1 thumbnail + Rotate90 -> succeeds cheaply!
    reset_peak();
    let res_36_exif = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [prog_36_exif_path.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let delta_exif_mib = peak_delta_bytes() as f64 / (1024.0 * 1024.0);
    eprintln!("36 MP progressive JPEG via EXIF IFD1 fallback: peak_delta={delta_exif_mib:.2} MiB");
    assert!(
        res_36_exif["results"][0].get("ok").is_some(),
        "over-cap progressive JPEG with EXIF thumbnail should succeed: {res_36_exif}"
    );
    assert_eq!(res_36_exif["results"][0]["ok"]["width"], json!(100));
    assert_eq!(res_36_exif["results"][0]["ok"]["height"], json!(200));
    let b64 = res_36_exif["results"][0]["ok"]["content"].as_str().unwrap();
    let decoded = image::load_from_memory(&BASE64.decode(b64).unwrap())
        .unwrap()
        .to_rgb8();
    let top_right = decoded.get_pixel(95, 5);
    assert!(
        top_right.0[0] > 180 && top_right.0[2] < 80,
        "expected Rotate90 red corner, got {top_right:?}"
    );
    assert!(
        peak_delta_bytes() < 8 * 1024 * 1024,
        "EXIF fallback allocated {delta_exif_mib:.2} MiB"
    );
}

#[tokio::test]
async fn thumb_reply_budget_caps_32_noisy_covers_and_transparent_pngs_under_16mib_line() {
    use audio_fixtures as af;

    let _guard = MEASURE_LOCK.lock().await;
    const LINK_LINE_LIMIT: usize = 16 * 1024 * 1024;
    const REPLY_B64_LIMIT: usize = 12 * 1024 * 1024;

    // Helper: drains a 32-path batch by re-asking any `busy (reply_budget)` paths, asserting every
    // reply line stays strictly < 16 MiB and total base64 stays <= 12 MiB, returning `(calls,
    // max_line_bytes, max_b64_bytes, peak_heap_delta_bytes)`.
    async fn drain_batch(
        node: &Node,
        all_paths: &[String],
        size: u32,
        fit: &str,
    ) -> (usize, usize, usize, usize) {
        let mut remaining: Vec<String> = all_paths.to_vec();
        let mut completed = std::collections::HashSet::new();
        let mut calls = 0usize;
        let mut max_line_bytes = 0usize;
        let mut max_b64_bytes = 0usize;
        let mut peak_heap_delta = 0usize;

        while !remaining.is_empty() {
            calls += 1;
            assert!(
                calls <= all_paths.len(),
                "reply budget must make progress on every call"
            );
            let live_before = LIVE_BYTES.load(Ordering::SeqCst);
            reset_peak();
            let res = node
                .call(Call::new(
                    Door::Cli,
                    "fs.thumb",
                    json!({"paths": remaining, "size": size, "fit": fit}),
                ))
                .await
                .unwrap();
            let serialized_line = serde_json::to_string(&res).unwrap();
            let call_peak_delta = peak_total_bytes().saturating_sub(live_before);
            peak_heap_delta = peak_heap_delta.max(call_peak_delta);
            max_line_bytes = max_line_bytes.max(serialized_line.len());
            assert!(
                serialized_line.len() < LINK_LINE_LIMIT,
                "serialized fs.thumb reply line ({} bytes) exceeded 16 MiB link limit",
                serialized_line.len()
            );

            let items = res["results"].as_array().unwrap();
            assert_eq!(items.len(), remaining.len());

            let mut total_b64 = 0usize;
            let mut saw_budget_cutoff = false;
            let mut next_remaining = Vec::new();
            for (idx, item) in items.iter().enumerate() {
                assert_eq!(item["path"], json!(remaining[idx]));
                if let Some(ok) = item.get("ok") {
                    assert!(
                        !saw_budget_cutoff,
                        "no later path may succeed once reply_budget cutoff triggers at {idx}"
                    );
                    total_b64 += ok["content"].as_str().unwrap().len();
                    assert!(completed.insert(remaining[idx].clone()));
                } else {
                    let err = &item["error"];
                    assert_eq!(err["code"], json!("busy"), "unexpected error: {item}");
                    assert_eq!(
                        err["details"]["reason"],
                        json!("reply_budget"),
                        "expected reply_budget reason: {item}"
                    );
                    assert!(
                        err["message"].as_str().unwrap().contains("again"),
                        "reply_budget message must tell caller to ask again: {item}"
                    );
                    saw_budget_cutoff = true;
                    next_remaining.push(remaining[idx].clone());
                }
            }
            assert!(
                total_b64 <= REPLY_B64_LIMIT,
                "base64 payload ({total_b64} bytes) exceeded 12 MiB cap"
            );
            assert!(
                next_remaining.len() < remaining.len(),
                "every call must complete at least one thumbnail"
            );
            max_b64_bytes = max_b64_bytes.max(total_b64);
            remaining = next_remaining;
        }

        assert_eq!(completed.len(), all_paths.len());
        (calls, max_line_bytes, max_b64_bytes, peak_heap_delta)
    }

    // 1. 32 transparent noise PNGs at size=512 (previously 40.9 MiB reply line) and size=1024
    //    (previously 179 MB reply line / 622 MiB node peak).
    {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let node = Node::new(
            Paths::under(dir.path()).with_home(&home),
            legion_tools::registry(),
        );

        let png512 = af::build_transparent_noise_png(512, 512, 1);
        let mut paths512 = Vec::with_capacity(32);
        for i in 0..32usize {
            let p = home.join(format!("noise512_{i:02}.png"));
            std::fs::write(&p, &png512).unwrap();
            paths512.push(p.to_string_lossy().into_owned());
        }
        let (calls512, line512, b64_512, peak512) =
            drain_batch(&node, &paths512, 512, "contain").await;
        assert!(
            calls512 >= 3,
            "32x 512px transparent noise PNGs must split across multiple calls (got {calls512})"
        );
        eprintln!(
            "REPLY_BUDGET 32x 512px transparent noise PNGs: calls={calls512}, max_line={:.2} MiB, max_b64={:.2} MiB, in_proc_peak_heap={:.2} MiB",
            line512 as f64 / (1024.0 * 1024.0),
            b64_512 as f64 / (1024.0 * 1024.0),
            peak512 as f64 / (1024.0 * 1024.0),
        );

        let png1024 = af::build_transparent_noise_png(1024, 1024, 2);
        let mut paths1024 = Vec::with_capacity(32);
        for i in 0..32usize {
            let p = home.join(format!("noise1024_{i:02}.png"));
            std::fs::write(&p, &png1024).unwrap();
            paths1024.push(p.to_string_lossy().into_owned());
        }
        // Verify first call + re-ask on 1024px transparent noise PNGs (measuring full 32-path call
        // peak heap delta including JSON line serialization).
        let live_before_1024 = LIVE_BYTES.load(Ordering::SeqCst);
        reset_peak();
        let res1024 = node
            .call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": paths1024, "size": 1024, "fit": "contain"}),
            ))
            .await
            .unwrap();
        let line1024 = serde_json::to_string(&res1024).unwrap();
        let peak1024 = peak_total_bytes().saturating_sub(live_before_1024);
        assert!(line1024.len() < LINK_LINE_LIMIT);
        let ok_count = res1024["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|it| it.get("ok").is_some())
            .count();
        assert!((1..=2).contains(&ok_count), "ok_count={ok_count}");
        assert_eq!(res1024["results"][ok_count]["error"]["code"], json!("busy"));
        assert_eq!(
            res1024["results"][ok_count]["error"]["details"]["reason"],
            json!("reply_budget")
        );
        eprintln!(
            "REPLY_BUDGET 32x 1024px transparent noise PNGs (call 1): ok={ok_count}/32, line={:.2} MiB, in_proc_peak_heap={:.2} MiB",
            line1024.len() as f64 / (1024.0 * 1024.0),
            peak1024 as f64 / (1024.0 * 1024.0),
        );
    }

    // 2. 32 songs with noisy ~10 MiB JPEG covers at size=1024 (previously 31.8 MiB reply line).
    {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let node = Node::new(
            Paths::under(dir.path()).with_home(&home),
            legion_tools::registry(),
        );
        let noisy_jpg = af::build_noisy_10mib_jpeg_cover();
        let base_mp3 = af::build_mp3_with_raw_cover("Noisy cover", "image/jpeg", &noisy_jpg);
        let mut song_paths = Vec::with_capacity(32);
        for i in 0..32usize {
            let p = home.join(format!("noisy_song_{i:02}.mp3"));
            std::fs::write(&p, &base_mp3).unwrap();
            song_paths.push(p.to_string_lossy().into_owned());
        }
        let (calls_songs, line_songs, b64_songs, peak_songs) =
            drain_batch(&node, &song_paths, 1024, "cover").await;
        assert!(
            calls_songs >= 2,
            "32x 10 MiB noisy JPEG covers at 1024 must split across multiple calls (got {calls_songs})"
        );
        eprintln!(
            "REPLY_BUDGET 32x 10 MiB noisy JPEG song covers at 1024: calls={calls_songs}, max_line={:.2} MiB, max_b64={:.2} MiB, in_proc_peak_heap={:.2} MiB",
            line_songs as f64 / (1024.0 * 1024.0),
            b64_songs as f64 / (1024.0 * 1024.0),
            peak_songs as f64 / (1024.0 * 1024.0),
        );
    }
}

#[tokio::test]
async fn format_bytes_per_pixel_stays_within_documented_factors() {
    let _guard = MEASURE_LOCK.lock().await;
    let w = 1000u32;
    let h = 1000u32;
    let px = u64::from(w) * u64::from(h);

    assert!(
        measure_decode_bpp(
            "jpeg_baseline_420",
            build_jpeg(1000, 1000, false, true),
            ImageFormat::Jpeg,
            px
        ) <= 3.5
    );
    assert!(
        measure_decode_bpp(
            "jpeg_baseline_444",
            build_jpeg(1000, 1000, false, false),
            ImageFormat::Jpeg,
            px
        ) <= 3.5
    );
    assert!(
        measure_decode_bpp(
            "jpeg_progressive_420",
            build_jpeg(1000, 1000, true, true),
            ImageFormat::Jpeg,
            px
        ) <= 6.5
    );
    assert!(
        measure_decode_bpp(
            "jpeg_progressive_444",
            build_jpeg(1000, 1000, true, false),
            ImageFormat::Jpeg,
            px
        ) <= 9.5
    );

    let rgb8 = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_pixel(w, h, Rgb([100, 150, 200]));
    let mut png_rgb8 = Vec::new();
    PngEncoder::new(&mut png_rgb8)
        .write_image(&rgb8, w, h, ExtendedColorType::Rgb8)
        .unwrap();
    assert!(measure_decode_bpp("png_rgb8", png_rgb8, ImageFormat::Png, px) <= 3.5);

    let rgba8 = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(w, h, Rgba([100, 150, 200, 255]));
    let mut png_rgba8 = Vec::new();
    PngEncoder::new(&mut png_rgba8)
        .write_image(&rgba8, w, h, ExtendedColorType::Rgba8)
        .unwrap();
    assert!(measure_decode_bpp("png_rgba8", png_rgba8, ImageFormat::Png, px) <= 4.5);

    let rgb16 = vec![0u8; (px as usize) * 6];
    let mut png_rgb16 = Vec::new();
    PngEncoder::new(&mut png_rgb16)
        .write_image(&rgb16, w, h, ExtendedColorType::Rgb16)
        .unwrap();
    assert!(measure_decode_bpp("png_rgb16", png_rgb16, ImageFormat::Png, px) <= 6.5);

    let rgba16 = vec![0u8; (px as usize) * 8];
    let mut png_rgba16 = Vec::new();
    PngEncoder::new(&mut png_rgba16)
        .write_image(&rgba16, w, h, ExtendedColorType::Rgba16)
        .unwrap();
    assert!(measure_decode_bpp("png_rgba16", png_rgba16, ImageFormat::Png, px) <= 8.5);

    assert!(
        measure_decode_bpp(
            "png_interlaced_rgb8",
            build_interlaced_png(w, h, 8, 2),
            ImageFormat::Png,
            px
        ) <= 6.5
    );
    assert!(
        measure_decode_bpp(
            "png_interlaced_rgba8",
            build_interlaced_png(w, h, 8, 6),
            ImageFormat::Png,
            px
        ) <= 8.5
    );
    assert!(
        measure_decode_bpp(
            "png_interlaced_rgba16",
            build_interlaced_png(w, h, 16, 6),
            ImageFormat::Png,
            px
        ) <= 16.5
    );

    let mut gif_bytes = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut gif_bytes);
        enc.encode_frame(Frame::new(rgba8.clone())).unwrap();
    }
    assert!(measure_decode_bpp("gif", gif_bytes, ImageFormat::Gif, px) <= 5.5);

    let mut webp_ll = Vec::new();
    WebPEncoder::new_lossless(&mut webp_ll)
        .encode(&rgb8, w, h, ExtendedColorType::Rgb8)
        .unwrap();
    assert!(measure_decode_bpp("webp_lossless_rgb", webp_ll, ImageFormat::WebP, px) <= 7.5);

    let mut bmp_rgb = Vec::new();
    BmpEncoder::new(&mut bmp_rgb)
        .encode(&rgb8, w, h, ExtendedColorType::Rgb8)
        .unwrap();
    assert!(measure_decode_bpp("bmp_rgb24", bmp_rgb, ImageFormat::Bmp, px) <= 4.6);
}

#[tokio::test]
#[ignore = "run explicitly in --release mode for realistic photo decode timings"]
async fn benchmark_realistic_photo_timings_and_cold_32_batch() {
    let _guard = MEASURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // Generate realistic high-detail textured photos via ffmpeg (real non-zero AC coefficients,
    // realistic ~2-12 MiB file sizes) so timings reflect real phone/camera JPEGs.
    let make_photo = |name: &str, size: &str, extra: &[&str]| {
        let p = home.join(name);
        let src = format!("testsrc2=size={size}:duration=0.1:rate=10");
        let mut args = vec![
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            &src,
            "-frames:v",
            "1",
            "-q:v",
            "3",
        ];
        args.extend_from_slice(extra);
        let p_str = p.to_string_lossy().into_owned();
        args.push(&p_str);
        let st = std::process::Command::new("ffmpeg")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "ffmpeg failed for {name}");
        p
    };

    let p12 = make_photo("photo_12mp.jpg", "4000x3000", &["-pix_fmt", "yuvj420p"]);
    let p50 = make_photo("photo_50mp.jpg", "8000x6250", &["-pix_fmt", "yuvj420p"]);
    // Use pure-Rust progressive 4:4:4 JPEG (or magick/cjpeg if available for real photo texture).
    let p25_prog = home.join("photo_25mp_prog.jpg");
    let p25_base = make_photo(
        "photo_25mp_base.jpg",
        "5000x5000",
        &["-pix_fmt", "yuvj444p"],
    );
    let converted_prog = std::process::Command::new("magick")
        .args([
            p25_base.to_str().unwrap(),
            "-interlace",
            "Plane",
            "-sampling-factor",
            "4:4:4",
            "-quality",
            "88",
            p25_prog.to_str().unwrap(),
        ])
        .status()
        .is_ok_and(|s| s.success());
    if !converted_prog {
        std::fs::write(&p25_prog, build_jpeg(5000, 5000, true, false)).unwrap();
    }

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    for (label, path) in [
        ("12 MP baseline 4:2:0", &p12),
        ("50 MP baseline 4:2:0", &p50),
        ("25 MP progressive 4:4:4", &p25_prog),
    ] {
        let sz_kib = std::fs::metadata(path).unwrap().len() as f64 / 1024.0;
        reset_peak();
        let t0 = Instant::now();
        let res = node
            .call(Call::new(
                Door::Cli,
                "fs.thumb",
                json!({"paths": [path.to_string_lossy()], "size": 256}),
            ))
            .await
            .unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let peak_mib = peak_total_bytes() as f64 / (1024.0 * 1024.0);
        assert!(res["results"][0].get("ok").is_some(), "{label}: {res}");
        eprintln!("{label:<26} | file={sz_kib:>7.1} KiB | peak={peak_mib:>5.1} MiB | {ms:>6.1} ms");
    }

    // Cold 32-photo batch of 12 MP phone photos (32 distinct files, empty cache dir).
    let batch_dir = tempfile::tempdir().unwrap();
    let batch_home = batch_dir.path().join("home");
    std::fs::create_dir_all(&batch_home).unwrap();
    let base_bytes = std::fs::read(&p12).unwrap();
    let mut batch_paths = Vec::with_capacity(32);
    for i in 0..32 {
        let dst = batch_home.join(format!("img_{i:02}.jpg"));
        std::fs::write(&dst, &base_bytes).unwrap();
        batch_paths.push(dst.to_string_lossy().into_owned());
    }
    let batch_node = Node::new(
        Paths::under(batch_dir.path()).with_home(&batch_home),
        legion_tools::registry(),
    );
    reset_peak();
    let t0 = Instant::now();
    let res = batch_node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": batch_paths, "size": 256}),
        ))
        .await
        .unwrap();
    let batch_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let batch_peak_mib = peak_total_bytes() as f64 / (1024.0 * 1024.0);
    let items = res["results"].as_array().unwrap();
    assert_eq!(items.len(), 32);
    assert!(items.iter().all(|it| it.get("ok").is_some()));
    eprintln!(
        "32x 12 MP cold batch       | peak={batch_peak_mib:>5.1} MiB | total={batch_ms:>6.1} ms ({:.1} ms/photo)",
        batch_ms / 32.0
    );
}

#[tokio::test]
async fn property_mutated_and_truncated_audio_fixtures_hold_memory_and_time_bounds() {
    use audio_fixtures as af;

    let _guard = MEASURE_LOCK.lock().await;

    let cover = af::encode_solid_jpeg(96, 96, [200, 50, 50]);
    let small_seeds: Vec<(&str, Vec<u8>)> = vec![
        (
            "mp3",
            af::build_mp3_id3v23(&[("image/jpeg", 3, &cover)], true, true),
        ),
        ("mp3", af::build_mp3_id3v22(Some(&cover))),
        ("mp3", af::build_mp3_id3v24(Some(&cover))),
        ("mp3", af::build_mp3_id3v1()),
        ("wav", af::build_wav(true, Some(&cover), 2)),
        ("aiff", af::build_aiff(true, Some(&cover), 2)),
        (
            "flac",
            af::build_flac(&[("TITLE", "First song")], &[(3, "image/jpeg", &cover)], 3),
        ),
        (
            "ogg",
            af::build_ogg_vorbis(&[("TITLE", "First song")], &[(3, "image/jpeg", &cover)], 3),
        ),
        (
            "opus",
            af::build_ogg_opus(&[("TITLE", "First song")], &[(3, "image/jpeg", &cover)], 3),
        ),
        (
            "m4a",
            af::build_mp4(true, Some((13, &cover)), 4, false, false),
        ),
        (
            "m4b",
            af::build_mp4(true, Some((14, &cover)), 4, true, false),
        ),
    ];

    // Iteration-cap hostile seeds covering every loop bound in audio_tags.rs.
    let cap_seeds: Vec<(&str, &str, Vec<u8>)> = vec![
        ("mp3_frames", "mp3", af::build_mp3_many_id3_frames(4160)),
        ("wav_chunks", "wav", af::build_wav_many_chunks(4160)),
        ("aiff_chunks", "aiff", af::build_aiff_many_chunks(4160)),
        ("flac_blocks", "flac", af::build_flac_many_blocks(4160)),
        (
            "flac_vorbis",
            "flac",
            af::build_flac_many_vorbis_comments(65_664),
        ),
        ("ogg_scan", "ogg", af::build_ogg_many_scan_pages(80)),
        (
            "opus_comment_pages",
            "opus",
            af::build_ogg_many_comment_pages(2080),
        ),
        ("mp4_depth", "m4a", af::build_mp4_deep_udta(14)),
        ("mp4_boxes", "m4a", af::build_mp4_many_boxes(4160)),
    ];

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    let mut rng = 0xA1B2_C3D4_E5F6_0708u64;
    let mut next_u64 = || -> u64 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    // Each file is already bounded by `fs.tags`'s own per-file decode timeout and memory cap;
    // these generous limits exist strictly as hang guards so noisy CI runners never flake.
    const CALL_HANG_GUARD: Duration = Duration::from_secs(60);
    const OVERALL_HANG_GUARD: Duration = Duration::from_secs(300);

    let t0 = Instant::now();
    let live_start = LIVE_BYTES.load(Ordering::SeqCst);
    reset_peak();

    // 1. 2,200 mutated & truncated fixtures across all 11 formats (200 per seed) in 32-path
    //    batches through `fs.tags`.
    let mut cases = 0usize;
    let mut pending_batch: Vec<String> = Vec::with_capacity(32);
    for (seed_idx, (ext, base)) in small_seeds.iter().enumerate() {
        for step in 0..200usize {
            let mut mutated = base.clone();
            match step % 5 {
                0 => {
                    let cut = (next_u64() as usize) % (mutated.len().max(1));
                    mutated.truncate(cut);
                }
                1 => {
                    // Corrupt declared lengths near the front with 0xFF/0x7F patterns.
                    for _ in 0..4 {
                        if !mutated.is_empty() {
                            let off = (next_u64() as usize) % mutated.len().min(96);
                            mutated[off] = if (next_u64() & 1) == 0 { 0xFF } else { 0x7F };
                        }
                    }
                }
                2 => {
                    for _ in 0..8 {
                        if !mutated.is_empty() {
                            let off = (next_u64() as usize) % mutated.len();
                            mutated[off] ^= (next_u64() & 0xFF) as u8;
                        }
                    }
                }
                3 => {
                    let cut = (next_u64() as usize) % mutated.len().clamp(1, 128);
                    mutated.truncate(cut);
                    if mutated.len() >= 8 {
                        let off = (next_u64() as usize) % (mutated.len() - 4);
                        mutated[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
                    }
                }
                _ => {
                    // Inject control/BiDi/invisible/tag bytes into payloads.
                    if mutated.len() > 24 {
                        let off = 12 + ((next_u64() as usize) % (mutated.len() - 24));
                        let end = (off + 8).min(mutated.len());
                        mutated[off..end].copy_from_slice(
                            &[0xE2, 0x80, 0xAE, 0x00, 0xF3, 0xA0, 0x81, 0x81][..end - off],
                        );
                    }
                }
            }
            let p = home.join(format!("fuzz_{seed_idx}_{step}.{ext}"));
            std::fs::write(&p, &mutated).unwrap();
            pending_batch.push(p.to_string_lossy().into_owned());
            cases += 1;

            if pending_batch.len() == 32 {
                let res = tokio::time::timeout(
                    CALL_HANG_GUARD,
                    node.call(Call::new(
                        Door::Cli,
                        "fs.tags",
                        json!({"paths": pending_batch}),
                    )),
                )
                .await
                .expect("hang guard tripped: fs.tags fuzz batch did not finish within 60 s")
                .unwrap();
                let arr = res["results"].as_array().unwrap();
                assert_eq!(arr.len(), 32);
                for item in arr {
                    af::assert_clean_tags_item(item);
                }
                pending_batch.clear();
            }
        }
    }
    if !pending_batch.is_empty() {
        let res = tokio::time::timeout(
            CALL_HANG_GUARD,
            node.call(Call::new(
                Door::Cli,
                "fs.tags",
                json!({"paths": pending_batch}),
            )),
        )
        .await
        .expect("hang guard tripped: trailing fs.tags fuzz batch did not finish within 60 s")
        .unwrap();
        let arr = res["results"].as_array().unwrap();
        assert_eq!(arr.len(), pending_batch.len());
        for item in arr {
            af::assert_clean_tags_item(item);
        }
        pending_batch.clear();
    }

    // 2. Every iteration-cap hostile fixture intact + 24 mutated/truncated variants each (225 cases).
    for (name, ext, base) in &cap_seeds {
        let p = home.join(format!("cap_{name}_intact.{ext}"));
        std::fs::write(&p, base).unwrap();
        pending_batch.push(p.to_string_lossy().into_owned());
        cases += 1;
        for step in 0..24usize {
            let mut mutated = base.clone();
            if step.is_multiple_of(3) {
                let cut = (next_u64() as usize) % mutated.len().max(1);
                mutated.truncate(cut);
            } else if !mutated.is_empty() {
                let window = mutated.len().clamp(1, 256);
                for _ in 0..4 {
                    let off = (next_u64() as usize) % window;
                    mutated[off] ^= (next_u64() & 0xFF) as u8;
                }
            }
            let pm = home.join(format!("cap_{name}_{step}.{ext}"));
            std::fs::write(&pm, &mutated).unwrap();
            pending_batch.push(pm.to_string_lossy().into_owned());
            cases += 1;
            if pending_batch.len() == 32 {
                let res = tokio::time::timeout(
                    CALL_HANG_GUARD,
                    node.call(Call::new(
                        Door::Cli,
                        "fs.tags",
                        json!({"paths": pending_batch}),
                    )),
                )
                .await
                .expect("hang guard tripped: fs.tags cap-seed batch did not finish within 60 s")
                .unwrap();
                for item in res["results"].as_array().unwrap() {
                    af::assert_clean_tags_item(item);
                }
                pending_batch.clear();
            }
        }
    }
    if !pending_batch.is_empty() {
        let res = tokio::time::timeout(
            CALL_HANG_GUARD,
            node.call(Call::new(
                Door::Cli,
                "fs.tags",
                json!({"paths": pending_batch}),
            )),
        )
        .await
        .expect("hang guard tripped: trailing fs.tags cap-seed batch did not finish within 60 s")
        .unwrap();
        for item in res["results"].as_array().unwrap() {
            af::assert_clean_tags_item(item);
        }
        pending_batch.clear();
    }
    let small_and_cap_peak = peak_total_bytes().saturating_sub(live_start);

    // 3. Dedicated 16 MiB budget cases measured individually under CountingAlloc (ensuring every
    //    single worst-case 15.9 MiB & >16 MiB file stays strictly under the 48 MiB worker cap).
    let measure_single_tags = |label: &'static str, path: std::path::PathBuf| {
        let node_ref = &node;
        async move {
            let live_before = LIVE_BYTES.load(Ordering::SeqCst);
            reset_peak();
            let res = tokio::time::timeout(
                CALL_HANG_GUARD,
                node_ref.call(Call::new(
                    Door::Cli,
                    "fs.tags",
                    json!({"paths": [path.to_string_lossy()]}),
                )),
            )
            .await
            .unwrap_or_else(|_| {
                panic!("hang guard tripped: {label} did not finish within {CALL_HANG_GUARD:?}")
            })
            .unwrap();
            let peak = peak_total_bytes().saturating_sub(live_before);
            let item = res["results"][0].clone();
            af::assert_clean_tags_item(&item);
            eprintln!(
                "AUDIO_TAGS_MEM {label:<38} | in_proc_peak_heap={:.2} MiB | ok={}",
                peak as f64 / (1024.0 * 1024.0),
                item.get("ok").is_some(),
            );
            assert!(
                peak < 44 * 1024 * 1024,
                "{label} peaked at {:.2} MiB (must stay < 44 MiB / under 48 MiB worker cap)",
                peak as f64 / (1024.0 * 1024.0)
            );
            (item, peak)
        }
    };

    // 3a. 15.9 MiB line-wrapped base64 picture in Opus (strictly in budget).
    let opus_b64_path = home.join("budget_15_9mib_wrapped_b64.opus");
    {
        let data = af::build_opus_15_9mib_wrapped_base64_picture();
        std::fs::write(&opus_b64_path, &data).unwrap();
    }
    let (item_b64, _) =
        measure_single_tags("15.9 MiB wrapped base64 pic (Opus)", opus_b64_path).await;
    assert_eq!(item_b64["ok"]["cover"], json!(true));
    cases += 1;

    // 3b. 15.9 MiB invalid UTF-8 (0xFF) in Opus, MP3 ID3v2.4, and M4A (strictly in budget).
    let invalid_utf8 = vec![0xFFu8; 15_900_000];
    let opus_inv_path = home.join("budget_15_9mib_invalid_utf8.opus");
    std::fs::write(
        &opus_inv_path,
        af::build_ogg_opus_raw_comments(&[(b"TITLE", &invalid_utf8), (b"ARTIST", b"Someone")], 6),
    )
    .unwrap();
    let mp3_inv_path = home.join("budget_15_9mib_invalid_utf8.mp3");
    std::fs::write(&mp3_inv_path, af::build_mp3_id3v24_raw_title(&invalid_utf8)).unwrap();
    let m4a_inv_path = home.join("budget_15_9mib_invalid_utf8.m4a");
    std::fs::write(&m4a_inv_path, af::build_m4a_raw_title(&invalid_utf8)).unwrap();
    drop(invalid_utf8);

    for (label, path) in [
        ("15.9 MiB invalid UTF-8 (Opus)", opus_inv_path),
        ("15.9 MiB invalid UTF-8 (MP3 v2.4)", mp3_inv_path),
        ("15.9 MiB invalid UTF-8 (M4A)", m4a_inv_path),
    ] {
        let (item, _) = measure_single_tags(label, path).await;
        assert_eq!(
            item["ok"]["title"].as_str().unwrap().chars().count(),
            512,
            "{label}: {item}"
        );
        cases += 1;
    }

    // 3c. Near-16 MiB global-unsynced ID3v2.3 MP3 (in budget) & over-16 MiB seeds across formats.
    let mp3_unsync_near = home.join("budget_15_8mib_unsync.mp3");
    {
        let data = af::build_mp3_global_unsync_sized(15_800_000);
        std::fs::write(&mp3_unsync_near, &data).unwrap();
    }
    let (item_unsync, _) = measure_single_tags("15.8 MiB global-unsync MP3", mp3_unsync_near).await;
    assert_eq!(item_unsync["ok"]["title"], json!("Sized unsync song"));
    cases += 1;

    let over_cases: Vec<(&'static str, std::path::PathBuf)> = vec![
        ("16.5 MiB global-unsync MP3 (over)", {
            let p = home.join("budget_16_5mib_over.mp3");
            let data = af::build_mp3_global_unsync_sized(16 * 1024 * 1024 + 512 * 1024);
            std::fs::write(&p, &data).unwrap();
            p
        }),
        ("16.4 MiB cumulative FLAC pics (over)", {
            let p = home.join("budget_16_4mib_over.flac");
            let data = af::build_flac_cumulative_pictures(4, 4_200_000);
            std::fs::write(&p, &data).unwrap();
            p
        }),
        ("16.5 MiB MP4 covr (over)", {
            let p = home.join("budget_16_5mib_over.m4a");
            let data = af::build_mp4_sized_covr(16 * 1024 * 1024 + 512 * 1024);
            std::fs::write(&p, &data).unwrap();
            p
        }),
    ];
    for (label, path) in over_cases {
        let (item, _) = measure_single_tags(label, path).await;
        assert_eq!(
            item["error"]["details"]["reason"],
            json!("tag_too_large"),
            "{label}: {item}"
        );
        cases += 1;
    }

    // 3d. Mutate headers of a 16 MiB MP3 in-place 16 times and run through `fs.tags`.
    {
        let mut big_mp3 = af::build_mp3_global_unsync_sized(15_800_000);
        let orig_hdr: [u8; 64] = big_mp3[..64].try_into().unwrap();
        for step in 0..16usize {
            big_mp3[..64].copy_from_slice(&orig_hdr);
            for _ in 0..4 {
                let off = (next_u64() as usize) % 64;
                big_mp3[off] = if step.is_multiple_of(2) {
                    0xFF
                } else {
                    (next_u64() & 0xFF) as u8
                };
            }
            let pm = home.join(format!("budget_mut_{step}.mp3"));
            std::fs::write(&pm, &big_mp3).unwrap();
            let live_before = LIVE_BYTES.load(Ordering::SeqCst);
            reset_peak();
            let res = tokio::time::timeout(
                CALL_HANG_GUARD,
                node.call(Call::new(
                    Door::Cli,
                    "fs.tags",
                    json!({"paths": [pm.to_string_lossy()]}),
                )),
            )
            .await
            .expect("hang guard tripped: mutated 16 MiB MP3 did not finish within 60 s")
            .unwrap();
            let peak = peak_total_bytes().saturating_sub(live_before);
            af::assert_clean_tags_item(&res["results"][0]);
            assert!(
                peak < 44 * 1024 * 1024,
                "mutated 16 MiB MP3 #{step} peaked at {:.2} MiB",
                peak as f64 / (1024.0 * 1024.0)
            );
            let _ = std::fs::remove_file(&pm);
            cases += 1;
        }
    }

    let elapsed = t0.elapsed();
    eprintln!(
        "audio property fuzz: {cases} fixtures (small+cap+16MiB) in {:.2} s, small+cap peak heap = {:.2} MiB",
        elapsed.as_secs_f64(),
        small_and_cap_peak as f64 / (1024.0 * 1024.0)
    );
    assert!(
        cases >= 2_400,
        "expected >= 2,400 property fixtures, ran {cases}"
    );
    assert!(
        elapsed < OVERALL_HANG_GUARD,
        "hang guard tripped: mutated audio property test ran for {elapsed:?} (limit {OVERALL_HANG_GUARD:?})"
    );
    assert!(
        small_and_cap_peak < 64 * 1024 * 1024,
        "small+cap property batch exceeded 64 MiB heap delta: {} bytes",
        small_and_cap_peak
    );
}
