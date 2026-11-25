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
//!   counting-allocator-measured bytes-per-pixel factor (plus `max(2 * source_bytes + 4 MiB,
//!   decode + source_bytes)` and 2 MiB overhead). Images whose estimated peak fits within **256 MiB** are decoded directly (e.g.,
//!   50 MP baseline 4:2:0 JPEG peaks around ~145 MiB + source size; 25 MP progressive 4:4:4 JPEG
//!   peaks around ~215 MiB).
//! - **Exact cross-OS worker heap cap + matching in-flight budget**: each `__thumb-worker` arms
//!   [`legion_os::capped_alloc::set_memory_cap`] with its admitted heap cap before allocating and
//!   exits immediately with [`CAP_EXCEEDED_EXIT_CODE`](legion_os::capped_alloc::CAP_EXCEEDED_EXIT_CODE)
//!   on overshoot, backed by OS limits (`RLIMIT_AS` with headroom on Linux, `RLIMIT_DATA` on other
//!   non-macOS Unix, suspended-create Job Object limits on Windows) and a 10 ms RSS watchdog. Each
//!   worker reserves its exact watchdog limit (`measured_baseline + heap_cap + max(6 MiB, 10%)`)
//!   from the shared in-flight byte pool so the sum of live workers' limits never exceeds
//!   `min(512 MiB, effective_ram / 8)`. Backstop kills while `CappedAlloc` stayed under `heap_cap`
//!   return a transient error with diagnostics and are never cached as bad.
//! - **Embedded EXIF/MPF preview fallback**: when a JPEG exceeds the 256 MiB decode cap (such as a
//!   200 MP photo or a 36+ MP progressive 4:4:4 JPEG), `fs.thumb` extracts an embedded MPF APP2
//!   preview (preferred) or EXIF IFD1 thumbnail with strict overflow-safe offset/length bounds,
//!   validates and decodes that embedded image through the exact same probe, limits, and scheduler,
//!   and applies the main image's EXIF orientation. Only when no usable embedded thumbnail exists
//!   does it return the per-path `too_large` error (`ErrorCode::Failed` with
//!   `details.reason = "too_large"`).
//! - **In-flight byte-budget scheduling**: total in-flight image decodes and `<= 1080p` video
//!   posters across all sessions are scheduled round-robin by principal against
//!   `min(512 MiB, effective_ram / 8)` (floored at the 256 MiB per-image admission cap so an
//!   admitted image never deadlocks) across up to **16** concurrent decodes (scaled to CPU cores,
//!   bounded by the shared byte budget), with a **10 s** bounded wait (returning retryable `busy`)
//!   and a **10 s** wall-clock decode timeout. Exclusive `> 1080p` (4K/8K) video poster extractions
//!   drain the entire shared pool and all video slots first so they run strictly one at a time
//!   with zero concurrent images or videos under an exclusive `min(768 MiB, max(256 MiB,
//!   effective_ram / 4))` resident budget.
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
use std::io::{Cursor, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};
use legion_core::media::{Category, classify_path};
use legion_core::proto::{Effect, ErrorBody, ErrorCode, ToolSpec};
use legion_core::{
    Access, BoxFuture, CancellationToken, Ctx, Grant, Registry, Tool, ToolError, input_schema,
    parse_args,
};
use legion_os::OsError;
use legion_os::file_id::{self, FileId};
use legion_os::mem_limit::effective_memory_bytes;
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::task::JoinSet;

use super::audio_tags::{
    AudioFormat, CoverExtractMode, audio_format_for_path, mp4_has_video_track, parse_audio,
};
use super::tags::find_folder_covers;
use super::thumb_cache::{
    CachedThumb, ThumbCache, bad_cache_key, cache_key, tags_cache_key, temp_dir_in_cache,
};
use super::thumb_limit::{AcquireError, FairPool, compute_image_in_flight_budget};
use super::thumb_probe::{
    ImageProfile, MAX_IMAGE_DECODE_BYTES, image_limits, is_mpeg_ts_file, peek_image_file,
    probe_image_profile, target_output_overhead_bytes, too_large_error,
};
use super::thumb_worker::{
    AudioCoverWorkerOutcome, WorkerOutcome, compute_worker_limits, effective_decode_timeout,
    heap_cap_from_estimate, minimal_system_env, run_audio_cover_worker, run_thumb_worker,
    worker_baseline_rss_bytes, worker_enabled,
};
use super::{
    cancelled_error, check_fs_path, check_regular_file, map_io_error, open_regular_file, platform,
};

pub(super) const MIN_PATHS: usize = 1;
pub(super) const MAX_PATHS: usize = 32;
/// Maximum cumulative base64 thumbnail payload allowed in a single `fs.thumb` reply (12 MiB),
/// keeping the full JSON-RPC reply line comfortably under the 16 MiB link line limit while always
/// admitting at least one worst-case 1024×1024 PNG thumbnail (~5.6 MiB base64).
pub(super) const MAX_REPLY_BASE64_BYTES: usize = 12 * 1024 * 1024;
const MIN_THUMB_SIZE: u32 = 96;
const DEFAULT_THUMB_SIZE: u32 = 256;
const MAX_THUMB_SIZE: u32 = 1024;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Initial live-heap cap for `--audio-cover` workers (112 MiB: extracts up to 16 MiB tag/cover
/// data and decodes standard covers up to ~70 MiB estimated peak in a single launch, tightening
/// down to the exact cover probe estimate before decoding and retrying once with a larger pool
/// reservation only when an embedded cover legitimately needs more).
const AUDIO_COVER_INITIAL_HEAP_CAP_BYTES: u64 = 112 * 1024 * 1024;
const MAX_IN_FLIGHT_IMAGE_BYTES: u64 = 512 * 1024 * 1024;
const MIN_IN_FLIGHT_IMAGES: usize = 8;
const MAX_IN_FLIGHT_IMAGES_CAP: usize = 16;
const MAX_IN_FLIGHT_VIDEOS: usize = 2;
pub(super) const IMAGE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const IMAGE_DECODE_TIMEOUT: Duration = Duration::from_secs(10);
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
pub(super) struct TimeoutBackoff {
    entries: Mutex<HashMap<String, TimeoutEntry>>,
}

impl TimeoutBackoff {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn active_remaining(&self, bad_key: &str) -> Option<Duration> {
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

    pub(super) fn record_timeout(&self, bad_key: &str) {
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

pub(super) fn timeout_backoff() -> &'static TimeoutBackoff {
    TIMEOUT_BACKOFF.get_or_init(TimeoutBackoff::new)
}

pub(super) fn machine_in_flight_image_budget() -> u64 {
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

fn max_in_flight_images() -> usize {
    std::thread::available_parallelism().map_or(MIN_IN_FLIGHT_IMAGES, |n| {
        n.get()
            .clamp(MIN_IN_FLIGHT_IMAGES, MAX_IN_FLIGHT_IMAGES_CAP)
    })
}

pub(super) fn image_pool() -> Arc<FairPool> {
    Arc::clone(
        IMAGE_POOL.get_or_init(|| {
            FairPool::new(machine_in_flight_image_budget(), max_in_flight_images())
        }),
    )
}

fn video_pool() -> Arc<FairPool> {
    Arc::clone(
        VIDEO_POOL.get_or_init(|| FairPool::new(MAX_IN_FLIGHT_VIDEOS as u64, MAX_IN_FLIGHT_VIDEOS)),
    )
}

/// Direct [`Tool`] implementation for `fs.thumb` so the JSON reply object is assembled once
/// without building an intermediate Rust struct and cloning every base64 thumbnail into a second
/// [`serde_json::Value`] tree.
struct ThumbTool {
    spec: ToolSpec,
}

impl Tool for ThumbTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn call<'a>(&'a self, ctx: &'a Ctx, args: Value) -> BoxFuture<'a, Result<Value, ToolError>> {
        Box::pin(async move {
            let parsed = parse_args::<ThumbArgs>(&self.spec.name, args)?;
            thumb(ctx, parsed).await
        })
    }
}

pub(super) fn register(registry: &mut Registry) {
    registry.add(ThumbTool {
        spec: ToolSpec {
            name: "fs.thumb".to_owned(),
            description: "Generate thumbnails for 1 to 32 image, audio or video paths on this node (size 96 to 1024; 64 MiB image source cap, 256 MiB estimated decode memory per image with EXIF/MPF preview fallback for larger JPEGs, audio embedded cover or folder cover fallback with 16 MiB tag cap, out-of-process memory-bounded workers, in-flight budget capped at min(512 MiB, effective RAM/8)), returning per-path base64 JPEG (or PNG when transparent) thumbnails or per-path errors in input order, cached in a 256 MiB LRU store in the state directory. Total base64 content in one reply is capped at 12 MiB; once adding the next path would exceed that budget, that path and every later path return retryable busy (details.reason = \"reply_budget\") so the caller can ask for them again.".to_owned(),
            effect: Effect::Read,
            input_schema: input_schema::<ThumbArgs>(),
        },
    });
}

/// How a thumbnail is fitted into the square target dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, JsonSchema)]
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
    /// Target thumbnail dimension in pixels, from 96 to 1024 (default 256).
    #[serde(default = "default_thumb_size")]
    #[schemars(range(min = 96, max = 1024))]
    size: u32,
    /// Fitting mode (`contain` or `cover`, default `contain`).
    #[serde(default)]
    fit: ThumbFit,
}

fn default_thumb_size() -> u32 {
    DEFAULT_THUMB_SIZE
}

/// Unencoded per-path outcome returned by a worker before input-order budget accounting and
/// single-pass base64 JSON encoding.
enum ThumbOutcome {
    Ok(CachedThumb),
    Error(ErrorBody),
}

const fn base64_encoded_len(raw_len: usize) -> usize {
    raw_len.div_ceil(3).saturating_mul(4)
}

fn reply_budget_error(path: &str) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Busy,
        format!(
            "Reply base64 budget (12 MiB) reached before {path}; ask for this path again in another fs.thumb call."
        ),
    )
    .with_hint("Call fs.thumb again with the remaining paths that returned busy (reply_budget).")
    .with_details(json!({
        "reason": "reply_budget",
        "limit_bytes": MAX_REPLY_BASE64_BYTES,
    }))
}

