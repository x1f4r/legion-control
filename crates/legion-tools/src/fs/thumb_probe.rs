//! Zero-allocation image header inspection, per-format peak decode memory estimation, and
//! bounded EXIF IFD1 / MPF APP2 embedded thumbnail extraction for `fs.thumb`.
//!
//! All offsets and lengths parsed from untrusted EXIF and MPF (CIPA DC-007) segments are checked
//! with overflow-safe arithmetic against the containing segment and file bounds before slicing,
//! and every extracted embedded image must pass the exact same header probe and decode limits as
//! a top-level file.

use std::fs::File;
use std::io::{Cursor, Read as _, Seek as _, SeekFrom};
use std::path::Path;

use image::metadata::Orientation;
use image::{ImageFormat, ImageReader, Limits};
use legion_core::proto::{ErrorBody, ErrorCode};
use serde_json::json;

/// Hard per-image decode memory cap (256 MiB).
pub(crate) const MAX_IMAGE_DECODE_BYTES: u64 = 256 * 1024 * 1024;
/// Hard single-axis dimension cap (matches JPEG's 16-bit maximum).
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 65_535;
/// Hard cap on any single embedded EXIF IFD1 or MPF preview slice (16 MiB).
pub(crate) const MAX_EMBEDDED_THUMB_BYTES: usize = 16 * 1024 * 1024;
/// Initial header prefix read before reserving decode capacity from the fair pool (128 KiB).
pub(crate) const HEADER_PEEK_BYTES: usize = 128 * 1024;
/// Extended header prefix used only when a JPEG carries >128 KiB of metadata before SOF (512 KiB).
const MAX_HEADER_PEEK_BYTES: usize = 512 * 1024;
/// Maximum WebP chunks scanned inside an extended (`VP8X`) or animated (`ANMF`) container.
const MAX_WEBP_CHUNKS: usize = 64;
/// Fixed allocator overhead covering decoder tables and small resize buffers (2 MiB).
const BASE_DECODE_OVERHEAD_BYTES: u64 = 2 * 1024 * 1024;
/// Maximum JPEG marker segments scanned before SOS so crafted files cannot spin CPU.
const MAX_JPEG_MARKERS: usize = 1024;
/// Maximum TIFF IFD entries scanned in EXIF IFD0/IFD1 or MPF index IFD.
const MAX_IFD_ENTRIES: usize = 256;
/// Maximum MPF entries inspected from an APP2 MPF directory.
const MAX_MPF_ENTRIES: usize = 16;

// =================================================================================================
// Measured peak decode bytes-per-pixel table (expressed in tenths of a byte per pixel, excluding
// the input file buffer `source_bytes` which is added separately in `estimate_peak_bytes`).
//
// Measured with the counting global allocator in `tests/thumb_mem.rs` against `image 0.25.10`
// (`zune-jpeg 0.5.15`, `png 0.18.1`, `gif 0.14.2`, `image-webp 0.2.4`) using box-averaging
// `DynamicImage::thumbnail` downscaling for >=2x reductions and post-resize EXIF orientation:
// =================================================================================================

/// Baseline JPEG 4:2:0 (SOF0): measured 3.03 B/px at 50 MP (144.6 MiB), 3.06 B/px at 12 MP.
pub(crate) const BPP_X10_JPEG_BASELINE_420: u64 = 32; // 3.2 B/px
/// Baseline JPEG 4:4:4 (SOF0): measured 3.02 B/px at 50 MP (144.0 MiB), 3.05 B/px at 4 MP.
pub(crate) const BPP_X10_JPEG_BASELINE_444: u64 = 32; // 3.2 B/px
/// Baseline JPEG 4-component (CMYK/YCCK) or unusual subsampling: conservative 4.5 B/px.
pub(crate) const BPP_X10_JPEG_BASELINE_OTHER: u64 = 45; // 4.5 B/px
/// Progressive JPEG 4:2:0 (SOF2): measured 6.04 B/px at 25 MP (144.1 MiB; RGB8 + 4:2:0 i16 DCT).
pub(crate) const BPP_X10_JPEG_PROGRESSIVE_420: u64 = 62; // 6.2 B/px
/// Progressive JPEG 4:2:2 (SOF2): RGB8 (3 B) + 4:2:2 i16 DCT planes (4 B) = ~7.0 B/px.
pub(crate) const BPP_X10_JPEG_PROGRESSIVE_422: u64 = 74; // 7.4 B/px
/// Progressive JPEG 4:4:4 (SOF2): measured 9.02 B/px at 25 MP (215.0 MiB; RGB8 + 4:4:4 i16 DCT).
pub(crate) const BPP_X10_JPEG_PROGRESSIVE_444: u64 = 92; // 9.2 B/px
/// Progressive JPEG 4-component (CMYK/YCCK): RGB8 (3 B) + 4 full i16 DCT planes (8 B) = ~11 B/px.
pub(crate) const BPP_X10_JPEG_PROGRESSIVE_OTHER: u64 = 115; // 11.5 B/px

/// PNG 8-bit RGB (non-interlaced): measured 3.10 B/px at 4 MP (11.8 MiB).
pub(crate) const BPP_X10_PNG_RGB8: u64 = 33; // 3.3 B/px
/// PNG 8-bit RGBA (non-interlaced): measured 4.11 B/px at 4 MP (15.7 MiB).
pub(crate) const BPP_X10_PNG_RGBA8: u64 = 43; // 4.3 B/px
/// PNG 8-bit grayscale/indexed (non-interlaced): <= RGB8/RGBA8 (1-4 B/px); conservative 4.3 B/px.
pub(crate) const BPP_X10_PNG_OTHER8: u64 = 43; // 4.3 B/px
/// PNG 16-bit RGB (non-interlaced): measured 6.13 B/px at 4 MP (23.4 MiB).
pub(crate) const BPP_X10_PNG_RGB16: u64 = 63; // 6.3 B/px
/// PNG 16-bit RGBA (non-interlaced): measured 8.15 B/px at 4 MP (31.1 MiB).
pub(crate) const BPP_X10_PNG_RGBA16: u64 = 83; // 8.3 B/px
/// PNG interlaced (Adam7) 8-bit RGB: measured 6.07 B/px decode (2x RGB8 pass + output buffer).
pub(crate) const BPP_X10_PNG_INTERLACED_RGB8: u64 = 63; // 6.3 B/px
/// PNG interlaced (Adam7) 8-bit RGBA/other: measured 8.07 B/px decode (2x RGBA8 pass + output).
pub(crate) const BPP_X10_PNG_INTERLACED_RGBA8: u64 = 83; // 8.3 B/px
/// PNG interlaced (Adam7) 16-bit RGB: 2x 16-bit RGB pass + output = ~12.2 B/px.
pub(crate) const BPP_X10_PNG_INTERLACED_RGB16: u64 = 124; // 12.4 B/px
/// PNG interlaced (Adam7) 16-bit RGBA: measured 16.08 B/px decode (2x RGBA16 pass + output).
pub(crate) const BPP_X10_PNG_INTERLACED_RGBA16: u64 = 164; // 16.4 B/px

/// GIF first frame: measured 5.01 B/px at 4 MP (19.1 MiB; RGBA8 canvas + 1 B/px indexed LZW).
pub(crate) const BPP_X10_GIF: u64 = 52; // 5.2 B/px
/// WebP lossy (VP8): measured 4.64 B/px at 4 MP (17.7 MiB; RGB8 + YUV420 macroblock planes).
pub(crate) const BPP_X10_WEBP_LOSSY: u64 = 48; // 4.8 B/px
/// WebP lossy with VP8X alpha chunk: VP8 YUV420 + alpha + RGBA8 (~5.8 B/px).
pub(crate) const BPP_X10_WEBP_LOSSY_ALPHA: u64 = 60; // 6.0 B/px
/// WebP lossless (VP8L): measured 7.00 B/px at 4 MP for RGB (RGBA8 decode + RGB8 conversion)
/// and 4.07 B/px for RGBA.
pub(crate) const BPP_X10_WEBP_LOSSLESS: u64 = 72; // 7.2 B/px
/// BMP (24-bit RGB and 32-bit RGBA): measured 4.19 B/px decode at 4 MP (plus `source_bytes`).
pub(crate) const BPP_X10_BMP: u64 = 44; // 4.4 B/px

