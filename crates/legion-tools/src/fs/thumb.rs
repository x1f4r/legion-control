//! Node-side thumbnail generation (`fs.thumb`) for raster images and video posters.
//!
//! Supported raster images (JPEG, PNG, GIF first frame, WebP and BMP) are decoded in pure Rust
//! off the async runtime threads inside hard limits (64 MiB source, 100 megapixels, bounded
//! decoder memory, bounded process-wide concurrency) with EXIF orientation applied. Video posters
//! are extracted by invoking `ffmpeg` directly (no shell) with a hard time limit when `ffmpeg` is
//! available on `PATH`, and reported as `unsupported` otherwise.
//!
//! Results are stored in a 256 MiB LRU cache under the node's state directory (`thumb-cache/`).
//! Every path is checked against the caller's grant (requiring `fs.read` permission and path
//! confinement) before the file system or cache is touched.

use std::fs::{self, File, Metadata};
use std::io::{Cursor, Read as _};
use std::path::Path;
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
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::thumb_cache::{CachedThumb, ThumbCache, cache_key, temp_dir_in_cache};
use super::{cancelled_error, check_fs_path, check_regular_file, map_io_error, platform};

const MIN_PATHS: usize = 1;
const MAX_PATHS: usize = 32;
const MIN_THUMB_SIZE: u32 = 96;
const DEFAULT_THUMB_SIZE: u32 = 256;
const MAX_THUMB_SIZE: u32 = 512;
const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_PIXELS: u64 = 100_000_000;
const MAX_IMAGE_DIMENSION: u32 = 65_535;
const MAX_DECODE_ALLOC_BYTES: u64 = 450 * 1024 * 1024;
const JPEG_QUALITY: u8 = 82;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);
const FFMPEG_KILL_GRACE: Duration = Duration::from_secs(1);
const FFMPEG_CAPTURE_LIMIT: usize = 32 * 1024;

/// Process-wide semaphore bounding simultaneous heavy image/video decodes across all sessions.
static DECODE_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn decode_semaphore() -> Arc<Semaphore> {
    Arc::clone(DECODE_SEMAPHORE.get_or_init(|| {
        let permits = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 4))
            .unwrap_or(2);
        Arc::new(Semaphore::new(permits))
    }))
}

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.thumb",
        Effect::Read,
        "Generate thumbnails for 1 to 32 image or video paths on this node, returning per-path base64 JPEG (or PNG when transparent) thumbnails or per-path errors, cached in an LRU store in the state directory.",
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

/// Metadata snapshot taken before reading a file so changes between `stat` and `read` are caught.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    size: u64,
    mtime_nanos: Option<i128>,
    identity: Option<(u64, u64)>,
}

impl FileStamp {
    fn from_meta(meta: &Metadata) -> FileStamp {
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| jiff::Timestamp::try_from(t).ok())
            .map(jiff::Timestamp::as_nanosecond);
        FileStamp {
            size: meta.len(),
            mtime_nanos,
            identity: platform::file_identity(meta),
        }
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
    let cancel = ctx.cancel_token().clone();
    let size = args.size;
    let fit = args.fit;
    let total = args.paths.len();

    let mut set = JoinSet::new();
    for (idx, raw_path) in args.paths.into_iter().enumerate() {
        let grant = grant.clone();
        let cache = cache.clone();
        let cache_dir = cache_dir.clone();
        let cancel = cancel.clone();
        set.spawn(async move {
            let outcome = thumb_one(&grant, &cache, &cache_dir, raw_path, size, fit, cancel).await;
            (idx, outcome)
        });
    }

    let mut slots: Vec<Option<ThumbItem>> = (0..total).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((idx, Ok(item))) => {
                if let Some(slot) = slots.get_mut(idx) {
                    *slot = Some(item);
                }
            }
            Ok((_, Err(err))) => {
                if err.code == ErrorCode::Cancelled {
                    set.abort_all();
                    while set.join_next().await.is_some() {}
                    return Err(err);
                }
            }
            Err(err) => {
                return Err(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb worker stopped unexpectedly: {err}."),
                ));
            }
        }
    }

    if ctx.is_cancelled() {
        return Err(cancelled_error("fs.thumb"));
    }

    let results: Vec<ThumbItem> = slots.into_iter().flatten().collect();
    Ok(ThumbResult { results })
}

