//! Node-side thumbnail generation (`fs.thumb`) for raster images and video posters.
//!
//! # Security & resource limits
//!
//! - **Authorization parity**: requires `fs.read` permission on the role and per-path `check_fs_path`
//!   confinement before touching the file system or the thumbnail cache.
//! - **Open-once non-blocking regular-file reads**: files are opened once non-blockingly via
//!   [`legion_os::open_regular::open_regular_read`] (`O_NONBLOCK | O_NOCTTY` plus `fstat` regular-
//!   file verification on Unix; disk-file verification on Windows) so swapping a file for a FIFO
//!   or special file can never block a node thread; the open handle's size, mtime, OS file
//!   identity (`dev`/`ino` or Windows volume+file index) and metadata change time (`ctime` / NTFS
//!   `ChangeTime`) are checked before and after reading so rename swaps cannot substitute another
//!   file.
//! - **Per-format estimated decode memory cap (256 MiB)**: rather than a blanket pixel cap that
//!   would reject 48–61 MP baseline phone/camera JPEGs, [`thumb_probe`](super::thumb_probe) reads
//!   the format and encoding subtype from the header before decoding and multiplies pixels by a
//!   counting-allocator-measured bytes-per-pixel factor (plus compressed source bytes and 4 MiB
//!   overhead). Images whose estimated peak fits within **256 MiB** are decoded directly (e.g.,
//!   50 MP baseline 4:2:0 JPEG peaks around ~145 MiB + source size; 25 MP progressive 4:4:4 JPEG
//!   peaks around ~215 MiB).
//! - **Exact cross-OS worker heap cap + matching in-flight budget**: each `__thumb-worker` arms
//!   [`legion_os::capped_alloc::set_memory_cap`] with its admitted heap cap before allocating and
//!   exits immediately with [`CAP_EXCEEDED_EXIT_CODE`](legion_os::capped_alloc::CAP_EXCEEDED_EXIT_CODE)
//!   on overshoot, backed by OS limits (`RLIMIT_AS` with headroom on Linux, `RLIMIT_DATA` on other
//!   non-macOS Unix, suspended-create Job Object limits on Windows) and a 10 ms RSS watchdog. Each
//!   worker reserves its exact resident limit (`heap_cap + 12 MiB`) from the shared in-flight byte
//!   pool so the sum of live workers' limits never exceeds `min(512 MiB, effective_ram / 8)`.
//! - **Embedded EXIF/MPF preview fallback**: when a JPEG exceeds the 256 MiB decode cap (such as a
//!   200 MP photo or a 36+ MP progressive 4:4:4 JPEG), `fs.thumb` extracts an embedded MPF APP2
//!   preview (preferred) or EXIF IFD1 thumbnail with strict overflow-safe offset/length bounds,
//!   validates and decodes that embedded image through the exact same probe, limits, and scheduler,
//!   and applies the main image's EXIF orientation. Only when no usable embedded thumbnail exists
//!   does it return the per-path `too_large` error (`ErrorCode::Failed` with
//!   `details.reason = "too_large"`).
//! - **In-flight byte-budget scheduling**: total in-flight image decodes across all sessions are
//!   scheduled round-robin by principal against `min(512 MiB, total_machine_ram / 8)` (floored at
//!   the 256 MiB per-image cap so an admitted image never deadlocks) and at most **8** concurrent
//!   decodes, with a **10 s** bounded wait (returning retryable `busy`) and a **10 s** wall-clock
//!   decode timeout.
//! - **JPEG scaled IDCT note**: `image 0.25.10` decodes JPEGs via `zune-jpeg 0.5.15`, which does
//!   not expose reduced-resolution scaled IDCT (`1/2`, `1/4`, `1/8`; its internal `idct4x4` and
//!   `idct_1x1` are sparse-block optimizations that still output full 8×8 blocks). To minimize
//!   memory and decode time without switching decoders, `fs.thumb` passes the compressed input
//!   buffer by value (so `JpegDecoder` drops the duplicate copy before pixel allocation),
//!   downscales 2× or larger reductions with `DynamicImage::thumbnail` box-averaging (avoiding
//!   `FilterType::Triangle`'s 8–25 MiB filter weight buffers), and applies EXIF orientation
//!   *after* resizing (avoiding a second full-resolution frame allocation on rotated portrait
//!   photos).
//! - **Hardened `ffmpeg` video posters**:
//!   - `ffmpeg` is invoked directly by resolved absolute path with a cleared minimal environment
//!     (`MALLOC_ARENA_MAX=2`, fixed minimal `PATH`, throwaway temp dir), no shell, `-threads 1`
//!     on both input and output plus `-filter_threads 1` (limiting decoder/encoder/filter pools —
//!     `ffmpeg` still creates helper threads internally, so `MALLOC_ARENA_MAX=2` keeps glibc arena
//!     reservations small), `-max_pixels 67108864`, `-max_alloc 134217728`, `-protocol_whitelist
//!     file`, MOV/MP4 demuxer pinned with `-enable_drefs 0 -use_absolute_path 0`, non-MOV
//!     `-format_whitelist` excluding MOV and playlists/manifests, and `-map 0:v:0 -an -sn -dn`.
//!   - Measured `ffmpeg` peaks on Linux x86_64 with `MALLOC_ARENA_MAX=2`:
//!     - 1080p H.264: 73 MiB RSS / 501 MiB VmPeak (79 ms)
//!     - 1080p portrait phone clip (90° rotation metadata): 76 MiB RSS / 501 MiB VmPeak (84 ms, outputs 576×1024)
//!     - 1080p HEVC 10-bit: 111 MiB RSS / 522 MiB VmPeak (166 ms)
//!     - 4K H.264: 109–135 MiB RSS / 501–520 MiB VmPeak (192 ms)
//!     - 4K long-GOP (240-frame GOP): 119 MiB RSS / 501 MiB VmPeak (131 ms)
//!     - 4K HEVC 8-bit: 146 MiB RSS / 557 MiB VmPeak (84 ms)
//!     - 4K HEVC 10-bit: 242–265 MiB RSS / 652–676 MiB VmPeak (424–460 ms)
//!     - 8K H.264: 209 MiB RSS / 603 MiB VmPeak (140 ms)
//!     - 8K HEVC: 418–534 MiB RSS / 828–945 MiB VmPeak (311–404 ms)
//!     - 8K H.264 (16 ref frames): 395 MiB RSS / 851 MiB VmPeak (651 MiB RSS / 1051 MiB VmPeak during `-ss 1` scan)
//!   - Based on those measurements, the 10 ms RSS watchdog decides on resident memory (256 MiB for
//!     sniffed `<= 1080p` clips, 768 MiB capped to `max(256 MiB, effective_ram / 4)` for 4K/8K or
//!     unknown dimensions), while `RLIMIT_AS` stays a generous +1 GiB address-space backstop
//!     (1280–1792 MiB) so `pthread_create()` never fails on legitimate 4K/8K HEVC clips. Every
//!     `ffmpeg` call reserves its tier from the shared in-flight byte pool, and big (>1080p) clips
//!     run strictly one at a time.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::{self, FilterType};
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use legion_core::media::{Category, classify_path};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Registry, ToolError};
use legion_os::OsError;
use legion_os::file_id::{self, FileId};
use legion_os::mem_limit::effective_memory_bytes;
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::task::JoinSet;

use super::thumb_cache::{CachedThumb, ThumbCache, bad_cache_key, cache_key, temp_dir_in_cache};
use super::thumb_limit::{AcquireError, FairPool, compute_image_in_flight_budget};
use super::thumb_probe::{
    ImageProfile, MAX_IMAGE_DECODE_BYTES, image_limits, is_mpeg_ts_file, peek_image_file,
    probe_image_profile, too_large_error,
};
use super::thumb_worker::{
    WorkerOutcome, effective_decode_timeout, minimal_system_env, run_thumb_worker, worker_enabled,
};
use super::{
    cancelled_error, check_fs_path, check_regular_file, map_io_error, open_regular_file, platform,
};

const MIN_PATHS: usize = 1;
const MAX_PATHS: usize = 32;
const MIN_THUMB_SIZE: u32 = 96;
const DEFAULT_THUMB_SIZE: u32 = 256;
const MAX_THUMB_SIZE: u32 = 512;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IN_FLIGHT_IMAGE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_IN_FLIGHT_IMAGES: usize = 8;
const MAX_IN_FLIGHT_VIDEOS: usize = 2;
/// Fixed non-heap RSS allowance for a `__thumb-worker` process (binary, libc, stack). Included in
/// each worker's reservation so `sum(live worker resident limits) <= in_flight_budget`.
const WORKER_FIXED_OVERHEAD_BYTES: u64 = 12 * 1024 * 1024;
const MIN_IMAGE_HEAP_CAP_BYTES: u64 = 12 * 1024 * 1024;
const MAX_IMAGE_HEAP_CAP_BYTES: u64 = MAX_IMAGE_DECODE_BYTES - WORKER_FIXED_OVERHEAD_BYTES;
const IMAGE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const IMAGE_DECODE_TIMEOUT: Duration = Duration::from_secs(10);
const VIDEO_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const JPEG_QUALITY: u8 = 82;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);
const FFMPEG_KILL_GRACE: Duration = Duration::from_secs(1);
const FFMPEG_CAPTURE_LIMIT: usize = 32 * 1024;
/// Resident memory limit for sniffed <=1080p videos (256 MiB; up to 2 fit in a 512 MiB budget).
const VIDEO_SMALL_RSS_LIMIT: u64 = 256 * 1024 * 1024;
/// Resident memory limit for >1080p (4K/8K) or unknown-resolution videos (768 MiB; covers measured
/// 8K HEVC 534 MiB and 8K H.264 16-ref 651 MiB peaks, run one at a time).
const VIDEO_BIG_RSS_LIMIT: u64 = 768 * 1024 * 1024;
/// Maximum single video frame pixel count admitted by `ffmpeg` (64 MP; admits 8K 33.2 MP while
/// blocking 16k×16k bombs).
const FFMPEG_MAX_PIXELS: &str = "67108864";
/// Maximum single `av_malloc` allocation in `ffmpeg` (128 MiB).
const FFMPEG_MAX_ALLOC: &str = "134217728";
/// Generous virtual-address-space backstop added above `ffmpeg`'s resident cap (+1 GiB) so glibc
/// arenas and shared libraries never trigger `pthread_create()` failures on 4K/8K HEVC clips;
/// actual memory enforcement is performed by the 10 ms RSS watchdog.
const FFMPEG_RLIMIT_AS_HEADROOM: u64 = 1024 * 1024 * 1024;
/// Number of timeouts on the same file identity within `TIMEOUT_WINDOW` before applying a short
/// process-local back-off (never written to disk as a permanent bad-file record).
const TIMEOUT_BACKOFF_THRESHOLD: u32 = 2;
const TIMEOUT_WINDOW: Duration = Duration::from_secs(10 * 60);
const TIMEOUT_BACKOFF_DURATION: Duration = Duration::from_secs(5 * 60);
const MAX_TIMEOUT_BACKOFF_ENTRIES: usize = 1024;

