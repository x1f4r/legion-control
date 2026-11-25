//! Node-side thumbnail generation (`fs.thumb`) for raster images and video posters.
//!
//! # Security & resource limits
//!
//! - **Authorization parity**: requires `fs.read` permission on the role and per-path `check_fs_path`
//!   confinement before touching the file system or the thumbnail cache.
//! - **Open-once image reads**: images are stat'd to refuse special files (FIFOs/sockets/devices),
//!   opened once, checked via the open handle's size, mtime, OS file identity (`dev`/`ino` or
//!   Windows volume+file index) and metadata change time (`ctime` / NTFS `ChangeTime`), and read
//!   directly from that handle so rename swaps cannot substitute another file.
//! - **Bounded real decoder memory**: `zune-jpeg` buffers full `i16` DCT coefficients for
//!   progressive JPEGs (~6 bytes/pixel) plus RGB8 output (~3 bytes/pixel) plus resize scratch
//!   (~8.6 bytes/pixel measured; a 5000x5000 25 MP 4:4:4 progressive JPEG peaks at ~215 MiB as
//!   verified by the counting-allocator test in `tests/thumb_mem.rs`). Therefore:
//!   - Source file size is capped at **64 MiB**.
//!   - Source image dimensions are capped at **25 megapixels** (`MAX_SOURCE_PIXELS = 25_000_000`,
//!     verified from the header before allocating pixel buffers) and `Limits::max_alloc` at
//!     **256 MiB**.
//!   - Total in-flight image decodes across all sessions are capped at **50 megapixels** and at
//!     most **8** concurrent decodes, scheduled round-robin by principal so peak concurrent decode
//!     memory stays below ~450 MiB.
//!   - Every image decode has a **10 s** wall-clock timeout and bounded limiter wait (**10 s**,
//!     returning retryable `busy` on wait timeout).
//! - **Hardened `ffmpeg` video posters**:
//!   - Videos use a separate fair pool (**2** concurrent `ffmpeg` subprocesses, round-robin by
//!     principal, **15 s** wait timeout) so video floods never stall image thumbnails.
//!   - `ffmpeg` is invoked directly with no shell, `-threads 1`, `-nostdin`, `-protocol_whitelist
//!     file`, `-format_whitelist mov,mp4,m4a,3gp,3g2,mj2,matroska,webm,avi,asf,flv,mpeg,mpegts,ogg,rm,mpegvideo`,
//!     and `-map 0:v:0 -an -sn -dn` so manifests, playlists (`dash`, `hls`, `concat`), network
//!     protocols, and attachment streams are strictly blocked.
//!   - Passing an already-opened FD to `ffmpeg` is not used because `legion-os::run` feeds `stdin`
//!     over a non-seekable pipe (breaking `-ss` seeking and MP4 files with trailing `moov` atoms),
//!     sets `CLOEXEC` without `pre_exec`, and the workspace forbids `unsafe_code`; instead we
//!     combine the protocol/format whitelists with a post-extraction `FileStamp` check (`FileId`,
//!     `ctime`, size, mtime) so any swap during extraction fails with `conflict`.

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{Cursor, Read as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use legion_core::media::{Category, classify_path};
use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Registry, ToolError};
use legion_os::OsError;
use legion_os::file_id::{self, FileId};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::thumb_cache::{CachedThumb, ThumbCache, cache_key, temp_dir_in_cache};
use super::thumb_limit::{AcquireError, FairPool};
use super::{cancelled_error, check_fs_path, check_regular_file, map_io_error, platform};

