//! Honest deterministic multi-member synchronization simulator verifying all engine invariants
//! across partitions, clock skew and clock jumps (both directions), crashes during apply AND
//! during recovery, non-crash FS errors (permission denied, no space, cross-device), index store
//! commit failures, mid-apply human edits, serving-peer mid-fetch failures, backup restores,
//! reinstalls, duplicate member IDs, joining with old copies, membership changes while offline,
//! remove+re-add after tombstone GC, hostile peers, unreadable directories, empty mount points,
//! ignored files and OS litter in deleted directories, directory renames with concurrent edits
//! inside, case-only renames, bin restore, conflict resolution, and mixed-OS filesystem rules
//! (Unix + macOS case-insensitive/Unicode-normalizing + Windows naming/length/symlink rules).
//!
//! Replay any failing seed with:
//! ```text
//! LEGION_SYNC_SEED=<seed> cargo test -p legion-sync --release --test simulator replay_seed -- --nocapture
//! ```
//!
//! Run long mode (override range with `LEGION_SYNC_SEED_START` and `LEGION_SYNC_SEEDS`):
//! ```text
//! LEGION_SYNC_SEED_START=0 LEGION_SYNC_SEEDS=5000000 \
//!   cargo test -p legion-sync --release --test simulator simulator_long_mode -- --ignored --nocapture
//! ```

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Read, Write};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use legion_sync::path::{
    check_fs_compatibility, fs_equivalence_key, is_descendant_of, parse_conflict_copy_path,
    validate_flat_internal_name,
};
use legion_sync::{
    BinEntry, ConflictChoice, DeletionBrakeConfig, FetchOptions, FolderConfig, FolderMarker,
    FolderMeta, FsCapabilities, FsErrorKind, FsMeta, FsWalkEntry, FsWalkResult, HeldBackRecord,
    IgnoreMatcher, IndexRecord, IndexStore, MemberId, MemberState, MemoryIndexStore, Peer,
    PeerCursor, PeerHandshake, PendingOp, ProblemKind, RecordsPage, RestoreStrategy,
    SYNC_PROTOCOL_VERSION, StoreBatch, SyncEngine, SyncError, SyncFs, SyncWrite, VersionKey,
    VersionVector, is_internal_sync_path, validate_sync_path,
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

/// Live entry stored inside [`HonestSimFs`] with cached BLAKE3 hash for fast 50M-seed runs.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SimNode {
    File {
        bytes: Vec<u8>,
        hash: Hash,
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
    fn kind(&self) -> EntryKind {
        match self {
            SimNode::File { .. } => EntryKind::File,
            SimNode::Dir { .. } => EntryKind::Dir,
            SimNode::Symlink { .. } => EntryKind::Symlink,
        }
    }

    fn meta(&self) -> FsMeta {
        match self {
            SimNode::File {
                bytes,
                mtime,
                mode,
                gen_id,
                ..
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

#[derive(Debug, Clone)]
struct SimBinData {
    bytes: Vec<u8>,
    hash: Option<Hash>,
}

/// Canonical tree representation used for convergence assertions (Invariant a).
#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeEntry {
    File(Hash),
    Dir,
    Symlink(String),
}

/// Injectable filesystem fault: either a crash (before or after a mutating FS call) or a
/// recoverable non-crash OS error (`PermissionDenied`, `NoSpace`, `CrossDevice`).
#[derive(Debug, Clone, Copy)]
enum FsFault {
    Crash { countdown: usize, after_op: bool },
    NonCrash { countdown: usize, kind: FsErrorKind },
}

/// Human action fired right between two FS steps inside an active apply!
#[derive(Debug, Clone)]
enum MidApplyOp {
    EditOrCreateFile {
        path: String,
        bytes: Vec<u8>,
        hash: Hash,
        mtime: Timestamp,
    },
    DeletePath {
        path: String,
    },
}

#[derive(Debug, Clone)]
struct MidApplyFired {
    created_hash: Option<Hash>,
    removed_hashes: Vec<Hash>,
    touched_path: String,
}

#[derive(Debug, Clone)]
struct MidApplyHook {
    countdown: usize,
    op: MidApplyOp,
}

/// Snapshot of [`HonestSimFs`] used for backup-restore and cloned-machine schedules.
#[derive(Clone)]
struct FsSnapshot {
    caps: FsCapabilities,
    supports_exchange: bool,
    marker: Option<FolderMarker>,
    live: BTreeMap<String, SimNode>,
    bin: BTreeMap<String, SimBinData>,
    next_gen: u64,
}

/// Send-safe writer staging into the in-memory `.legion-sync/tmp` map on `finish_sync`.
struct SyncSimWriter {
    buf: Vec<u8>,
    name: String,
    target_tmp: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    fail_on_sync: Option<FsErrorKind>,
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
        if let Some(kind) = self.fail_on_sync {
            return Err(HonestSimFs::fault_err("finish_sync", kind));
        }
        let mut map = self.target_tmp.lock().unwrap();
        map.insert(self.name, self.buf);
        Ok(())
    }
}

/// Honest in-memory filesystem enforcing platform case-insensitivity, macOS Unicode NFC/NFD
/// normalization, Windows naming/length/symlink rules, unreadable subtrees, OS litter & ignored
/// files, non-replacing renames & optional atomic swaps, crash/non-crash fault injection, and
/// mid-apply human edit hooks.
struct HonestSimFs {
    caps: FsCapabilities,
    supports_exchange: bool,
    marker: RefCell<Option<FolderMarker>>,
    live: RefCell<BTreeMap<String, SimNode>>,
    tmp: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    bin: RefCell<BTreeMap<String, SimBinData>>,
    unreadable: RefCell<BTreeSet<String>>,
    any_unreadable: Cell<bool>,
    next_gen: Cell<u64>,
    root_escape_detected: Cell<bool>,
    platform_violation_detected: Cell<bool>,
    fault: RefCell<Option<FsFault>>,
    has_fault: Cell<bool>,
    did_crash: Cell<bool>,
    did_non_crash_fault: Cell<bool>,
    mid_apply: RefCell<Option<MidApplyHook>>,
    has_mid_apply: Cell<bool>,
    mid_apply_fired: RefCell<Option<MidApplyFired>>,
}

impl HonestSimFs {
    fn new(caps: FsCapabilities, supports_exchange: bool) -> Self {
        Self {
            caps,
            supports_exchange,
            marker: RefCell::new(None),
            live: RefCell::new(BTreeMap::new()),
            tmp: Arc::new(Mutex::new(BTreeMap::new())),
            bin: RefCell::new(BTreeMap::new()),
            unreadable: RefCell::new(BTreeSet::new()),
            any_unreadable: Cell::new(false),
            next_gen: Cell::new(1),
            root_escape_detected: Cell::new(false),
            platform_violation_detected: Cell::new(false),
            fault: RefCell::new(None),
            has_fault: Cell::new(false),
            did_crash: Cell::new(false),
            did_non_crash_fault: Cell::new(false),
            mid_apply: RefCell::new(None),
            has_mid_apply: Cell::new(false),
            mid_apply_fired: RefCell::new(None),
        }
    }

    fn snapshot(&self) -> FsSnapshot {
        FsSnapshot {
            caps: self.caps,
            supports_exchange: self.supports_exchange,
            marker: self.marker.borrow().clone(),
            live: self.live.borrow().clone(),
            bin: self.bin.borrow().clone(),
            next_gen: self.next_gen.get(),
        }
    }

    fn from_snapshot(snap: FsSnapshot) -> Self {
        Self {
            caps: snap.caps,
            supports_exchange: snap.supports_exchange,
            marker: RefCell::new(snap.marker),
            live: RefCell::new(snap.live),
            tmp: Arc::new(Mutex::new(BTreeMap::new())),
            bin: RefCell::new(snap.bin),
            unreadable: RefCell::new(BTreeSet::new()),
            any_unreadable: Cell::new(false),
            next_gen: Cell::new(snap.next_gen),
            root_escape_detected: Cell::new(false),
            platform_violation_detected: Cell::new(false),
            fault: RefCell::new(None),
            has_fault: Cell::new(false),
            did_crash: Cell::new(false),
            did_non_crash_fault: Cell::new(false),
            mid_apply: RefCell::new(None),
            has_mid_apply: Cell::new(false),
            mid_apply_fired: RefCell::new(None),
        }
    }

    fn alloc_gen(&self) -> u64 {
        let g = self.next_gen.get();
        self.next_gen.set(g + 1);
        g
    }

    fn arm_crash(&self, countdown: usize, after_op: bool) {
        *self.fault.borrow_mut() = Some(FsFault::Crash {
            countdown,
            after_op,
        });
        self.has_fault.set(true);
        self.did_crash.set(false);
        self.did_non_crash_fault.set(false);
    }

    fn arm_non_crash_fault(&self, countdown: usize, kind: FsErrorKind) {
        *self.fault.borrow_mut() = Some(FsFault::NonCrash { countdown, kind });
        self.has_fault.set(true);
        self.did_crash.set(false);
        self.did_non_crash_fault.set(false);
    }

    fn disarm_fault(&self) {
        *self.fault.borrow_mut() = None;
        self.has_fault.set(false);
    }

    fn crashed(&self) -> bool {
        self.did_crash.get()
    }

    fn had_non_crash_fault(&self) -> bool {
        self.did_non_crash_fault.get()
    }

    fn arm_mid_apply(&self, countdown: usize, op: MidApplyOp) {
        *self.mid_apply.borrow_mut() = Some(MidApplyHook { countdown, op });
        self.has_mid_apply.set(true);
        *self.mid_apply_fired.borrow_mut() = None;
    }

    fn take_mid_apply_fired(&self) -> Option<MidApplyFired> {
        if !self.has_mid_apply.get() && self.mid_apply_fired.borrow().is_none() {
            return None;
        }
        *self.mid_apply.borrow_mut() = None;
        self.has_mid_apply.set(false);
        self.mid_apply_fired.borrow_mut().take()
    }

    fn maybe_fire_mid_apply(&self) {
        if !self.has_mid_apply.get() {
            return;
        }
        let to_run = {
            let mut guard = self.mid_apply.borrow_mut();
            match guard.as_mut() {
                Some(hook) if hook.countdown == 0 => {
                    self.has_mid_apply.set(false);
                    guard.take().map(|h| h.op)
                }
                Some(hook) => {
                    hook.countdown -= 1;
                    None
                }
                None => {
                    self.has_mid_apply.set(false);
                    None
                }
            }
        };
        let Some(op) = to_run else {
            return;
        };
        let fired = match op {
            MidApplyOp::EditOrCreateFile {
                path,
                bytes,
                hash,
                mtime,
            } => {
                let mut removed = Vec::new();
                let mut live = self.live.borrow_mut();
                // Do not corrupt if an ancestor is a non-dir or path is a dir.
                let blocked = Self::has_ancestor_conflict(&live, &path)
                    || matches!(live.get(&path), Some(SimNode::Dir { .. }));
                if blocked {
                    return;
                }
                for anc in legion_sync::path::ancestors(&path) {
                    live.entry(anc.to_string())
                        .or_insert(SimNode::Dir { mtime });
                }
                if let Some(SimNode::File { hash: old_h, .. }) = live.get(&path) {
                    removed.push(*old_h);
                }
                live.insert(
                    path.clone(),
                    SimNode::File {
                        bytes,
                        hash,
                        mtime,
                        mode: if self.caps.windows_naming {
                            None
                        } else {
                            Some(0o644)
                        },
                        gen_id: self.alloc_gen(),
                    },
                );
                MidApplyFired {
                    created_hash: Some(hash),
                    removed_hashes: removed,
                    touched_path: path,
                }
            }
            MidApplyOp::DeletePath { path } => {
                let removed = self.user_remove_recursive(&path);
                MidApplyFired {
                    created_hash: None,
                    removed_hashes: removed,
                    touched_path: path,
                }
            }
        };
        *self.mid_apply_fired.borrow_mut() = Some(fired);
    }

    /// Consumes one fault step; returns `(fire_before, fire_after)`.
    fn tick_fault(&self) -> (Option<FsErrorKind>, Option<FsErrorKind>) {
        if !self.has_fault.get() {
            return (None, None);
        }
        let mut guard = self.fault.borrow_mut();
        let Some(mut f) = *guard else {
            self.has_fault.set(false);
            return (None, None);
        };
        match &mut f {
            FsFault::Crash {
                countdown,
                after_op,
            } => {
                if *countdown == 0 {
                    *guard = None;
                    self.has_fault.set(false);
                    self.did_crash.set(true);
                    if *after_op {
                        (None, Some(FsErrorKind::Crash))
                    } else {
                        (Some(FsErrorKind::Crash), None)
                    }
                } else {
                    *countdown -= 1;
                    *guard = Some(f);
                    (None, None)
                }
            }
            FsFault::NonCrash { countdown, kind } => {
                if *countdown == 0 {
                    *guard = None;
                    self.has_fault.set(false);
                    self.did_non_crash_fault.set(true);
                    (Some(*kind), None)
                } else {
                    *countdown -= 1;
                    *guard = Some(f);
                    (None, None)
                }
            }
        }
    }

    fn fault_err(op: &str, kind: FsErrorKind) -> SyncError {
        SyncError::Fs {
            path: Some(op.to_string()),
            detail: format!("simulated {kind:?}"),
            kind,
        }
    }

    fn wrap_mut<T>(
        &self,
        op: &str,
        allow_mid_apply: bool,
        f: impl FnOnce() -> legion_sync::Result<T>,
    ) -> legion_sync::Result<T> {
        if allow_mid_apply {
            self.maybe_fire_mid_apply();
        }
        let (before, after) = self.tick_fault();
        if let Some(kind) = before {
            return Err(Self::fault_err(op, kind));
        }
        let res = f()?;
        if let Some(kind) = after {
            return Err(Self::fault_err(op, kind));
        }
        Ok(res)
    }

    fn is_unreadable(&self, path: &str) -> bool {
        if !self.any_unreadable.get() {
            return false;
        }
        let unreadable = self.unreadable.borrow();
        unreadable
            .iter()
            .any(|u| path == u || is_descendant_of(path, u))
    }

    fn has_unreadable(&self) -> bool {
        self.any_unreadable.get()
    }

    fn set_unreadable(&self, path: &str, blocked: bool) {
        let mut unreadable = self.unreadable.borrow_mut();
        if blocked {
            unreadable.insert(path.to_string());
        } else {
            unreadable.remove(path);
        }
        self.any_unreadable.set(!unreadable.is_empty());
    }

    fn clear_mount_point(&self) {
        *self.marker.borrow_mut() = None;
        self.live.borrow_mut().clear();
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

    fn guard_platform(&self, clean: &str, kind: EntryKind) -> legion_sync::Result<()> {
        if let Err((_, msg)) = check_fs_compatibility(clean, kind, &self.caps) {
            self.platform_violation_detected.set(true);
            return Err(SyncError::InvalidPath {
                path: clean.to_string(),
                reason: msg,
            });
        }
        Ok(())
    }

    /// Resolves an existing key in `live`, honoring case-insensitivity and Unicode normalization
    /// on macOS/Windows filesystems.
    fn resolve_existing_key(
        &self,
        live: &BTreeMap<String, SimNode>,
        clean: &str,
    ) -> Option<String> {
        if live.contains_key(clean) {
            return Some(clean.to_string());
        }
        if !self.caps.case_insensitive && !self.caps.unicode_normalizing {
            return None;
        }
        let target = fs_equivalence_key(clean, &self.caps);
        live.keys()
            .find(|k| fs_equivalence_key(k, &self.caps) == target)
            .cloned()
    }

    fn user_write_file(
        &self,
        path: &str,
        bytes: Vec<u8>,
        hash: Hash,
        mtime: Timestamp,
    ) -> Option<Hash> {
        let clean = validate_sync_path(path).unwrap();
        let mut live = self.live.borrow_mut();
        for anc in legion_sync::path::ancestors(&clean) {
            let anc_key = self
                .resolve_existing_key(&live, anc)
                .unwrap_or_else(|| anc.to_string());
            live.entry(anc_key).or_insert(SimNode::Dir { mtime });
        }
        let target_key = self
            .resolve_existing_key(&live, &clean)
            .unwrap_or_else(|| clean.clone());
        let prev_hash = match live.get(&target_key) {
            Some(SimNode::File { hash: old, .. }) => Some(*old),
            _ => None,
        };
        live.insert(
            target_key,
            SimNode::File {
                bytes,
                hash,
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
        let mut live = self.live.borrow_mut();
        for anc in legion_sync::path::ancestors(&clean) {
            let anc_key = self
                .resolve_existing_key(&live, anc)
                .unwrap_or_else(|| anc.to_string());
            live.entry(anc_key).or_insert(SimNode::Dir { mtime });
        }
        let target_key = self
            .resolve_existing_key(&live, &clean)
            .unwrap_or_else(|| clean.clone());
        live.insert(target_key, SimNode::Dir { mtime });
    }

    fn user_symlink(&self, path: &str, target: String, mtime: Timestamp) {
        let clean = validate_sync_path(path).unwrap();
        let mut live = self.live.borrow_mut();
        for anc in legion_sync::path::ancestors(&clean) {
            let anc_key = self
                .resolve_existing_key(&live, anc)
                .unwrap_or_else(|| anc.to_string());
            live.entry(anc_key).or_insert(SimNode::Dir { mtime });
        }
        let target_key = self
            .resolve_existing_key(&live, &clean)
            .unwrap_or_else(|| clean.clone());
        live.insert(target_key, SimNode::Symlink { target, mtime });
    }

    fn user_remove_recursive(&self, path: &str) -> Vec<Hash> {
        let mut live = self.live.borrow_mut();
        let actual = self
            .resolve_existing_key(&live, path)
            .unwrap_or_else(|| path.to_string());
        let prefix = format!("{actual}/");
        let keys: Vec<String> = live
            .keys()
            .filter(|k| **k == actual || k.starts_with(&prefix))
            .cloned()
            .collect();
        let mut removed = Vec::new();
        for k in keys {
            if let Some(SimNode::File { hash, .. }) = live.remove(&k) {
                removed.push(hash);
            }
        }
        removed
    }

    fn user_rename_file(&self, from: &str, to: &str, mtime: Timestamp) -> Option<Hash> {
        let mut live = self.live.borrow_mut();
        let actual_from = self.resolve_existing_key(&live, from)?;
        let node = live.remove(&actual_from)?;
        let prev_to = self
            .resolve_existing_key(&live, to)
            .and_then(|k| match live.remove(&k) {
                Some(SimNode::File { hash, .. }) => Some(hash),
                _ => None,
            });
        for anc in legion_sync::path::ancestors(to) {
            let anc_key = self
                .resolve_existing_key(&live, anc)
                .unwrap_or_else(|| anc.to_string());
            live.entry(anc_key).or_insert(SimNode::Dir { mtime });
        }
        live.insert(to.to_string(), node);
        prev_to
    }

    /// Renames a directory and all its descendants (`from` -> `to`) atomically in-memory.
    fn user_rename_dir(&self, from: &str, to: &str, mtime: Timestamp) -> Vec<Hash> {
        let mut live = self.live.borrow_mut();
        if !matches!(live.get(from), Some(SimNode::Dir { .. })) {
            return Vec::new();
        }
        if live.contains_key(to) || Self::has_ancestor_conflict(&live, to) {
            return Vec::new();
        }
        let from_prefix = format!("{from}/");
        let to_move: Vec<(String, SimNode)> = live
            .keys()
            .filter(|k| *k == from || k.starts_with(&from_prefix))
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .filter_map(|k| live.remove(&k).map(|v| (k, v)))
            .collect();
        for anc in legion_sync::path::ancestors(to) {
            live.entry(anc.to_string())
                .or_insert(SimNode::Dir { mtime });
        }
        let mut overwritten = Vec::new();
        for (old_k, mut node) in to_move {
            let new_k = if old_k == from {
                to.to_string()
            } else {
                format!("{to}/{}", &old_k[from_prefix.len()..])
            };
            if let SimNode::File { gen_id, .. } = &mut node {
                *gen_id = self.alloc_gen();
            }
            if let Some(SimNode::File { hash, .. }) = live.insert(new_k, node) {
                overwritten.push(hash);
            }
        }
        overwritten
    }

    fn live_paths(&self) -> Vec<(String, EntryKind)> {
        self.live
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.kind()))
            .collect()
    }

    fn live_file_hash(&self, path: &str) -> Option<Hash> {
        let live = self.live.borrow();
        match live.get(path) {
            Some(SimNode::File { hash, .. }) => Some(*hash),
            _ => None,
        }
    }

    /// Returns the live tree excluding ignored entries (and optionally excluding directories that
    /// were kept solely for ignored/held-back children).
    fn live_tree(&self, ignore: &IgnoreMatcher) -> BTreeMap<String, TreeEntry> {
        self.live
            .borrow()
            .iter()
            .filter(|(k, v)| !ignore.is_path_or_ancestor_ignored(k, v.kind() == EntryKind::Dir))
            .map(|(k, v)| {
                let entry = match v {
                    SimNode::File { hash, .. } => TreeEntry::File(*hash),
                    SimNode::Dir { .. } => TreeEntry::Dir,
                    SimNode::Symlink { target, .. } => TreeEntry::Symlink(target.clone()),
                };
                (k.clone(), entry)
            })
            .collect()
    }

    fn for_each_live_hash(&self, mut f: impl FnMut(Hash)) {
        for n in self.live.borrow().values() {
            if let SimNode::File { hash, .. } = n {
                f(*hash);
            }
        }
    }

    fn all_live_hashes(&self) -> BTreeSet<Hash> {
        self.live
            .borrow()
            .values()
            .filter_map(|n| match n {
                SimNode::File { hash, .. } => Some(*hash),
                _ => None,
            })
            .collect()
    }

    fn all_bin_hashes(&self) -> BTreeSet<Hash> {
        self.bin
            .borrow()
            .iter()
            .filter(|(k, _)| k.ends_with(".data"))
            .filter_map(|(_, entry)| entry.hash)
            .collect()
    }

    fn root_escaped(&self) -> bool {
        self.root_escape_detected.get()
    }

    fn platform_violated(&self) -> bool {
        self.platform_violation_detected.get()
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

impl SyncFs for HonestSimFs {
    fn capabilities(&self) -> FsCapabilities {
        self.caps
    }

    fn read_marker(&self) -> legion_sync::Result<Option<FolderMarker>> {
        Ok(self.marker.borrow().clone())
    }

    fn write_marker(&self, marker: &FolderMarker) -> legion_sync::Result<()> {
        *self.marker.borrow_mut() = Some(marker.clone());
        Ok(())
    }

    fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
        Ok(())
    }

    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        let live = self.live.borrow();
        let has_unreadable = self.any_unreadable.get();
        let unreadable = if has_unreadable {
            Some(self.unreadable.borrow())
        } else {
            None
        };
        let mut entries = Vec::with_capacity(live.len());
        let mut skipped = Vec::new();
        let mut reported_unreadable = BTreeSet::new();

        for (path, node) in live.iter() {
            if let Some(ref unrd) = unreadable
                && let Some(u) = unrd
                    .iter()
                    .find(|u| path == u.as_str() || is_descendant_of(path, u))
            {
                if reported_unreadable.insert(u.clone()) {
                    skipped.push((u.clone(), "permission denied".to_string()));
                }
                continue;
            }
            let meta = node.meta();
            if ignore.is_path_or_ancestor_ignored(path, meta.kind == EntryKind::Dir) {
                continue;
            }
            entries.push(FsWalkEntry {
                path: path.clone(),
                meta,
            });
        }
        if let Some(ref unrd) = unreadable {
            for u in unrd.iter() {
                if reported_unreadable.insert(u.clone()) {
                    skipped.push((u.clone(), "permission denied".to_string()));
                }
            }
            skipped.sort();
        }
        Ok(FsWalkResult { entries, skipped })
    }

    fn walk_subtree(
        &self,
        rel_root: &str,
        ignore: &IgnoreMatcher,
    ) -> legion_sync::Result<FsWalkResult> {
        let clean = self.check_live_path(rel_root)?;
        let full = self.walk(ignore)?;
        let entries = full
            .entries
            .into_iter()
            .filter(|e| e.path == clean || is_descendant_of(&e.path, &clean))
            .collect();
        let skipped = full
            .skipped
            .into_iter()
            .filter(|(p, _)| p == &clean || is_descendant_of(p, &clean))
            .collect();
        Ok(FsWalkResult { entries, skipped })
    }

    fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
        let clean = self.check_live_path(rel_path)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        let live = self.live.borrow();
        for anc in legion_sync::path::ancestors(&clean) {
            if let Some(key) = self.resolve_existing_key(&live, anc) {
                match live.get(&key) {
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
        }
        Ok(self
            .resolve_existing_key(&live, &clean)
            .and_then(|k| live.get(&k))
            .map(SimNode::meta))
    }

    fn hash_live(&self, rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
        let clean = self.check_live_path(rel_path)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        let live = self.live.borrow();
        let key = self.resolve_existing_key(&live, &clean);
        match key.and_then(|k| live.get(&k)) {
            Some(SimNode::File { bytes, hash, .. }) => Ok((*hash, bytes.len() as u64)),
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

    fn open_live_read_at(
        &self,
        rel_path: &str,
        offset: u64,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        let clean = self.check_live_path(rel_path)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        let live = self.live.borrow();
        let key = self.resolve_existing_key(&live, &clean);
        match key.and_then(|k| live.get(&k)) {
            Some(SimNode::File { bytes, .. }) => {
                let start = (offset as usize).min(bytes.len());
                Ok(Box::new(Cursor::new(bytes[start..].to_vec())))
            }
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
        self.guard_platform(&clean, EntryKind::Dir)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        self.wrap_mut("create_dir_all_live", false, || {
            let mut live = self.live.borrow_mut();
            if Self::has_ancestor_conflict(&live, &clean) {
                return Err(SyncError::Conflict {
                    path: clean.clone(),
                    reason: "ancestor is not a directory".to_string(),
                });
            }
            for anc in legion_sync::path::ancestors(&clean) {
                if let Some(existing_key) = self.resolve_existing_key(&live, anc) {
                    if !matches!(live.get(&existing_key), Some(SimNode::Dir { .. })) {
                        return Err(SyncError::Conflict {
                            path: clean.clone(),
                            reason: "ancestor is not a directory".to_string(),
                        });
                    }
                } else {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                }
            }
            if let Some(existing_key) = self.resolve_existing_key(&live, &clean) {
                match live.get(&existing_key) {
                    Some(SimNode::Dir { .. }) => Ok(()),
                    _ => Err(SyncError::Conflict {
                        path: clean,
                        reason: "non-directory exists".to_string(),
                    }),
                }
            } else {
                live.insert(
                    clean,
                    SimNode::Dir {
                        mtime: Timestamp::new(0, 0),
                    },
                );
                Ok(())
            }
        })
    }

    fn list_dir_live(&self, rel_path: &str) -> legion_sync::Result<Vec<String>> {
        let clean = self.check_live_path(rel_path)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        let live = self.live.borrow();
        let actual = self
            .resolve_existing_key(&live, &clean)
            .unwrap_or_else(|| clean.clone());
        let actual_bytes = actual.as_bytes();
        let mut names = BTreeSet::new();
        for key in live
            .range::<str, _>((Bound::Excluded(actual.as_str()), Bound::Unbounded))
            .map(|(k, _)| k)
        {
            let kb = key.as_bytes();
            if !kb.starts_with(actual_bytes) {
                break;
            }
            if kb.get(actual_bytes.len()) != Some(&b'/') {
                if kb.get(actual_bytes.len()).is_some_and(|&b| b > b'/') {
                    break;
                }
                continue;
            }
            let rest = &key[actual_bytes.len() + 1..];
            let first = rest.split('/').next().unwrap_or(rest);
            names.insert(first.to_string());
        }
        Ok(names.into_iter().collect())
    }

    fn remove_empty_dir_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        let clean = self.check_live_path(rel_path)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        self.wrap_mut("remove_empty_dir_live", true, || {
            let mut live = self.live.borrow_mut();
            let Some(actual) = self.resolve_existing_key(&live, &clean) else {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            let actual_bytes = actual.as_bytes();
            let has_child = live
                .range::<str, _>((Bound::Excluded(actual.as_str()), Bound::Unbounded))
                .take_while(|(k, _)| k.as_bytes().starts_with(actual_bytes))
                .any(|(k, _)| k.as_bytes().get(actual_bytes.len()) == Some(&b'/'));
            if has_child {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "directory not empty".to_string(),
                    kind: FsErrorKind::DirectoryNotEmpty,
                });
            }
            match live.remove(&actual) {
                Some(SimNode::Dir { .. }) => Ok(()),
                Some(other) => {
                    live.insert(actual, other);
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
        self.guard_platform(&clean, EntryKind::Symlink)?;
        if !self.caps.supports_symlinks {
            return Err(SyncError::Unsupported {
                reason: "symlinks unsupported".to_string(),
                hint: None,
            });
        }
        self.wrap_mut("create_symlink_live", true, || {
            let mut live = self.live.borrow_mut();
            if Self::has_ancestor_conflict(&live, &clean) {
                return Err(SyncError::Conflict {
                    path: clean.clone(),
                    reason: "ancestor is not a directory".to_string(),
                });
            }
            if self.resolve_existing_key(&live, &clean).is_some() {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
            for anc in legion_sync::path::ancestors(&clean) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                }
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
        self.wrap_mut("remove_symlink_live", false, || {
            let mut live = self.live.borrow_mut();
            let Some(actual) = self.resolve_existing_key(&live, &clean) else {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            match live.remove(&actual) {
                Some(SimNode::Symlink { .. }) => Ok(()),
                Some(other) => {
                    live.insert(actual, other);
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
        self.wrap_mut("set_live_metadata", false, || {
            let mut live = self.live.borrow_mut();
            if let Some(actual) = self.resolve_existing_key(&live, &clean)
                && let Some(SimNode::File {
                    mtime: m, mode: md, ..
                }) = live.get_mut(&actual)
            {
                *m = mtime;
                if !self.caps.windows_naming && mode.is_some() {
                    *md = mode;
                }
            }
            Ok(())
        })
    }

    fn rename_live_to_live_no_replace(
        &self,
        from_rel: &str,
        to_rel: &str,
    ) -> legion_sync::Result<()> {
        let clean_from = self.check_live_path(from_rel)?;
        let clean_to = self.check_live_path(to_rel)?;
        self.guard_platform(&clean_to, EntryKind::File)?;
        self.wrap_mut("rename_live_to_live_no_replace", true, || {
            let mut live = self.live.borrow_mut();
            if self.resolve_existing_key(&live, &clean_to).is_some() {
                return Err(SyncError::Fs {
                    path: Some(clean_to),
                    detail: "destination already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
            let Some(actual_from) = self.resolve_existing_key(&live, &clean_from) else {
                return Err(SyncError::Fs {
                    path: Some(clean_from),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            let Some(node) = live.remove(&actual_from) else {
                return Err(SyncError::Fs {
                    path: Some(clean_from),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            for anc in legion_sync::path::ancestors(&clean_to) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                }
            }
            live.insert(clean_to, node);
            Ok(())
        })
    }

    fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        validate_flat_internal_name(tmp_name)?;
        let (before, after) = self.tick_fault();
        if let Some(kind) = before {
            return Err(Self::fault_err("create_tmp", kind));
        }
        Ok(Box::new(SyncSimWriter {
            buf: Vec::new(),
            name: tmp_name.to_string(),
            target_tmp: self.tmp.clone(),
            fail_on_sync: after,
        }))
    }

    fn open_tmp_append(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        validate_flat_internal_name(tmp_name)?;
        let (before, after) = self.tick_fault();
        if let Some(kind) = before {
            return Err(Self::fault_err("open_tmp_append", kind));
        }
        let existing = self
            .tmp
            .lock()
            .unwrap()
            .get(tmp_name)
            .cloned()
            .unwrap_or_default();
        Ok(Box::new(SyncSimWriter {
            buf: existing,
            name: tmp_name.to_string(),
            target_tmp: self.tmp.clone(),
            fail_on_sync: after,
        }))
    }

    fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        validate_flat_internal_name(tmp_name)?;
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
        validate_flat_internal_name(tmp_name)?;
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
        validate_flat_internal_name(tmp_name)?;
        self.tmp.lock().unwrap().remove(tmp_name);
        Ok(())
    }

    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.tmp.lock().unwrap().keys().cloned().collect())
    }

    fn commit_tmp_to_live_no_replace(
        &self,
        tmp_name: &str,
        live_rel: &str,
    ) -> legion_sync::Result<()> {
        validate_flat_internal_name(tmp_name)?;
        let clean = self.check_live_path(live_rel)?;
        self.guard_platform(&clean, EntryKind::File)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        self.wrap_mut("commit_tmp_to_live_no_replace", true, || {
            let mut live = self.live.borrow_mut();
            if Self::has_ancestor_conflict(&live, &clean) {
                return Err(SyncError::Conflict {
                    path: clean.clone(),
                    reason: "ancestor is not a directory".to_string(),
                });
            }
            if self.resolve_existing_key(&live, &clean).is_some() {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "destination already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
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
            let hash = hash_bytes(&bytes);
            for anc in legion_sync::path::ancestors(&clean) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                }
            }
            live.insert(
                clean,
                SimNode::File {
                    bytes,
                    hash,
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

    fn exchange_tmp_with_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<bool> {
        validate_flat_internal_name(tmp_name)?;
        let clean = self.check_live_path(live_rel)?;
        self.guard_platform(&clean, EntryKind::File)?;
        if !self.supports_exchange {
            return Ok(false);
        }
        self.wrap_mut("exchange_tmp_with_live", true, || {
            let mut live = self.live.borrow_mut();
            let mut tmp = self.tmp.lock().unwrap();
            let Some(actual) = self.resolve_existing_key(&live, &clean) else {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            if !matches!(live.get(&actual), Some(SimNode::File { .. })) {
                return Err(SyncError::Conflict {
                    path: clean,
                    reason: "target is not a regular file".to_string(),
                });
            }
            let staged_bytes = tmp.remove(tmp_name).ok_or_else(|| SyncError::Fs {
                path: Some(tmp_name.to_string()),
                detail: "tmp not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?;
            let staged_hash = hash_bytes(&staged_bytes);
            let old_node = live.insert(
                actual,
                SimNode::File {
                    bytes: staged_bytes,
                    hash: staged_hash,
                    mtime: Timestamp::new(0, 0),
                    mode: if self.caps.windows_naming {
                        None
                    } else {
                        Some(0o644)
                    },
                    gen_id: self.alloc_gen(),
                },
            );
            if let Some(SimNode::File { bytes: old_b, .. }) = old_node {
                tmp.insert(tmp_name.to_string(), old_b);
            }
            Ok(true)
        })
    }

    fn move_live_to_tmp_no_replace(
        &self,
        live_rel: &str,
        tmp_name: &str,
    ) -> legion_sync::Result<()> {
        let clean = self.check_live_path(live_rel)?;
        validate_flat_internal_name(tmp_name)?;
        self.wrap_mut("move_live_to_tmp_no_replace", true, || {
            let mut live = self.live.borrow_mut();
            let mut tmp = self.tmp.lock().unwrap();
            if tmp.contains_key(tmp_name) {
                return Err(SyncError::Fs {
                    path: Some(tmp_name.to_string()),
                    detail: "tmp already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
            let Some(actual) = self.resolve_existing_key(&live, &clean) else {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            match live.remove(&actual) {
                Some(SimNode::File { bytes, .. }) => {
                    tmp.insert(tmp_name.to_string(), bytes);
                    Ok(())
                }
                Some(other) => {
                    live.insert(actual, other);
                    Err(SyncError::Conflict {
                        path: clean,
                        reason: "expected regular file".to_string(),
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

    fn move_tmp_to_bin_no_replace(
        &self,
        tmp_name: &str,
        bin_data_name: &str,
    ) -> legion_sync::Result<()> {
        validate_flat_internal_name(tmp_name)?;
        validate_flat_internal_name(bin_data_name)?;
        self.wrap_mut("move_tmp_to_bin_no_replace", false, || {
            let mut tmp = self.tmp.lock().unwrap();
            let mut bin = self.bin.borrow_mut();
            if bin.contains_key(bin_data_name) {
                return Err(SyncError::Fs {
                    path: Some(bin_data_name.to_string()),
                    detail: "bin already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
            let bytes = tmp.remove(tmp_name).ok_or_else(|| SyncError::Fs {
                path: Some(tmp_name.to_string()),
                detail: "tmp not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?;
            let hash = Some(hash_bytes(&bytes));
            bin.insert(bin_data_name.to_string(), SimBinData { bytes, hash });
            Ok(())
        })
    }

    fn move_live_to_bin_no_replace(
        &self,
        live_rel: &str,
        bin_data_name: &str,
    ) -> legion_sync::Result<()> {
        let clean = self.check_live_path(live_rel)?;
        validate_flat_internal_name(bin_data_name)?;
        self.wrap_mut("move_live_to_bin_no_replace", false, || {
            let mut live = self.live.borrow_mut();
            let mut bin = self.bin.borrow_mut();
            if bin.contains_key(bin_data_name) {
                return Err(SyncError::Fs {
                    path: Some(bin_data_name.to_string()),
                    detail: "bin already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
            let Some(actual) = self.resolve_existing_key(&live, &clean) else {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "not found".to_string(),
                    kind: FsErrorKind::NotFound,
                });
            };
            match live.remove(&actual) {
                Some(SimNode::File { bytes, hash, .. }) => {
                    bin.insert(
                        bin_data_name.to_string(),
                        SimBinData {
                            bytes,
                            hash: Some(hash),
                        },
                    );
                    Ok(())
                }
                Some(other) => {
                    live.insert(actual, other);
                    Err(SyncError::Conflict {
                        path: clean,
                        reason: "expected file to move to bin".to_string(),
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

    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        validate_flat_internal_name(meta_name)?;
        self.wrap_mut("write_bin_meta", false, || {
            self.bin.borrow_mut().insert(
                meta_name.to_string(),
                SimBinData {
                    bytes: bytes.to_vec(),
                    hash: None,
                },
            );
            Ok(())
        })
    }

    fn read_bin_meta(&self, meta_name: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        validate_flat_internal_name(meta_name)?;
        Ok(self.bin.borrow().get(meta_name).map(|d| d.bytes.clone()))
    }

    fn stat_bin(&self, bin_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        validate_flat_internal_name(bin_name)?;
        Ok(self.bin.borrow().get(bin_name).map(|d| FsMeta {
            kind: EntryKind::File,
            size: d.bytes.len() as u64,
            mtime: Timestamp::new(0, 0),
            mode: Some(0o600),
            link_target: None,
            change_id: None,
        }))
    }

    fn hash_bin(&self, bin_name: &str) -> legion_sync::Result<(Hash, u64)> {
        validate_flat_internal_name(bin_name)?;
        match self.bin.borrow().get(bin_name) {
            Some(d) => Ok((
                d.hash.unwrap_or_else(|| hash_bytes(&d.bytes)),
                d.bytes.len() as u64,
            )),
            None => Err(SyncError::Fs {
                path: Some(bin_name.to_string()),
                detail: "bin file not found".to_string(),
                kind: FsErrorKind::NotFound,
            }),
        }
    }

    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(bin_data_name)?;
        validate_flat_internal_name(tmp_name)?;
        self.wrap_mut("copy_bin_to_tmp", false, || {
            let bytes = self
                .bin
                .borrow()
                .get(bin_data_name)
                .map(|d| d.bytes.clone())
                .ok_or_else(|| SyncError::Fs {
                    path: Some(bin_data_name.to_string()),
                    detail: "bin file not found".to_string(),
                    kind: FsErrorKind::NotFound,
                })?;
            self.tmp.lock().unwrap().insert(tmp_name.to_string(), bytes);
            Ok(())
        })
    }

    fn remove_bin_file(&self, bin_name: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(bin_name)?;
        self.bin.borrow_mut().remove(bin_name);
        Ok(())
    }

    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.bin.borrow().keys().cloned().collect())
    }
}

/// [`MemoryIndexStore`] wrapper whose `apply_batch` commits can be instructed to fail with
/// `SyncError::Storage` without mutating state.
#[derive(Debug, Clone)]
struct FaultyIndexStore {
    inner: MemoryIndexStore,
    fail_countdown: Option<usize>,
    did_fail: bool,
}

impl FaultyIndexStore {
    fn new() -> Self {
        Self {
            inner: MemoryIndexStore::new(),
            fail_countdown: None,
            did_fail: false,
        }
    }

    fn from_inner(inner: MemoryIndexStore) -> Self {
        Self {
            inner,
            fail_countdown: None,
            did_fail: false,
        }
    }

    fn snapshot(&self) -> MemoryIndexStore {
        self.inner.clone()
    }

    fn arm_commit_fail(&mut self, countdown: usize) {
        self.fail_countdown = Some(countdown);
        self.did_fail = false;
    }

    fn disarm(&mut self) {
        self.fail_countdown = None;
    }

    fn failed(&self) -> bool {
        self.did_fail
    }
}

impl IndexStore for FaultyIndexStore {
    fn schema_version(&self) -> legion_sync::Result<u32> {
        self.inner.schema_version()
    }

    fn load_meta(&self) -> legion_sync::Result<Option<FolderMeta>> {
        self.inner.load_meta()
    }

    fn get_record(&self, path: &str) -> legion_sync::Result<Option<IndexRecord>> {
        self.inner.get_record(path)
    }

    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> legion_sync::Result<bool>,
    ) -> legion_sync::Result<()> {
        self.inner.scan_records_in_order(visitor)
    }

    fn scan_subtree_in_order(
        &self,
        subtree_root: &str,
        visitor: &mut dyn FnMut(IndexRecord) -> legion_sync::Result<bool>,
    ) -> legion_sync::Result<()> {
        self.inner.scan_subtree_in_order(subtree_root, visitor)
    }

    fn has_live_descendant(&self, dir_path: &str) -> legion_sync::Result<bool> {
        self.inner.has_live_descendant(dir_path)
    }

    fn live_paths_for_folded(&self, folded_path: &str) -> legion_sync::Result<Vec<String>> {
        self.inner.live_paths_for_folded(folded_path)
    }

    fn records_since(
        &self,
        caller_member: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> legion_sync::Result<RecordsPage> {
        self.inner
            .records_since(caller_member, since_incarnation, since_seq, limit)
    }

    fn list_tombstones(&self) -> legion_sync::Result<Vec<IndexRecord>> {
        self.inner.list_tombstones()
    }

    fn list_conflicts(&self) -> legion_sync::Result<Vec<IndexRecord>> {
        self.inner.list_conflicts()
    }

    fn load_peer_cursors(&self) -> legion_sync::Result<BTreeMap<MemberId, PeerCursor>> {
        self.inner.load_peer_cursors()
    }

    fn list_bin_entries(&self) -> legion_sync::Result<Vec<BinEntry>> {
        self.inner.list_bin_entries()
    }

    fn get_bin_entry(&self, id: &str) -> legion_sync::Result<Option<BinEntry>> {
        self.inner.get_bin_entry(id)
    }

    fn list_held_back(&self) -> legion_sync::Result<Vec<HeldBackRecord>> {
        self.inner.list_held_back()
    }

    fn held_back_count(&self) -> legion_sync::Result<usize> {
        self.inner.held_back_count()
    }

    fn get_deleted_history(&self, path: &str) -> legion_sync::Result<Vec<Hash>> {
        self.inner.get_deleted_history(path)
    }

    fn load_pending_op(&self) -> legion_sync::Result<Option<PendingOp>> {
        self.inner.load_pending_op()
    }

    fn record_count(&self) -> legion_sync::Result<u64> {
        self.inner.record_count()
    }

    fn live_file_count(&self) -> legion_sync::Result<u64> {
        self.inner.live_file_count()
    }

    fn apply_batch(&mut self, batch: StoreBatch) -> legion_sync::Result<()> {
        if let Some(left) = self.fail_countdown {
            if left == 0 {
                self.fail_countdown = None;
                self.did_fail = true;
                return Err(SyncError::Storage {
                    context: "apply_batch".to_string(),
                    detail: "simulated store commit failure".to_string(),
                });
            }
            self.fail_countdown = Some(left - 1);
        }
        self.inner.apply_batch(batch)
    }
}

/// Reader that delivers `cut_at` bytes then returns `ConnectionReset` to simulate mid-fetch drop.
struct DroppingReader {
    data: Vec<u8>,
    pos: usize,
    cut_at: usize,
    did_drop: bool,
}

impl Read for DroppingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.cut_at && !self.did_drop {
            self.did_drop = true;
            return Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "simulated mid-fetch peer drop",
            ));
        }
        let end = if self.did_drop {
            self.data.len()
        } else {
            self.cut_at.min(self.data.len())
        };
        if self.pos >= end {
            return Ok(0);
        }
        let n = (end - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[derive(Debug, Clone, Copy)]
enum PeerFault {
    None,
    /// Drops the first fetch halfway (resumable on offset retry!).
    DropOnceResumable,
    /// Drops every fetch halfway (causes hard fetch failure after retries).
    DropAlways,
    /// Returns `PeerUnavailable` immediately on fetch.
    UnavailableOnFetch,
}

/// Peer adapter wrapping another member's engine, enforcing Invariant (f) on every response,
/// observing `(incarnation, snapshot_seq)` for Invariant (h), and optionally injecting mid-fetch
/// failures.
struct SimPeer<'a> {
    engine: &'a SyncEngine<HonestSimFs, FaultyIndexStore>,
    caller: &'a str,
    member_id: &'a str,
    fault: PeerFault,
    fetch_calls: AtomicUsize,
    leaked_internal: Cell<bool>,
    observed_heads: RefCell<Vec<(u64, u64)>>,
}

impl<'a> SimPeer<'a> {
    fn new(
        engine: &'a SyncEngine<HonestSimFs, FaultyIndexStore>,
        caller: &'a str,
        member_id: &'a str,
        fault: PeerFault,
    ) -> Self {
        Self {
            engine,
            caller,
            member_id,
            fault,
            fetch_calls: AtomicUsize::new(0),
            leaked_internal: Cell::new(false),
            observed_heads: RefCell::new(Vec::new()),
        }
    }
}

impl Peer for SimPeer<'_> {
    fn member_id(&self) -> &str {
        self.member_id
    }

    fn handshake(&self) -> legion_sync::Result<PeerHandshake> {
        self.engine.serve_handshake(self.caller)
    }

    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
        cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(SyncError::Cancelled {
                operation: "records_since".to_string(),
            });
        }
        let page =
            self.engine
                .serve_records_since(self.caller, since_incarnation, since_seq, limit)?;
        for rec in &page.records {
            if is_internal_sync_path(&rec.path) || validate_sync_path(&rec.path).is_err() {
                self.leaked_internal.set(true);
            }
        }
        self.observed_heads
            .borrow_mut()
            .push((page.incarnation, page.snapshot_seq));
        Ok(page)
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        if is_internal_sync_path(path) || validate_sync_path(path).is_err() {
            self.leaked_internal.set(true);
        }
        let call_no = self.fetch_calls.fetch_add(1, Ordering::Relaxed);
        match self.fault {
            PeerFault::None => {
                self.engine
                    .serve_fetch_file_range(self.caller, path, expected_hash, options.offset)
            }
            PeerFault::UnavailableOnFetch => Err(SyncError::PeerUnavailable {
                member: self.member_id.to_string(),
                detail: "simulated mid-fetch peer disconnect".to_string(),
            }),
            PeerFault::DropOnceResumable if call_no == 0 && options.offset == 0 => {
                let mut reader = self.engine.serve_fetch_file_range(
                    self.caller,
                    path,
                    expected_hash,
                    options.offset,
                )?;
                let mut data = Vec::new();
                reader.read_to_end(&mut data).unwrap();
                let cut_at = (data.len() / 2).max(1).min(data.len());
                Ok(Box::new(DroppingReader {
                    data,
                    pos: 0,
                    cut_at,
                    did_drop: false,
                }))
            }
            PeerFault::DropAlways => {
                let mut reader = self.engine.serve_fetch_file_range(
                    self.caller,
                    path,
                    expected_hash,
                    options.offset,
                )?;
                let mut data = Vec::new();
                reader.read_to_end(&mut data).unwrap();
                let cut_at = (data.len() / 2).min(data.len());
                Ok(Box::new(DroppingReader {
                    data,
                    pos: 0,
                    cut_at,
                    did_drop: false,
                }))
            }
            _ => {
                self.engine
                    .serve_fetch_file_range(self.caller, path, expected_hash, options.offset)
            }
        }
    }
}

struct SimMember {
    id: String,
    engine: SyncEngine<HonestSimFs, FaultyIndexStore>,
    clock_skew_secs: i64,
    active: bool,
    group: usize,
    ignore: IgnoreMatcher,
}

impl SimMember {
    fn local_time(&self, global_secs: i64) -> Timestamp {
        Timestamp::new(global_secs + self.clock_skew_secs, 0)
    }
}

/// Tracking state per unique content hash ever created in a live tree (Invariant b), packed into
/// bitmasks over member slots `0..8` for zero-allocation tracking across 50M seeds.
#[derive(Debug, Clone, Copy, Default)]
struct ContentRecord {
    ever_live_mask: u8,
    person_removed_mask: u8,
    bin_expired_mask: u8,
}

fn member_bit(member_id: &str) -> u8 {
    let idx = member_id
        .as_bytes()
        .get(1)
        .copied()
        .unwrap_or(b'0')
        .saturating_sub(b'0') as usize;
    1u8 << (idx & 7)
}

fn member_idx_of(member_id: &str) -> usize {
    (member_id
        .as_bytes()
        .get(1)
        .copied()
        .unwrap_or(b'0')
        .saturating_sub(b'0') as usize)
        & 7
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
    /// Paths confirmed deleted everywhere (Invariant c); checked throughout the ENTIRE run and
    /// cleared ONLY when a person explicitly recreates/restores that path.
    universally_deleted: BTreeSet<String>,
    /// Live conflict copies flagged as one of two versions (Invariant g); must NEVER be replaced
    /// with different content or removed without an explicit person action.
    protected_conflicts: BTreeMap<(String, String), Hash>,
    /// Conflict-copy paths that a person explicitly acted on (resolved, deleted, edited, or
    /// renamed) on some member, so propagating that person's action is allowed.
    person_acted_on_conflict_paths: BTreeSet<String>,
    /// Per `[observer_idx][peer_idx]` highest `(incarnation, seq)` observed (Invariant h).
    observed_peer_seq: [[(u64, u64); 8]; 8],
    record_trace: bool,
    trace: Vec<String>,
    heterogeneous: bool,
}

/// Rich path pool spanning 4 levels of depth, nested directories, file/dir swap candidates, and
/// enough variety to exercise renames, case clashes, and deep directory moves.
const PATH_POOL: &[&str] = &[
    "alpha.txt",
    "beta.txt",
    "gamma.txt",
    "notes.md",
    "readme.txt",
    "dir1/a.txt",
    "dir1/b.txt",
    "dir1/sub/deep.txt",
    "dir1/sub/leaf/bottom.txt",
    "dir2/report.txt",
    "dir2/data.bin",
    "dir2/sub/info.md",
    "proj/src/main.rs",
    "proj/src/lib.rs",
    "proj/docs/v1/spec.md",
    "proj/docs/v1/draft.txt",
    "proj/docs/v2/plan.md",
    "swap",
    "swap/child.txt",
    "swap/nested/leaf.txt",
    "item",
    "item/inside.txt",
    "box/one.txt",
    "box/two.txt",
    "box/inner/three.txt",
    "alt/one.txt",
    "alt/inner/three.txt",
];

fn brake_disabled() -> DeletionBrakeConfig {
    DeletionBrakeConfig {
        min_deleted_files: usize::MAX,
        min_deleted_percent: 100,
    }
}

impl SimWorld {
    fn new(
        root_seed: u64,
        scenario: &'static str,
        seed: u64,
        heterogeneous: bool,
        ignore_patterns: Vec<String>,
        record_trace: bool,
    ) -> Self {
        let mut rng = Rng::new(seed);
        let n = 2 + rng.range(5); // 2..=6 members
        let base_secs = 1_795_608_000i64; // 2026-11-25 12:00:00 UTC

        let member_names: Vec<String> = (0..n).map(|i| format!("m{i}")).collect();
        let ignore = IgnoreMatcher::compile(&ignore_patterns).unwrap();
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
            let supports_exchange = (i % 2) == 0;
            let mut cfg = FolderConfig::new("sim", id.clone(), member_names.clone());
            cfg.ignore = ignore_patterns.clone();
            cfg.initial_incarnation = Some(rng.next_u64() | 1);
            let fs = HonestSimFs::new(caps, supports_exchange);
            let store = FaultyIndexStore::new();
            let mut engine = SyncEngine::open(cfg, fs, store).unwrap();
            // Disable the mass-deletion brake on tiny 2..15-file randomized schedules unless
            // explicitly testing the brake, so normal schedules can delete down to 0 files freely.
            engine.set_deletion_brake(brake_disabled());
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
                ignore: ignore.clone(),
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
            protected_conflicts: BTreeMap::new(),
            person_acted_on_conflict_paths: BTreeSet::new(),
            observed_peer_seq: [[(0, 0); 8]; 8],
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
            run_one_seed(self.root_seed, true);
        }
        let trace_dump = self.trace.join("\n  ");
        panic!(
            "\n=== SIMULATOR INVARIANT VIOLATION ===\n\
             seed: {} (scenario: {})\n\
             replay: LEGION_SYNC_SEED={} cargo test -p legion-sync --release --test simulator replay_seed -- --nocapture\n\
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

    fn fresh_bytes(&mut self) -> (Vec<u8>, Hash) {
        let id = self.next_content_id;
        self.next_content_id += 1;
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&self.seed.to_le_bytes());
        bytes[8..].copy_from_slice(&id.to_le_bytes());
        let hash = hash_bytes(&bytes);
        (bytes.to_vec(), hash)
    }

    fn record_live_presence(&mut self) {
        let contents = &mut self.contents;
        for (idx, m) in self.members.iter().enumerate() {
            if !m.active {
                continue;
            }
            let bit = 1u8 << (idx & 7);
            m.engine.fs().for_each_live_hash(|h| {
                if let Some(rec) = contents.get_mut(&h) {
                    rec.ever_live_mask |= bit;
                }
            });
        }
    }

    /// Refreshes tracked live conflict copies and asserts Invariant (g): no live conflict copy was
    /// replaced with different content or removed without an explicit person action.
    fn check_and_track_conflicts(&mut self) {
        if !self.protected_conflicts.is_empty() {
            let mut violation: Option<String> = None;
            for ((mid, path), expected_hash) in &self.protected_conflicts {
                if self.person_acted_on_conflict_paths.contains(path) {
                    continue;
                }
                let Some(m) = self.members.iter().find(|m| m.active && &m.id == mid) else {
                    continue;
                };
                if m.engine.fs().is_unreadable(path) {
                    continue;
                }
                match m.engine.fs().live_file_hash(path) {
                    Some(actual) if actual == *expected_hash => {}
                    other => {
                        violation = Some(format!(
                            "Invariant (g): live conflict copy {path:?} on {mid} was replaced or removed without person action (expected {expected_hash}, got {other:?})"
                        ));
                        break;
                    }
                }
            }
            if let Some(msg) = violation {
                self.fail(msg);
            }
        }

        // Register any newly discovered live conflict copies.
        for m in &self.members {
            if !m.active {
                continue;
            }
            for rec in m.engine.store().list_conflicts().unwrap() {
                if !rec.deleted
                    && rec.conflict_of.is_some()
                    && !self.person_acted_on_conflict_paths.contains(&rec.path)
                    && let Some(h) = rec.hash
                    && m.engine.fs().live_file_hash(&rec.path) == Some(h)
                {
                    self.protected_conflicts.insert((m.id.clone(), rec.path), h);
                }
            }
        }
    }

    fn note_created(&mut self, member_id: &str, hash: Hash) {
        let bit = member_bit(member_id);
        let rec = self.contents.entry(hash).or_default();
        rec.ever_live_mask |= bit;
    }

    fn note_person_removed(&mut self, member_id: &str, hash: Hash) {
        if let Some(rec) = self.contents.get_mut(&hash) {
            rec.person_removed_mask |= member_bit(member_id);
        }
    }

    fn note_person_touched_path(&mut self, member_id: &str, path: &str) {
        let prefix = format!("{path}/");
        let touched_conflict = self
            .protected_conflicts
            .keys()
            .any(|(_, p)| p == path || p.starts_with(&prefix));
        if touched_conflict {
            self.person_acted_on_conflict_paths.insert(path.to_string());
            let sub: Vec<String> = self
                .protected_conflicts
                .keys()
                .filter(|(_, p)| p.starts_with(&prefix))
                .map(|(_, p)| p.clone())
                .collect();
            for p in sub {
                self.person_acted_on_conflict_paths.insert(p);
            }
        }
        self.protected_conflicts
            .remove(&(member_id.to_string(), path.to_string()));
        self.protected_conflicts
            .retain(|(mid, p), _| !(mid == member_id && p.starts_with(&prefix)));
    }

    fn clear_universally_deleted_for_path(&mut self, path: &str) {
        self.universally_deleted.remove(path);
        for anc in legion_sync::path::ancestors(path) {
            self.universally_deleted.remove(anc);
        }
        // If a person writes/restores `path`, a subsequent conflict on `path` may legitimately
        // re-derive a deterministic content-addressed conflict copy path for `path`.
        if !self.universally_deleted.is_empty() {
            self.universally_deleted.retain(|p| {
                parse_conflict_copy_path(p).is_none_or(|parsed| parsed.original_path != path)
            });
        }
    }

    fn pick_path(&mut self) -> String {
        PATH_POOL[self.rng.range(PATH_POOL.len())].to_string()
    }

    /// Scans `members[idx]` while enforcing Invariant (i): no deletion is ever recorded at or
    /// under any skipped path.
    fn scan_member(&mut self, idx: usize) {
        let has_unreadable = self.members[idx].engine.fs().has_unreadable();
        let pre_live: Vec<String> = if has_unreadable {
            let mut v = Vec::new();
            self.members[idx]
                .engine
                .store()
                .scan_records_in_order(&mut |r| {
                    if !r.deleted {
                        v.push(r.path);
                    }
                    Ok(true)
                })
                .unwrap();
            v
        } else {
            Vec::new()
        };

        let now = self.members[idx].local_time(self.global_secs);
        let rep = self.members[idx].engine.scan(now).unwrap();

        if !rep.skipped_errors.is_empty() {
            let mid = self.members[idx].id.clone();
            for (skipped_path, _) in &rep.skipped_errors {
                for p in &pre_live {
                    if p == skipped_path || is_descendant_of(p, skipped_path) {
                        let rec = self.members[idx].engine.store().get_record(p).unwrap();
                        if rec.is_none_or(|r| r.deleted) {
                            self.fail(format!(
                                "Invariant (i): scan on {mid} recorded deletion for {p:?} under skipped path {skipped_path:?}"
                            ));
                        }
                    }
                }
            }
        }
        self.record_live_presence();
        self.check_and_track_conflicts();
    }

    fn user_create_or_edit_file(&mut self, idx: usize, path: &str, also_scan: bool) {
        if self.members[idx].engine.fs().is_unreadable(path) {
            return;
        }
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let (bytes, hash) = self.fresh_bytes();

        let mut ancs_to_remove: Vec<&str> = Vec::new();
        let mut self_is_dir = false;
        {
            let live = self.members[idx].engine.fs().live.borrow();
            for anc in legion_sync::path::ancestors(path) {
                if let Some(node) = live.get(anc)
                    && node.kind() != EntryKind::Dir
                {
                    ancs_to_remove.push(anc);
                }
            }
            if let Some(node) = live.get(path)
                && node.kind() == EntryKind::Dir
            {
                self_is_dir = true;
            }
        }
        for anc in ancs_to_remove {
            let removed = self.members[idx].engine.fs().user_remove_recursive(anc);
            for h in removed {
                self.note_person_removed(&member_id, h);
            }
            self.note_person_touched_path(&member_id, anc);
        }
        if self_is_dir {
            let removed = self.members[idx].engine.fs().user_remove_recursive(path);
            for h in removed {
                self.note_person_removed(&member_id, h);
            }
        }

        let overwritten = self.members[idx]
            .engine
            .fs()
            .user_write_file(path, bytes, hash, now);
        if let Some(old_h) = overwritten {
            self.note_person_removed(&member_id, old_h);
        }
        self.note_person_touched_path(&member_id, path);
        self.note_created(&member_id, hash);
        self.clear_universally_deleted_for_path(path);

        if also_scan {
            self.scan_member(idx);
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
        if self.members[idx].engine.fs().is_unreadable(path) {
            return;
        }
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let removed = self.members[idx].engine.fs().user_remove_recursive(path);
        for h in removed {
            self.note_person_removed(&member_id, h);
        }
        self.note_person_touched_path(&member_id, path);
        if also_scan {
            self.scan_member(idx);
        }
        self.log(|| {
            format!(
                "{member_id} delete {path:?} scan={also_scan} t={}",
                now.secs
            )
        });
    }

    fn user_replace_file_with_dir(&mut self, idx: usize, path: &str, child_name: &str) {
        if self.members[idx].engine.fs().is_unreadable(path) {
            return;
        }
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let removed = self.members[idx].engine.fs().user_remove_recursive(path);
        for h in removed {
            self.note_person_removed(&member_id, h);
        }
        self.note_person_touched_path(&member_id, path);
        self.members[idx].engine.fs().user_mkdir(path, now);
        let child_path = format!("{path}/{child_name}");
        let (bytes, hash) = self.fresh_bytes();
        self.members[idx]
            .engine
            .fs()
            .user_write_file(&child_path, bytes, hash, now);
        self.note_created(&member_id, hash);
        self.clear_universally_deleted_for_path(&child_path);
        self.scan_member(idx);
        self.log(|| {
            format!(
                "{member_id} file->dir {path:?} + {child_path:?} hash={}",
                &hash.to_hex()[..8]
            )
        });
    }

    fn user_rename_directory_with_edit(&mut self, idx: usize, from_dir: &str, to_dir: &str) {
        let member_id = self.members[idx].id.clone();
        let now = self.members[idx].local_time(self.global_secs);
        let overwritten = self.members[idx]
            .engine
            .fs()
            .user_rename_dir(from_dir, to_dir, now);
        for h in overwritten {
            self.note_person_removed(&member_id, h);
        }
        self.note_person_touched_path(&member_id, from_dir);
        self.note_person_touched_path(&member_id, to_dir);
        // Clear universally_deleted for every moved path under `to_dir`.
        let moved: Vec<String> = self.members[idx]
            .engine
            .fs()
            .live_paths()
            .into_iter()
            .filter(|(p, _)| p == to_dir || is_descendant_of(p, to_dir))
            .map(|(p, _)| p)
            .collect();
        for p in moved {
            self.clear_universally_deleted_for_path(&p);
        }
        self.scan_member(idx);
        self.log(|| format!("{member_id} rename_dir {from_dir:?} -> {to_dir:?}"));
    }

    /// Runs `recover()` with optional crash injection DURING recovery itself (verifying a second
    /// `recover()` completes cleanly with zero loss!).
    fn recover_member(&mut self, idx: usize, crash_during_recovery: bool) {
        let mid = self.members[idx].id.clone();
        if crash_during_recovery {
            let op = self.rng.range(4);
            let after = self.rng.chance(1, 2);
            self.members[idx].engine.fs().arm_crash(op, after);
            let res = self.members[idx].engine.recover();
            let crashed = self.members[idx].engine.fs().crashed();
            self.members[idx].engine.fs().disarm_fault();
            match res {
                Ok(_) => {}
                Err(err) if err.is_crash() && crashed => {
                    self.log(|| {
                        format!(
                            "recover {mid} CRASH_DURING_RECOVERY(op={op},after={after}) -> recover() again"
                        )
                    });
                    self.members[idx].engine.recover().unwrap();
                }
                Err(err) => self.fail(format!("unexpected error during recover on {mid}: {err}")),
            }
        } else {
            self.members[idx].engine.recover().unwrap();
        }
        assert!(
            self.members[idx]
                .engine
                .store()
                .load_pending_op()
                .unwrap()
                .is_none(),
            "pending_op must be cleared after recover() on {mid}"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn sync_pair_with_faults(
        &mut self,
        dst_idx: usize,
        src_idx: usize,
        inject_crash: bool,
        inject_non_crash_fs: bool,
        inject_store_fail: bool,
        inject_mid_apply_edit: bool,
        peer_fault: PeerFault,
    ) {
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

        // Snapshot observer's cursor before sync for Invariant (h) check.
        let prev_cursor = self.members[dst_idx]
            .engine
            .store()
            .load_peer_cursors()
            .unwrap()
            .get(&src_id)
            .cloned();

        // Optional mid-apply human edit/create/delete between any two FS steps!
        if inject_mid_apply_edit {
            let live_files: Vec<String> = self.members[dst_idx]
                .engine
                .fs()
                .live_paths()
                .into_iter()
                .filter(|(_, k)| *k == EntryKind::File)
                .map(|(p, _)| p)
                .collect();
            let target_path = if !live_files.is_empty() && self.rng.chance(2, 3) {
                live_files[self.rng.range(live_files.len())].clone()
            } else {
                self.pick_path()
            };
            let countdown = self.rng.range(4);
            let op = if self.rng.chance(3, 4) {
                let (bytes, hash) = self.fresh_bytes();
                MidApplyOp::EditOrCreateFile {
                    path: target_path,
                    bytes,
                    hash,
                    mtime: Timestamp::new(now.secs + 1, 0),
                }
            } else {
                MidApplyOp::DeletePath { path: target_path }
            };
            self.members[dst_idx]
                .engine
                .fs()
                .arm_mid_apply(countdown, op);
        }

        if inject_crash {
            let op = self.rng.range(8);
            let after = self.rng.chance(1, 2);
            self.members[dst_idx].engine.fs().arm_crash(op, after);
        } else if inject_non_crash_fs {
            let op = self.rng.range(6);
            let kind = match self.rng.range(3) {
                0 => FsErrorKind::PermissionDenied,
                1 => FsErrorKind::NoSpace,
                _ => FsErrorKind::CrossDevice,
            };
            self.members[dst_idx]
                .engine
                .fs()
                .arm_non_crash_fault(op, kind);
        }

        if inject_store_fail {
            let op = self.rng.range(5);
            self.members[dst_idx].engine.store_mut().arm_commit_fail(op);
        }

        let (res, leaked, heads) = if dst_idx < src_idx {
            let (left, right) = self.members.split_at_mut(src_idx);
            let dst = &mut left[dst_idx];
            let src = &right[0];
            let peer = SimPeer::new(&src.engine, &dst.id, &src.id, peer_fault);
            let r = dst.engine.pull_from_peer(&peer, now);
            (
                r,
                peer.leaked_internal.get(),
                peer.observed_heads.into_inner(),
            )
        } else {
            let (left, right) = self.members.split_at_mut(dst_idx);
            let src = &left[src_idx];
            let dst = &mut right[0];
            let peer = SimPeer::new(&src.engine, &dst.id, &src.id, peer_fault);
            let r = dst.engine.pull_from_peer(&peer, now);
            (
                r,
                peer.leaked_internal.get(),
                peer.observed_heads.into_inner(),
            )
        };

        // Reconcile any mid-apply human edit into provenance tables BEFORE checking invariants!
        if let Some(fired) = self.members[dst_idx].engine.fs().take_mid_apply_fired() {
            for h in fired.removed_hashes {
                self.note_person_removed(&dst_id, h);
            }
            if let Some(h) = fired.created_hash {
                self.note_created(&dst_id, h);
                self.clear_universally_deleted_for_path(&fired.touched_path);
            }
            self.note_person_touched_path(&dst_id, &fired.touched_path);
            self.log(|| format!("{dst_id} MID_APPLY_HUMAN on {:?}", fired.touched_path));
        }

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
        if self.members[dst_idx].engine.fs().platform_violated() {
            self.fail(format!(
                "Invariant (f): platform naming/capability rule violated on {dst_id} during sync from {src_id}"
            ));
        }

        // Invariant (h): within an unchanged incarnation, peer's sequence as seen by observer
        // MUST NEVER go backwards!
        let d_slot = member_idx_of(&dst_id);
        let s_slot = member_idx_of(&src_id);
        for (inc, seq) in heads {
            let (prev_inc, prev_seq) = self.observed_peer_seq[d_slot][s_slot];
            if prev_inc != 0 && inc == prev_inc && seq < prev_seq {
                self.fail(format!(
                    "Invariant (h): peer {src_id} sequence went backwards for {dst_id} within incarnation {inc}: {prev_seq} -> {seq}"
                ));
            }
            self.observed_peer_seq[d_slot][s_slot] = (inc, seq);
        }

        let crashed = self.members[dst_idx].engine.fs().crashed();
        let non_crash_fs = self.members[dst_idx].engine.fs().had_non_crash_fault();
        let store_failed = self.members[dst_idx].engine.store().failed();
        self.members[dst_idx].engine.fs().disarm_fault();
        self.members[dst_idx].engine.store_mut().disarm();

        // Also check observer's persisted cursor never went backwards within the same incarnation.
        if let Some(prev) = prev_cursor
            && let Some(cur) = self.members[dst_idx]
                .engine
                .store()
                .load_peer_cursors()
                .unwrap()
                .get(&src_id)
                .cloned()
            && cur.incarnation == prev.incarnation
            && cur.seq < prev.seq
        {
            self.fail(format!(
                "Invariant (h): cursor for {src_id} on {dst_id} moved backwards within incarnation {}: {} -> {}",
                cur.incarnation, prev.seq, cur.seq
            ));
        }

        match res {
            Ok(_) => {
                self.log(|| format!("sync {dst_id} <- {src_id}"));
            }
            Err(err) if err.is_crash() && crashed => {
                let crash_in_recover = self.rng.chance(1, 3);
                self.log(|| {
                    format!("sync {dst_id} <- {src_id} CRASH -> recover(crash_in_recover={crash_in_recover})")
                });
                self.recover_member(dst_idx, crash_in_recover);
            }
            Err(_) if non_crash_fs => {
                // Item G: non-crash FS error mid-apply MUST recover immediately (no stuck pending_op)!
                if self.members[dst_idx]
                    .engine
                    .store()
                    .load_pending_op()
                    .unwrap()
                    .is_some()
                {
                    self.fail(format!(
                        "Item G violation: non-crash FS error left pending_op stuck on {dst_id}"
                    ));
                }
                self.log(|| format!("sync {dst_id} <- {src_id} NON_CRASH_FS_FAULT recovered"));
            }
            Err(_) if store_failed => {
                // Store commit failure may occur during immediate recovery too; call recover() cleanly.
                self.recover_member(dst_idx, false);
                self.log(|| format!("sync {dst_id} <- {src_id} STORE_COMMIT_FAIL recovered"));
            }
            Err(_) if !matches!(peer_fault, PeerFault::None) => {
                // Serving peer dropped mid-fetch or became unavailable -> no pending_op stuck!
                if self.members[dst_idx]
                    .engine
                    .store()
                    .load_pending_op()
                    .unwrap()
                    .is_some()
                {
                    self.fail(format!(
                        "Mid-fetch peer failure left pending_op stuck on {dst_id}"
                    ));
                }
                self.log(|| format!("sync {dst_id} <- {src_id} PEER_FAULT({peer_fault:?})"));
            }
            Err(err) => {
                self.fail(format!(
                    "unexpected error syncing {dst_id} <- {src_id}: {err}"
                ));
            }
        }

        self.record_live_presence();
        self.check_and_track_conflicts();
    }

    fn expire_bins_online(&mut self) {
        let mut expired_count = 0usize;
        for (idx, m) in self.members.iter_mut().enumerate() {
            if !m.active {
                continue;
            }
            let now = m.local_time(self.global_secs);
            let expired = m.engine.expire_bin(now).unwrap();
            let bit = 1u8 << (idx & 7);
            for entry in expired {
                if let Some(h) = entry.hash
                    && let Some(rec) = self.contents.get_mut(&h)
                {
                    rec.bin_expired_mask |= bit;
                }
                expired_count += 1;
            }
        }
        if expired_count > 0 {
            self.log(|| format!("expire_bin removed {expired_count} entries"));
        }
    }

    fn step_restore_from_bin(&mut self, idx: usize) {
        let entries = self.members[idx].engine.list_bin().unwrap();
        if entries.is_empty() {
            return;
        }
        let entry = entries[self.rng.range(entries.len())].clone();
        let strategy = if self.rng.chance(1, 2) {
            RestoreStrategy::Rename
        } else {
            RestoreStrategy::Replace
        };
        let now = self.members[idx].local_time(self.global_secs);
        let mid = self.members[idx].id.clone();

        // Scan first if random chance so both scanned and unscanned targets are exercised.
        if self.rng.chance(1, 2) {
            self.scan_member(idx);
        }

        let overwritten_before = self.members[idx]
            .engine
            .fs()
            .live_file_hash(&entry.original_path);

        match self.members[idx]
            .engine
            .restore_from_bin(&entry.id, None, strategy, now)
        {
            Ok(restored_path) => {
                if strategy == RestoreStrategy::Replace
                    && restored_path == entry.original_path
                    && let Some(old_h) = overwritten_before
                    && Some(old_h) != entry.hash
                {
                    self.note_person_removed(&mid, old_h);
                }
                if let Some(h) = entry.hash {
                    self.note_created(&mid, h);
                }
                self.clear_universally_deleted_for_path(&restored_path);
                self.note_person_touched_path(&mid, &restored_path);
                self.record_live_presence();
                self.check_and_track_conflicts();
                self.log(|| {
                    format!(
                        "{mid} restore_from_bin {:?} -> {restored_path:?} ({strategy:?})",
                        entry.id
                    )
                });
            }
            Err(
                SyncError::Conflict { .. }
                | SyncError::Fs { .. }
                | SyncError::InvalidPath { .. }
                | SyncError::Unsupported { .. },
            ) => {
                // Clean refusal (e.g. unscanned file at target, ancestor is a file, or platform rule).
                assert!(
                    self.members[idx]
                        .engine
                        .store()
                        .load_pending_op()
                        .unwrap()
                        .is_none(),
                    "restore_from_bin error left pending_op stuck on {mid}"
                );
            }
            Err(err) => self.fail(format!("unexpected restore_from_bin error on {mid}: {err}")),
        }
    }

    fn step_resolve_conflict(&mut self, idx: usize) {
        let conflicts = self.members[idx].engine.list_conflicts().unwrap();
        if conflicts.is_empty() {
            return;
        }
        let info = conflicts[self.rng.range(conflicts.len())].clone();
        let choice = match self.rng.range(3) {
            0 => ConflictChoice::KeepOriginal,
            1 => ConflictChoice::KeepConflict,
            _ => ConflictChoice::KeepBoth,
        };
        let now = self.members[idx].local_time(self.global_secs);
        let mid = self.members[idx].id.clone();

        let orig_hash_before = self.members[idx]
            .engine
            .fs()
            .live_file_hash(&info.original_path);
        let conflict_hash_before = self.members[idx]
            .engine
            .fs()
            .live_file_hash(&info.conflict_path);

        match self.members[idx]
            .engine
            .resolve_conflict(&info.conflict_path, choice, now)
        {
            Ok(()) => {
                match choice {
                    ConflictChoice::KeepOriginal => {
                        if let Some(h) = conflict_hash_before {
                            self.note_person_removed(&mid, h);
                        }
                    }
                    ConflictChoice::KeepConflict => {
                        if let Some(h) = orig_hash_before {
                            self.note_person_removed(&mid, h);
                        }
                    }
                    ConflictChoice::KeepBoth => {}
                }
                self.person_acted_on_conflict_paths
                    .insert(info.conflict_path.clone());
                self.protected_conflicts
                    .remove(&(mid.clone(), info.conflict_path.clone()));
                self.protected_conflicts
                    .remove(&(mid.clone(), info.original_path.clone()));
                self.clear_universally_deleted_for_path(&info.original_path);
                self.record_live_presence();
                self.check_and_track_conflicts();
                self.log(|| {
                    format!(
                        "{mid} resolve_conflict {:?} ({choice:?})",
                        info.conflict_path
                    )
                });
            }
            Err(
                SyncError::Conflict { .. } | SyncError::Fs { .. } | SyncError::InvalidPath { .. },
            ) => {
                assert!(
                    self.members[idx]
                        .engine
                        .store()
                        .load_pending_op()
                        .unwrap()
                        .is_none(),
                    "resolve_conflict error left pending_op stuck on {mid}"
                );
            }
            Err(err) => self.fail(format!("unexpected resolve_conflict error on {mid}: {err}")),
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

        // Membership change: add `new_id` to every active member (including partitioned ones
        // when they reconnect — here we update membership on all active members).
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
        let mut cfg = FolderConfig::new("sim", new_id.clone(), all_names);
        cfg.initial_incarnation = Some(self.rng.next_u64() | 1);
        // Supply departed members from the shared folder definition (`sync.toml`) so the joining
        // member can verify historical version vectors authored before those members left.
        cfg.former_members = self
            .members
            .iter()
            .filter(|m| !m.active)
            .map(|m| m.id.clone())
            .collect();
        let fs = HonestSimFs::new(caps, self.members.len().is_multiple_of(2));
        let store = FaultyIndexStore::new();
        let mut engine = SyncEngine::open(cfg, fs, store).unwrap();
        engine.set_deletion_brake(brake_disabled());
        let skew = self.rng.range_i64(-2 * 86_400, 2 * 86_400);
        let new_idx = self.members.len();
        self.members.push(SimMember {
            id: new_id.clone(),
            engine,
            clock_skew_secs: skew,
            active: true,
            group: 0,
            ignore: IgnoreMatcher::default(),
        });

        let count = 1 + self.rng.range(2);
        let now = self.members[new_idx].local_time(self.global_secs);
        for j in 0..count {
            let path = if j == 0 && self.rng.chance(1, 2) {
                self.pick_path()
            } else {
                format!("joined/{new_id}_{j}.txt")
            };
            let (bytes, hash) = self.fresh_bytes();
            self.members[new_idx]
                .engine
                .fs()
                .user_write_file(&path, bytes, hash, now);
            self.note_created(&new_id, hash);
            self.clear_universally_deleted_for_path(&path);
        }
        self.scan_member(new_idx);
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

    fn sync_connected_groups_to_quiescence(&mut self) {
        let active = self.active_indices();
        for &idx in &active {
            self.recover_member(idx, false);
            self.scan_member(idx);
        }

        for round in 0..32 {
            let mut progress = false;
            let active = self.active_indices();
            for &dst_idx in &active {
                for &src_idx in &active {
                    if dst_idx == src_idx
                        || self.members[dst_idx].group != self.members[src_idx].group
                    {
                        continue;
                    }
                    if round > 0 {
                        // Skip pairs on follow-up rounds only when both sides are completely caught
                        // up on records, direct acknowledgements, rejoining state, and held-back
                        // entries; Round 0 always runs every connected pair unconditionally.
                        let dst_m = self.members[dst_idx].engine.meta().unwrap();
                        let src_m = self.members[src_idx].engine.meta().unwrap();
                        let dst_cursors = self.members[dst_idx]
                            .engine
                            .store()
                            .load_peer_cursors()
                            .unwrap();
                        let src_cursors = self.members[src_idx]
                            .engine
                            .store()
                            .load_peer_cursors()
                            .unwrap();
                        let dst_c = dst_cursors.get(&self.members[src_idx].id);
                        let src_c = src_cursors.get(&self.members[dst_idx].id);
                        let records_caught_up = dst_c.is_some_and(|c| {
                            c.incarnation == src_m.incarnation && c.seq == src_m.local_seq
                        });
                        let ack_caught_up = match (dst_c, src_c) {
                            (Some(dc), Some(sc)) if sc.incarnation == dst_m.incarnation => {
                                dc.acked_local_incarnation == dst_m.incarnation
                                    && dc.acked_local_seq == sc.seq
                                    && sc.seq <= dst_m.local_seq
                            }
                            (_, None) => true,
                            _ => false,
                        };
                        let no_rejoin = !dst_m.rejoining_peers.contains(&self.members[src_idx].id);
                        let no_held_back = self.members[dst_idx]
                            .engine
                            .store()
                            .held_back_count()
                            .unwrap()
                            == 0;
                        if records_caught_up && ack_caught_up && no_rejoin && no_held_back {
                            continue;
                        }
                    }
                    let now = self.members[dst_idx].local_time(self.global_secs);
                    let (report, heads) = if dst_idx < src_idx {
                        let (left, right) = self.members.split_at_mut(src_idx);
                        let peer = SimPeer::new(
                            &right[0].engine,
                            &left[dst_idx].id,
                            &right[0].id,
                            PeerFault::None,
                        );
                        let r = match left[dst_idx].engine.pull_from_peer(&peer, now) {
                            Ok(r) => r,
                            Err(err) => {
                                let dst_c =
                                    left[dst_idx].engine.store().load_peer_cursors().unwrap();
                                let src_m = right[0].engine.meta().unwrap();
                                panic!(
                                    "quiescence pull {} <- {} failed: {err}\n  dst_cursors={dst_c:?}\n  src_meta_inc={} src_seq={} src_fp={:?}",
                                    left[dst_idx].id,
                                    right[0].id,
                                    src_m.incarnation,
                                    src_m.local_seq,
                                    src_m.head_fingerprint,
                                );
                            }
                        };
                        assert!(!peer.leaked_internal.get());
                        (r, peer.observed_heads.into_inner())
                    } else {
                        let (left, right) = self.members.split_at_mut(dst_idx);
                        let peer = SimPeer::new(
                            &left[src_idx].engine,
                            &right[0].id,
                            &left[src_idx].id,
                            PeerFault::None,
                        );
                        let r = match right[0].engine.pull_from_peer(&peer, now) {
                            Ok(r) => r,
                            Err(err) => {
                                let dst_c = right[0].engine.store().load_peer_cursors().unwrap();
                                let src_m = left[src_idx].engine.meta().unwrap();
                                panic!(
                                    "quiescence pull {} <- {} failed: {err}\n  dst_cursors={dst_c:?}\n  src_meta_inc={} src_seq={} src_fp={:?}",
                                    right[0].id,
                                    left[src_idx].id,
                                    src_m.incarnation,
                                    src_m.local_seq,
                                    src_m.head_fingerprint,
                                );
                            }
                        };
                        assert!(!peer.leaked_internal.get());
                        (r, peer.observed_heads.into_inner())
                    };
                    let d_slot = dst_idx & 7;
                    let s_slot = src_idx & 7;
                    for (inc, seq) in heads {
                        let (prev_inc, prev_seq) = self.observed_peer_seq[d_slot][s_slot];
                        if prev_inc != 0 && inc == prev_inc && seq < prev_seq {
                            let dst_id = self.members[dst_idx].id.clone();
                            let src_id = self.members[src_idx].id.clone();
                            self.fail(format!(
                                "Invariant (h): peer {src_id} sequence went backwards for {dst_id} within incarnation {inc}: {prev_seq} -> {seq}"
                            ));
                        }
                        self.observed_peer_seq[d_slot][s_slot] = (inc, seq);
                    }
                    if report.applied > 0
                        || report.held_back_resolved > 0
                        || report.records_seen > 0
                    {
                        progress = true;
                    }
                }
            }
            self.record_live_presence();
            self.check_and_track_conflicts();
            if !progress {
                break;
            }
        }
    }

    /// Captures newly universally-deleted paths mid-run so Invariant (c) is checked across the
    /// ENTIRE run, not only after 65-day bin expiry at the end.
    fn capture_universally_deleted_mid_run(&mut self) {
        let active = self.active_indices();
        if active.is_empty() {
            return;
        }
        // Only record when no active member has unreadable dirs or an in-flight pending_op.
        for &idx in &active {
            if self.members[idx].engine.fs().has_unreadable()
                || self.members[idx]
                    .engine
                    .store()
                    .load_pending_op()
                    .unwrap()
                    .is_some()
            {
                return;
            }
        }
        let mut candidate_tombstones: BTreeSet<String> = BTreeSet::new();
        for &idx in &active {
            for ts in self.members[idx].engine.store().list_tombstones().unwrap() {
                candidate_tombstones.insert(ts.path);
            }
        }
        for path in candidate_tombstones {
            // Derived content-addressed conflict copy names can legitimately be produced again by
            // a later conflict involving the same content hash (e.g. after restoring from bin).
            if parse_conflict_copy_path(&path).is_some() {
                continue;
            }
            let deleted_on_every_active = active.iter().all(|&idx| {
                self.members[idx]
                    .engine
                    .fs()
                    .stat_live(&path)
                    .unwrap()
                    .is_none()
                    && self.members[idx]
                        .engine
                        .store()
                        .get_record(&path)
                        .unwrap()
                        .is_some_and(|r| r.deleted)
                    && !self.members[idx]
                        .engine
                        .store()
                        .has_live_descendant(&path)
                        .unwrap()
            });
            if deleted_on_every_active {
                self.universally_deleted.insert(path);
            }
        }
    }

    /// Verifies invariants (a), (b), (c), (f), (g) across all active connected groups.
    fn check_invariants(&mut self, check_convergence: bool) {
        // Invariant (g): live conflict copies never replaced or removed without person action.
        self.check_and_track_conflicts();

        // Capture universally deleted paths throughout the whole run!
        self.capture_universally_deleted_mid_run();

        // Invariant (f): containment, platform honesty, and clean `.legion-sync/tmp` at quiescence.
        for m in &self.members {
            if !m.active {
                continue;
            }
            if m.engine.fs().root_escaped() {
                self.fail(format!("Invariant (f): root escape detected on {}", m.id));
            }
            if m.engine.fs().platform_violated() {
                self.fail(format!(
                    "Invariant (f): platform rule violation detected on {}",
                    m.id
                ));
            }
            if check_convergence && !m.engine.fs().list_tmp().unwrap().is_empty() {
                self.fail(format!(
                    "Invariant (f): leftover tmp files on {} at quiescence",
                    m.id
                ));
            }
        }

        // Invariant (a): convergence for BOTH homogeneous AND mixed-OS connected groups!
        if check_convergence {
            let mut by_group: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (idx, m) in self.members.iter().enumerate() {
                if m.active {
                    by_group.entry(m.group).or_default().push(idx);
                }
            }
            for (group_id, group_idxs) in by_group {
                if group_idxs.len() < 2 {
                    continue;
                }
                if !self.heterogeneous {
                    let ref_m = &self.members[group_idxs[0]];
                    let ref_tree = ref_m.engine.fs().live_tree(&ref_m.ignore);
                    for &other_idx in &group_idxs[1..] {
                        let other = &self.members[other_idx];
                        let other_tree = other.engine.fs().live_tree(&other.ignore);
                        if ref_tree != other_tree {
                            let mut diff_records = String::new();
                            let all_keys: BTreeSet<&String> =
                                ref_tree.keys().chain(other_tree.keys()).collect();
                            for k in all_keys {
                                if ref_tree.get(k) != other_tree.get(k) {
                                    let r0 = ref_m.engine.store().get_record(k).unwrap();
                                    let r1 = other.engine.store().get_record(k).unwrap();
                                    diff_records.push_str(&format!(
                                        "\n  diff {k:?}:\n    {}: {:?}\n    {}: {:?}",
                                        ref_m.id, r0, other.id, r1
                                    ));
                                }
                            }
                            self.fail(format!(
                                "Invariant (a): connected group {group_id} diverged between {} and {}{diff_records}",
                                ref_m.id, other.id
                            ));
                        }
                    }
                } else {
                    // Mixed-OS convergence: build reference union from Unix members (or all
                    // members), then verify every member materializes every representable
                    // non-clashing entry identically and never materializes forbidden entries.
                    let mut ref_union: BTreeMap<String, TreeEntry> = BTreeMap::new();
                    for &idx in &group_idxs {
                        let m = &self.members[idx];
                        for (k, v) in m.engine.fs().live_tree(&m.ignore) {
                            if let Some(existing) = ref_union.get(&k)
                                && existing != &v
                            {
                                self.fail(format!(
                                    "Invariant (a) mixed-OS: path {k:?} has different content across members: {existing:?} vs {v:?}"
                                ));
                            }
                            ref_union.insert(k, v);
                        }
                    }
                    for &idx in &group_idxs {
                        let m = &self.members[idx];
                        let caps = m.engine.fs().capabilities();
                        let tree = m.engine.fs().live_tree(&m.ignore);
                        // Count equivalence-class multiplicities under this member's caps.
                        let mut equiv_counts: BTreeMap<String, usize> = BTreeMap::new();
                        for path in ref_union.keys() {
                            let ek = fs_equivalence_key(path, &caps);
                            *equiv_counts.entry(ek).or_insert(0) += 1;
                        }
                        for (path, expected_entry) in &ref_union {
                            let kind = match expected_entry {
                                TreeEntry::File(_) => EntryKind::File,
                                TreeEntry::Dir => EntryKind::Dir,
                                TreeEntry::Symlink(_) => EntryKind::Symlink,
                            };
                            let compat = check_fs_compatibility(path, kind, &caps).is_ok();
                            let ek = fs_equivalence_key(path, &caps);
                            let has_clash = equiv_counts.get(&ek).copied().unwrap_or(0) > 1;
                            if !compat {
                                if tree.contains_key(path) {
                                    self.fail(format!(
                                        "Invariant (a) mixed-OS: {} materialized incompatible path {path:?}",
                                        m.id
                                    ));
                                }
                            } else if !has_clash && tree.get(path) != Some(expected_entry) {
                                self.fail(format!(
                                    "Invariant (a) mixed-OS: {} missed or mismatched representable path {path:?}: got {:?}, expected {:?}",
                                    m.id,
                                    tree.get(path),
                                    expected_entry
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Invariant (b): FIXED — every protected active member MUST retain `hash` in ITS OWN live
        // tree OR ITS OWN unexpired bin (never excused by somebody else's bin entry!).
        let live_hashes_all: Vec<BTreeSet<Hash>> = self
            .members
            .iter()
            .map(|m| m.engine.fs().all_live_hashes())
            .collect();
        let bin_hashes_all: Vec<BTreeSet<Hash>> = self
            .members
            .iter()
            .map(|m| {
                if m.active {
                    m.engine.fs().all_bin_hashes()
                } else {
                    BTreeSet::new()
                }
            })
            .collect();
        let mut active_mask = 0u8;
        for (idx, m) in self.members.iter().enumerate() {
            if m.active {
                active_mask |= 1u8 << (idx & 7);
            }
        }

        for (hash, rec) in &self.contents {
            let protected_mask =
                active_mask & rec.ever_live_mask & !rec.person_removed_mask & !rec.bin_expired_mask;
            if protected_mask == 0 {
                continue;
            }

            if rec.person_removed_mask == 0 {
                let live_anywhere = live_hashes_all.iter().any(|set| set.contains(hash));
                if !live_anywhere {
                    self.fail(format!(
                        "Invariant (b): un-deleted content {hash} disappeared from every live tree (protected_mask: {protected_mask:#010b})"
                    ));
                }
            }

            for (idx, m) in self.members.iter().enumerate() {
                let bit = 1u8 << (idx & 7);
                if (protected_mask & bit) == 0 {
                    continue;
                }
                let on_this_member =
                    live_hashes_all[idx].contains(hash) || bin_hashes_all[idx].contains(hash);
                if !on_this_member {
                    self.fail(format!(
                        "Invariant (b): content {hash} disappeared from member {}'s OWN live tree and OWN unexpired bin (not excused by any other member!)",
                        m.id
                    ));
                }
            }
        }

        // Invariant (c): a universally deleted path NEVER comes back on any active member.
        for path in &self.universally_deleted {
            for m in &self.members {
                if !m.active || m.engine.fs().is_unreadable(path) {
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

    /// Runs a dense randomized schedule exercising edits, deletes, file & directory renames,
    /// file/dir swaps, mid-apply human edits, crashes (during apply AND during recovery),
    /// non-crash FS faults, store commit failures, mid-fetch peer drops, clock jumps in BOTH
    /// directions, bin restore, conflict resolution, partitions, joins, leaves, and symlinks.
    fn run_schedule(&mut self, steps: usize) {
        let init_files = 2 + self.rng.range(3);
        for _ in 0..init_files {
            self.global_secs += 1;
            let active = self.active_indices();
            let idx = active[self.rng.range(active.len())];
            let path = self.pick_path();
            self.user_create_or_edit_file(idx, &path, true);
        }

        for step in 0..steps {
            self.global_secs += self.rng.range_i64(1, 300);
            let active = self.active_indices();
            let roll = self.rng.range(100);

            match roll {
                // Create or edit file (sometimes unscanned).
                0..=18 => {
                    let idx = active[self.rng.range(active.len())];
                    let path = self.pick_path();
                    let scan = self.rng.chance(3, 4);
                    self.user_create_or_edit_file(idx, &path, scan);
                }
                // Delete an existing live path.
                19..=29 => {
                    let idx = active[self.rng.range(active.len())];
                    let paths = self.members[idx].engine.fs().live_paths();
                    if !paths.is_empty() {
                        let (p, _) = &paths[self.rng.range(paths.len())];
                        let scan = self.rng.chance(4, 5);
                        self.user_delete_path(idx, p, scan);
                    }
                }
                // Rename file OR rename directory (with concurrent edits possible on peers).
                30..=38 => {
                    let idx = active[self.rng.range(active.len())];
                    if self.rng.chance(1, 4) {
                        let (from_d, to_d) = if self.rng.chance(1, 2) {
                            ("box", "alt")
                        } else {
                            ("alt", "box")
                        };
                        self.user_rename_directory_with_edit(idx, from_d, to_d);
                    } else {
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
                            let live_all = self.members[idx].engine.fs().live_paths();
                            if from != &to
                                && !live_all
                                    .iter()
                                    .any(|(p, k)| p == &to && *k == EntryKind::Dir)
                                && !legion_sync::path::ancestors(&to).into_iter().any(|anc| {
                                    live_all
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
                                self.note_person_touched_path(&mid, from);
                                self.note_person_touched_path(&mid, &to);
                                self.clear_universally_deleted_for_path(&to);
                                self.scan_member(idx);
                                let from_owned = from.clone();
                                self.log(|| format!("{mid} rename {from_owned:?} -> {to:?}"));
                            }
                        }
                    }
                }
                // File <-> directory swap.
                39..=44 => {
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
                // Sync random reachable pair with crashes, non-crash FS faults, store commit
                // failures, mid-apply human edits, or mid-fetch peer drops!
                45..=74 => {
                    let dst = active[self.rng.range(active.len())];
                    let src = active[self.rng.range(active.len())];
                    let fault_roll = self.rng.range(20);
                    let (crash, non_crash_fs, store_fail, mid_edit, peer_fault) = match fault_roll {
                        0..=3 => (true, false, false, false, PeerFault::None),
                        4 => (false, true, false, false, PeerFault::None),
                        5 => (false, false, true, false, PeerFault::None),
                        6 => (false, false, false, true, PeerFault::None),
                        7 => (false, false, false, false, PeerFault::DropOnceResumable),
                        8 => (false, false, false, false, PeerFault::DropAlways),
                        _ => (false, false, false, false, PeerFault::None),
                    };
                    self.sync_pair_with_faults(
                        dst,
                        src,
                        crash,
                        non_crash_fs,
                        store_fail,
                        mid_edit,
                        peer_fault,
                    );
                }
                // Explicit scan.
                75..=77 => {
                    let idx = active[self.rng.range(active.len())];
                    self.scan_member(idx);
                }
                // Network partition change.
                78..=83 => match self.rng.range(3) {
                    0 => {
                        for &idx in &active {
                            self.members[idx].group = self.rng.range(2);
                        }
                        self.log(|| "partition 2-way".to_string());
                    }
                    1 => {
                        let iso = active[self.rng.range(active.len())];
                        for &idx in &active {
                            self.members[idx].group = if idx == iso { 1 } else { 0 };
                        }
                        let iso_id = self.members[iso].id.clone();
                        self.log(|| format!("partition isolate {iso_id}"));
                    }
                    _ => {
                        for &idx in &active {
                            self.members[idx].group = 0;
                        }
                        self.log(|| "partition heal".to_string());
                    }
                },
                // Clock jump: forward (10..40d) OR backward (-2..40d, exercising implausible clock!).
                84..=88 => {
                    if self.rng.chance(2, 3) {
                        let days = self.rng.range_i64(10, 40);
                        self.global_secs += days * 86_400;
                        self.log(|| format!("clock_jump +{days}d"));
                    } else {
                        let idx = active[self.rng.range(active.len())];
                        let jump = -self.rng.range_i64(3600, 45 * 86_400);
                        self.members[idx].clock_skew_secs += jump;
                        let mid = self.members[idx].id.clone();
                        self.log(|| format!("clock_jump {mid} {jump}s"));
                    }
                    self.expire_bins_online();
                }
                // Restore from bin as a schedule step!
                89..=91 => {
                    let idx = active[self.rng.range(active.len())];
                    self.step_restore_from_bin(idx);
                }
                // Resolve a live conflict as a schedule step!
                92..=94 => {
                    let idx = active[self.rng.range(active.len())];
                    self.step_resolve_conflict(idx);
                }
                // Member joins with pre-existing files.
                95..=96 => {
                    self.add_new_member_with_existing_files();
                }
                // Member leaves (including while another member may be partitioned!).
                97 => {
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
                        self.clear_universally_deleted_for_path("sym_link");
                        self.scan_member(idx);
                        self.log(|| format!("{mid} symlink sym_link -> alpha.txt"));
                    }
                }
            }

            if step % 12 == 11 {
                self.check_invariants(false);
            }
        }

        // Phase A: connected-group convergence UNDER active partition.
        self.sync_connected_groups_to_quiescence();
        self.check_invariants(true);

        // Phase B: heal partitions (when any active member was partitioned) and converge globally.
        let mut any_partitioned = false;
        let first_group = self
            .members
            .iter()
            .find(|m| m.active)
            .map(|m| m.group)
            .unwrap_or(0);
        for m in &mut self.members {
            if m.active {
                if m.group != first_group {
                    any_partitioned = true;
                }
                m.group = 0;
            }
        }
        if any_partitioned {
            self.sync_connected_groups_to_quiescence();
            self.check_invariants(true);
        }

        // Phase C: capture all absent paths, jump +65d (past bin expiry), expire bins, re-sync,
        // and verify Invariant (c) holds after expiry!
        let active = self.active_indices();
        if let Some(&first) = active.first() {
            let live_now: BTreeSet<String> = self.members[first]
                .engine
                .fs()
                .live_tree(&self.members[first].ignore)
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

// ---------------------------------------------------------------------------
// Dedicated deep scenarios (run across millions of seeds AND standalone tests)
// ---------------------------------------------------------------------------

/// Scenario 0: 90-day long partition zombie test + direct-ack tombstone GC + backward clock jump.
fn run_long_partition_zombie_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "long_partition_zombie",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let active = world.active_indices();
    let sleeper = active[world.rng.range(active.len())];
    let online: Vec<usize> = active.iter().copied().filter(|&i| i != sleeper).collect();
    let editor = online[0];

    for path in ["z1.txt", "z2.txt", "dir/z3.txt", "dir/z4.txt"] {
        world.user_create_or_edit_file(editor, path, true);
    }
    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    world.members[sleeper].group = 1;
    for &i in &online {
        world.members[i].group = 0;
    }

    world.global_secs += 3600;
    world.user_delete_path(editor, "z1.txt", true);
    world.user_delete_path(editor, "dir/z3.txt", true);
    world.user_create_or_edit_file(editor, "z2.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Jump sleeper's clock backwards first (verifies clock-sanity guard prevents accidental bin
    // expiry), then advance global clock 90 days!
    world.members[sleeper].clock_skew_secs -= 200 * 86_400;
    world.global_secs += 90 * 86_400;
    world.expire_bins_online();
    world.sync_connected_groups_to_quiescence();

    let ts_online = world.members[editor]
        .engine
        .store()
        .list_tombstones()
        .unwrap();
    if ts_online.len() < 2 {
        world.fail(format!(
            "Invariant (c): tombstones GC'd while sleeper {} was still partitioned: {ts_online:?}",
            world.members[sleeper].id
        ));
    }

    world.members[sleeper].group = 0;
    world.sync_pair_with_faults(sleeper, editor, true, false, false, false, PeerFault::None);
    world.sync_pair_with_faults(editor, sleeper, true, false, false, false, PeerFault::None);
    world.sync_connected_groups_to_quiescence();

    for &i in &active {
        let tree = world.members[i]
            .engine
            .fs()
            .live_tree(&world.members[i].ignore);
        if tree.contains_key("z1.txt") || tree.contains_key("dir/z3.txt") {
            world.fail(format!(
                "Invariant (c): deleted file resurrected on {} after 90-day partition",
                world.members[i].id
            ));
        }
    }
    world.check_invariants(true);

    for &i in &active {
        let remaining = world.members[i].engine.store().list_tombstones().unwrap();
        if !remaining.is_empty() {
            world.fail(format!(
                "Direct-ack tombstone GC did not reclaim tombstones on {}: {remaining:?}",
                world.members[i].id
            ));
        }
    }
}

/// Scenario 1: Concurrent conflict + TWO separate same-day conflicts lost by the same member
/// (directly exercising the exact bug from the 6 review seeds!) + delete-vs-change + conflict
/// immutability (Invariant g) + identical conflict names everywhere (Invariant e).
fn run_conflict_and_delete_vs_edit_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "conflict_double_and_delete_vs_edit",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let active = world.active_indices();
    let a = active[0];
    let b = active[1];

    world.user_create_or_edit_file(a, "both.txt", true);
    world.user_create_or_edit_file(a, "del_vs_edit.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Round 1 conflict on `both.txt`.
    world.members[a].group = 1;
    world.members[b].group = 2;
    for &idx in &active[2..] {
        world.members[idx].group = 1 + world.rng.range(2);
    }

    world.global_secs += 60;
    world.user_create_or_edit_file(a, "both.txt", true);
    let h_a1 = world.members[a]
        .engine
        .fs()
        .live_file_hash("both.txt")
        .unwrap();
    world.global_secs += 60;
    world.user_create_or_edit_file(b, "both.txt", true);
    let h_b1 = world.members[b]
        .engine
        .fs()
        .live_file_hash("both.txt")
        .unwrap();

    world.user_delete_path(a, "del_vs_edit.txt", true);
    let scan_b = world.rng.chance(1, 2);
    world.user_create_or_edit_file(b, "del_vs_edit.txt", scan_b);
    let changed_hash = world.members[b]
        .engine
        .fs()
        .live_file_hash("del_vs_edit.txt")
        .unwrap();

    for &idx in &active {
        world.members[idx].group = 0;
    }
    world.sync_pair_with_faults(a, b, true, false, false, false, PeerFault::None);
    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    // Round 2 conflict on the SAME file `both.txt` on the SAME day! Both conflict copies MUST
    // coexist with distinct `<hash6>` names and identical names across all members!
    world.members[a].group = 1;
    world.members[b].group = 2;
    world.global_secs += 60;
    world.user_create_or_edit_file(a, "both.txt", true);
    let h_a2 = world.members[a]
        .engine
        .fs()
        .live_file_hash("both.txt")
        .unwrap();
    world.global_secs += 60;
    world.user_create_or_edit_file(b, "both.txt", true);
    let h_b2 = world.members[b]
        .engine
        .fs()
        .live_file_hash("both.txt")
        .unwrap();

    for &idx in &active {
        world.members[idx].group = 0;
    }
    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    // Verify Invariant (d): delete vs edit -> edit won everywhere.
    for &idx in &active {
        if world.members[idx]
            .engine
            .fs()
            .live_file_hash("del_vs_edit.txt")
            != Some(changed_hash)
        {
            world.fail(format!(
                "Invariant (d): del_vs_edit.txt lost on {}",
                world.members[idx].id
            ));
        }
    }

    // Verify Invariant (e) + (g): exact same 3 files for `both.txt` (current winner + 2 distinct
    // conflict copies) on EVERY member, containing both round-1 and round-2 losing hashes!
    let ref_both: BTreeMap<String, TreeEntry> = world.members[a]
        .engine
        .fs()
        .live_tree(&world.members[a].ignore)
        .into_iter()
        .filter(|(p, _)| p == "both.txt" || p.starts_with("both (changed on "))
        .collect();
    if ref_both.len() != 3 {
        world.fail(format!(
            "Invariant (e)/(g): expected 3 files after two same-day conflicts on both.txt, got {ref_both:?}"
        ));
    }
    let live_hashes: BTreeSet<Hash> = ref_both
        .values()
        .filter_map(|e| match e {
            TreeEntry::File(h) => Some(*h),
            _ => None,
        })
        .collect();
    // Both round-2 versions MUST be live, and whichever round-1 version lost MUST STILL be live!
    assert!(live_hashes.contains(&h_a2) && live_hashes.contains(&h_b2));
    assert!(live_hashes.contains(&h_a1) || live_hashes.contains(&h_b1));
    for &idx in &active[1..] {
        let member_both: BTreeMap<String, TreeEntry> = world.members[idx]
            .engine
            .fs()
            .live_tree(&world.members[idx].ignore)
            .into_iter()
            .filter(|(p, _)| p == "both.txt" || p.starts_with("both (changed on "))
            .collect();
        if member_both != ref_both {
            world.fail(format!(
                "Invariant (e): conflict files differ on {}: {member_both:?} vs {ref_both:?}",
                world.members[idx].id
            ));
        }
    }
}

/// Scenario 2: Mixed-OS heterogeneous group (Unix + macOS APFS case-insensitive/Unicode NFC-NFD +
/// Windows NTFS) exercising Windows reserved/forbidden/long names, symlinks, NFD/NFC clashes, and
/// case-only renames with full mixed-OS convergence check (Invariant a).
fn run_heterogeneous_and_case_rename_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "heterogeneous_and_case_rename",
        seed,
        true,
        Vec::new(),
        record_trace,
    );
    let unix_idx = 0;

    world.user_create_or_edit_file(unix_idx, "shared.txt", true);
    world.user_create_or_edit_file(unix_idx, "dir1/ok.txt", true);
    world.user_create_or_edit_file(unix_idx, "CON.txt", true);
    world.user_create_or_edit_file(unix_idx, "foo:bar.txt", true);
    world.user_create_or_edit_file(unix_idx, "Case.txt", true);
    world.user_create_or_edit_file(unix_idx, "case.txt", true);
    // macOS Unicode NFC vs NFD clash: "caf\u{00e9}.txt" vs "cafe\u{0301}.txt"
    world.user_create_or_edit_file(unix_idx, "caf\u{00e9}.txt", true);
    world.user_create_or_edit_file(unix_idx, "cafe\u{0301}.txt", true);
    // Over-long Windows path (> 240 bytes)
    let long_path = format!("deep/{}/{}/end.txt", "a".repeat(120), "b".repeat(120));
    world.user_create_or_edit_file(unix_idx, &long_path, true);
    let now = world.members[unix_idx].local_time(world.global_secs);
    world.members[unix_idx]
        .engine
        .fs()
        .user_symlink("unix_link", "shared.txt".to_string(), now);
    world.scan_member(unix_idx);

    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    // Now test a case-only rename on a non-clashing file ("rename_me.txt" -> "RENAME_ME.txt")!
    world.user_create_or_edit_file(unix_idx, "rename_me.txt", true);
    world.sync_connected_groups_to_quiescence();
    let expected_hash = world.members[unix_idx]
        .engine
        .fs()
        .live_file_hash("rename_me.txt")
        .unwrap();
    let now2 = world.members[unix_idx].local_time(world.global_secs + 10);
    world.members[unix_idx]
        .engine
        .fs()
        .user_rename_file("rename_me.txt", "RENAME_ME.txt", now2);
    world.scan_member(unix_idx);
    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);

    for m in &world.members {
        if !m.active {
            continue;
        }
        let tree = m.engine.fs().live_tree(&m.ignore);
        assert_eq!(
            tree.get("RENAME_ME.txt"),
            Some(&TreeEntry::File(expected_hash))
        );
        assert!(!tree.contains_key("rename_me.txt"));
        let caps = m.engine.fs().capabilities();
        if caps.windows_naming {
            assert!(!tree.contains_key("CON.txt"));
            assert!(!tree.contains_key("foo:bar.txt"));
            assert!(!tree.contains_key("unix_link"));
            assert!(!tree.contains_key(&long_path));
            assert!(matches!(
                m.engine.status().unwrap().state,
                MemberState::Problem { .. }
            ));
        } else if caps.unicode_normalizing {
            assert!(
                !(tree.contains_key("caf\u{00e9}.txt") && tree.contains_key("cafe\u{0301}.txt"))
            );
            assert!(matches!(
                m.engine.status().unwrap().state,
                MemberState::Problem { .. }
            ));
        }
    }
}

/// Scenario 3: Backup restore + reinstall with files kept + join with old copy (Rules C & D).
fn run_backup_reinstall_and_join_old_copy_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "backup_reinstall_join_old",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let a = 0;
    let b = 1;

    // v1 baseline synced to all members.
    world.user_create_or_edit_file(a, "doc.txt", true);
    world.user_create_or_edit_file(a, "gone.txt", true);
    world.user_create_or_edit_file(a, "keep.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Take backup snapshot of B at v1!
    let b_fs_snap = world.members[b].engine.fs().snapshot();
    let b_store_snap = world.members[b].engine.store().snapshot();
    let b_id = world.members[b].id.clone();
    let b_inc_before = world.members[b].engine.meta().unwrap().incarnation;

    // Advance to v2 on A, delete `gone.txt`, sync to B so B advances past backup point!
    world.global_secs += 300;
    world.user_create_or_edit_file(a, "doc.txt", true);
    let doc_v2 = world.members[a]
        .engine
        .fs()
        .live_file_hash("doc.txt")
        .unwrap();
    world.user_delete_path(a, "gone.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Restore B from its v1 backup snapshot! (Record post-backup hashes reverted on B so
    // Invariant (b) knows B itself was rolled back by the backup restore, not by sync.)
    let post_backup_hashes = world.members[b].engine.fs().all_live_hashes();
    let backup_hashes: BTreeSet<Hash> = b_fs_snap
        .live
        .values()
        .filter_map(|n| match n {
            SimNode::File { hash, .. } => Some(*hash),
            _ => None,
        })
        .collect();
    for h in post_backup_hashes.difference(&backup_hashes) {
        world.note_person_removed(&b_id, *h);
    }
    let all_names: Vec<String> = world.members.iter().map(|m| m.id.clone()).collect();
    let b_cfg = FolderConfig::new("sim", b_id.clone(), all_names.clone());
    let mut restored_engine = SyncEngine::open(
        b_cfg,
        HonestSimFs::from_snapshot(b_fs_snap),
        FaultyIndexStore::from_inner(b_store_snap),
    )
    .unwrap();
    restored_engine.set_deletion_brake(brake_disabled());
    world.members[b].engine = restored_engine;

    // Person creates a brand-new file and edits `keep.txt` on B AFTER restoring from backup!
    world.global_secs += 120;
    world.user_create_or_edit_file(b, "after_restore.txt", true);
    let after_restore_hash = world.members[b]
        .engine
        .fs()
        .live_file_hash("after_restore.txt")
        .unwrap();
    world.user_create_or_edit_file(b, "keep.txt", true);
    let keep_v2 = world.members[b]
        .engine
        .fs()
        .live_file_hash("keep.txt")
        .unwrap();

    // Reconnect: test BOTH directions! First let A pull from B (if B's seq happens to match A's
    // previous cursor with a diverging fingerprint, A cleanly refuses the rolled-back incarnation
    // until B rotates!), then let B pull from A (B detects rollback via direct ack, rotates to a
    // brand-new incarnation, applies Rule D), then full convergence!
    {
        let now_a = world.members[a].local_time(world.global_secs);
        let (left, right) = world.members.split_at_mut(b);
        let peer_b = SimPeer::new(&right[0].engine, &left[a].id, &right[0].id, PeerFault::None);
        let _ = left[a].engine.pull_from_peer(&peer_b, now_a);
    }
    {
        let now_b = world.members[b].local_time(world.global_secs);
        let (left, right) = world.members.split_at_mut(b);
        let peer_a = SimPeer::new(&left[a].engine, &right[0].id, &left[a].id, PeerFault::None);
        right[0].engine.pull_from_peer(&peer_a, now_b).unwrap();
    }
    world.sync_connected_groups_to_quiescence();

    assert_ne!(
        world.members[b].engine.meta().unwrap().incarnation,
        b_inc_before,
        "backup restore must rotate incarnation"
    );
    for m in &world.members {
        if !m.active {
            continue;
        }
        assert_eq!(m.engine.fs().live_file_hash("doc.txt"), Some(doc_v2));
        assert_eq!(m.engine.fs().live_file_hash("gone.txt"), None);
        assert_eq!(
            m.engine.fs().live_file_hash("after_restore.txt"),
            Some(after_restore_hash),
            "after_restore.txt missing on {} (b={b_id}, rec={:?}, b_rec={:?}, cursors={:?})",
            m.id,
            m.engine.store().get_record("after_restore.txt").unwrap(),
            world.members[b]
                .engine
                .store()
                .get_record("after_restore.txt")
                .unwrap(),
            m.engine.store().load_peer_cursors().unwrap(),
        );
        // Under Rule D, `keep_v2` is unknown new content created after restore while `keep.txt`
        // exists on peers (`keep_v1`), so BOTH versions are preserved (either `keep_v2` wins
        // `keep.txt` or is kept as its conflict copy depending on clock skew) — never lost!
        assert!(
            m.engine.fs().all_live_hashes().contains(&keep_v2),
            "post-restore edit keep_v2 must remain live on {}",
            m.id
        );
    }

    // Reinstall B with its live files kept (wipe store + marker, reopen, delete `after_restore.txt`,
    // sync -> deletion must propagate!).
    let kept_snap = world.members[b].engine.fs().snapshot();
    let reinstalled_fs = HonestSimFs::from_snapshot(kept_snap);
    *reinstalled_fs.marker.borrow_mut() = None;
    let b_cfg2 = FolderConfig::new("sim", b_id.clone(), all_names);
    let mut reinstalled =
        SyncEngine::open(b_cfg2, reinstalled_fs, FaultyIndexStore::new()).unwrap();
    reinstalled.set_deletion_brake(brake_disabled());
    world.members[b].engine = reinstalled;
    world.scan_member(b);
    world.sync_connected_groups_to_quiescence();
    world.user_delete_path(b, "after_restore.txt", true);
    world.sync_connected_groups_to_quiescence();

    for m in &world.members {
        if !m.active {
            continue;
        }
        assert_eq!(m.engine.fs().live_file_hash("after_restore.txt"), None);
    }
    world.check_invariants(true);
}

/// Scenario 4: Cloned machine / duplicate member ID (same member ID, alternating incarnations) is
/// detected and refused cleanly rather than syncing corrupted history (Item C).
fn run_duplicate_member_id_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "duplicate_member_id",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let a = 0;
    let b = 1;

    world.user_create_or_edit_file(a, "base.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Clone B into an independent second engine with the SAME member id "m1" but a fresh
    // incarnation (simulating two machines configured with the same member ID!).
    let names: Vec<String> = world.members.iter().map(|m| m.id.clone()).collect();
    let clone_fs = HonestSimFs::new(FsCapabilities::unix(), true);
    let clone_store = FaultyIndexStore::new();
    let mut clone_b = SyncEngine::open(
        FolderConfig::new("sim", world.members[b].id.clone(), names),
        clone_fs,
        clone_store,
    )
    .unwrap();
    let now = world.members[a].local_time(world.global_secs + 60);
    let (bytes, hash) = world.fresh_bytes();
    clone_b
        .fs()
        .user_write_file("clone_only.txt", bytes, hash, now);
    clone_b.scan(now).unwrap();

    // A first pulls from clone_b (incarnation 2 -> cursor switches to inc 2), then pulls from
    // original B (incarnation 1 -> alternation detected -> DuplicateMemberId refused!).
    let a_id = world.members[a].id.clone();
    let b_id = world.members[b].id.clone();
    let peer_clone = SimPeer::new(&clone_b, &a_id, &b_id, PeerFault::None);
    let _ = world.members[a].engine.pull_from_peer(&peer_clone, now);

    let (left, right) = world.members.split_at_mut(b);
    let peer_orig = SimPeer::new(&right[0].engine, &left[a].id, &right[0].id, PeerFault::None);
    let err = left[a]
        .engine
        .pull_from_peer(&peer_orig, now)
        .expect_err("alternating incarnations for same member ID must be refused");
    assert!(matches!(err, SyncError::Conflict { .. }));
    let status = left[a].engine.status().unwrap();
    assert!(
        matches!(
            status.state,
            MemberState::Problem { ref problems, .. }
                if problems.iter().any(|p| p.kind == ProblemKind::DuplicateMemberId)
        ),
        "DuplicateMemberId problem must be reported, got {:?}",
        status.state
    );
}

/// Scenario 5: Member offline through membership change + member removed and re-added with an old
/// copy AFTER tombstone GC (verifies `deleted_history` prevents resurrection!).
fn run_membership_change_and_readd_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "membership_change_and_readd",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    // Ensure at least 3 members for membership change while one is offline.
    while world.active_indices().len() < 3 {
        world.add_new_member_with_existing_files();
    }
    let a = 0;
    let b = 1;
    let c = 2;

    world.user_create_or_edit_file(a, "old_deleted.txt", true);
    world.user_create_or_edit_file(a, "stay.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Snapshot C's disk while `old_deleted.txt` is still live.
    let c_old_fs = world.members[c].engine.fs().snapshot();
    let c_id = world.members[c].id.clone();

    // Partition B offline while C is removed, `old_deleted.txt` is deleted, and tombstones GC!
    world.members[b].group = 1;
    world.members[a].group = 0;
    world.remove_member_by_idx(c);
    world.user_delete_path(a, "old_deleted.txt", true);

    // B reconnects after membership change (C was removed while B was partitioned).
    world.members[b].group = 0;
    world.sync_connected_groups_to_quiescence();

    // Tombstone for `old_deleted.txt` is now GC'd across all active members!
    for m in &world.members {
        if m.active {
            assert!(
                m.engine.store().list_tombstones().unwrap().is_empty(),
                "tombstone should have GC'd after active members synced"
            );
        }
    }

    // Re-add C with its OLD disk copy (still containing `old_deleted.txt`) and a brand-new store!
    let active_names: Vec<String> = world
        .members
        .iter()
        .filter(|m| m.active)
        .map(|m| m.id.clone())
        .collect();
    for m in &mut world.members {
        if m.active {
            m.engine.add_member(c_id.clone()).unwrap();
        }
    }
    let mut new_names = active_names;
    new_names.push(c_id.clone());
    let readded_fs = HonestSimFs::from_snapshot(c_old_fs);
    *readded_fs.marker.borrow_mut() = None;
    let mut readded = SyncEngine::open(
        FolderConfig::new("sim", c_id.clone(), new_names),
        readded_fs,
        FaultyIndexStore::new(),
    )
    .unwrap();
    readded.set_deletion_brake(brake_disabled());
    world.members[c].engine = readded;
    world.members[c].active = true;
    world.members[c].group = 0;
    world.scan_member(c);
    world.sync_connected_groups_to_quiescence();

    // `old_deleted.txt` MUST NOT resurrect anywhere (binned on C via `deleted_history`)!
    for m in &world.members {
        if m.active {
            assert_eq!(
                m.engine.fs().live_file_hash("old_deleted.txt"),
                None,
                "re-added member resurrected deleted file after tombstone GC on {}",
                m.id
            );
        }
    }
    world.check_invariants(true);
}

/// Scenario 6: Hostile peer sending bad paths (`.legion-sync` NFKC, `C:x`, `../escape`), unknown
/// `modified_by`, oversize stream, bogus VV -> skipped & reported without stalling honest records
/// or causing premature tombstone GC!
fn run_hostile_peer_seed(root_seed: u64, seed: u64, record_trace: bool) {
    struct HostilePeer {
        records: Vec<IndexRecord>,
        good_payload: Vec<u8>,
    }
    impl Peer for HostilePeer {
        fn member_id(&self) -> &str {
            "m1"
        }
        fn handshake(&self) -> legion_sync::Result<PeerHandshake> {
            Ok(PeerHandshake {
                folder_id: "sim".into(),
                member_id: "m1".into(),
                incarnation: 99,
                snapshot_seq: self.records.len() as u64,
                protocol_version: SYNC_PROTOCOL_VERSION,
            })
        }
        fn records_since(
            &self,
            since_incarnation: u64,
            since_seq: u64,
            limit: usize,
            _cancel: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            let start = if since_incarnation == 99 {
                since_seq as usize
            } else {
                0
            };
            let end = (start + limit).min(self.records.len());
            Ok(RecordsPage {
                incarnation: 99,
                records: self.records[start..end].to_vec(),
                snapshot_seq: self.records.len() as u64,
                has_more: end < self.records.len(),
                caller_acked: None,
                caller_acked_fingerprint: None,
                head_fingerprint: self.records.last().map(IndexRecord::head_fingerprint),
            })
        }
        fn fetch_file(
            &self,
            path: &str,
            _expected_hash: Hash,
            options: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            if path == "oversize.bin" {
                // Streams way more bytes than declared size; engine must cap at declared + 1!
                return Ok(Box::new(Cursor::new(vec![0xFFu8; 4096])));
            }
            let start = (options.offset as usize).min(self.good_payload.len());
            Ok(Box::new(Cursor::new(self.good_payload[start..].to_vec())))
        }
    }

    let mut world = SimWorld::new(
        root_seed,
        "hostile_peer",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let now = world.members[0].local_time(world.global_secs);
    let (good_bytes, good_hash) = world.fresh_bytes();
    let mk_rec = |seq: u64, path: &str, modified_by: &str, size: u64, hash: Hash| IndexRecord {
        path: path.into(),
        kind: EntryKind::File,
        size,
        mtime: now,
        disk_mtime: None,
        disk_change_id: None,
        mode: Some(0o644),
        hash: Some(hash),
        link_target: None,
        deleted: false,
        version: VersionVector::singleton(VersionKey::new("m1", 99), seq),
        modified_by: modified_by.into(),
        seq,
        recent_hashes: Vec::new(),
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    };

    let peer = HostilePeer {
        records: vec![
            mk_rec(1, ".LEGION-SYNC/evil.txt", "m1", 4, good_hash),
            mk_rec(2, "\u{FF0E}legion-sync/nfkc.txt", "m1", 4, good_hash),
            mk_rec(3, "C:drive_rel.txt", "m1", 4, good_hash),
            mk_rec(4, "forged.txt", "not_a_member", 4, good_hash),
            mk_rec(5, "oversize.bin", "m1", 8, good_hash),
            mk_rec(
                6,
                "honest_after_bad.txt",
                "m1",
                good_bytes.len() as u64,
                good_hash,
            ),
        ],
        good_payload: good_bytes,
    };

    let rep = world.members[0].engine.pull_from_peer(&peer, now).unwrap();
    assert!(
        rep.invalid_skipped >= 4,
        "bad records must be skipped and counted"
    );
    assert_eq!(
        world.members[0]
            .engine
            .fs()
            .live_file_hash("honest_after_bad.txt"),
        Some(good_hash),
        "honest record after bad records must still apply"
    );
    assert!(!world.members[0].engine.fs().root_escaped());
    assert!(world.members[0].engine.fs().list_tmp().unwrap().is_empty());
}

/// Scenario 7: Unreadable directory (Invariant i), empty mount point (Item B2), OS litter &
/// custom-ignored files inside deleted folders (Item H), and directory rename with concurrent
/// edit inside!
fn run_unreadable_mount_and_ignored_dir_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "unreadable_mount_ignored_dir",
        seed,
        false,
        vec!["*.log".into()],
        record_trace,
    );
    let a = 0;
    let b = 1;

    world.user_create_or_edit_file(a, "secret/a.txt", true);
    world.user_create_or_edit_file(a, "litter_dir/f.txt", true);
    world.user_create_or_edit_file(a, "custom_dir/f.txt", true);
    world.user_create_or_edit_file(a, "folder/x.txt", true);
    world.sync_connected_groups_to_quiescence();

    // 1. Unreadable directory on A -> zero deletions recorded (Invariant i)!
    world.members[a].engine.fs().set_unreadable("secret", true);
    world.scan_member(a);
    assert!(
        world.members[a]
            .engine
            .store()
            .get_record("secret/a.txt")
            .unwrap()
            .is_some_and(|r| !r.deleted)
    );
    world.members[a].engine.fs().set_unreadable("secret", false);

    // 2. OS litter (`.DS_Store`) in `litter_dir` + custom-ignored (`debug.log`) in `custom_dir` on B!
    let now_b = world.members[b].local_time(world.global_secs);
    let (litter_b, litter_h) = world.fresh_bytes();
    world.members[b]
        .engine
        .fs()
        .user_write_file("litter_dir/.DS_Store", litter_b, litter_h, now_b);
    let (log_b, log_h) = world.fresh_bytes();
    world.members[b]
        .engine
        .fs()
        .user_write_file("custom_dir/debug.log", log_b, log_h, now_b);

    // Delete both directories on A and sync to B!
    world.user_delete_path(a, "litter_dir", true);
    world.user_delete_path(a, "custom_dir", true);
    world.sync_connected_groups_to_quiescence();

    // `litter_dir` MUST be removed on B (`.DS_Store` went to bin!), while `custom_dir` MUST stay
    // with `debug.log` intact AND reported once without reviving on subsequent scan!
    assert!(
        world.members[b]
            .engine
            .fs()
            .stat_live("litter_dir")
            .unwrap()
            .is_none(),
        "deleted dir with only OS litter must be removed"
    );
    assert!(
        world.members[b]
            .engine
            .fs()
            .stat_live("custom_dir/debug.log")
            .unwrap()
            .is_some(),
        "custom-ignored file must stay intact"
    );
    assert!(
        matches!(
            world.members[b].engine.status().unwrap().state,
            MemberState::Problem { ref problems, .. }
                if problems.iter().any(|p| p.kind == ProblemKind::DirectoryKeptLocalFiles)
        ),
        "kept dir must be reported"
    );
    // Rescan on B MUST NOT revive `custom_dir` (even after tombstone GC removed its tombstone)!
    world.scan_member(b);
    assert!(
        world.members[b]
            .engine
            .store()
            .get_record("custom_dir")
            .unwrap()
            .is_none_or(|r| r.deleted),
        "rescan must not revive kept deleted dir custom_dir even after tombstone GC"
    );

    // 3. Directory rename on A (`folder` -> `renamed_folder`) concurrent with edit inside
    // `folder/x.txt` on B -> B's edit MUST NOT be lost!
    world.members[a].group = 1;
    world.members[b].group = 2;
    world.user_rename_directory_with_edit(a, "folder", "renamed_folder");
    world.user_create_or_edit_file(b, "folder/x.txt", true);
    let edited_x = world.members[b]
        .engine
        .fs()
        .live_file_hash("folder/x.txt")
        .unwrap();
    world.members[a].group = 0;
    world.members[b].group = 0;
    world.sync_connected_groups_to_quiescence();
    for m in &world.members {
        if m.active {
            assert_eq!(
                m.engine.fs().live_file_hash("folder/x.txt"),
                Some(edited_x),
                "concurrent edit inside renamed dir must survive (delete vs change -> change wins)"
            );
        }
    }

    // 4. Empty mount point on A (marker + live files vanish) -> stops with FolderUnavailable,
    // records ZERO deletions!
    world.members[a].engine.fs().clear_mount_point();
    let now_a = world.members[a].local_time(world.global_secs + 60);
    let err = world.members[a]
        .engine
        .scan(now_a)
        .expect_err("empty mount point with non-empty index must stop");
    assert!(matches!(err, SyncError::FolderUnavailable { .. }));
    assert!(
        world.members[a]
            .engine
            .store()
            .get_record("secret/a.txt")
            .unwrap()
            .is_some_and(|r| !r.deleted),
        "empty mount point must record zero deletions"
    );
}

/// Scenario 8: Crash DURING `recover()`, store commit failures, mid-apply human edits on BOTH
/// atomic-exchange and claim-then-rename members, and serving peer failing mid-fetch!
fn run_crash_recovery_mid_apply_and_peer_fail_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "crash_recovery_mid_apply_peer_fail",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let a = 0;
    let b = 1;

    world.user_create_or_edit_file(a, "target.txt", true);
    world.user_create_or_edit_file(a, "second.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Update on A, then sync to B with:
    // (1) mid-fetch drop once (resumable),
    // (2) mid-fetch drop always (fails cleanly without stuck pending_op),
    // (3) mid-apply human edit right before swap/claim on B,
    // (4) store commit failure,
    // (5) crash + crash during recovery!
    world.global_secs += 60;
    world.user_create_or_edit_file(a, "target.txt", true);
    world.user_create_or_edit_file(a, "second.txt", true);

    world.sync_pair_with_faults(b, a, false, false, false, false, PeerFault::DropAlways);
    world.sync_pair_with_faults(
        b,
        a,
        false,
        false,
        false,
        false,
        PeerFault::UnavailableOnFetch,
    );
    world.sync_pair_with_faults(
        b,
        a,
        false,
        false,
        false,
        true,
        PeerFault::DropOnceResumable,
    );

    world.global_secs += 60;
    world.user_create_or_edit_file(a, "target.txt", true);
    world.sync_pair_with_faults(b, a, false, false, true, false, PeerFault::None);
    world.sync_pair_with_faults(b, a, true, false, false, false, PeerFault::None);

    world.sync_connected_groups_to_quiescence();
    world.check_invariants(true);
}

fn run_one_seed(seed: u64, record_trace: bool) {
    // Every seed runs the dense randomized schedule (all faults, mid-apply human edits, bin
    // restore, conflict resolution, clock jumps both ways, directory renames, deep trees).
    let mut world = SimWorld::new(
        seed,
        "random_schedule",
        seed,
        seed.is_multiple_of(8),
        Vec::new(),
        record_trace,
    );
    world.run_schedule(32);

    // Every seed also hits a dedicated deep scenario on a clean modulo cadence so across 50M
    // seeds EACH scenario runs >3,100,000 independent times!
    match seed % 16 {
        0 => run_long_partition_zombie_seed(seed, seed ^ 0x5555_5555, record_trace),
        1 | 9 => run_conflict_and_delete_vs_edit_seed(seed, seed ^ 0xAAAA_AAAA, record_trace),
        2 => run_heterogeneous_and_case_rename_seed(seed, seed ^ 0x3333_3333, record_trace),
        3 => run_backup_reinstall_and_join_old_copy_seed(seed, seed ^ 0x7777_7777, record_trace),
        4 => run_duplicate_member_id_seed(seed, seed ^ 0x9999_9999, record_trace),
        5 => run_membership_change_and_readd_seed(seed, seed ^ 0xBBBB_BBBB, record_trace),
        6 => run_hostile_peer_seed(seed, seed ^ 0xCCCC_CCCC, record_trace),
        7 => run_unreadable_mount_and_ignored_dir_seed(seed, seed ^ 0xDDDD_DDDD, record_trace),
        8 => {
            run_crash_recovery_mid_apply_and_peer_fail_seed(seed, seed ^ 0xEEEE_EEEE, record_trace)
        }
        _ => {}
    }
}

fn run_seed_range_parallel(start_seed: u64, count: u64) -> (std::time::Duration, f64) {
    let started = Instant::now();
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(count.max(1) as usize);

    if threads <= 1 {
        for offset in 0..count {
            run_one_seed(start_seed + offset, false);
        }
    } else {
        std::thread::scope(|scope| {
            let chunk = count.div_ceil(threads as u64);
            let mut handles = Vec::with_capacity(threads);
            for t in 0..threads {
                let s = start_seed + (t as u64) * chunk;
                let e = (s + chunk).min(start_seed + count);
                if s >= e {
                    continue;
                }
                handles.push(scope.spawn(move || {
                    for seed in s..e {
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

/// Every seed that ever uncovered a bug stays here forever, including the six seeds from the
/// 5,000,000-seed review run (257640, 859170, 1381582, 1845682, 2777393, 4395930).
const REGRESSION_SEEDS: &[u64] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 13, 42, 55, 78, 95, 97, 99, 128, 196, 249, 302, 316, 444, 759,
    951, 1137, 1225, 1326, 1337, 1346, 1435, 1451, 1476, 1532, 1576, 1617, 2026, 11338, 19883,
    25021, 31444, 35879, 48566, 56498, 60211, 85172, 89687, 90830, 94963, 257640, 338474, 612202,
    834066, 859170, 1234029, 1276023, 1381582, 1442864, 1631700, 1735939, 1845682, 1905590,
    2132430, 2242736, 2469631, 2508764, 2770489, 2777393, 2838112, 2972224, 3137180, 3286448,
    3660488, 3804523, 3954961, 4066064, 4319726, 4377839, 4395930, 4628071, 4717156, 6102439,
    13635160, 17469556, 18877611, 37147542,
];

#[test]
fn simulator_regression_seeds() {
    for &seed in REGRESSION_SEEDS {
        run_one_seed(seed, true);
    }
}

#[test]
fn simulator_all_dedicated_scenarios() {
    for s in 0..16u64 {
        run_long_partition_zombie_seed(s, s ^ 0x5555_5555, true);
        run_conflict_and_delete_vs_edit_seed(s, s ^ 0xAAAA_AAAA, true);
        run_heterogeneous_and_case_rename_seed(s, s ^ 0x3333_3333, true);
        run_backup_reinstall_and_join_old_copy_seed(s, s ^ 0x7777_7777, true);
        run_duplicate_member_id_seed(s, s ^ 0x9999_9999, true);
        run_membership_change_and_readd_seed(s, s ^ 0xBBBB_BBBB, true);
        run_hostile_peer_seed(s, s ^ 0xCCCC_CCCC, true);
        run_unreadable_mount_and_ignored_dir_seed(s, s ^ 0xDDDD_DDDD, true);
        run_crash_recovery_mid_apply_and_peer_fail_seed(s, s ^ 0xEEEE_EEEE, true);
    }
}

#[test]
fn simulator_normal_suite() {
    let start: u64 = std::env::var("LEGION_SYNC_SEED_START")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let count: u64 = std::env::var("LEGION_SYNC_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let (elapsed, rate) = run_seed_range_parallel(start, count);
    eprintln!(
        "simulator_normal_suite: {count} seeds ({start}..{}) passed in {:.2}s ({:.0} seeds/sec)",
        start + count,
        elapsed.as_secs_f64(),
        rate
    );
}

#[test]
fn replay_seed() {
    let seed: u64 = std::env::var("LEGION_SYNC_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_845_682);
    run_one_seed(seed, true);
}

#[test]
#[ignore = "long simulation mode; run in --release with LEGION_SYNC_SEED_START and LEGION_SYNC_SEEDS"]
fn simulator_long_mode() {
    let start: u64 = std::env::var("LEGION_SYNC_SEED_START")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let count: u64 = std::env::var("LEGION_SYNC_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let (elapsed, rate) = run_seed_range_parallel(start, count);
    println!(
        "simulator_long_mode: {count} seeds ({start}..{}) passed in {:.2}s ({:.0} seeds/sec)",
        start + count,
        elapsed.as_secs_f64(),
        rate
    );
}
