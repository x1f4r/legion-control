//! Out-of-process, memory-bounded raster image decode worker for `fs.thumb`.
//!
//! When `legion` runs in production (or inside an opt-in integration harness), image decoding is
//! isolated in a hidden `__thumb-worker` subcommand of the same binary:
//!
//! - The parent process verifies policy, opens the regular file non-blockingly once, peeks only
//!   the header prefix (`<= 128 KiB`), reserves the worker's exact resident budget from the fair
//!   in-flight pool, and hands the already-opened file handle to the worker on `stdin`. The worker
//!   opens nothing by path and starts with a cleared, minimal environment.
//! - As its very first act, the worker validates its arguments and arms
//!   [`legion_os::capped_alloc::set_memory_cap`] to its exact live-heap cap. If decoding attempts
//!   to exceed that cap on *any* OS (including macOS), the worker exits immediately with
//!   [`legion_os::capped_alloc::CAP_EXCEEDED_EXIT_CODE`] (`107`) — zero sampling gap, zero
//!   overshoot, and unambiguously distinguished from external signals.
//! - The child is additionally backed by OS limits (`RLIMIT_AS` on Linux, `RLIMIT_DATA` on other
//!   non-macOS Unix, suspended-create Job Object memory limit on Windows), a 10 ms RSS watchdog on
//!   every platform, and a hard wall-clock timeout.
//! - The worker writes a length-prefixed binary frame (`LGTW`) to `stdout` carrying either the
//!   encoded JPEG/PNG thumbnail bytes or a structured JSON [`ErrorBody`].

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use legion_core::CancellationToken;
use legion_core::proto::{ErrorBody, ErrorCode};
use legion_os::capped_alloc::{self, CAP_EXCEEDED_EXIT_CODE};
use legion_os::mem_limit::process_rss_bytes;
use legion_os::run::{RunOptions, RunOutput, run};
use legion_os::shell::CommandSpec;
use serde_json::json;

use super::audio_tags::{AudioFormat, CoverExtractMode, ParsedAudioTags, parse_audio};
use super::cancelled_error;
use super::tags::{audio_format_cli_name, parse_audio_format_cli_name};
use super::thumb::{ThumbFit, decode_and_render_image_checked, select_decode_source};
use super::thumb_cache::CachedThumb;
use super::thumb_probe::target_output_overhead_bytes;

/// Hidden subcommand flag handled before Tokio starts in `legion-cli`.
pub const THUMB_WORKER_MODE: &str = "__thumb-worker";
const PROBE_BASELINE_ARG: &str = "--probe-baseline";
const AUDIO_TAGS_ARG: &str = "--audio-tags";
const AUDIO_COVER_ARG: &str = "--audio-cover";

const BASELINE_MAGIC: &[u8; 4] = b"LGTB";
const BASELINE_FRAME_LEN: usize = 12;
const WORKER_MAGIC: &[u8; 4] = b"LGTW";
const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;
const STATUS_TAGS_OK: u8 = 2;
const STATUS_NO_COVER: u8 = 3;
const STATUS_NEEDS_CAP: u8 = 4;
const MIME_JPEG_TAG: u8 = 1;
const MIME_PNG_TAG: u8 = 2;
const MAX_WORKER_STDOUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_WORKER_ERR_BYTES: usize = 16 * 1024;
const MAX_WORKER_TAGS_BYTES: usize = 32 * 1024;
/// Hard upper bounds enforced by the worker itself on its own CLI arguments so invoking
/// `__thumb-worker` manually with absurd values (such as `4294967295`) cannot trigger huge
/// allocations before parsing fails.
const WORKER_MIN_SIZE: u32 = 96;
const WORKER_MAX_SIZE: u32 = 1024;
const WORKER_HARD_MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const WORKER_HARD_MAX_HEAP_CAP_BYTES: u64 = 512 * 1024 * 1024;
/// Minimum backstop margin above `baseline + heap_cap` for allocator bookkeeping, page-size
/// granularity (4 KiB x86 / 16 KiB Apple Silicon), Windows working-set DLL/heap accounting,
/// thread stacks, and stdin read buffers (6 MiB floor, or 10% of `heap_cap`, whichever is larger).
const BACKSTOP_MARGIN_FLOOR_BYTES: u64 = 6 * 1024 * 1024;
const MIN_MEASURED_BASELINE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MEASURED_BASELINE_BYTES: u64 = 64 * 1024 * 1024;
/// Minimum heap cap allowed for any admitted image worker (16 MiB).
pub(crate) const MIN_IMAGE_HEAP_CAP_BYTES: u64 = 16 * 1024 * 1024;
/// Upper bound for any single image worker's heap cap after applying the safety factor and floor
/// to an admitted image (`estimated_peak_bytes <= 256 MiB`), still strictly bounded by the shared
/// in-flight pool budget.
pub(crate) const MAX_WORKER_HEAP_CAP_BYTES: u64 = 384 * 1024 * 1024;
/// Safety factor (`1.35x`) and additive floor (`+8 MiB`) applied to `estimated_peak_bytes` when
/// sizing `CappedAlloc`'s exact live-heap cap. Chosen from worker-bisected peaks across the full
/// honest corpus so every honest image peaks at `<= ~70%` of its cap (<= ~75% target) while lying
/// headers still exceed their cap by 4x–45x.
pub(crate) const HEAP_CAP_SAFETY_NUM: u64 = 135;
pub(crate) const HEAP_CAP_SAFETY_DEN: u64 = 100;
pub(crate) const HEAP_CAP_FLOOR_ADD_BYTES: u64 = 8 * 1024 * 1024;

/// Converts a header-predicted `estimated_peak_bytes` into a worker `CappedAlloc` live-heap cap:
/// `clamp(estimate * 1.35 + 8 MiB, 16 MiB, 384 MiB)`.
pub(crate) fn heap_cap_from_estimate(estimated_peak_bytes: u64) -> u64 {
    estimated_peak_bytes
        .saturating_mul(HEAP_CAP_SAFETY_NUM)
        .div_ceil(HEAP_CAP_SAFETY_DEN)
        .saturating_add(HEAP_CAP_FLOOR_ADD_BYTES)
        .clamp(MIN_IMAGE_HEAP_CAP_BYTES, MAX_WORKER_HEAP_CAP_BYTES)
}
/// Generous virtual-address-space / commit backstop added above the worker's watchdog RSS limit on
/// Linux (`RLIMIT_AS`), other non-macOS Unix (`RLIMIT_DATA`), and Windows (Job Object).
/// Exact heap enforcement is performed by `CappedAlloc`; the watchdog and OS limit stay purely as
/// backstops and must never fire for a worker whose live Rust heap stayed within `heap_cap`.
const WORKER_RLIMIT_AS_HEADROOM: u64 = 256 * 1024 * 1024;

static WORKER_ENABLED: AtomicBool = AtomicBool::new(false);
static TEST_DECODE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);
static TEST_FORCE_TIMEOUT_CPU_BUSY: AtomicU8 = AtomicU8::new(0);
static BASELINE_INIT: OnceLock<u64> = OnceLock::new();
static OBSERVED_BASELINE_BYTES: AtomicU64 = AtomicU64::new(0);
static PEAK_WORKER_RSS_FOR_TEST: AtomicU64 = AtomicU64::new(0);

/// Test-only helper that resets the high-water worker peak RSS counter.
#[doc(hidden)]
pub fn reset_peak_worker_rss_for_test() {
    PEAK_WORKER_RSS_FOR_TEST.store(0, Ordering::Release);
}

