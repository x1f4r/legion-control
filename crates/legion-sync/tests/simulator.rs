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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use legion_sync::path::{
    check_fs_compatibility, fs_equivalence_key, is_descendant_of, parse_conflict_copy_path,
    validate_flat_internal_name, validate_staged_tmp_token,
};
use legion_sync::{
    BinEntry, ConflictChoice, DeletionBrakeConfig, DeletionBrakeDecision,
    FORCED_ROTATION_COOLDOWN_SECS, FetchClock, FetchOptions, FolderConfig, FolderMarker,
    FolderMeta, FsCapabilities, FsErrorKind, FsMeta, FsWalkEntry, FsWalkResult, HeldBackRecord,
    INDEX_DAMAGED_PROBLEM_SENTENCE, IgnoreMatcher, InboundDeletionBrakeDecision, IndexRecord,
    IndexStore, MAX_FORCED_ROTATIONS_BEFORE_CONFIRM, MAX_VERSION_COUNTER,
    MIN_MIGRATABLE_SCHEMA_VERSION, MemberId, MemberState, MemoryIndexStore,
    MemoryInstanceTokenStore, PageLimit, Peer, PeerAckView, PeerCursor, PeerHandshake,
    PeerRollbackClaimDecision, PendingOp, ProblemKind, RecordProvenance, RecordsPage,
    RestoreStrategy, SYNC_PROTOCOL_VERSION, ScanOptions, StoreBatch, SyncEngine, SyncError, SyncFs,
    SyncWrite, VersionKey, VersionVector, is_internal_sync_path, validate_sync_path,
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

    fn meta(&self, windows_naming: bool) -> FsMeta {
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
                file_identity: if windows_naming {
                    None
                } else {
                    Some((1, *gen_id))
                },
            },
            SimNode::Dir { mtime } => FsMeta {
                kind: EntryKind::Dir,
                size: 0,
                mtime: *mtime,
                mode: Some(0o755),
                link_target: None,
                change_id: None,
                file_identity: None,
            },
            SimNode::Symlink { target, mtime } => FsMeta {
                kind: EntryKind::Symlink,
                size: 0,
                mtime: *mtime,
                mode: Some(0o777),
                link_target: Some(target.clone()),
                change_id: None,
                file_identity: None,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct SimBinData {
    bytes: Vec<u8>,
    hash: Option<Hash>,
    mtime: Timestamp,
    gen_id: u64,
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

/// Human action fired right before or after an FS mutation inside an active apply!
#[derive(Debug, Clone)]
enum MidApplyOp {
    EditOrCreateFile {
        path: String,
        bytes: Vec<u8>,
        hash: Hash,
        mtime: Timestamp,
    },
    /// Same-size, same-mtime overwrite (models `r3` where cached stat metadata looks unchanged).
    SameSizeInPlaceSave {
        path: String,
        bytes: Vec<u8>,
        hash: Hash,
    },
    /// Moves a live file to another path mid-apply (models `r2`).
    RenameFile {
        from: String,
        to: String,
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
    secondary_path: Option<String>,
}

#[derive(Debug, Clone)]
struct MidApplyHook {
    countdown: usize,
    after_op: bool,
    op_filter: Option<&'static str>,
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

/// Shared tmp-write durability tracking for [`HonestSimFs`] power-loss mode.
#[derive(Default)]
struct PowerLossTmpState {
    synced_tmp_data: BTreeSet<String>,
    last_unsynced_tmp: Option<(String, Vec<u8>)>,
    pending_power_loss: bool,
}

/// Unsynced filesystem operation awaiting its enclosing directory's `fsync`, allowing
/// [`HonestSimFs::simulate_power_loss`] to model partial persistence (an independent random subset
/// of unsynced operations surviving a power cut while others revert).
#[derive(Debug, Clone)]
enum UnsyncedFsOp {
    MoveTmpToLive { tmp_name: String, live_path: String },
    CreateDir { path: String },
    RemoveTmp { tmp_name: String },
}

/// Durable-vs-volatile disk state used when [`HonestSimFs`] runs in power-loss mode.
///
/// Mirrors `RealFs`'s exact `fsync` / `sync_dir` calls:
/// - File data written through [`SyncSimWriter`] is volatile until `finish_sync()` completes; a
///   power loss before `finish_sync()` either drops the file or leaves a torn partial write
///   (truncated prefix or zero-filled tail).
/// - Directory entry mutations (creates, renames, unlinks, deferred group-commit renames, mkdirs)
///   remain volatile until the affected parent directory is synced.
/// - On power loss, any live entry whose strict ancestor directory was never synced into its own
///   parent vanishes along with the unsynced ancestor directory.
struct PowerLossDisk {
    durable_marker: Option<FolderMarker>,
    durable_live: BTreeMap<String, SimNode>,
    durable_tmp: BTreeMap<String, Vec<u8>>,
    durable_tmp_meta: TmpMetaMap,
    durable_bin: BTreeMap<String, SimBinData>,
    tmp_state: Arc<Mutex<PowerLossTmpState>>,
    sync_created_ancestors: bool,
    sync_remove_tmp: bool,
    unsynced_ops: Vec<UnsyncedFsOp>,
    tear_rng: Rng,
    power_losses: usize,
}

/// Send-safe writer staging into the in-memory `.legion-sync/tmp` map on `finish_sync`.
struct SyncSimWriter {
    buf: Vec<u8>,
    name: String,
    target_tmp: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    fail_on_sync: Option<FsErrorKind>,
    pl_tmp: Option<Arc<Mutex<PowerLossTmpState>>>,
}

impl Write for SyncSimWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        if let Some(ref pl) = self.pl_tmp {
            // Visible in volatile page cache while writing, but NOT yet fsync'd!
            self.target_tmp
                .lock()
                .unwrap()
                .insert(self.name.clone(), self.buf.clone());
            pl.lock().unwrap().last_unsynced_tmp = Some((self.name.clone(), self.buf.clone()));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SyncWrite for SyncSimWriter {
    fn finish_sync(self: Box<Self>) -> legion_sync::Result<()> {
        if let Some(kind) = self.fail_on_sync {
            if kind == FsErrorKind::Crash
                && let Some(ref pl) = self.pl_tmp
            {
                let mut g = pl.lock().unwrap();
                // Leave `last_unsynced_tmp` populated so power loss drops or tears it!
                g.last_unsynced_tmp = Some((self.name.clone(), self.buf.clone()));
                g.pending_power_loss = true;
            }
            return Err(HonestSimFs::fault_err("finish_sync", kind));
        }
        let mut map = self.target_tmp.lock().unwrap();
        map.insert(self.name.clone(), self.buf);
        drop(map);
        if let Some(ref pl) = self.pl_tmp {
            let mut g = pl.lock().unwrap();
            g.synced_tmp_data.insert(self.name.clone());
            if g.last_unsynced_tmp
                .as_ref()
                .is_some_and(|(n, _)| n == &self.name)
            {
                g.last_unsynced_tmp = None;
            }
        }
        Ok(())
    }
}

/// Honest in-memory filesystem enforcing platform case-insensitivity, macOS Unicode NFC/NFD
/// normalization, Windows naming/length/symlink rules, unreadable subtrees, OS litter & ignored
/// files, non-replacing renames & optional atomic swaps, crash/non-crash fault injection,
/// power-loss durability simulation, and mid-apply human edit hooks.
type TmpMetaMap = BTreeMap<String, (Timestamp, Option<u32>, u64)>;

struct HonestSimFs {
    caps: FsCapabilities,
    supports_exchange: bool,
    marker: RefCell<Option<FolderMarker>>,
    live: RefCell<BTreeMap<String, SimNode>>,
    tmp: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    tmp_meta: RefCell<TmpMetaMap>,
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
    mid_apply: RefCell<Vec<MidApplyHook>>,
    has_mid_apply: Cell<bool>,
    mid_apply_fired: RefCell<Vec<MidApplyFired>>,
    free_space_budget: Cell<Option<u64>>,
    separate_mount_prefix: RefCell<Option<String>>,
    power_loss_enabled: Cell<bool>,
    power_loss: RefCell<Option<PowerLossDisk>>,
}

impl HonestSimFs {
    fn new(caps: FsCapabilities, supports_exchange: bool) -> Self {
        Self {
            caps,
            supports_exchange,
            marker: RefCell::new(None),
            live: RefCell::new(BTreeMap::new()),
            tmp: Arc::new(Mutex::new(BTreeMap::new())),
            tmp_meta: RefCell::new(BTreeMap::new()),
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
            mid_apply: RefCell::new(Vec::new()),
            has_mid_apply: Cell::new(false),
            mid_apply_fired: RefCell::new(Vec::new()),
            free_space_budget: Cell::new(None),
            separate_mount_prefix: RefCell::new(None),
            power_loss_enabled: Cell::new(false),
            power_loss: RefCell::new(None),
        }
    }

    fn enable_power_loss_mode(
        &self,
        seed: u64,
        sync_created_ancestors: bool,
        sync_remove_tmp: bool,
    ) {
        let mut tmp_state = PowerLossTmpState::default();
        let tmp_snap = self.tmp.lock().unwrap().clone();
        for k in tmp_snap.keys() {
            tmp_state.synced_tmp_data.insert(k.clone());
        }
        *self.power_loss.borrow_mut() = Some(PowerLossDisk {
            durable_marker: self.marker.borrow().clone(),
            durable_live: self.live.borrow().clone(),
            durable_tmp: tmp_snap,
            durable_tmp_meta: self.tmp_meta.borrow().clone(),
            durable_bin: self.bin.borrow().clone(),
            tmp_state: Arc::new(Mutex::new(tmp_state)),
            sync_created_ancestors,
            sync_remove_tmp,
            unsynced_ops: Vec::new(),
            tear_rng: Rng::new(seed ^ 0xD15C_1055),
            power_losses: 0,
        });
        self.power_loss_enabled.set(true);
    }

    fn power_loss_count(&self) -> usize {
        self.power_loss
            .borrow()
            .as_ref()
            .map_or(0, |pl| pl.power_losses)
    }

    fn pl_tmp_state(&self) -> Option<Arc<Mutex<PowerLossTmpState>>> {
        if !self.power_loss_enabled.get() {
            return None;
        }
        self.power_loss
            .borrow()
            .as_ref()
            .map(|pl| Arc::clone(&pl.tmp_state))
    }

    fn pl_flush_all_durable(&self) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let mut guard = self.power_loss.borrow_mut();
        if let Some(pl) = guard.as_mut() {
            pl.durable_marker = self.marker.borrow().clone();
            pl.durable_live = self.live.borrow().clone();
            pl.durable_bin = self.bin.borrow().clone();
        }
    }

    fn pl_flush_live_dir(&self, dir: &str) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let mut guard = self.power_loss.borrow_mut();
        let Some(pl) = guard.as_mut() else {
            return;
        };
        let live = self.live.borrow();
        let is_direct_child = |k: &str| {
            let p = k.rsplit_once('/').map_or("", |(parent, _)| parent);
            p == dir
        };
        pl.durable_live
            .retain(|k, _| !is_direct_child(k) || live.contains_key(k));
        for (k, v) in live.iter() {
            if is_direct_child(k) {
                pl.durable_live.insert(k.clone(), v.clone());
            }
        }
        pl.unsynced_ops.retain(|op| match op {
            UnsyncedFsOp::CreateDir { path } => !is_direct_child(path),
            UnsyncedFsOp::MoveTmpToLive { live_path, .. } => !is_direct_child(live_path),
            UnsyncedFsOp::RemoveTmp { .. } => true,
        });
    }

    fn pl_flush_live_parent(&self, clean: &str) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let parent = clean.rsplit_once('/').map_or("", |(p, _)| p);
        self.pl_flush_live_dir(parent);
    }

    fn pl_flush_created_ancestors(&self, created: &[String]) {
        if !self.power_loss_enabled.get() || created.is_empty() {
            return;
        }
        let should_sync = self
            .power_loss
            .borrow()
            .as_ref()
            .is_some_and(|pl| pl.sync_created_ancestors);
        if should_sync {
            for anc in created {
                self.pl_flush_live_parent(anc);
            }
        } else if let Some(pl) = self.power_loss.borrow_mut().as_mut() {
            for anc in created {
                pl.unsynced_ops
                    .push(UnsyncedFsOp::CreateDir { path: anc.clone() });
            }
        }
    }

    fn pl_record_unsynced_op(&self, op: UnsyncedFsOp) {
        if !self.power_loss_enabled.get() {
            return;
        }
        if let Some(pl) = self.power_loss.borrow_mut().as_mut() {
            pl.unsynced_ops.push(op);
        }
    }

    fn pl_flush_tmp_dir(&self) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let mut guard = self.power_loss.borrow_mut();
        let Some(pl) = guard.as_mut() else {
            return;
        };
        let tmp = self.tmp.lock().unwrap();
        let tmp_meta = self.tmp_meta.borrow();
        let tmp_state = pl.tmp_state.lock().unwrap();
        pl.durable_tmp.retain(|k, _| tmp.contains_key(k));
        pl.durable_tmp_meta.retain(|k, _| tmp.contains_key(k));
        for (k, v) in tmp.iter() {
            if tmp_state.synced_tmp_data.contains(k) {
                pl.durable_tmp.insert(k.clone(), v.clone());
                if let Some(m) = tmp_meta.get(k) {
                    pl.durable_tmp_meta.insert(k.clone(), *m);
                }
            }
        }
        pl.unsynced_ops.retain(|op| match op {
            UnsyncedFsOp::RemoveTmp { .. } => false,
            UnsyncedFsOp::MoveTmpToLive { tmp_name, .. } => tmp.contains_key(tmp_name),
            UnsyncedFsOp::CreateDir { .. } => true,
        });
    }

    fn pl_flush_bin_dir(&self) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let mut guard = self.power_loss.borrow_mut();
        if let Some(pl) = guard.as_mut() {
            pl.durable_bin = self.bin.borrow().clone();
        }
    }

    fn pl_mark_tmp_data_synced(&self, name: &str) {
        if !self.power_loss_enabled.get() {
            return;
        }
        if let Some(pl) = self.power_loss.borrow().as_ref() {
            pl.tmp_state
                .lock()
                .unwrap()
                .synced_tmp_data
                .insert(name.to_string());
        }
    }

    fn pl_is_tmp_data_synced(&self, name: &str) -> bool {
        if !self.power_loss_enabled.get() {
            return true;
        }
        self.power_loss
            .borrow()
            .as_ref()
            .is_none_or(|pl| pl.tmp_state.lock().unwrap().synced_tmp_data.contains(name))
    }

    fn apply_pending_power_loss(&self) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let pending = self
            .power_loss
            .borrow()
            .as_ref()
            .is_some_and(|pl| pl.tmp_state.lock().unwrap().pending_power_loss);
        if pending {
            self.simulate_power_loss();
        }
    }

    /// Cuts power abruptly with **partial persistence**: each unsynced directory operation
    /// independently persists or reverts at random (modeling out-of-order page-cache flushes
    /// rather than only all-or-nothing rollback), drops any live subtree whose newly created
    /// ancestor directory did not persist, and leaves the last unsynced tmp write dropped or torn
    /// (truncated prefix or zero-filled tail).
    fn simulate_power_loss(&self) {
        if !self.power_loss_enabled.get() {
            return;
        }
        let mut guard = self.power_loss.borrow_mut();
        let Some(pl) = guard.as_mut() else {
            return;
        };

        // Partial persistence: independently decide for each unsynced operation whether it made it
        // to media before power died or was lost in volatile cache.
        let volatile_live = self.live.borrow().clone();
        let pending_ops = std::mem::take(&mut pl.unsynced_ops);
        for op in pending_ops {
            if !pl.tear_rng.chance(1, 2) {
                continue;
            }
            match op {
                UnsyncedFsOp::CreateDir { path } => {
                    if let Some(node @ SimNode::Dir { .. }) = volatile_live.get(&path) {
                        pl.durable_live.insert(path, node.clone());
                    }
                }
                UnsyncedFsOp::MoveTmpToLive {
                    tmp_name,
                    live_path,
                } => {
                    if let Some(node) = volatile_live.get(&live_path) {
                        pl.durable_live.insert(live_path, node.clone());
                        pl.durable_tmp.remove(&tmp_name);
                        pl.durable_tmp_meta.remove(&tmp_name);
                    }
                }
                UnsyncedFsOp::RemoveTmp { tmp_name } => {
                    pl.durable_tmp.remove(&tmp_name);
                    pl.durable_tmp_meta.remove(&tmp_name);
                }
            }
        }

        // Drop any durable live entry whose strict ancestor directory is NOT durable.
        let durable_dirs: BTreeSet<String> = pl
            .durable_live
            .iter()
            .filter_map(|(k, v)| matches!(v, SimNode::Dir { .. }).then_some(k.clone()))
            .collect();
        pl.durable_live.retain(|path, _| {
            legion_sync::path::ancestors(path)
                .into_iter()
                .all(|anc| durable_dirs.contains(anc))
        });

        // Restore volatile views from durable state.
        *self.marker.borrow_mut() = pl.durable_marker.clone();
        *self.live.borrow_mut() = pl.durable_live.clone();
        *self.bin.borrow_mut() = pl.durable_bin.clone();

        let mut restored_tmp = pl.durable_tmp.clone();
        let mut restored_tmp_meta = pl.durable_tmp_meta.clone();

        // Model the last unsynced write: either lost completely or torn (truncated / zero-tail).
        let unsynced = {
            let mut ts = pl.tmp_state.lock().unwrap();
            ts.pending_power_loss = false;
            ts.synced_tmp_data.clear();
            for k in restored_tmp.keys() {
                ts.synced_tmp_data.insert(k.clone());
            }
            ts.last_unsynced_tmp.take()
        };
        if let Some((name, full_bytes)) = unsynced {
            if !full_bytes.is_empty() && pl.tear_rng.chance(2, 3) {
                let cut = if full_bytes.len() == 1 {
                    0
                } else {
                    1 + pl.tear_rng.range(full_bytes.len() - 1)
                };
                let torn = if pl.tear_rng.chance(1, 2) {
                    full_bytes[..cut].to_vec()
                } else {
                    let mut z = vec![0u8; full_bytes.len()];
                    z[..cut].copy_from_slice(&full_bytes[..cut]);
                    z
                };
                restored_tmp.insert(name.clone(), torn);
                restored_tmp_meta.remove(&name);
                pl.tmp_state.lock().unwrap().synced_tmp_data.insert(name);
            } else {
                restored_tmp.remove(&name);
                restored_tmp_meta.remove(&name);
            }
        }

        // Persist post-reboot state as the new durable baseline.
        pl.durable_tmp = restored_tmp.clone();
        pl.durable_tmp_meta = restored_tmp_meta.clone();
        *self.tmp.lock().unwrap() = restored_tmp;
        *self.tmp_meta.borrow_mut() = restored_tmp_meta;
        pl.power_losses += 1;
    }

    fn set_free_space(&self, bytes: Option<u64>) {
        self.free_space_budget.set(bytes);
    }

    fn set_separate_mount(&self, prefix: Option<&str>) {
        *self.separate_mount_prefix.borrow_mut() = prefix.map(str::to_string);
    }

    fn user_modify_bin_file(&self, storage_name: &str, bytes: Vec<u8>, mtime: Timestamp) -> Hash {
        let h = hash_bytes(&bytes);
        let gen_id = self.alloc_gen();
        let mut bin = self.bin.borrow_mut();
        bin.insert(
            storage_name.to_string(),
            SimBinData {
                bytes,
                hash: Some(h),
                mtime,
                gen_id,
            },
        );
        drop(bin);
        self.pl_flush_bin_dir();
        h
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
            tmp_meta: RefCell::new(BTreeMap::new()),
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
            mid_apply: RefCell::new(Vec::new()),
            has_mid_apply: Cell::new(false),
            mid_apply_fired: RefCell::new(Vec::new()),
            free_space_budget: Cell::new(None),
            separate_mount_prefix: RefCell::new(None),
            power_loss_enabled: Cell::new(false),
            power_loss: RefCell::new(None),
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

    fn arm_mid_apply(
        &self,
        countdown: usize,
        after_op: bool,
        op_filter: Option<&'static str>,
        op: MidApplyOp,
    ) {
        self.mid_apply.borrow_mut().push(MidApplyHook {
            countdown,
            after_op,
            op_filter,
            op,
        });
        self.has_mid_apply.set(true);
    }

    fn take_mid_apply_fired(&self) -> Vec<MidApplyFired> {
        if !self.has_mid_apply.get() && self.mid_apply_fired.borrow().is_empty() {
            return Vec::new();
        }
        self.mid_apply.borrow_mut().clear();
        self.has_mid_apply.set(false);
        std::mem::take(&mut *self.mid_apply_fired.borrow_mut())
    }

    fn maybe_fire_mid_apply(&self, op_name: &str, is_after: bool) {
        if !self.has_mid_apply.get() {
            return;
        }
        let mut to_run: Vec<MidApplyOp> = Vec::new();
        {
            let mut hooks = self.mid_apply.borrow_mut();
            let mut keep = Vec::with_capacity(hooks.len());
            for mut hook in hooks.drain(..) {
                let matches =
                    hook.after_op == is_after && hook.op_filter.is_none_or(|f| f == op_name);
                if matches {
                    if hook.countdown == 0 {
                        to_run.push(hook.op);
                    } else {
                        hook.countdown -= 1;
                        keep.push(hook);
                    }
                } else {
                    keep.push(hook);
                }
            }
            *hooks = keep;
            if hooks.is_empty() {
                self.has_mid_apply.set(false);
            }
        }
        for op in to_run {
            let fired = match op {
                MidApplyOp::EditOrCreateFile {
                    path,
                    bytes,
                    hash,
                    mtime,
                } => {
                    let mut removed = Vec::new();
                    let mut live = self.live.borrow_mut();
                    let blocked = Self::has_ancestor_conflict(&live, &path)
                        || matches!(live.get(&path), Some(SimNode::Dir { .. }));
                    if blocked {
                        continue;
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
                        secondary_path: None,
                    }
                }
                MidApplyOp::SameSizeInPlaceSave { path, bytes, hash } => {
                    let mut removed = Vec::new();
                    let mut live = self.live.borrow_mut();
                    let Some(SimNode::File {
                        bytes: old_b,
                        hash: old_h,
                        mtime,
                        mode,
                        gen_id,
                    }) = live.get_mut(&path)
                    else {
                        continue;
                    };
                    removed.push(*old_h);
                    // Preserve mtime and size (models coarse-clock same-size save right after
                    // placement), while bumping `gen_id` (real FS ctime/inode change).
                    let _ = (*mtime, *mode);
                    *old_b = bytes;
                    *old_h = hash;
                    *gen_id = self.alloc_gen();
                    MidApplyFired {
                        created_hash: Some(hash),
                        removed_hashes: removed,
                        touched_path: path,
                        secondary_path: None,
                    }
                }
                MidApplyOp::RenameFile { from, to } => {
                    let mut live = self.live.borrow_mut();
                    let blocked = Self::has_ancestor_conflict(&live, &to)
                        || matches!(live.get(&to), Some(SimNode::Dir { .. }));
                    if blocked || !matches!(live.get(&from), Some(SimNode::File { .. })) {
                        continue;
                    }
                    let mut removed = Vec::new();
                    if let Some(SimNode::File { hash: old_to_h, .. }) = live.get(&to) {
                        removed.push(*old_to_h);
                    }
                    let node = live.remove(&from).unwrap();
                    for anc in legion_sync::path::ancestors(&to) {
                        live.entry(anc.to_string()).or_insert(SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        });
                    }
                    live.insert(to.clone(), node);
                    MidApplyFired {
                        created_hash: None,
                        removed_hashes: removed,
                        touched_path: from,
                        secondary_path: Some(to),
                    }
                }
                MidApplyOp::DeletePath { path } => {
                    let removed = self.user_remove_recursive(&path);
                    MidApplyFired {
                        created_hash: None,
                        removed_hashes: removed,
                        touched_path: path,
                        secondary_path: None,
                    }
                }
            };
            self.mid_apply_fired.borrow_mut().push(fired);
            self.pl_flush_all_durable();
        }
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
            self.maybe_fire_mid_apply(op, false);
        }
        let (before, after) = self.tick_fault();
        if let Some(kind) = before {
            if kind == FsErrorKind::Crash {
                self.simulate_power_loss();
            }
            return Err(Self::fault_err(op, kind));
        }
        let res = f()?;
        if allow_mid_apply {
            self.maybe_fire_mid_apply(op, true);
        }
        if let Some(kind) = after {
            if kind == FsErrorKind::Crash {
                self.simulate_power_loss();
            }
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
        drop(live);
        self.pl_flush_all_durable();
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
        drop(live);
        self.pl_flush_all_durable();
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
        drop(live);
        self.pl_flush_all_durable();
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
        drop(live);
        self.pl_flush_all_durable();
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
        drop(live);
        self.pl_flush_all_durable();
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
        drop(live);
        self.pl_flush_all_durable();
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
        if self.power_loss_enabled.get()
            && let Some(pl) = self.power_loss.borrow_mut().as_mut()
        {
            pl.durable_marker = Some(marker.clone());
        }
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
            let meta = node.meta(self.caps.windows_naming);
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
            .map(|n| n.meta(self.caps.windows_naming)))
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
            let mut created_ancs = Vec::new();
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
                    created_ancs.push(anc.to_string());
                }
            }
            if let Some(existing_key) = self.resolve_existing_key(&live, &clean) {
                match live.get(&existing_key) {
                    Some(SimNode::Dir { .. }) => {
                        drop(live);
                        self.pl_flush_created_ancestors(&created_ancs);
                        Ok(())
                    }
                    _ => Err(SyncError::Conflict {
                        path: clean,
                        reason: "non-directory exists".to_string(),
                    }),
                }
            } else {
                live.insert(
                    clean.clone(),
                    SimNode::Dir {
                        mtime: Timestamp::new(0, 0),
                    },
                );
                drop(live);
                self.pl_flush_created_ancestors(&created_ancs);
                self.pl_flush_live_parent(&clean);
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
                Some(SimNode::Dir { .. }) => {
                    drop(live);
                    self.pl_flush_live_parent(&actual);
                    Ok(())
                }
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
            let mut created_ancs = Vec::new();
            for anc in legion_sync::path::ancestors(&clean) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                    created_ancs.push(anc.to_string());
                }
            }
            live.insert(
                clean.clone(),
                SimNode::Symlink {
                    target: target.to_string(),
                    mtime: Timestamp::new(0, 0),
                },
            );
            drop(live);
            self.pl_flush_created_ancestors(&created_ancs);
            self.pl_flush_live_parent(&clean);
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
                Some(SimNode::Symlink { .. }) => {
                    drop(live);
                    self.pl_flush_live_parent(&actual);
                    Ok(())
                }
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
            let mut created_ancs = Vec::new();
            for anc in legion_sync::path::ancestors(&clean_to) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                    created_ancs.push(anc.to_string());
                }
            }
            live.insert(clean_to.clone(), node);
            drop(live);
            self.pl_flush_created_ancestors(&created_ancs);
            self.pl_flush_live_parent(&clean_to);
            self.pl_flush_live_parent(&actual_from);
            Ok(())
        })
    }

    fn sync_parent_dir_live(&self, live_rel: &str) -> legion_sync::Result<()> {
        let clean = validate_sync_path(live_rel)?;
        self.pl_flush_live_parent(&clean);
        Ok(())
    }

    fn sync_tmp_dir(&self) -> legion_sync::Result<()> {
        self.pl_flush_tmp_dir();
        Ok(())
    }

    fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        validate_staged_tmp_token(tmp_name)?;
        let (before, after) = self.tick_fault();
        if let Some(kind) = before {
            if kind == FsErrorKind::Crash {
                self.simulate_power_loss();
            }
            return Err(Self::fault_err("create_tmp", kind));
        }
        Ok(Box::new(SyncSimWriter {
            buf: Vec::new(),
            name: tmp_name.to_string(),
            target_tmp: self.tmp.clone(),
            fail_on_sync: after,
            pl_tmp: self.pl_tmp_state(),
        }))
    }

    fn open_tmp_append(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        validate_staged_tmp_token(tmp_name)?;
        let (before, after) = self.tick_fault();
        if let Some(kind) = before {
            if kind == FsErrorKind::Crash {
                self.simulate_power_loss();
            }
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
            pl_tmp: self.pl_tmp_state(),
        }))
    }

    fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        validate_staged_tmp_token(tmp_name)?;
        let tmp = self.tmp.lock().unwrap();
        let tm = self.tmp_meta.borrow().get(tmp_name).copied();
        Ok(tmp.get(tmp_name).map(|b| {
            let (mtime, mode, gen_id) = tm.unwrap_or((Timestamp::new(0, 0), Some(0o600), 0));
            FsMeta {
                kind: EntryKind::File,
                size: b.len() as u64,
                mtime,
                mode,
                link_target: None,
                change_id: (gen_id != 0).then_some(gen_id),
                file_identity: if self.caps.windows_naming || gen_id == 0 {
                    None
                } else {
                    Some((1, gen_id))
                },
            }
        }))
    }

    fn set_tmp_metadata(
        &self,
        tmp_name: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        validate_staged_tmp_token(tmp_name)?;
        let effective_mode = if self.caps.windows_naming {
            None
        } else {
            Some(mode.unwrap_or(0o644) & 0o777)
        };
        let gen_id = self
            .tmp_meta
            .borrow()
            .get(tmp_name)
            .map(|(_, _, g)| *g)
            .filter(|&g| g != 0)
            .unwrap_or_else(|| self.alloc_gen());
        self.tmp_meta
            .borrow_mut()
            .insert(tmp_name.to_string(), (mtime, effective_mode, gen_id));
        Ok(())
    }

    fn hash_tmp(&self, tmp_name: &str) -> legion_sync::Result<(Hash, u64)> {
        validate_staged_tmp_token(tmp_name)?;
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
        validate_staged_tmp_token(tmp_name)?;
        self.tmp.lock().unwrap().remove(tmp_name);
        self.tmp_meta.borrow_mut().remove(tmp_name);
        if self.power_loss_enabled.get() {
            let sync_rm = self
                .power_loss
                .borrow()
                .as_ref()
                .is_some_and(|pl| pl.sync_remove_tmp);
            if sync_rm {
                if let Some(pl) = self.power_loss.borrow().as_ref() {
                    let mut ts = pl.tmp_state.lock().unwrap();
                    ts.synced_tmp_data.remove(tmp_name);
                    if ts
                        .last_unsynced_tmp
                        .as_ref()
                        .is_some_and(|(n, _)| n == tmp_name)
                    {
                        ts.last_unsynced_tmp = None;
                    }
                }
                self.pl_flush_tmp_dir();
            } else {
                self.pl_record_unsynced_op(UnsyncedFsOp::RemoveTmp {
                    tmp_name: tmp_name.to_string(),
                });
            }
        }
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
        validate_staged_tmp_token(tmp_name)?;
        let clean = self.check_live_path(live_rel)?;
        self.guard_platform(&clean, EntryKind::File)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        self.wrap_mut("commit_tmp_to_live_no_replace", true, || {
            assert!(
                self.pl_is_tmp_data_synced(tmp_name),
                "power-loss bug: commit_tmp_to_live_no_replace called on unsynced tmp '{tmp_name}' for '{clean}'"
            );
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
            let (mtime, mode, gen_id) =
                self.tmp_meta
                    .borrow_mut()
                    .remove(tmp_name)
                    .unwrap_or_else(|| {
                        (
                            Timestamp::new(0, 0),
                            if self.caps.windows_naming {
                                None
                            } else {
                                Some(0o644)
                            },
                            self.alloc_gen(),
                        )
                    });
            let hash = hash_bytes(&bytes);
            let mut created_ancs = Vec::new();
            for anc in legion_sync::path::ancestors(&clean) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                    created_ancs.push(anc.to_string());
                }
            }
            live.insert(
                clean.clone(),
                SimNode::File {
                    bytes,
                    hash,
                    mtime,
                    mode,
                    gen_id,
                },
            );
            drop(live);
            self.pl_flush_created_ancestors(&created_ancs);
            self.pl_flush_live_parent(&clean);
            self.pl_flush_tmp_dir();
            Ok(())
        })
    }

    fn commit_tmp_to_live_no_replace_deferred_dir_sync(
        &self,
        tmp_name: &str,
        live_rel: &str,
    ) -> legion_sync::Result<()> {
        validate_staged_tmp_token(tmp_name)?;
        let clean = self.check_live_path(live_rel)?;
        self.guard_platform(&clean, EntryKind::File)?;
        if self.is_unreadable(&clean) {
            return Err(Self::fault_err(&clean, FsErrorKind::PermissionDenied));
        }
        self.wrap_mut(
            "commit_tmp_to_live_no_replace_deferred_dir_sync",
            true,
            || {
                assert!(
                    self.pl_is_tmp_data_synced(tmp_name),
                    "power-loss bug: deferred commit called on unsynced tmp '{tmp_name}' for '{clean}'"
                );
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
                let (mtime, mode, gen_id) =
                    self.tmp_meta
                        .borrow_mut()
                        .remove(tmp_name)
                        .unwrap_or_else(|| {
                            (
                                Timestamp::new(0, 0),
                                if self.caps.windows_naming {
                                    None
                                } else {
                                    Some(0o644)
                                },
                                self.alloc_gen(),
                            )
                        });
                let hash = hash_bytes(&bytes);
                let mut created_ancs = Vec::new();
                for anc in legion_sync::path::ancestors(&clean) {
                    if self.resolve_existing_key(&live, anc).is_none() {
                        live.insert(
                            anc.to_string(),
                            SimNode::Dir {
                                mtime: Timestamp::new(0, 0),
                            },
                        );
                        created_ancs.push(anc.to_string());
                    }
                }
                live.insert(
                    clean.clone(),
                    SimNode::File {
                        bytes,
                        hash,
                        mtime,
                        mode,
                        gen_id,
                    },
                );
                drop(live);
                // Intentionally defer live parent and tmp dir syncs until `sync_parent_dir_live` /
                // `sync_tmp_dir` — matching `RealFs::commit_tmp_to_live_no_replace_deferred_dir_sync`!
                self.pl_flush_created_ancestors(&created_ancs);
                self.pl_record_unsynced_op(UnsyncedFsOp::MoveTmpToLive {
                    tmp_name: tmp_name.to_string(),
                    live_path: clean,
                });
                Ok(())
            },
        )
    }

    fn exchange_tmp_with_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<bool> {
        validate_staged_tmp_token(tmp_name)?;
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
            let (new_mtime, new_mode, new_gen) = self
                .tmp_meta
                .borrow_mut()
                .remove(tmp_name)
                .unwrap_or_else(|| {
                    (
                        Timestamp::new(0, 0),
                        if self.caps.windows_naming {
                            None
                        } else {
                            Some(0o644)
                        },
                        self.alloc_gen(),
                    )
                });
            let staged_hash = hash_bytes(&staged_bytes);
            let old_node = live.insert(
                actual,
                SimNode::File {
                    bytes: staged_bytes,
                    hash: staged_hash,
                    mtime: new_mtime,
                    mode: new_mode,
                    gen_id: new_gen,
                },
            );
            if let Some(SimNode::File {
                bytes: old_b,
                mtime: old_mt,
                mode: old_md,
                gen_id: old_g,
                ..
            }) = old_node
            {
                tmp.insert(tmp_name.to_string(), old_b);
                self.tmp_meta
                    .borrow_mut()
                    .insert(tmp_name.to_string(), (old_mt, old_md, old_g));
            }
            drop(tmp);
            drop(live);
            self.pl_mark_tmp_data_synced(tmp_name);
            self.pl_flush_live_parent(&clean);
            self.pl_flush_tmp_dir();
            Ok(true)
        })
    }

    fn move_live_to_tmp_no_replace(
        &self,
        live_rel: &str,
        tmp_name: &str,
    ) -> legion_sync::Result<()> {
        let clean = self.check_live_path(live_rel)?;
        validate_staged_tmp_token(tmp_name)?;
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
                Some(SimNode::File {
                    bytes,
                    mtime,
                    mode,
                    gen_id,
                    ..
                }) => {
                    tmp.insert(tmp_name.to_string(), bytes);
                    self.tmp_meta
                        .borrow_mut()
                        .insert(tmp_name.to_string(), (mtime, mode, gen_id));
                    drop(tmp);
                    drop(live);
                    self.pl_mark_tmp_data_synced(tmp_name);
                    self.pl_flush_tmp_dir();
                    self.pl_flush_live_parent(&actual);
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
        validate_staged_tmp_token(tmp_name)?;
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
            let (mtime, _, gen_id) = self.tmp_meta.borrow_mut().remove(tmp_name).unwrap_or((
                Timestamp::new(0, 0),
                Some(0o600),
                self.alloc_gen(),
            ));
            let hash = Some(hash_bytes(&bytes));
            bin.insert(
                bin_data_name.to_string(),
                SimBinData {
                    bytes,
                    hash,
                    mtime,
                    gen_id,
                },
            );
            drop(bin);
            drop(tmp);
            self.pl_flush_bin_dir();
            self.pl_flush_tmp_dir();
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
                Some(SimNode::File {
                    bytes,
                    hash,
                    mtime,
                    gen_id,
                    ..
                }) => {
                    bin.insert(
                        bin_data_name.to_string(),
                        SimBinData {
                            bytes,
                            hash: Some(hash),
                            mtime,
                            gen_id,
                        },
                    );
                    drop(bin);
                    drop(live);
                    self.pl_flush_bin_dir();
                    self.pl_flush_live_parent(&actual);
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
            let gen_id = self.alloc_gen();
            self.bin.borrow_mut().insert(
                meta_name.to_string(),
                SimBinData {
                    bytes: bytes.to_vec(),
                    hash: None,
                    mtime: Timestamp::new(0, 0),
                    gen_id,
                },
            );
            self.pl_flush_bin_dir();
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
            mtime: d.mtime,
            mode: Some(0o600),
            link_target: None,
            change_id: Some(d.gen_id),
            file_identity: None,
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
        validate_staged_tmp_token(tmp_name)?;
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
            self.pl_mark_tmp_data_synced(tmp_name);
            Ok(())
        })
    }

    fn move_bin_to_live_no_replace(
        &self,
        bin_data_name: &str,
        live_rel: &str,
    ) -> legion_sync::Result<()> {
        validate_flat_internal_name(bin_data_name)?;
        let clean = self.check_live_path(live_rel)?;
        self.guard_platform(&clean, EntryKind::File)?;
        self.wrap_mut("move_bin_to_live_no_replace", false, || {
            let mut live = self.live.borrow_mut();
            if self.resolve_existing_key(&live, &clean).is_some() {
                return Err(SyncError::Fs {
                    path: Some(clean),
                    detail: "destination already exists".to_string(),
                    kind: FsErrorKind::AlreadyExists,
                });
            }
            let data =
                self.bin
                    .borrow_mut()
                    .remove(bin_data_name)
                    .ok_or_else(|| SyncError::Fs {
                        path: Some(bin_data_name.to_string()),
                        detail: "bin file not found".to_string(),
                        kind: FsErrorKind::NotFound,
                    })?;
            let hash = data.hash.unwrap_or_else(|| hash_bytes(&data.bytes));
            let mut created_ancs = Vec::new();
            for anc in legion_sync::path::ancestors(&clean) {
                if self.resolve_existing_key(&live, anc).is_none() {
                    live.insert(
                        anc.to_string(),
                        SimNode::Dir {
                            mtime: Timestamp::new(0, 0),
                        },
                    );
                    created_ancs.push(anc.to_string());
                }
            }
            live.insert(
                clean.clone(),
                SimNode::File {
                    bytes: data.bytes,
                    hash,
                    mtime: data.mtime,
                    mode: if self.caps.windows_naming {
                        None
                    } else {
                        Some(0o644)
                    },
                    gen_id: self.alloc_gen(),
                },
            );
            drop(live);
            self.pl_flush_created_ancestors(&created_ancs);
            self.pl_flush_live_parent(&clean);
            self.pl_flush_bin_dir();
            Ok(())
        })
    }

    fn remove_bin_file(&self, bin_name: &str) -> legion_sync::Result<()> {
        validate_flat_internal_name(bin_name)?;
        self.bin.borrow_mut().remove(bin_name);
        self.pl_flush_bin_dir();
        Ok(())
    }

    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        Ok(self.bin.borrow().keys().cloned().collect())
    }

    fn available_space_for(&self, _rel_path: &str) -> legion_sync::Result<Option<u64>> {
        Ok(self.free_space_budget.get())
    }

    fn needs_local_staging(&self, rel_path: &str) -> legion_sync::Result<bool> {
        let mount = self.separate_mount_prefix.borrow();
        Ok(mount
            .as_deref()
            .is_some_and(|prefix| rel_path == prefix || is_descendant_of(rel_path, prefix)))
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

    fn set_schema_version_for_test(&mut self, version: u32) {
        self.inner.set_schema_version_for_test(version);
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

    fn migrate_schema_in_place(&mut self) -> legion_sync::Result<bool> {
        self.inner.migrate_schema_in_place()
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
        limit: PageLimit,
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

    fn load_pending_op(&self) -> legion_sync::Result<Option<PendingOp>> {
        self.inner.load_pending_op()
    }

    fn list_staged_tmps(&self) -> legion_sync::Result<BTreeSet<String>> {
        self.inner.list_staged_tmps()
    }

    fn record_count(&self) -> legion_sync::Result<u64> {
        self.inner.record_count()
    }

    fn get_seq_fingerprint(&self, seq: u64) -> legion_sync::Result<Option<Hash>> {
        self.inner.get_seq_fingerprint(seq)
    }

    fn list_problems(&self) -> legion_sync::Result<Vec<(String, legion_sync::SyncProblem)>> {
        self.inner.list_problems()
    }

    fn list_deferrals(&self) -> legion_sync::Result<Vec<legion_sync::DeferredItem>> {
        self.inner.list_deferrals()
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

    fn handshake(
        &self,
        caller: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<PeerHandshake> {
        self.engine.serve_handshake(self.caller, caller)
    }

    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: PageLimit,
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

    fn report_hash_mismatch(&self, path: &str) {
        self.engine.note_serve_hash_mismatch(path);
    }
}

struct SimMember {
    id: String,
    engine: SyncEngine<HonestSimFs, FaultyIndexStore>,
    token_store: MemoryInstanceTokenStore,
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
    DeletionBrakeConfig::disabled()
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

        let power_loss_world = (seed % 4) == 1;
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
            if power_loss_world {
                fs.enable_power_loss_mode(seed ^ ((i as u64 + 1) * 0x9E37), true, true);
            }
            let store = FaultyIndexStore::new();
            let token_store = MemoryInstanceTokenStore::new();
            let mut engine = SyncEngine::open(cfg, fs, store, token_store.clone()).unwrap();
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
                token_store,
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
        if std::env::var("LEGION_SYNC_DEBUG").is_ok() {
            for m in &self.members {
                if !m.active {
                    continue;
                }
                eprintln!(
                    "DEBUG {} meta={:?}\n  tree={:?}\n  bin={:?}\n  held={:?}\n  probs={:?}",
                    m.id,
                    m.engine.meta().ok().map(|mt| (
                        mt.incarnation,
                        mt.local_seq,
                        mt.kept_deleted_dirs
                    )),
                    m.engine.fs().live_tree(&m.ignore),
                    m.engine.list_bin().ok(),
                    m.engine.store().list_held_back().ok(),
                    m.engine.store().list_problems().ok(),
                );
            }
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
        self.members[idx].engine.fs().apply_pending_power_loss();
        if crash_during_recovery {
            let op = self.rng.range(4);
            let after = self.rng.chance(1, 2);
            self.members[idx].engine.fs().arm_crash(op, after);
            let res = self.members[idx].engine.recover();
            let crashed = self.members[idx].engine.fs().crashed();
            self.members[idx].engine.fs().disarm_fault();
            self.members[idx].engine.fs().apply_pending_power_loss();
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
            let after_op = self.rng.chance(1, 2);
            let op = match self.rng.range(5) {
                0..=2 => {
                    let (bytes, hash) = self.fresh_bytes();
                    MidApplyOp::EditOrCreateFile {
                        path: target_path,
                        bytes,
                        hash,
                        mtime: Timestamp::new(now.secs + 1, 0),
                    }
                }
                3 => {
                    let to = self.pick_path();
                    if to != target_path {
                        MidApplyOp::RenameFile {
                            from: target_path,
                            to,
                        }
                    } else {
                        MidApplyOp::DeletePath { path: target_path }
                    }
                }
                _ => MidApplyOp::DeletePath { path: target_path },
            };
            self.members[dst_idx]
                .engine
                .fs()
                .arm_mid_apply(countdown, after_op, None, op);
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

        // Reconcile any mid-apply human edits into provenance tables BEFORE checking invariants!
        for fired in self.members[dst_idx].engine.fs().take_mid_apply_fired() {
            for &h in &fired.removed_hashes {
                self.note_person_removed(&dst_id, h);
            }
            if let Some(h) = fired.created_hash {
                self.note_created(&dst_id, h);
                self.clear_universally_deleted_for_path(&fired.touched_path);
            }
            self.note_person_touched_path(&dst_id, &fired.touched_path);
            if let Some(ref sec) = fired.secondary_path {
                self.note_person_touched_path(&dst_id, sec);
                self.clear_universally_deleted_for_path(sec);
            }
            self.log(|| {
                format!(
                    "{dst_id} MID_APPLY_HUMAN on {:?} created={:?} removed={:?} secondary={:?}",
                    fired.touched_path,
                    fired.created_hash.map(|h| h.to_hex()[..8].to_string()),
                    fired
                        .removed_hashes
                        .iter()
                        .map(|h| h.to_hex()[..8].to_string())
                        .collect::<Vec<_>>(),
                    fired.secondary_path
                )
            });
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
            Ok(rep) => {
                self.log(|| {
                    if non_crash_fs || rep.held_back > 0 || rep.deferred > 0 {
                        format!(
                            "sync {dst_id} <- {src_id} ok(applied={},held={},def={},non_crash_fs={non_crash_fs})",
                            rep.applied, rep.held_back, rep.deferred
                        )
                    } else {
                        format!("sync {dst_id} <- {src_id}")
                    }
                });
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

        // In power-loss mode, occasionally cut power right AFTER a sync returns (post-commit or
        // post-error cleanup) and verify recovery preserves every committed file and leaves no
        // ghost engine tmp files behind!
        if self.members[dst_idx].engine.fs().power_loss_enabled.get() && self.rng.chance(1, 5) {
            let crash_in_rec = self.rng.chance(1, 4);
            self.members[dst_idx].engine.fs().simulate_power_loss();
            self.recover_member(dst_idx, crash_in_rec);
            self.scan_member(dst_idx);
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
        let token_store = MemoryInstanceTokenStore::new();
        let mut engine = SyncEngine::open(cfg, fs, store, token_store.clone()).unwrap();
        engine.set_deletion_brake(brake_disabled());
        let skew = self.rng.range_i64(-2 * 86_400, 2 * 86_400);
        let new_idx = self.members.len();
        self.members.push(SimMember {
            id: new_id.clone(),
            engine,
            token_store,
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
                                        "\n  diff {k:?}:\n    {} tree={:?} rec={:?}\n    {} tree={:?} rec={:?}",
                                        ref_m.id,
                                        ref_tree.get(k),
                                        r0,
                                        other.id,
                                        other_tree.get(k),
                                        r1
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
                    let mut diag = String::new();
                    for m in &self.members {
                        let bins: Vec<_> = m
                            .engine
                            .list_bin()
                            .unwrap()
                            .into_iter()
                            .filter(|b| b.hash == Some(*hash))
                            .map(|b| {
                                format!("{}({:?} by {})", b.original_path, b.reason, b.by_member)
                            })
                            .collect();
                        let mut recs = Vec::new();
                        let _ = m.engine.store().scan_records_in_order(&mut |r| {
                            if r.hash == Some(*hash)
                                || r.base_hash == Some(*hash)
                                || r.recent_hashes.contains(hash)
                            {
                                recs.push(format!(
                                    "{}:del={}:prov={:?}:base={:?}:h={:?}",
                                    r.path, r.deleted, r.provenance, r.base_hash, r.hash
                                ));
                            }
                            Ok(true)
                        });
                        diag.push_str(&format!("\n  {}: bins={bins:?} recs={recs:?}", m.id));
                    }
                    self.fail(format!(
                        "Invariant (b): un-deleted content {hash} disappeared from every live tree (protected_mask: {protected_mask:#010b}){diag}"
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
        let paths: BTreeSet<&str> = remaining.iter().map(|r| r.path.as_str()).collect();
        if !paths.contains("z1.txt") || !paths.contains("dir/z3.txt") {
            world.fail(format!(
                "Permanent deletion tombstones missing z1.txt or dir/z3.txt on {}: {remaining:?}",
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
        world.members[b].token_store.clone(),
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
        "backup restore must rotate incarnation (b_meta={:?}, a_cursors={:?}, b_cursors={:?})",
        world.members[b].engine.meta().unwrap(),
        world.members[a].engine.store().load_peer_cursors().unwrap(),
        world.members[b].engine.store().load_peer_cursors().unwrap(),
    );
    for m in &world.members {
        if !m.active {
            continue;
        }
        assert_eq!(m.engine.fs().live_file_hash("doc.txt"), Some(doc_v2));
        assert_eq!(
            m.engine.fs().live_file_hash("gone.txt"),
            None,
            "gone.txt resurrected on {} (b={b_id}, m_rec={:?}, a_rec={:?}, b_rec={:?})",
            m.id,
            m.engine.store().get_record("gone.txt").unwrap(),
            world.members[a]
                .engine
                .store()
                .get_record("gone.txt")
                .unwrap(),
            world.members[b]
                .engine
                .store()
                .get_record("gone.txt")
                .unwrap(),
        );
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
    let b_tok2 = MemoryInstanceTokenStore::new();
    let mut reinstalled = SyncEngine::open(
        b_cfg2,
        reinstalled_fs,
        FaultyIndexStore::new(),
        b_tok2.clone(),
    )
    .unwrap();
    reinstalled.set_deletion_brake(brake_disabled());
    world.members[b].engine = reinstalled;
    world.members[b].token_store = b_tok2;
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
        MemoryInstanceTokenStore::new(),
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

    // Permanent tombstone for `old_deleted.txt` stays in every active member's index & deletion log!
    for m in &world.members {
        if m.active {
            assert!(
                m.engine
                    .store()
                    .get_record("old_deleted.txt")
                    .unwrap()
                    .is_some_and(|r| r.deleted),
                "permanent tombstone for old_deleted.txt must stay in index"
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
    let c_tok = MemoryInstanceTokenStore::new();
    let mut readded = SyncEngine::open(
        FolderConfig::new("sim", c_id.clone(), new_names),
        readded_fs,
        FaultyIndexStore::new(),
        c_tok.clone(),
    )
    .unwrap();
    readded.set_deletion_brake(brake_disabled());
    world.members[c].engine = readded;
    world.members[c].token_store = c_tok;
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
        fn handshake(
            &self,
            _caller: &legion_sync::HandshakeHello,
        ) -> legion_sync::Result<PeerHandshake> {
            Ok(PeerHandshake {
                folder_id: "sim".into(),
                member_id: "m1".into(),
                incarnation: 99,
                snapshot_seq: self.records.len() as u64,
                head_fingerprint: self.records.last().map(IndexRecord::head_fingerprint),
                protocol_version: SYNC_PROTOCOL_VERSION,
                caller_pulled: None,
                caller_acked: None,
                caller_pull_matches: None,
                caller_rejoining: false,
                refused_from_caller: None,
            })
        }
        fn records_since(
            &self,
            since_incarnation: u64,
            since_seq: u64,
            limit: PageLimit,
            _cancel: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            let start = if since_incarnation == 99 {
                since_seq as usize
            } else {
                0
            };
            let end = (start + limit.max_records).min(self.records.len());
            Ok(RecordsPage {
                incarnation: 99,
                records: self.records[start..end].to_vec(),
                snapshot_seq: self.records.len() as u64,
                has_more: end < self.records.len(),
                caller_acked: None,
                caller_acked_fingerprint: None,
                head_fingerprint: self.records.last().map(IndexRecord::head_fingerprint),
                chain_fingerprints: BTreeMap::new(),
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
        provenance: RecordProvenance::Versioned(VersionVector::singleton(
            VersionKey::new("m1", 99),
            seq,
        )),
        base_hash: None,
        modified_by: modified_by.into(),
        seq,
        recent_hashes: Vec::new(),
        conflict_of: None,
        conflict_dismissed: false,
    };

    let m0_meta = world.members[0].engine.meta().unwrap();
    let mut forged_own = mk_rec(4, "forged_own.txt", "m1", 4, good_hash);
    if let RecordProvenance::Versioned(ref mut vv) = forged_own.provenance {
        vv.set(
            VersionKey::new(&m0_meta.local_member, m0_meta.incarnation),
            u64::MAX,
        );
    }
    let mut huge_cnt = mk_rec(5, "huge_cnt.txt", "m1", 4, good_hash);
    if let RecordProvenance::Versioned(ref mut vv) = huge_cnt.provenance {
        vv.set(
            VersionKey::new("m1", 99),
            legion_sync::MAX_VERSION_COUNTER + 1,
        );
    }
    let peer = HostilePeer {
        records: vec![
            mk_rec(1, ".LEGION-SYNC/evil.txt", "m1", 4, good_hash),
            mk_rec(2, "\u{FF0E}legion-sync/nfkc.txt", "m1", 4, good_hash),
            mk_rec(3, "C:drive_rel.txt", "m1", 4, good_hash),
            forged_own,
            huge_cnt,
            mk_rec(6, "unknown_member.txt", "not_a_member", 4, good_hash),
            mk_rec(7, "oversize.bin", "m1", 8, good_hash),
            mk_rec(
                8,
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
        rep.invalid_skipped >= 5,
        "bad/forged records must be refused and counted"
    );
    assert!(
        rep.held_back >= 1,
        "unknown syntactically valid member must be held back as 'not yet', not rejected as invalid"
    );
    assert_eq!(
        world.members[0]
            .engine
            .store()
            .load_peer_cursors()
            .unwrap()
            .get("m1")
            .unwrap()
            .applied_ack_seq,
        0,
        "applied_ack_seq must not advance past unapplied records"
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

/// Scenario 9: Swap/claim boundary races (`r1` two saves around exchange, `r2` save then move
/// around exchange, `r3` same-size/same-mtime save right after exchange, `r5`/`r6` crash after
/// claim/exchange then new save before restart).
fn run_swap_boundary_races_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "swap_boundary_races",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    // m0 uses atomic exchange (`supports_exchange = true`), m1 uses claim-then-rename (`false`).
    let a = 0;
    let b = 1;
    let target_member = if (seed & 1) == 0 { a } else { b };
    let source_member = if target_member == a { b } else { a };
    let displace_op = if target_member == a {
        "exchange_tmp_with_live"
    } else {
        "move_live_to_tmp_no_replace"
    };
    let place_op = if target_member == a {
        "exchange_tmp_with_live"
    } else {
        "commit_tmp_to_live_no_replace"
    };

    // Baseline synced.
    world.user_create_or_edit_file(source_member, "doc.txt", true);
    world.user_create_or_edit_file(source_member, "del.txt", true);
    world.sync_connected_groups_to_quiescence();

    // Case 1 (r1/r2/r3): source updates `doc.txt`; target has concurrent saves/moves right before
    // and right after the placement syscall!
    world.global_secs += 30;
    let src_id = world.members[source_member].id.clone();
    if let Some(old_h) = world.members[source_member]
        .engine
        .fs()
        .live_file_hash("doc.txt")
    {
        world.note_person_removed(&src_id, old_h);
    }
    let remote_bytes = b"remote-fixed-16!".to_vec();
    let remote_hash = hash_bytes(&remote_bytes);
    let now_src = world.members[source_member].local_time(world.global_secs);
    world.members[source_member].engine.fs().user_write_file(
        "doc.txt",
        remote_bytes.clone(),
        remote_hash,
        now_src,
    );
    world.note_created(&src_id, remote_hash);
    world.scan_member(source_member);

    let now_dst = world.members[target_member].local_time(world.global_secs + 1);
    let (save1_b, save1_h) = world.fresh_bytes();
    let save2_b = b"newest-fixed-16!".to_vec();
    let save2_h = hash_bytes(&save2_b);

    let subcase = (seed >> 1) % 3;
    match subcase {
        0 => {
            // r1: save1 right BEFORE swap/claim, save2 right AFTER swap/claim!
            world.members[target_member].engine.fs().arm_mid_apply(
                0,
                false,
                Some(displace_op),
                MidApplyOp::EditOrCreateFile {
                    path: "doc.txt".into(),
                    bytes: save1_b,
                    hash: save1_h,
                    mtime: Timestamp::new(now_dst.secs + 1, 0),
                },
            );
            world.members[target_member].engine.fs().arm_mid_apply(
                0,
                true,
                Some(displace_op),
                MidApplyOp::EditOrCreateFile {
                    path: "doc.txt".into(),
                    bytes: save2_b,
                    hash: save2_h,
                    mtime: Timestamp::new(now_dst.secs + 2, 0),
                },
            );
        }
        1 => {
            // r2: save1 BEFORE swap/claim, then move `doc.txt` -> `moved.txt` AFTER placement!
            world.members[target_member].engine.fs().arm_mid_apply(
                0,
                false,
                Some(displace_op),
                MidApplyOp::EditOrCreateFile {
                    path: "doc.txt".into(),
                    bytes: save1_b,
                    hash: save1_h,
                    mtime: Timestamp::new(now_dst.secs + 1, 0),
                },
            );
            world.members[target_member].engine.fs().arm_mid_apply(
                0,
                true,
                Some(place_op),
                MidApplyOp::RenameFile {
                    from: "doc.txt".into(),
                    to: "moved.txt".into(),
                },
            );
        }
        _ => {
            // r3: same-size, same-mtime save right AFTER placement (`needs_rescan` must catch it!).
            world.members[target_member].engine.fs().arm_mid_apply(
                0,
                true,
                Some(place_op),
                MidApplyOp::SameSizeInPlaceSave {
                    path: "doc.txt".into(),
                    bytes: save2_b,
                    hash: save2_h,
                },
            );
        }
    }

    // Pull without injecting random extra faults so our exact boundary hook fires.
    world.sync_pair_with_faults(
        target_member,
        source_member,
        false,
        false,
        false,
        false,
        PeerFault::None,
    );
    world.scan_member(target_member);

    if subcase == 2 {
        // Verify r3: normal scan MUST catch the same-size same-mtime overwrite via `needs_rescan`!
        let rec = world.members[target_member]
            .engine
            .store()
            .get_record("doc.txt")
            .unwrap()
            .unwrap();
        let disk_h = world.members[target_member]
            .engine
            .fs()
            .live_file_hash("doc.txt");
        assert_eq!(rec.hash, disk_h, "r3: index/disk mismatch after scan");
    }

    // Case 2 (r5/r6): delete `del.txt` on source, crash on target right after `move_live_to_tmp_no_replace`,
    // then person writes a brand-new `del.txt` before recovery runs!
    world.global_secs += 30;
    world.user_delete_path(source_member, "del.txt", true);
    world.members[target_member].engine.fs().arm_crash(0, true);
    world.sync_pair_with_faults(
        target_member,
        source_member,
        false,
        false,
        false,
        false,
        PeerFault::None,
    );
    let (new_del_b, new_del_h) = world.fresh_bytes();
    let now_post = world.members[target_member].local_time(world.global_secs + 5);
    world.members[target_member]
        .engine
        .fs()
        .user_write_file("del.txt", new_del_b, new_del_h, now_post);
    world.note_created(&world.members[target_member].id.clone(), new_del_h);
    world.members[target_member].engine.recover().unwrap();
    world.scan_member(target_member);

    world.sync_connected_groups_to_quiescence();
    for m in &world.members {
        if m.active {
            assert!(
                m.engine.fs().all_live_hashes().contains(&new_del_h),
                "post-crash user save at del.txt must survive live on {}",
                m.id
            );
            if subcase == 0 || subcase == 2 {
                assert!(
                    m.engine.fs().all_live_hashes().contains(&save2_h),
                    "newest save around swap must survive live on {}",
                    m.id
                );
            }
            if subcase == 0 || subcase == 1 {
                assert!(
                    m.engine.fs().all_live_hashes().contains(&save1_h)
                        || m.engine.fs().all_bin_hashes().contains(&save1_h),
                    "first save before swap must survive on {}",
                    m.id
                );
            }
        }
    }
    world.check_invariants(true);
}

/// Scenario 10: Late membership arrival (`m1`) and skipped/unapplied deletion never acknowledged
/// or resurrected (`m2`).
fn run_late_membership_and_unacked_deletion_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mut mk = |id: &str, members: &[&str]| {
        let mut cfg = FolderConfig::new(
            "sim",
            id.to_string(),
            members.iter().map(|s| (*s).to_string()),
        );
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };

    // m0 initially does NOT know about m2; m1 and m2 know all three.
    let mut m0 = mk("m0", &["m0", "m1"]);
    let mut m1 = mk("m1", &["m0", "m1", "m2"]);
    let mut m2 = mk("m2", &["m0", "m1", "m2"]);

    // m1 creates `base.txt` and syncs to m0 and m2.
    let base_b = b"base content".to_vec();
    let base_h = hash_bytes(&base_b);
    m1.fs().user_write_file("base.txt", base_b, base_h, t0);
    m1.scan(t0).unwrap();
    m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t0)
        .unwrap();
    m2.pull_from_peer(&SimPeer::new(&m1, "m2", "m1", PeerFault::None), t0)
        .unwrap();

    // m2 creates `from_m2.txt` AND deletes `base.txt`; m1 pulls from m2.
    let t1 = Timestamp::new(t0.secs + 10, 0);
    let m2_b = b"authored by m2".to_vec();
    let m2_h = hash_bytes(&m2_b);
    m2.fs().user_write_file("from_m2.txt", m2_b, m2_h, t1);
    m2.fs().user_remove_recursive("base.txt");
    m2.scan(t1).unwrap();
    m1.pull_from_peer(&SimPeer::new(&m2, "m1", "m2", PeerFault::None), t1)
        .unwrap();

    // m0 pulls from m1 BEFORE learning about m2 -> records mentioning m2 must be held back as
    // UnknownMember ("not yet") and NOT acknowledged!
    let t2 = Timestamp::new(t0.secs + 20, 0);
    let rep = m0
        .pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t2)
        .unwrap();
    assert!(rep.held_back >= 2);
    assert_eq!(rep.invalid_skipped, 0);
    let m0_c = m0.store().load_peer_cursors().unwrap();
    assert!(
        m0_c["m1"].applied_ack_seq < m0_c["m1"].seq,
        "applied_ack_seq must stay behind held-back records"
    );

    // m1 pulls from m0 -> m1 must NOT GC `base.txt`'s tombstone because m0 did not ack it!
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t2)
        .unwrap();
    assert!(
        m1.store()
            .get_record("base.txt")
            .unwrap()
            .is_some_and(|r| r.deleted),
        "m1 must retain base.txt tombstone while m0 holds it back unacked"
    );

    // Now m0 learns m2! Both `from_m2.txt` and `base.txt` deletion must converge cleanly!
    let t3 = Timestamp::new(t0.secs + 30, 0);
    m0.add_member("m2").unwrap();
    for _ in 0..3 {
        m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t3)
            .unwrap();
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t3)
            .unwrap();
        m2.pull_from_peer(&SimPeer::new(&m1, "m2", "m1", PeerFault::None), t3)
            .unwrap();
        m1.pull_from_peer(&SimPeer::new(&m2, "m1", "m2", PeerFault::None), t3)
            .unwrap();
    }
    assert_eq!(m0.fs().live_file_hash("from_m2.txt"), Some(m2_h));
    assert_eq!(m0.fs().live_file_hash("base.txt"), None);
    assert_eq!(m1.fs().live_file_hash("base.txt"), None);
    assert_eq!(m2.fs().live_file_hash("base.txt"), None);
    assert_eq!(m0.store().held_back_count().unwrap(), 0);
    let _ = root_seed;
}

/// Scenario 11: Restored backup making MORE changes than the lost gap (and edit-first-then-new)
/// must be detected immediately by bidirectional chained-fingerprint handshake without losing a
/// single post-restore edit (`m3`, `m5`, `m6`, `m7`).
fn run_backup_restore_more_changes_than_gap_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "backup_restore_more_changes_than_gap",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let a = 0;
    let b = 1;
    for i in 0..4 {
        world.user_create_or_edit_file(b, &format!("f{i}.txt"), true);
    }
    world.sync_connected_groups_to_quiescence();

    // Backup B.
    let b_fs_snap = world.members[b].engine.fs().snapshot();
    let b_store_snap = world.members[b].engine.store().snapshot();
    let b_id = world.members[b].id.clone();
    let b_inc_before = world.members[b].engine.meta().unwrap().incarnation;

    // 3 edits after backup, synced to everyone (small gap of 3).
    // When `seed % 3 == 2`, ONLY `a` creates files so `b` loses only APPLIED records (its own
    // `local_clock` never changed between backup and restore — `review5::b`).
    let applied_only_lost = (seed % 3) == 2;
    world.global_secs += 60;
    if applied_only_lost {
        for i in 0..3 {
            world.user_create_or_edit_file(a, &format!("from_a_{i}.txt"), true);
        }
    } else {
        for i in 0..3 {
            world.user_create_or_edit_file(b, &format!("f{i}.txt"), true);
        }
    }
    world.sync_connected_groups_to_quiescence();

    // Restore B from backup.
    let post_hashes = world.members[b].engine.fs().all_live_hashes();
    let backup_hashes: BTreeSet<Hash> = b_fs_snap
        .live
        .values()
        .filter_map(|n| match n {
            SimNode::File { hash, .. } => Some(*hash),
            _ => None,
        })
        .collect();
    for h in post_hashes.difference(&backup_hashes) {
        world.note_person_removed(&b_id, *h);
    }
    let all_names: Vec<String> = world.members.iter().map(|m| m.id.clone()).collect();
    let mut restored = SyncEngine::open(
        FolderConfig::new("sim", b_id.clone(), all_names),
        HonestSimFs::from_snapshot(b_fs_snap),
        FaultyIndexStore::from_inner(b_store_snap),
        world.members[b].token_store.clone(),
    )
    .unwrap();
    restored.set_deletion_brake(brake_disabled());
    world.members[b].engine = restored;

    // Make post-restore changes on B:
    // - mode 0: MORE changes than gap, edit-first-then-new (m7)
    // - mode 1: MORE changes than gap, new-first-then-edit (m6)
    // - mode 2: applied-only-records-lost backup + 1 new file (review5::b: local_seq < a.pulled_seq)
    world.global_secs += 60;
    let mut must_survive = Vec::new();
    if applied_only_lost {
        let p = "after_restore_applied_only.txt".to_string();
        world.user_create_or_edit_file(b, &p, true);
        must_survive.push(world.members[b].engine.fs().live_file_hash(&p).unwrap());
    } else if (seed & 1) == 0 {
        for i in 0..3 {
            world.user_create_or_edit_file(b, &format!("f{i}.txt"), true);
            must_survive.push(
                world.members[b]
                    .engine
                    .fs()
                    .live_file_hash(&format!("f{i}.txt"))
                    .unwrap(),
            );
        }
        for j in 0..5 {
            let p = format!("new{j}.txt");
            world.user_create_or_edit_file(b, &p, true);
            must_survive.push(world.members[b].engine.fs().live_file_hash(&p).unwrap());
        }
    } else {
        for j in 0..5 {
            let p = format!("new{j}.txt");
            world.user_create_or_edit_file(b, &p, true);
            must_survive.push(world.members[b].engine.fs().live_file_hash(&p).unwrap());
        }
        for i in 0..3 {
            world.user_create_or_edit_file(b, &format!("f{i}.txt"), true);
            must_survive.push(
                world.members[b]
                    .engine
                    .fs()
                    .live_file_hash(&format!("f{i}.txt"))
                    .unwrap(),
            );
        }
    }

    // A pulls from B first (must refuse!), then B pulls from A (must rotate incarnation!).
    {
        let now_a = world.members[a].local_time(world.global_secs);
        let (left, right) = world.members.split_at_mut(b);
        let peer_b = SimPeer::new(&right[0].engine, &left[a].id, &right[0].id, PeerFault::None);
        assert!(
            left[a].engine.pull_from_peer(&peer_b, now_a).is_err(),
            "healthy peer must refuse diverged restored member before rotation"
        );
    }
    {
        let now_b = world.members[b].local_time(world.global_secs);
        let (left, right) = world.members.split_at_mut(b);
        let peer_a = SimPeer::new(&left[a].engine, &right[0].id, &left[a].id, PeerFault::None);
        right[0].engine.pull_from_peer(&peer_a, now_b).unwrap();
    }
    assert_ne!(
        world.members[b].engine.meta().unwrap().incarnation,
        b_inc_before
    );
    world.sync_connected_groups_to_quiescence();
    for m in &world.members {
        if !m.active {
            continue;
        }
        let live = m.engine.fs().all_live_hashes();
        for h in &must_survive {
            assert!(
                live.contains(h),
                "post-restore change lost on {} (seed={seed})",
                m.id
            );
        }
    }
    world.check_invariants(true);
}

/// Scenario 12: Both kinds of store transition on a member holding unsynced work while a peer is
/// partitioned:
/// - Even seeds: in-place schema migration (`MIN_MIGRATABLE_SCHEMA_VERSION` -> current), keeping
///   the single store, incarnation, and every unsynced deletion intact without entering join mode.
/// - Odd seeds: destroyed `.legion-sync/index.redb` stops with `IndexDamaged`, changes nothing on
///   disk, and explicit `rebuild_from_disk` recovers as a Decision 3 true join (keeping peer-known
///   deletions deleted and reporting `WaitingForMember` while unsynced files remain local).
fn run_index_wipe_and_explicit_rebuild_seed(root_seed: u64, seed: u64, record_trace: bool) {
    let mut world = SimWorld::new(
        root_seed,
        "index_wipe_explicit_rebuild",
        seed,
        false,
        Vec::new(),
        record_trace,
    );
    let a = 0;
    let b = 1;
    let migrate_in_place = (seed & 1) == 0;
    world.user_create_or_edit_file(a, "keep.txt", true);
    world.user_create_or_edit_file(a, "synced_del.pdf", true);
    world.user_create_or_edit_file(a, "secret.pdf", true);
    let keep_h = world.members[a]
        .engine
        .fs()
        .live_file_hash("keep.txt")
        .unwrap();
    world.sync_connected_groups_to_quiescence();

    // Delete `synced_del.pdf` while connected so peers hold its permanent tombstone.
    world.global_secs += 30;
    world.user_delete_path(a, "synced_del.pdf", true);
    world.sync_connected_groups_to_quiescence();

    // Partition B off; A deletes `secret.pdf`.
    world.members[b].group = 1;
    world.global_secs += 30;
    world.user_delete_path(a, "secret.pdf", true);

    let a_snap = world.members[a].engine.fs().snapshot();
    let a_id = world.members[a].id.clone();
    let all_names: Vec<String> = world.members.iter().map(|m| m.id.clone()).collect();
    let now_a = world.members[a].local_time(world.global_secs + 10);
    let unsynced_body = format!("only-on-a-{seed}").into_bytes();
    let unsynced_h = hash_bytes(&unsynced_body);

    if migrate_in_place {
        let mut store = FaultyIndexStore::from_inner(world.members[a].engine.store().snapshot());
        let inc_before = world.members[a].engine.meta().unwrap().incarnation;
        store.set_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION);
        let mut reopened = SyncEngine::open(
            FolderConfig::new("sim", a_id, all_names),
            HonestSimFs::from_snapshot(a_snap),
            store,
            world.members[a].token_store.clone(),
        )
        .unwrap();
        reopened.set_deletion_brake(brake_disabled());
        assert!(!reopened.is_index_damaged());
        assert_eq!(reopened.meta().unwrap().incarnation, inc_before);
        reopened
            .fs()
            .user_write_file("only_on_a.txt", unsynced_body, unsynced_h, now_a);
        reopened.scan(now_a).unwrap();
        world.members[a].engine = reopened;
    } else {
        // Destroy A's index store while keeping A's folder + marker -> stops as IndexDamaged.
        let mut reopened = SyncEngine::open(
            FolderConfig::new("sim", a_id, all_names),
            HonestSimFs::from_snapshot(a_snap),
            FaultyIndexStore::new(),
            world.members[a].token_store.clone(),
        )
        .unwrap();
        reopened.set_deletion_brake(brake_disabled());
        assert!(reopened.is_index_damaged());
        match reopened.status().unwrap().state {
            MemberState::Problem { sentence, problems } => {
                assert_eq!(sentence, INDEX_DAMAGED_PROBLEM_SENTENCE);
                assert!(problems.iter().any(|p| p.kind == ProblemKind::IndexDamaged));
            }
            other => panic!("expected IndexDamaged state, got {other:?}"),
        }
        assert!(matches!(
            reopened.scan(now_a),
            Err(SyncError::IndexDamaged { .. })
        ));
        reopened
            .fs()
            .user_write_file("only_on_a.txt", unsynced_body, unsynced_h, now_a);
        reopened.rebuild_from_disk(now_a).unwrap();
        assert!(!reopened.is_index_damaged());
        world.members[a].engine = reopened;
    }

    // B reconnects: let A pull from B first -> A still holds `only_on_a.txt` that B has not
    // pulled yet, so A MUST report WaitingForMember (NEVER UpToDate)!
    world.members[b].group = 0;
    {
        let (left, right) = world.members.split_at_mut(b);
        left[a]
            .engine
            .pull_from_peer(
                &SimPeer::new(&right[0].engine, &left[a].id, &right[0].id, PeerFault::None),
                Timestamp::new(now_a.secs + 1, 0),
            )
            .unwrap();
        let st_a = left[a].engine.status().unwrap();
        assert!(
            matches!(st_a.state, MemberState::WaitingForMember { .. }),
            "seed={root_seed}: member holding unsynced file must report WaitingForMember, got {:?}",
            st_a.state
        );
    }
    world.sync_connected_groups_to_quiescence();
    for m in &world.members {
        if m.active {
            assert_eq!(m.engine.fs().live_file_hash("synced_del.pdf"), None);
            if migrate_in_place {
                assert_eq!(m.engine.fs().live_file_hash("secret.pdf"), None);
            }
            assert_eq!(m.engine.fs().live_file_hash("keep.txt"), Some(keep_h));
            assert_eq!(
                m.engine.fs().live_file_hash("only_on_a.txt"),
                Some(unsynced_h)
            );
        }
    }
    world.check_invariants(true);
}

/// Scenario 13: Move-aware windowed outbound brake (`b1`, `b2`, `b3`) and matching inbound peer
/// deletion brake (`b4`: both `KeepLocalFiles` and `ApplyDeletions`).
fn run_deletion_brake_and_inbound_brake_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let brake = DeletionBrakeConfig {
        min_deleted_files: 6,
        min_deleted_percent: 25,
        settle_secs: 30,
        window_secs: 600,
    };
    let mut mk = |id: &str| {
        let mut cfg = FolderConfig::new("sim", id.to_string(), ["m0".into(), "m1".into()]);
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        cfg.deletion_brake = brake;
        SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap()
    };
    let mut m0 = mk("m0");
    let mut m1 = mk("m1");
    let t0 = Timestamp::new(1_795_608_000, 0);

    // 16 files in d0 (8) and d1 (8).
    for d in 0..2 {
        for i in 0..8 {
            let b = format!("payload-{d}-{i}-{seed}").into_bytes();
            let h = hash_bytes(&b);
            m0.fs().user_write_file(&format!("d{d}/f{i}.txt"), b, h, t0);
        }
    }
    m0.scan(t0).unwrap();
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t0)
        .unwrap();

    // b1: rename d0 -> d0_moved (8 files moved > 6 and 50% > 25%) -> must NOT trip brake!
    let t1 = Timestamp::new(t0.secs + 5, 0);
    for i in 0..8 {
        let old_p = format!("d0/f{i}.txt");
        let new_p = format!("d0_moved/f{i}.txt");
        let node = m0.fs().live.borrow_mut().remove(&old_p).unwrap();
        m0.fs()
            .live
            .borrow_mut()
            .insert("d0_moved".to_string(), SimNode::Dir { mtime: t1 });
        m0.fs().live.borrow_mut().insert(new_p, node);
    }
    m0.fs().live.borrow_mut().remove("d0");
    let r_move = m0.scan(t1).unwrap();
    assert!(!r_move.brake_tripped);
    let p_move = m1
        .pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t1)
        .unwrap();
    assert_eq!(p_move.local_renames, 8);

    // b2: split deletion across two subtree scans -> 0 leaked, brake trips!
    let t2 = Timestamp::new(t0.secs + 40, 0);
    for i in 0..4 {
        m0.fs().user_remove_recursive(&format!("d0_moved/f{i}.txt"));
    }
    let r_half1 = m0
        .scan_subtree("d0_moved", t2, ScanOptions::default())
        .unwrap();
    assert!(!r_half1.brake_tripped);
    assert_eq!(r_half1.deleted, 0);
    for i in 4..8 {
        m0.fs().user_remove_recursive(&format!("d0_moved/f{i}.txt"));
    }
    let r_half2 = m0
        .scan_subtree(
            "d0_moved",
            Timestamp::new(t2.secs + 5, 0),
            ScanOptions::default(),
        )
        .unwrap();
    assert!(r_half2.brake_tripped);
    assert_eq!(r_half2.deleted, 0);

    // Confirm outbound brake on m0 -> m1 must trip inbound peer brake!
    let t3 = Timestamp::new(t2.secs + 10, 0);
    m0.resolve_deletion_brake(DeletionBrakeDecision::ConfirmDeleted, t3)
        .unwrap();
    assert!(matches!(
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t3),
        Err(SyncError::Paused { .. })
    ));
    assert_eq!(m1.status().unwrap().inbound_brakes.len(), 1);

    if (seed & 1) == 0 {
        // KeepLocalFiles re-asserts all 8 files back to m0.
        let kept = m1
            .resolve_inbound_deletion_brake(
                "m0",
                InboundDeletionBrakeDecision::KeepLocalFiles,
                Timestamp::new(t3.secs + 1, 0),
            )
            .unwrap();
        assert_eq!(kept, 8);
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t3)
            .unwrap();
        m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t3)
            .unwrap();
        assert!(m0.fs().live_file_hash("d0_moved/f0.txt").is_some());
    } else {
        // ApplyDeletions deletes all 8 files on m1.
        m1.resolve_inbound_deletion_brake(
            "m0",
            InboundDeletionBrakeDecision::ApplyDeletions,
            Timestamp::new(t3.secs + 1, 0),
        )
        .unwrap();
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t3)
            .unwrap();
        assert!(m1.fs().live_file_hash("d0_moved/f0.txt").is_none());
    }
    let _ = root_seed;
}

/// Scenario 14: Lying peer defenses — forged own counter, > 2^62 counter, 1/hour forced-rotation
/// rate limit, and escaping/absolute symlinks outbound and inbound (Item 9).
fn run_adversary_counters_rotation_and_symlinks_seed(
    root_seed: u64,
    seed: u64,
    _record_trace: bool,
) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mut cfg = FolderConfig::new("sim", "m0".to_string(), ["m0".into(), "m1".into()]);
    cfg.initial_incarnation = Some(rng.next_u64() | 1);
    let mut m0 = SyncEngine::open(
        cfg,
        HonestSimFs::new(FsCapabilities::unix(), true),
        FaultyIndexStore::new(),
        MemoryInstanceTokenStore::new(),
    )
    .unwrap();
    m0.set_deletion_brake(brake_disabled());

    // Outbound escaping symlink must be held back & listed as a problem, never indexed.
    m0.fs()
        .create_symlink_live("bad_link", "../../outside")
        .unwrap();
    m0.fs().create_symlink_live("good_link", "ok.txt").unwrap();
    let ok_b = b"ok".to_vec();
    let ok_h = hash_bytes(&ok_b);
    m0.fs().user_write_file("ok.txt", ok_b, ok_h, t0);
    let scan = m0.scan(t0).unwrap();
    assert_eq!(scan.held_back_problems.len(), 1);
    assert!(m0.store().get_record("bad_link").unwrap().is_none());
    assert!(m0.store().get_record("good_link").unwrap().is_some());

    // Lying peer triggers forced rotation twice within 1 hour -> 1st succeeds, 2nd rate-limited!
    struct ForcedRotPeer {
        claim_inc: u64,
        claim_seq: u64,
        records: Vec<IndexRecord>,
    }
    impl Peer for ForcedRotPeer {
        fn member_id(&self) -> &str {
            "m1"
        }
        fn handshake(
            &self,
            _caller: &legion_sync::HandshakeHello,
        ) -> legion_sync::Result<PeerHandshake> {
            Ok(PeerHandshake {
                folder_id: "sim".into(),
                member_id: "m1".into(),
                incarnation: 777,
                snapshot_seq: self.records.len() as u64,
                head_fingerprint: None,
                protocol_version: SYNC_PROTOCOL_VERSION,
                caller_pulled: Some(PeerAckView {
                    incarnation: self.claim_inc,
                    seq: self.claim_seq,
                    fingerprint: Some(hash_bytes(b"forged-fp")),
                }),
                caller_acked: None,
                caller_pull_matches: Some(true),
                caller_rejoining: false,
                refused_from_caller: None,
            })
        }
        fn records_since(
            &self,
            _inc: u64,
            since_seq: u64,
            _limit: PageLimit,
            _cancel: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            let records: Vec<IndexRecord> = self
                .records
                .iter()
                .filter(|r| r.seq > since_seq)
                .cloned()
                .collect();
            Ok(RecordsPage {
                incarnation: 777,
                records,
                snapshot_seq: self.records.len() as u64,
                has_more: false,
                caller_acked: None,
                caller_acked_fingerprint: None,
                head_fingerprint: None,
                chain_fingerprints: BTreeMap::new(),
            })
        }
        fn fetch_file(
            &self,
            _path: &str,
            _hash: Hash,
            _opts: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            Ok(Box::new(Cursor::new(Vec::new())))
        }
    }

    let inc0 = m0.meta().unwrap().incarnation;
    let mut bad_vv = VersionVector::new();
    bad_vv.bump_above(VersionKey::new("m1", 777), 0);
    let escaping_sym = IndexRecord {
        path: "in_escape".into(),
        kind: EntryKind::Symlink,
        size: 0,
        mtime: t0,
        disk_mtime: None,
        disk_change_id: None,
        mode: Some(0o777),
        hash: None,
        link_target: Some("../secret".into()),
        deleted: false,
        provenance: RecordProvenance::Versioned(bad_vv.clone()),
        base_hash: None,
        modified_by: "m1".into(),
        seq: 1,
        recent_hashes: Vec::new(),
        conflict_of: None,
        conflict_dismissed: false,
    };
    let mut huge_vv = VersionVector::new();
    huge_vv.bump_above(VersionKey::new("m1", 777), MAX_VERSION_COUNTER);
    let huge_rec = IndexRecord {
        path: "huge.txt".into(),
        kind: EntryKind::File,
        size: 1,
        mtime: t0,
        disk_mtime: None,
        disk_change_id: None,
        mode: Some(0o644),
        hash: Some(hash_bytes(b"x")),
        link_target: None,
        deleted: false,
        provenance: RecordProvenance::Versioned(huge_vv),
        base_hash: None,
        modified_by: "m1".into(),
        seq: 2,
        recent_hashes: Vec::new(),
        conflict_of: None,
        conflict_dismissed: false,
    };

    // A peer reporting received seq > local_seq triggers self-detected rollback rotation once,
    // while still refusing escaping symlinks and > 2^62 counters.
    let p1 = ForcedRotPeer {
        claim_inc: inc0,
        claim_seq: 999,
        records: vec![escaping_sym, huge_rec],
    };
    let r1 = m0
        .pull_from_peer(&p1, Timestamp::new(t0.secs + 60, 0))
        .unwrap();
    let inc1 = m0.meta().unwrap().incarnation;
    assert_ne!(
        inc1, inc0,
        "first rollback claim (seq > local_seq) rotates once"
    );
    assert_eq!(r1.held_back, 1, "escaping inbound symlink held back");
    assert_eq!(r1.invalid_skipped, 1, "> 2^62 counter refused");
    assert!(m0.fs().stat_live("in_escape").unwrap().is_none());

    // Second forced rotation within 1 day MUST be rate-limited!
    let p2 = ForcedRotPeer {
        claim_inc: inc1,
        claim_seq: 1,
        records: Vec::new(),
    };
    assert!(matches!(
        m0.pull_from_peer(&p2, Timestamp::new(t0.secs + 120, 0)),
        Err(SyncError::Conflict { .. })
    ));
    assert_eq!(m0.meta().unwrap().incarnation, inc1);
    let _ = root_seed;
}

/// Scenario 15: Two joiners carrying the exact same old USB copy meet each other BEFORE meeting
/// any member that saw the deletion (both with active tombstone and after tombstone GC), plus
/// intentional re-creation after GC (`r1`, `r17`).
fn run_two_joiners_same_old_copy_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mut mk = |id: &str, members: &[&str]| {
        let mut cfg = FolderConfig::new(
            "sim",
            id.to_string(),
            members.iter().map(|s| (*s).to_string()),
        );
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };

    let mut m0 = mk("m0", &["m0", "m1"]);
    let mut m1 = mk("m1", &["m0", "m1"]);
    let old_bytes = format!("old-tax-return-{seed}").into_bytes();
    let old_h = hash_bytes(&old_bytes);
    let keep_bytes = format!("keep-{seed}").into_bytes();
    let keep_h = hash_bytes(&keep_bytes);

    m0.fs()
        .user_write_file("old.pdf", old_bytes.clone(), old_h, t0);
    m0.fs()
        .user_write_file("keep.txt", keep_bytes.clone(), keep_h, t0);
    m0.scan(t0).unwrap();
    for _ in 0..2 {
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t0)
            .unwrap();
        m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t0)
            .unwrap();
    }

    // m0 deletes old.pdf; sync across m0 and m1 and verify permanent tombstones stay cached.
    let t1 = Timestamp::new(t0.secs + 10, 0);
    m0.fs().user_remove_recursive("old.pdf");
    m0.scan(t1).unwrap();
    let full_round = (seed & 1) == 0;
    if full_round {
        for _ in 0..3 {
            m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t1)
                .unwrap();
            m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t1)
                .unwrap();
        }
        assert!(
            m0.store()
                .get_record("old.pdf")
                .unwrap()
                .is_some_and(|r| r.deleted)
        );
        assert!(
            m1.store()
                .get_record("old.pdf")
                .unwrap()
                .is_some_and(|r| r.deleted)
        );
    } else {
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t1)
            .unwrap();
    }

    // Two new joiners (m2, m3) start from the exact same old USB copy!
    let all4 = ["m0", "m1", "m2", "m3"];
    m0.add_member("m2").unwrap();
    m0.add_member("m3").unwrap();
    m1.add_member("m2").unwrap();
    m1.add_member("m3").unwrap();
    let mut m2 = mk("m2", &all4);
    let mut m3 = mk("m3", &all4);
    let t2 = Timestamp::new(t0.secs + 20, 0);
    for m in [&mut m2, &mut m3] {
        m.fs()
            .user_write_file("old.pdf", old_bytes.clone(), old_h, t0);
        m.fs()
            .user_write_file("keep.txt", keep_bytes.clone(), keep_h, t0);
        m.scan(t2).unwrap();
    }

    // Joiners meet EACH OTHER first before meeting m0 or m1!
    m2.pull_from_peer(&SimPeer::new(&m3, "m2", "m3", PeerFault::None), t2)
        .unwrap();
    m3.pull_from_peer(&SimPeer::new(&m2, "m3", "m2", PeerFault::None), t2)
        .unwrap();

    // Now sync all 4 in varied order (starting with non-originator m1 on half the seeds).
    let t3 = Timestamp::new(t0.secs + 30, 0);
    let mut es = [m0, m1, m2, m3];
    let first_pair = if (seed & 2) == 0 { (1, 2) } else { (2, 1) };
    {
        let (a, b) = first_pair;
        let (x, y) = if a < b {
            let (l, r) = es.split_at_mut(b);
            (&mut l[a], &r[0])
        } else {
            let (l, r) = es.split_at_mut(a);
            (&mut r[0], &l[b])
        };
        let x_id = x.meta().unwrap().local_member;
        let y_id = y.meta().unwrap().local_member;
        x.pull_from_peer(&SimPeer::new(y, &x_id, &y_id, PeerFault::None), t3)
            .unwrap();
    }
    for _ in 0..4 {
        for a in 0..4 {
            for b in 0..4 {
                if a == b {
                    continue;
                }
                let (x, y) = if a < b {
                    let (l, r) = es.split_at_mut(b);
                    (&mut l[a], &r[0])
                } else {
                    let (l, r) = es.split_at_mut(a);
                    (&mut r[0], &l[b])
                };
                let x_id = x.meta().unwrap().local_member;
                let y_id = y.meta().unwrap().local_member;
                x.pull_from_peer(&SimPeer::new(y, &x_id, &y_id, PeerFault::None), t3)
                    .unwrap();
            }
        }
    }

    for e in &es {
        assert_eq!(
            e.fs().live_file_hash("old.pdf"),
            None,
            "seed={root_seed}: deleted old.pdf resurrected on {}",
            e.meta().unwrap().local_member
        );
        assert_eq!(e.fs().live_file_hash("keep.txt"), Some(keep_h));
    }

    // Intentional re-creation on m1 after seeing the deletion MUST sync out everywhere.
    if (seed & 4) == 0 {
        let t4 = Timestamp::new(t0.secs + 40, 0);
        es[1]
            .fs()
            .user_write_file("old.pdf", old_bytes.clone(), old_h, t4);
        es[1].scan(t4).unwrap();
        for _ in 0..3 {
            for a in 0..4 {
                for b in 0..4 {
                    if a == b {
                        continue;
                    }
                    let (x, y) = if a < b {
                        let (l, r) = es.split_at_mut(b);
                        (&mut l[a], &r[0])
                    } else {
                        let (l, r) = es.split_at_mut(a);
                        (&mut r[0], &l[b])
                    };
                    let x_id = x.meta().unwrap().local_member;
                    let y_id = y.meta().unwrap().local_member;
                    x.pull_from_peer(&SimPeer::new(y, &x_id, &y_id, PeerFault::None), t4)
                        .unwrap();
                }
            }
        }
        for e in &es {
            assert_eq!(
                e.fs().live_file_hash("old.pdf"),
                Some(old_h),
                "seed={root_seed}: intentionally recreated old.pdf did not sync to {}",
                e.meta().unwrap().local_member
            );
        }
    }
}

/// Scenario 16: Lifetime of both in-place schema migrations and destroyed-index rebuilds,
/// verifying zero version-vector growth on untouched files (both unversioned and versioned) and
/// full convergence of edited files across 2 or 3 members (`r4`, `review5::g`, Decision 1 & 3).
fn run_lifetime_rotations_and_vv_pruning_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let three_members = (seed & 1) == 1;
    let names: Vec<&str> = if three_members {
        vec!["m0", "m1", "m2"]
    } else {
        vec!["m0", "m1"]
    };
    let n = names.len();
    let tokens: Vec<MemoryInstanceTokenStore> =
        (0..n).map(|_| MemoryInstanceTokenStore::new()).collect();
    let mut es: Vec<SyncEngine<HonestSimFs, FaultyIndexStore>> = names
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut cfg = FolderConfig::new(
                "sim",
                (*id).to_string(),
                names.iter().map(|s| (*s).to_string()),
            );
            cfg.initial_incarnation = Some(rng.next_u64() | 1);
            let mut e = SyncEngine::open(
                cfg,
                HonestSimFs::new(FsCapabilities::unix(), true),
                FaultyIndexStore::new(),
                tokens[i].clone(),
            )
            .unwrap();
            e.set_deletion_brake(brake_disabled());
            e
        })
        .collect();

    // `f0.txt` is born pre-initial-scan (Unversioned); `f1.txt` and `f2.txt` are born post-scan
    // (Versioned). Neither untouched `f0.txt` nor untouched `f2.txt` may grow keys!
    let b0 = format!("base-0-{seed}").into_bytes();
    es[0]
        .fs()
        .user_write_file("f0.txt", b0.clone(), hash_bytes(&b0), t0);
    es[0].scan(t0).unwrap();
    for i in 1..=2 {
        let b = format!("base-{i}-{seed}").into_bytes();
        let h = hash_bytes(&b);
        es[0]
            .fs()
            .user_write_file(&format!("f{i}.txt"), b, h, Timestamp::new(t0.secs + 1, 0));
    }
    es[0].scan(Timestamp::new(t0.secs + 1, 0)).unwrap();
    let sync_rounds =
        |es: &mut [SyncEngine<HonestSimFs, FaultyIndexStore>], now: Timestamp, rounds: usize| {
            let len = es.len();
            for _ in 0..rounds {
                for a in 0..len {
                    for b in 0..len {
                        if a == b {
                            continue;
                        }
                        let (x, y) = if a < b {
                            let (l, r) = es.split_at_mut(b);
                            (&mut l[a], &r[0])
                        } else {
                            let (l, r) = es.split_at_mut(a);
                            (&mut r[0], &l[b])
                        };
                        let x_id = x.meta().unwrap().local_member;
                        let y_id = y.meta().unwrap().local_member;
                        x.pull_from_peer(&SimPeer::new(y, &x_id, &y_id, PeerFault::None), now)
                            .unwrap();
                    }
                }
            }
        };
    sync_rounds(&mut es, Timestamp::new(t0.secs + 2, 0), 2);
    let initial_f0_prov = es[0]
        .store()
        .get_record("f0.txt")
        .unwrap()
        .unwrap()
        .provenance;
    let initial_f2_prov = es[0]
        .store()
        .get_record("f2.txt")
        .unwrap()
        .unwrap()
        .provenance;
    assert_eq!(initial_f0_prov, RecordProvenance::Unversioned);
    assert!(initial_f2_prov.as_versioned().is_some());

    // Alternate in-place schema migration AND destroyed-index rebuilds on m1 across 6 rounds.
    let mut expected_m1_incs = 0usize;
    let mut last_m1_inc = es[1].meta().unwrap().incarnation;
    for rot in 1..=6i64 {
        let now = Timestamp::new(t0.secs + rot * 100, 0);
        let snap = es[1].fs().snapshot();
        let mut cfg = FolderConfig::new(
            "sim",
            "m1".to_string(),
            names.iter().map(|s| (*s).to_string()),
        );
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let migrate_this_round = ((seed + rot as u64) & 1) == 0;
        if migrate_this_round {
            let mut store = FaultyIndexStore::from_inner(es[1].store().snapshot());
            store.set_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION);
            let mut migrated = SyncEngine::open(
                cfg,
                HonestSimFs::from_snapshot(snap),
                store,
                tokens[1].clone(),
            )
            .unwrap();
            migrated.set_deletion_brake(brake_disabled());
            assert!(!migrated.is_index_damaged());
            assert_eq!(migrated.meta().unwrap().incarnation, last_m1_inc);
            es[1] = migrated;
        } else {
            let mut rebuilt = SyncEngine::open(
                cfg,
                HonestSimFs::from_snapshot(snap),
                FaultyIndexStore::new(),
                tokens[1].clone(),
            )
            .unwrap();
            rebuilt.set_deletion_brake(brake_disabled());
            assert!(rebuilt.is_index_damaged());
            rebuilt.rebuild_from_disk(now).unwrap();
            last_m1_inc = rebuilt.meta().unwrap().incarnation;
            es[1] = rebuilt;
        }
        expected_m1_incs += usize::from(!migrate_this_round);

        sync_rounds(&mut es, Timestamp::new(now.secs + 1, 0), 2);

        // Edit f1.txt on m1 after migration/rebuild and sync everywhere.
        let body = format!("edit-after-rebuild-{rot}-{seed}").into_bytes();
        let h = hash_bytes(&body);
        es[1]
            .fs()
            .user_write_file("f1.txt", body, h, Timestamp::new(now.secs + 10, 0));
        es[1].scan(Timestamp::new(now.secs + 10, 0)).unwrap();
        sync_rounds(&mut es, Timestamp::new(now.secs + 12, 0), 3);

        for idx in 0..n {
            assert_eq!(
                es[idx].fs().live_file_hash("f1.txt"),
                Some(h),
                "seed={root_seed} rot={rot}: member {} did not receive f1.txt",
                names[idx]
            );
            assert_eq!(
                es[idx].store().held_back_count().unwrap(),
                0,
                "seed={root_seed} rot={rot}: unexpected held-back on {}",
                names[idx]
            );
        }
        let rec0 = es[0].store().get_record("f1.txt").unwrap().unwrap();
        let rec1 = es[1].store().get_record("f1.txt").unwrap().unwrap();
        let m1_keys = rec0
            .provenance
            .to_version_vector()
            .iter()
            .filter(|(k, _)| k.member == "m1")
            .count();
        assert!(
            m1_keys >= expected_m1_incs.max(1) && m1_keys <= expected_m1_incs + 1,
            "seed={root_seed} rot={rot}: expected whole VV retaining m1 keys, got {m1_keys} (rotations={expected_m1_incs})"
        );
        assert_eq!(rec0.provenance, rec1.provenance);
        // Untouched files MUST NOT grow their version vectors across migrations or rebuilds!
        for (untouched, expected_prov) in
            [("f0.txt", &initial_f0_prov), ("f2.txt", &initial_f2_prov)]
        {
            for idx in 0..n {
                let got_prov = es[idx]
                    .store()
                    .get_record(untouched)
                    .unwrap()
                    .unwrap()
                    .provenance;
                assert_eq!(
                    &got_prov, expected_prov,
                    "seed={root_seed} rot={rot}: rebuild/migration grew untouched {untouched} provenance on {} (before={expected_prov:?}, after={got_prov:?})",
                    names[idx]
                );
            }
        }
    }
    let _ = &tokens;
}

