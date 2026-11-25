//! Deterministic multi-member synchronization simulator proving the engine's six invariants
//! across arbitrary partitions, clock skew, crashes between any two apply steps, member joins and
//! leaves, file-vs-directory replacements, bin expiry, and heterogeneous filesystem rules.
//!
//! Replay any failing seed with:
//! ```text
//! LEGION_SYNC_SEED=<seed> cargo test -p legion-sync --test simulator replay_seed -- --nocapture
//! ```
//!
//! Run long mode (default 100,000 seeds; override with `LEGION_SYNC_SEEDS=1000000`):
//! ```text
//! cargo test -p legion-sync --release --test simulator simulator_long_mode -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Read, Write};
use std::time::Instant;

use legion_sync::{
    FolderConfig, FsCapabilities, FsErrorKind, FsMeta, FsWalkEntry, FsWalkResult, IgnoreMatcher,
    IndexStore, MemberState, MemoryIndexStore, Peer, RecordsPage, SyncEngine, SyncError, SyncFs,
    SyncWrite, is_internal_sync_path, validate_sync_path,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};

/// Deterministic fast PRNG (`SplitMix64`) so schedules replay identically on every platform.
#[derive(Debug, Clone)]
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            return 0;
        }
        (self.next_u64() as usize) % upper
    }

    fn range_i64(&mut self, low: i64, high: i64) -> i64 {
        let span = (high - low) as u64 + 1;
        low + (self.next_u64() % span) as i64
    }

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        (self.next_u64() % denominator) < numerator
    }
}

/// Live entry stored inside [`SimFs`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum SimNode {
    File {
        bytes: Vec<u8>,
        mtime: Timestamp,
        mode: Option<u32>,
        gen_id: u64,
    },
    Dir {
        mtime: Timestamp,
    },
    Symlink {
        target: String,
        mtime: Timestamp,
    },
}

impl SimNode {
    fn meta(&self) -> FsMeta {
        match self {
            SimNode::File {
                bytes,
                mtime,
                mode,
                gen_id,
            } => FsMeta {
                kind: EntryKind::File,
                size: bytes.len() as u64,
                mtime: *mtime,
                mode: *mode,
                link_target: None,
                change_id: Some(*gen_id),
            },
            SimNode::Dir { mtime } => FsMeta {
                kind: EntryKind::Dir,
                size: 0,
                mtime: *mtime,
                mode: Some(0o755),
                link_target: None,
                change_id: None,
            },
            SimNode::Symlink { target, mtime } => FsMeta {
                kind: EntryKind::Symlink,
                size: 0,
                mtime: *mtime,
                mode: Some(0o777),
                link_target: Some(target.clone()),
                change_id: None,
            },
        }
    }
}

/// Canonical tree representation used for convergence assertions (Invariant a).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeEntry {
    File(Hash),
    Dir,
    Symlink(String),
}

/// Send-safe writer staging into the in-memory `.legion-sync/tmp` map on `finish_sync`.
struct SyncSimWriter {
    buf: Vec<u8>,
    name: String,
    target_tmp: std::sync::Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>,
    crash_on_sync: bool,
}

impl Write for SyncSimWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SyncWrite for SyncSimWriter {
    fn finish_sync(self: Box<Self>) -> legion_sync::Result<()> {
        if self.crash_on_sync {
            return Err(ThreadSafeSimFs::crash_err("finish_sync"));
        }
        let mut map = self.target_tmp.lock().unwrap();
        map.insert(self.name, self.buf);
        Ok(())
    }
}

/// Clean Send-safe in-memory filesystem using `Mutex` (zero `unsafe`).
struct ThreadSafeSimFs {
    caps: FsCapabilities,
    live: std::sync::Mutex<BTreeMap<String, SimNode>>,
    tmp: std::sync::Arc<std::sync::Mutex<BTreeMap<String, Vec<u8>>>>,
    bin: std::sync::Mutex<BTreeMap<String, Vec<u8>>>,
    next_gen: Cell<u64>,
    root_escape_detected: Cell<bool>,
    crash_countdown: Cell<Option<usize>>,
    crash_after: Cell<bool>,
    did_crash: Cell<bool>,
}

impl ThreadSafeSimFs {
    fn new(caps: FsCapabilities) -> Self {
        Self {
            caps,
            live: std::sync::Mutex::new(BTreeMap::new()),
            tmp: std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            bin: std::sync::Mutex::new(BTreeMap::new()),
            next_gen: Cell::new(1),
            root_escape_detected: Cell::new(false),
            crash_countdown: Cell::new(None),
            crash_after: Cell::new(false),
            did_crash: Cell::new(false),
        }
    }

    fn alloc_gen(&self) -> u64 {
        let g = self.next_gen.get();
        self.next_gen.set(g + 1);
        g
    }

    fn arm_crash(&self, countdown: usize, after_op: bool) {
        self.crash_countdown.set(Some(countdown));
        self.crash_after.set(after_op);
        self.did_crash.set(false);
    }

    fn disarm_crash(&self) {
        self.crash_countdown.set(None);
    }

    fn crashed(&self) -> bool {
        self.did_crash.get()
    }

    fn tick_crash(&self) -> bool {
        if let Some(left) = self.crash_countdown.get() {
            if left == 0 {
                self.crash_countdown.set(None);
                self.did_crash.set(true);
                return true;
            }
            self.crash_countdown.set(Some(left - 1));
        }
        false
    }

    fn crash_err(op: &str) -> SyncError {
        SyncError::Fs {
            path: Some(op.to_string()),
            detail: "simulated crash".to_string(),
            kind: FsErrorKind::Crash,
        }
    }

    fn wrap_mut<T>(
        &self,
        op: &str,
        f: impl FnOnce() -> legion_sync::Result<T>,
    ) -> legion_sync::Result<T> {
        let crash = self.tick_crash();
        if crash && !self.crash_after.get() {
            return Err(Self::crash_err(op));
        }
        let res = f()?;
        if crash && self.crash_after.get() {
            return Err(Self::crash_err(op));
        }
        Ok(res)
    }

    fn check_live_path(&self, rel: &str) -> legion_sync::Result<String> {
        if is_internal_sync_path(rel) {
            self.root_escape_detected.set(true);
            return Err(SyncError::InvalidPath {
                path: rel.to_string(),
                reason: "internal sync path passed to live fs".to_string(),
            });
        }
        match validate_sync_path(rel) {
            Ok(clean) => Ok(clean),
            Err(err) => {
                self.root_escape_detected.set(true);
                Err(err)
            }
        }
    }

    fn user_write_file(&self, path: &str, bytes: Vec<u8>, mtime: Timestamp) -> Option<Hash> {
        let clean = validate_sync_path(path).unwrap();
        let mut live = self.live.lock().unwrap();
        for anc in legion_sync::path::ancestors(&clean) {
            live.entry(anc.to_string())
                .or_insert(SimNode::Dir { mtime });
        }
        let prev_hash = match live.get(&clean) {
            Some(SimNode::File { bytes: old, .. }) => Some(hash_bytes(old)),
            _ => None,
        };
        live.insert(
            clean,
            SimNode::File {
                bytes,
                mtime,
                mode: if self.caps.windows_naming {
                    None
                } else {
                    Some(0o644)
                },
                gen_id: self.alloc_gen(),
            },
        );
        prev_hash
    }

    fn user_mkdir(&self, path: &str, mtime: Timestamp) {
        let clean = validate_sync_path(path).unwrap();
        let mut live = self.live.lock().unwrap();
        for anc in legion_sync::path::ancestors(&clean) {
            live.entry(anc.to_string())
                .or_insert(SimNode::Dir { mtime });
        }
        live.insert(clean, SimNode::Dir { mtime });
    }

    fn user_symlink(&self, path: &str, target: String, mtime: Timestamp) {
        let clean = validate_sync_path(path).unwrap();
        let mut live = self.live.lock().unwrap();
        for anc in legion_sync::path::ancestors(&clean) {
            live.entry(anc.to_string())
                .or_insert(SimNode::Dir { mtime });
        }
        live.insert(clean, SimNode::Symlink { target, mtime });
    }

    fn user_remove_recursive(&self, path: &str) -> Vec<Hash> {
        let mut live = self.live.lock().unwrap();
        let prefix = format!("{path}/");
        let keys: Vec<String> = live
            .keys()
            .filter(|k| *k == path || k.starts_with(&prefix))
            .cloned()
            .collect();
        let mut removed = Vec::new();
        for k in keys {
            if let Some(SimNode::File { bytes, .. }) = live.remove(&k) {
                removed.push(hash_bytes(&bytes));
            }
        }
        removed
    }