/// Test-only helper returning the highest worker peak RSS observed since the last reset.
#[doc(hidden)]
pub fn peak_worker_rss_for_test() -> u64 {
    PEAK_WORKER_RSS_FOR_TEST.load(Ordering::Acquire)
}

/// Returns a per-platform fallback startup RSS estimate used only when out-of-process worker
/// probing is unavailable (for example, in unit tests without a worker binary).
const fn fallback_worker_baseline_bytes() -> u64 {
    #[cfg(all(target_os = "linux", not(debug_assertions)))]
    {
        4 * 1024 * 1024
    }
    #[cfg(all(target_os = "linux", debug_assertions))]
    {
        12 * 1024 * 1024
    }
    #[cfg(all(target_os = "macos", not(debug_assertions)))]
    {
        8 * 1024 * 1024
    }
    #[cfg(all(target_os = "macos", debug_assertions))]
    {
        16 * 1024 * 1024
    }
    #[cfg(all(windows, not(debug_assertions)))]
    {
        12 * 1024 * 1024
    }
    #[cfg(all(windows, debug_assertions))]
    {
        18 * 1024 * 1024
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        12 * 1024 * 1024
    }
}

/// Returns the backstop margin added above `baseline + heap_cap` for the RSS watchdog and OS
/// backstops: `max(6 MiB, 10% of heap_cap)`.
pub(crate) const fn backstop_margin_bytes(heap_cap_bytes: u64) -> u64 {
    let pct = heap_cap_bytes / 10;
    if pct > BACKSTOP_MARGIN_FLOOR_BYTES {
        pct
    } else {
        BACKSTOP_MARGIN_FLOOR_BYTES
    }
}

/// Probes the current binary's actual `__thumb-worker` startup resident memory (after process
/// startup and before decoding) once per process, falling back to [`fallback_worker_baseline_bytes`]
/// if out-of-process workers are disabled or the probe fails.
pub(crate) fn worker_baseline_rss_bytes() -> u64 {
    let initial = *BASELINE_INIT.get_or_init(|| {
        let probed = if worker_enabled() {
            probe_worker_baseline_once()
        } else {
            None
        };
        let baseline = probed
            .map(|b| b.clamp(MIN_MEASURED_BASELINE_BYTES, MAX_MEASURED_BASELINE_BYTES))
            .unwrap_or_else(fallback_worker_baseline_bytes);
        OBSERVED_BASELINE_BYTES.fetch_max(baseline, Ordering::Relaxed);
        baseline
    });
    OBSERVED_BASELINE_BYTES.load(Ordering::Relaxed).max(initial)
}

/// Test-only accessor returning the measured `__thumb-worker` startup baseline RSS in bytes.
#[doc(hidden)]
pub fn measured_worker_baseline_bytes_for_test() -> u64 {
    worker_baseline_rss_bytes()
}

fn probe_worker_baseline_once() -> Option<u64> {
    let exe = legion_core::current_exe()
        .or_else(|_| std::env::current_exe())
        .ok()?;
    let mut cmd = Command::new(exe);
    cmd.args([THUMB_WORKER_MODE, PROBE_BASELINE_ARG])
        .env_clear()
        .envs(minimal_system_env())
        .env("MALLOC_ARENA_MAX", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let (baseline, _) = split_baseline_preamble(&out.stdout);
    baseline.filter(|&b| b > 0)
}

/// Computes `(heap_cap_bytes, watchdog_limit_bytes, baseline_rss_bytes)` for an admitted image
/// worker so that:
/// - `watchdog_limit_bytes == baseline_rss_bytes + heap_cap_bytes + backstop_margin_bytes(heap_cap_bytes)`
/// - `watchdog_limit_bytes <= budget_bytes` (so reserving `watchdog_limit_bytes` guarantees
///   `sum(live worker limits) <= budget_bytes` strictly).
pub(crate) fn compute_worker_limits(
    requested_heap_cap_bytes: u64,
    budget_bytes: u64,
    baseline_rss_bytes: u64,
) -> (u64, u64, u64) {
    let available = budget_bytes.saturating_sub(baseline_rss_bytes);
    let max_cap_for_budget = if available <= BACKSTOP_MARGIN_FLOOR_BYTES {
        MIN_IMAGE_HEAP_CAP_BYTES
    } else {
        // Solve `cap + max(6 MiB, cap / 10) <= available` without exceeding `MAX_WORKER_HEAP_CAP_BYTES`.
        let by_pct = available.saturating_mul(10) / 11;
        let by_floor = available.saturating_sub(BACKSTOP_MARGIN_FLOOR_BYTES);
        by_pct
            .min(by_floor)
            .clamp(MIN_IMAGE_HEAP_CAP_BYTES, MAX_WORKER_HEAP_CAP_BYTES)
    };
    let heap_cap = requested_heap_cap_bytes.clamp(MIN_IMAGE_HEAP_CAP_BYTES, max_cap_for_budget);
    let watchdog_limit = baseline_rss_bytes
        .saturating_add(heap_cap)
        .saturating_add(backstop_margin_bytes(heap_cap))
        .min(budget_bytes);
    (heap_cap, watchdog_limit, baseline_rss_bytes)
}

fn platform_label() -> String {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    format!(
        "{}-{}-{profile}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

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

/// Test-only override for the timeout `cpu_busy` classification (`None` restores the real OS CPU
/// time check, `Some(true)` forces CPU-busy, `Some(false)` forces not CPU-busy).
#[doc(hidden)]
pub fn set_thumb_timeout_cpu_busy_for_test(forced: Option<bool>) {
    let val = match forced {
        None => 0,
        Some(false) => 1,
        Some(true) => 2,
    };
    TEST_FORCE_TIMEOUT_CPU_BUSY.store(val, Ordering::Release);
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
    /// The worker returned a structured error frame (corrupt file, header-probed `too_large`, etc.).
    Rejected(ErrorBody),
    /// `CappedAlloc` stopped the worker (`CAP_EXCEEDED_EXIT_CODE == 107`) because decoding exceeded
    /// its exact live-heap cap. Eligible for an expiring per-`(file, size, fit)` bad-file record.
    MemoryLimitExceeded {
        error: ErrorBody,
    },
    /// The worker exceeded the wall-clock decode timeout (not permanently cached as bad).
    TimedOut {
        error: ErrorBody,
        cpu_busy: bool,
    },
    /// The worker died for a reason NOT decided by our own limits (for example an external
    /// `SIGKILL`, `SIGTERM`, OS low-memory killer, or unexpected crash). Transient and NEVER cached
    /// as a bad file.
    TransientFailure(ErrorBody),
}

/// Result of running one out-of-process `--audio-tags` worker.
#[derive(Debug)]
pub(crate) enum AudioTagsWorkerOutcome {
    Ok(ParsedAudioTags),
    Rejected(ErrorBody),
    MemoryLimitExceeded { error: ErrorBody },
    TimedOut { error: ErrorBody, cpu_busy: bool },
    TransientFailure(ErrorBody),
}

/// Result of running one out-of-process `--audio-cover` worker.
#[derive(Debug)]
pub(crate) enum AudioCoverWorkerOutcome {
    Ok(CachedThumb),
    /// File parsed cleanly with no embedded raster cover; returns its parsed tags so the caller
    /// can populate the shared tags cache and check for a folder cover.
    NoEmbeddedCover(ParsedAudioTags),
    /// Embedded cover header probe requires a larger live-heap cap than the initial reservation.
    NeedsCap(u64),
    Rejected(ErrorBody),
    MemoryLimitExceeded {
        error: ErrorBody,
    },
    TimedOut {
        error: ErrorBody,
        cpu_busy: bool,
    },
    TransientFailure(ErrorBody),
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

#[derive(Debug)]
struct WorkerArgs {
    size: u32,
    fit: ThumbFit,
    expected_source_bytes: u64,
    max_source_bytes: u64,
    heap_cap_usize: usize,
    display_label: String,
}

fn parse_worker_args(args: &[OsString]) -> Result<WorkerArgs, ErrorBody> {
    // Expect: <size> <fit> <expected_source_bytes> <max_source_bytes> <heap_cap_bytes> <display_label>
    if args.len() < 6 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid thumbnail worker arguments.",
        ));
    }
    let size = args[0].to_string_lossy().parse::<u32>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid thumbnail worker size argument.",
        )
    })?;
    if !(WORKER_MIN_SIZE..=WORKER_MAX_SIZE).contains(&size) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Thumbnail worker size must be between {WORKER_MIN_SIZE} and {WORKER_MAX_SIZE}, got {size}."
            ),
        ));
    }
    let fit = match args[1].to_string_lossy().as_ref() {
        "contain" => ThumbFit::Contain,
        "cover" => ThumbFit::Cover,
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Invalid thumbnail worker fit argument.",
            ));
        }
    };
    let expected_source_bytes = args[2].to_string_lossy().parse::<u64>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid thumbnail worker expected_source_bytes argument.",
        )
    })?;
    let max_source_bytes = args[3].to_string_lossy().parse::<u64>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid thumbnail worker max_source_bytes argument.",
        )
    })?;
    if max_source_bytes == 0 || max_source_bytes > WORKER_HARD_MAX_SOURCE_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Thumbnail worker max_source_bytes ({max_source_bytes}) exceeds hard limit."),
        ));
    }
    if expected_source_bytes > max_source_bytes {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Thumbnail worker expected_source_bytes ({expected_source_bytes}) exceeds max_source_bytes ({max_source_bytes})."
            ),
        ));
    }
    let heap_cap_bytes = args[4].to_string_lossy().parse::<u64>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid thumbnail worker heap_cap_bytes argument.",
        )
    })?;
    if heap_cap_bytes == 0 || heap_cap_bytes > WORKER_HARD_MAX_HEAP_CAP_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Thumbnail worker heap_cap_bytes ({heap_cap_bytes}) is out of range."),
        ));
    }
    let heap_cap_usize = usize::try_from(heap_cap_bytes).map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Thumbnail worker heap_cap_bytes exceeds addressable range.",
        )
    })?;

    Ok(WorkerArgs {
        size,
        fit,
        expected_source_bytes,
        max_source_bytes,
        heap_cap_usize,
        display_label: args[5].to_string_lossy().into_owned(),
    })
}

