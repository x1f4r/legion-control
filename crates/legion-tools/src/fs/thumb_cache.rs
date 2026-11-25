//! Bounded on-disk LRU cache for `fs.thumb` inside the node's state directory.
//!
//! Design properties:
//! - **Directory is the truth**: there is no separate index file to drift or rewrite. Every
//!   `.thumb` file in `<state_dir>/thumb-cache/` is written atomically (`0o600` on Unix) and
//!   counted during reconciliation, so entries left behind by crashes or killed processes are
//!   always accounted for and evicted.
//! - **Cheap lock-free hits**: [`ThumbCache::get`] reads `<key>.thumb` directly without taking
//!   any lock or calling `fsync`, and updates LRU recency with a best-effort mtime touch on that
//!   single file only. Lock contention or eviction races never turn a valid cache hit into a miss.
//! - **Bounded cap & temp cleanup**: [`ThumbCache::put`] reconciles on first write, periodically,
//!   and whenever approximate usage approaches the 256 MiB cap under an exclusive `cache.lock`,
//!   evicting least-recently-used `.thumb` files strictly down to the cap and sweeping stale
//!   temporary files and `tmp/` frame directories left by interrupted processes.
//! - **Protected by state dir**: because the cache lives under `state_dir`, non-admin roles cannot
//!   read or modify it through `fs.*` tools.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use legion_core::proto::ErrorBody;
use legion_os::atomic::{WriteOptions, write_atomic};
use legion_os::file_id::FileId;
use legion_os::lock::{FileLock, LockMode};

use super::platform;
use super::thumb::ThumbFit;

/// Default maximum total size of cached thumbnail files on disk (256 MiB).
pub(crate) const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

const CACHE_VERSION: &str = "legion-thumb-v2";
const BAD_CACHE_VERSION: &str = "legion-thumb-bad-v1";
const ENTRY_MAGIC: &[u8; 4] = b"LGTH";
const ENTRY_VERSION: u8 = 1;
const MIME_JPEG_TAG: u8 = 1;
const MIME_PNG_TAG: u8 = 2;
const BAD_ENTRY_TAG: u8 = 3;
const MAX_BAD_ERROR_BYTES: usize = 16 * 1024;
/// Negative bad-file cache records expire after 7 days so files are retried once after expiry.
pub(crate) const BAD_ENTRY_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ENTRY_HEADER_LEN: usize = 14;
const LOCK_FILE: &str = "cache.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);
const RECONCILE_EVERY_PUTS: u64 = 16;
const DEFAULT_STALE_TMP_AGE: Duration = Duration::from_secs(5 * 60);

static CACHE_INSTANCES: OnceLock<Mutex<HashMap<PathBuf, ThumbCache>>> = OnceLock::new();

/// Decoded/encoded thumbnail payload stored in and retrieved from the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CachedThumb {
    pub(crate) mime: &'static str,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) bytes: Vec<u8>,
}