/// Strictly allow only local file I/O in `ffmpeg` — blocks `http`, `https`, `concat`, `crypto`,
/// `data`, `pipe`, `subfile`, and all network/virtual protocols.
const FFMPEG_PROTOCOL_WHITELIST: &str = "file";
/// Whitelist when opening MOV/MP4 containers, always paired with `-enable_drefs 0` and
/// `-use_absolute_path 0` so external data references cannot open other files.
const FFMPEG_MOV_FORMAT_WHITELIST: &str = "mov,mp4,m4a,3gp,3g2,mj2";
/// Whitelist for non-MOV containers; excludes the MOV demuxer so a renamed MOV file with external
/// `dref` entries cannot bypass the pinned MOV options.
const FFMPEG_OTHER_FORMAT_WHITELIST: &str =
    "matroska,webm,avi,asf,flv,mpeg,mpegts,ogg,rm,mpegvideo";

/// Whether a video container uses the MOV/MP4 demuxer family (which accepts `-enable_drefs 0` and
/// `-use_absolute_path 0`) or another container format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoContainer {
    MovFamily,
    Other,
}

/// Resident-memory scheduling tier for a video poster extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VideoTier {
    /// Confirmed `<= 1920x1080`: 256 MiB RSS cap, up to 2 concurrent within a 512 MiB budget.
    Small,
    /// `> 1080p` (4K/8K) or unknown resolution: up to 768 MiB RSS cap, strictly 1 at a time.
    Big,
}

struct TimeoutEntry {
    count: u32,
    last_seen: Instant,
    backoff_until: Option<Instant>,
}

/// Bounded in-memory backoff tracker for repeated timeouts of the same file identity.
///
/// Timeouts are intentionally NOT written to the on-disk bad-file cache; this short in-memory
/// back-off keeps a spinning file from monopolizing worker slots on repeated folder reopens.
struct TimeoutBackoff {
    entries: Mutex<HashMap<String, TimeoutEntry>>,
}

impl TimeoutBackoff {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn active_remaining(&self, bad_key: &str) -> Option<Duration> {
        let now = Instant::now();
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let entry = guard.get(bad_key)?;
        if let Some(until) = entry.backoff_until {
            if until > now {
                return Some(until.saturating_duration_since(now));
            }
            guard.remove(bad_key);
        }
        None
    }

    fn record_timeout(&self, bad_key: &str) {
        let now = Instant::now();
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if guard.len() >= MAX_TIMEOUT_BACKOFF_ENTRIES {
            guard.retain(|_, v| {
                v.backoff_until.is_some_and(|u| u > now)
                    || now.saturating_duration_since(v.last_seen) < TIMEOUT_WINDOW
            });
            if guard.len() >= MAX_TIMEOUT_BACKOFF_ENTRIES {
                guard.clear();
            }
        }
        let entry = guard
            .entry(bad_key.to_owned())
            .or_insert_with(|| TimeoutEntry {
                count: 0,
                last_seen: now,
                backoff_until: None,
            });
        if now.saturating_duration_since(entry.last_seen) > TIMEOUT_WINDOW {
            entry.count = 0;
            entry.backoff_until = None;
        }
        entry.count = entry.count.saturating_add(1);
        entry.last_seen = now;
        if entry.count >= TIMEOUT_BACKOFF_THRESHOLD {
            entry.backoff_until = Some(now + TIMEOUT_BACKOFF_DURATION);
        }
    }
}

static IMAGE_POOL: OnceLock<Arc<FairPool>> = OnceLock::new();
static VIDEO_POOL: OnceLock<Arc<FairPool>> = OnceLock::new();
static TIMEOUT_BACKOFF: OnceLock<TimeoutBackoff> = OnceLock::new();

fn timeout_backoff() -> &'static TimeoutBackoff {
    TIMEOUT_BACKOFF.get_or_init(TimeoutBackoff::new)
}

fn machine_in_flight_image_budget() -> u64 {
    let effective_ram = effective_memory_bytes();
    compute_image_in_flight_budget(
        effective_ram,
        MAX_IMAGE_DECODE_BYTES,
        MAX_IN_FLIGHT_IMAGE_BYTES,
    )
}

fn video_big_rss_limit() -> u64 {
    let effective_ram = effective_memory_bytes();
    if effective_ram == 0 {
        return VIDEO_BIG_RSS_LIMIT;
    }
    let quarter = effective_ram / 4;
    quarter.clamp(VIDEO_SMALL_RSS_LIMIT, VIDEO_BIG_RSS_LIMIT)
}

fn video_tier_limits(tier: VideoTier) -> (u64, u64, u64) {
    // Returns `(rss_limit_bytes, in_flight_reserve_bytes, video_pool_slots)`.
    let budget = machine_in_flight_image_budget();
    match tier {
        VideoTier::Small => (VIDEO_SMALL_RSS_LIMIT, VIDEO_SMALL_RSS_LIMIT.min(budget), 1),
        VideoTier::Big => (
            video_big_rss_limit(),
            budget, // Reserve the full shared in-flight budget so big videos run alone.
            MAX_IN_FLIGHT_VIDEOS as u64,
        ),
    }
}

fn image_pool() -> Arc<FairPool> {
    Arc::clone(
        IMAGE_POOL
            .get_or_init(|| FairPool::new(machine_in_flight_image_budget(), MAX_IN_FLIGHT_IMAGES)),
    )
}

fn video_pool() -> Arc<FairPool> {
    Arc::clone(
        VIDEO_POOL.get_or_init(|| FairPool::new(MAX_IN_FLIGHT_VIDEOS as u64, MAX_IN_FLIGHT_VIDEOS)),
    )
}

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.thumb",
        Effect::Read,
        "Generate thumbnails for 1 to 32 image or video paths on this node (64 MiB source cap, 256 MiB estimated decode memory per image with EXIF/MPF preview fallback for larger JPEGs, out-of-process memory-bounded workers, in-flight budget capped at min(512 MiB, effective RAM/8)), returning per-path base64 JPEG (or PNG when transparent) thumbnails or per-path errors in input order, cached in a 256 MiB LRU store in the state directory.",
        thumb,
    );
}

/// How a thumbnail is fitted into the square target dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThumbFit {
    /// Preserve aspect ratio so the thumbnail fits inside `size x size` without cropping.
    #[default]
    Contain,
    /// Scale and center-crop so the thumbnail fills `size x size`.
    Cover,
}

/// Arguments of `fs.thumb`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ThumbArgs {
    /// One to 32 file paths to thumbnail (absolute or starting with `~`).
    paths: Vec<String>,
    /// Target thumbnail dimension in pixels, from 96 to 512 (default 256).
    #[serde(default = "default_thumb_size")]
    #[schemars(range(min = 96, max = 512))]
    size: u32,
    /// Fitting mode (`contain` or `cover`, default `contain`).
    #[serde(default)]
    fit: ThumbFit,
}

fn default_thumb_size() -> u32 {
    DEFAULT_THUMB_SIZE
}

/// Encoded thumbnail returned for a single successful path.
#[derive(Debug, Serialize)]
struct ThumbData {
    mime: String,
    content: String,
    width: u32,
    height: u32,
}

/// Per-path entry in the `fs.thumb` reply.
#[derive(Debug, Serialize)]
struct ThumbItem {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<ThumbData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody>,
}

impl ThumbItem {
    fn from_cached(path: String, thumb: CachedThumb) -> ThumbItem {
        ThumbItem {
            path,
            ok: Some(ThumbData {
                mime: thumb.mime.to_owned(),
                content: BASE64.encode(&thumb.bytes),
                width: thumb.width,
                height: thumb.height,
            }),
            error: None,
        }
    }

    fn from_error(path: String, error: ErrorBody) -> ThumbItem {
        ThumbItem {
            path,
            ok: None,
            error: Some(error),
        }
    }
}

/// Result of `fs.thumb`.
#[derive(Debug, Serialize)]
struct ThumbResult {
    results: Vec<ThumbItem>,
}

/// Metadata + OS identity snapshot used for cache keys and TOCTOU checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    mtime_nanos: Option<i128>,
    file_id: Option<FileId>,
}

impl FileStamp {
    fn from_meta_and_id(meta: &Metadata, file_id: Option<FileId>) -> FileStamp {
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| jiff::Timestamp::try_from(t).ok())
            .map(jiff::Timestamp::as_nanosecond);
        FileStamp {
            size: meta.len(),
            mtime_nanos,
            file_id,
        }
    }

    fn from_open_file(file: &File, meta: &Metadata) -> FileStamp {
        Self::from_meta_and_id(meta, file_id::from_file(file))
    }

    fn from_path(path: &Path, meta: &Metadata) -> FileStamp {
        Self::from_meta_and_id(meta, file_id::from_path(path))
    }
}