fn thumb_ok_value(path: String, thumb: CachedThumb) -> (usize, Value) {
    let content = BASE64.encode(&thumb.bytes);
    let content_len = content.len();
    drop(thumb.bytes);
    let mut ok = Map::with_capacity(4);
    ok.insert("mime".to_owned(), Value::String(thumb.mime.to_owned()));
    ok.insert("content".to_owned(), Value::String(content));
    ok.insert("width".to_owned(), Value::from(thumb.width));
    ok.insert("height".to_owned(), Value::from(thumb.height));
    let mut item = Map::with_capacity(2);
    item.insert("path".to_owned(), Value::String(path));
    item.insert("ok".to_owned(), Value::Object(ok));
    (content_len, Value::Object(item))
}

fn thumb_error_value(path: String, error: ErrorBody) -> Value {
    let mut item = Map::with_capacity(2);
    item.insert("path".to_owned(), Value::String(path));
    let err_val = serde_json::to_value(error).unwrap_or_else(|_| {
        json!({
            "code": "internal",
            "message": "Failed to serialize thumbnail error."
        })
    });
    item.insert("error".to_owned(), err_val);
    Value::Object(item)
}

/// Metadata + OS identity snapshot used for cache keys and TOCTOU checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileStamp {
    pub(super) size: u64,
    pub(super) mtime_nanos: Option<i128>,
    pub(super) file_id: Option<FileId>,
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

    pub(super) fn from_open_file(file: &File, meta: &Metadata) -> FileStamp {
        Self::from_meta_and_id(meta, file_id::from_file(file))
    }

    pub(super) fn from_path(path: &Path, meta: &Metadata) -> FileStamp {
        Self::from_meta_and_id(meta, file_id::from_path(path))
    }
}

async fn thumb(ctx: &Ctx, args: ThumbArgs) -> Result<Value, ToolError> {
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
        .with_hint("Choose a thumbnail size between 96 and 1024 pixels."));
    }

    let cache_dir = ctx.paths().thumb_cache_dir();
    let cache = ThumbCache::for_dir(cache_dir.clone());
    let grant = ctx.grant().clone();
    let principal = ctx.principal().to_owned();
    let cancel = ctx.cancel_token().clone();
    let size = args.size;
    let fit = args.fit;

    let results = run_batch(
        args.paths,
        size,
        MAX_REPLY_BASE64_BYTES,
        cancel,
        move |raw_path, cancel| {
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
        },
    )
    .await?;

    let mut root = Map::with_capacity(1);
    root.insert("results".to_owned(), Value::Array(results));
    Ok(Value::Object(root))
}