#[derive(Debug)]
struct AudioTagsWorkerArgs {
    format: AudioFormat,
    heap_cap_usize: usize,
    display_label: String,
}

fn parse_audio_tags_worker_args(args: &[OsString]) -> Result<AudioTagsWorkerArgs, ErrorBody> {
    // Expect: <format> <heap_cap_bytes> <display_label>
    if args.len() < 3 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid audio tag worker arguments.",
        ));
    }
    let format =
        parse_audio_format_cli_name(args[0].to_string_lossy().as_ref()).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Invalid audio worker format argument.",
            )
        })?;
    let heap_cap_bytes = args[1].to_string_lossy().parse::<u64>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid audio tag worker heap_cap_bytes argument.",
        )
    })?;
    if heap_cap_bytes == 0 || heap_cap_bytes > WORKER_HARD_MAX_HEAP_CAP_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Audio tag worker heap_cap_bytes ({heap_cap_bytes}) is out of range."),
        ));
    }
    let heap_cap_usize = usize::try_from(heap_cap_bytes).map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Audio tag worker heap_cap_bytes exceeds addressable range.",
        )
    })?;
    Ok(AudioTagsWorkerArgs {
        format,
        heap_cap_usize,
        display_label: args[2].to_string_lossy().into_owned(),
    })
}

#[derive(Debug)]
struct AudioCoverWorkerArgs {
    format: AudioFormat,
    size: u32,
    fit: ThumbFit,
    heap_cap_bytes: u64,
    heap_cap_usize: usize,
    allow_needs_cap: bool,
    display_label: String,
}

fn parse_audio_cover_worker_args(args: &[OsString]) -> Result<AudioCoverWorkerArgs, ErrorBody> {
    // Expect: <format> <size> <fit> <heap_cap_bytes> <allow_needs_cap> <display_label>
    if args.len() < 6 {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid audio cover worker arguments.",
        ));
    }
    let format =
        parse_audio_format_cli_name(args[0].to_string_lossy().as_ref()).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Invalid audio cover format argument.",
            )
        })?;
    let size = args[1].to_string_lossy().parse::<u32>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid audio cover worker size argument.",
        )
    })?;
    if !(WORKER_MIN_SIZE..=WORKER_MAX_SIZE).contains(&size) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Audio cover worker size must be between {WORKER_MIN_SIZE} and {WORKER_MAX_SIZE}, got {size}."
            ),
        ));
    }
    let fit = match args[2].to_string_lossy().as_ref() {
        "contain" => ThumbFit::Contain,
        "cover" => ThumbFit::Cover,
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Invalid audio cover worker fit argument.",
            ));
        }
    };
    let heap_cap_bytes = args[3].to_string_lossy().parse::<u64>().map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Invalid audio cover worker heap_cap_bytes argument.",
        )
    })?;
    if heap_cap_bytes == 0 || heap_cap_bytes > WORKER_HARD_MAX_HEAP_CAP_BYTES {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!("Audio cover worker heap_cap_bytes ({heap_cap_bytes}) is out of range."),
        ));
    }
    let heap_cap_usize = usize::try_from(heap_cap_bytes).map_err(|_| {
        ErrorBody::new(
            ErrorCode::InvalidArgs,
            "Audio cover worker heap_cap_bytes exceeds addressable range.",
        )
    })?;
    let allow_needs_cap = match args[4].to_string_lossy().as_ref() {
        "0" => false,
        "1" => true,
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::InvalidArgs,
                "Invalid audio cover worker allow_needs_cap argument.",
            ));
        }
    };
    Ok(AudioCoverWorkerArgs {
        format,
        size,
        fit,
        heap_cap_bytes,
        heap_cap_usize,
        allow_needs_cap,
        display_label: args[5].to_string_lossy().into_owned(),
    })
}

/// Duplicates the already-opened regular file handle inherited on `stdin` into a seekable [`File`]
/// without using `unsafe` and without opening any path on disk.
fn open_inherited_stdin_regular_file(label_path: &Path) -> Result<(File, u64), ErrorBody> {
    let cloned: io::Result<File> = {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd as _;
            io::stdin().as_fd().try_clone_to_owned().map(File::from)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle as _;
            io::stdin().as_handle().try_clone_to_owned().map(File::from)
        }
    };
    let mut file = cloned.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Audio worker could not access inherited file handle for {}: {err}.",
                label_path.display()
            ),
        )
    })?;
    let meta = file.metadata().map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Audio worker could not inspect inherited handle for {}: {err}.",
                label_path.display()
            ),
        )
    })?;
    if !meta.is_file() {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "Inherited handle for {} is not a regular file.",
                label_path.display()
            ),
        ));
    }
    file.seek(SeekFrom::Start(0)).map_err(|err| {
        ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Audio worker could not seek {}: {err}.",
                label_path.display()
            ),
        )
    })?;
    Ok((file, meta.len()))
}