    fn user_rename_file(&self, from: &str, to: &str, mtime: Timestamp) -> Option<Hash> {
        let mut live = self.live.lock().unwrap();
        let node = live.remove(from)?;
        let prev_to = match live.get(to) {
            Some(SimNode::File { bytes, .. }) => Some(hash_bytes(bytes)),
            _ => None,
        };
        for anc in legion_sync::path::ancestors(to) {
            live.entry(anc.to_string())
                .or_insert(SimNode::Dir { mtime });
        }
        live.insert(to.to_string(), node);
        prev_to
    }

    fn live_paths(&self) -> Vec<(String, EntryKind)> {
        self.live
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.meta().kind))
            .collect()
    }

    fn live_tree(&self) -> BTreeMap<String, TreeEntry> {
        self.live
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let entry = match v {
                    SimNode::File { bytes, .. } => TreeEntry::File(hash_bytes(bytes)),
                    SimNode::Dir { .. } => TreeEntry::Dir,
                    SimNode::Symlink { target, .. } => TreeEntry::Symlink(target.clone()),
                };
                (k.clone(), entry)
            })
            .collect()
    }

    fn all_live_hashes(&self) -> BTreeSet<Hash> {
        self.live
            .lock()
            .unwrap()
            .values()
            .filter_map(|n| match n {
                SimNode::File { bytes, .. } => Some(hash_bytes(bytes)),
                _ => None,
            })
            .collect()
    }

    fn all_bin_hashes(&self) -> BTreeSet<Hash> {
        self.bin
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.ends_with(".data"))
            .map(|(_, bytes)| hash_bytes(bytes))
            .collect()
    }

    fn root_escaped(&self) -> bool {
        self.root_escape_detected.get()
    }

    fn has_ancestor_conflict(live: &BTreeMap<String, SimNode>, rel: &str) -> bool {
        for anc in legion_sync::path::ancestors(rel) {
            if let Some(node) = live.get(anc)
                && !matches!(node, SimNode::Dir { .. })
            {
                return true;
            }
        }
        false
    }
}