/// Computes the deterministic BLAKE3 cache key for one thumbnail request.
///
/// Includes the resolved path, source byte length, modification time, OS file identity (`dev`/`ino`
/// or Windows volume+file index) and metadata change time (`ctime` / NTFS `ChangeTime`), plus
/// thumbnail parameters, so a file swapped with identical size and mtime never serves a stale
/// thumbnail.
pub(crate) fn cache_key(
    resolved_path: &Path,
    source_size: u64,
    mtime_nanos: Option<i128>,
    file_id: Option<FileId>,
    thumb_size: u32,
    fit: ThumbFit,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(CACHE_VERSION.as_bytes());
    let path_bytes = resolved_path.as_os_str().as_encoded_bytes();
    hasher.update(&(path_bytes.len() as u64).to_le_bytes());
    hasher.update(path_bytes);
    hasher.update(&source_size.to_le_bytes());
    match mtime_nanos {
        Some(nanos) => {
            hasher.update(&[1]);
            hasher.update(&nanos.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    match file_id {
        Some(id) => {
            hasher.update(&[1]);
            hasher.update(&id.volume.to_le_bytes());
            hasher.update(&id.index.to_le_bytes());
            match id.change_time {
                Some(ct) => {
                    hasher.update(&[1]);
                    hasher.update(&ct.to_le_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.update(&thumb_size.to_le_bytes());
    hasher.update(&[match fit {
        ThumbFit::Contain => 0,
        ThumbFit::Cover => 1,
    }]);
    hasher.finalize().to_hex().to_string()
}

/// Computes the deterministic BLAKE3 key for a negative bad-file cache entry tied to the source
/// file's identity (independent of target `size` and `fit` so reopening a folder at any size does
/// not repeatedly spawn and kill a worker on the same hostile/corrupt file).
pub(crate) fn bad_cache_key(
    resolved_path: &Path,
    source_size: u64,
    mtime_nanos: Option<i128>,
    file_id: Option<FileId>,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(BAD_CACHE_VERSION.as_bytes());
    let path_bytes = resolved_path.as_os_str().as_encoded_bytes();
    hasher.update(&(path_bytes.len() as u64).to_le_bytes());
    hasher.update(path_bytes);
    hasher.update(&source_size.to_le_bytes());
    match mtime_nanos {
        Some(nanos) => {
            hasher.update(&[1]);
            hasher.update(&nanos.to_le_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
    match file_id {
        Some(id) => {
            hasher.update(&[1]);
            hasher.update(&id.volume.to_le_bytes());
            hasher.update(&id.index.to_le_bytes());
            match id.change_time {
                Some(ct) => {
                    hasher.update(&[1]);
                    hasher.update(&ct.to_le_bytes());
                }
                None => {
                    hasher.update(&[0]);
                }
            }
        }
        None => {
            hasher.update(&[0]);
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// Process-local hints used to avoid rescanning a huge cache directory on every single miss while
/// still reconciling on startup, periodically, and whenever usage approaches `max_bytes`.
#[derive(Debug)]
struct CacheHints {
    reconciled_once: AtomicBool,
    puts_since_reconcile: AtomicU64,
    approx_bytes: AtomicU64,
    last_reconcile: Mutex<Option<Instant>>,
    last_touch: Mutex<SystemTime>,
}

impl Default for CacheHints {
    fn default() -> Self {
        Self {
            reconciled_once: AtomicBool::new(false),
            puts_since_reconcile: AtomicU64::new(0),
            approx_bytes: AtomicU64::new(0),
            last_reconcile: Mutex::new(None),
            last_touch: Mutex::new(UNIX_EPOCH),
        }
    }
}

/// On-disk LRU thumbnail cache rooted in a single directory.
#[derive(Clone, Debug)]
pub(crate) struct ThumbCache {
    dir: PathBuf,
    max_bytes: u64,
    stale_tmp_age: Duration,
    hints: Arc<CacheHints>,
}

impl ThumbCache {
    /// Returns the process-wide shared [`ThumbCache`] for `dir` so reconciliation hints persist
    /// across `fs.thumb` calls instead of rescanning the cache directory on every write.
    pub(crate) fn for_dir(dir: PathBuf) -> ThumbCache {
        let map = CACHE_INSTANCES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .entry(dir.clone())
            .or_insert_with(|| ThumbCache::new(dir))
            .clone()
    }

    /// Opens an unshared cache handle at `dir` capped at [`MAX_CACHE_BYTES`] (256 MiB).
    pub(crate) fn new(dir: PathBuf) -> ThumbCache {
        ThumbCache {
            dir,
            max_bytes: MAX_CACHE_BYTES,
            stale_tmp_age: DEFAULT_STALE_TMP_AGE,
            hints: Arc::new(CacheHints::default()),
        }
    }

    /// Opens a cache at `dir` with an explicit byte cap and stale-temp threshold (used in tests).
    #[cfg(test)]
    pub(crate) fn with_limits(dir: PathBuf, max_bytes: u64, stale_tmp_age: Duration) -> ThumbCache {
        ThumbCache {
            dir,
            max_bytes,
            stale_tmp_age,
            hints: Arc::new(CacheHints::default()),
        }
    }

    /// Looks up `key` without taking any lock, rewriting any index, or calling `fsync`.
    ///
    /// On hit, updates the entry file's modification time best-effort so LRU eviction keeps hot
    /// thumbnails. Any error touching mtime is ignored and never turns a hit into a miss.
    pub(crate) fn get(&self, key: &str) -> Option<CachedThumb> {
        let entry_path = self.entry_path(key);
        let raw = fs::read(&entry_path).ok()?;
        let Some(thumb) = decode_entry(&raw) else {
            let _ = fs::remove_file(&entry_path);
            return None;
        };
        let _ = self.touch_recency(&entry_path);
        Some(thumb)
    }

    /// Looks up a cached negative bad-file entry by [`bad_cache_key`], lock-free.
    ///
    /// Entries older than [`BAD_ENTRY_TTL`] (7 days) are deleted and treated as a miss so the
    /// file is retried once after expiry.
    pub(crate) fn get_bad(&self, key: &str) -> Option<ErrorBody> {
        self.get_bad_at(key, now_unix_secs())
    }

    pub(crate) fn get_bad_at(&self, key: &str, now_secs: u64) -> Option<ErrorBody> {
        let entry_path = self.entry_path(key);
        let raw = fs::read(&entry_path).ok()?;
        let Some((recorded_at, err)) = decode_bad_entry(&raw) else {
            let _ = fs::remove_file(&entry_path);
            return None;
        };
        if now_secs.saturating_sub(recorded_at) >= BAD_ENTRY_TTL.as_secs() {
            let _ = fs::remove_file(&entry_path);
            return None;
        }
        let _ = self.touch_recency(&entry_path);
        Some(err)
    }

    /// Stores `thumb` atomically under `key`, then reconciles and evicts least-recently-used
    /// entries when needed so the directory stays within `self.max_bytes` and free of stale
    /// temporaries.
    pub(crate) fn put(&self, key: &str, thumb: &CachedThumb) {
        let Some(encoded) = encode_entry(thumb) else {
            return;
        };
        self.write_encoded(key, &encoded);
    }

    /// Stores a negative bad-file entry atomically under `key` (sharing the same LRU byte cap,
    /// reconciliation, and 7-day expiry as positive thumbnail entries) so reopening a folder with
    /// a file rejected by this node's own memory limit does not repeat the kill.
    pub(crate) fn put_bad(&self, key: &str, error: &ErrorBody) {
        self.put_bad_at(key, error, now_unix_secs());
    }

    pub(crate) fn put_bad_at(&self, key: &str, error: &ErrorBody, recorded_at_secs: u64) {
        let Some(encoded) = encode_bad_entry(error, recorded_at_secs) else {
            return;
        };
        self.write_encoded(key, &encoded);
    }

    fn write_encoded(&self, key: &str, encoded: &[u8]) {
        let entry_size = encoded.len() as u64;
        if entry_size > self.max_bytes {
            return;
        }
        if self.ensure_dir().is_err() {
            return;
        }
        let entry_path = self.entry_path(key);
        if write_atomic(&entry_path, encoded, WriteOptions::with_mode(0o600)).is_err() {
            return;
        }
        let _ = self.touch_recency(&entry_path);

        let approx = self
            .hints
            .approx_bytes
            .fetch_add(entry_size, Ordering::Relaxed)
            .saturating_add(entry_size);
        let puts = self
            .hints
            .puts_since_reconcile
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);

        let need_reconcile = !self.hints.reconciled_once.load(Ordering::Relaxed)
            || self.max_bytes <= 1024 * 1024
            || approx > (self.max_bytes / 10).saturating_mul(9)
            || puts >= RECONCILE_EVERY_PUTS
            || self
                .hints
                .last_reconcile
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_none_or(|t| t.elapsed() >= RECONCILE_INTERVAL);

        if need_reconcile {
            self.reconcile(Some(key));
        }
    }

    /// Scans the cache directory as the source of truth under `cache.lock`: counts every `.thumb`
    /// file (including any orphaned entries from killed processes), evicts oldest-mtime entries
    /// down to `self.max_bytes`, and removes stale temporary files and `tmp/` subdirectories.
    pub(crate) fn reconcile(&self, keep_key: Option<&str>) {
        if !self.dir.exists() {
            return;
        }
        let Ok(_lock) = self.acquire_lock() else {
            return;
        };

        let now = SystemTime::now();
        let mut entries: Vec<(SystemTime, String, PathBuf, u64)> = Vec::new();
        let mut total: u64 = 0;

        if let Ok(read_dir) = fs::read_dir(&self.dir) {
            for item in read_dir.flatten() {
                let path = item.path();
                let name = item.file_name();
                let name_str = name.to_string_lossy();
                if name_str == LOCK_FILE || name_str == "tmp" {
                    continue;
                }
                let Ok(meta) = item.metadata() else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                if let Some(key) = name_str.strip_suffix(".thumb")
                    && is_valid_key(key)
                {
                    let size = meta.len();
                    let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
                    total = total.saturating_add(size);
                    entries.push((mtime, key.to_owned(), path, size));
                } else {
                    // Clean up legacy index files or stale staging files left by interrupted writes.
                    let is_stale = meta
                        .modified()
                        .ok()
                        .and_then(|t| now.duration_since(t).ok())
                        .is_some_and(|age| age >= self.stale_tmp_age);
                    if name_str == "index.v1" || is_stale {
                        let _ = fs::remove_file(&path);
                    }
                }
            }
        }

        if total > self.max_bytes {
            entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            for (_, key, path, size) in entries {
                if total <= self.max_bytes {
                    break;
                }
                if keep_key.is_some_and(|k| k == key) {
                    continue;
                }
                if fs::remove_file(&path).is_ok() {
                    total = total.saturating_sub(size);
                }
            }
        }

        self.cleanup_stale_tmp(now);

        self.hints.approx_bytes.store(total, Ordering::Relaxed);
        self.hints.puts_since_reconcile.store(0, Ordering::Relaxed);
        self.hints.reconciled_once.store(true, Ordering::Relaxed);
        *self
            .hints
            .last_reconcile
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
    }

    fn cleanup_stale_tmp(&self, now: SystemTime) {
        let tmp_root = self.dir.join("tmp");
        let Ok(read_dir) = fs::read_dir(&tmp_root) else {
            return;
        };
        for item in read_dir.flatten() {
            let path = item.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            let stale = meta
                .modified()
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age >= self.stale_tmp_age);
            if !stale {
                continue;
            }
            if meta.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
    }

    /// Monotonically advances the entry file's modification time without locking or fsyncing,
    /// guaranteeing strict LRU ordering even when multiple cache accesses happen within the same
    /// filesystem timestamp tick.
    fn touch_recency(&self, path: &Path) -> io::Result<()> {
        let stamp = {
            let mut last = self
                .hints
                .last_touch
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let now = SystemTime::now();
            let next = match last.checked_add(Duration::from_millis(10)) {
                Some(bumped) if bumped > now => bumped,
                _ => now,
            };
            *last = next;
            next
        };
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_modified(stamp)
    }

    fn ensure_dir(&self) -> io::Result<()> {
        if !self.dir.exists() {
            fs::create_dir_all(&self.dir)?;
            let _ = platform::set_unix_mode(&self.dir, 0o700);
        }
        Ok(())
    }

    fn acquire_lock(&self) -> legion_os::Result<FileLock> {
        let lock_path = self.dir.join(LOCK_FILE);
        FileLock::acquire_private_timeout(&lock_path, LockMode::Exclusive, LOCK_TIMEOUT)
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.thumb"))
    }
}

fn is_valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn encode_entry(thumb: &CachedThumb) -> Option<Vec<u8>> {
    let mime_tag = match thumb.mime {
        "image/jpeg" => MIME_JPEG_TAG,
        "image/png" => MIME_PNG_TAG,
        _ => return None,
    };
    let mut out = Vec::with_capacity(ENTRY_HEADER_LEN + thumb.bytes.len());
    out.extend_from_slice(ENTRY_MAGIC);
    out.push(ENTRY_VERSION);
    out.push(mime_tag);
    out.extend_from_slice(&thumb.width.to_le_bytes());
    out.extend_from_slice(&thumb.height.to_le_bytes());
    out.extend_from_slice(&thumb.bytes);
    Some(out)
}

fn decode_entry(raw: &[u8]) -> Option<CachedThumb> {
    if raw.len() <= ENTRY_HEADER_LEN || &raw[..4] != ENTRY_MAGIC || raw[4] != ENTRY_VERSION {
        return None;
    }
    let mime = match raw[5] {
        MIME_JPEG_TAG => "image/jpeg",
        MIME_PNG_TAG => "image/png",
        _ => return None,
    };
    let width = u32::from_le_bytes(raw[6..10].try_into().ok()?);
    let height = u32::from_le_bytes(raw[10..14].try_into().ok()?);
    if width == 0 || height == 0 {
        return None;
    }
    Some(CachedThumb {
        mime,
        width,
        height,
        bytes: raw[ENTRY_HEADER_LEN..].to_vec(),
    })
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn encode_bad_entry(error: &ErrorBody, recorded_at_secs: u64) -> Option<Vec<u8>> {
    let payload = serde_json::to_vec(error).ok()?;
    if payload.is_empty() || payload.len() > MAX_BAD_ERROR_BYTES {
        return None;
    }
    let mut out = Vec::with_capacity(ENTRY_HEADER_LEN + payload.len());
    out.extend_from_slice(ENTRY_MAGIC);
    out.push(ENTRY_VERSION);
    out.push(BAD_ENTRY_TAG);
    out.extend_from_slice(&recorded_at_secs.to_le_bytes());
    out.extend_from_slice(&payload);
    Some(out)
}

fn decode_bad_entry(raw: &[u8]) -> Option<(u64, ErrorBody)> {
    if raw.len() <= ENTRY_HEADER_LEN
        || raw.len() > ENTRY_HEADER_LEN + MAX_BAD_ERROR_BYTES
        || &raw[..4] != ENTRY_MAGIC
        || raw[4] != ENTRY_VERSION
        || raw[5] != BAD_ENTRY_TAG
    {
        return None;
    }
    let recorded_at = u64::from_le_bytes(raw[6..14].try_into().ok()?);
    let err: ErrorBody = serde_json::from_slice(&raw[ENTRY_HEADER_LEN..]).ok()?;
    Some((recorded_at, err))
}

/// Creates a throwaway directory for temporary `ffmpeg` frame output inside the cache directory
/// (keeping scratch out of small memory-backed `/tmp`).
pub(crate) fn temp_dir_in_cache(cache_dir: &Path) -> io::Result<tempfile::TempDir> {
    let tmp_root = cache_dir.join("tmp");
    fs::create_dir_all(&tmp_root)?;
    let _ = platform::set_unix_mode(cache_dir, 0o700);
    let _ = platform::set_unix_mode(&tmp_root, 0o700);
    tempfile::tempdir_in(&tmp_root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tag: u8, payload_len: usize) -> CachedThumb {
        CachedThumb {
            mime: "image/jpeg",
            width: 128,
            height: 96,
            bytes: vec![tag; payload_len],
        }
    }

    fn fid(vol: u64, idx: u64, ct: i128) -> Option<FileId> {
        Some(FileId {
            volume: vol,
            index: idx,
            change_time: Some(ct),
        })
    }

    #[test]
    fn cache_evicts_least_recently_used_and_honors_cap() {
        let dir = tempfile::tempdir().unwrap();
        // Each 100-byte payload is 114 bytes on disk; cap of 250 bytes holds at most 2.
        let cache =
            ThumbCache::with_limits(dir.path().join("thumb-cache"), 250, DEFAULT_STALE_TMP_AGE);
        let k1 = cache_key(
            Path::new("/a.jpg"),
            10,
            Some(1),
            fid(1, 1, 1),
            256,
            ThumbFit::Contain,
        );
        let k2 = cache_key(
            Path::new("/b.jpg"),
            10,
            Some(1),
            fid(1, 2, 1),
            256,
            ThumbFit::Contain,
        );
        let k3 = cache_key(
            Path::new("/c.jpg"),
            10,
            Some(1),
            fid(1, 3, 1),
            256,
            ThumbFit::Contain,
        );

        cache.put(&k1, &sample(1, 100));
        cache.put(&k2, &sample(2, 100));
        // Hit k1 so k2 becomes the least recently used entry.
        assert_eq!(cache.get(&k1), Some(sample(1, 100)));

        // Inserting k3 must evict k2 while keeping k1 and k3.
        cache.put(&k3, &sample(3, 100));
        assert_eq!(cache.get(&k1), Some(sample(1, 100)));
        assert_eq!(cache.get(&k2), None);
        assert_eq!(cache.get(&k3), Some(sample(3, 100)));
    }

    #[test]
    fn cache_hit_succeeds_without_taking_cache_lock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("thumb-cache");
        let cache = ThumbCache::with_limits(root.clone(), 4096, DEFAULT_STALE_TMP_AGE);
        let k1 = cache_key(
            Path::new("/hot.jpg"),
            10,
            Some(1),
            fid(1, 1, 1),
            256,
            ThumbFit::Contain,
        );
        cache.put(&k1, &sample(9, 64));

        // Hold an exclusive lock on `cache.lock` as if another writer is reconciling.
        let _held = FileLock::acquire(root.join(LOCK_FILE), LockMode::Exclusive).unwrap();
        // `get` must STILL succeed immediately without waiting on or failing for the lock.
        assert_eq!(cache.get(&k1), Some(sample(9, 64)));
    }

    #[test]
    fn reconcile_counts_orphaned_files_and_sweeps_stale_tmp_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("thumb-cache");
        let cache = ThumbCache::with_limits(root.clone(), 250, Duration::from_secs(60));
        fs::create_dir_all(root.join("tmp")).unwrap();

        // Drop an "orphaned" `.thumb` file directly on disk (simulating a kill after write) with
        // an old mtime so reconciliation sees it and evicts it once the cap is exceeded.
        let orphan_key = cache_key(
            Path::new("/orphan.jpg"),
            10,
            Some(1),
            fid(1, 99, 1),
            256,
            ThumbFit::Contain,
        );
        let orphan_path = root.join(format!("{orphan_key}.thumb"));
        let encoded_orphan = encode_entry(&sample(4, 150)).unwrap();
        fs::write(&orphan_path, &encoded_orphan).unwrap();
        let old_time = SystemTime::now() - Duration::from_secs(3600);
        OpenOptions::new()
            .write(true)
            .open(&orphan_path)
            .unwrap()
            .set_modified(old_time)
            .unwrap();

        // Create a stale `tmp/dead` file and a fresh `tmp/live` directory.
        let stale_tmp = root.join("tmp").join("dead.png");
        fs::write(&stale_tmp, b"partial").unwrap();
        OpenOptions::new()
            .write(true)
            .open(&stale_tmp)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        let fresh_tmp = root.join("tmp").join("live-dir");
        fs::create_dir_all(&fresh_tmp).unwrap();

        // Putting a new 150-byte entry pushes total past 250 bytes -> orphan is evicted and stale
        // tmp is removed while fresh tmp stays.
        let fresh_key = cache_key(
            Path::new("/fresh.jpg"),
            10,
            Some(1),
            fid(1, 100, 1),
            256,
            ThumbFit::Contain,
        );
        cache.put(&fresh_key, &sample(5, 150));

        assert!(!orphan_path.exists(), "orphaned entry was not evicted");
        assert!(cache.get(&fresh_key).is_some());
        assert!(!stale_tmp.exists(), "stale tmp file was not swept");
        assert!(
            fresh_tmp.exists(),
            "fresh tmp directory was swept too early"
        );
    }

    #[test]
    fn cache_removes_corrupt_entry_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("thumb-cache");
        let cache = ThumbCache::with_limits(root.clone(), 4096, DEFAULT_STALE_TMP_AGE);
        fs::create_dir_all(&root).unwrap();
        let k = cache_key(
            Path::new("/bad.png"),
            5,
            Some(2),
            fid(1, 2, 3),
            128,
            ThumbFit::Cover,
        );
        let p = root.join(format!("{k}.thumb"));
        fs::write(&p, b"garbage").unwrap();

        assert_eq!(cache.get(&k), None);
        assert!(!p.exists());
    }

    #[test]
    fn cache_key_changes_with_every_parameter_including_identity_and_ctime() {
        let base = cache_key(
            Path::new("/a.jpg"),
            100,
            Some(500),
            fid(1, 42, 1000),
            256,
            ThumbFit::Contain,
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/b.jpg"),
                100,
                Some(500),
                fid(1, 42, 1000),
                256,
                ThumbFit::Contain
            )
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/a.jpg"),
                101,
                Some(500),
                fid(1, 42, 1000),
                256,
                ThumbFit::Contain
            )
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/a.jpg"),
                100,
                Some(501),
                fid(1, 42, 1000),
                256,
                ThumbFit::Contain
            )
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/a.jpg"),
                100,
                Some(500),
                fid(1, 43, 1000),
                256,
                ThumbFit::Contain
            ),
            "different inode/file index with identical path, size and mtime must miss cache"
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/a.jpg"),
                100,
                Some(500),
                fid(1, 42, 1001),
                256,
                ThumbFit::Contain
            ),
            "different ctime with identical path, size and mtime must miss cache"
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/a.jpg"),
                100,
                Some(500),
                fid(1, 42, 1000),
                128,
                ThumbFit::Contain
            )
        );
        assert_ne!(
            base,
            cache_key(
                Path::new("/a.jpg"),
                100,
                Some(500),
                fid(1, 42, 1000),
                256,
                ThumbFit::Cover
            )
        );
    }

    #[test]
    fn cache_is_safe_under_concurrent_reads_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let cache =
            ThumbCache::with_limits(dir.path().join("thumb-cache"), 4096, DEFAULT_STALE_TMP_AGE);
        std::thread::scope(|scope| {
            for worker in 0..8u8 {
                let c = cache.clone();
                scope.spawn(move || {
                    for i in 0..16u8 {
                        let key = cache_key(
                            Path::new(&format!("/img_{}_{}.jpg", worker, i % 4)),
                            100,
                            Some(1),
                            fid(1, u64::from(worker) * 10 + u64::from(i % 4), 1),
                            256,
                            ThumbFit::Contain,
                        );
                        c.put(&key, &sample(worker, 200));
                        let _ = c.get(&key);
                    }
                });
            }
        });
        let total: u64 = fs::read_dir(dir.path().join("thumb-cache"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= 4096, "cache exceeded cap: {total}");
    }

    #[test]
    fn for_dir_shares_hints_across_calls_for_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shared-cache");
        let a = ThumbCache::for_dir(root.clone());
        let b = ThumbCache::for_dir(root);
        assert!(
            Arc::ptr_eq(&a.hints, &b.hints),
            "ThumbCache::for_dir must share CacheHints across calls"
        );
        let k = cache_key(
            Path::new("/shared.jpg"),
            10,
            Some(1),
            fid(1, 7, 1),
            256,
            ThumbFit::Contain,
        );
        a.put(&k, &sample(7, 64));
        assert!(
            b.hints.reconciled_once.load(Ordering::Relaxed),
            "reconciliation state must survive across separate handles"
        );
    }

    #[test]
    fn negative_bad_file_entries_round_trip_and_invalidate_on_identity_change() {
        let dir = tempfile::tempdir().unwrap();
        let cache =
            ThumbCache::with_limits(dir.path().join("bad-cache"), 4096, DEFAULT_STALE_TMP_AGE);
        let k1 = bad_cache_key(Path::new("/bomb.gif"), 123, Some(500), fid(1, 10, 100));
        let k2 = bad_cache_key(Path::new("/bomb.gif"), 123, Some(500), fid(1, 11, 100));
        assert_ne!(k1, k2);

        let err = ErrorBody::new(
            legion_core::proto::ErrorCode::Failed,
            "Worker exceeded memory limit.",
        )
        .with_hint("File too large.");
        cache.put_bad(&k1, &err);

        let got = cache.get_bad(&k1).expect("expected cached bad entry");
        assert_eq!(got.code, err.code);
        assert_eq!(got.message, err.message);
        assert_eq!(cache.get_bad(&k2), None);
        // Positive lookup on the same key must not treat a bad entry as a valid thumbnail.
        assert_eq!(cache.get(&k1), None);

        // Entries older than BAD_ENTRY_TTL (7 days) expire, are removed, and return None for retry.
        let k_exp = bad_cache_key(Path::new("/old_bomb.gif"), 123, Some(500), fid(1, 12, 100));
        cache.put_bad_at(&k_exp, &err, 1_000_000);
        assert!(
            cache
                .get_bad_at(&k_exp, 1_000_000 + BAD_ENTRY_TTL.as_secs() - 1)
                .is_some()
        );
        assert_eq!(
            cache.get_bad_at(&k_exp, 1_000_000 + BAD_ENTRY_TTL.as_secs()),
            None
        );
        assert!(
            !cache.entry_path(&k_exp).exists(),
            "expired bad entry must be deleted on miss"
        );
    }
}