async fn thumb_one(
    grant: &legion_core::Grant,
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

    // Stat without blocking the async runtime, refusing directories and special files before open.
    let resolved_for_stat = resolved.clone();
    let stamp = match tokio::task::spawn_blocking(move || {
        let meta = check_regular_file(
            &resolved_for_stat,
            "thumbnail",
            "Pass a regular file path to fs.thumb; directories cannot be thumbnailed.",
        )?;
        Ok::<FileStamp, ToolError>(FileStamp::from_meta(&meta))
    })
    .await
    .map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!("fs.thumb stat stopped unexpectedly: {err}."),
        )
    })? {
        Ok(stamp) => stamp,
        Err(err) => return Ok(ThumbItem::from_error(raw_path, err)),
    };

    let media = classify_path(&resolved);
    match media.category {
        Category::Image => {
            if stamp.size == 0 {
                return Ok(ThumbItem::from_error(
                    raw_path,
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "{} is empty and cannot be decoded as an image.",
                            resolved.display()
                        ),
                    ),
                ));
            }
            if stamp.size > MAX_SOURCE_BYTES {
                return Ok(ThumbItem::from_error(
                    raw_path,
                    ErrorBody::new(
                        ErrorCode::Failed,
                        format!(
                            "{} is {} bytes, exceeding the 64 MiB thumbnail source limit.",
                            resolved.display(),
                            stamp.size
                        ),
                    )
                    .with_hint("Files larger than 64 MiB are not thumbnailed; open or download the file directly."),
                ));
            }
        }
        Category::Video => {}
        _ => {
            return Ok(ThumbItem::from_error(
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
            ));
        }
    }

    let key = cache_key(&resolved, stamp.size, stamp.mtime_nanos, size, fit);
    let cache_get = cache.clone();
    let key_get = key.clone();
    let cached = tokio::task::spawn_blocking(move || cache_get.get(&key_get))
        .await
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("fs.thumb cache lookup stopped unexpectedly: {err}."),
            )
        })?;
    if let Some(hit) = cached {
        return Ok(ThumbItem::from_cached(raw_path, hit));
    }

    // Acquire a bounded decode slot while watching cancellation.
    let permit = tokio::select! {
        acquired = decode_semaphore().acquire_owned() => match acquired {
            Ok(permit) => permit,
            Err(_) => {
                return Err(ErrorBody::new(
                    ErrorCode::Internal,
                    "fs.thumb decode limiter closed unexpectedly.",
                ));
            }
        },
        () = cancel.cancelled() => return Err(cancelled_error("fs.thumb")),
    };

    let produced = match media.category {
        Category::Image => {
            let resolved_decode = resolved.clone();
            let cancel_decode = cancel.clone();
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if cancel_decode.is_cancelled() {
                    return Err(cancelled_error("fs.thumb"));
                }
                let bytes = read_source_checked(&resolved_decode, stamp, || {})?;
                if cancel_decode.is_cancelled() {
                    return Err(cancelled_error("fs.thumb"));
                }
                decode_and_render_image(&resolved_decode, &bytes, size, fit)
            })
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.thumb image decode stopped unexpectedly: {err}."),
                )
            })?
        }
        Category::Video => {
            let outcome =
                extract_video_poster(&resolved, stamp, cache_dir, size, fit, cancel.clone()).await;
            drop(permit);
            outcome
        }
        _ => unreachable!(),
    };

    match produced {
        Ok(thumb) => {
            let cache_put = cache.clone();
            let thumb_put = thumb.clone();
            let key_put = key;
            let _ = tokio::task::spawn_blocking(move || cache_put.put(&key_put, &thumb_put)).await;
            Ok(ThumbItem::from_cached(raw_path, thumb))
        }
        Err(err) if err.code == ErrorCode::Cancelled => Err(err),
        Err(err) => Ok(ThumbItem::from_error(raw_path, err)),
    }
}

/// Reads `path` up to [`MAX_SOURCE_BYTES`] while verifying that its size, modification time and
/// file identity did not change between `stat` and `read`.
fn read_source_checked<F>(
    path: &Path,
    expected: FileStamp,
    before_open: F,
) -> Result<Vec<u8>, ErrorBody>
where
    F: FnOnce(),
{
    before_open();
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
    if FileStamp::from_meta(&open_meta) != expected {
        return Err(changed_while_reading(path));
    }

    let capacity = usize::try_from(expected.size.min(MAX_SOURCE_BYTES)).unwrap_or(0);
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
    if bytes.len() as u64 != expected.size {
        return Err(changed_while_reading(path));
    }
    let after_path_meta = fs::metadata(path).map_err(|err| map_io_error("stat", path, err))?;
    if FileStamp::from_meta(&after_path_meta) != expected {
        return Err(changed_while_reading(path));
    }
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

fn is_supported_image_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "gif" | "webp" | "bmp" | "dib"
    )
}