impl SyncFs for ThreadSafeSimFs {
    fn capabilities(&self) -> FsCapabilities {
        self.caps
    }

    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        let live = self.live.lock().unwrap();
        let mut entries = Vec::new();
        for (path, node) in live.iter() {
            let meta = node.meta();
            if ignore.is_path_or_ancestor_ignored(path, meta.kind == EntryKind::Dir) {
                continue;
            }
            entries.push(FsWalkEntry {
                path: path.clone(),
                meta,
            });
        }
        Ok(FsWalkResult {
            entries,
            skipped: Vec::new(),
        })
    }

    fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
        let clean = self.check_live_path(rel_path)?;
        let live = self.live.lock().unwrap();
        for anc in legion_sync::path::ancestors(&clean) {
            match live.get(anc) {
                Some(SimNode::Symlink { .. }) => {
                    return Err(SyncError::InvalidPath {
                        path: clean,
                        reason: "ancestor is a symlink".to_string(),
                    });
                }
                Some(SimNode::File { .. }) => return Ok(None),
                _ => {}
            }
        }
        Ok(live.get(&clean).map(SimNode::meta))
    }

    fn hash_live(&self, rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
        let clean = self.check_live_path(rel_path)?;
        let live = self.live.lock().unwrap();
        match live.get(&clean) {
            Some(SimNode::File { bytes, .. }) => Ok((hash_bytes(bytes), bytes.len() as u64)),
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a regular file".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn open_live_read(&self, rel_path: &str) -> legion_sync::Result<Box<dyn Read + Send>> {
        let clean = self.check_live_path(rel_path)?;
        let live = self.live.lock().unwrap();
        match live.get(&clean) {
            Some(SimNode::File { bytes, .. }) => Ok(Box::new(Cursor::new(bytes.clone()))),
            Some(_) => Err(SyncError::Conflict {
                path: clean,
                reason: "not a regular file".to_string(),
            }),
            None => Err(SyncError::Fs {
                path: Some(clean),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn create_dir_all_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = self.check_live_path(rel_path)?;
        self.wrap_mut("create_dir_all_live", || {
            let mut live = self.live.lock().unwrap();
            if Self::has_ancestor_conflict(&live, &clean) {
                return Err(SyncError::Conflict {
                    path: clean.clone(),
                    reason: "ancestor is not a directory".to_string(),
                });
            }
            for anc in legion_sync::path::ancestors(&clean) {
                live.entry(anc.to_string()).or_insert(SimNode::Dir {
                    mtime: Timestamp::new(0, 0),
                });
            }
            match live.get(&clean) {
                Some(SimNode::Dir { .. }) => Ok(()),
                Some(_) => Err(SyncError::Conflict {
                    path: clean,
                    reason: "non-directory exists".to_string(),
                }),
                None => {
                    live.insert(
                        clean,
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                    Ok(())
                }
            }
        })
    }

    fn list_dir_live(&self, rel_path: &str) -> legion_sync::Result<Vec<String>> {
        let clean = self.check_live_path(rel_path)?;
        let live = self.live.lock().unwrap();
        let prefix = format!("{clean}/");
        let mut names = BTreeSet::new();
        for key in live.keys() {
            if let Some(rest) = key.strip_prefix(&prefix) {
                let first = rest.split('/').next().unwrap_or(rest);
                names.insert(first.to_string());
            }
        }
        Ok(names.into_iter().collect())
    }

    fn remove_empty_dir_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = self.check_live_path(rel_path)?;
        self.wrap_mut("remove_empty_dir_live", || {
            let mut live = self.live.lock().unwrap();
            let prefix = format!("{clean}/");
            if live.keys().any(|k| k.starts_with(&prefix)) {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "directory not empty".to_string(),
                    kind: FsErrorKind::DirectoryNotEmpty,
                });
            }
            match live.remove(&clean) {
                Some(SimNode::Dir { .. }) => Ok(()),
                Some(other) => {
                    live.insert(clean.clone(), other);
                    Err(SyncError::Conflict {
                        path: clean,
                        reason: "not a directory".to_string(),
                    })
                }
                None => Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                }),
            }
        })
    }

    fn create_symlink_live(&self, rel_path: &str, target: &str) -> legion_sync::Result<()> {
        let clean = self.check_live_path(rel_path)?;
        if !self.caps.supports_symlinks {
            return Err(SyncError::Unsupported {
                reason: "symlinks unsupported".to_string(),
                hint: None,
            });
        }
        self.wrap_mut("create_symlink_live", || {
            let mut live = self.live.lock().unwrap();
            if Self::has_ancestor_conflict(&live, &clean) {
                return Err(SyncError::Conflict {
                    path: clean.clone(),
                    reason: "ancestor is not a directory".to_string(),
                });
            }
            for anc in legion_sync::path::ancestors(&clean) {
                live.entry(anc.to_string()).or_insert(SimNode::Dir {
                    mtime: Timestamp::new(0, 0),
                });
            }
            live.insert(
                clean,
                SimNode::Symlink {
                    target: target.to_string(),
                    mtime: Timestamp::new(0, 0),
                },
            );
            Ok(())
        })
    }

    fn remove_symlink_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = self.check_live_path(rel_path)?;
        self.wrap_mut("remove_symlink_live", || {
            let mut live = self.live.lock().unwrap();
            match live.remove(&clean) {
                Some(SimNode::Symlink { .. }) => Ok(()),
                Some(other) => {
                    live.insert(clean.clone(), other);
                    Err(SyncError::Conflict {
                        path: clean,
                        reason: "not a symlink".to_string(),
                    })
                }
                None => Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                }),
            }
        })
    }

    fn set_live_metadata(
        &self,
        rel_path: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        let clean = self.check_live_path(rel_path)?;
        self.wrap_mut("set_live_metadata", || {
            let mut live = self.live.lock().unwrap();
            if let Some(SimNode::File {
                mtime: m, mode: md, ..
            }) = live.get_mut(&clean)
            {
                *m = mtime;
                if !self.caps.windows_naming && mode.is_some() {
                    *md = mode;
                }
            }
            Ok(())
        })
    }

    fn rename_live_to_live(&self, from_rel: &str, to_rel: &str) -> legion_sync::Result<()> {
        let clean_from = self.check_live_path(from_rel)?;
        let clean_to = self.check_live_path(to_rel)?;
        self.wrap_mut("rename_live_to_live", || {
            let mut live = self.live.lock().unwrap();
            let Some(node) = live.remove(&clean_from) else {
                return Err(SyncError::Fs {
                    path: Some(clean_from),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            for anc in legion_sync::path::ancestors(&clean_to) {
                live.entry(anc.to_string()).or_insert(SimNode::Dir {
                    mtime: Timestamp::new(0, 0),
                });
            }
            live.insert(clean_to, node);
            Ok(())
        })
    }

    fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        legion_sync::path::validate_flat_internal_name(tmp_name)?;
        let crash = self.tick_crash();
        if crash && !self.crash_after.get() {
            return Err(Self::crash_err("create_tmp"));
        }
        Ok(Box::new(SyncSimWriter {
            buf: Vec::new(),
            name: tmp_name.to_string(),
            target_tmp: self.tmp.clone(),
            crash_on_sync: crash && self.crash_after.get(),
        }))
    }

    fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        legion_sync::path::validate_flat_internal_name(tmp_name)?;
        let tmp = self.tmp.lock().unwrap();
        Ok(tmp.get(tmp_name).map(|b| FsMeta {
            kind: EntryKind::File,
            size: b.len() as u64,
            mtime: Timestamp::new(0, 0),
            mode: Some(0o600),
            link_target: None,
            change_id: None,
        }))
    }

    fn hash_tmp(&self, tmp_name: &str) -> legion_sync::Result<(Hash, u64)> {
        legion_sync::path::validate_flat_internal_name(tmp_name)?;
        let tmp = self.tmp.lock().unwrap();
        match tmp.get(tmp_name) {
            Some(bytes) => Ok((hash_bytes(bytes), bytes.len() as u64)),
            None => Err(SyncError::Fs {
                path: Some(tmp_name.to_string()),
                detail: "tmp not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn remove_tmp(&self, tmp_name: &str) -> legion_sync::Result<()> {
        legion_sync::path::validate_flat_internal_name(tmp_name)?;
        self.tmp.lock().unwrap().remove(tmp_name);
        Ok(())
    }

    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.tmp.lock().unwrap().keys().cloned().collect())
    }

    fn commit_tmp_to_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<()> {
        legion_sync::path::validate_flat_internal_name(tmp_name)?;
        let clean = self.check_live_path(live_rel)?;
        self.wrap_mut("commit_tmp_to_live", || {
            let bytes = self
                .tmp
                .lock()
                .unwrap()
                .remove(tmp_name)
                .ok_or_else(|| SyncError::Fs {
                    path: Some(tmp_name.to_string()),
                    detail: "tmp not found".to_string(),
                    kind: FsErrorKind::NotFound,
                })?;
            let mut live = self.live.lock().unwrap();
            if Self::has_ancestor_conflict(&live, &clean) {
                return Err(SyncError::Conflict {
                    path: clean.clone(),
                    reason: "ancestor is not a directory".to_string(),
                });
            }
            for anc in legion_sync::path::ancestors(&clean) {
                live.entry(anc.to_string()).or_insert(SimNode::Dir {
                    mtime: Timestamp::new(0, 0),
                });
            }
            live.insert(
                clean,
                SimNode::File {
                    bytes,
                    mtime: Timestamp::new(0, 0),
                    mode: if self.caps.windows_naming {
                        None
                    } else {
                        Some(0o644)
                    },
                    gen_id: self.alloc_gen(),
                },
            );
            Ok(())
        })
    }

    fn move_live_to_bin(&self, live_rel: &str, bin_data_name: &str) -> legion_sync::Result<()> {
        let clean = self.check_live_path(live_rel)?;
        legion_sync::path::validate_flat_internal_name(bin_data_name)?;
        self.wrap_mut("move_live_to_bin", || {
            let mut live = self.live.lock().unwrap();
            let node = live.remove(&clean).ok_or_else(|| SyncError::Fs {
                path: Some(clean.clone()),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?;
            match node {
                SimNode::File { bytes, .. } => {
                    self.bin
                        .lock()
                        .unwrap()
                        .insert(bin_data_name.to_string(), bytes);
                    Ok(())
                }
                other => {
                    live.insert(clean.clone(), other);
                    Err(SyncError::Conflict {
                        path: clean,
                        reason: "expected file to move to bin".to_string(),
                    })
                }
            }
        })
    }

    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        legion_sync::path::validate_flat_internal_name(meta_name)?;
        self.wrap_mut("write_bin_meta", || {
            self.bin
                .lock()
                .unwrap()
                .insert(meta_name.to_string(), bytes.to_vec());
            Ok(())
        })
    }

    fn read_bin_meta(&self, meta_name: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        legion_sync::path::validate_flat_internal_name(meta_name)?;
        Ok(self.bin.lock().unwrap().get(meta_name).cloned())
    }

    fn stat_bin(&self, bin_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        legion_sync::path::validate_flat_internal_name(bin_name)?;
        Ok(self.bin.lock().unwrap().get(bin_name).map(|b| FsMeta {
            kind: EntryKind::File,
            size: b.len() as u64,
            mtime: Timestamp::new(0, 0),
            mode: Some(0o600),
            link_target: None,
            change_id: None,
        }))
    }

    fn hash_bin(&self, bin_name: &str) -> legion_sync::Result<(Hash, u64)> {
        legion_sync::path::validate_flat_internal_name(bin_name)?;
        match self.bin.lock().unwrap().get(bin_name) {
            Some(bytes) => Ok((hash_bytes(bytes), bytes.len() as u64)),
            None => Err(SyncError::Fs {
                path: Some(bin_name.to_string()),
                detail: "bin file not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> legion_sync::Result<()> {
        legion_sync::path::validate_flat_internal_name(bin_data_name)?;
        legion_sync::path::validate_flat_internal_name(tmp_name)?;
        let bytes = self
            .bin
            .lock()
            .unwrap()
            .get(bin_data_name)
            .cloned()
            .ok_or_else(|| SyncError::Fs {
                path: Some(bin_data_name.to_string()),
                detail: "bin file not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?;
        self.tmp.lock().unwrap().insert(tmp_name.to_string(), bytes);
        Ok(())
    }

    fn remove_bin_file(&self, bin_name: &str) -> legion_sync::Result<()> {
        legion_sync::path::validate_flat_internal_name(bin_name)?;
        self.bin.lock().unwrap().remove(bin_name);
        Ok(())
    }

    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.bin.lock().unwrap().keys().cloned().collect())
    }
}

/// Peer adapter wrapping another member's engine and enforcing Invariant (f) on every response.
struct SimPeer<'a> {
    engine: &'a SyncEngine<ThreadSafeSimFs, MemoryIndexStore>,
    caller: String,
    member_id: String,
    leaked_internal: Cell<bool>,
}

impl<'a> SimPeer<'a> {
    fn new(
        engine: &'a SyncEngine<ThreadSafeSimFs, MemoryIndexStore>,
        caller: &str,
        member_id: &str,
    ) -> Self {
        Self {
            engine,
            caller: caller.to_string(),
            member_id: member_id.to_string(),
            leaked_internal: Cell::new(false),
        }
    }
}

impl Peer for SimPeer<'_> {
    fn member_id(&self) -> &str {
        &self.member_id
    }

    fn records_since(&self, since_seq: u64, limit: usize) -> legion_sync::Result<RecordsPage> {
        let page = self
            .engine
            .serve_records_since(&self.caller, since_seq, limit)?;
        for rec in &page.records {
            if is_internal_sync_path(&rec.path) || validate_sync_path(&rec.path).is_err() {
                self.leaked_internal.set(true);
            }
        }
        Ok(page)
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        if is_internal_sync_path(path) || validate_sync_path(path).is_err() {
            self.leaked_internal.set(true);
        }
        self.engine
            .serve_fetch_file(&self.caller, path, expected_hash)
    }
}

struct SimMember {
    id: String,
    engine: SyncEngine<ThreadSafeSimFs, MemoryIndexStore>,
    /// Clock offset in seconds relative to global simulation time (minutes to days, both signs).
    clock_skew_secs: i64,
    active: bool,
    /// Partition group id (members with the same group id can talk to each other).
    group: usize,
}

impl SimMember {
    fn local_time(&self, global_secs: i64) -> Timestamp {
        Timestamp::new(global_secs + self.clock_skew_secs, 0)
    }
}

/// Tracking state per unique content hash ever created in a live tree (Invariant b).
#[derive(Debug, Clone)]
struct ContentRecord {
    /// Every member where this content hash was ever present in a live tree.
    ever_live_on: BTreeSet<String>,
    /// Members where a person directly deleted or overwrote a live copy of this hash.
    person_removed_on: BTreeSet<String>,
    /// Members where bin expiry removed a bin entry holding this hash.
    bin_expired_on: BTreeSet<String>,
}

struct SimWorld {
    root_seed: u64,
    scenario: &'static str,
    seed: u64,
    rng: Rng,
    global_secs: i64,
    next_content_id: u64,
    members: Vec<SimMember>,
    contents: BTreeMap<Hash, ContentRecord>,
    /// Paths that have been confirmed deleted everywhere (Invariant c); cleared only if a person
    /// explicitly recreates that path in the schedule.
    universally_deleted: BTreeSet<String>,
    record_trace: bool,
    trace: Vec<String>,
    heterogeneous: bool,
}

const PATH_POOL: &[&str] = &[
    "alpha.txt",
    "beta.txt",
    "notes.md",
    "dir1/a.txt",
    "dir1/b.txt",
    "dir1/sub/deep.txt",
    "dir2/report.txt",
    "dir2/data.bin",
    "swap",
    "swap/child.txt",
    "swap/nested/leaf.txt",
    "item",
    "item/inside.txt",
];

impl SimWorld {
    fn new(
        root_seed: u64,
        scenario: &'static str,
        seed: u64,
        heterogeneous: bool,
        record_trace: bool,
    ) -> Self {
        let mut rng = Rng::new(seed);
        let n = 2 + rng.range(5); // 2..=6 members
        let base_secs = 1_795_608_000i64; // 2026-11-25 12:00:00 UTC

        let member_names: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
        let mut members = Vec::with_capacity(n);

        for (i, id) in member_names.iter().enumerate() {
            let caps = if heterogeneous {
                match i % 3 {
                    0 => FsCapabilities::unix(),
                    1 => FsCapabilities::macos_case_insensitive(),
                    _ => FsCapabilities::windows(),
                }
            } else {
                FsCapabilities::unix()
            };
            let cfg = FolderConfig::new("sim", id.clone(), member_names.clone());
            let fs = ThreadSafeSimFs::new(caps);
            let store = MemoryIndexStore::new();
            let engine = SyncEngine::open(cfg, fs, store).unwrap();
            // Clock skew from -3 days to +3 days (includes minute-scale and day-scale skew).
            let skew = if rng.chance(1, 2) {
                rng.range_i64(-3 * 86_400, 3 * 86_400)
            } else {
                rng.range_i64(-3600, 3600)
            };
            members.push(SimMember {
                id: id.clone(),
                engine,
                clock_skew_secs: skew,
                active: true,
                group: 0,
            });
        }

        let mut trace = Vec::new();
        if record_trace {
            trace.push(format!(
                "init root_seed={root_seed} scenario={scenario} seed={seed} members={n} heterogeneous={heterogeneous} skews={:?}",
                members
                    .iter()
                    .map(|m| (m.id.as_str(), m.clock_skew_secs))
                    .collect::<Vec<_>>()
            ));
        }

        Self {
            root_seed,
            scenario,
            seed,
            rng,
            global_secs: base_secs,
            next_content_id: 1,
            members,
            contents: BTreeMap::new(),
            universally_deleted: BTreeSet::new(),
            record_trace,
            trace,
            heterogeneous,
        }
    }

    fn log(&mut self, msg: impl FnOnce() -> String) {
        if self.record_trace {
            self.trace.push(msg());
        }
    }

    fn fail(&self, msg: String) -> ! {
        if !self.record_trace {
            // Re-run this exact root seed with tracing enabled so the failure always prints a full
            // minimal step-by-step trace.
            run_one_seed(self.root_seed, true);
        }
        let trace_dump = self.trace.join("\n  ");
        panic!(
            "\n=== SIMULATOR INVARIANT VIOLATION ===\n\
             seed: {} (scenario: {})\n\
             replay: LEGION_SYNC_SEED={} cargo test -p legion-sync --test simulator replay_seed -- --nocapture\n\
             violation: {}\n\
             trace ({} steps):\n  {}\n",
            self.root_seed,
            self.scenario,
            self.root_seed,
            msg,
            self.trace.len(),
            trace_dump
        );
    }

    fn active_indices(&self) -> Vec<usize> {
        self.members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.active)
            .map(|(i, _)| i)
            .collect()
    }

    fn fresh_bytes(&mut self, member_id: &str) -> (Vec<u8>, Hash) {
        let id = self.next_content_id;
        self.next_content_id += 1;
        let bytes = format!("s{}:c{}:by:{member_id}", self.seed, id).into_bytes();
        let hash = hash_bytes(&bytes);
        (bytes, hash)
    }

    fn record_live_presence(&mut self) {
        for m in &self.members {
            if !m.active {
                continue;
            }
            for h in m.engine.fs().all_live_hashes() {
                if let Some(rec) = self.contents.get_mut(&h) {
                    rec.ever_live_on.insert(m.id.clone());
                }
            }
        }
    }

    fn note_created(&mut self, member_id: &str, hash: Hash) {
        let rec = self.contents.entry(hash).or_insert_with(|| ContentRecord {
            ever_live_on: BTreeSet::new(),
            person_removed_on: BTreeSet::new(),
            bin_expired_on: BTreeSet::new(),
        });
        rec.ever_live_on.insert(member_id.to_string());
    }

    fn note_person_removed(&mut self, member_id: &str, hash: Hash) {
        if let Some(rec) = self.contents.get_mut(&hash) {
            rec.person_removed_on.insert(member_id.to_string());
        }
    }

    fn pick_path(&mut self) -> String {
        PATH_POOL[self.rng.range(PATH_POOL.len())].to_string()
    }

    /// Ensures path can be written cleanly on `member` without violating FS structure (if an
    /// ancestor is a file/symlink, or path is a dir, handle via explicit remove first).
    fn user_create_or_edit_file(&mut self, idx: usize, path: &str, also_scan: bool) {
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let (bytes, hash) = self.fresh_bytes(&member_id);

        // If any strict ancestor is currently a non-dir on this member, skip to avoid invalid FS
        // state, or remove non-dir ancestor cleanly as a person action.
        let existing_paths = self.members[idx].engine.fs().live_paths();
        for anc in legion_sync::path::ancestors(path) {
            if existing_paths
                .iter()
                .any(|(p, k)| p == anc && *k != EntryKind::Dir)
            {
                let removed = self.members[idx].engine.fs().user_remove_recursive(anc);
                for h in removed {
                    self.note_person_removed(&member_id, h);
                }
            }
        }
        // If `path` is currently a directory, remove its tree first (simulates replacing dir with file).
        if existing_paths
            .iter()
            .any(|(p, k)| p == path && *k == EntryKind::Dir)
        {
            let removed = self.members[idx].engine.fs().user_remove_recursive(path);
            for h in removed {
                self.note_person_removed(&member_id, h);
            }
        }

        let overwritten = self.members[idx]
            .engine
            .fs()
            .user_write_file(path, bytes, now);
        if let Some(old_h) = overwritten {
            self.note_person_removed(&member_id, old_h);
        }
        self.note_created(&member_id, hash);
        self.universally_deleted.remove(path);
        for anc in legion_sync::path::ancestors(path) {
            self.universally_deleted.remove(anc);
        }

        if also_scan {
            self.members[idx].engine.scan(now).unwrap();
        }
        self.log(|| {
            format!(
                "{member_id} write {path:?} hash={} scan={also_scan} t={}",
                &hash.to_hex()[..8],
                now.secs
            )
        });
    }

    fn user_delete_path(&mut self, idx: usize, path: &str, also_scan: bool) {
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let removed = self.members[idx].engine.fs().user_remove_recursive(path);
        for h in removed {
            self.note_person_removed(&member_id, h);
        }
        if also_scan {
            self.members[idx].engine.scan(now).unwrap();
        }
        self.log(|| {
            format!(
                "{member_id} delete {path:?} scan={also_scan} t={}",
                now.secs
            )
        });
    }

    fn user_replace_file_with_dir(&mut self, idx: usize, path: &str, child_name: &str) {
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let removed = self.members[idx].engine.fs().user_remove_recursive(path);
        for h in removed {
            self.note_person_removed(&member_id, h);
        }
        self.members[idx].engine.fs().user_mkdir(path, now);
        let child_path = format!("{path}/{child_name}");
        let (bytes, hash) = self.fresh_bytes(&member_id);
        self.members[idx]
            .engine
            .fs()
            .user_write_file(&child_path, bytes, now);
        self.note_created(&member_id, hash);
        self.universally_deleted.remove(path);
        self.universally_deleted.remove(&child_path);
        self.members[idx].engine.scan(now).unwrap();
        self.log(|| {
            format!(
                "{member_id} file->dir {path:?} + {child_path:?} hash={}",
                &hash.to_hex()[..8]
            )
        });
    }

    fn sync_pair(&mut self, dst_idx: usize, src_idx: usize, inject_crash: bool) {
        if dst_idx == src_idx
            || !self.members[dst_idx].active
            || !self.members[src_idx].active
            || self.members[dst_idx].group != self.members[src_idx].group
        {
            return;
        }

        let dst_id = self.members[dst_idx].id.clone();
        let src_id = self.members[src_idx].id.clone();
        let now = self.members[dst_idx].local_time(self.global_secs);

        // Split mutable borrow of `members[dst_idx]` and immutable borrow of `members[src_idx]`.
        let (crash_op, crash_after) = if inject_crash {
            (Some(self.rng.range(8)), self.rng.chance(1, 2))
        } else {
            (None, false)
        };

        if let Some(op) = crash_op {
            self.members[dst_idx].engine.fs().arm_crash(op, crash_after);
        }

        let (res, leaked) = if dst_idx < src_idx {
            let (left, right) = self.members.split_at_mut(src_idx);
            let dst = &mut left[dst_idx];
            let src = &right[0];
            let peer = SimPeer::new(&src.engine, &dst.id, &src.id);
            let r = dst.engine.pull_from_peer(&peer, now);
            (r, peer.leaked_internal.get())
        } else {
            let (left, right) = self.members.split_at_mut(dst_idx);
            let src = &left[src_idx];
            let dst = &mut right[0];
            let peer = SimPeer::new(&src.engine, &dst.id, &src.id);
            let r = dst.engine.pull_from_peer(&peer, now);
            (r, peer.leaked_internal.get())
        };

        if leaked {
            self.fail(format!(
                "Invariant (f): peer {src_id} leaked internal or unsafe path to {dst_id}"
            ));
        }
        if self.members[dst_idx].engine.fs().root_escaped() {
            self.fail(format!(
                "Invariant (f): root escape detected on {dst_id} during sync from {src_id}"
            ));
        }

        let crashed = self.members[dst_idx].engine.fs().crashed();
        self.members[dst_idx].engine.fs().disarm_crash();

        match res {
            Ok(_) => {
                self.log(|| format!("sync {dst_id} <- {src_id}"));
            }
            Err(err) if err.is_crash() && crashed => {
                self.log(|| {
                    format!(
                        "sync {dst_id} <- {src_id} CRASH(op={:?},after={crash_after}) -> recover()",
                        crash_op
                    )
                });
                // Immediate restart recovery!
                self.members[dst_idx].engine.recover().unwrap();
            }
            Err(err) => {
                self.fail(format!(
                    "unexpected error syncing {dst_id} <- {src_id}: {err}"
                ));
            }
        }

        self.record_live_presence();
    }

    fn expire_bins_online(&mut self) {
        let mut expired_log = Vec::new();
        for m in &mut self.members {
            if !m.active {
                continue;
            }
            let now = m.local_time(self.global_secs);
            let expired = m.engine.expire_bin(now).unwrap();
            for entry in expired {
                if let Some(h) = entry.hash
                    && let Some(rec) = self.contents.get_mut(&h)
                {
                    rec.bin_expired_on.insert(m.id.clone());
                }
                expired_log.push((m.id.clone(), entry.original_path));
            }
        }
        if !expired_log.is_empty() {
            let n = expired_log.len();
            self.log(|| format!("expire_bin removed {n} entries"));
        }
    }

    fn add_new_member_with_existing_files(&mut self) {
        let active = self.active_indices();
        if active.len() >= 6 || self.members.len() >= 8 {
            return;
        }
        let new_id = format!("m{}", self.members.len());
        let current_names: Vec<String> = self
            .members
            .iter()
            .filter(|m| m.active)
            .map(|m| m.id.clone())
            .collect();

        // Add `new_id` to every active member's folder membership.
        for &idx in &active {
            self.members[idx].engine.add_member(new_id.clone()).unwrap();
        }

        let mut all_names = current_names;
        all_names.push(new_id.clone());

        let caps = if self.heterogeneous {
            match self.members.len() % 3 {
                0 => FsCapabilities::unix(),
                1 => FsCapabilities::macos_case_insensitive(),
                _ => FsCapabilities::windows(),
            }
        } else {
            FsCapabilities::unix()
        };
        let cfg = FolderConfig::new("sim", new_id.clone(), all_names);
        let fs = ThreadSafeSimFs::new(caps);
        let store = MemoryIndexStore::new();
        let engine = SyncEngine::open(cfg, fs, store).unwrap();
        let skew = self.rng.range_i64(-2 * 86_400, 2 * 86_400);
        let new_idx = self.members.len();
        self.members.push(SimMember {
            id: new_id.clone(),
            engine,
            clock_skew_secs: skew,
            active: true,
            group: 0,
        });

        // Pre-populate the joining member with 1..=3 existing files (some overlapping!).
        let count = 1 + self.rng.range(3);
        let now = self.members[new_idx].local_time(self.global_secs);
        for j in 0..count {
            let path = if j == 0 && self.rng.chance(1, 2) {
                self.pick_path()
            } else {
                format!("joined/{new_id}_{j}.txt")
            };
            let (bytes, hash) = self.fresh_bytes(&new_id);
            self.members[new_idx]
                .engine
                .fs()
                .user_write_file(&path, bytes, now);
            self.note_created(&new_id, hash);
            self.universally_deleted.remove(&path);
            for anc in legion_sync::path::ancestors(&path) {
                self.universally_deleted.remove(anc);
            }
        }
        self.members[new_idx].engine.scan(now).unwrap();
        self.log(|| format!("join {new_id} with {count} pre-existing files"));
    }

    fn remove_member_by_idx(&mut self, remove_idx: usize) {
        let active = self.active_indices();
        if active.len() <= 2 || !self.members[remove_idx].active {
            return;
        }
        let removed_id = self.members[remove_idx].id.clone();
        self.members[remove_idx].active = false;
        for m in &mut self.members {
            if m.active {
                m.engine.remove_member(&removed_id).unwrap();
            }
        }
        self.log(|| format!("leave {removed_id}"));
    }

    /// Scans all active members and syncs every connected component to quiescence.
    fn sync_connected_groups_to_quiescence(&mut self) {
        for m in &mut self.members {
            if m.active {
                let now = m.local_time(self.global_secs);
                m.engine.recover().unwrap();
                m.engine.scan(now).unwrap();
            }
        }
        self.record_live_presence();

        // Repeatedly pull across every reachable pair until a full round changes nothing.
        for _round in 0..32 {
            let mut progress = false;
            let active = self.active_indices();
            for &dst_idx in &active {
                for &src_idx in &active {
                    if dst_idx == src_idx
                        || self.members[dst_idx].group != self.members[src_idx].group
                    {
                        continue;
                    }
                    let now = self.members[dst_idx].local_time(self.global_secs);
                    let report = if dst_idx < src_idx {
                        let (left, right) = self.members.split_at_mut(src_idx);
                        let peer = SimPeer::new(&right[0].engine, &left[dst_idx].id, &right[0].id);
                        let r = left[dst_idx].engine.pull_from_peer(&peer, now).unwrap();
                        assert!(!peer.leaked_internal.get());
                        r
                    } else {
                        let (left, right) = self.members.split_at_mut(dst_idx);
                        let peer =
                            SimPeer::new(&left[src_idx].engine, &right[0].id, &left[src_idx].id);
                        let r = right[0].engine.pull_from_peer(&peer, now).unwrap();
                        assert!(!peer.leaked_internal.get());
                        r
                    };
                    if report.applied > 0
                        || report.held_back_resolved > 0
                        || report.tombstones_gc > 0
                    {
                        progress = true;
                    }
                }
            }
            self.record_live_presence();
            if !progress {
                break;
            }
        }
    }

    /// Verifies invariants (a), (b), (c), (f) across all active connected groups.
    fn check_invariants(&self, check_convergence: bool) {
        // Invariant (f): no member ever escaped root, and `.legion-sync/tmp` is clean at quiescence.
        for m in &self.members {
            if !m.active {
                continue;
            }
            if m.engine.fs().root_escaped() {
                self.fail(format!("Invariant (f): root escape detected on {}", m.id));
            }
            if check_convergence && !m.engine.fs().list_tmp().unwrap().is_empty() {
                self.fail(format!(
                    "Invariant (f): leftover tmp files on {} at quiescence",
                    m.id
                ));
            }
        }

        // Invariant (a): every connected group has identical trees at quiescence.
        if check_convergence && !self.heterogeneous {
            let mut by_group: BTreeMap<usize, Vec<&SimMember>> = BTreeMap::new();
            for m in &self.members {
                if m.active {
                    by_group.entry(m.group).or_default().push(m);
                }
            }
            for (group_id, group_members) in by_group {
                if group_members.len() < 2 {
                    continue;
                }
                let ref_tree = group_members[0].engine.fs().live_tree();
                for other in &group_members[1..] {
                    let other_tree = other.engine.fs().live_tree();
                    if ref_tree != other_tree {
                        let mut diff_records = String::new();
                        let all_keys: BTreeSet<&String> =
                            ref_tree.keys().chain(other_tree.keys()).collect();
                        for k in all_keys {
                            if ref_tree.get(k) != other_tree.get(k) {
                                let r0 = group_members[0].engine.store().get_record(k).unwrap();
                                let r1 = other.engine.store().get_record(k).unwrap();
                                diff_records.push_str(&format!(
                                    "\n  diff {k:?}:\n    {}: {:?}\n    {}: {:?}",
                                    group_members[0].id, r0, other.id, r1
                                ));
                            }
                        }
                        let c0 = group_members[0].engine.store().load_peer_cursors().unwrap();
                        let c1 = other.engine.store().load_peer_cursors().unwrap();
                        self.fail(format!(
                            "Invariant (a): connected group {group_id} diverged between {} and {}{diff_records}\n  cursors {}: {:?}\n  cursors {}: {:?}",
                            group_members[0].id,
                            other.id,
                            group_members[0].id,
                            c0,
                            other.id,
                            c1,
                        ));
                    }
                }
            }
        }

        // Invariant (b): no content that ever existed in a live tree disappears from every live
        // tree, conflict copy, and unexpired bin unless a person deleted/overwrote it everywhere
        // or the bin expired.
        // Note: removing a member leaves its files untouched on its disk, so a file created or
        // renamed solely on a member that leaves before syncing remains live on that departed
        // member (and in any other member's unexpired bin if a delete arrived before the rename).
        let active_ids: BTreeSet<&str> = self
            .members
            .iter()
            .filter(|m| m.active)
            .map(|m| m.id.as_str())
            .collect();

        let live_hashes_all_members: BTreeMap<&str, BTreeSet<Hash>> = self
            .members
            .iter()
            .map(|m| (m.id.as_str(), m.engine.fs().all_live_hashes()))
            .collect();
        let bin_hashes_by_member: BTreeMap<&str, BTreeSet<Hash>> = self
            .members
            .iter()
            .filter(|m| m.active)
            .map(|m| (m.id.as_str(), m.engine.fs().all_bin_hashes()))
            .collect();

        for (hash, rec) in &self.contents {
            // Active members that once held `hash` live and did NOT have a person directly
            // remove/overwrite `hash` on that member, and did NOT expire `hash` from their bin.
            let protected_active: Vec<&str> = rec
                .ever_live_on
                .iter()
                .map(String::as_str)
                .filter(|id| {
                    active_ids.contains(id)
                        && !rec.person_removed_on.contains(*id)
                        && !rec.bin_expired_on.contains(*id)
                })
                .collect();

            if protected_active.is_empty() {
                continue;
            }

            // If NO person ever deleted or overwrote `hash` on any member, it MUST remain live on
            // at least one member (active or departed, since leaving never deletes local files) OR
            // in an unexpired bin of a protected active member.
            if rec.person_removed_on.is_empty() {
                let present_live_anywhere = live_hashes_all_members
                    .values()
                    .any(|set| set.contains(hash));
                if !present_live_anywhere {
                    self.fail(format!(
                        "Invariant (b): un-deleted content {} disappeared from every live tree (protected active: {:?})",
                        hash, protected_active
                    ));
                }
            }

            // Every protected active member (which held `hash`, did not have a person delete it
            // directly, and has not expired its bin) must still retain `hash` in its live tree,
            // conflict copy, or unexpired bin.
            let present_on_protected = protected_active.iter().any(|id| {
                live_hashes_all_members[id].contains(hash)
                    || bin_hashes_by_member[id].contains(hash)
            });
            if !present_on_protected {
                self.fail(format!(
                    "Invariant (b): content {} disappeared from every live tree and unexpired bin of protected members {:?}",
                    hash, protected_active
                ));
            }
        }

        // Invariant (c): a deletion that reached every active member never comes back.
        for path in &self.universally_deleted {
            for m in &self.members {
                if !m.active {
                    continue;
                }
                if m.engine.fs().stat_live(path).unwrap().is_some() {
                    self.fail(format!(
                        "Invariant (c): universally deleted path {path:?} came back on {}",
                        m.id
                    ));
                }
            }
        }
    }

    /// Runs a full randomized schedule for `steps` actions, checking invariants mid-run, under
    /// partition, after long-offline (>60 days) return, and at final quiescence.
    fn run_schedule(&mut self, steps: usize) {
        // Seed 2..=4 initial files across members so schedules have rich state immediately.
        let init_files = 2 + self.rng.range(3);
        for _ in 0..init_files {
            self.global_secs += 1;
            let active = self.active_indices();
            let idx = active[self.rng.range(active.len())];
            let path = self.pick_path();
            self.user_create_or_edit_file(idx, &path, true);
        }

        for step in 0..steps {
            // Advance global time slightly between steps.
            self.global_secs += self.rng.range_i64(1, 300);

            let active = self.active_indices();
            let roll = self.rng.range(100);

            match roll {
                // Create or edit file (sometimes leaving it unscanned before next sync to stress
                // apply's local re-check!).
                0..=21 => {
                    let idx = active[self.rng.range(active.len())];
                    let path = self.pick_path();
                    let scan = self.rng.chance(3, 4);
                    self.user_create_or_edit_file(idx, &path, scan);
                }
                // Delete an existing live path.
                22..=34 => {
                    let idx = active[self.rng.range(active.len())];
                    let paths = self.members[idx].engine.fs().live_paths();
                    if !paths.is_empty() {
                        let (p, _) = &paths[self.rng.range(paths.len())];
                        let scan = self.rng.chance(4, 5);
                        self.user_delete_path(idx, p, scan);
                    }
                }
                // Rename or move file between directories.
                35..=44 => {
                    let idx = active[self.rng.range(active.len())];
                    let files: Vec<String> = self.members[idx]
                        .engine
                        .fs()
                        .live_paths()
                        .into_iter()
                        .filter(|(_, k)| *k == EntryKind::File)
                        .map(|(p, _)| p)
                        .collect();
                    if !files.is_empty() {
                        let from = &files[self.rng.range(files.len())];
                        let to = self.pick_path();
                        if from != &to
                            && !self.members[idx]
                                .engine
                                .fs()
                                .live_paths()
                                .iter()
                                .any(|(p, k)| p == &to && *k == EntryKind::Dir)
                            && !legion_sync::path::ancestors(&to).into_iter().any(|anc| {
                                self.members[idx]
                                    .engine
                                    .fs()
                                    .live_paths()
                                    .iter()
                                    .any(|(p, k)| p == anc && *k != EntryKind::Dir)
                            })
                        {
                            let now = self.members[idx].local_time(self.global_secs);
                            let mid = self.members[idx].id.clone();
                            let prev_to = self.members[idx]
                                .engine
                                .fs()
                                .user_rename_file(from, &to, now);
                            if let Some(h) = prev_to {
                                self.note_person_removed(&mid, h);
                            }
                            self.universally_deleted.remove(&to);
                            for anc in legion_sync::path::ancestors(&to) {
                                self.universally_deleted.remove(anc);
                            }
                            self.members[idx].engine.scan(now).unwrap();
                            let from_owned = from.clone();
                            self.log(|| format!("{mid} rename {from_owned:?} -> {to:?}"));
                        }
                    }
                }
                // Replace file with directory (and child) OR directory with file!
                45..=52 => {
                    let idx = active[self.rng.range(active.len())];
                    let target = if self.rng.chance(1, 2) {
                        "swap"
                    } else {
                        "item"
                    };
                    let existing = self.members[idx].engine.fs().stat_live(target).unwrap();
                    match existing {
                        Some(m) if m.kind == EntryKind::Dir => {
                            self.user_create_or_edit_file(idx, target, true);
                        }
                        _ => {
                            self.user_replace_file_with_dir(idx, target, "child.txt");
                        }
                    }
                }
                // Sync random reachable pair (with frequent crash injection + recovery!).
                53..=77 => {
                    let dst = active[self.rng.range(active.len())];
                    let src = active[self.rng.range(active.len())];
                    let crash = self.rng.chance(1, 3);
                    self.sync_pair(dst, src, crash);
                }
                // Scan a member explicitly.
                78..=81 => {
                    let idx = active[self.rng.range(active.len())];
                    let now = self.members[idx].local_time(self.global_secs);
                    self.members[idx].engine.scan(now).unwrap();
                    self.record_live_presence();
                }
                // Change network partition (split, isolate one member, or heal).
                82..=88 => {
                    let mode = self.rng.range(3);
                    match mode {
                        0 => {
                            // Random 2-way partition.
                            for &idx in &active {
                                self.members[idx].group = self.rng.range(2);
                            }
                            self.log(|| "partition 2-way".to_string());
                        }
                        1 => {
                            // Isolate one member from the rest.
                            let iso = active[self.rng.range(active.len())];
                            for &idx in &active {
                                self.members[idx].group = if idx == iso { 1 } else { 0 };
                            }
                            let iso_id = self.members[iso].id.clone();
                            self.log(|| format!("partition isolate {iso_id}"));
                        }
                        _ => {
                            // Heal partition.
                            for &idx in &active {
                                self.members[idx].group = 0;
                            }
                            self.log(|| "partition heal".to_string());
                        }
                    }
                }
                // Advance time by 10..40 days & expire bins (tests bin expiry + long partition!).
                89..=92 => {
                    let days = self.rng.range_i64(10, 40);
                    self.global_secs += days * 86_400;
                    self.log(|| format!("advance_clock +{days}d"));
                    self.expire_bins_online();
                }
                // Member joins with pre-existing files (if under 6 active).
                93..=95 => {
                    self.add_new_member_with_existing_files();
                }
                // Member leaves (if more than 2 active).
                96..=97 => {
                    if active.len() > 2 {
                        let victim = active[self.rng.range(active.len())];
                        self.remove_member_by_idx(victim);
                    }
                }
                // Symlink on Unix member.
                _ => {
                    let idx = active[self.rng.range(active.len())];
                    if self.members[idx]
                        .engine
                        .fs()
                        .capabilities()
                        .supports_symlinks
                    {
                        let now = self.members[idx].local_time(self.global_secs);
                        let mid = self.members[idx].id.clone();
                        self.members[idx].engine.fs().user_symlink(
                            "sym_link",
                            "alpha.txt".to_string(),
                            now,
                        );
                        self.universally_deleted.remove("sym_link");
                        self.members[idx].engine.scan(now).unwrap();
                        self.log(|| format!("{mid} symlink sym_link -> alpha.txt"));
                    }
                }
            }

            // Periodic mid-run invariant check (and capture universally deleted paths when healed).
            if step % 15 == 14 {
                self.check_invariants(false);
            }
        }

        // Phase A: Check connected-group convergence UNDER whatever partition currently exists!
        self.sync_connected_groups_to_quiescence();
        self.check_invariants(true);

        // Phase B: Heal all partitions and sync everyone to global quiescence.
        for m in &mut self.members {
            if m.active {
                m.group = 0;
            }
        }
        self.sync_connected_groups_to_quiescence();
        self.check_invariants(true);

        // Record all paths currently deleted everywhere, advance clock by 65 days, expire bins,
        // and verify Invariant (c) holds after expiry and another sync pass!
        let active = self.active_indices();
        if let Some(&first) = active.first() {
            let live_now: BTreeSet<String> = self.members[first]
                .engine
                .fs()
                .live_tree()
                .keys()
                .cloned()
                .collect();
            for &p in PATH_POOL {
                if !live_now.contains(p) {
                    self.universally_deleted.insert(p.to_string());
                }
            }
        }
        self.global_secs += 65 * 86_400;
        self.expire_bins_online();
        self.sync_connected_groups_to_quiescence();
        self.check_invariants(true);
    }
}

/// Dedicated verification of Invariant (c): a member partitioned for 90 days (3x bin expiry) while
/// files are deleted and bins expire on the online members NEVER resurrects deleted files on
/// return, and tombstones are GC'd once every member has acknowledged them.
fn run_long_partition_zombie_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "long_partition_zombie",
        seed,
        false,
        record_trace,
    );
    let active = world.active_indices();
    let sleeper = active[world.rng.range(active.len())];
    let online: Vec<usize> = active.iter().copied().filter(|&i| i != sleeper).collect();
    let editor = online[0];

    // Create 4 files and sync to EVERY member (including sleeper).
    for path in ["z1.txt", "z2.txt", "dir/z3.txt", "dir/z4.txt"] {
        world.user_create_or_edit_file(editor, path, true);
    }
    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    // Partition sleeper away!
    world.members[sleeper].group = 1;
    for &i in &online {
        world.members[i].group = 0;
    }

    // Online members delete z1.txt and dir/z3.txt, and edit z2.txt.
    world.global_secs += 3600;
    world.user_delete_path(editor, "z1.txt", true);
    world.user_delete_path(editor, "dir/z3.txt", true);
    world.user_create_or_edit_file(editor, "z2.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Advance 90 days (3x the 30-day bin expiry) and expire bins on online members!
    world.global_secs += 90 * 86_400;
    world.expire_bins_online();
    world.sync_connected_groups_to_quiescence();

    // Tombstones MUST still exist on online members because `sleeper` hasn't acknowledged them!
    let ts_online = world.members[editor]
        .engine
        .store()
        .list_tombstones()
        .unwrap();
    if ts_online.len() < 2 {
        world.fail(format!(
            "Invariant (c): tombstones were GC'd while sleeper {} was still partitioned: {:?}",
            world.members[sleeper].id, ts_online
        ));
    }

    // Sleeper returns after 90 days! Sync with random crashes injected!
    world.members[sleeper].group = 0;
    world.sync_pair(sleeper, editor, true);
    world.sync_pair(editor, sleeper, true);
    world.sync_connected_groups_to_quiescence();

    // Verify z1.txt and dir/z3.txt did NOT come back on ANY member!
    for &i in &active {
        let tree = world.members[i].engine.fs().live_tree();
        if tree.contains_key("z1.txt") || tree.contains_key("dir/z3.txt") {
            world.fail(format!(
                "Invariant (c): deleted file resurrected on {} after 90-day partition return",
                world.members[i].id
            ));
        }
    }
    world.check_invariants(true);

    // Once all members have synced back and forth, tombstones MUST be garbage-collected!
    for &i in &active {
        let remaining_ts = world.members[i].engine.store().list_tombstones().unwrap();
        if !remaining_ts.is_empty() {
            world.fail(format!(
                "Tombstone GC did not reclaim acknowledged tombstones on {}: {:?}",
                world.members[i].id, remaining_ts
            ));
        }
    }
}

/// Dedicated verification of Invariant (d) (delete vs change -> change wins) and Invariant (e)
/// (two concurrent edits -> exact same 2 files and names everywhere) under random N=2..6, clock
/// skew, chain topologies, and crashes.
fn run_conflict_and_delete_vs_edit_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "conflict_and_delete_vs_edit",
        seed,
        false,
        record_trace,
    );
    let active = world.active_indices();
    let a = active[0];
    let b = active[1];

    // Establish baseline files on all members.
    world.user_create_or_edit_file(a, "both.txt", true);
    world.user_create_or_edit_file(a, "del_vs_edit.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Partition A and B apart (others in random groups).
    world.members[a].group = 1;
    world.members[b].group = 2;
    for &idx in &active[2..] {
        world.members[idx].group = 1 + world.rng.range(2);
    }

    // Concurrent edits to `both.txt` on A and B.
    world.global_secs += 120;
    world.user_create_or_edit_file(a, "both.txt", true);
    let hash_a = match world.members[a].engine.fs().live_tree().get("both.txt") {
        Some(TreeEntry::File(h)) => *h,
        other => world.fail(format!("expected file both.txt on A, got {other:?}")),
    };
    world.global_secs += 120;
    world.user_create_or_edit_file(b, "both.txt", true);
    let hash_b = match world.members[b].engine.fs().live_tree().get("both.txt") {
        Some(TreeEntry::File(h)) => *h,
        other => world.fail(format!("expected file both.txt on B, got {other:?}")),
    };
    assert_ne!(hash_a, hash_b);

    // Concurrent delete on A vs edit on B for `del_vs_edit.txt` (sometimes leave B unscanned!).
    world.user_delete_path(a, "del_vs_edit.txt", true);
    let scan_b = world.rng.chance(1, 2);
    world.user_create_or_edit_file(b, "del_vs_edit.txt", scan_b);
    let changed_hash = match world.members[b]
        .engine
        .fs()
        .live_tree()
        .get("del_vs_edit.txt")
    {
        Some(TreeEntry::File(h)) => *h,
        other => world.fail(format!("expected file del_vs_edit.txt on B, got {other:?}")),
    };

    // Heal partition, inject a few random crashy syncs, then converge!
    for &idx in &active {
        world.members[idx].group = 0;
    }
    for _ in 0..6 {
        let dst = active[world.rng.range(active.len())];
        let src = active[world.rng.range(active.len())];
        world.sync_pair(dst, src, true);
    }
    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    // Verify Invariant (d): `del_vs_edit.txt` is LIVE everywhere with `changed_hash`.
    for &idx in &active {
        let tree = world.members[idx].engine.fs().live_tree();
        if tree.get("del_vs_edit.txt") != Some(&TreeEntry::File(changed_hash)) {
            world.fail(format!(
                "Invariant (d): del_vs_edit.txt did not survive with changed content on {}: {:?}",
                world.members[idx].id, tree
            ));
        }
    }

    // Verify Invariant (e): `both.txt` produced EXACTLY TWO files everywhere, with identical names
    // and containing `{hash_a, hash_b}`.
    let ref_both: BTreeMap<String, TreeEntry> = world.members[a]
        .engine
        .fs()
        .live_tree()
        .into_iter()
        .filter(|(p, _)| p == "both.txt" || p.starts_with("both (changed on "))
        .collect();
    if ref_both.len() != 2 {
        world.fail(format!(
            "Invariant (e): expected exactly 2 files for both.txt, got {:?}",
            ref_both
        ));
    }
    let found_hashes: BTreeSet<Hash> = ref_both
        .values()
        .filter_map(|e| match e {
            TreeEntry::File(h) => Some(*h),
            _ => None,
        })
        .collect();
    let expected_hashes: BTreeSet<Hash> = [hash_a, hash_b].into_iter().collect();
    if found_hashes != expected_hashes {
        world.fail(format!(
            "Invariant (e): missing one of the two concurrent versions: got {:?}, expected {:?}",
            found_hashes, expected_hashes
        ));
    }
    for &idx in &active[1..] {
        let member_both: BTreeMap<String, TreeEntry> = world.members[idx]
            .engine
            .fs()
            .live_tree()
            .into_iter()
            .filter(|(p, _)| p == "both.txt" || p.starts_with("both (changed on "))
            .collect();
        if member_both != ref_both {
            world.fail(format!(
                "Invariant (e): conflict files differ on {}: {:?} vs {:?}",
                world.members[idx].id, member_both, ref_both
            ));
        }
    }
}