async fn thumb(ctx: &Ctx, args: ThumbArgs) -> Result<ThumbResult, ToolError> {
    let read_spec = ctx.node().spec("fs.read")?;
    if !ctx.grant().allows(&read_spec) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "{} may not use fs.thumb: its role {} is not allowed to call fs.read.",
                ctx.principal(),
                ctx.grant().role()
            ),
        )
        .with_hint("Thumbnails require the same permission as fs.read; ask the owner to allow fs.read in policy.toml."));
    }

    if !(MIN_PATHS..=MAX_PATHS).contains(&args.paths.len()) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.thumb accepts between {MIN_PATHS} and {MAX_PATHS} paths per call, and {} were given.",
                args.paths.len()
            ),
        )
        .with_hint("Pass a non-empty list of up to 32 file paths."));
    }
    if !(MIN_THUMB_SIZE..=MAX_THUMB_SIZE).contains(&args.size) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.thumb size must be between {MIN_THUMB_SIZE} and {MAX_THUMB_SIZE} pixels, and {} was given.",
                args.size
            ),
        )
        .with_hint("Choose a thumbnail size between 96 and 512 pixels."));
    }

    let cache_dir = ctx.paths().thumb_cache_dir();
    let cache = ThumbCache::for_dir(cache_dir.clone());
    let grant = ctx.grant().clone();
    let principal = ctx.principal().to_owned();
    let cancel = ctx.cancel_token().clone();
    let size = args.size;
    let fit = args.fit;

    let results = run_batch(args.paths, cancel, move |raw_path, cancel| {
        let grant = grant.clone();
        let principal = principal.clone();
        let cache = cache.clone();
        let cache_dir = cache_dir.clone();
        async move {
            thumb_one(
                &grant, &principal, &cache, &cache_dir, raw_path, size, fit, cancel,
            )
            .await
        }
    })
    .await?;

    Ok(ThumbResult { results })
}

/// Runs per-path workers concurrently and guarantees every input path receives exactly one
/// [`ThumbItem`] in input order, converting any non-cancellation worker error or task panic into a
/// per-path error rather than dropping the slot.
async fn run_batch<F, Fut>(
    paths: Vec<String>,
    cancel: CancellationToken,
    worker: F,
) -> Result<Vec<ThumbItem>, ToolError>
where
    F: Fn(String, CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<ThumbItem, ToolError>> + Send + 'static,
{
    let total = paths.len();
    let original_paths = paths.clone();
    let mut set = JoinSet::new();
    let mut task_owners: HashMap<tokio::task::Id, (usize, String)> = HashMap::with_capacity(total);

    for (idx, raw_path) in paths.into_iter().enumerate() {
        let fut = worker(raw_path.clone(), cancel.clone());
        let handle = set.spawn(async move {
            let outcome = fut.await;
            (idx, raw_path, outcome)
        });
        task_owners.insert(handle.id(), (idx, original_paths[idx].clone()));
    }

    let mut slots: Vec<Option<ThumbItem>> = (0..total).map(|_| None).collect();
    while let Some(joined) = set.join_next_with_id().await {
        match joined {
            Ok((task_id, (idx, _raw_path, Ok(item)))) => {
                task_owners.remove(&task_id);
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(item);
                }
            }
            Ok((task_id, (idx, raw_path, Err(err)))) => {
                task_owners.remove(&task_id);
                if err.code == ErrorCode::Cancelled {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    return Err(err);
                }
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(ThumbItem::from_error(raw_path, err));
                }
            }
            Err(join_err) => {
                let task_id = join_err.id();
                if let Some((idx, raw_path)) = task_owners.remove(&task_id)
                    && let Some(slot) = slots.get_mut(idx)
                {
                    *slot = Some(ThumbItem::from_error(
                        raw_path,
                        ErrorBody::new(
                            ErrorCode::Internal,
                            format!("Thumbnail worker stopped unexpectedly: {join_err}."),
                        ),
                    ));
                }
            }
        }
    }

    if cancel.is_cancelled() {
        return Err(cancelled_error("fs.thumb"));
    }

    let results = slots
        .into_iter()
        .enumerate()
        .map(|(idx, slot)| {
            slot.unwrap_or_else(|| {
                ThumbItem::from_error(
                    original_paths[idx].clone(),
                    ErrorBody::new(
                        ErrorCode::Internal,
                        "Thumbnail worker exited without returning a result.",
                    ),
                )
            })
        })
        .collect();
    Ok(results)
}

#[allow(clippy::too_many_arguments)]
async fn thumb_one(
    grant: &legion_core::Grant,
    principal: &str,
    cache: &ThumbCache,
    cache_dir: &Path,
    raw_path: String,
    size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<ThumbItem, ToolError> {
    if cancel.is_cancelled() {
        return Err(cancelled_error("fs.thumb"));
    }

    let grant_for_check = grant.clone();
    let raw_for_check = raw_path.clone();
    let resolved = match tokio::task::spawn_blocking(move || {
        check_fs_path(&grant_for_check, &raw_for_check, Access::Read)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb path check stopped unexpectedly: {err}."),
        )
    })? {
        Ok(path) => path,
        Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
    };

    let media = classify_path(&resolved);
    let mut category = media.category;
    if category == Category::Text
        && resolved
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ts"))
    {
        let resolved_sniff = resolved.clone();
        let is_video_ts = tokio::task::spawn_blocking(move || is_mpeg_ts_file(&resolved_sniff))
            .await
            .unwrap_or(false);
        if is_video_ts {
            category = Category::Video;
        }
    }

    match category {
        Category::Image => {
            thumb_image(principal, cache, raw_path, resolved, size, fit, cancel).await
        }
        Category::Video => {
            thumb_video(
                principal, cache, cache_dir, raw_path, resolved, size, fit, cancel,
            )
            .await
        }
        _ => {
            let resolved_stat = resolved.clone();
            let stat_res = tokio::task::spawn_blocking(move || {
                check_regular_file(
                    &resolved_stat,
                    "thumbnail",
                    "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
                )
            })
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb stat stopped unexpectedly: {err}."),
                )
            })?;
            if let Err(err) = stat_res {
                return Ok(ThumbItem::from_error(raw_path, err));
            }
            Ok(ThumbItem::from_error(
                raw_path,
                ErrorBody::new(
                    ErrorCode::Unsupported,
                    format!(
                        "{} ({}) cannot be thumbnailed.",
                        resolved.display(),
                        category
                    ),
                )
                .with_hint("fs.thumb supports raster images (JPEG, PNG, GIF, WebP, BMP) and videos (when ffmpeg is on PATH)."),
            ))
        }
    }
}

/// Prepared image file opened non-blockingly once, verified against TOCTOU swaps, checked against
/// positive and expiring negative cache entries, and header-peeked (`<= 128 KiB`) BEFORE
/// requesting byte-budget capacity or reading the full source file.
enum ImagePrep {
    CacheHit(CachedThumb),
    CachedBad(ErrorBody),
    Ready {
        key: String,
        bad_key: String,
        open_stamp: FileStamp,
        file: File,
        reserve_bytes: u64,
        heap_cap_bytes: u64,
        rss_limit_bytes: u64,
    },
}

