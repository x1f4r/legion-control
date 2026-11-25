//! Dedicated test binary with a counting global allocator that measures the real peak heap
//! allocation of `fs.thumb` decoding a large progressive JPEG at the 25 MP ceiling and verifies
//! that a progressive JPEG just above the ceiling is rejected before allocating pixel buffers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use legion_core::{Call, Door, Node, Paths};
use serde_json::json;

struct CountingAlloc;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

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

/// Builds a tiny, valid 3-component 4:4:4 progressive JPEG (SOF2 `0xFFC2`) of `width x height`
/// pixels with all-zero DC coefficients so `zune-jpeg` runs its full progressive coefficient
/// allocation and inverse-DCT pipeline on a <10 KiB input file (works on Linux, macOS and Windows
/// without external tools).
fn build_progressive_jpeg(width: u16, height: u16) -> Vec<u8> {
    let mut out = Vec::new();
    // SOI
    out.extend_from_slice(&[0xFF, 0xD8]);

    // DQT (table 0, 8-bit precision, all 16s)
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 67, 0x00]);
    out.extend_from_slice(&[16u8; 64]);

    // SOF2 (Progressive DCT, 8-bit, 3 components, 1x1 sampling -> 4:4:4 so all 3 planes are full
    // resolution, exercising worst-case progressive coefficient memory in zune-jpeg).
    out.extend_from_slice(&[0xFF, 0xC2, 0x00, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(3); // 3 components
    for id in 1u8..=3 {
        out.extend_from_slice(&[id, 0x11, 0]);
    }

    // DHT (DC table 0: symbol 0 -> 1-bit code `0`)
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 20, 0x00]);
    let mut dc_counts = [0u8; 16];
    dc_counts[0] = 1; // one 1-bit symbol
    out.extend_from_slice(&dc_counts);
    out.push(0x00); // category 0 (DC diff = 0)

    // SOS (DC scan for all 3 components: Ss=0, Se=0, Ah=0, Al=0)
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 12, 3]);
    for id in 1u8..=3 {
        out.extend_from_slice(&[id, 0x00]);
    }
    out.extend_from_slice(&[0x00, 0x00, 0x00]);

    // Each MCU has 3 blocks, each emitting a single `0` bit (DC diff = 0).
    let mcus = usize::from(width.div_ceil(8)) * usize::from(height.div_ceil(8));
    let zero_bits = mcus * 3;
    let zero_bytes = zero_bits.div_ceil(8);
    out.resize(out.len() + zero_bytes, 0x00);

    // EOI
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

#[tokio::test]
async fn progressive_jpeg_memory_is_bounded_and_above_cap_allocates_negligibly() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // 1. 25 MP (5000 x 5000) progressive JPEG (the exact maximum allowed): must succeed and stay
    // well below 260 MiB peak allocation (measured ~215 MiB on zune-jpeg).
    let allowed_path = home.join("prog_25mp.jpg");
    std::fs::write(&allowed_path, build_progressive_jpeg(5000, 5000)).unwrap();

    // 2. 36 MP (6000 x 6000) progressive JPEG (above the 25 MP ceiling): must fail at header check
    // with negligible peak heap allocation (< 4 MiB; measured ~0.4 MiB).
    let rejected_path = home.join("prog_36mp.jpg");
    std::fs::write(&rejected_path, build_progressive_jpeg(6000, 6000)).unwrap();

    let node = Node::new(
        Paths::under(dir.path()).with_home(&home),
        legion_tools::registry(),
    );

    reset_peak();
    let ok_res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [allowed_path.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let peak_25mp_mib = peak_total_bytes() as f64 / (1024.0 * 1024.0);
    let delta_25mp_mib = peak_delta_bytes() as f64 / (1024.0 * 1024.0);
    eprintln!(
        "25 MP progressive JPEG: peak_total={peak_25mp_mib:.1} MiB, peak_delta={delta_25mp_mib:.1} MiB"
    );
    assert!(
        ok_res["results"][0].get("ok").is_some(),
        "25 MP progressive JPEG should succeed: {ok_res}"
    );
    assert_eq!(ok_res["results"][0]["ok"]["width"], json!(256));
    assert_eq!(ok_res["results"][0]["ok"]["height"], json!(256));
    assert!(
        peak_total_bytes() < 260 * 1024 * 1024,
        "25 MP progressive JPEG peaked at {peak_25mp_mib:.1} MiB (expected < 260 MiB)"
    );

    // Measure peak allocation for a 36 MP progressive JPEG (> 25 MP cap) — must reject from the
    // header without allocating DCT or RGB buffers.
    reset_peak();
    let rejected_res = node
        .call(Call::new(
            Door::Cli,
            "fs.thumb",
            json!({"paths": [rejected_path.to_string_lossy()], "size": 256}),
        ))
        .await
        .unwrap();
    let delta_36mp_mib = peak_delta_bytes() as f64 / (1024.0 * 1024.0);
    eprintln!("36 MP progressive JPEG (rejected): peak_delta={delta_36mp_mib:.2} MiB");
    assert_eq!(
        rejected_res["results"][0]["error"]["code"],
        json!("failed"),
        "{rejected_res}"
    );
    assert!(
        peak_delta_bytes() < 4 * 1024 * 1024,
        "rejected 36 MP progressive JPEG allocated {delta_36mp_mib:.2} MiB (expected < 4 MiB)"
    );
}