/// Runs per-path workers across a bounded sliding window ahead of the in-order commit cursor,
/// encodes each winning thumbnail directly into its final JSON [`Value`] once while freeing raw
/// bytes immediately, and stops committing once adding the next thumbnail would exceed
/// `reply_b64_budget`. Every path from the overflowing index onward receives a retryable
/// `busy` (`details.reason = "reply_budget"`) error in input order, and in-flight work past the
/// cutoff is cancelled immediately.
async fn run_batch<F, Fut>(
    paths: Vec<String>,
    size: u32,
    reply_b64_budget: usize,
    cancel: CancellationToken,
    worker: F,
) -> Result<Vec<Value>, ToolError>
where
    F: Fn(String, CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<ThumbOutcome, ToolError>> + Send + 'static,
{
    let total = paths.len();
    let mut original_paths = paths;
    // Keep lookahead small for large target sizes so raw thumbnails stay tightly bounded before the
    // 12 MiB base64 cutoff triggers: at 1024px, worst-case transparent PNGs are ~4.2 MiB raw (~5.6
    // MiB base64) so at most 2 fit in one reply anyway; at 512px, up to 4 raw buffers is <= 4.2 MiB.
    let max_window = if size > 512 {
        2usize
    } else if size >= 512 {
        4usize
    } else {
        max_in_flight_images()
    }
    .min(total.max(1));

    let batch_cancel = cancel.child_token();
    let mut set = JoinSet::new();
    let mut task_owners: HashMap<tokio::task::Id, usize> = HashMap::with_capacity(max_window);
    let mut slots: Vec<Option<ThumbOutcome>> = (0..total).map(|_| None).collect();
    let mut results: Vec<Value> = Vec::with_capacity(total);

    let mut next_to_spawn = 0usize;
    let mut next_to_commit = 0usize;
    let mut committed_b64_bytes = 0usize;
    let mut budget_exhausted = false;

    while next_to_commit < total {
        while !budget_exhausted
            && next_to_spawn < total
            && next_to_spawn - next_to_commit < max_window
        {
            let idx = next_to_spawn;
            let raw_path = original_paths[idx].clone();
            let fut = worker(raw_path, batch_cancel.clone());
            let handle = set.spawn(async move {
                let outcome = fut.await;
                (idx, outcome)
            });
            task_owners.insert(handle.id(), idx);
            next_to_spawn += 1;
        }

        let Some(joined) = set.join_next_with_id().await else {
            break;
        };

        match joined {
            Ok((task_id, (idx, Ok(outcome)))) => {
                task_owners.remove(&task_id);
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(outcome);
                }
            }
            Ok((task_id, (idx, Err(err)))) => {
                task_owners.remove(&task_id);
                if err.code == ErrorCode::Cancelled {
                    if cancel.is_cancelled() {
                        batch_cancel.cancel();
                        set.abort_all();
                        while set.join_next().await.is_some() {}
                        return Err(err);
                    }
                    if let Some(slot) = slots.get_mut(idx) {
                        *slot = Some(ThumbOutcome::Error(err));
                    }
                } else if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(ThumbOutcome::Error(err));
                }
            }
            Err(join_err) => {
                let task_id = join_err.id();
                if let Some(idx) = task_owners.remove(&task_id)
                    && let Some(slot) = slots.get_mut(idx)
                {
                    *slot = Some(ThumbOutcome::Error(ErrorBody::new(
                        ErrorCode::Internal,
                        format!("Thumbnail worker stopped unexpectedly: {join_err}."),
                    )));
                }
            }
        }

        while next_to_commit < total {
            let Some(outcome) = slots[next_to_commit].take() else {
                break;
            };
            match outcome {
                ThumbOutcome::Ok(thumb) => {
                    let b64_len = base64_encoded_len(thumb.bytes.len());
                    if committed_b64_bytes.saturating_add(b64_len) <= reply_b64_budget {
                        let path = std::mem::take(&mut original_paths[next_to_commit]);
                        let (written_len, val) = thumb_ok_value(path, thumb);
                        committed_b64_bytes = committed_b64_bytes.saturating_add(written_len);
                        results.push(val);
                        next_to_commit += 1;
                    } else {
                        drop(thumb);
                        budget_exhausted = true;
                        break;
                    }
                }
                ThumbOutcome::Error(err) => {
                    let path = std::mem::take(&mut original_paths[next_to_commit]);
                    results.push(thumb_error_value(path, err));
                    next_to_commit += 1;
                }
            }
        }

        if budget_exhausted {
            // Free any already-completed raw thumbnails past the cutoff immediately, then cancel
            // and reap in-flight workers so they release pool permits and child processes cleanly.
            for slot in &mut slots[next_to_commit..] {
                *slot = None;
            }
            batch_cancel.cancel();
            let drained = tokio::time::timeout(Duration::from_secs(2), async {
                while set.join_next().await.is_some() {}
            })
            .await;
            if drained.is_err() {
                set.abort_all();
                while set.join_next().await.is_some() {}
            }
            break;
        }
    }

    if cancel.is_cancelled() {
        return Err(cancelled_error("fs.thumb"));
    }

    if budget_exhausted {
        for path in original_paths.into_iter().skip(next_to_commit) {
            let err = reply_budget_error(&path);
            results.push(thumb_error_value(path, err));
        }
    } else if next_to_commit < total {
        for path in original_paths.into_iter().skip(next_to_commit) {
            results.push(thumb_error_value(
                path,
                ErrorBody::new(
                    ErrorCode::Internal,
                    "Thumbnail worker exited without returning a result.",
                ),
            ));
        }
    }

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
) -> Result<ThumbOutcome, ToolError> {
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
        Err(err) => return Ok(ThumbOutcome::Error(err)),
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
    } else if category == Category::Video
        && resolved
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
    {
        let resolved_sniff = resolved.clone();
        let is_audio_only_mp4 = tokio::task::spawn_blocking(move || {
            let Ok((mut file, meta)) = open_regular_file(
                &resolved_sniff,
                "thumbnail",
                "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
            ) else {
                return false;
            };
            mp4_has_video_track(&mut file, meta.len()) == Some(false)
        })
        .await
        .unwrap_or(false);
        if is_audio_only_mp4 {
            category = Category::Audio;
        }
    }

    match category {
        Category::Image => thumb_image(principal, cache, resolved, size, fit, cancel).await,
        Category::Audio => thumb_audio(grant, principal, cache, resolved, size, fit, cancel).await,
        Category::Video => {
            thumb_video(principal, cache, cache_dir, resolved, size, fit, cancel).await
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
                return Ok(ThumbOutcome::Error(err));
            }
            Ok(ThumbOutcome::Error(
                ErrorBody::new(
                    ErrorCode::Unsupported,
                    format!(
                        "{} ({}) cannot be thumbnailed.",
                        resolved.display(),
                        category
                    ),
                )
                .with_hint("fs.thumb supports raster images (JPEG, PNG, GIF, WebP, BMP), audio covers (embedded or folder picture), and videos (when ffmpeg is on PATH)."),
            ))
        }
    }
}

enum AudioCoverPrep {
    CacheHit(CachedThumb),
    CachedBad(ErrorBody),
    /// Cached tags already confirmed this audio file has no embedded cover; jump straight to
    /// folder cover lookup.
    NoEmbeddedCover,
    Ready {
        key: String,
        bad_key: String,
        tags_key: String,
        open_stamp: FileStamp,
        file: File,
        format: AudioFormat,
    },
}

fn prepare_audio_cover_open_once(
    path: &Path,
    cache: &ThumbCache,
    target_size: u32,
    fit: ThumbFit,
) -> Result<AudioCoverPrep, ErrorBody> {
    let pre_meta = check_regular_file(
        path,
        "thumbnail",
        "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
    )?;
    let pre_stamp = FileStamp::from_path(path, &pre_meta);

    let (file, open_meta) = open_regular_file(
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
                "{} is empty and cannot be read as an audio file.",
                path.display()
            ),
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
        return Ok(AudioCoverPrep::CacheHit(hit));
    }

    let bad_key = bad_cache_key(
        path,
        open_stamp.size,
        open_stamp.mtime_nanos,
        open_stamp.file_id,
        target_size,
        fit,
    );
    if let Some(bad) = cache.get_bad(&bad_key) {
        return Ok(AudioCoverPrep::CachedBad(bad));
    }

    let tags_key = tags_cache_key(
        path,
        open_stamp.size,
        open_stamp.mtime_nanos,
        open_stamp.file_id,
    );
    if let Some(cached_tags) = cache.get_tags(&tags_key)
        && !cached_tags.has_embedded_cover
    {
        return Ok(AudioCoverPrep::NoEmbeddedCover);
    }

    let format = audio_format_for_path(path).unwrap_or(AudioFormat::GenericAudio);
    Ok(AudioCoverPrep::Ready {
        key,
        bad_key,
        tags_key,
        open_stamp,
        file,
        format,
    })
}