async fn thumb_image(
    principal: &str,
    cache: &ThumbCache,
    raw_path: String,
    resolved: std::path::PathBuf,
    size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<ThumbItem, ToolError> {
    let cache_prep = cache.clone();
    let resolved_prep = resolved.clone();
    let prep = tokio::task::spawn_blocking(move || {
        prepare_image_open_once(&resolved_prep, &cache_prep, size, fit, || {})
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb image open stopped unexpectedly: {err}."),
        )
    })?;

    let (key, bad_key, open_stamp, file, reserve_bytes, heap_cap_bytes, rss_limit_bytes) =
        match prep {
            Ok(ImagePrep::CacheHit(hit)) => return Ok(ThumbItem::from_cached(raw_path, hit)),
            Ok(ImagePrep::CachedBad(err)) => return Ok(ThumbItem::from_error(raw_path, err)),
            Ok(ImagePrep::Ready {
                key,
                bad_key,
                open_stamp,
                file,
                reserve_bytes,
                heap_cap_bytes,
                rss_limit_bytes,
            }) => (
                key,
                bad_key,
                open_stamp,
                file,
                reserve_bytes,
                heap_cap_bytes,
                rss_limit_bytes,
            ),
            Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
        };

    if let Some(remaining) = timeout_backoff().active_remaining(&bad_key) {
        return Ok(ThumbItem::from_error(
            raw_path,
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Decoding {} timed out repeatedly and is in a temporary back-off ({} s remaining).",
                    resolved.display(),
                    remaining.as_secs().max(1)
                ),
            )
            .with_hint(
                "Timeouts are not cached permanently; retry after the short back-off expires.",
            ),
        ));
    }

    // Reserve the worker's exact resident budget from the fair image pool BEFORE reading the full
    // file or launching a decode worker so the sum of live workers' limits never exceeds budget.
    let permit = match image_pool()
        .acquire(principal, reserve_bytes, IMAGE_WAIT_TIMEOUT, &cancel)
        .await
    {
        Ok(permit) => permit,
        Err(AcquireError::Cancelled) => return Err(cancelled_error("fs.thumb")),
        Err(AcquireError::TimedOut) => {
            return Ok(ThumbItem::from_error(
                raw_path,
                ErrorBody::new(
                    ErrorCode::Busy,
                    format!(
                        "Timed out waiting for thumbnail decode capacity for {}.",
                        resolved.display()
                    ),
                )
                .with_hint("Thumbnail decoding is busy on this node; retry fs.thumb shortly."),
            ));
        }
    };

    let decode_timeout = effective_decode_timeout(IMAGE_DECODE_TIMEOUT);

    if worker_enabled() {
        let handle_check = match file.try_clone() {
            Ok(cloned) => cloned,
            Err(err) => {
                drop(permit);
                return Ok(ThumbItem::from_error(
                    raw_path,
                    map_io_error("read", &resolved, err),
                ));
            }
        };
        let worker_res = run_thumb_worker(
            &resolved,
            file,
            size,
            fit,
            open_stamp.size,
            MAX_SOURCE_BYTES,
            heap_cap_bytes,
            rss_limit_bytes,
            decode_timeout,
            cancel.clone(),
        )
        .await;
        drop(permit);

        let worker_outcome = match worker_res {
            Ok(outcome) => outcome,
            Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
            Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
        };

        return match worker_outcome {
            WorkerOutcome::Ok(thumb) => {
                let resolved_check = resolved.clone();
                let stamp_ok = tokio::task::spawn_blocking(move || {
                    verify_post_read_stamp(&resolved_check, &handle_check, open_stamp)
                })
                .await
                .map_err(|err| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("fs.thumb post-read check stopped unexpectedly: {err}."),
                    )
                })?;
                if let Err(err) = stamp_ok {
                    return Ok(ThumbItem::from_error(raw_path, err));
                }
                let cache_put = cache.clone();
                let thumb_put = thumb.clone();
                let _ = tokio::task::spawn_blocking(move || cache_put.put(&key, &thumb_put)).await;
                Ok(ThumbItem::from_cached(raw_path, thumb))
            }
            WorkerOutcome::Rejected(err) | WorkerOutcome::TransientFailure(err) => {
                // External kills, crashes, and normal format rejections are NOT cached as bad.
                Ok(ThumbItem::from_error(raw_path, err))
            }
            WorkerOutcome::TimedOut { error, cpu_busy } => {
                // Timeouts are NOT stored permanently on disk; repeated CPU-busy timeouts get a
                // short in-memory back-off to bound repeated damage.
                if cpu_busy {
                    timeout_backoff().record_timeout(&bad_key);
                }
                Ok(ThumbItem::from_error(raw_path, error))
            }
            WorkerOutcome::MemoryLimitExceeded { error, .. } => {
                let resolved_check = resolved.clone();
                let cache_bad = cache.clone();
                let err_bad = error.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if verify_post_read_stamp(&resolved_check, &handle_check, open_stamp).is_ok() {
                        cache_bad.put_bad(&bad_key, &err_bad);
                    }
                })
                .await;
                Ok(ThumbItem::from_error(raw_path, error))
            }
        };
    }

    // In-process fallback (used only in standard unit/integration test binaries that do not
    // dispatch `__thumb-worker`). Full file bytes are still read only AFTER acquiring `permit`.
    let abort_flag = Arc::new(AtomicBool::new(false));
    let abort_worker = Arc::clone(&abort_flag);
    let resolved_decode = resolved.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let bytes = read_open_file_verified(&resolved_decode, &file, open_stamp)?;
        let (decode_bytes, profile, override_orientation) =
            select_decode_source(&resolved_decode, bytes)?;
        decode_and_render_image_checked(
            &resolved_decode,
            decode_bytes,
            profile,
            override_orientation,
            size,
            fit,
            &abort_worker,
        )
    });

    let sleep = tokio::time::sleep(decode_timeout);
    tokio::pin!(sleep);

    let outcome = tokio::select! {
        joined = &mut handle => {
            joined.map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb image decode stopped unexpectedly: {err}."),
                )
            })?
        }
        () = cancel.cancelled() => {
            abort_flag.store(true, Ordering::Release);
            return Err(cancelled_error("fs.thumb"));
        }
        () = &mut sleep => {
            abort_flag.store(true, Ordering::Release);
            Err(ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Decoding a thumbnail for {} exceeded the {}-second time limit.",
                    resolved.display(),
                    decode_timeout.as_secs().max(1)
                ),
            )
            .with_hint("The image may be malformed or too expensive to decode quickly."))
        }
    };

    match outcome {
        Ok(thumb) => {
            let cache_put = cache.clone();
            let thumb_put = thumb.clone();
            let _ = tokio::task::spawn_blocking(move || cache_put.put(&key, &thumb_put)).await;
            Ok(ThumbItem::from_cached(raw_path, thumb))
        }
        Err(err) if err.code == ErrorCode::Cancelled => Err(err),
        Err(err) => Ok(ThumbItem::from_error(raw_path, err)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn thumb_video(
    principal: &str,
    cache: &ThumbCache,
    cache_dir: &Path,
    raw_path: String,
    resolved: std::path::PathBuf,
    size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<ThumbItem, ToolError> {
    let resolved_stat = resolved.clone();
    let cache_get = cache.clone();
    let pre = tokio::task::spawn_blocking(move || {
        let (file, meta) = open_regular_file(
            &resolved_stat,
            "thumbnail",
            "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
        )?;
        let stamp = FileStamp::from_open_file(&file, &meta);
        let key = cache_key(
            &resolved_stat,
            stamp.size,
            stamp.mtime_nanos,
            stamp.file_id,
            size,
            fit,
        );
        let bad_key = bad_cache_key(&resolved_stat, stamp.size, stamp.mtime_nanos, stamp.file_id);
        let hit = cache_get.get(&key);
        let bad = if hit.is_none() {
            cache_get.get_bad(&bad_key)
        } else {
            None
        };
        let (container, tier) = if hit.is_none() && bad.is_none() {
            sniff_video_info(&resolved_stat, file)
        } else {
            (VideoContainer::Other, VideoTier::Small)
        };
        Ok::<
            (
                FileStamp,
                String,
                String,
                VideoContainer,
                VideoTier,
                Option<CachedThumb>,
                Option<ErrorBody>,
            ),
            ToolError,
        >((stamp, key, bad_key, container, tier, hit, bad))
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb video stat stopped unexpectedly: {err}."),
        )
    })?;

    let (stamp, key, bad_key, container, initial_tier, hit, bad) = match pre {
        Ok(v) => v,
        Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
    };
    if let Some(hit) = hit {
        return Ok(ThumbItem::from_cached(raw_path, hit));
    }
    if let Some(bad) = bad {
        return Ok(ThumbItem::from_error(raw_path, bad));
    }
    if let Some(remaining) = timeout_backoff().active_remaining(&bad_key) {
        return Ok(ThumbItem::from_error(
            raw_path,
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Extracting a poster for {} timed out repeatedly and is in a temporary back-off ({} s remaining).",
                    resolved.display(),
                    remaining.as_secs().max(1)
                ),
            )
            .with_hint(
                "Timeouts are not cached permanently; retry after the short back-off expires.",
            ),
        ));
    }

    let mut tier = initial_tier;
    let outcome = loop {
        let (rss_limit, budget_reserve, video_slots) = video_tier_limits(tier);
        let video_permit = match video_pool()
            .acquire(principal, video_slots, VIDEO_WAIT_TIMEOUT, &cancel)
            .await
        {
            Ok(permit) => permit,
            Err(AcquireError::Cancelled) => return Err(cancelled_error("fs.thumb")),
            Err(AcquireError::TimedOut) => {
                return Ok(ThumbItem::from_error(
                    raw_path,
                    ErrorBody::new(
                        ErrorCode::Busy,
                        format!(
                            "Timed out waiting for a video thumbnail slot for {}.",
                            resolved.display()
                        ),
                    )
                    .with_hint(
                        "Video thumbnail extraction is busy on this node; retry fs.thumb shortly.",
                    ),
                ));
            }
        };
        let mem_permit = match image_pool()
            .acquire(principal, budget_reserve, VIDEO_WAIT_TIMEOUT, &cancel)
            .await
        {
            Ok(permit) => permit,
            Err(AcquireError::Cancelled) => {
                drop(video_permit);
                return Err(cancelled_error("fs.thumb"));
            }
            Err(AcquireError::TimedOut) => {
                drop(video_permit);
                return Ok(ThumbItem::from_error(
                    raw_path,
                    ErrorBody::new(
                        ErrorCode::Busy,
                        format!(
                            "Timed out waiting for thumbnail memory budget for {}.",
                            resolved.display()
                        ),
                    )
                    .with_hint("Thumbnail decoding is busy on this node; retry fs.thumb shortly."),
                ));
            }
        };

        let attempt = extract_video_poster(
            &resolved,
            stamp,
            container,
            rss_limit,
            cache_dir,
            size,
            fit,
            cancel.clone(),
        )
        .await;
        drop(mem_permit);
        drop(video_permit);

        // If a sniffed <=1080p video unexpectedly tripped the 256 MiB small-tier watchdog, retry
        // once in the exclusive big tier (768 MiB, 1 at a time) before treating it as too large.
        if tier == VideoTier::Small
            && let Err(ref err) = attempt
            && is_our_memory_kill(err)
        {
            tier = VideoTier::Big;
            continue;
        }
        break attempt;
    };

    match outcome {
        Ok(thumb) => {
            let cache_put = cache.clone();
            let thumb_put = thumb.clone();
            let _ = tokio::task::spawn_blocking(move || cache_put.put(&key, &thumb_put)).await;
            Ok(ThumbItem::from_cached(raw_path, thumb))
        }
        Err(err) if err.code == ErrorCode::Cancelled => Err(err),
        Err(err) => {
            if err.code == ErrorCode::Timeout {
                let cpu_busy = err
                    .details
                    .as_ref()
                    .and_then(|d| d.get("cpu_busy"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if cpu_busy {
                    timeout_backoff().record_timeout(&bad_key);
                }
            } else if is_our_memory_kill(&err) {
                let cache_bad = cache.clone();
                let err_bad = err.clone();
                let _ = tokio::task::spawn_blocking(move || cache_bad.put_bad(&bad_key, &err_bad))
                    .await;
            }
            Ok(ThumbItem::from_error(raw_path, err))
        }
    }
}

fn is_our_memory_kill(err: &ErrorBody) -> bool {
    err.details
        .as_ref()
        .and_then(|d| d.get("killed_for_memory"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Selects either the full image (when its estimated decode peak fits the 256 MiB cap) or the best
/// validated embedded MPF/EXIF JPEG preview (with the main image's EXIF orientation).
pub(crate) fn select_decode_source(
    path: &Path,
    bytes: Vec<u8>,
) -> Result<(Vec<u8>, ImageProfile, Option<Orientation>), ErrorBody> {
    let (profile, jpeg_meta) = probe_image_profile(path, &bytes)?;
    if profile.fits_cap() {
        return Ok((bytes, profile, None));
    }

    // Over the 256 MiB decode cap: attempt embedded MPF preview / EXIF IFD1 thumbnail fallback.
    // Every embedded slice goes through the exact same header probe and 256 MiB cap so an embedded
    // bomb cannot bypass limits.
    if let Some(meta) = jpeg_meta {
        for (start, end) in meta.embedded_thumbs {
            let Some(slice) = bytes.get(start..end) else {
                continue;
            };
            let candidate = slice.to_vec();
            if let Ok((sub_profile, _)) = probe_image_profile(path, &candidate)
                && sub_profile.format == ImageFormat::Jpeg
                && sub_profile.fits_cap()
            {
                return Ok((candidate, sub_profile, Some(meta.orientation)));
            }
        }
    }

    Err(too_large_error(path, &profile))
}

/// Opens an image file once, verifies its metadata and identity on the open handle, checks the
/// lock-free positive and negative caches, and peeks ONLY the header prefix (`<= 128 KiB`) before
/// rewinding to byte 0 so full source bytes are never read into the parent process prior to
/// reserving pool capacity.
fn prepare_image_open_once<F>(
    path: &Path,
    cache: &ThumbCache,
    target_size: u32,
    fit: ThumbFit,
    between_stat_and_open: F,
) -> Result<ImagePrep, ErrorBody>
where
    F: FnOnce(),
{
    let pre_meta = check_regular_file(
        path,
        "thumbnail",
        "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
    )?;
    let pre_stamp = FileStamp::from_path(path, &pre_meta);

    between_stat_and_open();

    // Non-blocking open (`O_NONBLOCK | O_NOCTTY` on Unix, disk-handle check on Windows) ensures a
    // FIFO swapped into `path` after `stat` can never block a node thread.
    let (mut file, open_meta) = open_regular_file(
        path,
        "read",
        "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
    )?;
    let open_stamp = FileStamp::from_open_file(&file, &open_meta);
    if open_stamp != pre_stamp {
        return Err(changed_while_reading(path));
    }

    if open_stamp.size == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is empty and cannot be decoded as an image.",
                path.display()
            ),
        ));
    }
    if open_stamp.size > MAX_SOURCE_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is {} bytes, exceeding the 64 MiB thumbnail source limit.",
                path.display(),
                open_stamp.size
            ),
        )
        .with_hint(
            "Files larger than 64 MiB are not thumbnailed; open or download the file directly.",
        ));
    }

    let key = cache_key(
        path,
        open_stamp.size,
        open_stamp.mtime_nanos,
        open_stamp.file_id,
        target_size,
        fit,
    );
    if let Some(hit) = cache.get(&key) {
        return Ok(ImagePrep::CacheHit(hit));
    }
    let bad_key = bad_cache_key(
        path,
        open_stamp.size,
        open_stamp.mtime_nanos,
        open_stamp.file_id,
    );
    if let Some(bad) = cache.get_bad(&bad_key) {
        return Ok(ImagePrep::CachedBad(bad));
    }

    let peek = peek_image_file(path, &mut file, open_stamp.size)?;
    if !peek.profile.fits_cap() && !peek.may_have_embedded_preview {
        return Err(too_large_error(path, &peek.profile));
    }

    // Reserve EXACTLY what the worker is allowed to use (`rss_limit_bytes == reserve_bytes`,
    // `heap_cap_bytes == reserve_bytes - WORKER_FIXED_OVERHEAD_BYTES`) so the sum of all live
    // workers' limits never exceeds the in-flight budget.
    let heap_cap_bytes = if peek.uncertain_frame_bounds {
        MAX_IMAGE_HEAP_CAP_BYTES
    } else if peek.profile.fits_cap() {
        peek.profile
            .estimated_peak_bytes
            .clamp(MIN_IMAGE_HEAP_CAP_BYTES, MAX_IMAGE_HEAP_CAP_BYTES)
    } else {
        // Over-cap JPEG with EXIF/MPF marker: reserve 2x source + preview headroom and cap the
        // worker tightly so it can only decode an embedded preview, never the full over-cap frame.
        open_stamp
            .size
            .saturating_mul(2)
            .saturating_add(32 * 1024 * 1024)
            .clamp(MIN_IMAGE_HEAP_CAP_BYTES, MAX_IMAGE_HEAP_CAP_BYTES)
    };
    let reserve_bytes = heap_cap_bytes
        .saturating_add(WORKER_FIXED_OVERHEAD_BYTES)
        .min(MAX_IMAGE_DECODE_BYTES);
    let rss_limit_bytes = reserve_bytes;

    Ok(ImagePrep::Ready {
        key,
        bad_key,
        open_stamp,
        file,
        reserve_bytes,
        heap_cap_bytes,
        rss_limit_bytes,
    })
}

