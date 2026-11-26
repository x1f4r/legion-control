//! Node-side audio metadata extraction (`fs.tags`) and shared folder-cover discovery.
//!
//! # Security & resource limits
//!
//! - **Authorization parity**: requires `fs.read` permission on the role and per-path
//!   [`check_fs_path`] confinement before touching the file system or the shared thumbnail cache.
//!   Folder cover pictures (`cover`, `folder`, `front`, `album` with `.jpg`/`.jpeg`/`.png`/`.webp`,
//!   any case) are likewise checked against `fs.read` per caller on every lookup and never cached
//!   across principals.
//! - **Out-of-process hostile-input isolation**: audio tag parsing runs inside the existing
//!   `__thumb-worker` subprocess from the already-opened regular file handle on `stdin` under the
//!   same live-heap allocator cap, RSS watchdog, OS limits, shared in-flight pool, and wall-clock
//!   time limit as picture decoding; the worker opens nothing by path and reads at most 16 MiB of
//!   tag data per file.
//! - **Shared cache**: file-intrinsic tags (excluding folder-cover lookup) are stored in
//!   `thumb-cache/` under the same path/size/mtime/file-identity/ctime key rules as `fs.thumb`, so
//!   modifying the audio file invalidates the entry while adding, removing, or restricting a
//!   folder cover takes effect immediately.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use legion_core::proto::{Effect, ErrorBody, ErrorCode};
use legion_core::{Access, CancellationToken, Ctx, Grant, Registry, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use super::audio_tags::{
    AudioFormat, CoverExtractMode, ParsedAudioTags, audio_format_for_path, parse_audio,
};
use super::thumb::{
    FileStamp, IMAGE_DECODE_TIMEOUT, IMAGE_WAIT_TIMEOUT, MAX_PATHS, MIN_PATHS,
    changed_while_reading, image_pool, machine_in_flight_image_budget, timeout_backoff,
    verify_post_read_stamp,
};
use super::thumb_cache::{ThumbCache, tags_bad_cache_key, tags_cache_key};
use super::thumb_limit::AcquireError;
use super::thumb_worker::{
    AudioTagsWorkerOutcome, MIN_IMAGE_HEAP_CAP_BYTES, backstop_margin_bytes, compute_worker_limits,
    effective_decode_timeout, run_audio_tags_worker, worker_baseline_rss_bytes, worker_enabled,
};
use super::{
    cancelled_error, check_fs_path, check_regular_file, map_io_error, open_regular_file,
    path_to_string,
};

/// Live-heap cap for `--audio-tags` on audio files larger than 4 MiB (covers the up-to-16 MiB
/// cumulative tag buffer, bounded Ogg packet reassembly growth, and worker runtime overhead; text
/// values decode only a bounded prefix and base64 `METADATA_BLOCK_PICTURE` values are sniffed in a
/// streaming pass without copying or decoding the full picture).
const AUDIO_TAGS_LARGE_HEAP_CAP_BYTES: u64 = 48 * 1024 * 1024;
const SMALL_AUDIO_FILE_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;

static TEST_AUDIO_TAGS_HEAP_CAP_BYTES: AtomicU64 = AtomicU64::new(0);

/// Test-only override for the `--audio-tags` worker's live-heap cap (`None` restores the
/// production 16 MiB / 48 MiB sizing).
#[doc(hidden)]
pub fn set_audio_tags_heap_cap_for_test(cap_bytes: Option<u64>) {
    TEST_AUDIO_TAGS_HEAP_CAP_BYTES.store(cap_bytes.unwrap_or(0), Ordering::Release);
}

const FOLDER_COVER_STEMS: [&str; 4] = ["cover", "folder", "front", "album"];
const FOLDER_COVER_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

pub(super) fn register(registry: &mut Registry) {
    registry.add_fn(
        "fs.tags",
        Effect::Read,
        "Read audio metadata tags and cover presence for 1 to 32 audio file paths on this node (ID3v2/ID3v1 in MP3, WAV and AIFF, FLAC blocks, Ogg Vorbis/Opus comments, and MP4 items in M4A, M4B and MP4; parsed in an out-of-process memory-bounded worker capped at 16 MiB of tag data per file), returning per-path tags or per-path errors in input order, cached alongside thumbnails in the state directory. The cover flag reports cheaply that an embedded or folder picture was found, not that it decodes; callers should show their placeholder if fs.thumb later fails.",
        tags,
    );
}

/// Arguments of `fs.tags`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TagsArgs {
    /// One to 32 audio file paths to inspect (absolute or starting with `~`).
    paths: Vec<String>,
}