async fn record_bad_audio_cover(
    cache: &ThumbCache,
    resolved: &Path,
    file: File,
    open_stamp: FileStamp,
    bad_key: String,
    error: &ErrorBody,
) {
    let resolved_check = resolved.to_path_buf();
    let cache_bad = cache.clone();
    let err_bad = error.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if verify_post_read_stamp(&resolved_check, &file, open_stamp).is_ok() {
            cache_bad.put_bad(&bad_key, &err_bad);
        }
    })
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn thumb_audio(
    grant: &Grant,
    principal: &str,
    cache: &ThumbCache,
    resolved: PathBuf,
    size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<ThumbOutcome, ToolError> {
    let cache_prep = cache.clone();
    let resolved_prep = resolved.clone();
    let prep = tokio::task::spawn_blocking(move || {
        prepare_audio_cover_open_once(&resolved_prep, &cache_prep, size, fit)
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb audio open stopped unexpectedly: {err}."),
        )
    })?;

    let (key, bad_key, tags_key, open_stamp, mut file, format) = match prep {
        Ok(AudioCoverPrep::CacheHit(hit)) => return Ok(ThumbOutcome::Ok(hit)),
        Ok(AudioCoverPrep::CachedBad(err)) => {
            return thumb_audio_folder_cover(
                grant,
                principal,
                cache,
                resolved,
                size,
                fit,
                Some(err),
                cancel,
            )
            .await;
        }
        Ok(AudioCoverPrep::NoEmbeddedCover) => {
            return thumb_audio_folder_cover(
                grant, principal, cache, resolved, size, fit, None, cancel,
            )
            .await;
        }
        Ok(AudioCoverPrep::Ready {
            key,
            bad_key,
            tags_key,
            open_stamp,
            file,
            format,
        }) => (key, bad_key, tags_key, open_stamp, file, format),
        Err(err) => return Ok(ThumbOutcome::Error(err)),
    };

    if let Some(remaining) = timeout_backoff().active_remaining(&bad_key) {
        return Ok(ThumbOutcome::Error(
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Decoding cover art for {} timed out repeatedly and is in a temporary back-off ({} s remaining).",
                    resolved.display(),
                    remaining.as_secs().max(1)
                ),
            )
            .with_hint(
                "Timeouts are not cached permanently; retry after the short back-off expires.",
            ),
        ));
    }

    let decode_timeout = effective_decode_timeout(IMAGE_DECODE_TIMEOUT);

    if worker_enabled() {
        let mut requested_heap_cap = AUDIO_COVER_INITIAL_HEAP_CAP_BYTES;
        for attempt in 0..2u8 {
            let allow_needs_cap = attempt == 0;
            let budget = machine_in_flight_image_budget();
            let baseline = worker_baseline_rss_bytes();
            let (heap_cap_bytes, rss_limit_bytes, baseline_rss_bytes) =
                compute_worker_limits(requested_heap_cap, budget, baseline);

            let permit = match image_pool()
                .acquire(principal, rss_limit_bytes, IMAGE_WAIT_TIMEOUT, &cancel)
                .await
            {
                Ok(permit) => permit,
                Err(AcquireError::Cancelled) => return Err(cancelled_error("fs.thumb")),
                Err(AcquireError::TimedOut) => {
                    return Ok(ThumbOutcome::Error(
                        ErrorBody::new(
                            ErrorCode::Busy,
                            format!(
                                "Timed out waiting for thumbnail decode capacity for {}.",
                                resolved.display()
                            ),
                        )
                        .with_hint(
                            "Thumbnail decoding is busy on this node; retry fs.thumb shortly.",
                        ),
                    ));
                }
            };

            let worker_file = match file.try_clone() {
                Ok(mut cloned) => {
                    if attempt > 0
                        && let Err(err) = cloned.seek(SeekFrom::Start(0))
                    {
                        drop(permit);
                        return Ok(ThumbOutcome::Error(map_io_error("read", &resolved, err)));
                    }
                    cloned
                }
                Err(err) => {
                    drop(permit);
                    return Ok(ThumbOutcome::Error(map_io_error("read", &resolved, err)));
                }
            };

            let worker_res = run_audio_cover_worker(
                &resolved,
                worker_file,
                format,
                size,
                fit,
                heap_cap_bytes,
                allow_needs_cap,
                rss_limit_bytes,
                baseline_rss_bytes,
                decode_timeout,
                cancel.clone(),
            )
            .await;
            drop(permit);

            let worker_outcome = match worker_res {
                Ok(outcome) => outcome,
                Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
                Err(err) => return Ok(ThumbOutcome::Error(err)),
            };

            match worker_outcome {
                AudioCoverWorkerOutcome::Ok(thumb) => {
                    let resolved_check = resolved.clone();
                    let stamp_ok = tokio::task::spawn_blocking(move || {
                        verify_post_read_stamp(&resolved_check, &file, open_stamp)
                    })
                    .await
                    .map_err(|err| {
                        ErrorBody::new(
                            ErrorCode::Internal,
                            format!("fs.thumb post-read check stopped unexpectedly: {err}."),
                        )
                    })?;
                    if let Err(err) = stamp_ok {
                        return Ok(ThumbOutcome::Error(err));
                    }
                    let cache_put = cache.clone();
                    let thumb = tokio::task::spawn_blocking(move || {
                        cache_put.put(&key, &thumb);
                        thumb
                    })
                    .await
                    .map_err(|err| {
                        ErrorBody::new(
                            ErrorCode::Internal,
                            format!("fs.thumb cache write stopped unexpectedly: {err}."),
                        )
                    })?;
                    return Ok(ThumbOutcome::Ok(thumb));
                }
                AudioCoverWorkerOutcome::NoEmbeddedCover(parsed_tags) => {
                    let resolved_check = resolved.clone();
                    let cache_put = cache.clone();
                    let tags_key_put = tags_key.clone();
                    let stamp_ok = tokio::task::spawn_blocking(move || {
                        verify_post_read_stamp(&resolved_check, &file, open_stamp)?;
                        cache_put.put_tags(&tags_key_put, &parsed_tags);
                        Ok::<(), ErrorBody>(())
                    })
                    .await
                    .map_err(|err| {
                        ErrorBody::new(
                            ErrorCode::Internal,
                            format!("fs.thumb post-read check stopped unexpectedly: {err}."),
                        )
                    })?;
                    if let Err(err) = stamp_ok {
                        return Ok(ThumbOutcome::Error(err));
                    }
                    return thumb_audio_folder_cover(
                        grant, principal, cache, resolved, size, fit, None, cancel,
                    )
                    .await;
                }
                AudioCoverWorkerOutcome::NeedsCap(needed) if attempt == 0 => {
                    requested_heap_cap = needed;
                    continue;
                }
                AudioCoverWorkerOutcome::NeedsCap(_) => {
                    let err = ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "Embedded cover in {} exceeded the allowed thumbnail decode memory budget.",
                            resolved.display()
                        ),
                    )
                    .with_details(json!({"reason": "too_large"}));
                    record_bad_audio_cover(cache, &resolved, file, open_stamp, bad_key, &err).await;
                    return thumb_audio_folder_cover(
                        grant,
                        principal,
                        cache,
                        resolved,
                        size,
                        fit,
                        Some(err),
                        cancel,
                    )
                    .await;
                }
                AudioCoverWorkerOutcome::Rejected(err) => {
                    record_bad_audio_cover(cache, &resolved, file, open_stamp, bad_key, &err).await;
                    return thumb_audio_folder_cover(
                        grant,
                        principal,
                        cache,
                        resolved,
                        size,
                        fit,
                        Some(err),
                        cancel,
                    )
                    .await;
                }
                AudioCoverWorkerOutcome::TransientFailure(err) => {
                    return Ok(ThumbOutcome::Error(err));
                }
                AudioCoverWorkerOutcome::TimedOut { error, cpu_busy } => {
                    if cpu_busy {
                        timeout_backoff().record_timeout(&bad_key);
                    }
                    return Ok(ThumbOutcome::Error(error));
                }
                AudioCoverWorkerOutcome::MemoryLimitExceeded { error, .. } => {
                    record_bad_audio_cover(cache, &resolved, file, open_stamp, bad_key, &error)
                        .await;
                    return thumb_audio_folder_cover(
                        grant,
                        principal,
                        cache,
                        resolved,
                        size,
                        fit,
                        Some(error),
                        cancel,
                    )
                    .await;
                }
            }
        }
    }

    // In-process fallback (standard unit/integration test binaries without `__thumb-worker`).
    let budget = machine_in_flight_image_budget();
    let baseline = worker_baseline_rss_bytes();
    let (_, reserve_bytes, _) =
        compute_worker_limits(AUDIO_COVER_INITIAL_HEAP_CAP_BYTES, budget, baseline);
    let permit = match image_pool()
        .acquire(principal, reserve_bytes, IMAGE_WAIT_TIMEOUT, &cancel)
        .await
    {
        Ok(permit) => permit,
        Err(AcquireError::Cancelled) => return Err(cancelled_error("fs.thumb")),
        Err(AcquireError::TimedOut) => {
            return Ok(ThumbOutcome::Error(
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

    let handle_check = file.try_clone().ok();
    let abort_flag = Arc::new(AtomicBool::new(false));
    let abort_worker = Arc::clone(&abort_flag);
    let resolved_decode = resolved.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let extracted = parse_audio(
            &mut file,
            open_stamp.size,
            format,
            CoverExtractMode::ExtractBytes,
            &resolved_decode,
        )?;
        verify_post_read_stamp(&resolved_decode, &file, open_stamp)?;
        match extracted.cover {
            Some(cover_bytes) => {
                let (decode_bytes, profile, override_orientation) =
                    select_decode_source(&resolved_decode, cover_bytes)?;
                let thumb = decode_and_render_image_checked(
                    &resolved_decode,
                    decode_bytes,
                    profile,
                    override_orientation,
                    size,
                    fit,
                    &abort_worker,
                )?;
                Ok::<Result<CachedThumb, super::audio_tags::ParsedAudioTags>, ErrorBody>(Ok(thumb))
            }
            None => Ok(Err(extracted.tags)),
        }
    });

    let sleep = tokio::time::sleep(decode_timeout);
    tokio::pin!(sleep);

    let outcome = tokio::select! {
        joined = &mut handle => {
            joined.map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb audio cover decode stopped unexpectedly: {err}."),
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
                    "Decoding cover art for {} exceeded the {}-second time limit.",
                    resolved.display(),
                    decode_timeout.as_secs().max(1)
                ),
            )
            .with_hint("The audio file or cover image may be malformed or too expensive to decode quickly."))
        }
    };

    match outcome {
        Ok(Ok(thumb)) => {
            let cache_put = cache.clone();
            let thumb = tokio::task::spawn_blocking(move || {
                cache_put.put(&key, &thumb);
                thumb
            })
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb cache write stopped unexpectedly: {err}."),
                )
            })?;
            Ok(ThumbOutcome::Ok(thumb))
        }
        Ok(Err(parsed_tags)) => {
            let cache_put = cache.clone();
            let _ =
                tokio::task::spawn_blocking(move || cache_put.put_tags(&tags_key, &parsed_tags))
                    .await;
            thumb_audio_folder_cover(grant, principal, cache, resolved, size, fit, None, cancel)
                .await
        }
        Err(err) if err.code == ErrorCode::Cancelled => Err(err),
        Err(err) => {
            if matches!(
                err.code,
                ErrorCode::Timeout | ErrorCode::Busy | ErrorCode::Conflict
            ) {
                return Ok(ThumbOutcome::Error(err));
            }
            if let Some(check_file) = handle_check {
                record_bad_audio_cover(cache, &resolved, check_file, open_stamp, bad_key, &err)
                    .await;
            }
            thumb_audio_folder_cover(
                grant,
                principal,
                cache,
                resolved,
                size,
                fit,
                Some(err),
                cancel,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn thumb_audio_folder_cover(
    grant: &Grant,
    principal: &str,
    cache: &ThumbCache,
    resolved: PathBuf,
    size: u32,
    fit: ThumbFit,
    embedded_failure: Option<ErrorBody>,
    cancel: CancellationToken,
) -> Result<ThumbOutcome, ToolError> {
    let grant_cover = grant.clone();
    let resolved_cover = resolved.clone();
    let folder_covers =
        tokio::task::spawn_blocking(move || find_folder_covers(&grant_cover, &resolved_cover))
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb folder cover lookup stopped unexpectedly: {err}."),
                )
            })?;

    for cover_path in folder_covers {
        match thumb_image(principal, cache, cover_path, size, fit, cancel.clone()).await? {
            ThumbOutcome::Ok(thumb) => return Ok(ThumbOutcome::Ok(thumb)),
            ThumbOutcome::Error(err) => {
                if matches!(
                    err.code,
                    ErrorCode::Busy | ErrorCode::Timeout | ErrorCode::Conflict
                ) {
                    return Ok(ThumbOutcome::Error(err));
                }
                // Undecodable folder candidate: try the next candidate in priority order.
            }
        }
    }

    let mut not_found = ErrorBody::new(
        ErrorCode::NotFound,
        format!(
            "No decodable embedded or folder cover found for {}.",
            resolved.display()
        ),
    )
    .with_hint("Add a valid embedded cover image or place a cover, folder, front, or album picture (.jpg, .jpeg, .png, or .webp) in the same folder.");
    if let Some(details) = embedded_failure.and_then(|err| err.details) {
        not_found = not_found.with_details(details);
    }
    Ok(ThumbOutcome::Error(not_found))
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
        baseline_rss_bytes: u64,
    },
}