fn run_worker_main(args: &[OsString]) -> ExitCode {
    if args.first().is_some_and(|a| a == PROBE_BASELINE_ARG) {
        let baseline = process_rss_bytes(std::process::id()).unwrap_or(0);
        let _ = write_baseline_frame(baseline);
        return ExitCode::SUCCESS;
    }
    if args.first().is_some_and(|a| a == AUDIO_TAGS_ARG) {
        return run_audio_tags_worker_main(&args[1..]);
    }
    if args.first().is_some_and(|a| a == AUDIO_COVER_ARG) {
        return run_audio_cover_worker_main(&args[1..]);
    }

    let parsed = match parse_worker_args(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            let _ = write_err_frame(&err);
            return ExitCode::from(2);
        }
    };
    let WorkerArgs {
        size,
        fit,
        expected_source_bytes,
        max_source_bytes,
        heap_cap_usize,
        display_label,
    } = parsed;

    // Sample this worker's own startup resident size after initialization and before decoding, and
    // emit it as a 12-byte preamble frame (`LGTB` + u64) so the parent can track and log the real
    // per-platform/per-build baseline.
    let startup_rss = process_rss_bytes(std::process::id()).unwrap_or(0);
    let _ = write_baseline_frame(startup_rss);

    // Arm the exact process-wide live-heap allocator cap BEFORE touching stdin or allocating
    // buffers for untrusted input.
    capped_alloc::set_memory_cap(heap_cap_usize);

    // Display label ONLY (for error messages and extension phrasing). The worker NEVER opens any
    // path on disk; all image bytes arrive strictly through the inherited stdin file handle.
    let label_path = Path::new(&display_label);

    // Pre-allocate exact capacity (+1 for the overflow sentinel byte) so `read_to_end` never
    // triggers a 2x Vec doubling spike on large (up to 64 MiB) source files.
    let prealloc = usize::try_from(expected_source_bytes)
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