/// Scenario 17: Late write through an already-open file handle after a displaced file enters
/// `.legion-sync/bin` — rescued as a live conflict copy by scan, restore, or expiry (`r7`).
fn run_late_write_into_bin_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mut mk = |id: &str| {
        let mut cfg = FolderConfig::new("sim", id.to_string(), ["m0".into(), "m1".into()]);
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };
    let mut m0 = mk("m0");
    let mut m1 = mk("m1");

    let v1 = format!("old-mailbox-{seed}").into_bytes();
    let v1_h = hash_bytes(&v1);
    m0.fs().user_write_file("mail.mbox", v1, v1_h, t0);
    m0.scan(t0).unwrap();
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t0)
        .unwrap();

    // m0 replaces mail.mbox; m1 pulls so m1's old copy enters `.legion-sync/bin`.
    let t1 = Timestamp::new(t0.secs + 10, 0);
    let v2 = format!("newer-mailbox-{seed}").into_bytes();
    let v2_h = hash_bytes(&v2);
    m0.fs().user_write_file("mail.mbox", v2, v2_h, t1);
    m0.scan(t1).unwrap();
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t1)
        .unwrap();

    let bin = m1.list_bin().unwrap();
    assert_eq!(bin.len(), 1);
    let bin_entry = bin[0].clone();

    // Held-open handle writes new bytes into the binned file!
    let t2 = Timestamp::new(t0.secs + 20, 0);
    let late_bytes = format!("old-mailbox-{seed}+NEW-MESSAGE-THROUGH-OPEN-HANDLE").into_bytes();
    let late_h = m1
        .fs()
        .user_modify_bin_file(&bin_entry.storage_name, late_bytes, t2);

    match seed % 3 {
        0 => {
            // Scan detects modified bin entry and promotes it to a conflict copy.
            m1.scan(Timestamp::new(t2.secs + 1, 0)).unwrap();
        }
        1 => {
            // Restore promotes instead of failing HashMismatch!
            let restored = m1
                .restore_from_bin(
                    &bin_entry.id,
                    Some("restored.mbox"),
                    RestoreStrategy::FailIfExists,
                    Timestamp::new(t2.secs + 1, 0),
                )
                .unwrap();
            assert!(!restored.is_empty());
        }
        _ => {
            // Expiry verifies hash and promotes instead of erasing!
            let expired = m1
                .expire_bin(Timestamp::new(t2.secs + 31 * 86_400, 0))
                .unwrap();
            assert!(
                expired.is_empty(),
                "seed={root_seed}: modified bin entry must not be expired"
            );
        }
    }

    let t3 = Timestamp::new(t2.secs + 5, 0);
    m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t3)
        .unwrap();
    assert!(
        m1.fs().all_live_hashes().contains(&late_h),
        "seed={root_seed}: late handle write lost on m1"
    );
    assert!(
        m0.fs().all_live_hashes().contains(&late_h),
        "seed={root_seed}: late handle write did not sync to m0"
    );
    assert_eq!(m1.fs().live_file_hash("mail.mbox"), Some(v2_h));
    assert_eq!(m0.fs().live_file_hash("mail.mbox"), Some(v2_h));
}