async fn thumb_image(
    principal: &str,
    cache: &ThumbCache,
    resolved: std::path::PathBuf,
    size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<ThumbOutcome, ToolError> {
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

    let (
        key,
        bad_key,
        open_stamp,
        file,
        reserve_bytes,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
    ) = match prep {
        Ok(ImagePrep::CacheHit(hit)) => return Ok(ThumbOutcome::Ok(hit)),
        Ok(ImagePrep::CachedBad(err)) => return Ok(ThumbOutcome::Error(err)),
        Ok(ImagePrep::Ready {
            key,
            bad_key,
            open_stamp,
            file,
            reserve_bytes,
            heap_cap_bytes,
            rss_limit_bytes,
            baseline_rss_bytes,
        }) => (
            key,
            bad_key,
            open_stamp,
            file,
            reserve_bytes,
            heap_cap_bytes,
            rss_limit_bytes,
            baseline_rss_bytes,
        ),
        Err(err) => return Ok(ThumbOutcome::Error(err)),
    };

    if let Some(remaining) = timeout_backoff().active_remaining(&bad_key) {
        return Ok(ThumbOutcome::Error(
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
            return Ok(ThumbOutcome::Error(
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
                return Ok(ThumbOutcome::Error(map_io_error("read", &resolved, err)));
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
            baseline_rss_bytes,
            decode_timeout,
            cancel.clone(),
        )
        .await;
        drop(permit);

        let worker_outcome = match worker_res {
            Ok(outcome) => outcome,
            Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
            Err(err) => return Ok(ThumbOutcome::Error(err)),
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
                    return Ok(ThumbOutcome::Error(err));
                }
                let cache_put = cache.clone();
                let thumb = tokio::task::spawn_blocking(move || {
                    cache_put.put(&key, &thumb);
                    thumb
                })
                .await
                .map_err(|err| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("fs.thumb cache write stopped unexpectedly: {err}."),
                    )
                })?;
                Ok(ThumbOutcome::Ok(thumb))
            }
            WorkerOutcome::Rejected(err) | WorkerOutcome::TransientFailure(err) => {
                // External kills, crashes, and normal format rejections are NOT cached as bad.
                Ok(ThumbOutcome::Error(err))
            }
            WorkerOutcome::TimedOut { error, cpu_busy } => {
                // Timeouts are NOT stored permanently on disk; repeated CPU-busy timeouts get a
                // short in-memory back-off to bound repeated damage.
                if cpu_busy {
                    timeout_backoff().record_timeout(&bad_key);
                }
                Ok(ThumbOutcome::Error(error))
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
                Ok(ThumbOutcome::Error(error))
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
            let thumb = tokio::task::spawn_blocking(move || {
                cache_put.put(&key, &thumb);
                thumb
            })
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb cache write stopped unexpectedly: {err}."),
                )
            })?;
            Ok(ThumbOutcome::Ok(thumb))
        }
        Err(err) if err.code == ErrorCode::Cancelled => Err(err),
        Err(err) => Ok(ThumbOutcome::Error(err)),
    }
}

