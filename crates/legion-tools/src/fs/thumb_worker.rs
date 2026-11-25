//! Out-of-process, memory-bounded raster image decode worker for `fs.thumb`.
//!
//! When `legion` runs in production (or inside an opt-in integration harness), image decoding is
//! isolated in a hidden `__thumb-worker` subcommand of the same binary:
//!
//! - The parent process verifies policy, opens the regular file once, peeks only the header prefix
//!   (`<= 128 KiB`), reserves estimated bytes from the fair image pool, and hands the already-opened
//!   file handle to the worker on `stdin`. The worker never opens any file by path.
//! - The child runs under `legion_os::run` with a hard OS memory limit (`RLIMIT_AS` on Linux,
//!   `RLIMIT_DATA` on other non-macOS Unix, Job Object memory limit on Windows), a 10 ms RSS
//!   watchdog on every platform (including macOS), and a hard wall-clock timeout.
//! - The worker writes a length-prefixed binary frame (`LGTW`) to `stdout` carrying either the
//!   encoded JPEG/PNG thumbnail bytes or a structured JSON [`ErrorBody`].

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use legion_core::CancellationToken;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::run::{RunOptions, run};
use legion_os::shell::CommandSpec;
use serde_json::json;

use super::cancelled_error;
use super::thumb::{ThumbFit, decode_and_render_image_checked, select_decode_source};
use super::thumb_cache::CachedThumb;

/// Hidden subcommand flag handled before Tokio starts in `legion-cli`.
pub const THUMB_WORKER_MODE: &str = "__thumb-worker";

const WORKER_MAGIC: &[u8; 4] = b"LGTW";
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;
const MIME_JPEG_TAG: u8 = 1;
const MIME_PNG_TAG: u8 = 2;
const MAX_WORKER_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_WORKER_ERR_BYTES: usize = 16 * 1024;
/// Virtual address headroom added to `memory_limit` when computing Linux `RLIMIT_AS` for the
/// worker binary (covers binary mappings, libc, and allocator arenas; measured `legion` baseline
/// `VmPeak` is ~24 MiB).
const WORKER_RLIMIT_AS_HEADROOM: u64 = 192 * 1024 * 1024;

static WORKER_ENABLED: AtomicBool = AtomicBool::new(false);
static TEST_DECODE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

/// Enables out-of-process `__thumb-worker` decoding for this process.
///
/// Called unconditionally at startup by `legion-cli` (and by harness-less integration tests that
/// dispatch [`try_thumb_worker_cli`]). Standard Rust `#[test]` binaries do not dispatch hidden CLI
/// subcommands on `current_exe()`, so they keep the checked in-process path unless they opt in.
pub fn enable_thumb_worker_self() {
    WORKER_ENABLED.store(true, Ordering::Release);
}

pub(crate) fn worker_enabled() -> bool {
    WORKER_ENABLED.load(Ordering::Acquire)
}

/// Test-only override for the per-image decode timeout (`None` restores the production default).
#[doc(hidden)]
pub fn set_thumb_decode_timeout_for_test(timeout: Option<Duration>) {
    let ms = timeout
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1))
        .unwrap_or(0);
    TEST_DECODE_TIMEOUT_MS.store(ms, Ordering::Release);
}

pub(crate) fn effective_decode_timeout(default: Duration) -> Duration {
    match TEST_DECODE_TIMEOUT_MS.load(Ordering::Acquire) {
        0 => default,
        ms => Duration::from_millis(ms),
    }
}

/// Result of running one out-of-process image decode worker.
#[derive(Debug)]
pub(crate) enum WorkerOutcome {
    Ok(CachedThumb),
    /// The worker returned a structured error frame (corrupt file, `too_large`, etc.).
    Rejected(ErrorBody),
    /// The worker was killed by the OS memory limit or RSS watchdog (should be cached as a bad
    /// file so reopening the directory does not repeat the kill).
    KilledForMemory {
        error: ErrorBody,
        #[allow(dead_code)]
        peak_rss_bytes: u64,
    },
    /// The worker terminated abnormally without producing a valid response frame.
    Crashed(ErrorBody),
}