/// Specific format and encoding subtype discovered from the image header before decoding pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageSubtype {
    JpegBaseline420,
    JpegBaseline444,
    JpegBaselineOther,
    JpegProgressive420,
    JpegProgressive422,
    JpegProgressive444,
    JpegProgressiveOther,
    PngRgb8,
    PngRgba8,
    PngOther8,
    PngRgb16,
    PngRgba16,
    PngInterlacedRgb8,
    PngInterlacedRgba8,
    PngInterlacedRgb16,
    PngInterlacedRgba16,
    Gif,
    WebpLossy,
    WebpLossyAlpha,
    WebpLossless,
    Bmp,
}

impl ImageSubtype {
    /// Returns the measured decode-memory factor in tenths of a byte per pixel.
    pub(crate) const fn bpp_x10(self) -> u64 {
        match self {
            ImageSubtype::JpegBaseline420 => BPP_X10_JPEG_BASELINE_420,
            ImageSubtype::JpegBaseline444 => BPP_X10_JPEG_BASELINE_444,
            ImageSubtype::JpegBaselineOther => BPP_X10_JPEG_BASELINE_OTHER,
            ImageSubtype::JpegProgressive420 => BPP_X10_JPEG_PROGRESSIVE_420,
            ImageSubtype::JpegProgressive422 => BPP_X10_JPEG_PROGRESSIVE_422,
            ImageSubtype::JpegProgressive444 => BPP_X10_JPEG_PROGRESSIVE_444,
            ImageSubtype::JpegProgressiveOther => BPP_X10_JPEG_PROGRESSIVE_OTHER,
            ImageSubtype::PngRgb8 => BPP_X10_PNG_RGB8,
            ImageSubtype::PngRgba8 => BPP_X10_PNG_RGBA8,
            ImageSubtype::PngOther8 => BPP_X10_PNG_OTHER8,
            ImageSubtype::PngRgb16 => BPP_X10_PNG_RGB16,
            ImageSubtype::PngRgba16 => BPP_X10_PNG_RGBA16,
            ImageSubtype::PngInterlacedRgb8 => BPP_X10_PNG_INTERLACED_RGB8,
            ImageSubtype::PngInterlacedRgba8 => BPP_X10_PNG_INTERLACED_RGBA8,
            ImageSubtype::PngInterlacedRgb16 => BPP_X10_PNG_INTERLACED_RGB16,
            ImageSubtype::PngInterlacedRgba16 => BPP_X10_PNG_INTERLACED_RGBA16,
            ImageSubtype::Gif => BPP_X10_GIF,
            ImageSubtype::WebpLossy => BPP_X10_WEBP_LOSSY,
            ImageSubtype::WebpLossyAlpha => BPP_X10_WEBP_LOSSY_ALPHA,
            ImageSubtype::WebpLossless => BPP_X10_WEBP_LOSSLESS,
            ImageSubtype::Bmp => BPP_X10_BMP,
        }
    }

    /// Short human-readable label for error messages and diagnostics.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            ImageSubtype::JpegBaseline420 => "baseline JPEG (4:2:0)",
            ImageSubtype::JpegBaseline444 => "baseline JPEG (4:4:4)",
            ImageSubtype::JpegBaselineOther => "baseline JPEG",
            ImageSubtype::JpegProgressive420 => "progressive JPEG (4:2:0)",
            ImageSubtype::JpegProgressive422 => "progressive JPEG (4:2:2)",
            ImageSubtype::JpegProgressive444 => "progressive JPEG (4:4:4)",
            ImageSubtype::JpegProgressiveOther => "progressive JPEG",
            ImageSubtype::PngRgb8 => "8-bit RGB PNG",
            ImageSubtype::PngRgba8 => "8-bit RGBA PNG",
            ImageSubtype::PngOther8 => "8-bit PNG",
            ImageSubtype::PngRgb16 => "16-bit RGB PNG",
            ImageSubtype::PngRgba16 => "16-bit RGBA PNG",
            ImageSubtype::PngInterlacedRgb8 => "interlaced 8-bit RGB PNG",
            ImageSubtype::PngInterlacedRgba8 => "interlaced 8-bit RGBA PNG",
            ImageSubtype::PngInterlacedRgb16 => "interlaced 16-bit RGB PNG",
            ImageSubtype::PngInterlacedRgba16 => "interlaced 16-bit RGBA PNG",
            ImageSubtype::Gif => "GIF",
            ImageSubtype::WebpLossy => "lossy WebP",
            ImageSubtype::WebpLossyAlpha => "lossy WebP with alpha",
            ImageSubtype::WebpLossless => "lossless WebP",
            ImageSubtype::Bmp => "BMP",
        }
    }
}

/// Header-derived decode profile used to admit or reject an image before allocating pixel buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ImageProfile {
    pub(crate) format: ImageFormat,
    pub(crate) subtype: ImageSubtype,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) estimated_peak_bytes: u64,
}

impl ImageProfile {
    /// Returns true if the image's estimated decode memory fits within the 256 MiB per-image cap.
    pub(crate) const fn fits_cap(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_IMAGE_DIMENSION
            && self.height <= MAX_IMAGE_DIMENSION
            && self.estimated_peak_bytes <= MAX_IMAGE_DECODE_BYTES
    }
}

/// Extracted JPEG metadata used for both subtype estimation and over-cap embedded thumbnail
/// fallback (MPF APP2 preview preferred over EXIF IFD1 thumbnail).
#[derive(Debug)]
pub(crate) struct JpegScanMeta {
    pub(crate) subtype: Option<ImageSubtype>,
    pub(crate) max_width: u32,
    pub(crate) max_height: u32,
    pub(crate) orientation: Orientation,
    /// True when an EXIF APP1 or MPF APP2 segment was observed in the scanned prefix, even if an
    /// appended MPF preview starts past the prefix length.
    pub(crate) saw_embedded_marker: bool,
    /// Validated `(start, end)` byte slices inside the JPEG file, ordered best-first
    /// (largest valid MPF preview first, then EXIF IFD1 thumbnail).
    pub(crate) embedded_thumbs: Vec<(usize, usize)>,
}

/// Outcome of peeking only the file header before reserving decode capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderPeek {
    pub(crate) profile: ImageProfile,
    /// True when an over-cap JPEG may still carry an embedded EXIF/MPF preview that the worker can
    /// extract and decode within the 256 MiB cap.
    pub(crate) may_have_embedded_preview: bool,
    /// True when inner frame dimensions could not be verified within the bounded header peek (for
    /// example, a GIF with >128 KiB of leading extension blocks before its first image descriptor),
    /// so the caller must reserve the full expensive-case budget.
    pub(crate) uncertain_frame_bounds: bool,
}