/// Scenario 18: Single-record local placement failures (free-space pre-check, separate mount
/// staging beside destination, single-record permission error) hold back only the failing record,
/// let unrelated records continue, surface the problem on both sides via handshake, and recover
/// once the local condition clears (`r12`, `r14`).
fn run_local_placement_failures_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mut mk = |id: &str| {
        let mut cfg = FolderConfig::new("sim", id.to_string(), ["m0".into(), "m1".into()]);
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };
    let mut m0 = mk("m0");
    let mut m1 = mk("m1");

    // Seed initial doc0 + separate mount staging under VMs/.
    m1.fs().set_separate_mount(Some("VMs"));
    let doc0_v1 = b"doc0-v1".to_vec();
    m0.fs()
        .user_write_file("doc0.txt", doc0_v1.clone(), hash_bytes(&doc0_v1), t0);
    let vm_v1 = format!("vm-notes-{seed}").into_bytes();
    let vm_h = hash_bytes(&vm_v1);
    m0.fs().user_write_file("VMs/notes.txt", vm_v1, vm_h, t0);
    m0.scan(t0).unwrap();
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t0)
        .unwrap();
    assert_eq!(m1.fs().live_file_hash("VMs/notes.txt"), Some(vm_h));

    // Now add an oversized video (larger than m1's free space), small docs, and a doc0 edit.
    let t1 = Timestamp::new(t0.secs + 10, 0);
    let big = vec![b'V'; 16 * 1024];
    let big_h = hash_bytes(&big);
    m0.fs().user_write_file("video.mkv", big, big_h, t1);
    for i in 1..=5 {
        let b = format!("small-{i}-{seed}").into_bytes();
        let h = hash_bytes(&b);
        m0.fs().user_write_file(&format!("doc{i}.txt"), b, h, t1);
    }
    let doc0_v2 = format!("doc0-edited-{seed}").into_bytes();
    let doc0_v2_h = hash_bytes(&doc0_v2);
    m0.fs().user_write_file("doc0.txt", doc0_v2, doc0_v2_h, t1);
    m0.scan(t1).unwrap();

    // Limit free space on m1 so video.mkv fails pre-check (8 KiB free < 16 KiB + margin),
    // OR inject reactive NoSpace on the first file write on odd seeds.
    let precheck_mode = (seed & 1) == 0;
    if precheck_mode {
        m1.fs().set_free_space(Some(8 * 1024));
    } else {
        m1.fs().arm_non_crash_fault(0, FsErrorKind::NoSpace);
    }
    let rep = m1
        .pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t1)
        .unwrap();
    assert!(
        rep.held_back >= 1,
        "seed={root_seed}: expected single failing record held back"
    );
    assert_eq!(
        m1.fs().live_file_hash("doc0.txt"),
        Some(doc0_v2_h),
        "seed={root_seed}: doc0 edit must not be blocked by failing record"
    );
    let arrived_small = (1..=5)
        .filter(|i| m1.fs().live_file_hash(&format!("doc{i}.txt")).is_some())
        .count();
    if precheck_mode {
        assert_eq!(
            arrived_small, 5,
            "seed={root_seed}: all 5 small docs must arrive when only video.mkv exceeds free space"
        );
        assert!(
            m1.fs().live_file_hash("video.mkv").is_none(),
            "seed={root_seed}: oversized video.mkv must be held back before fetching"
        );
    } else {
        assert!(
            arrived_small >= 4,
            "seed={root_seed}: unrelated small docs must still arrive after single reactive ENOSPC (got {arrived_small}/5)"
        );
    }
    assert!(
        matches!(m1.status().unwrap().state, MemberState::Problem { .. }),
        "seed={root_seed}: m1 must report Problem, not UpToDate or Syncing"
    );

    // Sender m0 learns via handshake that m1 refused a record!
    m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t1)
        .unwrap();
    assert!(
        matches!(
            m0.status().unwrap().state,
            MemberState::Problem { ref problems, .. }
                if problems.iter().any(|p| p.kind == ProblemKind::PeerRefusedRecords)
        ),
        "seed={root_seed}: m0 must surface PeerRefusedRecords from handshake"
    );

    // Free space becomes available -> next pull resolves held-back record and clears both sides!
    m1.fs().set_free_space(None);
    m1.fs().disarm_fault();
    let t2 = Timestamp::new(t0.secs + 20, 0);
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t2)
        .unwrap();
    m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t2)
        .unwrap();
    assert_eq!(m1.fs().live_file_hash("video.mkv"), Some(big_h));
    for i in 1..=5 {
        assert!(
            m1.fs().live_file_hash(&format!("doc{i}.txt")).is_some(),
            "seed={root_seed}: doc{i}.txt must arrive once space is available"
        );
    }
    assert_eq!(m1.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(m0.status().unwrap().state, MemberState::UpToDate);
}