/// Metadata reported for a single audio file.
#[derive(Debug, Serialize)]
struct TagsData {
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    track: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_s: Option<f64>,
    cover: bool,
}

/// Per-path entry in the `fs.tags` reply.
#[derive(Debug, Serialize)]
struct TagsItem {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ok: Option<TagsData>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorBody>,
}

impl TagsItem {
    fn from_parsed(path: String, parsed: ParsedAudioTags, cover: bool) -> TagsItem {
        let duration_s = parsed.duration_s.filter(|d| d.is_finite() && *d > 0.0);
        TagsItem {
            path,
            ok: Some(TagsData {
                title: parsed.title,
                artist: parsed.artist,
                album: parsed.album,
                album_artist: parsed.album_artist,
                track: parsed.track,
                disc: parsed.disc,
                year: parsed.year,
                duration_s,
                cover,
            }),
            error: None,
        }
    }

    fn from_error(path: String, error: ErrorBody) -> TagsItem {
        TagsItem {
            path,
            ok: None,
            error: Some(error),
        }
    }
}

/// Result of `fs.tags`.
#[derive(Debug, Serialize)]
struct TagsResult {
    results: Vec<TagsItem>,
}

/// Finds all candidate folder cover pictures in `audio_path`'s parent directory in priority order
/// that are non-empty regular files the caller's [`Grant`] is allowed to `fs.read`.
///
/// Matches `<stem>.<ext>` (ASCII case-insensitive) where `stem` is one of `cover`, `folder`,
/// `front`, `album` (in that priority order) and `ext` is one of `jpg`, `jpeg`, `png`, `webp`.
/// Directories, FIFOs, sockets, broken symlinks, and files outside the caller's allowed paths are
/// skipped without blocking.
pub(super) fn find_folder_covers(grant: &Grant, audio_path: &Path) -> Vec<PathBuf> {
    let Some(parent) = audio_path.parent() else {
        return Vec::new();
    };
    let Ok(read_dir) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut candidates: Vec<(usize, usize, std::ffi::OsString, PathBuf)> = Vec::new();

    for entry in read_dir.flatten() {
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        let Some((stem_rank, ext_rank)) = match_folder_cover_name(name_str) else {
            continue;
        };
        candidates.push((stem_rank, ext_rank, file_name, entry.path()));
    }

    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    let mut allowed = Vec::new();
    for (_, _, _, cand_path) in candidates {
        let Ok(resolved) = check_fs_path(grant, &path_to_string(&cand_path), Access::Read) else {
            continue;
        };
        if let Ok((_file, meta)) = open_regular_file(
            &resolved,
            "read",
            "Folder cover candidate must be a regular file.",
        ) && meta.len() > 0
        {
            allowed.push(resolved);
        }
    }
    allowed
}

/// Finds the highest-priority folder cover picture in `audio_path`'s parent directory that is a
/// non-empty regular file the caller's [`Grant`] is allowed to `fs.read` (stops on the first match
/// so `fs.tags` stays cheap).
pub(super) fn find_folder_cover(grant: &Grant, audio_path: &Path) -> Option<PathBuf> {
    let parent = audio_path.parent()?;
    let read_dir = fs::read_dir(parent).ok()?;
    let mut candidates: Vec<(usize, usize, std::ffi::OsString, PathBuf)> = Vec::new();

    for entry in read_dir.flatten() {
        let file_name = entry.file_name();
        let Some(name_str) = file_name.to_str() else {
            continue;
        };
        let Some((stem_rank, ext_rank)) = match_folder_cover_name(name_str) else {
            continue;
        };
        candidates.push((stem_rank, ext_rank, file_name, entry.path()));
    }

    candidates.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });

    for (_, _, _, cand_path) in candidates {
        let Ok(resolved) = check_fs_path(grant, &path_to_string(&cand_path), Access::Read) else {
            continue;
        };
        if let Ok((_file, meta)) = open_regular_file(
            &resolved,
            "read",
            "Folder cover candidate must be a regular file.",
        ) && meta.len() > 0
        {
            return Some(resolved);
        }
    }
    None
}