/// Dedicated heterogeneous test: Unix + case-insensitive macOS + Windows members syncing safe
/// paths alongside Windows-forbidden names, case clashes, and symlinks.
fn run_heterogeneous_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(root_seed, "heterogeneous", seed, true, record_trace);
    let unix_idx = 0;

    // Seed normal files and platform-problematic files on Unix member 0.
    world.user_create_or_edit_file(unix_idx, "shared.txt", true);
    world.user_create_or_edit_file(unix_idx, "dir1/ok.txt", true);
    world.user_create_or_edit_file(unix_idx, "CON.txt", true);
    world.user_create_or_edit_file(unix_idx, "a:b.txt", true);
    world.user_create_or_edit_file(unix_idx, "Case.txt", true);
    world.user_create_or_edit_file(unix_idx, "case.txt", true);
    let now = world.members[unix_idx].local_time(world.global_secs);
    world.members[unix_idx]
        .engine
        .fs()
        .user_symlink("unix_link", "shared.txt".to_string(), now);
    world.members[unix_idx].engine.scan(now).unwrap();

    world.sync_connected_groups_to_quiescence();

    // Every member MUST have `shared.txt` and `dir1/ok.txt` identically; Windows/macOS members must
    // hold back unrepresentable items and report them as `MemberState::Problem` (never mangle).
    let expected_shared = world.members[unix_idx]
        .engine
        .fs()
        .live_tree()
        .get("shared.txt")
        .cloned();
    let expected_ok = world.members[unix_idx]
        .engine
        .fs()
        .live_tree()
        .get("dir1/ok.txt")
        .cloned();

    for m in &world.members {
        if !m.active {
            continue;
        }
        let tree = m.engine.fs().live_tree();
        assert_eq!(tree.get("shared.txt"), expected_shared.as_ref());
        assert_eq!(tree.get("dir1/ok.txt"), expected_ok.as_ref());
        let caps = m.engine.fs().capabilities();
        if caps.windows_naming {
            assert!(!tree.contains_key("CON.txt"));
            assert!(!tree.contains_key("a:b.txt"));
            assert!(!tree.contains_key("unix_link"));
            assert!(matches!(
                m.engine.status().unwrap().state,
                MemberState::Problem { .. }
            ));
        } else if caps.case_insensitive {
            // Exactly one of Case.txt / case.txt is materialized; the other is held back.
            let both_present = tree.contains_key("Case.txt") && tree.contains_key("case.txt");
            assert!(
                !both_present,
                "case-insensitive member wrote both Case.txt and case.txt"
            );
            assert!(matches!(
                m.engine.status().unwrap().state,
                MemberState::Problem { .. }
            ));
        }
    }
}