const MIN_PATHS: usize = 1;
const MAX_PATHS: usize = 32;
const MIN_THUMB_SIZE: u32 = 96;
const DEFAULT_THUMB_SIZE: u32 = 256;
const MAX_THUMB_SIZE: u32 = 512;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
/// Single-image pixel ceiling (25 MP). Measured with a counting global allocator in
/// `tests/thumb_mem.rs`: a 5000x5000 (25 MP) 4:4:4 progressive JPEG peaks at ~215 MiB in
/// `zune-jpeg` (~8.6 bytes/pixel for i16 DCT planes + RGB8 output + resize scratch), keeping
/// single decodes well below 256 MiB and 50 MP of concurrent in-flight work below ~450 MiB.
const MAX_SOURCE_PIXELS: u64 = 25_000_000;
const MAX_IMAGE_DIMENSION: u32 = 65_535;
const MAX_DECODE_ALLOC_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IN_FLIGHT_IMAGE_MP: usize = 50;
const MAX_IN_FLIGHT_IMAGES: usize = 8;
const MAX_IN_FLIGHT_VIDEOS: usize = 2;
const IMAGE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const IMAGE_DECODE_TIMEOUT: Duration = Duration::from_secs(10);
const VIDEO_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const JPEG_QUALITY: u8 = 82;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);
const FFMPEG_KILL_GRACE: Duration = Duration::from_secs(1);
const FFMPEG_CAPTURE_LIMIT: usize = 32 * 1024;

/// Strictly allow only local file I/O in `ffmpeg` — blocks `http`, `https`, `concat`, `crypto`,
/// `data`, `pipe`, `subfile`, and all network/virtual protocols.
const FFMPEG_PROTOCOL_WHITELIST: &str = "file";
/// Strictly allow only container video demuxers in `ffmpeg` — blocks `dash`, `hls`, `concat`,
/// `m3u8`, and any playlist or external-reference manifest demuxer.
const FFMPEG_FORMAT_WHITELIST: &str =
    "mov,mp4,m4a,3gp,3g2,mj2,matroska,webm,avi,asf,flv,mpeg,mpegts,ogg,rm,mpegvideo";

static IMAGE_POOL: OnceLock<Arc<FairPool>> = OnceLock::new();
static VIDEO_POOL: OnceLock<Arc<FairPool>> = OnceLock::new();

fn image_pool() -> Arc<FairPool> {
    Arc::clone(
        IMAGE_POOL.get_or_init(|| FairPool::new(MAX_IN_FLIGHT_IMAGE_MP, MAX_IN_FLIGHT_IMAGES)),
    )
}

fn video_pool() -> Arc<FairPool> {
    Arc::clone(VIDEO_POOL.get_or_init(|| FairPool::new(MAX_IN_FLIGHT_VIDEOS, MAX_IN_FLIGHT_VIDEOS)))
}

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.thumb",
        Effect::Read,
        "Generate thumbnails for 1 to 32 image or video paths on this node, returning per-path base64 JPEG (or PNG when transparent) thumbnails or per-path errors in input order, cached in a 256 MiB LRU store in the state directory.",
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
    // Enforce the contract: thumbnailing a file is allowed exactly when `fs.read` of it is.
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
    let cache = ThumbCache::new(cache_dir.clone());
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

    // Check authorization BEFORE touching the file system or the thumbnail cache.
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
    match media.category {
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
            // Confirm existence / regular file type first only after category check, or report
            // unsupported directly for non-image/non-video regular files. Still check regular file
            // so missing paths report `not_found` and directories/FIFOs report `invalid_args`.
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
                        media.category
                    ),
                )
                .with_hint("fs.thumb supports raster images (JPEG, PNG, GIF, WebP, BMP) and videos (when ffmpeg is on PATH)."),
            ))
        }
    }
}