/// Estimates peak heap allocation in bytes for decoding an image of `width x height` pixels with
/// `subtype` and compressed input size `source_bytes`.
///
/// Accounts for `max(2 * source_bytes, decode_bytes + source_bytes)`: `image::ImageReader` may
/// copy the compressed input into a codec buffer before dropping the caller's `Vec` (briefly
/// holding `2 * source_bytes`), after which only one compressed copy and the decoded pixel buffers
/// (`decode_bytes + source_bytes`) are live simultaneously.
pub(crate) fn estimate_peak_bytes(
    width: u32,
    height: u32,
    subtype: ImageSubtype,
    source_bytes: usize,
) -> u64 {
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    let decode_bytes = pixels.saturating_mul(subtype.bpp_x10()).saturating_add(9) / 10;
    let src = source_bytes as u64;
    // During `into_decoder()`, `Vec::read_to_end` grows geometrically (rounding up to a power of
    // two, up to +4 MiB on 63 MiB padded files) while the caller's `Vec` is still live.
    let copy_peak = src.saturating_mul(2).saturating_add(4 * 1024 * 1024);
    decode_bytes
        .saturating_add(src)
        .max(copy_peak)
        .saturating_add(BASE_DECODE_OVERHEAD_BYTES)
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

pub(crate) fn image_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    limits
}

/// Inspects magic bytes, dimensions, and format/subtype headers without decoding pixels, returning
/// an [`ImageProfile`] with its estimated peak decode memory.
///
/// Succeeds even when `estimated_peak_bytes > MAX_IMAGE_DECODE_BYTES` so the caller can attempt
/// EXIF/MPF embedded thumbnail fallback before returning `too_large`.
pub(crate) fn probe_image_profile(
    path: &Path,
    bytes: &[u8],
) -> Result<(ImageProfile, Option<JpegScanMeta>), ErrorBody> {
    probe_image_profile_with_source_size(path, bytes, bytes.len())
}

/// Same as [`probe_image_profile`], but accepts the true compressed `source_bytes` length when
/// `bytes` is only a header prefix read from disk.
pub(crate) fn probe_image_profile_with_source_size(
    path: &Path,
    bytes: &[u8],
    source_bytes: usize,
) -> Result<(ImageProfile, Option<JpegScanMeta>), ErrorBody> {
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
    let (mut w, mut h) = reader.into_dimensions().map_err(|err| {
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

    // Cross-check container dimensions against every inner frame header so files whose outer
    // header under-reports dimensions (fake JPEG SOF before real SOF, GIF frame larger than
    // logical screen, WebP VP8/VP8L/ANMF larger than VP8X canvas) cannot slip past admission.
    let (subtype, jpeg_meta) = match format {
        ImageFormat::Jpeg => {
            let meta = scan_jpeg_metadata(bytes);
            w = w.max(meta.max_width);
            h = h.max(meta.max_height);
            let sub = meta.subtype.unwrap_or(ImageSubtype::JpegProgressiveOther);
            (sub, Some(meta))
        }
        ImageFormat::Png => (parse_png_subtype(bytes), None),
        ImageFormat::Gif => {
            let (gw, gh, found_frame) = parse_gif_max_dimensions(bytes);
            w = w.max(gw);
            h = h.max(gh);
            if !found_frame && source_bytes > bytes.len() {
                // First image descriptor lies past the scanned prefix: treat as expensive so a
                // long run of extension blocks cannot smuggle a huge frame on a tiny reservation.
                let estimated_peak_bytes = MAX_IMAGE_DECODE_BYTES;
                return Ok((
                    ImageProfile {
                        format,
                        subtype: ImageSubtype::Gif,
                        width: w,
                        height: h,
                        estimated_peak_bytes,
                    },
                    None,
                ));
            }
            (ImageSubtype::Gif, None)
        }
        ImageFormat::WebP => {
            let (sub, ww, wh) = parse_webp_info(bytes);
            w = w.max(ww);
            h = h.max(wh);
            (sub, None)
        }
        ImageFormat::Bmp => (ImageSubtype::Bmp, None),
        _ => (ImageSubtype::JpegProgressiveOther, None),
    };

    let estimated_peak_bytes = estimate_peak_bytes(w, h, subtype, source_bytes);
    Ok((
        ImageProfile {
            format,
            subtype,
            width: w,
            height: h,
            estimated_peak_bytes,
        },
        jpeg_meta,
    ))
}

/// Peeks only the header prefix of an already-opened image file (never loading the full file into
/// the parent process) and rewinds the file back to byte 0 so it can be handed directly to the
/// decode worker.
pub(crate) fn peek_image_file(
    path: &Path,
    file: &mut File,
    file_size: u64,
) -> Result<HeaderPeek, ErrorBody> {
    let source_bytes = usize::try_from(file_size).unwrap_or(usize::MAX);
    let mut prefix = Vec::with_capacity(source_bytes.min(HEADER_PEEK_BYTES));
    file.take(HEADER_PEEK_BYTES as u64)
        .read_to_end(&mut prefix)
        .map_err(|err| super::map_io_error("read", path, err))?;

    let mut probed = probe_image_profile_with_source_size(path, &prefix, source_bytes);

    // Some camera JPEGs carry >128 KiB of XMP/ICC metadata before the first SOF marker. If the
    // initial 128 KiB prefix was a JPEG that hit EOF before SOF, extend the peek up to 512 KiB
    // while still never reading multi-megabyte bodies into the parent process.
    let is_jpeg_magic = prefix.starts_with(&[0xFF, 0xD8]);
    let need_more_jpeg = is_jpeg_magic
        && prefix.len() == HEADER_PEEK_BYTES
        && file_size > HEADER_PEEK_BYTES as u64
        && (probed.is_err()
            || matches!(
                &probed,
                Ok((_, Some(meta))) if meta.max_width == 0 || meta.max_height == 0
            ));

    if need_more_jpeg {
        let extra_cap = (MAX_HEADER_PEEK_BYTES - HEADER_PEEK_BYTES) as u64;
        file.take(extra_cap)
            .read_to_end(&mut prefix)
            .map_err(|err| super::map_io_error("read", path, err))?;
        probed = probe_image_profile_with_source_size(path, &prefix, source_bytes);
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|err| super::map_io_error("seek", path, err))?;

    let (profile, jpeg_meta) = match probed {
        Ok(pair) => pair,
        // If a large JPEG still has >512 KiB of metadata before SOF, reserve the full expensive-case
        // budget and let the memory-limited decode worker inspect the full stream.
        Err(_) if is_jpeg_magic && file_size > prefix.len() as u64 => {
            return Ok(HeaderPeek {
                profile: ImageProfile {
                    format: ImageFormat::Jpeg,
                    subtype: ImageSubtype::JpegProgressiveOther,
                    width: 1,
                    height: 1,
                    estimated_peak_bytes: MAX_IMAGE_DECODE_BYTES,
                },
                may_have_embedded_preview: true,
                uncertain_frame_bounds: true,
            });
        }
        Err(err) => return Err(err),
    };

    let uncertain_frame_bounds = profile.format == ImageFormat::Gif
        && file_size > prefix.len() as u64
        && !parse_gif_max_dimensions(&prefix).2;
    let may_have_embedded_preview =
        jpeg_meta.is_some_and(|meta| meta.saw_embedded_marker || !meta.embedded_thumbs.is_empty());
    Ok(HeaderPeek {
        profile,
        may_have_embedded_preview,
        uncertain_frame_bounds,
    })
}

/// Builds the per-path `too_large` error returned when an image exceeds the 256 MiB decode memory
/// cap and has no usable embedded thumbnail.
///
/// Uses Legion's closed `ErrorCode::Failed` wire code with structured `details.reason = "too_large"`
/// so callers can branch on the reason without expanding the 13-code wire contract.
pub(crate) fn too_large_error(path: &Path, profile: &ImageProfile) -> ErrorBody {
    let est_mib = profile.estimated_peak_bytes as f64 / (1024.0 * 1024.0);
    let limit_mib = MAX_IMAGE_DECODE_BYTES / (1024 * 1024);
    ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "{} ({}x{}, {}) would need about {est_mib:.0} MiB to decode, exceeding the {limit_mib} MiB thumbnail limit, and carries no usable embedded thumbnail.",
            path.display(),
            profile.width,
            profile.height,
            profile.subtype.label()
        ),
    )
    .with_hint("Open or download the original image instead, or embed a standard EXIF/MPF preview.")
    .with_details(json!({
        "reason": "too_large",
        "width": profile.width,
        "height": profile.height,
        "format": profile.subtype.label(),
        "estimated_bytes": profile.estimated_peak_bytes,
        "limit_bytes": MAX_IMAGE_DECODE_BYTES,
    }))
}