fn run_audio_tags_worker_main(args: &[OsString]) -> ExitCode {
    let parsed = match parse_audio_tags_worker_args(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            let _ = write_err_frame(&err);
            return ExitCode::from(2);
        }
    };
    let startup_rss = process_rss_bytes(std::process::id()).unwrap_or(0);
    let _ = write_baseline_frame(startup_rss);

    capped_alloc::set_memory_cap(parsed.heap_cap_usize);

    let label_path = Path::new(&parsed.display_label);
    let (mut file, file_len) = match open_inherited_stdin_regular_file(label_path) {
        Ok(pair) => pair,
        Err(err) => {
            let _ = write_err_frame(&err);
            return ExitCode::SUCCESS;
        }
    };

    match parse_audio(
        &mut file,
        file_len,
        parsed.format,
        CoverExtractMode::CheckOnly,
        label_path,
    ) {
        Ok(extracted) => {
            if write_tags_frame(STATUS_TAGS_OK, &extracted.tags).is_err() {
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

fn run_audio_cover_worker_main(args: &[OsString]) -> ExitCode {
    let parsed = match parse_audio_cover_worker_args(args) {
        Ok(parsed) => parsed,
        Err(err) => {
            let _ = write_err_frame(&err);
            return ExitCode::from(2);
        }
    };
    let startup_rss = process_rss_bytes(std::process::id()).unwrap_or(0);
    let _ = write_baseline_frame(startup_rss);

    capped_alloc::set_memory_cap(parsed.heap_cap_usize);

    let label_path = Path::new(&parsed.display_label);
    let (mut file, file_len) = match open_inherited_stdin_regular_file(label_path) {
        Ok(pair) => pair,
        Err(err) => {
            let _ = write_err_frame(&err);
            return ExitCode::SUCCESS;
        }
    };

    let extracted = match parse_audio(
        &mut file,
        file_len,
        parsed.format,
        CoverExtractMode::ExtractBytes,
        label_path,
    ) {
        Ok(extracted) => extracted,
        Err(err) => {
            if write_err_frame(&err).is_err() {
                return ExitCode::from(1);
            }
            return ExitCode::SUCCESS;
        }
    };
    drop(file);

    let Some(cover_bytes) = extracted.cover else {
        if write_tags_frame(STATUS_NO_COVER, &extracted.tags).is_err() {
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    };
    drop(extracted.tags);

    let (decode_bytes, profile, override_orientation) =
        match select_decode_source(label_path, cover_bytes) {
            Ok(selected) => selected,
            Err(err) => {
                if write_err_frame(&err).is_err() {
                    return ExitCode::from(1);
                }
                return ExitCode::SUCCESS;
            }
        };

    let needed_cap = heap_cap_from_estimate(
        profile
            .estimated_peak_bytes
            .saturating_add(target_output_overhead_bytes(parsed.size)),
    );
    if needed_cap > parsed.heap_cap_bytes {
        if parsed.allow_needs_cap {
            if write_needs_cap_frame(needed_cap).is_err() {
                return ExitCode::from(1);
            }
            return ExitCode::SUCCESS;
        }
    } else if let Ok(tight) = usize::try_from(needed_cap) {
        // Tighten CappedAlloc down to the exact cover probe estimate before decoding so a lying
        // embedded cover is caught at the same tight live-heap cap as a standalone picture.
        capped_alloc::tighten_memory_cap(tight);
    }

    let no_abort = AtomicBool::new(false);
    match decode_and_render_image_checked(
        label_path,
        decode_bytes,
        profile,
        override_orientation,
        parsed.size,
        parsed.fit,
        &no_abort,
    ) {
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

fn write_baseline_frame(baseline_rss_bytes: u64) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(BASELINE_MAGIC)?;
    out.write_all(&baseline_rss_bytes.to_le_bytes())?;
    out.flush()
}

fn split_baseline_preamble(raw: &[u8]) -> (Option<u64>, &[u8]) {
    if raw.len() >= BASELINE_FRAME_LEN && &raw[..4] == BASELINE_MAGIC {
        let baseline = u64::from_le_bytes([
            raw[4], raw[5], raw[6], raw[7], raw[8], raw[9], raw[10], raw[11],
        ]);
        (Some(baseline), &raw[BASELINE_FRAME_LEN..])
    } else {
        (None, raw)
    }
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

fn write_tags_frame(status: u8, tags: &ParsedAudioTags) -> io::Result<()> {
    let payload = serde_json::to_vec(tags)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    if payload.len() > MAX_WORKER_TAGS_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "tags frame too large",
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tags frame too large"))?;
    let mut out = io::stdout().lock();
    out.write_all(WORKER_MAGIC)?;
    out.write_all(&[status])?;
    out.write_all(&len.to_le_bytes())?;
    out.write_all(&payload)?;
    out.flush()
}

fn write_needs_cap_frame(needed_cap: u64) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(WORKER_MAGIC)?;
    out.write_all(&[STATUS_NEEDS_CAP])?;
    out.write_all(&needed_cap.to_le_bytes())?;
    out.flush()
}

enum ParsedWorkerFrame {
    Thumb(CachedThumb),
    Err(ErrorBody),
    Tags(ParsedAudioTags),
    NoCover(ParsedAudioTags),
    NeedsCap(u64),
}

fn parse_worker_frame_any(raw: &[u8]) -> Option<ParsedWorkerFrame> {
    let (_, body) = split_baseline_preamble(raw);
    if body.len() < 5 || &body[..4] != WORKER_MAGIC {
        return None;
    }
    match body[4] {
        STATUS_OK => {
            if body.len() < 18 {
                return None;
            }
            let mime = match body[5] {
                MIME_JPEG_TAG => "image/jpeg",
                MIME_PNG_TAG => "image/png",
                _ => return None,
            };
            let width = u32::from_le_bytes(body[6..10].try_into().ok()?);
            let height = u32::from_le_bytes(body[10..14].try_into().ok()?);
            let len = u32::from_le_bytes(body[14..18].try_into().ok()?) as usize;
            if width == 0 || height == 0 || len == 0 || len > MAX_WORKER_STDOUT_BYTES {
                return None;
            }
            let end = 18usize.checked_add(len)?;
            if body.len() != end {
                return None;
            }
            Some(ParsedWorkerFrame::Thumb(CachedThumb {
                mime,
                width,
                height,
                bytes: body[18..end].to_vec(),
            }))
        }
        STATUS_ERR => {
            if body.len() < 9 {
                return None;
            }
            let len = u32::from_le_bytes(body[5..9].try_into().ok()?) as usize;
            if len == 0 || len > MAX_WORKER_ERR_BYTES {
                return None;
            }
            let end = 9usize.checked_add(len)?;
            if body.len() != end {
                return None;
            }
            let err: ErrorBody = serde_json::from_slice(&body[9..end]).ok()?;
            Some(ParsedWorkerFrame::Err(err))
        }
        STATUS_TAGS_OK | STATUS_NO_COVER => {
            if body.len() < 9 {
                return None;
            }
            let len = u32::from_le_bytes(body[5..9].try_into().ok()?) as usize;
            if len == 0 || len > MAX_WORKER_TAGS_BYTES {
                return None;
            }
            let end = 9usize.checked_add(len)?;
            if body.len() != end {
                return None;
            }
            let tags: ParsedAudioTags = serde_json::from_slice(&body[9..end]).ok()?;
            if body[4] == STATUS_TAGS_OK {
                Some(ParsedWorkerFrame::Tags(tags))
            } else {
                Some(ParsedWorkerFrame::NoCover(tags))
            }
        }
        STATUS_NEEDS_CAP => {
            if body.len() != 13 {
                return None;
            }
            let needed = u64::from_le_bytes(body[5..13].try_into().ok()?);
            if needed == 0 || needed > WORKER_HARD_MAX_HEAP_CAP_BYTES {
                return None;
            }
            Some(ParsedWorkerFrame::NeedsCap(needed))
        }
        _ => None,
    }
}

fn parse_worker_frame(raw: &[u8]) -> Option<Result<CachedThumb, ErrorBody>> {
    match parse_worker_frame_any(raw)? {
        ParsedWorkerFrame::Thumb(thumb) => Some(Ok(thumb)),
        ParsedWorkerFrame::Err(err) => Some(Err(err)),
        _ => None,
    }
}

/// Minimal safe environment variables needed to start a child process without inheriting node
/// secrets, credentials, or arbitrary user environment.
pub(crate) fn minimal_system_env() -> Vec<(OsString, OsString)> {
    #[cfg(windows)]
    {
        let mut vars = Vec::new();
        for key in ["SYSTEMROOT", "WINDIR", "SYSTEMDRIVE", "PATHEXT"] {
            if let Some(val) = std::env::var_os(key) {
                vars.push((OsString::from(key), val));
            }
        }
        vars
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// Runs the out-of-process `__thumb-worker` on the already-opened `file`, enforcing an exact
/// `heap_cap_bytes` via [`capped_alloc`] plus `rss_limit_bytes` (`baseline + heap_cap + margin`)
/// via the RSS watchdog and OS backstops, and `timeout` via hard termination.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_thumb_worker(
    display_path: &Path,
    file: File,
    size: u32,
    fit: ThumbFit,
    expected_source_bytes: u64,
    max_source_bytes: u64,
    heap_cap_bytes: u64,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
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

    let mut options = RunOptions::new(CommandSpec::Argv {
        program: exe.into_os_string(),
        args: vec![
            OsString::from(THUMB_WORKER_MODE),
            OsString::from(size.to_string()),
            OsString::from(fit_str),
            OsString::from(expected_source_bytes.to_string()),
            OsString::from(max_source_bytes.to_string()),
            OsString::from(heap_cap_bytes.to_string()),
            display_path.as_os_str().to_os_string(),
        ],
    })
    .env_clear(true)
    .envs(minimal_system_env())
    .stdin_file(file)
    .memory_limit(rss_limit_bytes)
    .rlimit_as_headroom(WORKER_RLIMIT_AS_HEADROOM)
    .timeout(timeout)
    .kill_grace(Duration::ZERO)
    .capture_limit(MAX_WORKER_STDOUT_BYTES)
    .raw_stdout(true)
    .cancel(cancel);
    // Keep glibc single-arena lean inside the worker as well.
    options = options.env("MALLOC_ARENA_MAX", "2");

    let output = run(options).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!(
                "Failed to launch thumbnail worker for {}: {err}.",
                display_path.display()
            ),
        )
    })?;
    PEAK_WORKER_RSS_FOR_TEST.fetch_max(output.peak_rss_bytes, Ordering::Relaxed);

    classify_worker_output(
        display_path,
        &output,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
        timeout,
    )
}

/// Runs the out-of-process `__thumb-worker --audio-tags` on the already-opened audio `file`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_audio_tags_worker(
    display_path: &Path,
    file: File,
    format: AudioFormat,
    heap_cap_bytes: u64,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<AudioTagsWorkerOutcome, ErrorBody> {
    let exe = legion_core::current_exe()
        .or_else(|_| std::env::current_exe())
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Cannot locate the legion binary to start an audio tag worker: {err}."),
            )
        })?;
    let mut options = RunOptions::new(CommandSpec::Argv {
        program: exe.into_os_string(),
        args: vec![
            OsString::from(THUMB_WORKER_MODE),
            OsString::from(AUDIO_TAGS_ARG),
            OsString::from(audio_format_cli_name(format)),
            OsString::from(heap_cap_bytes.to_string()),
            display_path.as_os_str().to_os_string(),
        ],
    })
    .env_clear(true)
    .envs(minimal_system_env())
    .stdin_file(file)
    .memory_limit(rss_limit_bytes)
    .rlimit_as_headroom(WORKER_RLIMIT_AS_HEADROOM)
    .timeout(timeout)
    .kill_grace(Duration::ZERO)
    .capture_limit(MAX_WORKER_TAGS_BYTES + 1024)
    .raw_stdout(true)
    .cancel(cancel);
    options = options.env("MALLOC_ARENA_MAX", "2");

    let output = run(options).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!(
                "Failed to launch audio tag worker for {}: {err}.",
                display_path.display()
            ),
        )
    })?;
    PEAK_WORKER_RSS_FOR_TEST.fetch_max(output.peak_rss_bytes, Ordering::Relaxed);

    classify_audio_tags_output(
        display_path,
        &output,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
        timeout,
    )
}