/// Prepared image file opened once, verified against TOCTOU swaps, and header-probed before
/// requesting megapixel capacity from the fair image pool.
enum ImagePrep {
    CacheHit(CachedThumb),
    Decode {
        key: String,
        bytes: Vec<u8>,
        format: ImageFormat,
        cost_mp: usize,
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
            format!("fs.thumb image read stopped unexpectedly: {err}."),
        )
    })?;

    let (key, bytes, format, cost_mp) = match prep {
        Ok(ImagePrep::CacheHit(hit)) => return Ok(ThumbItem::from_cached(raw_path, hit)),
        Ok(ImagePrep::Decode {
            key,
            bytes,
            format,
            cost_mp,
        }) => (key, bytes, format, cost_mp),
        Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
    };

    // Acquire pixel budget from the fair image pool (bounded wait, cancellable).
    let permit = match image_pool()
        .acquire(principal, cost_mp, IMAGE_WAIT_TIMEOUT, &cancel)
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

    // Decode off async threads with a hard wall-clock time limit. The permit lives inside the
    // blocking closure so its megapixel budget stays reserved until decoding finishes or unwinds.
    let abort_flag = Arc::new(AtomicBool::new(false));
    let abort_worker = Arc::clone(&abort_flag);
    let resolved_decode = resolved.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_and_render_image_checked(&resolved_decode, &bytes, format, size, fit, &abort_worker)
    });

    let sleep = tokio::time::sleep(IMAGE_DECODE_TIMEOUT);
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
                format!("Decoding a thumbnail for {} exceeded the 10-second time limit.", resolved.display()),
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
        let meta = check_regular_file(
            &resolved_stat,
            "thumbnail",
            "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
        )?;
        let stamp = FileStamp::from_path(&resolved_stat, &meta);
        let key = cache_key(
            &resolved_stat,
            stamp.size,
            stamp.mtime_nanos,
            stamp.file_id,
            size,
            fit,
        );
        let hit = cache_get.get(&key);
        Ok::<(FileStamp, String, Option<CachedThumb>), ToolError>((stamp, key, hit))
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb video stat stopped unexpectedly: {err}."),
        )
    })?;

    let (stamp, key, hit) = match pre {
        Ok(v) => v,
        Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
    };
    if let Some(hit) = hit {
        return Ok(ThumbItem::from_cached(raw_path, hit));
    }

    let permit = match video_pool()
        .acquire(principal, 1, VIDEO_WAIT_TIMEOUT, &cancel)
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

    let outcome =
        extract_video_poster(&resolved, stamp, cache_dir, size, fit, cancel.clone()).await;
    drop(permit);

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

/// Opens an image file once, checks its metadata and identity on the open handle, checks the
/// lock-free cache, reads bytes from that same open handle on miss, and probes the image format
/// and header dimensions (without allocating pixel buffers) before acquiring decode capacity.
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

    let file = File::open(path).map_err(|err| map_io_error("read", path, err))?;
    let open_meta = file
        .metadata()
        .map_err(|err| map_io_error("read", path, err))?;
    if !open_meta.is_file() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("{} is not a regular file.", path.display()),
        )
        .with_hint("Pass a regular file path to fs.thumb."));
    }
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

    // Read directly from the already-open, already-verified file handle.
    let capacity = usize::try_from(open_stamp.size.min(MAX_SOURCE_BYTES)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    (&file)
        .take(MAX_SOURCE_BYTES + 1)
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
    let after_handle_meta = file
        .metadata()
        .map_err(|err| map_io_error("read", path, err))?;
    if FileStamp::from_open_file(&file, &after_handle_meta) != open_stamp {
        return Err(changed_while_reading(path));
    }
    let after_path_meta = fs::metadata(path).map_err(|err| map_io_error("stat", path, err))?;
    if FileStamp::from_path(path, &after_path_meta) != open_stamp {
        return Err(changed_while_reading(path));
    }

    let (format, cost_mp) = probe_image_header(path, &bytes)?;
    Ok(ImagePrep::Decode {
        key,
        bytes,
        format,
        cost_mp,
    })
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

fn is_supported_image_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "gif" | "webp" | "bmp" | "dib"
    )
}

fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
    limits
}