/// Scenario 19: Store transitions during the 30-second deletion settle hold (`r9`, Decisions 1-3):
/// - `seed % 3 == 0`: in-place schema migration keeps held `pending_deletions` intact until settled.
/// - `seed % 3 == 1`: reinstall (token lost, store kept) rotates incarnation while keeping held
///   `pending_deletions` intact until settled.
/// - `seed % 3 == 2`: destroyed `.legion-sync/index.redb` stops as `IndexDamaged` and rebuilds as
///   a clean true join without panicking or losing any kept or revived file.
fn run_index_loss_during_hold_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let brake = DeletionBrakeConfig {
        min_deleted_files: 6,
        min_deleted_percent: 25,
        settle_secs: 30,
        window_secs: 600,
    };
    let tok0 = MemoryInstanceTokenStore::new();
    let tok1 = MemoryInstanceTokenStore::new();
    let mut mk = |id: &str, tok: MemoryInstanceTokenStore| {
        let mut cfg = FolderConfig::new("sim", id.to_string(), ["m0".into(), "m1".into()]);
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        cfg.deletion_brake = brake;
        SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            tok,
        )
        .unwrap()
    };
    let mut m0 = mk("m0", tok0);
    let mut m1 = mk("m1", tok1.clone());

    let mut hashes = Vec::new();
    for i in 0..12 {
        let b = format!("hold-file-{i}-{seed}").into_bytes();
        let h = hash_bytes(&b);
        hashes.push(h);
        m0.fs().user_write_file(&format!("f{i}.txt"), b, h, t0);
    }
    m0.scan(t0).unwrap();
    for _ in 0..2 {
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t0)
            .unwrap();
        m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), t0)
            .unwrap();
    }

    // Delete f0 and f1 on m1 and scan once (inside the 30-second settle hold!).
    let t1 = Timestamp::new(t0.secs + 10, 0);
    m1.fs().user_remove_recursive("f0.txt");
    m1.fs().user_remove_recursive("f1.txt");
    let s1 = m1.scan(t1).unwrap();
    assert_eq!(s1.deleted, 0, "deletions should still be held");
    assert_eq!(m1.meta().unwrap().pending_deletions.len(), 2);

    // On half the seeds, revive f1 during the hold before the transition.
    let revive_f1 = (seed & 1) == 1;
    let revived_h = if revive_f1 {
        let rb = format!("revived-f1-{seed}").into_bytes();
        let rh = hash_bytes(&rb);
        m1.fs()
            .user_write_file("f1.txt", rb, rh, Timestamp::new(t1.secs + 5, 0));
        m1.scan(Timestamp::new(t1.secs + 5, 0)).unwrap();
        Some(rh)
    } else {
        None
    };

    let snap = m1.fs().snapshot();
    let store_snap = m1.store().snapshot();
    let inc_before = m1.meta().unwrap().incarnation;
    let mut cfg = FolderConfig::new("sim", "m1".to_string(), ["m0".into(), "m1".into()]);
    cfg.deletion_brake = brake;
    let t2 = Timestamp::new(t1.secs + 40, 0); // past the 30s settle window
    let mode = seed % 3;
    let mut m1_after = match mode {
        0 => {
            // In-place schema migration: store and pending_deletions stay intact!
            let mut store = FaultyIndexStore::from_inner(store_snap);
            store.set_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION);
            let mut e =
                SyncEngine::open(cfg, HonestSimFs::from_snapshot(snap), store, tok1).unwrap();
            assert!(!e.is_index_damaged());
            assert_eq!(e.meta().unwrap().incarnation, inc_before);
            e.scan(t2).unwrap();
            e
        }
        1 => {
            // Reinstall (token lost, store kept): Decision 2 rotates while keeping pending_deletions!
            let mut e = SyncEngine::open(
                cfg,
                HonestSimFs::from_snapshot(snap),
                FaultyIndexStore::from_inner(store_snap),
                MemoryInstanceTokenStore::new(),
            )
            .unwrap();
            assert!(!e.is_index_damaged());
            assert_ne!(e.meta().unwrap().incarnation, inc_before);
            e.scan(t2).unwrap();
            e
        }
        _ => {
            // Destroyed `.legion-sync/index.redb`: stops as damaged and rebuilds as true join.
            let mut e = SyncEngine::open(
                cfg,
                HonestSimFs::from_snapshot(snap),
                FaultyIndexStore::new(),
                tok1,
            )
            .unwrap();
            assert!(e.is_index_damaged());
            e.rebuild_from_disk(t2).unwrap();
            e
        }
    };

    for _ in 0..3 {
        m0.pull_from_peer(&SimPeer::new(&m1_after, "m0", "m1", PeerFault::None), t2)
            .unwrap();
        m1_after
            .pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t2)
            .unwrap();
    }

    if mode != 2 {
        assert_eq!(
            m0.fs().live_file_hash("f0.txt"),
            None,
            "seed={root_seed} mode={mode}: held deletion f0 was forgotten"
        );
        assert_eq!(m1_after.fs().live_file_hash("f0.txt"), None);
        if let Some(rh) = revived_h {
            assert_eq!(m0.fs().live_file_hash("f1.txt"), Some(rh));
            assert_eq!(m1_after.fs().live_file_hash("f1.txt"), Some(rh));
        } else {
            assert_eq!(m0.fs().live_file_hash("f1.txt"), None);
            assert_eq!(m1_after.fs().live_file_hash("f1.txt"), None);
        }
    } else if let Some(rh) = revived_h {
        assert_eq!(m0.fs().live_file_hash("f1.txt"), Some(rh));
        assert_eq!(m1_after.fs().live_file_hash("f1.txt"), Some(rh));
    }
    for (i, h) in hashes.iter().enumerate().skip(2) {
        assert_eq!(m0.fs().live_file_hash(&format!("f{i}.txt")), Some(*h));
        assert_eq!(m1_after.fs().live_file_hash(&format!("f{i}.txt")), Some(*h));
    }
}