fn match_folder_cover_name(name: &str) -> Option<(usize, usize)> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || stem.contains('.') {
        return None;
    }
    let stem_rank = FOLDER_COVER_STEMS
        .iter()
        .position(|&s| stem.eq_ignore_ascii_case(s))?;
    let ext_rank = FOLDER_COVER_EXTS
        .iter()
        .position(|&e| ext.eq_ignore_ascii_case(e))?;
    Some((stem_rank, ext_rank))
}

async fn tags(ctx: &Ctx, args: TagsArgs) -> Result<TagsResult, ToolError> {
    let read_spec = ctx.node().spec("fs.read")?;
    if !ctx.grant().allows(&read_spec) {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            format!(
                "{} may not use fs.tags: its role {} is not allowed to call fs.read.",
                ctx.principal(),
                ctx.grant().role()
            ),
        )
        .with_hint("Reading audio tags requires the same permission as fs.read; ask the owner to allow fs.read in policy.toml."));
    }

    if !(MIN_PATHS..=MAX_PATHS).contains(&args.paths.len()) {
        return Err(ErrorBody::new(
            ErrorCode::InvalidArgs,
            format!(
                "fs.tags accepts between {MIN_PATHS} and {MAX_PATHS} paths per call, and {} were given.",
                args.paths.len()
            ),
        )
        .with_hint("Pass a non-empty list of up to 32 audio file paths."));
    }

    let cache = ThumbCache::for_dir(ctx.paths().thumb_cache_dir());
    let grant = ctx.grant().clone();
    let principal = ctx.principal().to_owned();
    let cancel = ctx.cancel_token().clone();

    let results = run_tags_batch(args.paths, cancel, move |raw_path, cancel| {
        let grant = grant.clone();
        let principal = principal.clone();
        let cache = cache.clone();
        async move { tags_one(&grant, &principal, &cache, raw_path, cancel).await }
    })
    .await?;

    Ok(TagsResult { results })
}

async fn run_tags_batch<F, Fut>(
    paths: Vec<String>,
    cancel: CancellationToken,
    worker: F,
) -> Result<Vec<TagsItem>, ToolError>
where
    F: Fn(String, CancellationToken) -> Fut,
    Fut: std::future::Future<Output = Result<TagsItem, ToolError>> + Send + 'static,
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

    let mut slots: Vec<Option<TagsItem>> = (0..total).map(|_| None).collect();
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
                    *slot = Some(TagsItem::from_error(raw_path, err));
                }
            }
            Err(join_err) => {
                let task_id = join_err.id();
                if let Some((idx, raw_path)) = task_owners.remove(&task_id)
                    && let Some(slot) = slots.get_mut(idx)
                {
                    *slot = Some(TagsItem::from_error(
                        raw_path,
                        ErrorBody::new(
                            ErrorCode::Internal,
                            format!("Audio tag worker stopped unexpectedly: {join_err}."),
                        ),
                    ));
                }
            }
        }
    }

    if cancel.is_cancelled() {
        return Err(cancelled_error("fs.tags"));
    }

    let results = slots
        .into_iter()
        .enumerate()
        .map(|(idx, slot)| {
            slot.unwrap_or_else(|| {
                TagsItem::from_error(
                    original_paths[idx].clone(),
                    ErrorBody::new(
                        ErrorCode::Internal,
                        "Audio tag worker exited without returning a result.",
                    ),
                )
            })
        })
        .collect();
    Ok(results)
}