/// Decodes a supported raster image inside hard dimension and allocation limits, applies EXIF
/// orientation, resizes according to `fit`, and encodes JPEG (or PNG when transparent).
fn decode_and_render_image(
    path: &Path,
    bytes: &[u8],
    target_size: u32,
    fit: ThumbFit,
) -> Result<CachedThumb, ErrorBody> {
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

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);

    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);

    let mut decoder = reader.into_decoder().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!("Cannot decode {}: {err}.", path.display()),
        )
        .with_hint("The image may be corrupted or exceed the thumbnail size and memory limits.")
    })?;

    let (w, h) = decoder.dimensions();
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
                "{} is {w}x{h} ({:.1} megapixels), exceeding the 100 megapixel thumbnail limit.",
                path.display(),
                pixels as f64 / 1_000_000.0
            ),
        )
        .with_hint(
            "Images larger than 100 megapixels are not thumbnailed to protect node memory.",
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
                .with_hint("The video file may be corrupted or use a codec this node's ffmpeg cannot decode."));
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

    // Confirm the source video did not change while ffmpeg was running.
    let resolved_check = resolved.to_path_buf();
    let frame_for_decode = frame_path.clone();
    tokio::task::spawn_blocking(move || {
        let after_meta = fs::metadata(&resolved_check)
            .map_err(|err| map_io_error("stat", &resolved_check, err))?;
        if FileStamp::from_meta(&after_meta) != expected_stamp {
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
        decode_and_render_image(&frame_for_decode, &frame_bytes, target_size, fit)
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

    let mut args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-nostdin".to_owned(),
        "-y".to_owned(),
    ];
    if let Some(ss) = seek {
        args.push("-ss".to_owned());
        args.push(ss.to_owned());
    }
    args.extend([
        "-i".to_owned(),
        input_arg.to_owned(),
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

    /// Builds a valid minimal JPEG with an EXIF APP1 orientation tag injected right after SOI.
    fn jpeg_with_exif_orientation(width: u32, height: u32, orientation: u16) -> Vec<u8> {
        let mut img = ImageBuffer::<Rgb<u8>, Vec<u8>>::new(width, height);
        // Put a distinct bright red marker at top-left (0..4, 0..4) and blue elsewhere so rotation
        // and flips can be verified by pixel location.
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

        // Little-endian TIFF header + 1 IFD0 entry for Orientation (0x0112, SHORT, count=1).
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"Exif\0\0");
        tiff.extend_from_slice(b"II"); // little-endian
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // offset to IFD0
        tiff.extend_from_slice(&1u16.to_le_bytes()); // 1 entry
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation tag
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0

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
    fn exif_orientation_rotates_and_flips_jpeg_correctly() {
        // 200x100 landscape JPEG tagged Orientation=6 (Rotate 90 CW) must become 100x200 portrait,
        // moving the top-left red corner to the top-right corner!
        let bytes = jpeg_with_exif_orientation(200, 100, 6);
        let thumb =
            decode_and_render_image(Path::new("/photo.jpg"), &bytes, 256, ThumbFit::Contain)
                .unwrap();
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

        // Orientation=3 (Rotate 180) keeps 200x100 and moves red from top-left to bottom-right.
        let bytes_180 = jpeg_with_exif_orientation(200, 100, 3);
        let thumb_180 = decode_and_render_image(
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
            decode_and_render_image(Path::new("/alpha.png"), &png_alpha, 256, ThumbFit::Contain)
                .unwrap();
        assert_eq!(res_alpha.mime, "image/png");

        let opaque =
            ImageBuffer::<Rgba<u8>, Vec<u8>>::from_pixel(120, 120, Rgba([10, 20, 30, 255]));
        let mut png_opaque = Vec::new();
        PngEncoder::new(&mut png_opaque)
            .write_image(&opaque, 120, 120, image::ExtendedColorType::Rgba8)
            .unwrap();
        let res_opaque = decode_and_render_image(
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
        // Craft a tiny valid PNG header declaring 20000 x 20000 pixels (400 MP > 100 MP).
        let mut bomb = Vec::new();
        bomb.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]); // PNG signature
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(b"IHDR");
        ihdr.extend_from_slice(&20_000u32.to_be_bytes());
        ihdr.extend_from_slice(&20_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
        let crc = crc32_ieee(&ihdr);
        bomb.extend_from_slice(&13u32.to_be_bytes());
        bomb.extend_from_slice(&ihdr);
        bomb.extend_from_slice(&crc.to_be_bytes());
        // Minimal dummy IDAT + IEND chunks so the PNG header parser succeeds and reaches our
        // 100-megapixel / allocation limit check rather than failing early on missing IDAT.
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

        let err = decode_and_render_image(Path::new("/bomb.png"), &bomb, 256, ThumbFit::Contain)
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Failed);
        assert!(
            err.message.contains("100 megapixel") || err.message.contains("limit"),
            "{err}"
        );
    }

    #[test]
    fn read_source_checked_detects_file_changing_between_stat_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raced.png");
        fs::write(&path, b"initial-bytes").unwrap();
        let stamp = FileStamp::from_meta(&fs::metadata(&path).unwrap());

        let err = read_source_checked(&path, stamp, || {
            fs::write(&path, b"modified-longer-bytes").unwrap();
        })
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
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