/// Runs the out-of-process `__thumb-worker --audio-cover` on the already-opened audio `file`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_audio_cover_worker(
    display_path: &Path,
    file: File,
    format: AudioFormat,
    size: u32,
    fit: ThumbFit,
    heap_cap_bytes: u64,
    allow_needs_cap: bool,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
    timeout: Duration,
    cancel: CancellationToken,
) -> Result<AudioCoverWorkerOutcome, ErrorBody> {
    let exe = legion_core::current_exe()
        .or_else(|_| std::env::current_exe())
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("Cannot locate the legion binary to start an audio cover worker: {err}."),
            )
        })?;
    let fit_str = match fit {
        ThumbFit::Contain => "contain",
        ThumbFit::Cover => "cover",
    };
    let mut options = RunOptions::new(CommandSpec::Argv {
        program: exe.into_os_string(),
        args: vec![
            OsString::from(THUMB_WORKER_MODE),
            OsString::from(AUDIO_COVER_ARG),
            OsString::from(audio_format_cli_name(format)),
            OsString::from(size.to_string()),
            OsString::from(fit_str),
            OsString::from(heap_cap_bytes.to_string()),
            OsString::from(if allow_needs_cap { "1" } else { "0" }),
            display_path.as_os_str().to_os_string(),
        ],
    })
    .env_clear(true)
    .envs(minimal_system_env())
    .stdin_file(file)
    .memory_limit(rss_limit_bytes)
    .rlimit_as_headroom(WORKER_RLIMIT_AS_HEADROOM)
    .timeout(timeout)
    .kill_grace(Duration::ZERO)
    .capture_limit(MAX_WORKER_STDOUT_BYTES)
    .raw_stdout(true)
    .cancel(cancel);
    options = options.env("MALLOC_ARENA_MAX", "2");

    let output = run(options).await.map_err(|err| {
        ErrorBody::new(
            ErrorCode::Internal,
            format!(
                "Failed to launch audio cover worker for {}: {err}.",
                display_path.display()
            ),
        )
    })?;
    PEAK_WORKER_RSS_FOR_TEST.fetch_max(output.peak_rss_bytes, Ordering::Relaxed);

    classify_audio_cover_output(
        display_path,
        &output,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
        timeout,
    )
}

enum CommonWorkerStop {
    TimedOut { error: ErrorBody, cpu_busy: bool },
    MemoryLimitExceeded { error: ErrorBody },
    TransientFailure(ErrorBody),
}

fn classify_common_worker_stop(
    tool: &'static str,
    display_path: &Path,
    output: &RunOutput,
    heap_cap_bytes: u64,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
    timeout: Duration,
) -> Result<Option<CommonWorkerStop>, ErrorBody> {
    if output.cancelled {
        return Err(cancelled_error(tool));
    }

    let (worker_baseline_rss, _) = split_baseline_preamble(&output.stdout_bytes);
    if let Some(observed) = worker_baseline_rss
        && (MIN_MEASURED_BASELINE_BYTES..=MAX_MEASURED_BASELINE_BYTES).contains(&observed)
    {
        OBSERVED_BASELINE_BYTES.fetch_max(observed, Ordering::Relaxed);
    }

    if output.timed_out {
        let cpu_busy = match TEST_FORCE_TIMEOUT_CPU_BUSY.load(Ordering::Acquire) {
            1 => false,
            2 => true,
            _ => output
                .cpu_time
                .is_some_and(|cpu| cpu.mul_f32(2.0) >= timeout),
        };
        let err = ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "Worker processing {} exceeded the {}-second time limit.",
                display_path.display(),
                timeout.as_secs().max(1)
            ),
        )
        .with_hint("The file may be malformed or too expensive to process quickly.");
        return Ok(Some(CommonWorkerStop::TimedOut {
            error: err,
            cpu_busy,
        }));
    }

    let platform = platform_label();

    if output.exit_code == Some(CAP_EXCEEDED_EXIT_CODE) {
        let cap_mib = heap_cap_bytes / (1024 * 1024);
        let err = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Processing {} needed more memory than the {cap_mib} MiB worker limit and was stopped.",
                display_path.display()
            ),
        )
        .with_hint("This file needed more memory than Legion allows for media inspection; open or download the original file instead.")
        .with_details(json!({
            "reason": "too_large",
            "killed_for_memory": true,
            "enforced_by": "allocator_cap",
            "platform": platform,
            "baseline_rss_bytes": baseline_rss_bytes,
            "worker_baseline_rss_bytes": worker_baseline_rss,
            "heap_cap_bytes": heap_cap_bytes,
            "limit_bytes": rss_limit_bytes,
            "peak_rss_bytes": output.peak_rss_bytes,
        }));
        return Ok(Some(CommonWorkerStop::MemoryLimitExceeded { error: err }));
    }

    if output.killed_for_memory {
        let enforced_by = if output.memory_watchdog_killed {
            "rss_watchdog"
        } else {
            "os_limit"
        };
        let _ = writeln!(
            io::stderr(),
            "legion: {tool} backstop ({enforced_by}) stopped worker under heap cap for {}: platform={platform} baseline={baseline_rss_bytes}B worker_baseline={worker_baseline_rss:?}B cap={heap_cap_bytes}B limit={rss_limit_bytes}B peak={}B",
            display_path.display(),
            output.peak_rss_bytes,
        );
        let err = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Worker for {} hit the {enforced_by} backstop while within its heap cap.",
                display_path.display()
            ),
        )
        .with_hint("This was not caused by the file exceeding its heap cap; the file was not marked bad. Retry shortly.")
        .with_details(json!({
            "reason": "backstop_limit",
            "transient": true,
            "enforced_by": enforced_by,
            "platform": platform,
            "baseline_rss_bytes": baseline_rss_bytes,
            "worker_baseline_rss_bytes": worker_baseline_rss,
            "heap_cap_bytes": heap_cap_bytes,
            "limit_bytes": rss_limit_bytes,
            "peak_rss_bytes": output.peak_rss_bytes,
        }));
        return Ok(Some(CommonWorkerStop::TransientFailure(err)));
    }

    Ok(None)
}

fn unexpected_worker_stop_error(tool: &str, display_path: &Path, output: &RunOutput) -> ErrorBody {
    let detail = output.stderr.text.trim();
    let summary = if let Some(sig) = output.signal {
        format!("terminated by signal {sig}")
    } else if !detail.is_empty() {
        detail.lines().next().unwrap_or(detail).trim().to_owned()
    } else if let Some(code) = output.exit_code {
        format!("exited with status {code}")
    } else {
        "terminated unexpectedly".to_owned()
    };
    ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "Media worker for {} stopped unexpectedly ({summary}).",
            display_path.display()
        ),
    )
    .with_hint(format!(
        "This failure was not caused by the memory limit; retry {tool}."
    ))
}

fn classify_audio_tags_output(
    display_path: &Path,
    output: &RunOutput,
    heap_cap_bytes: u64,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
    timeout: Duration,
) -> Result<AudioTagsWorkerOutcome, ErrorBody> {
    if let Some(stop) = classify_common_worker_stop(
        "fs.tags",
        display_path,
        output,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
        timeout,
    )? {
        return Ok(match stop {
            CommonWorkerStop::TimedOut { error, cpu_busy } => {
                AudioTagsWorkerOutcome::TimedOut { error, cpu_busy }
            }
            CommonWorkerStop::MemoryLimitExceeded { error } => {
                AudioTagsWorkerOutcome::MemoryLimitExceeded { error }
            }
            CommonWorkerStop::TransientFailure(err) => {
                AudioTagsWorkerOutcome::TransientFailure(err)
            }
        });
    }
    match parse_worker_frame_any(&output.stdout_bytes) {
        Some(ParsedWorkerFrame::Tags(tags)) => Ok(AudioTagsWorkerOutcome::Ok(tags)),
        Some(ParsedWorkerFrame::Err(err)) => Ok(AudioTagsWorkerOutcome::Rejected(err)),
        _ => Ok(AudioTagsWorkerOutcome::TransientFailure(
            unexpected_worker_stop_error("fs.tags", display_path, output),
        )),
    }
}