enum TagsPrep {
    CacheHit(ParsedAudioTags),
    CachedBad(ErrorBody),
    Ready {
        tags_key: String,
        bad_key: String,
        open_stamp: FileStamp,
        file: File,
        reserve_bytes: u64,
        heap_cap_bytes: u64,
        rss_limit_bytes: u64,
        baseline_rss_bytes: u64,
    },
}

fn prepare_tags_open_once(path: &Path, cache: &ThumbCache) -> Result<TagsPrep, ErrorBody> {
    let pre_meta = check_regular_file(
        path,
        "read tags from",
        "Pass a regular audio file path to fs.tags; directories do not have audio tags.",
    )?;
    let pre_stamp = FileStamp::from_path(path, &pre_meta);

    let (file, open_meta) = open_regular_file(
        path,
        "read",
        "Pass a regular audio file path to fs.tags; directories do not have audio tags.",
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
        )
        .with_hint("The file may be truncated or still being written."));
    }

    let tags_key = tags_cache_key(
        path,
        open_stamp.size,
        open_stamp.mtime_nanos,
        open_stamp.file_id,
    );
    if let Some(hit) = cache.get_tags(&tags_key) {
        return Ok(TagsPrep::CacheHit(hit));
    }

    let bad_key = tags_bad_cache_key(
        path,
        open_stamp.size,
        open_stamp.mtime_nanos,
        open_stamp.file_id,
    );
    if let Some(bad) = cache.get_bad(&bad_key) {
        return Ok(TagsPrep::CachedBad(bad));
    }

    let budget = machine_in_flight_image_budget();
    let baseline = worker_baseline_rss_bytes();
    let (heap_cap_bytes, rss_limit_bytes, baseline_rss_bytes) =
        match TEST_AUDIO_TAGS_HEAP_CAP_BYTES.load(Ordering::Acquire) {
            0 => {
                let requested_heap_cap = if open_stamp.size <= SMALL_AUDIO_FILE_THRESHOLD_BYTES {
                    MIN_IMAGE_HEAP_CAP_BYTES
                } else {
                    AUDIO_TAGS_LARGE_HEAP_CAP_BYTES
                };
                compute_worker_limits(requested_heap_cap, budget, baseline)
            }
            override_cap => {
                let heap_cap = override_cap.min(budget);
                let rss_limit = baseline
                    .saturating_add(heap_cap)
                    .saturating_add(backstop_margin_bytes(heap_cap))
                    .min(budget);
                (heap_cap, rss_limit, baseline)
            }
        };

    Ok(TagsPrep::Ready {
        tags_key,
        bad_key,
        open_stamp,
        file,
        reserve_bytes: rss_limit_bytes,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
    })
}

async fn finish_with_cover(
    grant: &Grant,
    resolved: &Path,
    raw_path: String,
    parsed: ParsedAudioTags,
) -> Result<TagsItem, ToolError> {
    let cover = if parsed.has_embedded_cover {
        true
    } else {
        let grant_c = grant.clone();
        let resolved_c = resolved.to_path_buf();
        tokio::task::spawn_blocking(move || find_folder_cover(&grant_c, &resolved_c).is_some())
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.tags folder cover check stopped unexpectedly: {err}."),
                )
            })?
    };
    Ok(TagsItem::from_parsed(raw_path, parsed, cover))
}