fn parse_png_subtype(bytes: &[u8]) -> ImageSubtype {
    // PNG signature (8) + length (4) + "IHDR" (4) + width (4) + height (4) + bit_depth (1)
    // + color_type (1) + compression (1) + filter (1) + interlace (1) = 29 bytes.
    if bytes.len() < 29 || &bytes[12..16] != b"IHDR" {
        return ImageSubtype::PngInterlacedRgba16;
    }
    let bit_depth = bytes[24];
    let color_type = bytes[25];
    let interlace = bytes[28];
    let interlaced = interlace != 0;
    let is_16 = bit_depth == 16;

    match (interlaced, is_16, color_type) {
        (false, false, 2) => ImageSubtype::PngRgb8,
        (false, false, 6) => ImageSubtype::PngRgba8,
        (false, false, _) => ImageSubtype::PngOther8,
        (false, true, 2) => ImageSubtype::PngRgb16,
        (false, true, _) => ImageSubtype::PngRgba16,
        (true, false, 2) => ImageSubtype::PngInterlacedRgb8,
        (true, false, _) => ImageSubtype::PngInterlacedRgba8,
        (true, true, 2) => ImageSubtype::PngInterlacedRgb16,
        (true, true, _) => ImageSubtype::PngInterlacedRgba16,
    }
}

/// Walks GIF extension blocks up to the byte bound of `bytes` to locate the first Image
/// Descriptor (`0x2C`), returning `(max_width, max_height, found_image_descriptor)`.
///
/// Bounding strictly by byte offset (rather than a fixed extension-block count) ensures a crafted
/// file with hundreds of tiny comment blocks cannot push its first frame descriptor past a block
/// cap while still keeping the scan O(prefix length).
fn parse_gif_max_dimensions(bytes: &[u8]) -> (u32, u32, bool) {
    // GIF87a / GIF89a header (6) + Logical Screen Descriptor (7) = 13 bytes.
    if bytes.len() < 13 || (&bytes[0..6] != b"GIF87a" && &bytes[0..6] != b"GIF89a") {
        return (0, 0, false);
    }
    let mut max_w = u32::from(u16::from_le_bytes([bytes[6], bytes[7]]));
    let mut max_h = u32::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    let packed = bytes[10];
    let mut pos = 13usize;
    if (packed & 0x80) != 0 {
        let gct_entries = 1usize << ((packed & 0x07) + 1);
        pos = pos.saturating_add(gct_entries * 3);
    }

    let mut found_frame = false;
    while pos < bytes.len() {
        match bytes[pos] {
            0x21 => {
                // Extension block: introducer (1) + label (1) + sub-blocks terminated by 0x00.
                let Some(mut cur) = pos.checked_add(2) else {
                    break;
                };
                let mut terminated = false;
                while let Some(&sub_len) = bytes.get(cur) {
                    cur = cur.saturating_add(1);
                    if sub_len == 0 {
                        terminated = true;
                        break;
                    }
                    let Some(next) = cur.checked_add(usize::from(sub_len)) else {
                        return (max_w, max_h, false);
                    };
                    cur = next;
                }
                if !terminated || cur <= pos {
                    break;
                }
                pos = cur;
            }
            0x2C => {
                // Image Descriptor: 0x2C + left(2) + top(2) + width(2) + height(2) + packed(1).
                if let Some(desc) = bytes.get(pos + 1..pos + 10) {
                    let left = u32::from(u16::from_le_bytes([desc[0], desc[1]]));
                    let top = u32::from(u16::from_le_bytes([desc[2], desc[3]]));
                    let fw = u32::from(u16::from_le_bytes([desc[4], desc[5]]));
                    let fh = u32::from(u16::from_le_bytes([desc[6], desc[7]]));
                    max_w = max_w.max(fw).max(left.saturating_add(fw));
                    max_h = max_h.max(fh).max(top.saturating_add(fh));
                    found_frame = true;
                }
                break;
            }
            0x3B => {
                found_frame = true;
                break;
            }
            _ => break,
        }
    }
    (max_w, max_h, found_frame)
}

fn parse_vp8_dims(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 10 || payload[3..6] != [0x9D, 0x01, 0x2A] {
        return None;
    }
    let w = u32::from(u16::from_le_bytes([payload[6], payload[7]]) & 0x3FFF);
    let h = u32::from(u16::from_le_bytes([payload[8], payload[9]]) & 0x3FFF);
    Some((w, h))
}

fn parse_vp8l_dims(payload: &[u8]) -> Option<(u32, u32)> {
    if payload.len() < 5 || payload[0] != 0x2F {
        return None;
    }
    let bits = u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]);
    let w = (bits & 0x3FFF) + 1;
    let h = ((bits >> 14) & 0x3FFF) + 1;
    Some((w, h))
}