/// Dispatches the hidden `__thumb-worker` CLI mode if present as the first argument.
///
/// Must be called before Tokio starts in `main()`. Returns `Some(ExitCode)` when handled.
pub fn try_thumb_worker_cli(args: impl IntoIterator<Item = OsString>) -> Option<ExitCode> {
    let mut iter = args.into_iter();
    let first = iter.next()?;
    if first != THUMB_WORKER_MODE {
        return None;
    }
    let rest: Vec<OsString> = iter.collect();
    Some(run_worker_main(&rest))
}

fn run_worker_main(args: &[OsString]) -> ExitCode {
    if args.len() < 5 {
        let _ = write_err_frame(&ErrorBody::new(
            ErrorCode::Internal,
            "Invalid thumbnail worker arguments.",
        ));
        return ExitCode::from(2);
    }
    let Ok(size) = args[0].to_string_lossy().parse::<u32>() else {
        let _ = write_err_frame(&ErrorBody::new(
            ErrorCode::Internal,
            "Invalid thumbnail worker size argument.",
        ));
        return ExitCode::from(2);
    };
    let fit = match args[1].to_string_lossy().as_ref() {
        "contain" => ThumbFit::Contain,
        "cover" => ThumbFit::Cover,
        _ => {
            let _ = write_err_frame(&ErrorBody::new(
                ErrorCode::Internal,
                "Invalid thumbnail worker fit argument.",
            ));
            return ExitCode::from(2);
        }
    };
    let Ok(expected_source_bytes) = args[2].to_string_lossy().parse::<u64>() else {
        let _ = write_err_frame(&ErrorBody::new(
            ErrorCode::Internal,
            "Invalid thumbnail worker expected_source_bytes argument.",
        ));
        return ExitCode::from(2);
    };
    let Ok(max_source_bytes) = args[3].to_string_lossy().parse::<u64>() else {
        let _ = write_err_frame(&ErrorBody::new(
            ErrorCode::Internal,
            "Invalid thumbnail worker max_source_bytes argument.",
        ));
        return ExitCode::from(2);
    };
    // Display label ONLY (for error messages and extension phrasing). The worker NEVER opens any
    // path on disk; all image bytes arrive strictly through the inherited stdin file handle.
    let display_label = args[4].to_string_lossy().into_owned();
    let label_path = Path::new(&display_label);

    // Pre-allocate exact capacity (+1 for the overflow sentinel byte) so `read_to_end` never
    // triggers a 2x Vec doubling spike on large (up to 64 MiB) source files.
    let prealloc = usize::try_from(expected_source_bytes.min(max_source_bytes))
        .unwrap_or(0)
        .saturating_add(1);
    let mut bytes = Vec::with_capacity(prealloc);
    let read_res = io::stdin()
        .take(max_source_bytes.saturating_add(1))
        .read_to_end(&mut bytes);
    if let Err(err) = read_res {
        let _ = write_err_frame(&ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Thumbnail worker could not read {}: {err}.",
                label_path.display()
            ),
        ));
        return ExitCode::SUCCESS;
    }
    if bytes.len() as u64 > max_source_bytes {
        let _ = write_err_frame(
            &ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "{} grew past the 64 MiB thumbnail source limit while reading.",
                    label_path.display()
                ),
            )
            .with_hint("Retry fs.thumb once the file has finished changing."),
        );
        return ExitCode::SUCCESS;
    }

    let outcome = select_decode_source(label_path, bytes).and_then(
        |(decode_bytes, profile, override_orientation)| {
            let no_abort = AtomicBool::new(false);
            decode_and_render_image_checked(
                label_path,
                decode_bytes,
                profile,
                override_orientation,
                size,
                fit,
                &no_abort,
            )
        },
    );

    match outcome {
        Ok(thumb) => {
            if write_ok_frame(&thumb).is_err() {
                return ExitCode::from(1);
            }
        }
        Err(err) => {
            if write_err_frame(&err).is_err() {
                return ExitCode::from(1);
            }
        }
    }
    ExitCode::SUCCESS
}