fn verify_post_read_stamp(
    path: &Path,
    file: &File,
    open_stamp: FileStamp,
) -> Result<(), ErrorBody> {
    let after_handle_meta = file
        .metadata()
        .map_err(|err| map_io_error("read", path, err))?;
    if FileStamp::from_open_file(file, &after_handle_meta) != open_stamp {
        return Err(changed_while_reading(path));
    }
    let after_path_meta = fs::metadata(path).map_err(|err| map_io_error("stat", path, err))?;
    if FileStamp::from_path(path, &after_path_meta) != open_stamp {
        return Err(changed_while_reading(path));
    }
    Ok(())
}

fn read_open_file_verified(
    path: &Path,
    file: &File,
    open_stamp: FileStamp,
) -> Result<Vec<u8>, ErrorBody> {
    let capacity = usize::try_from(open_stamp.size.min(MAX_SOURCE_BYTES)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| map_io_error("read", path, err))?;

    if bytes.len() as u64 > MAX_SOURCE_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} grew past the 64 MiB thumbnail source limit while reading.",
                path.display()
            ),
        )
        .with_hint("Retry fs.thumb once the file has finished changing."));
    }
    if bytes.len() as u64 != open_stamp.size {
        return Err(changed_while_reading(path));
    }
    verify_post_read_stamp(path, file, open_stamp)?;
    Ok(bytes)
}

fn changed_while_reading(path: &Path) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Conflict,
        format!(
            "{} changed while reading it for a thumbnail.",
            path.display()
        ),
    )
    .with_hint("Retry fs.thumb once the file has finished changing.")
}

/// Decodes a header-verified raster image inside hard limits, downscales before applying EXIF
/// orientation (avoiding a second full-resolution allocation on rotated portrait photos), and
/// encodes JPEG (or PNG when transparent).
pub(crate) fn decode_and_render_image_checked(
    path: &Path,
    bytes: Vec<u8>,
    profile: ImageProfile,
    override_orientation: Option<Orientation>,
    target_size: u32,
    fit: ThumbFit,
    abort: &AtomicBool,
) -> Result<CachedThumb, ErrorBody> {
    if abort.load(Ordering::Acquire) {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!("Decoding {} was aborted.", path.display()),
        ));
    }
    if !profile.fits_cap() {
        return Err(too_large_error(path, &profile));
    }

    let mut reader = ImageReader::with_format(Cursor::new(bytes), profile.format);
    reader.limits(image_limits());

    let mut decoder = reader.into_decoder().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot decode {}: {err}.", path.display()),
        )
        .with_hint("The image may be corrupted or exceed the thumbnail size and memory limits.")
    })?;

    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 || w > profile.width || h > profile.height {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} reported inconsistent image dimensions ({w}x{h} vs {}x{}).",
                path.display(),
                profile.width,
                profile.height
            ),
        ));
    }

    let orientation = override_orientation
        .unwrap_or_else(|| decoder.orientation().unwrap_or(Orientation::NoTransforms));
    let img = DynamicImage::from_decoder(decoder).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot decode {}: {err}.", path.display()),
        )
        .with_hint("The image may be truncated, corrupted, or exceed memory limits.")
    })?;

    if abort.load(Ordering::Acquire) {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!("Decoding {} was aborted.", path.display()),
        ));
    }

    // Resize BEFORE applying EXIF orientation: `target_size x target_size` is square, so both
    // `Contain` and `Cover` are symmetric under 90°/180°/270° rotations and flips, and rotating
    // the tiny <=512px thumbnail avoids allocating a second full-resolution frame (e.g. +143 MiB
    // on a 50 MP portrait photo).
    let mut resized = resize_for_thumb(&img, target_size, fit);
    drop(img);
    resized.apply_orientation(orientation);

    if abort.load(Ordering::Acquire) {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!("Decoding {} was aborted.", path.display()),
        ));
    }

    encode_thumbnail(&resized)
}

/// Resizes `img` for a square `target_size x target_size` thumbnail.
///
/// For >=2x downscales, uses `DynamicImage::thumbnail` / `imageops::thumbnail` integer box
/// averaging, which allocates only the tiny output buffer (eliminating `FilterType::Triangle`'s
/// 8–25 MiB filter weight scratch on large photos) and runs >2x faster. Smaller downscales and
/// `Cover` upscales use `FilterType::Triangle`.
fn resize_for_thumb(img: &DynamicImage, target_size: u32, fit: ThumbFit) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    match fit {
        ThumbFit::Contain => {
            if w >= target_size.saturating_mul(2) && h >= target_size.saturating_mul(2) {
                img.thumbnail(target_size, target_size)
            } else if w > target_size || h > target_size {
                img.resize(target_size, target_size, FilterType::Triangle)
            } else {
                img.clone()
            }
        }
        ThumbFit::Cover => {
            let side = w.min(h);
            if side >= target_size.saturating_mul(2) {
                let x = (w - side) / 2;
                let y = (h - side) / 2;
                thumbnail_crop_square(img, x, y, side, target_size)
            } else {
                img.resize_to_fill(target_size, target_size, FilterType::Triangle)
            }
        }
    }
}