fn parse_webp_info(bytes: &[u8]) -> (ImageSubtype, u32, u32) {
    // RIFF (4) + size (4) + WEBP (4) + FourCC (4) = 16 bytes.
    if bytes.len() < 20 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return (ImageSubtype::WebpLossless, 0, 0);
    }
    match &bytes[12..16] {
        b"VP8 " => {
            let (w, h) = parse_vp8_dims(&bytes[20..]).unwrap_or((0, 0));
            (ImageSubtype::WebpLossy, w, h)
        }
        b"VP8L" => {
            let (w, h) = parse_vp8l_dims(&bytes[20..]).unwrap_or((0, 0));
            (ImageSubtype::WebpLossless, w, h)
        }
        b"VP8X" => {
            // Extended WebP: flags byte at offset 20 (bit 4 = alpha), canvas width/height minus 1
            // as 24-bit LE at offsets 24..30, followed by sub-chunks starting at offset 30.
            let has_alpha = bytes.get(20).is_some_and(|flags| (flags & 0x10) != 0);
            let mut max_w = bytes
                .get(24..27)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], 0]) + 1);
            let mut max_h = bytes
                .get(27..30)
                .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], 0]) + 1);
            let mut subtype: Option<ImageSubtype> = None;

            let mut record_sub = |cand: ImageSubtype| {
                subtype = Some(match subtype {
                    Some(prev) if prev.bpp_x10() >= cand.bpp_x10() => prev,
                    _ => cand,
                });
            };

            let mut pos = 30usize;
            for _ in 0..MAX_WEBP_CHUNKS {
                let Some(header) = bytes.get(pos..pos + 8) else {
                    break;
                };
                let fourcc = &header[0..4];
                let chunk_len =
                    u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
                let payload_start = pos + 8;
                let payload_end = payload_start.saturating_add(chunk_len).min(bytes.len());
                let payload = bytes.get(payload_start..payload_end).unwrap_or(&[]);

                match fourcc {
                    b"VP8L" => {
                        if let Some((w, h)) = parse_vp8l_dims(payload) {
                            max_w = max_w.max(w);
                            max_h = max_h.max(h);
                        }
                        record_sub(ImageSubtype::WebpLossless);
                    }
                    b"VP8 " => {
                        if let Some((w, h)) = parse_vp8_dims(payload) {
                            max_w = max_w.max(w);
                            max_h = max_h.max(h);
                        }
                        record_sub(if has_alpha {
                            ImageSubtype::WebpLossyAlpha
                        } else {
                            ImageSubtype::WebpLossy
                        });
                    }
                    b"ANMF" if payload.len() >= 16 => {
                        let fw = u32::from_le_bytes([payload[6], payload[7], payload[8], 0]) + 1;
                        let fh = u32::from_le_bytes([payload[9], payload[10], payload[11], 0]) + 1;
                        max_w = max_w.max(fw);
                        max_h = max_h.max(fh);
                        let mut sub_pos = 16usize;
                        for _ in 0..4 {
                            let Some(sh) = payload.get(sub_pos..sub_pos + 8) else {
                                break;
                            };
                            let s_fcc = &sh[0..4];
                            let s_len = u32::from_le_bytes([sh[4], sh[5], sh[6], sh[7]]) as usize;
                            let s_start = sub_pos + 8;
                            let s_end = s_start.saturating_add(s_len).min(payload.len());
                            let s_pay = payload.get(s_start..s_end).unwrap_or(&[]);
                            if s_fcc == b"VP8 " {
                                if let Some((w, h)) = parse_vp8_dims(s_pay) {
                                    max_w = max_w.max(w);
                                    max_h = max_h.max(h);
                                }
                                record_sub(if has_alpha {
                                    ImageSubtype::WebpLossyAlpha
                                } else {
                                    ImageSubtype::WebpLossy
                                });
                            } else if s_fcc == b"VP8L" {
                                if let Some((w, h)) = parse_vp8l_dims(s_pay) {
                                    max_w = max_w.max(w);
                                    max_h = max_h.max(h);
                                }
                                record_sub(ImageSubtype::WebpLossless);
                            }
                            let s_pad = s_len.saturating_add(s_len & 1);
                            let Some(s_next) =
                                sub_pos.checked_add(8).and_then(|p| p.checked_add(s_pad))
                            else {
                                break;
                            };
                            if s_next <= sub_pos || s_next > payload.len() {
                                break;
                            }
                            sub_pos = s_next;
                        }
                    }
                    _ => {}
                }

                let padded = chunk_len.saturating_add(chunk_len & 1);
                let Some(next) = pos.checked_add(8).and_then(|p| p.checked_add(padded)) else {
                    break;
                };
                if next <= pos || next > bytes.len() {
                    break;
                }
                pos = next;
            }
            // Unknown VP8X payload: conservatively assume lossless (highest factor).
            (subtype.unwrap_or(ImageSubtype::WebpLossless), max_w, max_h)
        }
        _ => (ImageSubtype::WebpLossless, 0, 0),
    }
}

/// Scans JPEG marker segments before `SOS` to determine the worst-case frame encoding subtype and
/// dimensions across every SOF segment encountered, the main image EXIF orientation, and validated
/// byte ranges for any embedded MPF preview or EXIF IFD1 thumbnail.
pub(crate) fn scan_jpeg_metadata(bytes: &[u8]) -> JpegScanMeta {
    let mut meta = JpegScanMeta {
        subtype: None,
        max_width: 0,
        max_height: 0,
        orientation: Orientation::NoTransforms,
        saw_embedded_marker: false,
        embedded_thumbs: Vec::new(),
    };
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return meta;
    }

    let mut pos = 2usize;
    let mut exif_thumb: Option<(usize, usize)> = None;
    let mut mpf_candidates: Vec<(u8, usize, usize)> = Vec::new(); // (priority, start, end)

    for _ in 0..MAX_JPEG_MARKERS {
        if pos >= bytes.len() {
            break;
        }
        while pos < bytes.len() && bytes[pos] != 0xFF {
            pos += 1;
        }
        while pos < bytes.len() && bytes[pos] == 0xFF {
            pos += 1;
        }
        if pos >= bytes.len() {
            break;
        }
        let marker = bytes[pos];
        pos += 1;

        if marker == 0x00 || marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break;
        }

        let Some(len_bytes) = bytes.get(pos..pos + 2) else {
            break;
        };
        let seg_len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        if seg_len < 2 {
            break;
        }
        let Some(payload_start) = pos.checked_add(2) else {
            break;
        };
        let Some(seg_end) = pos.checked_add(seg_len) else {
            break;
        };
        if seg_end > bytes.len() {
            break;
        }
        let payload = &bytes[payload_start..seg_end];

        match marker {
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                // Take the maximum dimensions and most memory-expensive subtype across ALL SOF
                // markers before SOS so a tiny decoy SOF hidden behind a malformed marker cannot
                // mask a real progressive/large SOF that zune-jpeg actually decodes.
                let (cand, sof_w, sof_h) = parse_jpeg_sof(marker, payload);
                meta.max_width = meta.max_width.max(sof_w);
                meta.max_height = meta.max_height.max(sof_h);
                meta.subtype = Some(match meta.subtype {
                    Some(prev) if prev.bpp_x10() >= cand.bpp_x10() => prev,
                    _ => cand,
                });
            }
            0xE1 => {
                if payload.starts_with(b"Exif\0\0") && payload.len() > 14 {
                    let tiff_start = payload_start + 6;
                    let tiff = &payload[6..];
                    let (orient, ifd1_range) = parse_exif_orientation_and_ifd1(tiff, tiff_start);
                    if meta.orientation == Orientation::NoTransforms
                        && orient != Orientation::NoTransforms
                    {
                        meta.orientation = orient;
                    }
                    if exif_thumb.is_none()
                        && let Some((s, e)) = ifd1_range
                        && bytes
                            .get(s..e)
                            .is_some_and(|sl| sl.starts_with(&[0xFF, 0xD8]))
                    {
                        meta.saw_embedded_marker = true;
                        exif_thumb = Some((s, e));
                    }
                }
            }
            0xE2 if payload.starts_with(b"MPF\0") && payload.len() > 12 => {
                meta.saw_embedded_marker = true;
                let mpf_tiff_start = payload_start + 4;
                let mpf_tiff = &payload[4..];
                collect_mpf_candidates(
                    bytes,
                    mpf_tiff,
                    mpf_tiff_start,
                    seg_end,
                    &mut mpf_candidates,
                );
            }
            _ => {}
        }

        pos = seg_end;
    }

    mpf_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| (b.2 - b.1).cmp(&(a.2 - a.1))));
    for (_, s, e) in mpf_candidates {
        meta.embedded_thumbs.push((s, e));
    }
    if let Some((s, e)) = exif_thumb {
        meta.embedded_thumbs.push((s, e));
    }
    meta
}