fn classify_audio_cover_output(
    display_path: &Path,
    output: &RunOutput,
    heap_cap_bytes: u64,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
    timeout: Duration,
) -> Result<AudioCoverWorkerOutcome, ErrorBody> {
    if let Some(stop) = classify_common_worker_stop(
        "fs.thumb",
        display_path,
        output,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
        timeout,
    )? {
        return Ok(match stop {
            CommonWorkerStop::TimedOut { error, cpu_busy } => {
                AudioCoverWorkerOutcome::TimedOut { error, cpu_busy }
            }
            CommonWorkerStop::MemoryLimitExceeded { error } => {
                AudioCoverWorkerOutcome::MemoryLimitExceeded { error }
            }
            CommonWorkerStop::TransientFailure(err) => {
                AudioCoverWorkerOutcome::TransientFailure(err)
            }
        });
    }
    match parse_worker_frame_any(&output.stdout_bytes) {
        Some(ParsedWorkerFrame::Thumb(thumb)) => Ok(AudioCoverWorkerOutcome::Ok(thumb)),
        Some(ParsedWorkerFrame::NoCover(tags)) => {
            Ok(AudioCoverWorkerOutcome::NoEmbeddedCover(tags))
        }
        Some(ParsedWorkerFrame::NeedsCap(cap)) => Ok(AudioCoverWorkerOutcome::NeedsCap(cap)),
        Some(ParsedWorkerFrame::Err(err)) => Ok(AudioCoverWorkerOutcome::Rejected(err)),
        _ => Ok(AudioCoverWorkerOutcome::TransientFailure(
            unexpected_worker_stop_error("fs.thumb", display_path, output),
        )),
    }
}