/// Inspects magic bytes and image header dimensions without decoding pixels, rejecting unsupported
/// formats and images over [`MAX_SOURCE_PIXELS`] (25 MP) before acquiring megapixel budget.
fn probe_image_header(path: &Path, bytes: &[u8]) -> Result<(ImageFormat, usize), ErrorBody> {
    if bytes.is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is empty and cannot be decoded as an image.",
                path.display()
            ),
        ));
    }

    let format = match image::guess_format(bytes) {
        Ok(
            fmt @ (ImageFormat::Jpeg
            | ImageFormat::Png
            | ImageFormat::Gif
            | ImageFormat::WebP
            | ImageFormat::Bmp),
        ) => fmt,
        _ => {
            if is_supported_image_extension(path) {
                return Err(ErrorBody::new(
                    ErrorCode::Failed,
                    format!(
                        "{} is not a valid JPEG, PNG, GIF, WebP or BMP image.",
                        path.display()
                    ),
                )
                .with_hint("The file may be truncated, corrupted, or misnamed."));
            }
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "Thumbnails for {} are not supported on this node.",
                    path.display()
                ),
            )
            .with_hint("Supported raster formats are JPEG, PNG, GIF, WebP and BMP."));
        }
    };

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(image_limits());
    let (w, h) = reader.into_dimensions().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot read image header of {}: {err}.", path.display()),
        )
        .with_hint("The image may be corrupted or exceed the thumbnail size and memory limits.")
    })?;

    if w == 0 || h == 0 {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} declares zero image dimensions ({w}x{h}).",
                path.display()
            ),
        ));
    }
    let pixels = u64::from(w).saturating_mul(u64::from(h));
    if pixels > MAX_SOURCE_PIXELS {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is {w}x{h} ({:.1} megapixels), exceeding the 25 megapixel thumbnail limit.",
                path.display(),
                pixels as f64 / 1_000_000.0
            ),
        )
        .with_hint(
            "Images larger than 25 megapixels are not thumbnailed to protect node memory.",
        ));
    }

    let cost_mp = usize::try_from(pixels.div_ceil(1_000_000))
        .unwrap_or(MAX_IN_FLIGHT_IMAGE_MP)
        .clamp(1, MAX_IN_FLIGHT_IMAGE_MP);
    Ok((format, cost_mp))
}

/// Decodes a header-verified raster image inside hard limits, applies EXIF orientation, resizes
/// according to `fit`, and encodes JPEG (or PNG when transparent).
fn decode_and_render_image_checked(
    path: &Path,
    bytes: &[u8],
    format: ImageFormat,
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

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(image_limits());

    let mut decoder = reader.into_decoder().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot decode {}: {err}.", path.display()),
        )
        .with_hint("The image may be corrupted or exceed the thumbnail size and memory limits.")
    })?;

    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w).saturating_mul(u64::from(h));
    if w == 0 || h == 0 || pixels > MAX_SOURCE_PIXELS {
        return Err(ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "{} is {w}x{h}, exceeding the 25 megapixel thumbnail limit.",
                path.display()
            ),
        )
        .with_hint(
            "Images larger than 25 megapixels are not thumbnailed to protect node memory.",
        ));
    }

    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|err| {
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

    img.apply_orientation(orientation);

    let resized = match fit {
        ThumbFit::Contain => {
            if img.width() > target_size || img.height() > target_size {
                img.resize(target_size, target_size, FilterType::Triangle)
            } else {
                img
            }
        }
        ThumbFit::Cover => img.resize_to_fill(target_size, target_size, FilterType::Triangle),
    };

    if abort.load(Ordering::Acquire) {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!("Decoding {} was aborted.", path.display()),
        ));
    }

    encode_thumbnail(&resized)
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

/// Builds the locked-down `ffmpeg` argument list for single-frame video poster extraction.
fn build_ffmpeg_args(input_arg: &str, frame_arg: &str, seek: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
        "-threads".to_owned(),
        "1".to_owned(),
        "-protocol_whitelist".to_owned(),
        FFMPEG_PROTOCOL_WHITELIST.to_owned(),
        "-format_whitelist".to_owned(),
        FFMPEG_FORMAT_WHITELIST.to_owned(),
    ];
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

