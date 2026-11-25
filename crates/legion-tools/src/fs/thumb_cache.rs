//! Bounded on-disk LRU cache for `fs.thumb` inside the node's state directory.
//!
//! Entries live under `<state_dir>/thumb-cache/` as atomic `<key>.thumb` files alongside an
//! `index.v1` manifest ordered by a monotonic sequence number. A kernel file lock (`cache.lock`)
//! serializes index updates and eviction across concurrent calls and processes without ever being
//! held while an image or video is decoded. Because the cache resides under the node's state
//! directory, non-admin roles cannot read or modify it through `fs.*` tools.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use legion_os::atomic::{WriteOptions, write_atomic};
use legion_os::lock::{FileLock, LockMode};

use super::platform;
use super::thumb::ThumbFit;

/// Default maximum total size of cached thumbnail files on disk (256 MiB).
pub(crate) const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

const CACHE_VERSION: &str = "legion-thumb-v1";
const ENTRY_MAGIC: &[u8; 4] = b"LGTH";
const ENTRY_VERSION: u8 = 1;
const MIME_JPEG_TAG: u8 = 1;
const MIME_PNG_TAG: u8 = 2;
const ENTRY_HEADER_LEN: usize = 14;
const INDEX_FILE: &str = "index.v1";
const LOCK_FILE: &str = "cache.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

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
/// Every component is length- or fixed-width-delimited so two distinct inputs can never collide.
pub(crate) fn cache_key(
    resolved_path: &Path,
    source_size: u64,
    mtime_nanos: Option<i128>,
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
    hasher.update(&thumb_size.to_le_bytes());
    hasher.update(&[match fit {
        ThumbFit::Contain => 0,
        ThumbFit::Cover => 1,
    }]);
    hasher.finalize().to_hex().to_string()
}

/// On-disk LRU thumbnail cache rooted in a single directory.
#[derive(Clone, Debug)]
pub(crate) struct ThumbCache {
    dir: PathBuf,
    max_bytes: u64,
}

impl ThumbCache {
    /// Opens a cache at `dir` capped at [`MAX_CACHE_BYTES`] (256 MiB).
    pub(crate) fn new(dir: PathBuf) -> ThumbCache {
        ThumbCache {
            dir,
            max_bytes: MAX_CACHE_BYTES,
        }
    }

    /// Opens a cache at `dir` with an explicit byte cap (used in tests).
    #[cfg(test)]
    pub(crate) fn with_max_bytes(dir: PathBuf, max_bytes: u64) -> ThumbCache {
        ThumbCache { dir, max_bytes }
    }