/// Scenario 20: Lying peer claiming false rollbacks is bounded strictly per claimant (at most
/// 1/day and stops after 3 rotations until confirmed in status), surfaces a clear confirmation
/// prompt naming the peer, never cuts off honest syncing with other members, and honours the
/// user's confirmation choice (`KeepHonouring` or `IgnoreReports`) (`r11`, `p7`, `review5`).
fn run_lying_peer_regression_rate_limit_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    struct RegressionLiar<'a> {
        inner: SimPeer<'a>,
    }
    impl Peer for RegressionLiar<'_> {
        fn member_id(&self) -> &str {
            self.inner.member_id()
        }
        fn handshake(&self, c: &legion_sync::HandshakeHello) -> legion_sync::Result<PeerHandshake> {
            let mut h = self.inner.handshake(c)?;
            h.caller_pulled = Some(PeerAckView {
                incarnation: c.incarnation,
                seq: c.snapshot_seq.saturating_add(1),
                fingerprint: Some(hash_bytes(b"forged-fp")),
            });
            Ok(h)
        }
        fn records_since(
            &self,
            a: u64,
            b: u64,
            l: PageLimit,
            c: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            self.inner.records_since(a, b, l, c)
        }
        fn fetch_file(
            &self,
            p: &str,
            h: Hash,
            o: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            self.inner.fetch_file(p, h, o)
        }
    }

    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let all = ["tower", "pi", "nas"];
    let mut mk = |id: &str| {
        let mut cfg =
            FolderConfig::new("sim", id.to_string(), all.iter().map(|s| (*s).to_string()));
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), true),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };
    let mut tower = mk("tower");
    let mut pi = mk("pi");
    let mut nas = mk("nas");

    let base = format!("base-{seed}").into_bytes();
    tower
        .fs()
        .user_write_file("f.txt", base.clone(), hash_bytes(&base), t0);
    tower.scan(t0).unwrap();
    for _ in 0..2 {
        pi.pull_from_peer(&SimPeer::new(&tower, "pi", "tower", PeerFault::None), t0)
            .unwrap();
        nas.pull_from_peer(&SimPeer::new(&tower, "nas", "tower", PeerFault::None), t0)
            .unwrap();
        tower
            .pull_from_peer(&SimPeer::new(&pi, "tower", "pi", PeerFault::None), t0)
            .unwrap();
        tower
            .pull_from_peer(&SimPeer::new(&nas, "tower", "nas", PeerFault::None), t0)
            .unwrap();
    }

    // Repeated false rollback claims across 10 ticks spaced every 12h: capped at 1/day and stops
    // after MAX_FORCED_ROTATIONS_BEFORE_CONFIRM (3) rotations until confirmed!
    let mut rotations = 0u32;
    let mut prev_inc = pi.meta().unwrap().incarnation;
    let mut rot_times: Vec<i64> = Vec::new();
    for step in 1..=10i64 {
        let now = Timestamp::new(
            t0.secs + step * (FORCED_ROTATION_COOLDOWN_SECS as i64 / 2),
            0,
        );
        let _ = pi.pull_from_peer(
            &RegressionLiar {
                inner: SimPeer::new(&nas, "pi", "nas", PeerFault::None),
            },
            now,
        );
        let inc = pi.meta().unwrap().incarnation;
        if inc != prev_inc {
            rotations += 1;
            prev_inc = inc;
            rot_times.push(now.secs);
            if rot_times.len() >= 2 {
                let gap = rot_times[rot_times.len() - 1] - rot_times[rot_times.len() - 2];
                assert!(
                    gap >= FORCED_ROTATION_COOLDOWN_SECS as i64,
                    "seed={root_seed}: rotated twice inside 1 day (gap={gap})"
                );
            }
        }
        // Honest work on pi ALWAYS reaches tower!
        let work = format!("pi-work-{step}-{seed}").into_bytes();
        let wh = hash_bytes(&work);
        pi.fs()
            .user_write_file("f.txt", work, wh, Timestamp::new(now.secs + 10, 0));
        pi.scan(Timestamp::new(now.secs + 10, 0)).unwrap();
        tower
            .pull_from_peer(
                &SimPeer::new(&pi, "tower", "pi", PeerFault::None),
                Timestamp::new(now.secs + 11, 0),
            )
            .unwrap();
        pi.pull_from_peer(
            &SimPeer::new(&tower, "pi", "tower", PeerFault::None),
            Timestamp::new(now.secs + 12, 0),
        )
        .unwrap();
        assert_eq!(
            tower.fs().live_file_hash("f.txt"),
            Some(wh),
            "seed={root_seed} step={step}: tower did not get pi's work"
        );
        assert_eq!(tower.store().held_back_count().unwrap(), 0);
    }

    assert_eq!(
        rotations, MAX_FORCED_ROTATIONS_BEFORE_CONFIRM,
        "seed={root_seed}: expected exactly {MAX_FORCED_ROTATIONS_BEFORE_CONFIRM} rotations before stop, got {rotations}"
    );
    let st = pi.status().unwrap();
    assert_eq!(
        st.pending_rollback_confirmations,
        vec!["nas".to_string()],
        "seed={root_seed}: pi must require confirmation for nas"
    );
    assert!(
        matches!(
            st.state,
            MemberState::Problem { ref problems, .. }
                if problems.iter().any(|p| p.kind == ProblemKind::PeerRollbackRateLimited
                    && p.sentence.contains("has reported three times"))
        ),
        "seed={root_seed}: pi must report exact 3-rotation confirmation prompt"
    );

    // Confirming with either KeepHonouring or IgnoreReports clears the confirmation prompt.
    let ignore = rng.chance(1, 2);
    let decision = if ignore {
        PeerRollbackClaimDecision::IgnoreReports
    } else {
        PeerRollbackClaimDecision::KeepHonouring
    };
    pi.confirm_peer_rollback_claims("nas", decision).unwrap();
    assert!(
        pi.status()
            .unwrap()
            .pending_rollback_confirmations
            .is_empty(),
        "seed={root_seed}: pending confirmation must clear after confirm_peer_rollback_claims"
    );
    let later = Timestamp::new(t0.secs + 20 * FORCED_ROTATION_COOLDOWN_SECS as i64, 0);
    let inc_before = pi.meta().unwrap().incarnation;
    pi.pull_from_peer(
        &RegressionLiar {
            inner: SimPeer::new(&nas, "pi", "nas", PeerFault::None),
        },
        later,
    )
    .unwrap();
    if ignore {
        assert_eq!(
            pi.meta().unwrap().incarnation,
            inc_before,
            "seed={root_seed}: IgnoreReports must make future claims from nas change nothing"
        );
    } else {
        assert_ne!(
            pi.meta().unwrap().incarnation,
            inc_before,
            "seed={root_seed}: KeepHonouring must allow future claims from nas again"
        );
    }
}