fn write_ok_frame(thumb: &CachedThumb) -> io::Result<()> {
    let mime_tag = match thumb.mime {
        "image/jpeg" => MIME_JPEG_TAG,
        "image/png" => MIME_PNG_TAG,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported thumbnail mime type",
            ));
        }
    };
    let len = u32::try_from(thumb.bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "thumbnail too large"))?;
    let mut out = io::stdout().lock();
    out.write_all(WORKER_MAGIC)?;
    out.write_all(&[STATUS_OK, mime_tag])?;
    out.write_all(&thumb.width.to_le_bytes())?;
    out.write_all(&thumb.height.to_le_bytes())?;
    out.write_all(&len.to_le_bytes())?;
    out.write_all(&thumb.bytes)?;
    out.flush()
}

fn write_err_frame(err: &ErrorBody) -> io::Result<()> {
    let payload = serde_json::to_vec(err)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "error frame too large"))?;
    let mut out = io::stdout().lock();
    out.write_all(WORKER_MAGIC)?;
    out.write_all(&[STATUS_ERR])?;
    out.write_all(&len.to_le_bytes())?;
    out.write_all(&payload)?;
    out.flush()
}

fn parse_worker_frame(raw: &[u8]) -> Option<Result<CachedThumb, ErrorBody>> {
    if raw.len() < 5 || &raw[..4] != WORKER_MAGIC {
        return None;
    }
    match raw[4] {
        STATUS_OK => {
            if raw.len() < 18 {
                return None;
            }
            let mime = match raw[5] {
                MIME_JPEG_TAG => "image/jpeg",
                MIME_PNG_TAG => "image/png",
                _ => return None,
            };
            let width = u32::from_le_bytes(raw[6..10].try_into().ok()?);
            let height = u32::from_le_bytes(raw[10..14].try_into().ok()?);
            let len = u32::from_le_bytes(raw[14..18].try_into().ok()?) as usize;
            if width == 0 || height == 0 || len == 0 || len > MAX_WORKER_STDOUT_BYTES {
                return None;
            }
            let end = 18usize.checked_add(len)?;
            if raw.len() != end {
                return None;
            }
            Some(Ok(CachedThumb {
                mime,
                width,
                height,
                bytes: raw[18..end].to_vec(),
            }))
        }
        STATUS_ERR => {
            if raw.len() < 9 {
                return None;
            }
            let len = u32::from_le_bytes(raw[5..9].try_into().ok()?) as usize;
            if len == 0 || len > MAX_WORKER_ERR_BYTES {
                return None;
            }
            let end = 9usize.checked_add(len)?;
            if raw.len() != end {
                return None;
            }
            let err: ErrorBody = serde_json::from_slice(&raw[9..end]).ok()?;
            Some(Err(err))
        }
        _ => None,
    }
}