#[allow(clippy::too_many_arguments)]
async fn thumb_video(
    principal: &str,
    cache: &ThumbCache,
    cache_dir: &Path,
    resolved: std::path::PathBuf,
    size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<ThumbOutcome, ToolError> {
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
        let bad_key = bad_cache_key(
            &resolved_stat,
            stamp.size,
            stamp.mtime_nanos,
            stamp.file_id,
            size,
            fit,
        );
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
        Err(err) => return Ok(ThumbOutcome::Error(err)),
    };
    if let Some(hit) = hit {
        return Ok(ThumbOutcome::Ok(hit));
    }
    if let Some(bad) = bad {
        return Ok(ThumbOutcome::Error(bad));
    }
    if let Some(remaining) = timeout_backoff().active_remaining(&bad_key) {
        return Ok(ThumbOutcome::Error(
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
                return Ok(ThumbOutcome::Error(
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
                return Ok(ThumbOutcome::Error(
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
            let thumb = tokio::task::spawn_blocking(move || {
                cache_put.put(&key, &thumb);
                thumb
            })
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb cache write stopped unexpectedly: {err}."),
                )
            })?;
            Ok(ThumbOutcome::Ok(thumb))
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
            Ok(ThumbOutcome::Error(err))
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
        let orientation = jpeg_meta.map(|meta| meta.orientation);
        return Ok((bytes, profile, orientation));
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
        target_size,
        fit,
    );
    if let Some(bad) = cache.get_bad(&bad_key) {
        return Ok(ImagePrep::CachedBad(bad));
    }

    let peek = peek_image_file(path, &mut file, open_stamp.size)?;
    if !peek.profile.fits_cap() && !peek.may_have_embedded_preview {
        return Err(too_large_error(path, &peek.profile));
    }

    // Size `CappedAlloc` from `estimate * 1.35 + 8 MiB` (floored at 16 MiB) so every honest image
    // peaks at <= ~70% of its cap, then reserve the exact watchdog resident limit
    // (`baseline + heap_cap + max(6 MiB, 10%)`) from the shared in-flight pool so `CappedAlloc` is
    // the exact limit, backstops never falsely kill honest images, and `sum(live limits) <= budget`.
    let output_overhead = target_output_overhead_bytes(target_size);
    let requested_heap_cap = if peek.uncertain_frame_bounds {
        heap_cap_from_estimate(MAX_IMAGE_DECODE_BYTES)
    } else if peek.profile.fits_cap() {
        heap_cap_from_estimate(
            peek.profile
                .estimated_peak_bytes
                .saturating_add(output_overhead),
        )
    } else {
        // Over-cap JPEG with EXIF/MPF marker: reserve 2x source + preview headroom and cap the
        // worker tightly so it can only decode an embedded preview, never the full over-cap frame.
        heap_cap_from_estimate(
            open_stamp
                .size
                .saturating_mul(2)
                .saturating_add(32 * 1024 * 1024)
                .saturating_add(output_overhead)
                .min(MAX_IMAGE_DECODE_BYTES),
        )
    };
    let budget = machine_in_flight_image_budget();
    let baseline = worker_baseline_rss_bytes();
    let (heap_cap_bytes, rss_limit_bytes, baseline_rss_bytes) =
        compute_worker_limits(requested_heap_cap, budget, baseline);
    let reserve_bytes = rss_limit_bytes;

    Ok(ImagePrep::Ready {
        key,
        bad_key,
        open_stamp,
        file,
        reserve_bytes,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
    })
}

pub(super) fn verify_post_read_stamp(
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

pub(super) fn changed_while_reading(path: &Path) -> ErrorBody {
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

/// Resizes `img` for a square `target_size x target_size` thumbnail using strictly bounded integer
/// paths whose scratch memory depends ONLY on `target_size` (never `source_width * target_size`).
///
/// - For `Cover`, ALWAYS center-crops to a square view `(crop_x, crop_y, side, side)` first with
///   zero copy so wide/tall strips (such as `20000x200` or `1x20000`) never allocate buffers
///   proportional to the long axis.
/// - For `>= 2x` downscales, uses the cache-friendly row-major integer box downscaler (8-bit) or
///   `imageops::thumbnail` on the crop view (16-bit / `LumaA8`).
/// - For `< 2x` downscales or upscales, uses a bounded integer bilinear resampler directly from
///   the source/crop slice into the `dst_w x dst_h` output buffer (`<= 2 MiB`).
fn resize_for_thumb(img: &DynamicImage, target_size: u32, fit: ThumbFit) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    match fit {
        ThumbFit::Contain => {
            if w <= target_size && h <= target_size {
                return img.clone();
            }
            let (dw, dh) = contain_dimensions(w, h, target_size, target_size);
            if w >= dw.saturating_mul(2) || h >= dh.saturating_mul(2) {
                fast_box_thumbnail(img, 0, 0, w, h, dw, dh)
                    .unwrap_or_else(|| thumbnail_crop_rect(img, 0, 0, w, h, dw, dh))
            } else {
                bilinear_resize_rect(img, 0, 0, w, h, dw, dh)
            }
        }
        ThumbFit::Cover => {
            let side = w.min(h);
            let x = (w - side) / 2;
            let y = (h - side) / 2;
            if side == target_size {
                crop_copy_rect(img, x, y, side, side)
            } else if side >= target_size.saturating_mul(2) {
                fast_box_thumbnail(img, x, y, side, side, target_size, target_size).unwrap_or_else(
                    || thumbnail_crop_rect(img, x, y, side, side, target_size, target_size),
                )
            } else {
                bilinear_resize_rect(img, x, y, side, side, target_size, target_size)
            }
        }
    }
}

fn contain_dimensions(width: u32, height: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let wratio = f64::from(max_w) / f64::from(width);
    let hratio = f64::from(max_h) / f64::from(height);
    let ratio = f64::min(wratio, hratio);
    let nw = ((f64::from(width) * ratio).round() as u32).max(1);
    let nh = ((f64::from(height) * ratio).round() as u32).max(1);
    (nw, nh)
}

fn fast_box_thumbnail(
    img: &DynamicImage,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Option<DynamicImage> {
    match img {
        DynamicImage::ImageRgb8(buf) => {
            let raw = box_downscale_8bit::<3>(
                buf.as_raw(),
                buf.width() as usize,
                crop_x as usize,
                crop_y as usize,
                crop_w as usize,
                crop_h as usize,
                dst_w as usize,
                dst_h as usize,
            );
            image::RgbImage::from_raw(dst_w, dst_h, raw).map(DynamicImage::ImageRgb8)
        }
        DynamicImage::ImageRgba8(buf) => {
            let raw = box_downscale_8bit::<4>(
                buf.as_raw(),
                buf.width() as usize,
                crop_x as usize,
                crop_y as usize,
                crop_w as usize,
                crop_h as usize,
                dst_w as usize,
                dst_h as usize,
            );
            image::RgbaImage::from_raw(dst_w, dst_h, raw).map(DynamicImage::ImageRgba8)
        }
        DynamicImage::ImageLuma8(buf) => {
            let raw = box_downscale_8bit::<1>(
                buf.as_raw(),
                buf.width() as usize,
                crop_x as usize,
                crop_y as usize,
                crop_w as usize,
                crop_h as usize,
                dst_w as usize,
                dst_h as usize,
            );
            image::GrayImage::from_raw(dst_w, dst_h, raw).map(DynamicImage::ImageLuma8)
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn box_downscale_8bit<const C: usize>(
    src: &[u8],
    src_w: usize,
    crop_x: usize,
    crop_y: usize,
    crop_w: usize,
    crop_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    let row_stride = src_w * C;
    let mut out = vec![0u8; dst_w * dst_h * C];

    let mut x_bounds = Vec::with_capacity(dst_w);
    let mut max_x_span = 1usize;
    for ox in 0..dst_w {
        let x0 = crop_x + ox * crop_w / dst_w;
        let x1 = (crop_x + (ox + 1) * crop_w / dst_w)
            .max(x0 + 1)
            .min(crop_x + crop_w);
        let span = x1 - x0;
        max_x_span = max_x_span.max(span);
        x_bounds.push((x0 * C, x1 * C, span));
    }

    let max_y_span = crop_h.div_ceil(dst_h).max(1);
    let use_u32 = max_x_span
        .checked_mul(max_y_span)
        .is_some_and(|px| px <= 16_000_000);

    if use_u32 {
        let mut col_sums = vec![[0u32; C]; dst_w];
        for oy in 0..dst_h {
            let y0 = crop_y + oy * crop_h / dst_h;
            let y1 = (crop_y + (oy + 1) * crop_h / dst_h)
                .max(y0 + 1)
                .min(crop_y + crop_h);
            let y_count = (y1 - y0) as u32;
            col_sums.fill([0u32; C]);

            for y in y0..y1 {
                let row = &src[y * row_stride..(y + 1) * row_stride];
                for (acc, &(bx0, bx1, _)) in col_sums.iter_mut().zip(x_bounds.iter()) {
                    let mut local = [0u32; C];
                    for px in row[bx0..bx1].as_chunks::<C>().0 {
                        for ch in 0..C {
                            local[ch] += u32::from(px[ch]);
                        }
                    }
                    for ch in 0..C {
                        acc[ch] += local[ch];
                    }
                }
            }

            let out_row = &mut out[oy * dst_w * C..(oy + 1) * dst_w * C];
            for (dst_px, (acc, &(_, _, x_span))) in out_row
                .as_chunks_mut::<C>()
                .0
                .iter_mut()
                .zip(col_sums.iter().zip(x_bounds.iter()))
            {
                let count = (x_span as u32) * y_count;
                let half = count / 2;
                for ch in 0..C {
                    dst_px[ch] = ((acc[ch] + half) / count) as u8;
                }
            }
        }
    } else {
        let mut col_sums = vec![[0u64; C]; dst_w];
        for oy in 0..dst_h {
            let y0 = crop_y + oy * crop_h / dst_h;
            let y1 = (crop_y + (oy + 1) * crop_h / dst_h)
                .max(y0 + 1)
                .min(crop_y + crop_h);
            let y_count = (y1 - y0) as u64;
            col_sums.fill([0u64; C]);

            for y in y0..y1 {
                let row = &src[y * row_stride..(y + 1) * row_stride];
                for (acc, &(bx0, bx1, _)) in col_sums.iter_mut().zip(x_bounds.iter()) {
                    let mut local = [0u64; C];
                    for px in row[bx0..bx1].as_chunks::<C>().0 {
                        for ch in 0..C {
                            local[ch] += u64::from(px[ch]);
                        }
                    }
                    for ch in 0..C {
                        acc[ch] += local[ch];
                    }
                }
            }

            let out_row = &mut out[oy * dst_w * C..(oy + 1) * dst_w * C];
            for (dst_px, (acc, &(_, _, x_span))) in out_row
                .as_chunks_mut::<C>()
                .0
                .iter_mut()
                .zip(col_sums.iter().zip(x_bounds.iter()))
            {
                let count = (x_span as u64) * y_count;
                let half = count / 2;
                for ch in 0..C {
                    dst_px[ch] = ((acc[ch] + half) / count) as u8;
                }
            }
        }
    }

    out
}

fn thumbnail_crop_rect(
    img: &DynamicImage,
    x: u32,
    y: u32,
    crop_w: u32,
    crop_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> DynamicImage {
    match img {
        DynamicImage::ImageLuma8(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageLuma8(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageLumaA8(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageLumaA8(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageRgb8(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageRgb8(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageRgba8(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageRgba8(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageLuma16(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageLuma16(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageLumaA16(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageLumaA16(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageRgb16(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageRgb16(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        DynamicImage::ImageRgba16(buf) => {
            let view = imageops::crop_imm(buf, x, y, crop_w, crop_h);
            DynamicImage::ImageRgba16(imageops::thumbnail(&*view, dst_w, dst_h))
        }
        other => {
            let rgba = other.to_rgba8();
            let view = imageops::crop_imm(&rgba, x, y, crop_w, crop_h);
            DynamicImage::ImageRgba8(imageops::thumbnail(&*view, dst_w, dst_h))
        }
    }
}

fn crop_copy_rect(img: &DynamicImage, x: u32, y: u32, crop_w: u32, crop_h: u32) -> DynamicImage {
    match img {
        DynamicImage::ImageLuma8(buf) => {
            DynamicImage::ImageLuma8(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageLumaA8(buf) => {
            DynamicImage::ImageLumaA8(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageRgb8(buf) => {
            DynamicImage::ImageRgb8(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageRgba8(buf) => {
            DynamicImage::ImageRgba8(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageLuma16(buf) => {
            DynamicImage::ImageLuma16(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageLumaA16(buf) => {
            DynamicImage::ImageLumaA16(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageRgb16(buf) => {
            DynamicImage::ImageRgb16(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        DynamicImage::ImageRgba16(buf) => {
            DynamicImage::ImageRgba16(imageops::crop_imm(buf, x, y, crop_w, crop_h).to_image())
        }
        other => {
            let rgba = other.to_rgba8();
            DynamicImage::ImageRgba8(imageops::crop_imm(&rgba, x, y, crop_w, crop_h).to_image())
        }
    }
}

/// Bounded integer bilinear resampler from `(crop_x, crop_y, crop_w, crop_h)` to `dst_w x dst_h`.
///
/// Allocates only the `dst_w x dst_h` output buffer (`<= 2 MiB` at 512px) plus a 1D x-lookup
/// table of length `dst_w` (`<= 12 KiB`) — zero float buffers and zero memory scaling with source
/// dimensions.
fn bilinear_resize_rect(
    img: &DynamicImage,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> DynamicImage {
    let (cx, cy, cw, ch, dw, dh) = (
        crop_x as usize,
        crop_y as usize,
        crop_w as usize,
        crop_h as usize,
        dst_w as usize,
        dst_h as usize,
    );
    match img {
        DynamicImage::ImageLuma8(buf) => {
            let raw = bilinear_u8::<1>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageLuma8(
                image::GrayImage::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageLumaA8(buf) => {
            let raw = bilinear_u8::<2>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageLumaA8(
                image::GrayAlphaImage::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageRgb8(buf) => {
            let raw = bilinear_u8::<3>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageRgb8(
                image::RgbImage::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageRgba8(buf) => {
            let raw = bilinear_u8::<4>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageLuma16(buf) => {
            let raw = bilinear_u16::<1>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageLuma16(
                image::ImageBuffer::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageLumaA16(buf) => {
            let raw = bilinear_u16::<2>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageLumaA16(
                image::ImageBuffer::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageRgb16(buf) => {
            let raw = bilinear_u16::<3>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageRgb16(
                image::ImageBuffer::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        DynamicImage::ImageRgba16(buf) => {
            let raw = bilinear_u16::<4>(buf.as_raw(), buf.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageRgba16(
                image::ImageBuffer::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
        other => {
            let rgba = other.to_rgba8();
            let raw =
                bilinear_u8::<4>(rgba.as_raw(), rgba.width() as usize, cx, cy, cw, ch, dw, dh);
            DynamicImage::ImageRgba8(
                image::RgbaImage::from_raw(dst_w, dst_h, raw).unwrap_or_default(),
            )
        }
    }
}

fn bilinear_axis_sample(
    dst_idx: usize,
    crop_origin: usize,
    crop_len: usize,
    dst_len: usize,
) -> (usize, usize, u32, u32) {
    if crop_len <= 1 || dst_len <= 1 {
        let i = crop_origin;
        return (i, i, 256, 0);
    }
    let pos = ((dst_idx as f64 + 0.5) * (crop_len as f64) / (dst_len as f64) - 0.5)
        .clamp(0.0, (crop_len - 1) as f64);
    let i0 = pos.floor() as usize;
    let i1 = (i0 + 1).min(crop_len - 1);
    let w1 = ((pos - i0 as f64) * 256.0).round() as u32;
    let w0 = 256 - w1;
    (crop_origin + i0, crop_origin + i1, w0, w1)
}

#[allow(clippy::too_many_arguments)]
fn bilinear_u8<const C: usize>(
    src: &[u8],
    src_w: usize,
    crop_x: usize,
    crop_y: usize,
    crop_w: usize,
    crop_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u8> {
    let row_stride = src_w * C;
    let mut out = vec![0u8; dst_w * dst_h * C];
    let x_lut: Vec<(usize, usize, u32, u32)> = (0..dst_w)
        .map(|ox| {
            let (x0, x1, wx0, wx1) = bilinear_axis_sample(ox, crop_x, crop_w, dst_w);
            (x0 * C, x1 * C, wx0, wx1)
        })
        .collect();

    for oy in 0..dst_h {
        let (y0, y1, wy0, wy1) = bilinear_axis_sample(oy, crop_y, crop_h, dst_h);
        let r0 = &src[y0 * row_stride..(y0 + 1) * row_stride];
        let r1 = &src[y1 * row_stride..(y1 + 1) * row_stride];
        let out_row = &mut out[oy * dst_w * C..(oy + 1) * dst_w * C];
        for (dst_px, &(bx0, bx1, wx0, wx1)) in out_row.as_chunks_mut::<C>().0.iter_mut().zip(&x_lut)
        {
            let w00 = wx0 * wy0;
            let w10 = wx1 * wy0;
            let w01 = wx0 * wy1;
            let w11 = wx1 * wy1;
            for ch in 0..C {
                let sum = u32::from(r0[bx0 + ch]) * w00
                    + u32::from(r0[bx1 + ch]) * w10
                    + u32::from(r1[bx0 + ch]) * w01
                    + u32::from(r1[bx1 + ch]) * w11;
                dst_px[ch] = ((sum + 32_768) >> 16) as u8;
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn bilinear_u16<const C: usize>(
    src: &[u16],
    src_w: usize,
    crop_x: usize,
    crop_y: usize,
    crop_w: usize,
    crop_h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Vec<u16> {
    let row_stride = src_w * C;
    let mut out = vec![0u16; dst_w * dst_h * C];
    let x_lut: Vec<(usize, usize, u64, u64)> = (0..dst_w)
        .map(|ox| {
            let (x0, x1, wx0, wx1) = bilinear_axis_sample(ox, crop_x, crop_w, dst_w);
            (x0 * C, x1 * C, u64::from(wx0), u64::from(wx1))
        })
        .collect();

    for oy in 0..dst_h {
        let (y0, y1, wy0, wy1) = bilinear_axis_sample(oy, crop_y, crop_h, dst_h);
        let (wy0, wy1) = (u64::from(wy0), u64::from(wy1));
        let r0 = &src[y0 * row_stride..(y0 + 1) * row_stride];
        let r1 = &src[y1 * row_stride..(y1 + 1) * row_stride];
        let out_row = &mut out[oy * dst_w * C..(oy + 1) * dst_w * C];
        for (dst_px, &(bx0, bx1, wx0, wx1)) in out_row.as_chunks_mut::<C>().0.iter_mut().zip(&x_lut)
        {
            let w00 = wx0 * wy0;
            let w10 = wx1 * wy0;
            let w01 = wx0 * wy1;
            let w11 = wx1 * wy1;
            for ch in 0..C {
                let sum = u64::from(r0[bx0 + ch]) * w00
                    + u64::from(r0[bx1 + ch]) * w10
                    + u64::from(r1[bx0 + ch]) * w01
                    + u64::from(r1[bx1 + ch]) * w11;
                dst_px[ch] = ((sum + 32_768) >> 16) as u16;
            }
        }
    }
    out
}

fn encode_thumbnail(img: &DynamicImage) -> Result<CachedThumb, ErrorBody> {
    let width = img.width();
    let height = img.height();

    if img.color().has_alpha() {
        let owned_rgba;
        let rgba = match img.as_rgba8() {
            Some(existing) => existing,
            None => {
                owned_rgba = img.to_rgba8();
                &owned_rgba
            }
        };
        if rgba.pixels().any(|pixel| pixel.0[3] < 255) {
            let mut buf = Vec::new();
            PngEncoder::new(&mut buf)
                .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
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

    let owned_rgb;
    let rgb = match img.as_rgb8() {
        Some(existing) => existing,
        None => {
            owned_rgb = img.to_rgb8();
            &owned_rgb
        }
    };
    let mut buf = Vec::new();
    JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY)
        .encode(rgb, width, height, image::ExtendedColorType::Rgb8)
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

        let results = run_batch(
            paths.clone(),
            256,
            MAX_REPLY_BASE64_BYTES,
            cancel,
            |path, _cancel| async move {
                if path.contains("b_err") {
                    return Err(ErrorBody::new(ErrorCode::Failed, "forced worker error."));
                }
                if path.contains("c_panic") {
                    #[allow(clippy::panic)]
                    {
                        panic!("forced worker panic");
                    }
                }
                Ok(ThumbOutcome::Ok(CachedThumb {
                    mime: "image/jpeg",
                    width: 96,
                    height: 96,
                    bytes: vec![0xFF, 0xD8],
                }))
            },
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["path"], json!("/a.jpg"));
        assert!(results[0].get("ok").is_some());
        assert_eq!(results[1]["path"], json!("/b_err.jpg"));
        assert_eq!(results[1]["error"]["code"], json!("failed"));
        assert_eq!(results[2]["path"], json!("/c_panic.jpg"));
        assert_eq!(results[2]["error"]["code"], json!("internal"));
        assert_eq!(results[3]["path"], json!("/d.jpg"));
        assert!(results[3].get("ok").is_some());
    }

    #[tokio::test]
    async fn run_batch_enforces_reply_budget_in_input_order() {
        let cancel = CancellationToken::new();
        let paths: Vec<String> = (0..6).map(|i| format!("/p{i}.png")).collect();
        // Each 300-byte payload encodes to 400 bytes of base64. With a 900-byte budget, paths 0
        // and 1 (800 bytes) fit, path 2 is an error (0 bytes) and still reports its own error,
        // path 3 would bring total to 1200 > 900 so paths 3, 4, and 5 all get `busy/reply_budget`.
        let results = run_batch(paths, 512, 900, cancel, |path, _cancel| async move {
            if path == "/p2.png" {
                return Ok(ThumbOutcome::Error(ErrorBody::new(
                    ErrorCode::NotFound,
                    "missing",
                )));
            }
            let len = if path == "/p5.png" { 3 } else { 300 };
            Ok(ThumbOutcome::Ok(CachedThumb {
                mime: "image/png",
                width: 96,
                height: 96,
                bytes: vec![0xAA; len],
            }))
        })
        .await
        .unwrap();

        assert_eq!(results.len(), 6);
        assert!(results[0].get("ok").is_some());
        assert!(results[1].get("ok").is_some());
        assert_eq!(results[2]["error"]["code"], json!("not_found"));
        for (i, item) in results.iter().enumerate().skip(3) {
            assert_eq!(item["path"], json!(format!("/p{i}.png")));
            assert_eq!(item["error"]["code"], json!("busy"), "{item}");
            assert_eq!(
                item["error"]["details"]["reason"],
                json!("reply_budget"),
                "{item}"
            );
        }
    }

    #[test]
    fn fast_box_downscale_matches_contain_and_cover_dimensions_and_colors() {
        // 800x600 RGB8 with distinct colored quadrants: red TL, green TR, blue BL, white BR.
        let mut rgb = image::RgbImage::new(800, 600);
        for (x, y, px) in rgb.enumerate_pixels_mut() {
            *px = match (x < 400, y < 300) {
                (true, true) => image::Rgb([240, 20, 20]),
                (false, true) => image::Rgb([20, 220, 40]),
                (true, false) => image::Rgb([30, 50, 230]),
                (false, false) => image::Rgb([230, 230, 230]),
            };
        }
        let dyn_rgb = DynamicImage::ImageRgb8(rgb);

        let contain = resize_for_thumb(&dyn_rgb, 200, ThumbFit::Contain);
        assert_eq!((contain.width(), contain.height()), (200, 150));
        let c_rgb = contain.to_rgb8();
        assert_eq!(c_rgb.get_pixel(20, 20).0, [240, 20, 20]);
        assert_eq!(c_rgb.get_pixel(180, 20).0, [20, 220, 40]);
        assert_eq!(c_rgb.get_pixel(20, 130).0, [30, 50, 230]);
        assert_eq!(c_rgb.get_pixel(180, 130).0, [230, 230, 230]);

        let cover = resize_for_thumb(&dyn_rgb, 150, ThumbFit::Cover);
        assert_eq!((cover.width(), cover.height()), (150, 150));
        let v_rgb = cover.to_rgb8();
        assert_eq!(v_rgb.get_pixel(20, 20).0, [240, 20, 20]);
        assert_eq!(v_rgb.get_pixel(130, 130).0, [230, 230, 230]);

        // RGBA8 transparency preservation
        let rgba = image::RgbaImage::from_pixel(400, 400, image::Rgba([10, 200, 100, 128]));
        let out_rgba = resize_for_thumb(&DynamicImage::ImageRgba8(rgba), 100, ThumbFit::Contain);
        assert_eq!((out_rgba.width(), out_rgba.height()), (100, 100));
        assert_eq!(out_rgba.to_rgba8().get_pixel(50, 50).0, [10, 200, 100, 128]);

        // Luma8
        let gray = image::GrayImage::from_pixel(600, 300, image::Luma([177]));
        let out_gray = resize_for_thumb(&DynamicImage::ImageLuma8(gray), 150, ThumbFit::Contain);
        assert_eq!((out_gray.width(), out_gray.height()), (150, 75));
        assert_eq!(out_gray.to_luma8().get_pixel(10, 10).0, [177]);
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