fn thumbnail_crop_square(
    img: &DynamicImage,
    x: u32,
    y: u32,
    side: u32,
    target_size: u32,
) -> DynamicImage {
    match img {
        DynamicImage::ImageLuma8(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageLuma8(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageLumaA8(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageLumaA8(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageRgb8(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageRgb8(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageRgba8(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageRgba8(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageLuma16(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageLuma16(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageLumaA16(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageLumaA16(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageRgb16(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageRgb16(imageops::thumbnail(&*view, target_size, target_size))
        }
        DynamicImage::ImageRgba16(buf) => {
            let view = imageops::crop_imm(buf, x, y, side, side);
            DynamicImage::ImageRgba16(imageops::thumbnail(&*view, target_size, target_size))
        }
        other => other.resize_to_fill(target_size, target_size, FilterType::Triangle),
    }
}

fn encode_thumbnail(img: &DynamicImage) -> Result<CachedThumb, ErrorBody> {
    let width = img.width();
    let height = img.height();

    if img.color().has_alpha() {
        let rgba = img.to_rgba8();
        if rgba.pixels().any(|pixel| pixel.0[3] < 255) {
            let mut buf = Vec::new();
            PngEncoder::new(&mut buf)
                .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
                .map_err(|err| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("Encoding PNG thumbnail failed: {err}."),
                    )
                })?;
            return Ok(CachedThumb {
                mime: "image/png",
                width,
                height,
                bytes: buf,
            });
        }
    }

    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY)
        .encode(&rgb, width, height, image::ExtendedColorType::Rgb8)
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Encoding JPEG thumbnail failed: {err}."),
            )
        })?;
    Ok(CachedThumb {
        mime: "image/jpeg",
        width,
        height,
        bytes: buf,
    })
}

/// Sniffs both container family (`MovFamily` vs `Other`) and resident-memory tier (`Small` for
/// confirmed `<= 1920x1080`, `Big` for `> 1080p` or unknown dimensions) directly from an already-
/// opened regular file handle (never blocking on FIFOs).
fn sniff_video_info(path: &Path, file: File) -> (VideoContainer, VideoTier) {
    let mut header = Vec::with_capacity(64 * 1024);
    let _ = file.take(64 * 1024).read_to_end(&mut header);

    let is_mov_ext = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "mov" | "mp4" | "m4v" | "m4a" | "3gp" | "3g2" | "mj2"
        )
    });
    let is_mov_magic = header.len() >= 8
        && matches!(
            &header[4..8],
            b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"pnot"
        );
    let container = if is_mov_ext || is_mov_magic {
        VideoContainer::MovFamily
    } else {
        VideoContainer::Other
    };

    let dims = match container {
        VideoContainer::MovFamily => parse_mov_tkhd_dimensions(&header),
        VideoContainer::Other => None,
    };
    let tier = match dims {
        Some((w, h))
            if w > 0 && h > 0 && u64::from(w).saturating_mul(u64::from(h)) <= 1920 * 1080 =>
        {
            VideoTier::Small
        }
        _ => VideoTier::Big,
    };
    (container, tier)
}

/// Scans an MP4/MOV header prefix for `tkhd` track header boxes and returns the largest coded
/// `(width, height)` found (fixed-point 16.16 pixels). Returns `None` when `moov` is at EOF or no
/// video `tkhd` is present in the prefix, causing the caller to use the conservative `Big` tier.
fn parse_mov_tkhd_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut max_w = 0u32;
    let mut max_h = 0u32;
    let mut i = 0usize;
    while i + 12 <= bytes.len() {
        if &bytes[i + 4..i + 8] == b"tkhd" {
            let version = bytes[i + 8];
            let dims_off = match version {
                0 => i + 8 + 4 + 72, // version+flags(4) + 72 bytes to width(4), height(4)
                1 => i + 8 + 4 + 84, // 64-bit times add 12 bytes
                _ => {
                    i += 4;
                    continue;
                }
            };
            if let Some(sl) = bytes.get(dims_off..dims_off + 8) {
                let w = u32::from_be_bytes([sl[0], sl[1], sl[2], sl[3]]) >> 16;
                let h = u32::from_be_bytes([sl[4], sl[5], sl[6], sl[7]]) >> 16;
                if w > 0 && h > 0 && w <= 65_535 && h <= 65_535 {
                    max_w = max_w.max(w);
                    max_h = max_h.max(h);
                }
            }
            i += 8;
        } else {
            i += 1;
        }
    }
    (max_w > 0 && max_h > 0).then_some((max_w, max_h))
}

/// Resolves `ffmpeg`'s absolute path from the current `PATH` before clearing the child environment.
fn resolve_ffmpeg_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let names = ["ffmpeg.exe", "ffmpeg.cmd", "ffmpeg.bat", "ffmpeg"];
    #[cfg(not(windows))]
    let names = ["ffmpeg"];

    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Builds the minimal cleared environment for `ffmpeg`: `MALLOC_ARENA_MAX=2` (preventing glibc
/// per-thread 64 MiB arena explosion on 4K/8K HEVC), a fixed minimal `PATH`, the throwaway temp
/// directory, and required Windows system-root variables.
fn minimal_ffmpeg_env(temp_dir: &Path) -> Vec<(OsString, OsString)> {
    let mut vars = minimal_system_env();
    let temp = temp_dir.as_os_str().to_os_string();
    vars.push((OsString::from("MALLOC_ARENA_MAX"), OsString::from("2")));
    vars.push((OsString::from("TMPDIR"), temp.clone()));
    vars.push((OsString::from("TEMP"), temp.clone()));
    vars.push((OsString::from("TMP"), temp));
    #[cfg(windows)]
    {
        let sys_root =
            std::env::var_os("SYSTEMROOT").unwrap_or_else(|| OsString::from(r"C:\Windows"));
        let mut minimal_path = OsString::new();
        minimal_path.push(&sys_root);
        minimal_path.push(r"\System32;");
        minimal_path.push(&sys_root);
        vars.push((OsString::from("PATH"), minimal_path));
    }
    #[cfg(not(windows))]
    {
        vars.push((OsString::from("PATH"), OsString::from("/usr/bin:/bin")));
    }
    vars
}

/// Builds the locked-down `ffmpeg` argument list for single-frame video poster extraction.
fn build_ffmpeg_args(
    input_arg: &str,
    frame_arg: &str,
    seek: Option<&str>,
    container: VideoContainer,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
        "-threads".to_owned(),
        "1".to_owned(),
        "-max_pixels".to_owned(),
        FFMPEG_MAX_PIXELS.to_owned(),
        "-max_alloc".to_owned(),
        FFMPEG_MAX_ALLOC.to_owned(),
        "-protocol_whitelist".to_owned(),
        FFMPEG_PROTOCOL_WHITELIST.to_owned(),
    ];
    match container {
        VideoContainer::MovFamily => {
            args.extend([
                "-format_whitelist".to_owned(),
                FFMPEG_MOV_FORMAT_WHITELIST.to_owned(),
                "-enable_drefs".to_owned(),
                "0".to_owned(),
                "-use_absolute_path".to_owned(),
                "0".to_owned(),
            ]);
        }
        VideoContainer::Other => {
            args.extend([
                "-format_whitelist".to_owned(),
                FFMPEG_OTHER_FORMAT_WHITELIST.to_owned(),
            ]);
        }
    }
    if let Some(ss) = seek {
        args.push("-ss".to_owned());
        args.push(ss.to_owned());
    }
    args.extend([
        "-i".to_owned(),
        input_arg.to_owned(),
        "-map".to_owned(),
        "0:v:0".to_owned(),
        "-frames:v".to_owned(),
        "1".to_owned(),
        "-threads".to_owned(),
        "1".to_owned(),
        "-filter_threads".to_owned(),
        "1".to_owned(),
        "-an".to_owned(),
        "-sn".to_owned(),
        "-dn".to_owned(),
        "-vf".to_owned(),
        "scale=min(1024\\,iw):min(1024\\,ih):force_original_aspect_ratio=decrease".to_owned(),
        "-f".to_owned(),
        "image2".to_owned(),
        "-vcodec".to_owned(),
        "png".to_owned(),
        frame_arg.to_owned(),
    ]);
    args
}

#[allow(clippy::too_many_arguments)]
async fn extract_video_poster(
    resolved: &Path,
    expected_stamp: FileStamp,
    container: VideoContainer,
    rss_limit: u64,
    cache_dir: &Path,
    target_size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<CachedThumb, ErrorBody> {
    let ffmpeg_bin = resolve_ffmpeg_path().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Unsupported,
            "Video thumbnails are not available on this node because ffmpeg is not on PATH.",
        )
        .with_hint(
            "Install ffmpeg on this node and ensure it is on PATH to generate video thumbnails.",
        )
    })?;

    let deadline = Instant::now() + FFMPEG_TIMEOUT;
    let temp_dir = temp_dir_in_cache(cache_dir)
        .or_else(|_| tempfile::tempdir())
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!("Cannot create temporary directory for video thumbnail: {err}."),
            )
        })?;
    let frame_path = temp_dir.path().join("frame.png");
    let frame_arg = frame_path.to_string_lossy().into_owned();
    let input_arg = platform::ffmpeg_input_arg(resolved);

    let seek_outcome = run_ffmpeg_extract(
        &ffmpeg_bin,
        temp_dir.path(),
        resolved,
        &input_arg,
        &frame_arg,
        Some("1"),
        container,
        rss_limit,
        deadline,
        cancel.clone(),
    )
    .await?;

    let frame_ready = fs::metadata(&frame_path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false);

    if !frame_ready {
        if !seek_outcome.success() {
            let detail = seek_outcome.stderr.text.trim();
            if !detail.is_empty() && !detail.to_ascii_lowercase().contains("output file is empty") {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "ffmpeg could not extract a frame from {}: {}",
                        resolved.display(),
                        first_line(detail)
                    ),
                )
                .with_hint("The file may be corrupted, exceed pixel/memory limits, reference external data (blocked for safety), or use a codec this node's ffmpeg cannot decode."));
            }
        }
        let fallback_outcome = run_ffmpeg_extract(
            &ffmpeg_bin,
            temp_dir.path(),
            resolved,
            &input_arg,
            &frame_arg,
            None,
            container,
            rss_limit,
            deadline,
            cancel.clone(),
        )
        .await?;
        let fallback_ready = fs::metadata(&frame_path)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false);
        if !fallback_ready {
            let detail = fallback_outcome.stderr.text.trim();
            let reason = if detail.is_empty() {
                "no video frame was produced".to_owned()
            } else {
                first_line(detail).to_owned()
            };
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "ffmpeg could not extract a poster frame from {}: {reason}.",
                    resolved.display()
                ),
            )
            .with_hint("The video file may have no decodable video stream or be corrupted."));
        }
    }

    let resolved_check = resolved.to_path_buf();
    let frame_for_decode = frame_path.clone();
    tokio::task::spawn_blocking(move || {
        let after_meta = fs::metadata(&resolved_check)
            .map_err(|err| map_io_error("stat", &resolved_check, err))?;
        if FileStamp::from_path(&resolved_check, &after_meta) != expected_stamp {
            return Err(changed_while_reading(&resolved_check));
        }
        let mut frame_bytes = Vec::new();
        File::open(&frame_for_decode)
            .map_err(|err| map_io_error("read", &frame_for_decode, err))?
            .take(MAX_SOURCE_BYTES + 1)
            .read_to_end(&mut frame_bytes)
            .map_err(|err| map_io_error("read", &frame_for_decode, err))?;
        if frame_bytes.len() as u64 > MAX_SOURCE_BYTES {
            return Err(ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "Extracted video frame for {} exceeded the 64 MiB limit.",
                    resolved_check.display()
                ),
            ));
        }
        let (decode_bytes, profile, override_orientation) =
            select_decode_source(&frame_for_decode, frame_bytes)?;
        let no_abort = AtomicBool::new(false);
        decode_and_render_image_checked(
            &frame_for_decode,
            decode_bytes,
            profile,
            override_orientation,
            target_size,
            fit,
            &no_abort,
        )
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb video poster encode stopped unexpectedly: {err}."),
        )
    })?
}