    /// Looks up `key` in the cache, updating its LRU recency on hit.
    ///
    /// Returns `None` on miss, corruption, or lock contention so cache errors never fail a
    /// thumbnail call.
    pub(crate) fn get(&self, key: &str) -> Option<CachedThumb> {
        if !self.dir.exists() {
            return None;
        }
        let _lock = self.acquire_lock().ok()?;
        let entry_path = self.entry_path(key);
        let raw = match fs::read(&entry_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                // Keep the index clean if the entry file disappeared externally.
                let mut index = self.load_or_rebuild_index();
                if index.entries.remove(key).is_some() {
                    let _ = self.save_index(&index);
                }
                return None;
            }
        };
        let Some(thumb) = decode_entry(&raw) else {
            let _ = fs::remove_file(&entry_path);
            let mut index = self.load_or_rebuild_index();
            if index.entries.remove(key).is_some() {
                let _ = self.save_index(&index);
            }
            return None;
        };
        let mut index = self.load_or_rebuild_index();
        let seq = index.bump_seq();
        index.entries.insert(
            key.to_owned(),
            IndexEntry {
                size: raw.len() as u64,
                last_used: seq,
            },
        );
        let _ = self.save_index(&index);
        Some(thumb)
    }

    /// Stores `thumb` under `key` and evicts least-recently-used entries until the total cached
    /// bytes fit within `self.max_bytes`.
    pub(crate) fn put(&self, key: &str, thumb: &CachedThumb) {
        let Some(encoded) = encode_entry(thumb) else {
            return;
        };
        let entry_size = encoded.len() as u64;
        if entry_size > self.max_bytes {
            return;
        }
        if self.ensure_dir().is_err() {
            return;
        }
        let Ok(_lock) = self.acquire_lock() else {
            return;
        };
        let entry_path = self.entry_path(key);
        if write_atomic(&entry_path, &encoded, WriteOptions::with_mode(0o600)).is_err() {
            return;
        }
        let mut index = self.load_or_rebuild_index();
        let seq = index.bump_seq();
        index.entries.insert(
            key.to_owned(),
            IndexEntry {
                size: entry_size,
                last_used: seq,
            },
        );
        self.evict_to_cap(&mut index, key);
        let _ = self.save_index(&index);
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

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    fn load_or_rebuild_index(&self) -> CacheIndex {
        let path = self.index_path();
        if let Ok(text) = fs::read_to_string(&path)
            && let Some(index) = CacheIndex::parse(&text)
        {
            return index;
        }
        self.rebuild_index()
    }

    fn rebuild_index(&self) -> CacheIndex {
        let mut discovered: Vec<(String, u64, u128)> = Vec::new();
        if let Ok(read_dir) = fs::read_dir(&self.dir) {
            for item in read_dir.flatten() {
                let name = item.file_name();
                let Some(name_str) = name.to_str() else {
                    continue;
                };
                let Some(key) = name_str.strip_suffix(".thumb") else {
                    continue;
                };
                if !is_valid_key(key) {
                    continue;
                }
                let Ok(meta) = item.metadata() else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let stamp = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                discovered.push((key.to_owned(), meta.len(), stamp));
            }
        }
        discovered.sort_by_key(|(_, _, stamp)| *stamp);
        let mut index = CacheIndex::default();
        for (key, size, _) in discovered {
            let seq = index.bump_seq();
            index.entries.insert(
                key,
                IndexEntry {
                    size,
                    last_used: seq,
                },
            );
        }
        index
    }

    fn evict_to_cap(&self, index: &mut CacheIndex, keep_key: &str) {
        let mut total: u64 = index.entries.values().map(|e| e.size).sum();
        if total <= self.max_bytes {
            return;
        }
        let mut candidates: Vec<(u64, String, u64)> = index
            .entries
            .iter()
            .filter(|(k, _)| k.as_str() != keep_key)
            .map(|(k, e)| (e.last_used, k.clone(), e.size))
            .collect();
        candidates.sort_by_key(|(last_used, _, _)| *last_used);

        for (_, key, size) in candidates {
            if total <= self.max_bytes {
                break;
            }
            index.entries.remove(&key);
            let _ = fs::remove_file(self.entry_path(&key));
            total = total.saturating_sub(size);
        }
    }

    fn save_index(&self, index: &CacheIndex) -> io::Result<()> {
        let serialized = index.serialize();
        write_atomic(
            self.index_path(),
            serialized.as_bytes(),
            WriteOptions::with_mode(0o600),
        )
        .map_err(|err| io::Error::other(err.to_string()))
    }
}

#[derive(Clone, Copy, Debug)]
struct IndexEntry {
    size: u64,
    last_used: u64,
}

#[derive(Debug, Default)]
struct CacheIndex {
    next_seq: u64,
    entries: HashMap<String, IndexEntry>,
}

impl CacheIndex {
    fn bump_seq(&mut self) -> u64 {
        self.next_seq = self.next_seq.saturating_add(1);
        self.next_seq
    }

    fn parse(text: &str) -> Option<CacheIndex> {
        let mut lines = text.lines();
        let header = lines.next()?;
        let next_seq = header.strip_prefix("v1 ")?.parse::<u64>().ok()?;
        let mut entries = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split(' ');
            let key = parts.next()?;
            let size = parts.next()?.parse::<u64>().ok()?;
            let last_used = parts.next()?.parse::<u64>().ok()?;
            if parts.next().is_some() || !is_valid_key(key) {
                return None;
            }
            entries.insert(key.to_owned(), IndexEntry { size, last_used });
        }
        Some(CacheIndex { next_seq, entries })
    }

    fn serialize(&self) -> String {
        let mut out = String::with_capacity(self.entries.len().saturating_mul(84) + 32);
        let _ = writeln_str(&mut out, &format!("v1 {}", self.next_seq));
        for (key, entry) in &self.entries {
            let _ = writeln_str(
                &mut out,
                &format!("{key} {} {}", entry.size, entry.last_used),
            );
        }
        out
    }
}