async fn tags_one(
    grant: &Grant,
    principal: &str,
    cache: &ThumbCache,
    raw_path: String,
    cancel: CancellationToken,
) -> Result<TagsItem, ToolError> {
    if cancel.is_cancelled() {
        return Err(cancelled_error("fs.tags"));
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
            format!("fs.tags path check stopped unexpectedly: {err}."),
        )
    })? {
        Ok(path) => path,
        Err(err) => return Ok(TagsItem::from_error(raw_path, err)),
    };

    let Some(format) = audio_format_for_path(&resolved) else {
        let resolved_stat = resolved.clone();
        let stat_res = tokio::task::spawn_blocking(move || {
            check_regular_file(
                &resolved_stat,
                "read tags from",
                "Pass a regular audio file path to fs.tags; directories do not have audio tags.",
            )
        })
        .await
        .map_err(|err| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!("fs.tags stat stopped unexpectedly: {err}."),
            )
        })?;
        if let Err(err) = stat_res {
            return Ok(TagsItem::from_error(raw_path, err));
        }
        return Ok(TagsItem::from_error(
            raw_path,
            ErrorBody::new(
                ErrorCode::Unsupported,
                format!("{} is not an audio file.", resolved.display()),
            )
            .with_hint(
                "fs.tags reads metadata from audio files (mp3, wav, aif, aiff, flac, ogg, opus, m4a, m4b, mp4).",
            ),
        ));
    };

    let cache_prep = cache.clone();
    let resolved_prep = resolved.clone();
    let prep =
        tokio::task::spawn_blocking(move || prepare_tags_open_once(&resolved_prep, &cache_prep))
            .await
            .map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.tags open stopped unexpectedly: {err}."),
                )
            })?;

    let (
        tags_key,
        bad_key,
        open_stamp,
        mut file,
        reserve_bytes,
        heap_cap_bytes,
        rss_limit_bytes,
        baseline_rss_bytes,
    ) = match prep {
        Ok(TagsPrep::CacheHit(hit)) => {
            return finish_with_cover(grant, &resolved, raw_path, hit).await;
        }
        Ok(TagsPrep::CachedBad(err)) => return Ok(TagsItem::from_error(raw_path, err)),
        Ok(TagsPrep::Ready {
            tags_key,
            bad_key,
            open_stamp,
            file,
            reserve_bytes,
            heap_cap_bytes,
            rss_limit_bytes,
            baseline_rss_bytes,
        }) => (
            tags_key,
            bad_key,
            open_stamp,
            file,
            reserve_bytes,
            heap_cap_bytes,
            rss_limit_bytes,
            baseline_rss_bytes,
        ),
        Err(err) => return Ok(TagsItem::from_error(raw_path, err)),
    };

    if let Some(remaining) = timeout_backoff().active_remaining(&bad_key) {
        return Ok(TagsItem::from_error(
            raw_path,
            ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Reading tags for {} timed out repeatedly and is in a temporary back-off ({} s remaining).",
                    resolved.display(),
                    remaining.as_secs().max(1)
                ),
            )
            .with_hint(
                "Timeouts are not cached permanently; retry after the short back-off expires.",
            ),
        ));
    }

    let permit = match image_pool()
        .acquire(principal, reserve_bytes, IMAGE_WAIT_TIMEOUT, &cancel)
        .await
    {
        Ok(permit) => permit,
        Err(AcquireError::Cancelled) => return Err(cancelled_error("fs.tags")),
        Err(AcquireError::TimedOut) => {
            return Ok(TagsItem::from_error(
                raw_path,
                ErrorBody::new(
                    ErrorCode::Busy,
                    format!(
                        "Timed out waiting for media worker capacity for {}.",
                        resolved.display()
                    ),
                )
                .with_hint("Media workers are busy on this node; retry fs.tags shortly."),
            ));
        }
    };

    let decode_timeout = effective_decode_timeout(IMAGE_DECODE_TIMEOUT);

    if worker_enabled() {
        let handle_check = match file.try_clone() {
            Ok(cloned) => cloned,
            Err(err) => {
                drop(permit);
                return Ok(TagsItem::from_error(
                    raw_path,
                    map_io_error("read", &resolved, err),
                ));
            }
        };
        let worker_res = run_audio_tags_worker(
            &resolved,
            file,
            format,
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
            Err(err) => return Ok(TagsItem::from_error(raw_path, err)),
        };

        return match worker_outcome {
            AudioTagsWorkerOutcome::Ok(parsed) => {
                let resolved_check = resolved.clone();
                let stamp_ok = tokio::task::spawn_blocking(move || {
                    verify_post_read_stamp(&resolved_check, &handle_check, open_stamp)
                })
                .await
                .map_err(|err| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("fs.tags post-read check stopped unexpectedly: {err}."),
                    )
                })?;
                if let Err(err) = stamp_ok {
                    return Ok(TagsItem::from_error(raw_path, err));
                }
                let cache_put = cache.clone();
                let parsed_put = parsed.clone();
                let _ =
                    tokio::task::spawn_blocking(move || cache_put.put_tags(&tags_key, &parsed_put))
                        .await;
                finish_with_cover(grant, &resolved, raw_path, parsed).await
            }
            AudioTagsWorkerOutcome::Rejected(err)
            | AudioTagsWorkerOutcome::TransientFailure(err) => {
                Ok(TagsItem::from_error(raw_path, err))
            }
            AudioTagsWorkerOutcome::TimedOut { error, cpu_busy } => {
                if cpu_busy {
                    timeout_backoff().record_timeout(&bad_key);
                }
                Ok(TagsItem::from_error(raw_path, error))
            }
            AudioTagsWorkerOutcome::MemoryLimitExceeded { error } => {
                let resolved_check = resolved.clone();
                let cache_bad = cache.clone();
                let err_bad = error.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if verify_post_read_stamp(&resolved_check, &handle_check, open_stamp).is_ok() {
                        cache_bad.put_bad(&bad_key, &err_bad);
                    }
                })
                .await;
                Ok(TagsItem::from_error(raw_path, error))
            }
        };
    }

    // In-process fallback (standard unit/integration test binaries without `__thumb-worker`).
    let abort_flag = Arc::new(AtomicBool::new(false));
    let resolved_parse = resolved.clone();
    let mut handle = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let extracted = parse_audio(
            &mut file,
            open_stamp.size,
            format,
            CoverExtractMode::CheckOnly,
            &resolved_parse,
        )?;
        verify_post_read_stamp(&resolved_parse, &file, open_stamp)?;
        Ok::<ParsedAudioTags, ErrorBody>(extracted.tags)
    });

    let sleep = tokio::time::sleep(decode_timeout);
    tokio::pin!(sleep);

    let outcome = tokio::select! {
        joined = &mut handle => {
            joined.map_err(|err| {
                ErrorBody::new(
                    ErrorCode::Internal,
                    format!("fs.tags parser stopped unexpectedly: {err}."),
                )
            })?
        }
        () = cancel.cancelled() => {
            abort_flag.store(true, Ordering::Release);
            return Err(cancelled_error("fs.tags"));
        }
        () = &mut sleep => {
            abort_flag.store(true, Ordering::Release);
            Err(ErrorBody::new(
                ErrorCode::Timeout,
                format!(
                    "Reading audio tags for {} exceeded the {}-second time limit.",
                    resolved.display(),
                    decode_timeout.as_secs().max(1)
                ),
            )
            .with_hint("The audio file may be malformed or too slow to read."))
        }
    };

    match outcome {
        Ok(parsed) => {
            let cache_put = cache.clone();
            let parsed_put = parsed.clone();
            let _ = tokio::task::spawn_blocking(move || cache_put.put_tags(&tags_key, &parsed_put))
                .await;
            finish_with_cover(grant, &resolved, raw_path, parsed).await
        }
        Err(err) if err.code == ErrorCode::Cancelled => Err(err),
        Err(err) => Ok(TagsItem::from_error(raw_path, err)),
    }
}

/// Converts an [`AudioFormat`] to its stable CLI token for `__thumb-worker`.
pub(super) const fn audio_format_cli_name(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Mp3 => "mp3",
        AudioFormat::Wav => "wav",
        AudioFormat::Aiff => "aiff",
        AudioFormat::Flac => "flac",
        AudioFormat::Ogg => "ogg",
        AudioFormat::Mp4 => "mp4",
        AudioFormat::GenericAudio => "generic",
    }
}

/// Parses a CLI format token produced by [`audio_format_cli_name`].
pub(super) fn parse_audio_format_cli_name(s: &str) -> Option<AudioFormat> {
    match s {
        "mp3" => Some(AudioFormat::Mp3),
        "wav" => Some(AudioFormat::Wav),
        "aiff" => Some(AudioFormat::Aiff),
        "flac" => Some(AudioFormat::Flac),
        "ogg" => Some(AudioFormat::Ogg),
        "mp4" => Some(AudioFormat::Mp4),
        "generic" => Some(AudioFormat::GenericAudio),
        _ => None,
    }
}