/// Scenario 21: Randomized asymmetric membership views (`p2`, `p2b`, `p6`, `p6b`):
/// - subcase 0: member added on a subset of peers (unknown to others), relaying edits and
///   deletions with dropped pages mid-sync;
/// - subcase 1: member removed while offline, returning with unsynced edits and deletions after
///   concurrent deletions and tombstone GC on active members;
/// - subcase 2: removed and re-added member with concurrent delete-vs-edit and staggered re-add.
fn run_asymmetric_membership_views_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    struct DropPagesPeer<'a>(SimPeer<'a>);
    impl Peer for DropPagesPeer<'_> {
        fn member_id(&self) -> &str {
            self.0.member_id()
        }
        fn handshake(&self, c: &legion_sync::HandshakeHello) -> legion_sync::Result<PeerHandshake> {
            self.0.handshake(c)
        }
        fn records_since(
            &self,
            _i: u64,
            _s: u64,
            _l: PageLimit,
            _c: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            Err(SyncError::PeerUnavailable {
                member: self.0.member_id().to_string(),
                detail: "simulated dropped page mid-sync".to_string(),
            })
        }
        fn fetch_file(
            &self,
            p: &str,
            h: Hash,
            o: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            self.0.fetch_file(p, h, o)
        }
    }

    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let mk = |id: &str, members: &[&str], rng: &mut Rng| {
        let mut cfg = FolderConfig::new(
            "sim",
            id.to_string(),
            members.iter().map(|s| (*s).to_string()),
        );
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), rng.chance(1, 2)),
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };

    let sync_slice =
        |es: &mut [&mut SyncEngine<HonestSimFs, FaultyIndexStore>], t_base: i64, rng: &mut Rng| {
            let n = es.len();
            for round in 0..4i64 {
                let mut pairs: Vec<(usize, usize)> = Vec::new();
                for i in 0..n {
                    for j in 0..n {
                        if i != j {
                            pairs.push((i, j));
                        }
                    }
                }
                for k in (1..pairs.len()).rev() {
                    let s = rng.range(k + 1);
                    pairs.swap(k, s);
                }
                for (i, j) in pairs {
                    let now = Timestamp::new(t_base + round * 5, 0);
                    let (dst, src) = if i < j {
                        let (l, r) = es.split_at_mut(j);
                        (&mut l[i], &r[0])
                    } else {
                        let (l, r) = es.split_at_mut(i);
                        (&mut r[0], &l[j])
                    };
                    let dst_id = dst.meta().unwrap().local_member;
                    let src_id = src.meta().unwrap().local_member;
                    let _ = dst
                        .pull_from_peer(&SimPeer::new(src, &dst_id, &src_id, PeerFault::None), now);
                }
            }
        };

    match rng.range(4) {
        0 => {
            // Subcase 0: `pi` added on `nas` (and optionally `laptop`), unknown to `tower`;
            // concurrent edit + deletion + optional dropped page when `tower` finally learns `pi`.
            let three = ["tower", "nas", "laptop"];
            let four = ["tower", "nas", "laptop", "pi"];
            let mut tower = mk("tower", &three, &mut rng);
            let mut nas = mk("nas", &three, &mut rng);
            let mut laptop = mk("laptop", &three, &mut rng);

            let base_doc = format!("base-doc-{seed}").into_bytes();
            let base_del = format!("base-del-{seed}").into_bytes();
            tower
                .fs()
                .user_write_file("doc.txt", base_doc.clone(), hash_bytes(&base_doc), t0);
            tower
                .fs()
                .user_write_file("gone.txt", base_del.clone(), hash_bytes(&base_del), t0);
            tower.scan(t0).unwrap();
            sync_slice(
                &mut [&mut tower, &mut nas, &mut laptop],
                t0.secs + 1,
                &mut rng,
            );

            nas.add_member("pi").unwrap();
            let laptop_knows_pi = rng.chance(1, 2);
            if laptop_knows_pi {
                laptop.add_member("pi").unwrap();
            }
            let mut pi = mk("pi", &four, &mut rng);
            if laptop_knows_pi {
                sync_slice(
                    &mut [&mut nas, &mut laptop, &mut pi],
                    t0.secs + 30,
                    &mut rng,
                );
            } else {
                sync_slice(&mut [&mut nas, &mut pi], t0.secs + 30, &mut rng);
            }

            // `pi` edits `doc.txt` (or deletes `gone.txt` while `laptop` edits `doc.txt`), relayed via `nas`!
            let pi_edit = format!("pi-edit-{seed}").into_bytes();
            let pi_edit_h = hash_bytes(&pi_edit);
            let t1 = Timestamp::new(t0.secs + 80, 0);
            pi.fs().user_write_file("doc.txt", pi_edit, pi_edit_h, t1);
            pi.fs().user_remove_recursive("gone.txt");
            pi.scan(t1).unwrap();
            nas.pull_from_peer(&SimPeer::new(&pi, "nas", "pi", PeerFault::None), t1)
                .unwrap();

            // `tower` pulls from `nas` BEFORE knowing `pi` -> MUST hold back records naming `pi` unacked!
            let rep_before = tower
                .pull_from_peer(&SimPeer::new(&nas, "tower", "nas", PeerFault::None), t1)
                .unwrap();
            assert!(
                rep_before.held_back >= 1,
                "seed={root_seed}: tower must hold back records referencing unknown member pi"
            );

            // Optionally remove `laptop` before `tower` learns `pi`!
            let drop_laptop = rng.chance(1, 2);
            if drop_laptop {
                for e in [&mut tower, &mut nas, &mut pi] {
                    e.remove_member("laptop").unwrap();
                }
            } else if !laptop_knows_pi {
                laptop.add_member("pi").unwrap();
            }

            // `tower` learns `pi`; optionally suffer a dropped page right after handshake!
            tower.add_member("pi").unwrap();
            if rng.chance(1, 2) {
                let _ = pi.pull_from_peer(
                    &DropPagesPeer(SimPeer::new(&tower, "pi", "tower", PeerFault::None)),
                    Timestamp::new(t0.secs + 120, 0),
                );
            }

            if drop_laptop {
                sync_slice(
                    &mut [&mut tower, &mut nas, &mut pi],
                    t0.secs + 140,
                    &mut rng,
                );
                for e in [&tower, &nas, &pi] {
                    assert_eq!(
                        e.fs().live_file_hash("doc.txt"),
                        Some(pi_edit_h),
                        "seed={root_seed}: pi edit lost on {}",
                        e.meta().unwrap().local_member
                    );
                    assert_eq!(
                        e.fs().live_file_hash("gone.txt"),
                        None,
                        "seed={root_seed}: pi deletion resurrected on {}",
                        e.meta().unwrap().local_member
                    );
                    assert_eq!(e.store().held_back_count().unwrap(), 0);
                    assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
                }
            } else {
                sync_slice(
                    &mut [&mut tower, &mut nas, &mut laptop, &mut pi],
                    t0.secs + 140,
                    &mut rng,
                );
                for e in [&tower, &nas, &laptop, &pi] {
                    assert_eq!(e.fs().live_file_hash("doc.txt"), Some(pi_edit_h));
                    assert_eq!(e.fs().live_file_hash("gone.txt"), None);
                    assert_eq!(e.store().held_back_count().unwrap(), 0);
                    assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
                }
            }
        }
        1 => {
            // Subcase 1: `laptop` removed while offline, returns with unsynced edits AND deletions
            // after active members concurrently deleted one file (and GC'd tombstones) and edited another!
            let all = ["tower", "nas", "laptop"];
            let mut tower = mk("tower", &all, &mut rng);
            let mut nas = mk("nas", &all, &mut rng);
            let mut laptop = mk("laptop", &all, &mut rng);

            for name in ["keep_edit.txt", "del_by_active.txt", "del_by_offline.txt"] {
                let b = format!("v1-{name}-{seed}").into_bytes();
                tower
                    .fs()
                    .user_write_file(name, b.clone(), hash_bytes(&b), t0);
            }
            tower.scan(t0).unwrap();
            sync_slice(
                &mut [&mut tower, &mut nas, &mut laptop],
                t0.secs + 1,
                &mut rng,
            );

            // Offline `laptop` edits `keep_edit.txt` and `del_by_active.txt` (or leaves `del_by_active.txt` untouched)
            // and deletes `del_by_offline.txt`.
            let t1 = Timestamp::new(t0.secs + 50, 0);
            let lap_keep = format!("laptop-offline-keep-{seed}").into_bytes();
            let lap_keep_h = hash_bytes(&lap_keep);
            laptop
                .fs()
                .user_write_file("keep_edit.txt", lap_keep, lap_keep_h, t1);
            let laptop_also_edited_deleted = rng.chance(1, 2);
            let lap_del_h = if laptop_also_edited_deleted {
                let b = format!("laptop-saved-deleted-{seed}").into_bytes();
                let h = hash_bytes(&b);
                laptop.fs().user_write_file("del_by_active.txt", b, h, t1);
                Some(h)
            } else {
                None
            };
            laptop.fs().user_remove_recursive("del_by_offline.txt");
            laptop.scan(t1).unwrap();

            // Active members remove `laptop`, delete `del_by_active.txt`, edit `del_by_offline.txt`, and GC!
            tower.remove_member("laptop").unwrap();
            nas.remove_member("laptop").unwrap();
            let t2 = Timestamp::new(t0.secs + 60, 0);
            tower.fs().user_remove_recursive("del_by_active.txt");
            let active_saved = format!("tower-edited-offline-del-{seed}").into_bytes();
            let active_saved_h = hash_bytes(&active_saved);
            tower
                .fs()
                .user_write_file("del_by_offline.txt", active_saved, active_saved_h, t2);
            tower.scan(t2).unwrap();
            sync_slice(&mut [&mut tower, &mut nas], t0.secs + 70, &mut rng);

            // Re-add `laptop` on `tower` first, optionally pull in random direction, then add on `nas`!
            tower.add_member("laptop").unwrap();
            let t3 = Timestamp::new(t0.secs + 120, 0);
            if rng.chance(1, 2) {
                let _ = tower.pull_from_peer(
                    &SimPeer::new(&laptop, "tower", "laptop", PeerFault::None),
                    t3,
                );
                let _ = laptop.pull_from_peer(
                    &SimPeer::new(&tower, "laptop", "tower", PeerFault::None),
                    t3,
                );
            } else {
                let _ = laptop.pull_from_peer(
                    &SimPeer::new(&tower, "laptop", "tower", PeerFault::None),
                    t3,
                );
                let _ = tower.pull_from_peer(
                    &SimPeer::new(&laptop, "tower", "laptop", PeerFault::None),
                    t3,
                );
            }
            nas.add_member("laptop").unwrap();
            sync_slice(
                &mut [&mut tower, &mut nas, &mut laptop],
                t0.secs + 140,
                &mut rng,
            );

            for e in [&tower, &nas, &laptop] {
                let mid = e.meta().unwrap().local_member;
                assert_eq!(
                    e.fs().live_file_hash("keep_edit.txt"),
                    Some(lap_keep_h),
                    "seed={root_seed}: offline laptop edit lost on {mid}"
                );
                assert_eq!(
                    e.fs().live_file_hash("del_by_offline.txt"),
                    Some(active_saved_h),
                    "seed={root_seed}: delete-vs-edit must preserve tower edit on {mid}"
                );
                if let Some(h) = lap_del_h {
                    assert_eq!(
                        e.fs().live_file_hash("del_by_active.txt"),
                        Some(h),
                        "seed={root_seed}: delete-vs-edit must preserve offline laptop edit on {mid}"
                    );
                } else {
                    assert_eq!(
                        e.fs().live_file_hash("del_by_active.txt"),
                        None,
                        "seed={root_seed}: untouched deleted file resurrected on {mid}"
                    );
                }
                assert_eq!(e.store().held_back_count().unwrap(), 0);
                assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
            }
        }
        2 => {
            // Subcase 2: `p2b`-style relay edit + concurrent deletion + laptop removal +
            // dropped page on rejoin, randomized order!
            let three = ["tower", "nas", "laptop"];
            let four = ["tower", "nas", "laptop", "pi"];
            let mut tower = mk("tower", &three, &mut rng);
            let mut nas = mk("nas", &three, &mut rng);
            let mut laptop = mk("laptop", &three, &mut rng);

            let v1 = format!("v1-{seed}").into_bytes();
            tower
                .fs()
                .user_write_file("doc.txt", v1.clone(), hash_bytes(&v1), t0);
            tower.scan(t0).unwrap();
            sync_slice(
                &mut [&mut tower, &mut nas, &mut laptop],
                t0.secs + 1,
                &mut rng,
            );

            nas.add_member("pi").unwrap();
            laptop.add_member("pi").unwrap();
            let mut pi = mk("pi", &four, &mut rng);
            sync_slice(
                &mut [&mut nas, &mut laptop, &mut pi],
                t0.secs + 30,
                &mut rng,
            );

            let lap_v2 = format!("laptop-relay-{seed}").into_bytes();
            let lap_v2_h = hash_bytes(&lap_v2);
            let t1 = Timestamp::new(t0.secs + 60, 0);
            laptop.fs().user_write_file("doc.txt", lap_v2, lap_v2_h, t1);
            laptop.scan(t1).unwrap();
            pi.pull_from_peer(&SimPeer::new(&laptop, "pi", "laptop", PeerFault::None), t1)
                .unwrap();

            nas.fs().user_remove_recursive("doc.txt");
            nas.scan(Timestamp::new(t0.secs + 62, 0)).unwrap();
            drop(laptop);
            for e in [&mut tower, &mut nas, &mut pi] {
                e.remove_member("laptop").unwrap();
            }
            if rng.chance(1, 2) {
                sync_slice(&mut [&mut tower, &mut nas], t0.secs + 80, &mut rng);
            }
            tower.add_member("pi").unwrap();
            let t2 = Timestamp::new(t0.secs + 120, 0);
            nas.pull_from_peer(&SimPeer::new(&pi, "nas", "pi", PeerFault::None), t2)
                .unwrap();
            tower
                .pull_from_peer(&SimPeer::new(&pi, "tower", "pi", PeerFault::None), t2)
                .unwrap();
            let _ = pi.pull_from_peer(
                &DropPagesPeer(SimPeer::new(&tower, "pi", "tower", PeerFault::None)),
                t2,
            );
            sync_slice(
                &mut [&mut tower, &mut nas, &mut pi],
                t0.secs + 140,
                &mut rng,
            );

            for e in [&tower, &nas, &pi] {
                assert_eq!(
                    e.fs().live_file_hash("doc.txt"),
                    Some(lap_v2_h),
                    "seed={root_seed}: relayed laptop edit lost on {}",
                    e.meta().unwrap().local_member
                );
                assert_eq!(e.store().held_back_count().unwrap(), 0);
                assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
            }
        }
        3 => {
            // Subcase 3 (`review5::s5b`): relay with `local_clock == 0` pulls a file from a third
            // member, removes that third member (leaving 2 members), and pulls from the remaining
            // peer: status MUST report `WaitingForMember`, NEVER `UpToDate`, until the peer pulls!
            let three = ["tower", "pi", "laptop"];
            let mut tower = mk("tower", &three, &mut rng);
            let mut pi = mk("pi", &three, &mut rng);
            let mut laptop = mk("laptop", &three, &mut rng);

            let p_body = format!("p-{seed}").into_bytes();
            pi.fs()
                .user_write_file("p.txt", p_body.clone(), hash_bytes(&p_body), t0);
            pi.scan(t0).unwrap();
            sync_slice(
                &mut [&mut tower, &mut pi, &mut laptop],
                t0.secs + 1,
                &mut rng,
            );

            let lap_body = format!("relayed-{seed}").into_bytes();
            let lap_h = hash_bytes(&lap_body);
            let t1 = Timestamp::new(t0.secs + 20, 0);
            laptop.fs().user_write_file("doc.txt", lap_body, lap_h, t1);
            laptop.scan(t1).unwrap();
            tower
                .pull_from_peer(
                    &SimPeer::new(&laptop, "tower", "laptop", PeerFault::None),
                    t1,
                )
                .unwrap();
            drop(laptop);
            tower.remove_member("laptop").unwrap();
            pi.remove_member("laptop").unwrap();
            tower
                .pull_from_peer(
                    &SimPeer::new(&pi, "tower", "pi", PeerFault::None),
                    Timestamp::new(t0.secs + 25, 0),
                )
                .unwrap();
            assert_eq!(tower.meta().unwrap().local_clock, 0);
            assert_eq!(tower.meta().unwrap().members.len(), 2);
            assert_eq!(pi.fs().live_file_hash("doc.txt"), None);
            assert_eq!(
                tower.status().unwrap().state,
                MemberState::WaitingForMember {
                    members: vec!["pi".to_string()],
                },
                "seed={root_seed}: 2-member relay with local_clock=0 must report WaitingForMember, not UpToDate"
            );

            // Once pi pulls from tower (and tower hears pi's ack), both converge to UpToDate.
            sync_slice(&mut [&mut tower, &mut pi], t0.secs + 30, &mut rng);
            assert_eq!(pi.fs().live_file_hash("doc.txt"), Some(lap_h));
            assert_eq!(tower.status().unwrap().state, MemberState::UpToDate);
            assert_eq!(pi.status().unwrap().state, MemberState::UpToDate);
        }
        _ => unreachable!(),
    }
}

/// Scenario 22: Joiners carrying old pre-deletion copies arriving AFTER the surviving members
/// underwent either in-place schema migrations / reinstalls (all members, history kept) OR a
/// single-member destroyed-index rebuild (true join), plus optional intentional post-deletion
/// re-creation of another deleted file (`p1`, `r6_p1`, Decisions 1-4).
fn run_joiner_after_deleter_rebuild_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let has_nas = rng.chance(1, 2);
    let init_members: Vec<&str> = if has_nas {
        vec!["tower", "pi", "nas"]
    } else {
        vec!["tower", "pi"]
    };
    let mut tokens: Vec<MemoryInstanceTokenStore> = (0..init_members.len())
        .map(|_| MemoryInstanceTokenStore::new())
        .collect();
    let mk = |id: &str, members: &[&str], tok: MemoryInstanceTokenStore, rng: &mut Rng| {
        let mut cfg = FolderConfig::new(
            "sim",
            id.to_string(),
            members.iter().map(|s| (*s).to_string()),
        );
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), rng.chance(1, 2)),
            FaultyIndexStore::new(),
            tok,
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };

    let sync_vec =
        |es: &mut [SyncEngine<HonestSimFs, FaultyIndexStore>], t_base: i64, rng: &mut Rng| {
            let n = es.len();
            for round in 0..4i64 {
                let mut pairs = Vec::new();
                for i in 0..n {
                    for j in 0..n {
                        if i != j {
                            pairs.push((i, j));
                        }
                    }
                }
                for k in (1..pairs.len()).rev() {
                    let s = rng.range(k + 1);
                    pairs.swap(k, s);
                }
                for (i, j) in pairs {
                    let now = Timestamp::new(t_base + round * 5, 0);
                    let (dst, src) = if i < j {
                        let (l, r) = es.split_at_mut(j);
                        (&mut l[i], &r[0])
                    } else {
                        let (l, r) = es.split_at_mut(i);
                        (&mut r[0], &l[j])
                    };
                    let dst_id = dst.meta().unwrap().local_member;
                    let src_id = src.meta().unwrap().local_member;
                    dst.pull_from_peer(&SimPeer::new(src, &dst_id, &src_id, PeerFault::None), now)
                        .unwrap();
                }
            }
        };

    let mut active: Vec<SyncEngine<HonestSimFs, FaultyIndexStore>> = init_members
        .iter()
        .enumerate()
        .map(|(i, id)| mk(id, &init_members, tokens[i].clone(), &mut rng))
        .collect();

    // Seed initial files: `keep.txt`, `tax1.pdf`, `tax2.pdf`, `recreate.txt`.
    let keep_b = format!("keep-{seed}").into_bytes();
    let keep_h = hash_bytes(&keep_b);
    let tax1_b = format!("tax1-deleted-{seed}").into_bytes();
    let tax2_b = format!("tax2-deleted-{seed}").into_bytes();
    let rec_v1_b = format!("recreate-v1-{seed}").into_bytes();
    let rec_v1_h = hash_bytes(&rec_v1_b);

    active[0]
        .fs()
        .user_write_file("keep.txt", keep_b, keep_h, t0);
    active[1]
        .fs()
        .user_write_file("tax1.pdf", tax1_b.clone(), hash_bytes(&tax1_b), t0);
    active[1]
        .fs()
        .user_write_file("dir/tax2.pdf", tax2_b.clone(), hash_bytes(&tax2_b), t0);
    active[1]
        .fs()
        .user_write_file("recreate.txt", rec_v1_b.clone(), rec_v1_h, t0);
    active[0].scan(t0).unwrap();
    active[1].scan(t0).unwrap();
    sync_vec(&mut active, t0.secs + 1, &mut rng);

    // Snapshot pre-deletion disk copy for future joiner(s).
    let old_copy_snap = active[0].fs().snapshot();

    // Deleter (`pi`, index 1) deletes `tax1.pdf`, `dir/tax2.pdf`, and optionally `recreate.txt`,
    // then syncs to all active members.
    let t1 = Timestamp::new(t0.secs + 50, 0);
    active[1].fs().user_remove_recursive("tax1.pdf");
    active[1].fs().user_remove_recursive("dir/tax2.pdf");
    let do_recreate = rng.chance(1, 2);
    if do_recreate {
        active[1].fs().user_remove_recursive("recreate.txt");
    }
    active[1].scan(t1).unwrap();
    sync_vec(&mut active, t1.secs + 1, &mut rng);

    // Optional intentional post-deletion re-creation of `recreate.txt` (half with identical old
    // bytes, half with brand-new bytes!) BEFORE the migrations/rebuilds too on half the seeds,
    // exercising Review 6 Probe 1 directly!
    let recreate_before_upgrades = do_recreate && rng.chance(1, 2);
    let mut expected_recreate_h = Some(rec_v1_h);
    if recreate_before_upgrades {
        let trc = Timestamp::new(t0.secs + 80, 0);
        let (bytes, h) = if rng.chance(1, 2) {
            (rec_v1_b.clone(), rec_v1_h)
        } else {
            let nb = format!("recreate-new-{seed}").into_bytes();
            let nh = hash_bytes(&nb);
            (nb, nh)
        };
        active[0]
            .fs()
            .user_write_file("recreate.txt", bytes, h, trc);
        active[0].scan(trc).unwrap();
        sync_vec(&mut active, trc.secs + 1, &mut rng);
        expected_recreate_h = Some(h);
    }

    // Exercise BOTH kinds across 1..=4 rounds:
    // - Even seeds: EVERY surviving member migrates schema in place or loses token (history kept!).
    // - Odd seeds: one member destroys `.legion-sync/index.redb` and rebuilds from disk (true join)
    //   while the remaining member(s) migrate in place.
    let rebuilds = 1 + rng.range(4);
    let destroy_one_store = (seed & 1) == 1;
    let mut latest_keep_h = keep_h;
    for r in 0..rebuilds {
        let tr = Timestamp::new(t0.secs + 100 + (r as i64) * 40, 0);
        let victim_idx = r % active.len();
        for (idx, e) in active.iter_mut().enumerate() {
            let meta = e.meta().unwrap();
            let fs_snap = e.fs().snapshot();
            let store_snap = e.store().snapshot();
            let fresh_fs = HonestSimFs::from_snapshot(fs_snap);
            let mut rcfg = FolderConfig::new(
                meta.folder_id.clone(),
                meta.local_member.clone(),
                meta.members.iter().cloned(),
            );
            rcfg.former_members = meta.former_members.clone();
            if destroy_one_store && idx == victim_idx {
                let mut rebuilt =
                    SyncEngine::open(rcfg, fresh_fs, FaultyIndexStore::new(), tokens[idx].clone())
                        .unwrap();
                assert!(
                    rebuilt.is_index_damaged(),
                    "seed={root_seed}: missing store beside existing marker must stop as damaged before rebuild"
                );
                rebuilt.set_deletion_brake(brake_disabled());
                rebuilt.rebuild_from_disk(tr).unwrap();
                *e = rebuilt;
            } else if rng.chance(1, 2) {
                // In-place schema migration (same token, same incarnation, history kept!).
                let mut store = FaultyIndexStore::from_inner(store_snap);
                store.set_schema_version_for_test(MIN_MIGRATABLE_SCHEMA_VERSION);
                let mut migrated =
                    SyncEngine::open(rcfg, fresh_fs, store, tokens[idx].clone()).unwrap();
                assert!(!migrated.is_index_damaged());
                migrated.set_deletion_brake(brake_disabled());
                *e = migrated;
            } else {
                // Reinstall (token lost, store kept): rotates incarnation while keeping history!
                let new_tok = MemoryInstanceTokenStore::new();
                let mut rotated = SyncEngine::open(
                    rcfg,
                    fresh_fs,
                    FaultyIndexStore::from_inner(store_snap),
                    new_tok.clone(),
                )
                .unwrap();
                assert!(!rotated.is_index_damaged());
                rotated.set_deletion_brake(brake_disabled());
                tokens[idx] = new_tok;
                *e = rotated;
            }
        }
        sync_vec(&mut active, tr.secs + 1, &mut rng);
        let edited = format!("keep-after-rebuild-{r}-{seed}").into_bytes();
        latest_keep_h = hash_bytes(&edited);
        let editor_idx = rng.range(active.len());
        active[editor_idx].fs().user_write_file(
            "keep.txt",
            edited,
            latest_keep_h,
            Timestamp::new(tr.secs + 20, 0),
        );
        active[editor_idx]
            .scan(Timestamp::new(tr.secs + 20, 0))
            .unwrap();
        sync_vec(&mut active, tr.secs + 21, &mut rng);
    }

    if do_recreate && !recreate_before_upgrades {
        let trc = Timestamp::new(t0.secs + 350, 0);
        let (bytes, h) = if rng.chance(1, 2) {
            (rec_v1_b, rec_v1_h)
        } else {
            let nb = format!("recreate-new-{seed}").into_bytes();
            let nh = hash_bytes(&nb);
            (nb, nh)
        };
        active[0]
            .fs()
            .user_write_file("recreate.txt", bytes, h, trc);
        active[0].scan(trc).unwrap();
        sync_vec(&mut active, trc.secs + 1, &mut rng);
        expected_recreate_h = Some(h);
    }

    // Now 1 or 2 joiners arrive carrying the pre-deletion old copy on disk (`join_mode = true`).
    let joiner_count = 1 + rng.range(2);
    let joiner_names = ["laptop", "studio"];
    let mut final_members = init_members.clone();
    for (j, &jid) in joiner_names.iter().enumerate().take(joiner_count) {
        final_members.push(jid);
        for e in &mut active {
            e.add_member(jid).unwrap();
        }
        let jfs = HonestSimFs::from_snapshot(old_copy_snap.clone());
        *jfs.marker.borrow_mut() = None;
        let mut jcfg = FolderConfig::new(
            "sim",
            jid.to_string(),
            final_members.iter().map(|s| (*s).to_string()),
        );
        jcfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut je = SyncEngine::open(
            jcfg,
            jfs,
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        je.set_deletion_brake(brake_disabled());
        je.scan(Timestamp::new(t0.secs + 400 + (j as i64) * 5, 0))
            .unwrap();
        active.push(je);
    }

    // Sync in randomized order (exercising joiner pulling first, deleter pulling first, and
    // non-deleter pulling first!).
    sync_vec(&mut active, t0.secs + 450, &mut rng);

    for e in &active {
        let mid = e.meta().unwrap().local_member;
        assert_eq!(
            e.fs().live_file_hash("tax1.pdf"),
            None,
            "seed={root_seed}: tax1.pdf resurrected on {mid} after {rebuilds} rounds"
        );
        assert_eq!(
            e.fs().live_file_hash("dir/tax2.pdf"),
            None,
            "seed={root_seed}: dir/tax2.pdf resurrected on {mid} after {rebuilds} rounds"
        );
        assert_eq!(
            e.fs().live_file_hash("keep.txt"),
            Some(latest_keep_h),
            "seed={root_seed}: keep.txt mismatch on {mid}"
        );
        assert_eq!(
            e.fs().live_file_hash("recreate.txt"),
            expected_recreate_h,
            "seed={root_seed}: recreate.txt mismatch on {mid}"
        );
        assert_eq!(e.store().held_back_count().unwrap(), 0);
        assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
    }
}