fn writeln_str(buf: &mut String, line: &str) -> io::Result<()> {
    buf.push_str(line);
    buf.push('\n');
    Ok(())
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

/// Helper to create a throwaway directory for temporary `ffmpeg` frame output inside the cache
/// directory (keeping large scratch out of small memory-backed `/tmp`).
pub(crate) fn temp_dir_in_cache(cache_dir: &Path) -> io::Result<tempfile::TempDir> {
    let tmp_root = cache_dir.join("tmp");
    fs::create_dir_all(&tmp_root)?;
    let _ = platform::set_unix_mode(cache_dir, 0o700);
    let _ = platform::set_unix_mode(&tmp_root, 0o700);
    tempfile::tempdir_in(&tmp_root)
}

/// Touches a file to verify writeability during tests (unused outside tests).
#[cfg(test)]
fn _touch_corrupt(path: &Path, data: &[u8]) {
    use std::io::Write as _;
    let mut f = fs::File::create(path).unwrap();
    f.write_all(data).unwrap();
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

    #[test]
    fn cache_evicts_least_recently_used_and_honors_cap() {
        let dir = tempfile::tempdir().unwrap();
        // Each entry with 100-byte payload is 114 bytes on disk; cap of 250 bytes holds at most 2.
        let cache = ThumbCache::with_max_bytes(dir.path().join("thumb-cache"), 250);
        let k1 = cache_key(Path::new("/a.jpg"), 10, Some(1), 256, ThumbFit::Contain);
        let k2 = cache_key(Path::new("/b.jpg"), 10, Some(1), 256, ThumbFit::Contain);
        let k3 = cache_key(Path::new("/c.jpg"), 10, Some(1), 256, ThumbFit::Contain);

        cache.put(&k1, &sample(1, 100));
        cache.put(&k2, &sample(2, 100));
        // Touch k1 so k2 becomes the least recently used entry.
        assert_eq!(cache.get(&k1), Some(sample(1, 100)));

        // Inserting k3 must evict k2 while keeping k1 and k3.
        cache.put(&k3, &sample(3, 100));
        assert_eq!(cache.get(&k1), Some(sample(1, 100)));
        assert_eq!(cache.get(&k2), None);
        assert_eq!(cache.get(&k3), Some(sample(3, 100)));
    }

    #[test]
    fn cache_recovers_from_corrupt_index_and_entry_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("thumb-cache");
        let cache = ThumbCache::with_max_bytes(root.clone(), 4096);
        let k1 = cache_key(Path::new("/ok.png"), 5, Some(2), 128, ThumbFit::Cover);
        let k2 = cache_key(Path::new("/bad.png"), 5, Some(2), 128, ThumbFit::Cover);

        cache.put(&k1, &sample(7, 64));
        _touch_corrupt(&root.join(INDEX_FILE), b"corrupt index\n");
        _touch_corrupt(&root.join(format!("{k2}.thumb")), b"garbage");

        assert_eq!(cache.get(&k1), Some(sample(7, 64)));
        assert_eq!(cache.get(&k2), None);
        assert!(!root.join(format!("{k2}.thumb")).exists());
    }

    #[test]
    fn cache_key_changes_with_every_parameter() {
        let base = cache_key(Path::new("/a.jpg"), 100, Some(500), 256, ThumbFit::Contain);
        assert_ne!(
            base,
            cache_key(Path::new("/b.jpg"), 100, Some(500), 256, ThumbFit::Contain)
        );
        assert_ne!(
            base,
            cache_key(Path::new("/a.jpg"), 101, Some(500), 256, ThumbFit::Contain)
        );
        assert_ne!(
            base,
            cache_key(Path::new("/a.jpg"), 100, Some(501), 256, ThumbFit::Contain)
        );
        assert_ne!(
            base,
            cache_key(Path::new("/a.jpg"), 100, Some(500), 128, ThumbFit::Contain)
        );
        assert_ne!(
            base,
            cache_key(Path::new("/a.jpg"), 100, Some(500), 256, ThumbFit::Cover)
        );
    }

    #[test]
    fn cache_is_safe_under_concurrent_reads_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::with_max_bytes(dir.path().join("thumb-cache"), 4096);
        std::thread::scope(|scope| {
            for worker in 0..8u8 {
                let c = cache.clone();
                scope.spawn(move || {
                    for i in 0..16u8 {
                        let key = cache_key(
                            Path::new(&format!("/img_{}_{}.jpg", worker, i % 4)),
                            100,
                            Some(1),
                            256,
                            ThumbFit::Contain,
                        );
                        c.put(&key, &sample(worker, 200));
                        let _ = c.get(&key);
                    }
                });
            }
        });
        // Total size of surviving `.thumb` files never exceeds the cap.
        let total: u64 = fs::read_dir(dir.path().join("thumb-cache"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".thumb"))
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(total <= 4096, "cache exceeded cap: {total}");
    }
}