fn run_one_seed(seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(seed, "random_schedule", seed, false, record_trace);
    world.run_schedule(50);

    // Every 5th seed also exercises the dedicated 90-day zombie test, every 3rd exercises explicit
    // concurrent conflict + delete-vs-change, and every 10th exercises heterogeneous FS rules.
    if seed.is_multiple_of(5) {
        run_long_partition_zombie_seed(seed, seed ^ 0x5555_5555, record_trace);
    }
    if seed.is_multiple_of(3) {
        run_conflict_and_delete_vs_edit_seed(seed, seed ^ 0xAAAA_AAAA, record_trace);
    }
    if seed.is_multiple_of(10) {
        run_heterogeneous_seed(seed, seed ^ 0x3333_3333, record_trace);
    }
}

fn run_seed_range_parallel(count: u64) -> (std::time::Duration, f64) {
    let started = Instant::now();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(count.max(1) as usize);

    if threads <= 1 {
        for seed in 0..count {
            run_one_seed(seed, false);
        }
    } else {
        std::thread::scope(|scope| {
            let chunk = count.div_ceil(threads as u64);
            let mut handles = Vec::with_capacity(threads);
            for t in 0..threads {
                let start = (t as u64) * chunk;
                let end = (start + chunk).min(count);
                if start >= end {
                    continue;
                }
                handles.push(scope.spawn(move || {
                    for seed in start..end {
                        run_one_seed(seed, false);
                    }
                }));
            }
            for h in handles {
                if let Err(payload) = h.join() {
                    std::panic::resume_unwind(payload);
                }
            }
        });
    }

    let elapsed = started.elapsed();
    let rate = count as f64 / elapsed.as_secs_f64().max(1e-9);
    (elapsed, rate)
}