fn cached_real_fs_durability_audit() -> &'static legion_sync::RealFsDurabilityAudit {
    static AUDIT: std::sync::OnceLock<legion_sync::RealFsDurabilityAudit> =
        std::sync::OnceLock::new();
    AUDIT.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("legion-sync-real-fs-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let res = legion_sync::audit_real_fs_durability(&dir)
            .expect("RealFs durability audit must succeed");
        let _ = std::fs::remove_dir_all(&dir);
        res
    })
}

/// Scenario 23: Power-loss disk mode crash recovery — wired directly to the live [`RealFs`]
/// durability audit so dropping a `sync_dir` call in `fs_real.rs` fails this generator at seed 0,
/// and exercising partial-persistence power cuts across unsynced writes, deferred group-commit
/// renames, nested ancestor directories, `remove_tmp`, bin restores, and recovery itself.
fn run_power_loss_crash_recovery_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let audit = cached_real_fs_durability_audit();
    assert!(
        audit.unsynced_ops.is_empty(),
        "seed={root_seed}: RealFs durability audit found unsynced directory mutations: {:?}",
        audit.unsynced_ops
    );
    let sync_created_ancestors = audit.sync_created_ancestors;
    let sync_remove_tmp = audit.sync_remove_tmp;

    let mut mk = |id: &str, supports_exchange: bool, pl_seed: u64| {
        let mut cfg = FolderConfig::new("sim", id.to_string(), ["m0".into(), "m1".into()]);
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let fs = HonestSimFs::new(FsCapabilities::unix(), supports_exchange);
        fs.enable_power_loss_mode(pl_seed, sync_created_ancestors, sync_remove_tmp);
        let mut e = SyncEngine::open(
            cfg,
            fs,
            FaultyIndexStore::new(),
            MemoryInstanceTokenStore::new(),
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };

    let mut m0 = mk("m0", true, seed ^ 0x1111);
    let mut m1 = mk("m1", (seed & 1) == 0, seed ^ 0x2222);

    // Phase 1: Batch of brand-new nested files (exercises `apply_new_files_group_commit`,
    // `commit_tmp_to_live_no_replace_deferred_dir_sync`, and nested ancestor dir creation!).
    let mut expected: BTreeMap<String, Hash> = BTreeMap::new();
    let paths = [
        "deep/sub/a.txt",
        "deep/sub/b.txt",
        "deep/other/c.txt",
        "top.txt",
        "replace_me.txt",
    ];
    for p in paths {
        let b = format!("v1-{p}-{seed}-payload-for-tearing-check").into_bytes();
        let h = hash_bytes(&b);
        m0.fs().user_write_file(p, b, h, t0);
        expected.insert(p.to_string(), h);
    }
    m0.scan(t0).unwrap();

    // Inject power-loss crashes at random points during initial batch pull (including during
    // `finish_sync` -> torn tmp write, and during deferred rename -> reverted dir entries), plus
    // power loss during `recover()` itself!
    for attempt in 0..3 {
        let op = rng.range(10);
        let after = rng.chance(1, 2);
        m1.fs().arm_crash(op, after);
        let _ = m1.pull_from_peer(
            &SimPeer::new(&m0, "m1", "m0", PeerFault::None),
            Timestamp::new(t0.secs + 1 + attempt, 0),
        );
        m1.fs().disarm_fault();
        m1.fs().apply_pending_power_loss();
        if rng.chance(1, 2) {
            m1.fs().arm_crash(rng.range(3), rng.chance(1, 2));
            let _ = m1.recover();
            m1.fs().disarm_fault();
            m1.fs().apply_pending_power_loss();
        }
        m1.recover().unwrap();
    }

    // Complete pull cleanly, then IMMEDIATELY cut power right after commit (verifies EVERY
    // committed index entry and every newly created ancestor directory is actually durable!).
    let t1 = Timestamp::new(t0.secs + 20, 0);
    m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), t1)
        .unwrap();
    m1.fs().simulate_power_loss();
    m1.recover().unwrap();
    m1.scan(Timestamp::new(t1.secs + 1, 0)).unwrap();

    // Phase 2: Replace existing file, delete a file, AND trigger a failed mid-fetch drop
    // (`remove_tmp` + `unregister_staged_tmp`) followed immediately by power loss!
    let t2 = Timestamp::new(t0.secs + 40, 0);
    let rep_v2 = format!("v2-replace-{seed}-longer-payload-bytes").into_bytes();
    let rep_v2_h = hash_bytes(&rep_v2);
    m0.fs()
        .user_write_file("replace_me.txt", rep_v2, rep_v2_h, t2);
    expected.insert("replace_me.txt".to_string(), rep_v2_h);
    m0.fs().user_remove_recursive("deep/other/c.txt");
    expected.remove("deep/other/c.txt");
    let new_drop = format!("drop-fetch-payload-{seed}-0123456789abcdef").into_bytes();
    let new_drop_h = hash_bytes(&new_drop);
    m0.fs()
        .user_write_file("deep/dropped.txt", new_drop, new_drop_h, t2);
    expected.insert("deep/dropped.txt".to_string(), new_drop_h);
    m0.scan(t2).unwrap();

    // Failed fetch (`DropAlways` calls `finish_sync` on resume attempts, then `remove_tmp` and
    // `unregister_staged_tmp`), immediately followed by power loss!
    let _ = m1.pull_from_peer(
        &SimPeer::new(&m0, "m1", "m0", PeerFault::DropAlways),
        Timestamp::new(t2.secs + 1, 0),
    );
    m1.fs().simulate_power_loss();
    m1.recover().unwrap();
    m1.scan(Timestamp::new(t2.secs + 2, 0)).unwrap();

    // Power-loss crash during replace/delete apply too!
    m1.fs().arm_crash(rng.range(6), rng.chance(1, 2));
    let _ = m1.pull_from_peer(
        &SimPeer::new(&m0, "m1", "m0", PeerFault::None),
        Timestamp::new(t2.secs + 3, 0),
    );
    m1.fs().disarm_fault();
    m1.fs().apply_pending_power_loss();
    m1.recover().unwrap();

    // Final convergence without crashes, followed by restoring a binned file into a deleted nested
    // directory (`deep/other/c.txt`, where `deep/other` was deleted on disk) and immediately
    // cutting power right after `restore_from_bin` removes the bin copy!
    for r in 0..3i64 {
        let tn = Timestamp::new(t2.secs + 10 + r * 2, 0);
        m1.scan(tn).unwrap();
        m0.scan(tn).unwrap();
        m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), tn)
            .unwrap();
        m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), tn)
            .unwrap();
    }
    // Remove `deep/other` dir on m0 and sync so `deep/other` does not exist on m1 when restoring!
    m0.fs().user_remove_recursive("deep/other");
    m0.scan(Timestamp::new(t2.secs + 20, 0)).unwrap();
    m1.pull_from_peer(
        &SimPeer::new(&m0, "m1", "m0", PeerFault::None),
        Timestamp::new(t2.secs + 21, 0),
    )
    .unwrap();
    if let Some(binned_c) = m1
        .list_bin()
        .unwrap()
        .into_iter()
        .find(|b| b.original_path == "deep/other/c.txt")
    {
        let c_hash = binned_c.hash.unwrap();
        let restored = m1
            .restore_from_bin(
                &binned_c.id,
                None,
                RestoreStrategy::Rename,
                Timestamp::new(t2.secs + 22, 0),
            )
            .unwrap();
        assert_eq!(restored, "deep/other/c.txt");
        expected.insert("deep/other/c.txt".to_string(), c_hash);
        // Immediate power loss right after `restore_from_bin` deletes the bin copy!
        m1.fs().simulate_power_loss();
        m1.recover().unwrap();
        m1.scan(Timestamp::new(t2.secs + 23, 0)).unwrap();
        for r in 0..2i64 {
            let tn = Timestamp::new(t2.secs + 24 + r * 2, 0);
            m0.pull_from_peer(&SimPeer::new(&m1, "m0", "m1", PeerFault::None), tn)
                .unwrap();
            m1.pull_from_peer(&SimPeer::new(&m0, "m1", "m0", PeerFault::None), tn)
                .unwrap();
        }
    }
    m1.fs().simulate_power_loss();
    m1.recover().unwrap();
    m1.scan(Timestamp::new(t2.secs + 30, 0)).unwrap();

    // Verify ZERO data loss, ZERO corrupt torn files promoted to live tree, and EXACT convergence!
    assert!(
        m1.fs().power_loss_count() >= 3,
        "seed={root_seed}: power-loss mode must have fired"
    );
    assert!(
        m1.fs().list_tmp().unwrap().is_empty(),
        "seed={root_seed}: leftover tmp files after power-loss recovery"
    );
    let ignore = IgnoreMatcher::default();
    let t_m0 = m0.fs().live_tree(&ignore);
    let t_m1 = m1.fs().live_tree(&ignore);
    assert_eq!(
        t_m0, t_m1,
        "seed={root_seed}: members diverged after power-loss recovery\n  m0={t_m0:?}\n  m1={t_m1:?}"
    );
    for (p, h) in &expected {
        assert_eq!(
            m1.fs().live_file_hash(p),
            Some(*h),
            "seed={root_seed}: file {p:?} lost or corrupted after power loss"
        );
    }
    for path in t_m1.keys() {
        assert!(
            !path.starts_with("recovered-file"),
            "seed={root_seed}: torn/removed engine tmp was wrongly rescued as live file {path:?}"
        );
    }
    assert_eq!(m0.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(m1.status().unwrap().state, MemberState::UpToDate);
}