fn classify_worker_output(
    display_path: &Path,
    output: &RunOutput,
    heap_cap_bytes: u64,
    rss_limit_bytes: u64,
    baseline_rss_bytes: u64,
    timeout: Duration,
) -> Result<WorkerOutcome, ErrorBody> {
    if output.cancelled {
        return Err(cancelled_error("fs.thumb"));
    }

    let (worker_baseline_rss, _) = split_baseline_preamble(&output.stdout_bytes);
    if let Some(observed) = worker_baseline_rss
        && (MIN_MEASURED_BASELINE_BYTES..=MAX_MEASURED_BASELINE_BYTES).contains(&observed)
    {
        OBSERVED_BASELINE_BYTES.fetch_max(observed, Ordering::Relaxed);
    }

    if output.timed_out {
        let cpu_busy = match TEST_FORCE_TIMEOUT_CPU_BUSY.load(Ordering::Acquire) {
            1 => false,
            2 => true,
            _ => output
                .cpu_time
                .is_some_and(|cpu| cpu.mul_f32(2.0) >= timeout),
        };
        let err = ErrorBody::new(
            ErrorCode::Timeout,
            format!(
                "Decoding a thumbnail for {} exceeded the {}-second time limit.",
                display_path.display(),
                timeout.as_secs().max(1)
            ),
        )
        .with_hint("The image may be malformed or too expensive to decode quickly.");
        return Ok(WorkerOutcome::TimedOut {
            error: err,
            cpu_busy,
        });
    }

    let platform = platform_label();

    // `CappedAlloc` (`CAP_EXCEEDED_EXIT_CODE == 107`) is the exact deciding limit for image
    // workers: only when the allocator itself stopped the worker for exceeding `heap_cap_bytes` is
    // the file definitely at fault and eligible for expiring bad-file caching.
    if output.exit_code == Some(CAP_EXCEEDED_EXIT_CODE) {
        let cap_mib = heap_cap_bytes / (1024 * 1024);
        let err = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Decoding {} needed more memory than the {cap_mib} MiB thumbnail limit and was stopped.",
                display_path.display()
            ),
        )
        .with_hint("This image needed more memory than Legion allows for a thumbnail; open or download the original file instead.")
        .with_details(json!({
            "reason": "too_large",
            "killed_for_memory": true,
            "enforced_by": "allocator_cap",
            "platform": platform,
            "baseline_rss_bytes": baseline_rss_bytes,
            "worker_baseline_rss_bytes": worker_baseline_rss,
            "heap_cap_bytes": heap_cap_bytes,
            "limit_bytes": rss_limit_bytes,
            "peak_rss_bytes": output.peak_rss_bytes,
        }));
        return Ok(WorkerOutcome::MemoryLimitExceeded { error: err });
    }

    // A backstop kill (RSS watchdog or OS limit) while `CappedAlloc` stayed under `heap_cap_bytes`
    // is NOT the file's fault: return a transient error, NEVER record the file as bad in cache,
    // and emit full diagnostics (baseline, cap, limit, peak, platform).
    if output.killed_for_memory {
        let enforced_by = if output.memory_watchdog_killed {
            "rss_watchdog"
        } else {
            "os_limit"
        };
        let _ = writeln!(
            io::stderr(),
            "legion: fs.thumb backstop ({enforced_by}) stopped worker under heap cap for {}: platform={platform} baseline={baseline_rss_bytes}B worker_baseline={worker_baseline_rss:?}B cap={heap_cap_bytes}B limit={rss_limit_bytes}B peak={}B",
            display_path.display(),
            output.peak_rss_bytes,
        );
        let err = ErrorBody::new(
            ErrorCode::Failed,
            format!(
                "Thumbnail worker for {} hit the {enforced_by} backstop while within its heap cap.",
                display_path.display()
            ),
        )
        .with_hint("This was not caused by the image exceeding its heap cap; the file was not marked bad. Retry fs.thumb.")
        .with_details(json!({
            "reason": "backstop_limit",
            "transient": true,
            "enforced_by": enforced_by,
            "platform": platform,
            "baseline_rss_bytes": baseline_rss_bytes,
            "worker_baseline_rss_bytes": worker_baseline_rss,
            "heap_cap_bytes": heap_cap_bytes,
            "limit_bytes": rss_limit_bytes,
            "peak_rss_bytes": output.peak_rss_bytes,
        }));
        return Ok(WorkerOutcome::TransientFailure(err));
    }

    if let Some(parsed) = parse_worker_frame(&output.stdout_bytes) {
        return match parsed {
            Ok(thumb) => Ok(WorkerOutcome::Ok(thumb)),
            Err(err) => Ok(WorkerOutcome::Rejected(err)),
        };
    }

    // Worker died for an external or unexpected reason (e.g. external SIGKILL/SIGTERM, host OOM
    // killer under unrelated memory pressure, or crash). Report truthfully and do NOT mark the
    // file bad in cache.
    let detail = output.stderr.text.trim();
    let summary = if let Some(sig) = output.signal {
        format!("terminated by signal {sig}")
    } else if !detail.is_empty() {
        detail.lines().next().unwrap_or(detail).trim().to_owned()
    } else if let Some(code) = output.exit_code {
        format!("exited with status {code}")
    } else {
        "terminated unexpectedly".to_owned()
    };
    let err = ErrorBody::new(
        ErrorCode::Failed,
        format!(
            "Thumbnail worker for {} stopped unexpectedly ({summary}).",
            display_path.display()
        ),
    )
    .with_hint("This failure was not caused by the thumbnail memory limit; retry fs.thumb.");
    Ok(WorkerOutcome::TransientFailure(err))
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

    #[test]
    fn worker_cli_rejects_absurd_arguments_without_allocating() {
        // Reproduces manual invocation with u32::MAX size / expected bytes / absurd heap cap.
        let err1 = parse_worker_args(&[
            OsString::from("4294967295"),
            OsString::from("contain"),
            OsString::from("4294967295"),
            OsString::from("67108864"),
            OsString::from("67108864"),
            OsString::from("/bogus.jpg"),
        ])
        .unwrap_err();
        assert_eq!(err1.code, ErrorCode::InvalidArgs);

        let err2 = parse_worker_args(&[
            OsString::from("256"),
            OsString::from("contain"),
            OsString::from("4294967295"),
            OsString::from("67108864"),
            OsString::from("67108864"),
            OsString::from("/bogus.jpg"),
        ])
        .unwrap_err();
        assert_eq!(err2.code, ErrorCode::InvalidArgs);

        let err3 = parse_worker_args(&[
            OsString::from("256"),
            OsString::from("contain"),
            OsString::from("1024"),
            OsString::from("67108864"),
            OsString::from("999999999999"),
            OsString::from("/bogus.jpg"),
        ])
        .unwrap_err();
        assert_eq!(err3.code, ErrorCode::InvalidArgs);
    }

    #[test]
    fn worker_limits_respect_measured_baseline_margin_and_budget() {
        let budget_512 = 512 * 1024 * 1024;
        // Linux release baseline (~4 MiB) + 43.3 MiB 12 MP heap cap + 6 MiB floor = 53.3 MiB -> 9 fit
        let (cap, limit, base) =
            compute_worker_limits(44 * 1024 * 1024, budget_512, 4 * 1024 * 1024);
        assert_eq!(base, 4 * 1024 * 1024);
        assert_eq!(cap, 44 * 1024 * 1024);
        assert_eq!(limit, 54 * 1024 * 1024);
        assert_eq!(budget_512 / limit, 9);

        // Windows debug baseline (~14 MiB) + 132 MiB padded file cap + 13.2 MiB 10% margin = 159.2 MiB
        let (cap_w, limit_w, _) =
            compute_worker_limits(132 * 1024 * 1024, budget_512, 14 * 1024 * 1024);
        assert_eq!(cap_w, 132 * 1024 * 1024);
        assert_eq!(
            limit_w,
            14 * 1024 * 1024 + 132 * 1024 * 1024 + (132 * 1024 * 1024) / 10
        );
        assert!(limit_w <= budget_512);

        // Small 256 MiB container budget: watchdog limit never exceeds budget
        let budget_256 = 256 * 1024 * 1024;
        let (cap_s, limit_s, _) =
            compute_worker_limits(256 * 1024 * 1024, budget_256, 12 * 1024 * 1024);
        assert!(limit_s <= budget_256);
        assert_eq!(
            limit_s,
            12 * 1024 * 1024 + cap_s + backstop_margin_bytes(cap_s)
        );
    }

    #[test]
    fn backstop_kill_under_heap_cap_is_transient_while_allocator_cap_is_memory_exceeded() {
        use legion_os::run::CapturedOutput;

        let mut base_out = RunOutput {
            exit_code: None,
            signal: Some(9),
            stdout: CapturedOutput::default(),
            stdout_bytes: {
                let mut b = Vec::new();
                b.extend_from_slice(BASELINE_MAGIC);
                b.extend_from_slice(&(11 * 1024 * 1024u64).to_le_bytes());
                b
            },
            stderr: CapturedOutput::default(),
            timed_out: false,
            cancelled: false,
            killed_for_memory: true,
            memory_watchdog_killed: true,
            peak_rss_bytes: 150 * 1024 * 1024,
            cpu_time: None,
            duration: Duration::from_millis(25),
        };

        // 1. Watchdog kill while allocator did NOT exit 107 -> TransientFailure (NOT cached bad!)
        let outcome = classify_worker_output(
            Path::new("/honest.jpg"),
            &base_out,
            132 * 1024 * 1024,
            149 * 1024 * 1024,
            10 * 1024 * 1024,
            Duration::from_secs(10),
        )
        .unwrap();
        match outcome {
            WorkerOutcome::TransientFailure(err) => {
                let d = err.details.expect("backstop diagnostics");
                assert_eq!(d["transient"], json!(true));
                assert_eq!(d["enforced_by"], json!("rss_watchdog"));
                assert_eq!(d["baseline_rss_bytes"], json!(10 * 1024 * 1024u64));
                assert_eq!(d["worker_baseline_rss_bytes"], json!(11 * 1024 * 1024u64));
                assert_eq!(d["heap_cap_bytes"], json!(132 * 1024 * 1024u64));
                assert_eq!(d["peak_rss_bytes"], json!(150 * 1024 * 1024u64));
                assert!(d["platform"].is_string());
            }
            other => panic!("backstop kill under heap cap must be TransientFailure, got {other:?}"),
        }

        // 2. Allocator cap exit 107 -> MemoryLimitExceeded (eligible for bad-file cache)
        base_out.exit_code = Some(CAP_EXCEEDED_EXIT_CODE);
        base_out.signal = None;
        base_out.memory_watchdog_killed = false;
        let outcome2 = classify_worker_output(
            Path::new("/liar.jpg"),
            &base_out,
            12 * 1024 * 1024,
            28 * 1024 * 1024,
            10 * 1024 * 1024,
            Duration::from_secs(10),
        )
        .unwrap();
        match outcome2 {
            WorkerOutcome::MemoryLimitExceeded { error } => {
                let d = error.details.expect("allocator cap details");
                assert_eq!(d["killed_for_memory"], json!(true));
                assert_eq!(d["enforced_by"], json!("allocator_cap"));
            }
            other => panic!("allocator cap exit 107 must be MemoryLimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn audio_worker_frames_and_cli_args_validate_bounds() {
        let tags = ParsedAudioTags {
            title: Some("First song".to_owned()),
            artist: Some("Someone".to_owned()),
            album: Some("First album".to_owned()),
            album_artist: None,
            track: Some(1),
            disc: Some(1),
            year: Some(2025),
            duration_s: Some(4.5),
            has_embedded_cover: true,
        };
        let payload = serde_json::to_vec(&tags).unwrap();

        let mut tags_raw = Vec::new();
        tags_raw.extend_from_slice(WORKER_MAGIC);
        tags_raw.push(STATUS_TAGS_OK);
        tags_raw.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        tags_raw.extend_from_slice(&payload);
        match parse_worker_frame_any(&tags_raw) {
            Some(ParsedWorkerFrame::Tags(got)) => assert_eq!(got, tags),
            other => panic!("expected Tags frame, got {:?}", other.map(|_| ())),
        }

        let mut no_cover_raw = Vec::new();
        no_cover_raw.extend_from_slice(WORKER_MAGIC);
        no_cover_raw.push(STATUS_NO_COVER);
        no_cover_raw.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        no_cover_raw.extend_from_slice(&payload);
        match parse_worker_frame_any(&no_cover_raw) {
            Some(ParsedWorkerFrame::NoCover(got)) => assert_eq!(got, tags),
            other => panic!("expected NoCover frame, got {:?}", other.map(|_| ())),
        }

        let mut cap_raw = Vec::new();
        cap_raw.extend_from_slice(WORKER_MAGIC);
        cap_raw.push(STATUS_NEEDS_CAP);
        cap_raw.extend_from_slice(&(200 * 1024 * 1024u64).to_le_bytes());
        match parse_worker_frame_any(&cap_raw) {
            Some(ParsedWorkerFrame::NeedsCap(cap)) => assert_eq!(cap, 200 * 1024 * 1024),
            other => panic!("expected NeedsCap frame, got {:?}", other.map(|_| ())),
        }

        // Absurd audio worker CLI arguments must be rejected before allocating.
        assert_eq!(
            parse_audio_tags_worker_args(&[
                OsString::from("bogus"),
                OsString::from("16777216"),
                OsString::from("/a.mp3"),
            ])
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            parse_audio_tags_worker_args(&[
                OsString::from("mp3"),
                OsString::from("999999999999"),
                OsString::from("/a.mp3"),
            ])
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgs
        );
        assert_eq!(
            parse_audio_cover_worker_args(&[
                OsString::from("mp3"),
                OsString::from("1025"),
                OsString::from("contain"),
                OsString::from("67108864"),
                OsString::from("1"),
                OsString::from("/a.mp3"),
            ])
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgs
        );
    }
}