/// Runs the out-of-process `__thumb-worker` on the already-opened `file`, enforcing `memory_limit`
/// via OS limits + RSS watchdog and `timeout` via hard termination.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_thumb_worker(
    display_path: &Path,
    file: File,
    size: u32,
    fit: ThumbFit,
    expected_source_bytes: u64,
    max_source_bytes: u64,
    memory_limit: u64,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<WorkerOutcome, ErrorBody> {
    let exe = legion_core::current_exe()
        .or_else(|_| std::env::current_exe())
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Cannot locate the legion binary to start a thumbnail worker: {err}."),
            )
        })?;
    let fit_str = match fit {
        ThumbFit::Contain => "contain",
        ThumbFit::Cover => "cover",
    };

    let options = RunOptions::new(CommandSpec::Argv {
        program: exe.into_os_string(),
        args: vec![
            OsString::from(THUMB_WORKER_MODE),
            OsString::from(size.to_string()),
            OsString::from(fit_str),
            OsString::from(expected_source_bytes.to_string()),
            OsString::from(max_source_bytes.to_string()),
            display_path.as_os_str().to_os_string(),
        ],
    })
    .stdin_file(file)
    .memory_limit(memory_limit)
    .rlimit_as_headroom(WORKER_RLIMIT_AS_HEADROOM)
    .timeout(timeout)
    .kill_grace(Duration::ZERO)
    .capture_limit(MAX_WORKER_STDOUT_BYTES)
    .raw_stdout(true)
    .cancel(cancel);

    let output = run(options).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!(
                "Failed to launch thumbnail worker for {}: {err}.",
                display_path.display()
            ),
        )
    })?;

    if output.cancelled {
        return Err(cancelled_error("fs.thumb"));
    }
    if output.killed_for_memory {
        let limit_mib = memory_limit / (1024 * 1024);
        let err = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Decoding {} exceeded the {limit_mib} MiB thumbnail worker memory limit and was stopped.",
                display_path.display()
            ),
        )
        .with_hint("The image requires more memory to decode than its header indicated; open or download the original file instead.")
        .with_details(json!({
            "reason": "too_large",
            "killed_for_memory": true,
            "limit_bytes": memory_limit,
            "peak_rss_bytes": output.peak_rss_bytes,
        }));
        return Ok(WorkerOutcome::KilledForMemory {
            error: err,
            peak_rss_bytes: output.peak_rss_bytes,
        });
    }
    if output.timed_out {
        return Err(ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "Decoding a thumbnail for {} exceeded the {}-second time limit.",
                display_path.display(),
                timeout.as_secs().max(1)
            ),
        )
        .with_hint("The image may be malformed or too expensive to decode quickly."));
    }

    if let Some(parsed) = parse_worker_frame(&output.stdout_bytes) {
        return match parsed {
            Ok(thumb) => Ok(WorkerOutcome::Ok(thumb)),
            Err(err) => Ok(WorkerOutcome::Rejected(err)),
        };
    }

    // Worker died without emitting a valid frame (e.g. OOM killer signal, allocator abort, panic).
    let detail = output.stderr.text.trim();
    let summary = if !detail.is_empty() {
        detail.lines().next().unwrap_or(detail).trim().to_owned()
    } else if let Some(sig) = output.signal {
        format!("killed by signal {sig}")
    } else if let Some(code) = output.exit_code {
        format!("exited with status {code}")
    } else {
        "terminated unexpectedly".to_owned()
    };
    let err = ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "Thumbnail worker failed while decoding {}: {summary}.",
            display_path.display()
        ),
    )
    .with_hint("The image may be malformed or exceed memory limits.");
    Ok(WorkerOutcome::Crashed(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_frame_round_trips_ok_and_error_and_rejects_malformed() {
        let thumb = CachedThumb {
            mime: "image/jpeg",
            width: 128,
            height: 96,
            bytes: vec![0xFF, 0xD8, 0xFF, 0xD9],
        };
        let mut raw = Vec::new();
        raw.extend_from_slice(WORKER_MAGIC);
        raw.push(STATUS_OK);
        raw.push(MIME_JPEG_TAG);
        raw.extend_from_slice(&thumb.width.to_le_bytes());
        raw.extend_from_slice(&thumb.height.to_le_bytes());
        raw.extend_from_slice(&(thumb.bytes.len() as u32).to_le_bytes());
        raw.extend_from_slice(&thumb.bytes);
        assert_eq!(parse_worker_frame(&raw).unwrap().unwrap(), thumb);

        let err = ErrorBody::new(ErrorCode::Failed, "bad image").with_hint("try again");
        let payload = serde_json::to_vec(&err).unwrap();
        let mut err_raw = Vec::new();
        err_raw.extend_from_slice(WORKER_MAGIC);
        err_raw.push(STATUS_ERR);
        err_raw.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        err_raw.extend_from_slice(&payload);
        let parsed_err = parse_worker_frame(&err_raw).unwrap().unwrap_err();
        assert_eq!(parsed_err.code, ErrorCode::Failed);
        assert_eq!(parsed_err.message, "bad image");

        // Truncated and bad-magic frames must return None.
        assert!(parse_worker_frame(b"NOPE").is_none());
        assert!(parse_worker_frame(&raw[..raw.len() - 1]).is_none());
    }
}