#[allow(clippy::too_many_arguments)]
async fn run_ffmpeg_extract(
    ffmpeg_bin: &Path,
    temp_dir: &Path,
    resolved: &Path,
    input_arg: &str,
    frame_arg: &str,
    seek: Option<&str>,
    container: VideoContainer,
    rss_limit: u64,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<legion_os::run::RunOutput, ErrorBody> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            "Extracting a video poster frame exceeded the time limit.",
        )
        .with_hint("The video may be very slow to seek or on slow storage."));
    }

    let args = build_ffmpeg_args(input_arg, frame_arg, seek, container);
    let options = RunOptions::new(CommandSpec::Argv {
        program: ffmpeg_bin.as_os_str().to_os_string(),
        args: args.into_iter().map(Into::into).collect(),
    })
    .env_clear(true)
    .envs(minimal_ffmpeg_env(temp_dir))
    .memory_limit(rss_limit)
    .rlimit_as_headroom(FFMPEG_RLIMIT_AS_HEADROOM)
    .timeout(remaining)
    .kill_grace(FFMPEG_KILL_GRACE)
    .capture_limit(FFMPEG_CAPTURE_LIMIT)
    .cancel(cancel);

    let output = run(options).await.map_err(|err| match err {
        OsError::ProgramNotFound { .. } | OsError::NotFound { .. } => ErrorBody::new(
            ErrorCode::Unsupported,
            "Video thumbnails are not available on this node because ffmpeg is not on PATH.",
        )
        .with_hint(
            "Install ffmpeg on this node and ensure it is on PATH to generate video thumbnails.",
        ),
        other => ErrorBody::new(
            ErrorCode::Failed,
            format!("Running ffmpeg for video thumbnail failed: {other}."),
        ),
    })?;

    if output.cancelled {
        return Err(cancelled_error("fs.thumb"));
    }
    if output.killed_for_memory {
        let limit_mib = rss_limit / (1024 * 1024);
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Extracting a video poster for {} exceeded the {limit_mib} MiB resident memory limit and ffmpeg was stopped.",
                resolved.display()
            ),
        )
        .with_hint("The video frame requires too much memory to decode as a thumbnail.")
        .with_details(json!({
            "reason": "too_large",
            "killed_for_memory": true,
            "limit_bytes": rss_limit,
            "peak_rss_bytes": output.peak_rss_bytes,
        })));
    }
    if output.timed_out {
        let cpu_busy = output
            .cpu_time
            .is_some_and(|cpu| cpu.mul_f32(2.0) >= remaining);
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            "Extracting a video poster frame with ffmpeg exceeded the time limit.",
        )
        .with_hint("The video may be corrupted or too slow to decode within the 15-second limit.")
        .with_details(json!({ "cpu_busy": cpu_busy })));
    }
    Ok(output)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::thumb_probe::tests::{build_exif_app1, build_mpf_app2};
    use image::{ImageBuffer, Rgb, Rgba};
    use serde_json::json;

    fn decode_helper(
        path: &Path,
        bytes: &[u8],
        size: u32,
        fit: ThumbFit,
    ) -> Result<CachedThumb, ErrorBody> {
        let (decode_bytes, profile, override_orientation) =
            select_decode_source(path, bytes.to_vec())?;
        decode_and_render_image_checked(
            path,
            decode_bytes,
            profile,
            override_orientation,
            size,
            fit,
            &AtomicBool::new(false),
        )
    }

    fn encode_quadrant_jpeg(width: u32, height: u32, top_left_rgb: [u8; 3]) -> Vec<u8> {
        let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let color = if x < width / 4 && y < height / 4 {
                    Rgb(top_left_rgb)
                } else {
                    Rgb([20, 40, 230])
                };
                img.put_pixel(x, y, color);
            }
        }
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode(&img, width, height, image::ExtendedColorType::Rgb8)
            .unwrap();
        jpeg
    }

    fn jpeg_with_exif_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let jpeg = encode_quadrant_jpeg(width, height, [240, 20, 20]);
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"Exif\0\0");
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let app1_len = (tiff.len() + 2) as u16;
        let mut out = Vec::with_capacity(jpeg.len() + tiff.len() + 4);
        out.extend_from_slice(&jpeg[..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&app1_len.to_be_bytes());
        out.extend_from_slice(&tiff);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn over_cap_progressive_444_header_jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 67, 0x00];
        out.extend_from_slice(&[16u8; 64]);
        out.extend_from_slice(&[0xFF, 0xC2, 0x00, 17, 8]);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.push(3);
        out.extend_from_slice(&[1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]);
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 12, 3, 1, 0, 2, 0, 3, 0, 0, 0, 0]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn ffmpeg_args_enforce_whitelists_pixel_alloc_caps_and_mov_dref_pinning() {
        let mov_args = build_ffmpeg_args(
            "file:/clip.mp4",
            "/tmp/out.png",
            Some("1"),
            VideoContainer::MovFamily,
        );
        let idx = |list: &[String], flag: &str| list.iter().position(|a| a == flag);
        let input_pos = idx(&mov_args, "-i").expect("-i missing");
        let proto_pos = idx(&mov_args, "-protocol_whitelist").expect("-protocol_whitelist missing");
        let fmt_pos = idx(&mov_args, "-format_whitelist").expect("-format_whitelist missing");
        let threads_pos = idx(&mov_args, "-threads").expect("-threads missing");
        let max_px_pos = idx(&mov_args, "-max_pixels").expect("-max_pixels missing");
        let max_alloc_pos = idx(&mov_args, "-max_alloc").expect("-max_alloc missing");
        let drefs_pos = idx(&mov_args, "-enable_drefs").expect("-enable_drefs missing");
        let abs_pos = idx(&mov_args, "-use_absolute_path").expect("-use_absolute_path missing");
        assert!(proto_pos < input_pos, "-protocol_whitelist must precede -i");
        assert!(fmt_pos < input_pos, "-format_whitelist must precede -i");
        assert!(threads_pos < input_pos, "-threads must precede -i");
        assert!(max_px_pos < input_pos, "-max_pixels must precede -i");
        assert!(max_alloc_pos < input_pos, "-max_alloc must precede -i");
        assert!(drefs_pos < input_pos, "-enable_drefs must precede -i");
        assert!(abs_pos < input_pos, "-use_absolute_path must precede -i");
        assert_eq!(mov_args[proto_pos + 1], "file");
        assert_eq!(mov_args[threads_pos + 1], "1");
        assert_eq!(mov_args[max_px_pos + 1], FFMPEG_MAX_PIXELS);
        assert_eq!(mov_args[max_alloc_pos + 1], FFMPEG_MAX_ALLOC);
        assert_eq!(mov_args[drefs_pos + 1], "0");
        assert_eq!(mov_args[abs_pos + 1], "0");
        assert!(mov_args.contains(&"-nostdin".to_owned()));
        assert!(mov_args.contains(&"-an".to_owned()));
        assert!(mov_args.contains(&"-sn".to_owned()));
        assert!(mov_args.contains(&"-dn".to_owned()));
        let map_pos = idx(&mov_args, "-map").expect("-map missing");
        assert_eq!(mov_args[map_pos + 1], "0:v:0");
        let mov_formats: Vec<&str> = mov_args[fmt_pos + 1].split(',').collect();
        for forbidden in ["dash", "hls", "concat", "m3u8", "http", "https"] {
            assert!(
                !mov_formats.contains(&forbidden),
                "forbidden format {forbidden}"
            );
        }

        // Non-MOV containers must NOT pass MOV-private options and MUST exclude the MOV demuxer.
        let mkv_args = build_ffmpeg_args(
            "file:/clip.mkv",
            "/tmp/out.png",
            Some("1"),
            VideoContainer::Other,
        );
        let mkv_in = idx(&mkv_args, "-i").expect("-i missing");
        let mkv_fmt = idx(&mkv_args, "-format_whitelist").expect("-format_whitelist missing");
        assert!(idx(&mkv_args, "-max_pixels").unwrap() < mkv_in);
        assert!(idx(&mkv_args, "-max_alloc").unwrap() < mkv_in);
        assert!(idx(&mkv_args, "-enable_drefs").is_none());
        assert!(idx(&mkv_args, "-use_absolute_path").is_none());
        let mkv_formats: Vec<&str> = mkv_args[mkv_fmt + 1].split(',').collect();
        for forbidden in ["mov", "mp4", "dash", "hls", "concat", "m3u8"] {
            assert!(
                !mkv_formats.contains(&forbidden),
                "non-MOV whitelist must exclude {forbidden}"
            );
        }
    }

    #[test]
    fn exif_orientation_rotates_and_flips_jpeg_correctly() {
        let bytes = jpeg_with_exif_orientation(200, 100, 6);
        let thumb = decode_helper(Path::new("/photo.jpg"), &bytes, 256, ThumbFit::Contain).unwrap();
        assert_eq!(thumb.mime, "image/jpeg");
        assert_eq!(thumb.width, 100);
        assert_eq!(thumb.height, 200);

        let decoded = image::load_from_memory(&thumb.bytes).unwrap().to_rgb8();
        let top_right = decoded.get_pixel(95, 5);
        let top_left = decoded.get_pixel(5, 5);
        assert!(
            top_right.0[0] > 180 && top_right.0[2] < 80,
            "expected red at top-right after Rotate90, got {top_right:?}"
        );
        assert!(
            top_left.0[2] > 150 && top_left.0[0] < 80,
            "expected blue at top-left after Rotate90, got {top_left:?}"
        );

        let bytes_180 = jpeg_with_exif_orientation(200, 100, 3);
        let thumb_180 = decode_helper(
            Path::new("/photo180.jpg"),
            &bytes_180,
            256,
            ThumbFit::Contain,
        )
        .unwrap();
        assert_eq!(thumb_180.width, 200);
        assert_eq!(thumb_180.height, 100);
        let dec180 = image::load_from_memory(&thumb_180.bytes).unwrap().to_rgb8();
        let bottom_right = dec180.get_pixel(190, 90);
        assert!(
            bottom_right.0[0] > 180 && bottom_right.0[2] < 80,
            "expected red at bottom-right after Rotate180, got {bottom_right:?}"
        );
    }

    #[test]
    fn over_cap_progressive_jpeg_uses_embedded_exif_and_mpf_preview_with_main_orientation() {
        // 1. Over-cap 6000x6000 (36 MP) progressive 4:4:4 JPEG (~318 MiB > 256 MiB) with an EXIF
        // IFD1 200x100 thumbnail and main EXIF Orientation=6 (Rotate90).
        let main = over_cap_progressive_444_header_jpeg(6000, 6000);
        let exif_thumb = encode_quadrant_jpeg(200, 100, [240, 20, 20]);
        let app1 = build_exif_app1(6, 56, exif_thumb.len() as u32, &exif_thumb);

        let mut with_exif_only = Vec::new();
        with_exif_only.extend_from_slice(&main[..2]);
        with_exif_only.extend_from_slice(&app1);
        with_exif_only.extend_from_slice(&main[2..]);

        let rendered = decode_helper(
            Path::new("/overcap_exif.jpg"),
            &with_exif_only,
            256,
            ThumbFit::Contain,
        )
        .unwrap();
        assert_eq!(rendered.width, 100);
        assert_eq!(rendered.height, 200);
        let dec = image::load_from_memory(&rendered.bytes).unwrap().to_rgb8();
        let top_right = dec.get_pixel(95, 5);
        assert!(
            top_right.0[0] > 180 && top_right.0[2] < 80,
            "expected Rotate90 red corner from EXIF IFD1 fallback, got {top_right:?}"
        );

        // 2. When BOTH an MPF Full-HD preview (green corner, 320x160) AND an EXIF IFD1 thumbnail
        // (red corner, 200x100) are present, the larger MPF preview is preferred AND receives the
        // main image's EXIF Orientation=6 (Rotate90 -> 160x320 -> Contain 256 -> 128x256).
        let mpf_preview = encode_quadrant_jpeg(320, 160, [20, 230, 30]);
        let preview_len = mpf_preview.len() as u32;
        let placeholder_app2 = build_mpf_app2(&[(0x030000, 0, 0), (0x010002, preview_len, 1000)]);
        // Layout: SOI (2) + APP1 + APP2 + main[2..] + mpf_preview
        // `mpf_tiff_start` = 2 + app1.len() + 2 (marker) + 2 (len) + 4 ("MPF\0")
        let mpf_tiff_start = 2 + app1.len() + 8;
        let preview_start = 2 + app1.len() + placeholder_app2.len() + (main.len() - 2);
        let rel_off = (preview_start - mpf_tiff_start) as u32;
        let app2 = build_mpf_app2(&[(0x030000, 0, 0), (0x010002, preview_len, rel_off)]);
        assert_eq!(app2.len(), placeholder_app2.len());

        let mut with_mpf = Vec::new();
        with_mpf.extend_from_slice(&main[..2]);
        with_mpf.extend_from_slice(&app1);
        with_mpf.extend_from_slice(&app2);
        with_mpf.extend_from_slice(&main[2..]);
        with_mpf.extend_from_slice(&mpf_preview);

        let rendered_mpf = decode_helper(
            Path::new("/overcap_mpf.jpg"),
            &with_mpf,
            256,
            ThumbFit::Contain,
        )
        .unwrap();
        assert_eq!(rendered_mpf.width, 128);
        assert_eq!(rendered_mpf.height, 256);
        let dec_mpf = image::load_from_memory(&rendered_mpf.bytes)
            .unwrap()
            .to_rgb8();
        let top_right_mpf = dec_mpf.get_pixel(120, 8);
        assert!(
            top_right_mpf.0[1] > 170 && top_right_mpf.0[0] < 80,
            "expected green corner from preferred MPF preview after Rotate90, got {top_right_mpf:?}"
        );
    }

    #[test]
    fn over_cap_jpeg_whose_embedded_thumbnail_is_a_bomb_returns_too_large() {
        let main = over_cap_progressive_444_header_jpeg(6000, 6000);
        // Embedded thumbnail is itself a 6000x6000 progressive 4:4:4 bomb!
        let bomb_thumb = over_cap_progressive_444_header_jpeg(6000, 6000);
        let app1 = build_exif_app1(1, 56, bomb_thumb.len() as u32, &bomb_thumb);

        let mut doc = Vec::new();
        doc.extend_from_slice(&main[..2]);
        doc.extend_from_slice(&app1);
        doc.extend_from_slice(&main[2..]);

        let err = decode_helper(Path::new("/bomb_in_exif.jpg"), &doc, 256, ThumbFit::Contain)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("reason")),
            Some(&json!("too_large")),
            "{err}"
        );
    }

    #[test]
    fn transparent_png_stays_png_and_opaque_png_becomes_jpeg() {
        let mut transparent =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(120, 120, Rgba([10, 20, 30, 255]));
        transparent.put_pixel(10, 10, Rgba([10, 20, 30, 0]));
        let mut png_alpha = Vec::new();
        PngEncoder::new(&mut png_alpha)
            .write_image(&transparent, 120, 120, image::ExtendedColorType::Rgba8)
            .unwrap();
        let res_alpha =
            decode_helper(Path::new("/alpha.png"), &png_alpha, 256, ThumbFit::Contain).unwrap();
        assert_eq!(res_alpha.mime, "image/png");

        let opaque =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(120, 120, Rgba([10, 20, 30, 255]));
        let mut png_opaque = Vec::new();
        PngEncoder::new(&mut png_opaque)
            .write_image(&opaque, 120, 120, image::ExtendedColorType::Rgba8)
            .unwrap();
        let res_opaque = decode_helper(
            Path::new("/opaque.png"),
            &png_opaque,
            256,
            ThumbFit::Contain,
        )
        .unwrap();
        assert_eq!(res_opaque.mime, "image/jpeg");
    }

    #[test]
    fn decompression_bomb_header_is_rejected_without_allocating() {
        // Tiny valid PNG header declaring 12000 x 12000 8-bit RGBA pixels (~590 MiB > 256 MiB cap).
        let mut bomb = Vec::new();
        bomb.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&12_000u32.to_be_bytes());
        ihdr.extend_from_slice(&12_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        let crc = crc32_ieee(&ihdr);
        bomb.extend_from_slice(&13u32.to_be_bytes());
        bomb.extend_from_slice(&ihdr);
        bomb.extend_from_slice(&crc.to_be_bytes());
        let idat_payload = [
            0x78u8, 0x01, 0x01, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x01,
        ];
        let mut idat = b"IDAT".to_vec();
        idat.extend_from_slice(&idat_payload);
        bomb.extend_from_slice(&(idat_payload.len() as u32).to_be_bytes());
        bomb.extend_from_slice(&idat);
        bomb.extend_from_slice(&crc32_ieee(&idat).to_be_bytes());
        bomb.extend_from_slice(&0u32.to_be_bytes());
        let iend_crc = crc32_ieee(b"IEND");
        bomb.extend_from_slice(b"IEND");
        bomb.extend_from_slice(&iend_crc.to_be_bytes());

        let err = decode_helper(Path::new("/bomb.png"), &bomb, 256, ThumbFit::Contain).unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert_eq!(
            err.details.as_ref().and_then(|d| d.get("reason")),
            Some(&json!("too_large")),
            "{err}"
        );
    }

    #[test]
    fn open_once_detects_rename_swap_even_with_identical_size_and_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("raced.png");
        let replacement = dir.path().join("replacement.png");
        fs::write(&target, b"12345678").unwrap();
        let fixed_mtime = fs::metadata(&target).unwrap().modified().unwrap();
        fs::write(&replacement, b"87654321").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_modified(fixed_mtime)
            .unwrap();

        let cache = ThumbCache::new(dir.path().join("cache"));
        let err = prepare_image_open_once(&target, &cache, 128, ThumbFit::Contain, || {
            fs::rename(&replacement, &target).unwrap();
        })
        .err()
        .unwrap();
        assert_eq!(err.code, ErrorCode::Conflict, "{err}");
    }

    #[tokio::test]
    async fn run_batch_never_drops_a_path_on_non_cancellation_error_or_panic() {
        let cancel = CancellationToken::new();
        let paths = vec![
            "/a.jpg".to_owned(),
            "/b_err.jpg".to_owned(),
            "/c_panic.jpg".to_owned(),
            "/d.jpg".to_owned(),
        ];

        let results = run_batch(paths.clone(), cancel, |path, _cancel| async move {
            if path.contains("b_err") {
                return Err(ErrorBody::new(ErrorCode::Failed, "forced worker error."));
            }
            if path.contains("c_panic") {
                #[allow(clippy::panic)]
                {
                    panic!("forced worker panic");
                }
            }
            Ok(ThumbItem::from_cached(
                path,
                CachedThumb {
                    mime: "image/jpeg",
                    width: 96,
                    height: 96,
                    bytes: vec![0xFF, 0xD8],
                },
            ))
        })
        .await
        .unwrap();

        assert_eq!(results.len(), 4);
        assert_eq!(results[0].path, "/a.jpg");
        assert!(results[0].ok.is_some());
        assert_eq!(results[1].path, "/b_err.jpg");
        assert_eq!(results[1].error.as_ref().unwrap().code, ErrorCode::Failed);
        assert_eq!(results[2].path, "/c_panic.jpg");
        assert_eq!(results[2].error.as_ref().unwrap().code, ErrorCode::Internal);
        assert_eq!(results[3].path, "/d.jpg");
        assert!(results[3].ok.is_some());
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
}