fn parse_jpeg_sof(marker: u8, payload: &[u8]) -> (ImageSubtype, u32, u32) {
    let progressive = matches!(marker, 0xC2 | 0xC6 | 0xCA | 0xCE);
    if payload.len() < 6 {
        return (
            if progressive {
                ImageSubtype::JpegProgressiveOther
            } else {
                ImageSubtype::JpegBaselineOther
            },
            0,
            0,
        );
    }
    let sof_h = u32::from(u16::from_be_bytes([payload[1], payload[2]]));
    let sof_w = u32::from(u16::from_be_bytes([payload[3], payload[4]]));
    let num_components = usize::from(payload[5]);
    if num_components == 0 || payload.len() < 6 + num_components * 3 {
        return (
            if progressive {
                ImageSubtype::JpegProgressiveOther
            } else {
                ImageSubtype::JpegBaselineOther
            },
            sof_w,
            sof_h,
        );
    }

    if num_components == 1 {
        return (
            if progressive {
                ImageSubtype::JpegProgressive420
            } else {
                ImageSubtype::JpegBaseline420
            },
            sof_w,
            sof_h,
        );
    }
    if num_components == 3 {
        let s0 = payload[7];
        let s1 = payload[10];
        let s2 = payload[13];
        let (h0, v0) = (s0 >> 4, s0 & 0x0F);
        let (h1, v1) = (s1 >> 4, s1 & 0x0F);
        let (h2, v2) = (s2 >> 4, s2 & 0x0F);
        if (h0, v0) == (2, 2) && (h1, v1) == (1, 1) && (h2, v2) == (1, 1) {
            return (
                if progressive {
                    ImageSubtype::JpegProgressive420
                } else {
                    ImageSubtype::JpegBaseline420
                },
                sof_w,
                sof_h,
            );
        }
        if ((h0, v0) == (2, 1) || (h0, v0) == (1, 2)) && (h1, v1) == (1, 1) && (h2, v2) == (1, 1) {
            return (
                if progressive {
                    ImageSubtype::JpegProgressive422
                } else {
                    ImageSubtype::JpegBaseline420
                },
                sof_w,
                sof_h,
            );
        }
        if (h0, v0) == (1, 1) && (h1, v1) == (1, 1) && (h2, v2) == (1, 1) {
            return (
                if progressive {
                    ImageSubtype::JpegProgressive444
                } else {
                    ImageSubtype::JpegBaseline444
                },
                sof_w,
                sof_h,
            );
        }
    }

    (
        if progressive {
            ImageSubtype::JpegProgressiveOther
        } else {
            ImageSubtype::JpegBaselineOther
        },
        sof_w,
        sof_h,
    )
}

/// Returns `true` when `bytes` looks like an MPEG Transport Stream (188-byte packets starting with
/// `0x47`, or 192-byte M2TS packets with `0x47` at offset 4) and is not plain UTF-8 source text.
pub(crate) fn is_mpeg_ts_bytes(bytes: &[u8]) -> bool {
    if looks_like_text(bytes) {
        return false;
    }
    let has_sync_stride = |offset: usize, stride: usize| {
        if bytes.len() > offset + stride * 3 {
            bytes[offset] == 0x47
                && bytes[offset + stride] == 0x47
                && bytes[offset + stride * 2] == 0x47
                && bytes[offset + stride * 3] == 0x47
        } else if bytes.len() > offset + stride {
            bytes[offset] == 0x47 && bytes[offset + stride] == 0x47
        } else {
            false
        }
    };
    has_sync_stride(0, 188) || has_sync_stride(4, 192)
}

fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if std::str::from_utf8(bytes).is_err() {
        return false;
    }
    bytes
        .iter()
        .all(|&b| matches!(b, b'\t' | b'\n' | b'\r' | 0x20..=0x7E | 0x80..=0xFF))
}

/// Sniffs whether a regular `.ts` file on disk is an MPEG-TS video recording rather than a
/// TypeScript source file, using non-blocking regular-file open so swapped FIFOs never block.
pub(crate) fn is_mpeg_ts_file(path: &Path) -> bool {
    if !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ts"))
    {
        return false;
    }
    let Ok((file, meta)) = legion_os::open_regular::open_regular_read(path) else {
        return false;
    };
    if meta.len() < 189 {
        return false;
    }
    let mut buf = Vec::with_capacity(1024);
    if file.take(1024).read_to_end(&mut buf).is_err() {
        return false;
    }
    is_mpeg_ts_bytes(&buf)
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn from_tiff(tiff: &[u8]) -> Option<(Endian, usize)> {
        if tiff.len() < 8 {
            return None;
        }
        let endian = match &tiff[0..2] {
            b"II" => Endian::Little,
            b"MM" => Endian::Big,
            _ => return None,
        };
        if endian.u16(&tiff[2..4]) != 42 {
            return None;
        }
        let ifd0_offset = endian.u32(&tiff[4..8]) as usize;
        Some((endian, ifd0_offset))
    }

    fn u16(self, b: &[u8]) -> u16 {
        match self {
            Endian::Little => u16::from_le_bytes([b[0], b[1]]),
            Endian::Big => u16::from_be_bytes([b[0], b[1]]),
        }
    }

    fn u32(self, b: &[u8]) -> u32 {
        match self {
            Endian::Little => u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Endian::Big => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        }
    }
}

fn read_ifd_scalar_u32(endian: Endian, entry: &[u8]) -> Option<u32> {
    let field_type = endian.u16(&entry[2..4]);
    let count = endian.u32(&entry[4..8]);
    if count != 1 {
        return None;
    }
    match field_type {
        3 => Some(u32::from(endian.u16(&entry[8..10]))),
        4 => Some(endian.u32(&entry[8..12])),
        _ => None,
    }
}

fn parse_exif_orientation_and_ifd1(
    tiff: &[u8],
    tiff_abs_start: usize,
) -> (Orientation, Option<(usize, usize)>) {
    let Some((endian, ifd0_off)) = Endian::from_tiff(tiff) else {
        return (Orientation::NoTransforms, None);
    };
    let mut orientation = Orientation::NoTransforms;

    let Some((ifd0_entries, next_ifd_off)) = read_ifd_table(tiff, endian, ifd0_off) else {
        return (Orientation::NoTransforms, None);
    };
    for entry in ifd0_entries
        .as_chunks::<12>()
        .0
        .iter()
        .take(MAX_IFD_ENTRIES)
    {
        let tag = endian.u16(&entry[0..2]);
        if tag == 0x0112
            && let Some(val) = read_ifd_scalar_u32(endian, entry)
            && let Ok(code) = u8::try_from(val)
            && let Some(parsed) = Orientation::from_exif(code)
        {
            orientation = parsed;
            break;
        }
    }

    if next_ifd_off < 8 || next_ifd_off == ifd0_off {
        return (orientation, None);
    }
    let Some((ifd1_entries, _)) = read_ifd_table(tiff, endian, next_ifd_off) else {
        return (orientation, None);
    };
    let mut thumb_off: Option<usize> = None;
    let mut thumb_len: Option<usize> = None;
    for entry in ifd1_entries
        .as_chunks::<12>()
        .0
        .iter()
        .take(MAX_IFD_ENTRIES)
    {
        let tag = endian.u16(&entry[0..2]);
        match tag {
            0x0201 => thumb_off = read_ifd_scalar_u32(endian, entry).map(|v| v as usize),
            0x0202 => thumb_len = read_ifd_scalar_u32(endian, entry).map(|v| v as usize),
            _ => {}
        }
    }

    let ifd1_range = match (thumb_off, thumb_len) {
        (Some(off), Some(len)) if off >= 8 && len > 2 && len <= MAX_EMBEDDED_THUMB_BYTES => {
            match off.checked_add(len) {
                Some(end) if end <= tiff.len() => {
                    let abs_start = tiff_abs_start.checked_add(off);
                    let abs_end = abs_start.and_then(|s| s.checked_add(len));
                    abs_start.zip(abs_end)
                }
                _ => None,
            }
        }
        _ => None,
    };

    (orientation, ifd1_range)
}

fn read_ifd_table(tiff: &[u8], endian: Endian, offset: usize) -> Option<(&[u8], usize)> {
    let count_bytes = tiff.get(offset..offset.checked_add(2)?)?;
    let count = usize::from(endian.u16(count_bytes)).min(MAX_IFD_ENTRIES);
    let entries_start = offset.checked_add(2)?;
    let entries_len = count.checked_mul(12)?;
    let entries_end = entries_start.checked_add(entries_len)?;
    let entries = tiff.get(entries_start..entries_end)?;
    let next_bytes = tiff.get(entries_end..entries_end.checked_add(4)?)?;
    let next_off = endian.u32(next_bytes) as usize;
    Some((entries, next_off))
}