/// Every seed that ever uncovered a bug during development or fault testing stays here forever.
const REGRESSION_SEEDS: &[u64] = &[
    0, 1, 2, 3, 4, 7, 13, 42, 55, 78, 95, 97, 99, 128, 196, 302, 1337, 1435, 1532, 2026,
];

#[test]
fn simulator_regression_seeds() {
    for &seed in REGRESSION_SEEDS {
        run_one_seed(seed, true);
    }
}

#[test]
fn simulator_normal_suite() {
    let count: u64 = std::env::var("LEGION_SYNC_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3_000);
    let (elapsed, rate) = run_seed_range_parallel(count);
    eprintln!(
        "simulator_normal_suite: {count} seeds passed in {:.2}s ({:.0} seeds/sec)",
        elapsed.as_secs_f64(),
        rate
    );
}

#[test]
fn replay_seed() {
    let seed: u64 = std::env::var("LEGION_SYNC_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(42);
    run_one_seed(seed, true);
}

#[test]
#[ignore = "long simulation mode; run with --ignored and optional LEGION_SYNC_SEEDS=1000000"]
fn simulator_long_mode() {
    let count: u64 = std::env::var("LEGION_SYNC_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let (elapsed, rate) = run_seed_range_parallel(count);
    println!(
        "simulator_long_mode: {count} seeds passed in {:.2}s ({:.0} seeds/sec)",
        elapsed.as_secs_f64(),
        rate
    );
}