/// Scenario 24: Reinstall / machine-local instance token loss (Decision 2) while peers are
/// partitioned and BOTH sides hold unsynced edits, deletions, new files, and a concurrent edit.
/// Verifies incarnation rotates cleanly while preserving all history (never entering join mode),
/// zero lost saves, zero resurrected deletions, and clean convergence.
fn run_token_loss_reinstall_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let three = (seed & 1) == 1;
    let names: Vec<&str> = if three {
        vec!["m0", "m1", "m2"]
    } else {
        vec!["m0", "m1"]
    };
    let mut tokens: Vec<MemoryInstanceTokenStore> = (0..names.len())
        .map(|_| MemoryInstanceTokenStore::new())
        .collect();
    let mut es: Vec<SyncEngine<HonestSimFs, FaultyIndexStore>> = names
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut cfg = FolderConfig::new(
                "sim",
                (*id).to_string(),
                names.iter().map(|s| (*s).to_string()),
            );
            cfg.initial_incarnation = Some(rng.next_u64() | 1);
            let mut e = SyncEngine::open(
                cfg,
                HonestSimFs::new(FsCapabilities::unix(), rng.chance(1, 2)),
                FaultyIndexStore::new(),
                tokens[i].clone(),
            )
            .unwrap();
            e.set_deletion_brake(brake_disabled());
            e
        })
        .collect();

    let sync_all =
        |es: &mut [SyncEngine<HonestSimFs, FaultyIndexStore>], t_base: i64, rng: &mut Rng| {
            let n = es.len();
            for round in 0..4i64 {
                let mut pairs = Vec::new();
                for i in 0..n {
                    for j in 0..n {
                        if i != j {
                            pairs.push((i, j));
                        }
                    }
                }
                for k in (1..pairs.len()).rev() {
                    let s = rng.range(k + 1);
                    pairs.swap(k, s);
                }
                for (i, j) in pairs {
                    let now = Timestamp::new(t_base + round * 3, 0);
                    let (dst, src) = if i < j {
                        let (l, r) = es.split_at_mut(j);
                        (&mut l[i], &r[0])
                    } else {
                        let (l, r) = es.split_at_mut(i);
                        (&mut r[0], &l[j])
                    };
                    let d_id = dst.meta().unwrap().local_member;
                    let s_id = src.meta().unwrap().local_member;
                    dst.pull_from_peer(&SimPeer::new(src, &d_id, &s_id, PeerFault::None), now)
                        .unwrap();
                }
            }
        };

    es[0].scan(t0).unwrap();
    let t1 = Timestamp::new(t0.secs + 5, 0);
    for name in [
        "edit_m0.txt",
        "edit_m1.txt",
        "del_m0.txt",
        "del_m1.txt",
        "both.txt",
        "stay.txt",
    ] {
        let b = format!("base-{name}-{seed}").into_bytes();
        es[0]
            .fs()
            .user_write_file(name, b.clone(), hash_bytes(&b), t1);
    }
    let stay_h = es[0].fs().live_file_hash("stay.txt").unwrap();
    es[0].scan(t1).unwrap();
    sync_all(&mut es, t1.secs + 1, &mut rng);

    let t2 = Timestamp::new(t0.secs + 50, 0);
    let m0_edit = format!("m0-edit-{seed}").into_bytes();
    let m0_edit_h = hash_bytes(&m0_edit);
    let m0_both = format!("m0-both-{seed}").into_bytes();
    let m0_both_h = hash_bytes(&m0_both);
    let m0_new = format!("m0-new-{seed}").into_bytes();
    let m0_new_h = hash_bytes(&m0_new);
    es[0]
        .fs()
        .user_write_file("edit_m0.txt", m0_edit, m0_edit_h, t2);
    es[0]
        .fs()
        .user_write_file("both.txt", m0_both, m0_both_h, t2);
    es[0]
        .fs()
        .user_write_file("m0_new.txt", m0_new, m0_new_h, t2);
    es[0].fs().user_remove_recursive("del_m0.txt");
    es[0].scan(t2).unwrap();

    let m1_edit = format!("m1-edit-{seed}").into_bytes();
    let m1_edit_h = hash_bytes(&m1_edit);
    let m1_both = format!("m1-both-{seed}").into_bytes();
    let m1_both_h = hash_bytes(&m1_both);
    let m1_new = format!("m1-new-{seed}").into_bytes();
    let m1_new_h = hash_bytes(&m1_new);
    es[1]
        .fs()
        .user_write_file("edit_m1.txt", m1_edit, m1_edit_h, t2);
    es[1]
        .fs()
        .user_write_file("both.txt", m1_both, m1_both_h, t2);
    es[1]
        .fs()
        .user_write_file("m1_new.txt", m1_new, m1_new_h, t2);
    es[1].fs().user_remove_recursive("del_m1.txt");
    es[1].scan(t2).unwrap();

    // Reinstall m1 (and m0 on half the seeds): wipe machine-local instance token while keeping
    // the folder and its internal store intact.
    let victims: &[usize] = if rng.chance(1, 2) { &[1] } else { &[0, 1] };
    for &idx in victims {
        let old_inc = es[idx].meta().unwrap().incarnation;
        let fs_snap = es[idx].fs().snapshot();
        let store_snap = es[idx].store().snapshot();
        let fresh_tok = MemoryInstanceTokenStore::new();
        let cfg = FolderConfig::new(
            "sim",
            names[idx].to_string(),
            names.iter().map(|s| (*s).to_string()),
        );
        let mut reopened = SyncEngine::open(
            cfg,
            HonestSimFs::from_snapshot(fs_snap),
            FaultyIndexStore::from_inner(store_snap),
            fresh_tok.clone(),
        )
        .unwrap();
        reopened.set_deletion_brake(brake_disabled());
        assert!(!reopened.is_index_damaged());
        assert_ne!(
            reopened.meta().unwrap().incarnation,
            old_inc,
            "seed={root_seed}: token loss on {} must rotate incarnation",
            names[idx]
        );
        assert!(
            !reopened.meta().unwrap().join_mode,
            "seed={root_seed}: token loss rotation must keep history and never enter join mode"
        );
        tokens[idx] = fresh_tok;
        es[idx] = reopened;
    }

    sync_all(&mut es, t0.secs + 100, &mut rng);

    for (idx, e) in es.iter().enumerate() {
        let id = names[idx];
        assert_eq!(
            e.fs().live_file_hash("stay.txt"),
            Some(stay_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("edit_m0.txt"),
            Some(m0_edit_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("edit_m1.txt"),
            Some(m1_edit_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("m0_new.txt"),
            Some(m0_new_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("m1_new.txt"),
            Some(m1_new_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("del_m0.txt"),
            None,
            "seed={root_seed}: del_m0.txt resurrected on {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("del_m1.txt"),
            None,
            "seed={root_seed}: del_m1.txt resurrected on {id}"
        );
        let all_live = e.fs().all_live_hashes();
        assert!(
            all_live.contains(&m0_both_h) && all_live.contains(&m1_both_h),
            "seed={root_seed}: concurrent edit on both.txt lost on {id}"
        );
        assert_eq!(e.store().held_back_count().unwrap(), 0);
        assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
    }
    let _ = &tokens;
}

/// Scenario 25: Copy a folder TOGETHER with its `.legion-sync` store to a second member and run
/// both (Decision 2). The copied member has no matching machine token, so open rotates its
/// incarnation and re-binds its member name while keeping all history (including past deletions)
/// without colliding with the source member.
fn run_folder_copy_with_store_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let init_members = ["m0", "m2"];
    let tok0 = MemoryInstanceTokenStore::new();
    let tok2 = MemoryInstanceTokenStore::new();
    let mut mk = |id: &str, members: &[&str], tok: MemoryInstanceTokenStore| {
        let mut cfg = FolderConfig::new(
            "sim",
            id.to_string(),
            members.iter().map(|s| (*s).to_string()),
        );
        cfg.initial_incarnation = Some(rng.next_u64() | 1);
        let mut e = SyncEngine::open(
            cfg,
            HonestSimFs::new(FsCapabilities::unix(), rng.chance(1, 2)),
            FaultyIndexStore::new(),
            tok,
        )
        .unwrap();
        e.set_deletion_brake(brake_disabled());
        e
    };
    let mut m0 = mk("m0", &init_members, tok0);
    let mut m2 = mk("m2", &init_members, tok2);

    m0.scan(t0).unwrap();
    let t1 = Timestamp::new(t0.secs + 5, 0);
    for name in [
        "keep.txt",
        "edit_m0.txt",
        "edit_m1.txt",
        "del_m0.txt",
        "del_m1.txt",
        "both.txt",
        "already_gone.txt",
    ] {
        let b = format!("copy-base-{name}-{seed}").into_bytes();
        m0.fs().user_write_file(name, b.clone(), hash_bytes(&b), t1);
    }
    let keep_h = m0.fs().live_file_hash("keep.txt").unwrap();
    m0.scan(t1).unwrap();
    m2.pull_from_peer(&SimPeer::new(&m0, "m2", "m0", PeerFault::None), t1)
        .unwrap();

    let t2 = Timestamp::new(t0.secs + 20, 0);
    m0.fs().user_remove_recursive("already_gone.txt");
    m0.scan(t2).unwrap();
    m2.pull_from_peer(&SimPeer::new(&m0, "m2", "m0", PeerFault::None), t2)
        .unwrap();
    m0.pull_from_peer(&SimPeer::new(&m2, "m0", "m2", PeerFault::None), t2)
        .unwrap();

    // Copy m0's ENTIRE folder (live files + marker + `.legion-sync/index.redb`) to brand-new
    // member `m1` on another machine (distinct empty token store!).
    let m0_inc = m0.meta().unwrap().incarnation;
    let copied_fs = HonestSimFs::from_snapshot(m0.fs().snapshot());
    let copied_store = FaultyIndexStore::from_inner(m0.store().snapshot());
    m0.add_member("m1").unwrap();
    m2.add_member("m1").unwrap();

    let cfg1 = FolderConfig::new(
        "sim",
        "m1".to_string(),
        ["m0".into(), "m1".into(), "m2".into()],
    );
    let mut m1 = SyncEngine::open(
        cfg1,
        copied_fs,
        copied_store,
        MemoryInstanceTokenStore::new(),
    )
    .unwrap();
    m1.set_deletion_brake(brake_disabled());
    assert!(!m1.is_index_damaged());
    assert_eq!(m1.meta().unwrap().local_member, "m1");
    assert_ne!(
        m1.meta().unwrap().incarnation,
        m0_inc,
        "seed={root_seed}: copied folder opened on m1 must rotate away from m0's incarnation"
    );
    assert!(
        m1.store()
            .get_record("already_gone.txt")
            .unwrap()
            .is_some_and(|r| r.deleted),
        "seed={root_seed}: copied folder must retain deletion history after token-mismatch rotation"
    );

    let t3 = Timestamp::new(t0.secs + 60, 0);
    let m0_edit = format!("m0-after-copy-{seed}").into_bytes();
    let m0_edit_h = hash_bytes(&m0_edit);
    let m0_both = format!("m0-both-copy-{seed}").into_bytes();
    let m0_both_h = hash_bytes(&m0_both);
    m0.fs()
        .user_write_file("edit_m0.txt", m0_edit, m0_edit_h, t3);
    m0.fs().user_write_file("both.txt", m0_both, m0_both_h, t3);
    m0.fs().user_remove_recursive("del_m0.txt");
    m0.scan(t3).unwrap();

    let m1_edit = format!("m1-after-copy-{seed}").into_bytes();
    let m1_edit_h = hash_bytes(&m1_edit);
    let m1_both = format!("m1-both-copy-{seed}").into_bytes();
    let m1_both_h = hash_bytes(&m1_both);
    m1.fs()
        .user_write_file("edit_m1.txt", m1_edit, m1_edit_h, t3);
    m1.fs().user_write_file("both.txt", m1_both, m1_both_h, t3);
    m1.fs().user_remove_recursive("del_m1.txt");
    m1.scan(t3).unwrap();

    let mut es = [m0, m1, m2];
    for round in 0..4i64 {
        let mut pairs = vec![(0, 1), (1, 0), (0, 2), (2, 0), (1, 2), (2, 1)];
        for k in (1..pairs.len()).rev() {
            let s = rng.range(k + 1);
            pairs.swap(k, s);
        }
        for (i, j) in pairs {
            let now = Timestamp::new(t3.secs + 10 + round * 3, 0);
            let (dst, src) = if i < j {
                let (l, r) = es.split_at_mut(j);
                (&mut l[i], &r[0])
            } else {
                let (l, r) = es.split_at_mut(i);
                (&mut r[0], &l[j])
            };
            let d_id = dst.meta().unwrap().local_member;
            let s_id = src.meta().unwrap().local_member;
            dst.pull_from_peer(&SimPeer::new(src, &d_id, &s_id, PeerFault::None), now)
                .unwrap();
        }
    }

    for e in &es {
        let id = e.meta().unwrap().local_member;
        assert_eq!(
            e.fs().live_file_hash("keep.txt"),
            Some(keep_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("edit_m0.txt"),
            Some(m0_edit_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("edit_m1.txt"),
            Some(m1_edit_h),
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("del_m0.txt"),
            None,
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("del_m1.txt"),
            None,
            "seed={root_seed}: {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("already_gone.txt"),
            None,
            "seed={root_seed}: {id}"
        );
        let all_live = e.fs().all_live_hashes();
        assert!(
            all_live.contains(&m0_both_h) && all_live.contains(&m1_both_h),
            "seed={root_seed}: concurrent both.txt edit lost on {id}"
        );
        assert_eq!(e.store().held_back_count().unwrap(), 0);
        assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
    }
}

/// Scenario 26: True join (no `.legion-sync` index or marker) followed by edits, deletions, new
/// files, and untouched files BEFORE the first exchange with offline peers — on both 2 and 3
/// members (Decisions 3 & 4, Review 6 Probes 5 & 6).
fn run_true_join_act_before_exchange_seed(root_seed: u64, seed: u64, _record_trace: bool) {
    let mut rng = Rng::new(seed);
    let t0 = Timestamp::new(1_795_608_000, 0);
    let three = (seed & 1) == 1;
    let names: Vec<&str> = if three {
        vec!["m0", "m1", "m2"]
    } else {
        vec!["m0", "m1"]
    };
    let mut es: Vec<SyncEngine<HonestSimFs, FaultyIndexStore>> = names
        .iter()
        .map(|id| {
            let mut cfg = FolderConfig::new(
                "sim",
                (*id).to_string(),
                names.iter().map(|s| (*s).to_string()),
            );
            cfg.initial_incarnation = Some(rng.next_u64() | 1);
            let mut e = SyncEngine::open(
                cfg,
                HonestSimFs::new(FsCapabilities::unix(), rng.chance(1, 2)),
                FaultyIndexStore::new(),
                MemoryInstanceTokenStore::new(),
            )
            .unwrap();
            e.set_deletion_brake(brake_disabled());
            e
        })
        .collect();

    let sync_all =
        |es: &mut [SyncEngine<HonestSimFs, FaultyIndexStore>], t_base: i64, rng: &mut Rng| {
            let n = es.len();
            for round in 0..4i64 {
                let mut pairs = Vec::new();
                for i in 0..n {
                    for j in 0..n {
                        if i != j {
                            pairs.push((i, j));
                        }
                    }
                }
                for k in (1..pairs.len()).rev() {
                    let s = rng.range(k + 1);
                    pairs.swap(k, s);
                }
                for (i, j) in pairs {
                    let now = Timestamp::new(t_base + round * 3, 0);
                    let (dst, src) = if i < j {
                        let (l, r) = es.split_at_mut(j);
                        (&mut l[i], &r[0])
                    } else {
                        let (l, r) = es.split_at_mut(i);
                        (&mut r[0], &l[j])
                    };
                    let d_id = dst.meta().unwrap().local_member;
                    let s_id = src.meta().unwrap().local_member;
                    dst.pull_from_peer(&SimPeer::new(src, &d_id, &s_id, PeerFault::None), now)
                        .unwrap();
                }
            }
        };

    es[0].scan(t0).unwrap();
    let t1 = Timestamp::new(t0.secs + 5, 0);
    for name in [
        "untouched.txt",
        "to_edit.txt",
        "to_delete.txt",
        "concurrent.txt",
        "peer_edits.txt",
        "peer_deletes.txt",
    ] {
        let b = format!("join-base-{name}-{seed}").into_bytes();
        es[0]
            .fs()
            .user_write_file(name, b.clone(), hash_bytes(&b), t1);
    }
    let untouched_h = es[0].fs().live_file_hash("untouched.txt").unwrap();
    es[0].scan(t1).unwrap();
    sync_all(&mut es, t1.secs + 1, &mut rng);

    let untouched_prov_before = es[0]
        .store()
        .get_record("untouched.txt")
        .unwrap()
        .unwrap()
        .provenance;
    assert!(untouched_prov_before.as_versioned().is_some());

    // True join on m1 while peers are off: wipe `.legion-sync` completely (marker + store + token),
    // keep live files on disk!
    let t2 = Timestamp::new(t0.secs + 40, 0);
    let m1_snap = es[1].fs().snapshot();
    let m1_fs = HonestSimFs::from_snapshot(m1_snap);
    *m1_fs.marker.borrow_mut() = None;
    let cfg1 = FolderConfig::new(
        "sim",
        "m1".to_string(),
        names.iter().map(|s| (*s).to_string()),
    );
    let mut m1 = SyncEngine::open(
        cfg1,
        m1_fs,
        FaultyIndexStore::new(),
        MemoryInstanceTokenStore::new(),
    )
    .unwrap();
    m1.set_deletion_brake(brake_disabled());
    assert!(!m1.is_index_damaged());
    m1.scan(t2).unwrap();
    assert_eq!(
        m1.meta().unwrap().local_clock,
        0,
        "seed={root_seed}: Decision 3 initial join scan must keep local_clock == 0"
    );
    assert_eq!(
        m1.store()
            .get_record("untouched.txt")
            .unwrap()
            .unwrap()
            .provenance,
        RecordProvenance::Unversioned,
        "seed={root_seed}: Decision 3 initial join scan must emit Unversioned records"
    );

    // BEFORE ANY EXCHANGE with any peer: m1 edits, deletes, creates, and leaves untouched files.
    let t3 = Timestamp::new(t0.secs + 60, 0);
    let m1_edit = format!("m1-join-edit-{seed}").into_bytes();
    let m1_edit_h = hash_bytes(&m1_edit);
    let m1_conc = format!("m1-conc-{seed}").into_bytes();
    let m1_conc_h = hash_bytes(&m1_conc);
    let m1_new = format!("m1-join-new-{seed}").into_bytes();
    let m1_new_h = hash_bytes(&m1_new);
    m1.fs()
        .user_write_file("to_edit.txt", m1_edit, m1_edit_h, t3);
    m1.fs().user_remove_recursive("to_delete.txt");
    m1.fs()
        .user_write_file("concurrent.txt", m1_conc, m1_conc_h, t3);
    m1.fs()
        .user_write_file("joiner_new.txt", m1_new, m1_new_h, t3);
    m1.scan(t3).unwrap();
    es[1] = m1;

    let peer_actor = if three && rng.chance(1, 2) { 2 } else { 0 };
    let peer_conc = format!("peer-conc-{seed}").into_bytes();
    let peer_conc_h = hash_bytes(&peer_conc);
    let peer_edit = format!("peer-edit-{seed}").into_bytes();
    let peer_edit_h = hash_bytes(&peer_edit);
    es[peer_actor]
        .fs()
        .user_write_file("concurrent.txt", peer_conc, peer_conc_h, t3);
    es[peer_actor]
        .fs()
        .user_write_file("peer_edits.txt", peer_edit, peer_edit_h, t3);
    es[peer_actor]
        .fs()
        .user_remove_recursive("peer_deletes.txt");
    es[peer_actor].scan(t3).unwrap();

    sync_all(&mut es, t0.secs + 100, &mut rng);

    for (idx, e) in es.iter().enumerate() {
        let id = names[idx];
        assert_eq!(
            e.fs().live_file_hash("untouched.txt"),
            Some(untouched_h),
            "seed={root_seed}: untouched.txt lost on {id}"
        );
        assert_eq!(
            e.store()
                .get_record("untouched.txt")
                .unwrap()
                .unwrap()
                .provenance,
            untouched_prov_before,
            "seed={root_seed}: Decision 3 true join grew untouched.txt version vector on {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("to_edit.txt"),
            Some(m1_edit_h),
            "seed={root_seed}: Decision 4 joiner edit did not win cleanly on {id}"
        );
        assert!(
            e.fs()
                .live_tree(&IgnoreMatcher::default())
                .keys()
                .all(|p| !p.starts_with("to_edit (changed on ")),
            "seed={root_seed}: Decision 4 joiner edit made spurious conflict copy on {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("to_delete.txt"),
            None,
            "seed={root_seed}: Decision 4 joiner deletion did not win on {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("peer_edits.txt"),
            Some(peer_edit_h),
            "seed={root_seed}: peer edit did not win cleanly on {id}"
        );
        assert!(
            e.fs()
                .live_tree(&IgnoreMatcher::default())
                .keys()
                .all(|p| !p.starts_with("peer_edits (changed on ")),
            "seed={root_seed}: peer edit made spurious conflict copy on {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("peer_deletes.txt"),
            None,
            "seed={root_seed}: peer deletion resurrected on {id}"
        );
        assert_eq!(
            e.fs().live_file_hash("joiner_new.txt"),
            Some(m1_new_h),
            "seed={root_seed}: joiner_new.txt missing on {id}"
        );
        let all_live = e.fs().all_live_hashes();
        assert!(
            all_live.contains(&m1_conc_h) && all_live.contains(&peer_conc_h),
            "seed={root_seed}: concurrent edit on concurrent.txt lost on {id}"
        );
        assert_eq!(e.store().held_back_count().unwrap(), 0);
        assert_eq!(e.status().unwrap().state, MemberState::UpToDate);
    }
}

fn run_one_seed(seed: u64, record_trace: bool) {
    // Every seed runs the dense randomized schedule (all faults, mid-apply human edits before &
    // after swap/claim, bin restore, conflict resolution, clock jumps both ways, directory renames,
    // deep trees).
    let mut world = SimWorld::new(
        seed,
        "random_schedule",
        seed,
        seed.is_multiple_of(8),
        Vec::new(),
        record_trace,
    );
    world.run_schedule(32);

    // Every seed also hits a dedicated deep scenario on a clean modulo cadence.
    match seed % 27 {
        0 => run_long_partition_zombie_seed(seed, seed ^ 0x5555_5555, record_trace),
        1 => run_conflict_and_delete_vs_edit_seed(seed, seed ^ 0xAAAA_AAAA, record_trace),
        2 => run_heterogeneous_and_case_rename_seed(seed, seed ^ 0x3333_3333, record_trace),
        3 => run_backup_reinstall_and_join_old_copy_seed(seed, seed ^ 0x7777_7777, record_trace),
        4 => run_duplicate_member_id_seed(seed, seed ^ 0x9999_9999, record_trace),
        5 => run_membership_change_and_readd_seed(seed, seed ^ 0xBBBB_BBBB, record_trace),
        6 => run_hostile_peer_seed(seed, seed ^ 0xCCCC_CCCC, record_trace),
        7 => run_unreadable_mount_and_ignored_dir_seed(seed, seed ^ 0xDDDD_DDDD, record_trace),
        8 => {
            run_crash_recovery_mid_apply_and_peer_fail_seed(seed, seed ^ 0xEEEE_EEEE, record_trace)
        }
        9 => run_swap_boundary_races_seed(seed, seed ^ 0x1111_1111, record_trace),
        10 => run_late_membership_and_unacked_deletion_seed(seed, seed ^ 0x2222_2222, record_trace),
        11 => run_backup_restore_more_changes_than_gap_seed(seed, seed ^ 0x4444_4444, record_trace),
        12 => run_index_wipe_and_explicit_rebuild_seed(seed, seed ^ 0x6666_6666, record_trace),
        13 => run_deletion_brake_and_inbound_brake_seed(seed, seed ^ 0x8888_8888, record_trace),
        14 => run_adversary_counters_rotation_and_symlinks_seed(
            seed,
            seed ^ 0xF0F0_F0F0,
            record_trace,
        ),
        15 => run_two_joiners_same_old_copy_seed(seed, seed ^ 0x1357_2468, record_trace),
        16 => run_lifetime_rotations_and_vv_pruning_seed(seed, seed ^ 0x2468_1357, record_trace),
        17 => run_late_write_into_bin_seed(seed, seed ^ 0x369C_F258, record_trace),
        18 => run_local_placement_failures_seed(seed, seed ^ 0x48C0_159D, record_trace),
        19 => run_index_loss_during_hold_seed(seed, seed ^ 0x5AD1_6BE2, record_trace),
        20 => run_lying_peer_regression_rate_limit_seed(seed, seed ^ 0x6BE2_7CF3, record_trace),
        21 => run_asymmetric_membership_views_seed(seed, seed ^ 0x7CF3_8D04, record_trace),
        22 => run_joiner_after_deleter_rebuild_seed(seed, seed ^ 0x8D04_9E15, record_trace),
        23 => run_power_loss_crash_recovery_seed(seed, seed ^ 0x9E15_AF26, record_trace),
        24 => run_token_loss_reinstall_seed(seed, seed ^ 0xAF26_B037, record_trace),
        25 => run_folder_copy_with_store_seed(seed, seed ^ 0xB037_C148, record_trace),
        _ => run_true_join_act_before_exchange_seed(seed, seed ^ 0xC148_D259, record_trace),
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
        let failed = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let chunk = count.div_ceil(threads as u64);
            let mut handles = Vec::with_capacity(threads);
            let failed_ref = &failed;
            for t in 0..threads {
                let s = start_seed + (t as u64) * chunk;
                let e = (s + chunk).min(start_seed + count);
                if s >= e {
                    continue;
                }
                handles.push(scope.spawn(move || {
                    for seed in s..e {
                        if failed_ref.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Err(payload) =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                run_one_seed(seed, false);
                            }))
                        {
                            failed_ref.store(true, Ordering::Relaxed);
                            std::panic::resume_unwind(payload);
                        }
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
    951, 1137, 1225, 1326, 1337, 1346, 1435, 1451, 1476, 1532, 1576, 1617, 2026, 2478, 5635, 11146,
    11338, 15197, 19883, 25021, 25180, 27850, 31444, 32580, 35879, 48566, 49954, 56498, 60211,
    71221, 85172, 85757, 89687, 90830, 94963, 248230, 257640, 338474, 405410, 521801, 612202,
    640064, 656614, 834066, 859170, 1234029, 1252407, 1276023, 1325262, 1381582, 1442864, 1631700,
    1735939, 1845682, 1905590, 1914338, 2132430, 2212240, 2242736, 2469631, 2508764, 2594599,
    2770489, 2777393, 2838112, 2844675, 2909715, 2972224, 3132684, 3137180, 3286448, 3660488,
    3739249, 3804523, 3810856, 3954961, 4066064, 4139193, 4319726, 4376094, 4377839, 4395930,
    4560644, 4628071, 4689380, 4717156, 5028251, 6102439, 6250401, 6656918, 6972988, 7227298,
    7504275, 7909139, 8482679, 8730315, 8855009, 9067232, 9625426, 13635160, 17469556, 18877611,
    19447454, 37147542,
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
        run_swap_boundary_races_seed(s, s ^ 0x1111_1111, true);
        run_late_membership_and_unacked_deletion_seed(s, s ^ 0x2222_2222, true);
        run_backup_restore_more_changes_than_gap_seed(s, s ^ 0x4444_4444, true);
        run_index_wipe_and_explicit_rebuild_seed(s, s ^ 0x6666_6666, true);
        run_deletion_brake_and_inbound_brake_seed(s, s ^ 0x8888_8888, true);
        run_adversary_counters_rotation_and_symlinks_seed(s, s ^ 0xF0F0_F0F0, true);
        run_two_joiners_same_old_copy_seed(s, s ^ 0x1357_2468, true);
        run_lifetime_rotations_and_vv_pruning_seed(s, s ^ 0x2468_1357, true);
        run_late_write_into_bin_seed(s, s ^ 0x369C_F258, true);
        run_local_placement_failures_seed(s, s ^ 0x48C0_159D, true);
        run_index_loss_during_hold_seed(s, s ^ 0x5AD1_6BE2, true);
        run_lying_peer_regression_rate_limit_seed(s, s ^ 0x6BE2_7CF3, true);
        run_asymmetric_membership_views_seed(s, s ^ 0x7CF3_8D04, true);
        run_joiner_after_deleter_rebuild_seed(s, s ^ 0x8D04_9E15, true);
        run_power_loss_crash_recovery_seed(s, s ^ 0x9E15_AF26, true);
        run_token_loss_reinstall_seed(s, s ^ 0xAF26_B037, true);
        run_folder_copy_with_store_seed(s, s ^ 0xB037_C148, true);
        run_true_join_act_before_exchange_seed(s, s ^ 0xC148_D259, true);
    }
}

#[test]
fn simulator_idle_fetch_timeout_virtual_clock() {
    struct SimTickReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        tick_ms: u64,
        eof_tick_ms: u64,
        clock_ms: Arc<AtomicU64>,
    }
    impl Read for SimTickReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                self.clock_ms.fetch_add(self.eof_tick_ms, Ordering::SeqCst);
                return Ok(0);
            }
            self.clock_ms.fetch_add(self.tick_ms, Ordering::SeqCst);
            let n = (self.data.len() - self.pos).min(self.chunk).min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }
    struct SimTimedPeer<'a> {
        inner: SimPeer<'a>,
        payload: Vec<u8>,
        chunk: usize,
        tick_ms: u64,
        eof_tick_ms: u64,
        clock_ms: Arc<AtomicU64>,
    }
    impl Peer for SimTimedPeer<'_> {
        fn member_id(&self) -> &str {
            self.inner.member_id()
        }
        fn handshake(&self, c: &legion_sync::HandshakeHello) -> legion_sync::Result<PeerHandshake> {
            self.inner.handshake(c)
        }
        fn records_since(
            &self,
            a: u64,
            b: u64,
            l: PageLimit,
            c: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            self.inner.records_since(a, b, l, c)
        }
        fn fetch_file(
            &self,
            _p: &str,
            _h: Hash,
            o: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            Ok(Box::new(SimTickReader {
                data: self.payload.clone(),
                pos: (o.offset as usize).min(self.payload.len()),
                chunk: self.chunk,
                tick_ms: self.tick_ms,
                eof_tick_ms: self.eof_tick_ms,
                clock_ms: Arc::clone(&self.clock_ms),
            }))
        }
    }

    let t0 = Timestamp::new(1_795_608_000, 0);
    let members = ["m0".to_string(), "m1".to_string()];
    let mut cfg0 = FolderConfig::new("sim", "m0", members.clone());
    cfg0.initial_incarnation = Some(101);
    let mut cfg1 = FolderConfig::new("sim", "m1", members);
    cfg1.initial_incarnation = Some(202);
    let mut e0 = SyncEngine::open(
        cfg0,
        HonestSimFs::new(FsCapabilities::unix(), true),
        FaultyIndexStore::new(),
        MemoryInstanceTokenStore::new(),
    )
    .unwrap();
    let mut e1 = SyncEngine::open(
        cfg1,
        HonestSimFs::new(FsCapabilities::unix(), true),
        FaultyIndexStore::new(),
        MemoryInstanceTokenStore::new(),
    )
    .unwrap();

    let payload = vec![b'a'; 2_000];
    e0.fs()
        .user_write_file("slow.bin", payload.clone(), hash_bytes(&payload), t0);
    e0.scan(t0).unwrap();

    // 20 chunks * 29,999ms = 599.98s total >> 30s idle limit, each read arrives 1ms inside limit.
    let clock_ms = Arc::new(AtomicU64::new(0));
    let vclock = {
        let c = Arc::clone(&clock_ms);
        FetchClock::new(move || Duration::from_millis(c.load(Ordering::SeqCst)))
    };
    let opts = FetchOptions::at_offset(0, Duration::from_secs(30), None).with_clock(vclock);
    let ok_peer = SimTimedPeer {
        inner: SimPeer::new(&e0, "m1", "m0", PeerFault::None),
        payload: payload.clone(),
        chunk: 100,
        tick_ms: 29_999,
        eof_tick_ms: 1_000,
        clock_ms: Arc::clone(&clock_ms),
    };
    e1.pull_from_peer_with_options(&ok_peer, 100, &opts, Timestamp::new(t0.secs + 1, 0))
        .unwrap();
    assert_eq!(
        e1.fs().hash_live("slow.bin").unwrap(),
        (hash_bytes(&payload), payload.len() as u64)
    );

    // Stall on chunk (30,001ms > 30s) aborts cleanly and leaves no tmp file.
    let payload2 = vec![b'b'; 500];
    let t1 = Timestamp::new(t0.secs + 10, 0);
    e0.fs()
        .user_write_file("slow.bin", payload2.clone(), hash_bytes(&payload2), t1);
    e0.scan(t1).unwrap();
    let stalled_peer = SimTimedPeer {
        inner: SimPeer::new(&e0, "m1", "m0", PeerFault::None),
        payload: payload2.clone(),
        chunk: 100,
        tick_ms: 30_001,
        eof_tick_ms: 0,
        clock_ms: Arc::clone(&clock_ms),
    };
    let err = e1
        .pull_from_peer_with_options(&stalled_peer, 100, &opts, Timestamp::new(t0.secs + 11, 0))
        .unwrap_err();
    assert!(matches!(err, SyncError::Timeout { .. }), "got {err:?}");
    assert!(e1.fs().list_tmp().unwrap().is_empty());

    // Stall before EOF (30,001ms > 30s) also aborts cleanly.
    let stalled_eof = SimTimedPeer {
        inner: SimPeer::new(&e0, "m1", "m0", PeerFault::None),
        payload: payload2,
        chunk: 500,
        tick_ms: 1_000,
        eof_tick_ms: 30_001,
        clock_ms,
    };
    let err_eof = e1
        .pull_from_peer_with_options(&stalled_eof, 100, &opts, Timestamp::new(t0.secs + 12, 0))
        .unwrap_err();
    assert!(
        matches!(err_eof, SyncError::Timeout { .. }),
        "got {err_eof:?}"
    );
    assert!(e1.fs().list_tmp().unwrap().is_empty());
}

#[test]
fn simulator_power_loss_durability_proves_sync_invariants() {
    // 0. Audit the real `RealFs` implementation first: removing any `sync_dir` in `fs_real.rs`
    //    must fail this test AND Generator 23 immediately.
    let audit = cached_real_fs_durability_audit();
    assert!(
        audit.sync_created_ancestors && audit.sync_remove_tmp && audit.unsynced_ops.is_empty(),
        "RealFs durability audit failed: {audit:?}"
    );

    // 1. Direct verification that creating a multi-level nested directory path and placing a file
    // inside it survives an immediate post-commit power loss (wired to `audit`).
    let fs = HonestSimFs::new(FsCapabilities::unix(), true);
    fs.enable_power_loss_mode(42, audit.sync_created_ancestors, audit.sync_remove_tmp);
    let mut w = fs.create_tmp(".legion-sync-fetch-1-aabbccdd.tmp").unwrap();
    w.write_all(b"nested durable payload").unwrap();
    w.finish_sync().unwrap();
    fs.commit_tmp_to_live_no_replace(".legion-sync-fetch-1-aabbccdd.tmp", "a/b/c/file.txt")
        .unwrap();
    fs.simulate_power_loss();
    assert_eq!(
        fs.live_file_hash("a/b/c/file.txt"),
        Some(hash_bytes(b"nested durable payload")),
        "multi-level newly created ancestor directories must survive immediate power loss"
    );

    // 2. Hammer 128 independent seeds of each new scenario generator (21, 22, 23).
    for s in 0..128u64 {
        run_asymmetric_membership_views_seed(s, s ^ 0x7CF3_8D04, true);
        run_joiner_after_deleter_rebuild_seed(s, s ^ 0x8D04_9E15, true);
        run_power_loss_crash_recovery_seed(s, s ^ 0x9E15_AF26, true);
        run_token_loss_reinstall_seed(s, s ^ 0xAF26_B037, true);
        run_folder_copy_with_store_seed(s, s ^ 0xB037_C148, true);
        run_true_join_act_before_exchange_seed(s, s ^ 0xC148_D259, true);
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