fn collect_mpf_candidates(
    file_bytes: &[u8],
    mpf_tiff: &[u8],
    mpf_tiff_abs_start: usize,
    app2_seg_end: usize,
    out: &mut Vec<(u8, usize, usize)>,
) {
    let Some((endian, ifd0_off)) = Endian::from_tiff(mpf_tiff) else {
        return;
    };
    let Some((entries, _)) = read_ifd_table(mpf_tiff, endian, ifd0_off) else {
        return;
    };

    let mut num_images: Option<usize> = None;
    let mut mp_entry_table: Option<(usize, usize)> = None;

    for entry in entries.as_chunks::<12>().0.iter().take(MAX_IFD_ENTRIES) {
        let tag = endian.u16(&entry[0..2]);
        match tag {
            0xB001 => {
                num_images = read_ifd_scalar_u32(endian, entry).map(|v| v as usize);
            }
            0xB002 => {
                let count = endian.u32(&entry[4..8]) as usize;
                let off = endian.u32(&entry[8..12]) as usize;
                mp_entry_table = Some((off, count));
            }
            _ => {}
        }
    }

    let (Some(count_imgs), Some((table_off, table_len))) = (num_images, mp_entry_table) else {
        return;
    };
    if !(2..=MAX_MPF_ENTRIES).contains(&count_imgs) {
        return;
    }
    let Some(expected_len) = count_imgs.checked_mul(16) else {
        return;
    };
    if table_len < expected_len {
        return;
    }
    let Some(table_end) = table_off.checked_add(expected_len) else {
        return;
    };
    let Some(table) = mpf_tiff.get(table_off..table_end) else {
        return;
    };

    for (idx, rec) in table.as_chunks::<16>().0.iter().enumerate() {
        if idx == 0 {
            continue;
        }
        let attr = endian.u32(&rec[0..4]);
        let img_size = endian.u32(&rec[4..8]) as usize;
        let img_offset = endian.u32(&rec[8..12]) as usize;

        if img_offset == 0 || img_size <= 2 || img_size > MAX_EMBEDDED_THUMB_BYTES {
            continue;
        }
        let Some(abs_start) = mpf_tiff_abs_start.checked_add(img_offset) else {
            continue;
        };
        let Some(abs_end) = abs_start.checked_add(img_size) else {
            continue;
        };
        if abs_start < app2_seg_end || abs_end > file_bytes.len() {
            continue;
        }
        if out.iter().any(|&(_, s, e)| abs_start < e && s < abs_end) {
            continue;
        }
        if !file_bytes
            .get(abs_start..abs_end)
            .is_some_and(|sl| sl.starts_with(&[0xFF, 0xD8]))
        {
            continue;
        }

        let mp_type = attr & 0x00FF_FFFF;
        if mp_type == 0x030000 {
            continue;
        }
        let priority = match mp_type {
            0x010002 => 3,
            0x010001 => 2,
            _ => 1,
        };
        out.push((priority, abs_start, abs_end));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn minimal_jpeg_bytes(width: u16, height: u16, progressive: bool, sub420: bool) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 67, 0x00];
        out.extend_from_slice(&[16u8; 64]);
        let sof = if progressive { 0xC2 } else { 0xC0 };
        out.extend_from_slice(&[0xFF, sof, 0x00, 17, 8]);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.push(3);
        out.extend_from_slice(&[1, if sub420 { 0x22 } else { 0x11 }, 0]);
        out.extend_from_slice(&[2, 0x11, 0]);
        out.extend_from_slice(&[3, 0x11, 0]);
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 12, 3, 1, 0, 2, 0, 3, 0, 0, 0, 0]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    pub(crate) fn build_exif_app1(
        orientation: u16,
        thumb_offset: u32,
        thumb_len: u32,
        extra: &[u8],
    ) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        // IFD0 @ 8
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&26u32.to_le_bytes());
        // IFD1 @ 26
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

    pub(crate) fn build_mpf_app2(entries: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&2u16.to_le_bytes());
        tiff.extend_from_slice(&0xB001u16.to_le_bytes());
        tiff.extend_from_slice(&4u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        tiff.extend_from_slice(&0xB002u16.to_le_bytes());
        tiff.extend_from_slice(&7u16.to_le_bytes());
        tiff.extend_from_slice(&((entries.len() * 16) as u32).to_le_bytes());
        tiff.extend_from_slice(&38u32.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(tiff.len(), 38);
        for &(attr, size, offset) in entries {
            tiff.extend_from_slice(&attr.to_le_bytes());
            tiff.extend_from_slice(&size.to_le_bytes());
            tiff.extend_from_slice(&offset.to_le_bytes());
            tiff.extend_from_slice(&0u32.to_le_bytes());
        }

        let mut seg = Vec::new();
        seg.extend_from_slice(&[0xFF, 0xE2]);
        let seg_len = (2 + 4 + tiff.len()) as u16;
        seg.extend_from_slice(&seg_len.to_be_bytes());
        seg.extend_from_slice(b"MPF\0");
        seg.extend_from_slice(&tiff);
        seg
    }

    #[test]
    fn jpeg_subtype_and_memory_estimation_admit_50mp_baseline_and_cap_36mp_progressive_444() {
        let base_50 = minimal_jpeg_bytes(8000, 6250, false, true);
        let (p_base, _) = probe_image_profile(Path::new("/a.jpg"), &base_50).unwrap();
        assert_eq!(p_base.subtype, ImageSubtype::JpegBaseline420);
        assert!(
            p_base.fits_cap(),
            "50 MP baseline should fit 256 MiB: {p_base:?}"
        );

        let prog_444_36 = minimal_jpeg_bytes(6000, 6000, true, false);
        let (p_prog, _) = probe_image_profile(Path::new("/b.jpg"), &prog_444_36).unwrap();
        assert_eq!(p_prog.subtype, ImageSubtype::JpegProgressive444);
        assert!(
            !p_prog.fits_cap(),
            "36 MP progressive 4:4:4 should exceed 256 MiB: {p_prog:?}"
        );
    }

    #[test]
    fn hostile_exif_and_mpf_offsets_are_ignored_without_panicking() {
        let main = minimal_jpeg_bytes(6000, 6000, true, false);
        let tiny_thumb = minimal_jpeg_bytes(160, 120, false, true);

        for (off, len) in [
            (9999u32, 100u32),
            (u32::MAX, 100),
            (56, u32::MAX),
            (56, (MAX_EMBEDDED_THUMB_BYTES as u32) + 1),
            (0, 50),
        ] {
            let mut doc = Vec::new();
            doc.extend_from_slice(&main[..2]);
            doc.extend_from_slice(&build_exif_app1(6, off, len, &tiny_thumb));
            doc.extend_from_slice(&main[2..]);
            let scanned = scan_jpeg_metadata(&doc);
            assert_eq!(scanned.orientation, Orientation::Rotate90);
            assert!(
                scanned.embedded_thumbs.is_empty(),
                "hostile EXIF ({off}, {len}) must yield no candidate"
            );
        }

        let mut doc = Vec::new();
        doc.extend_from_slice(&main[..2]);
        let app2 = build_mpf_app2(&[
            (0x030000, main.len() as u32, 0),
            (0x010002, 100, 0),
            (0x010002, 100, u32::MAX),
            (0x010002, (MAX_EMBEDDED_THUMB_BYTES as u32) + 10, 500),
            (0x010002, 50, 10),
            (0x010002, 100, 500_000),
        ]);
        doc.extend_from_slice(&app2);
        doc.extend_from_slice(&main[2..]);
        let scanned = scan_jpeg_metadata(&doc);
        assert!(scanned.embedded_thumbs.is_empty());

        // Two overlapping MPF entries pointing at the exact same appended preview: second rejected.
        let preview_len = tiny_thumb.len() as u32;
        let placeholder_app2 = build_mpf_app2(&[
            (0x030000, 0, 0),
            (0x010002, preview_len, 1000),
            (0x010001, preview_len, 1000),
        ]);
        let mpf_tiff_start = 2 + 4 + 4;
        let appended_start = 2 + placeholder_app2.len() + (main.len() - 2);
        let rel_off = (appended_start - mpf_tiff_start) as u32;
        let real_app2 = build_mpf_app2(&[
            (0x030000, 0, 0),
            (0x010002, preview_len, rel_off),
            (0x010001, preview_len, rel_off),
        ]);
        assert_eq!(real_app2.len(), placeholder_app2.len());
        let mut doc_overlap = Vec::new();
        doc_overlap.extend_from_slice(&main[..2]);
        doc_overlap.extend_from_slice(&real_app2);
        doc_overlap.extend_from_slice(&main[2..]);
        doc_overlap.extend_from_slice(&tiny_thumb);
        let scanned_ov = scan_jpeg_metadata(&doc_overlap);
        assert_eq!(scanned_ov.embedded_thumbs.len(), 1);
    }

    #[test]
    fn lying_jpeg_with_decoy_sof0_before_real_progressive_sof2_is_rejected() {
        // Reproduces `lie444.jpg`: FF D8, FF D0 00 0F, fake 16x16 1-component SOF0, then a real
        // 9000x9000 3-component 4:4:4 progressive SOF2.
        let mut lie = vec![
            0xFF, 0xD8, 0xFF, 0xD0, 0x00, 0x0F, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x10, 0x00,
            0x10, 0x01, 0x01, 0x11, 0x00,
        ];
        let real = minimal_jpeg_bytes(9000, 9000, true, false);
        lie.extend_from_slice(&real[2..]);

        let (profile, _) = probe_image_profile(Path::new("/lie444.jpg"), &lie).unwrap();
        assert_eq!(profile.width, 9000);
        assert_eq!(profile.height, 9000);
        assert_eq!(profile.subtype, ImageSubtype::JpegProgressive444);
        assert!(
            !profile.fits_cap(),
            "9000x9000 progressive 4:4:4 behind decoy SOF0 must exceed cap: {profile:?}"
        );
    }

    #[test]
    fn lying_gif_with_tiny_logical_screen_and_huge_first_frame_is_rejected() {
        // Reproduces `liegif.gif`: 64x64 logical screen + 8000x8000 first frame image descriptor.
        let mut gif = Vec::new();
        gif.extend_from_slice(b"GIF89a");
        gif.extend_from_slice(&64u16.to_le_bytes());
        gif.extend_from_slice(&64u16.to_le_bytes());
        gif.extend_from_slice(&[0x80, 0x00, 0x00]); // 2-entry GCT (6 bytes)
        gif.extend_from_slice(&[0x00; 6]);
        // Image Descriptor (0x2C): left=0, top=0, w=8000, h=8000, packed=0
        gif.push(0x2C);
        gif.extend_from_slice(&0u16.to_le_bytes());
        gif.extend_from_slice(&0u16.to_le_bytes());
        gif.extend_from_slice(&8000u16.to_le_bytes());
        gif.extend_from_slice(&8000u16.to_le_bytes());
        gif.push(0x00);
        gif.extend_from_slice(&[0x02, 0x00, 0x3B]);

        let (profile, _) = probe_image_profile(Path::new("/liegif.gif"), &gif).unwrap();
        assert_eq!(profile.width, 8000);
        assert_eq!(profile.height, 8000);
        assert!(
            !profile.fits_cap(),
            "8000x8000 GIF frame inside 64x64 logical screen must exceed cap: {profile:?}"
        );

        // Reproduces `mkgif2.py`: 250 comment extension blocks before an 8000x8000 frame must STILL
        // be scanned and rejected (no fixed 128-block cutoff).
        let mut gif_many_ext = Vec::new();
        gif_many_ext.extend_from_slice(b"GIF89a");
        gif_many_ext.extend_from_slice(&16u16.to_le_bytes());
        gif_many_ext.extend_from_slice(&16u16.to_le_bytes());
        gif_many_ext.extend_from_slice(&[0x80, 0x00, 0x00, 0, 0, 0, 255, 255, 255]);
        for _ in 0..250 {
            gif_many_ext.extend_from_slice(&[0x21, 0xFE, 0x01, b'x', 0x00]);
        }
        gif_many_ext.push(0x2C);
        gif_many_ext.extend_from_slice(&0u16.to_le_bytes());
        gif_many_ext.extend_from_slice(&0u16.to_le_bytes());
        gif_many_ext.extend_from_slice(&8000u16.to_le_bytes());
        gif_many_ext.extend_from_slice(&8000u16.to_le_bytes());
        gif_many_ext.push(0x00);
        gif_many_ext.extend_from_slice(&[0x02, 0x00, 0x3B]);

        let (profile2, _) =
            probe_image_profile(Path::new("/liegif_ext250.gif"), &gif_many_ext).unwrap();
        assert_eq!(profile2.width, 8000);
        assert_eq!(profile2.height, 8000);
        assert!(!profile2.fits_cap());
    }

    #[test]
    fn lying_webp_with_tiny_vp8x_canvas_and_huge_vp8_frame_is_rejected() {
        // Reproduces `liewebp.webp`: 64x64 VP8X canvas wrapping a 12000x12000 VP8 keyframe.
        let mut vp8 = vec![0x90, 0x01, 0x00, 0x9D, 0x01, 0x2A];
        vp8.extend_from_slice(&12_000u16.to_le_bytes());
        vp8.extend_from_slice(&12_000u16.to_le_bytes());
        vp8.extend_from_slice(&[0u8; 16]);

        let mut body = Vec::new();
        body.extend_from_slice(b"WEBP");
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // flags
        body.extend_from_slice(&[63, 0, 0]); // canvas width - 1 = 63 (64 px)
        body.extend_from_slice(&[63, 0, 0]); // canvas height - 1 = 63 (64 px)
        body.extend_from_slice(b"VP8 ");
        body.extend_from_slice(&(vp8.len() as u32).to_le_bytes());
        body.extend_from_slice(&vp8);

        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
        webp.extend_from_slice(&body);

        let (profile, _) = probe_image_profile(Path::new("/liewebp.webp"), &webp).unwrap();
        assert_eq!(profile.width, 12_000);
        assert_eq!(profile.height, 12_000);
        assert!(
            !profile.fits_cap(),
            "12000x12000 VP8 frame inside 64x64 VP8X canvas must exceed cap: {profile:?}"
        );
    }

    #[test]
    fn mpeg_ts_sniffer_distinguishes_transport_streams_from_typescript() {
        // Synthetic 4-packet MPEG-TS stream (188-byte stride starting with 0x47 + binary headers).
        let mut ts_video = vec![0u8; 188 * 4];
        for i in 0..4 {
            ts_video[i * 188] = 0x47;
            ts_video[i * 188 + 1] = 0x40;
            ts_video[i * 188 + 2] = 0x00;
            ts_video[i * 188 + 3] = 0x10;
        }
        assert!(is_mpeg_ts_bytes(&ts_video));

        // Real TypeScript source (even starting with 'G' = 0x47!) must never match MPEG-TS.
        let mut ts_code = String::from("G = 1;\n");
        while ts_code.len() < 1000 {
            ts_code.push_str("export const value: number = 42;\n");
        }
        assert!(!is_mpeg_ts_bytes(ts_code.as_bytes()));

        // Even a pathological UTF-8 TypeScript file with 'G' placed at offsets 0, 188, 376, 564.
        let mut crafted_text = vec![b'a'; 800];
        for i in 0..4 {
            crafted_text[i * 188] = b'G';
        }
        assert!(!is_mpeg_ts_bytes(&crafted_text));
    }
}