async fn extract_video_poster(
    resolved: &Path,
    expected_stamp: FileStamp,
    cache_dir: &Path,
    target_size: u32,
    fit: ThumbFit,
    cancel: CancellationToken,
) -> Result<CachedThumb, ErrorBody> {
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

    // Try seeking 1 second in first for a representative frame; if the clip is shorter than 1 s
    // and produces no frame, fall back once to the start of the stream.
    let seek_outcome =
        run_ffmpeg_extract(&input_arg, &frame_arg, Some("1"), deadline, cancel.clone()).await?;

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
                .with_hint("The file may be corrupted, a playlist/manifest (blocked for safety), or use a codec this node's ffmpeg cannot decode."));
            }
        }
        let fallback_outcome =
            run_ffmpeg_extract(&input_arg, &frame_arg, None, deadline, cancel.clone()).await?;
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

    // Confirm the source video did not change (size, mtime, inode/file-id, ctime) while ffmpeg ran.
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
        let (format, _) = probe_image_header(&frame_for_decode, &frame_bytes)?;
        let no_abort = AtomicBool::new(false);
        decode_and_render_image_checked(
            &frame_for_decode,
            &frame_bytes,
            format,
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

async fn run_ffmpeg_extract(
    input_arg: &str,
    frame_arg: &str,
    seek: Option<&str>,
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

    let args = build_ffmpeg_args(input_arg, frame_arg, seek);
    let options = RunOptions::new(CommandSpec::Argv {
        program: "ffmpeg".into(),
        args: args.into_iter().map(Into::into).collect(),
    })
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
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            "Extracting a video poster frame with ffmpeg exceeded the time limit.",
        )
        .with_hint(
            "The video may be corrupted or too slow to decode within the 15-second limit.",
        ));
    }
    Ok(output)
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb, Rgba};

    fn decode_helper(
        path: &Path,
        bytes: &[u8],
        size: u32,
        fit: ThumbFit,
    ) -> Result<CachedThumb, ErrorBody> {
        let (format, _) = probe_image_header(path, bytes)?;
        decode_and_render_image_checked(path, bytes, format, size, fit, &AtomicBool::new(false))
    }

    /// Builds a valid minimal JPEG with an EXIF APP1 orientation tag injected right after SOI.
    fn jpeg_with_exif_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let color = if x < width / 4 && y < height / 4 {
                    Rgb([240, 20, 20])
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

    #[test]
    fn ffmpeg_args_enforce_protocol_format_whitelists_single_thread_and_video_only_map() {
        let args = build_ffmpeg_args("file:/clip.mp4", "/tmp/out.png", Some("1"));
        let idx = |flag: &str| args.iter().position(|a| a == flag);
        let input_pos = idx("-i").expect("-i missing");
        let proto_pos = idx("-protocol_whitelist").expect("-protocol_whitelist missing");
        let fmt_pos = idx("-format_whitelist").expect("-format_whitelist missing");
        let threads_pos = idx("-threads").expect("-threads missing");
        assert!(proto_pos < input_pos, "-protocol_whitelist must precede -i");
        assert!(fmt_pos < input_pos, "-format_whitelist must precede -i");
        assert!(threads_pos < input_pos, "-threads must precede -i");
        assert_eq!(args[proto_pos + 1], "file");
        assert_eq!(args[threads_pos + 1], "1");
        assert!(args.contains(&"-nostdin".to_owned()));
        assert!(args.contains(&"-an".to_owned()));
        assert!(args.contains(&"-sn".to_owned()));
        assert!(args.contains(&"-dn".to_owned()));
        let map_pos = idx("-map").expect("-map missing");
        assert_eq!(args[map_pos + 1], "0:v:0");
        // Dangerous manifest demuxers must never be whitelisted.
        let formats: Vec<&str> = args[fmt_pos + 1].split(',').collect();
        for forbidden in ["dash", "hls", "concat", "m3u8", "http", "https"] {
            assert!(
                !formats.contains(&forbidden),
                "forbidden format {forbidden}"
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
        // Tiny valid PNG header declaring 6000 x 6000 pixels (36 MP > 25 MP limit).
        let mut bomb = Vec::new();
        bomb.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&6_000u32.to_be_bytes());
        ihdr.extend_from_slice(&6_000u32.to_be_bytes());
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
        assert!(
            err.message.contains("25 megapixel") || err.message.contains("limit"),
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
