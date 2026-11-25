//! Abstraction traits for the filesystem, remote peer, and durable index store.
//!
//! Keeping these three boundaries behind traits lets the core engine run identically against
//! real disks (`RealFs` + `RedbIndexStore`), network peers (supplied by upper crates), and the
//! deterministic fault-injecting simulator (`MemFs` + `MemoryIndexStore`).

use std::collections::BTreeMap;
use std::io::{Read, Write};

use legion_xfer::{EntryKind, Hash, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{
    BinEntry, CausalMatrix, FolderMeta, HeldBackRecord, IndexRecord, MemberId, PendingOp,
    RecordsPage,
};
use crate::path::IgnoreMatcher;

/// Filesystem capabilities and naming rules for a sync member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsCapabilities {
    /// `true` on case-insensitive filesystems (default macOS APFS/HFS+, Windows NTFS).
    pub case_insensitive: bool,
    /// `true` on Windows, enforcing reserved DOS device names, forbidden characters, and no
    /// trailing dots or spaces.
    pub windows_naming: bool,
    /// `true` on Unix where symbolic links are synced as links; `false` on Windows where they are
    /// held back and reported as problems.
    pub supports_symlinks: bool,
    /// Maximum length in bytes of any single path segment (`0` means unlimited).
    pub max_segment_len: usize,
    /// Maximum length in bytes of a relative sync path (`0` means unlimited).
    pub max_path_len: usize,
}

impl FsCapabilities {
    /// Typical Linux / case-sensitive Unix defaults.
    pub const fn unix() -> Self {
        Self {
            case_insensitive: false,
            windows_naming: false,
            supports_symlinks: true,
            max_segment_len: 255,
            max_path_len: 4096,
        }
    }

    /// Typical macOS default APFS (case-insensitive, supports symlinks).
    pub const fn macos_case_insensitive() -> Self {
        Self {
            case_insensitive: true,
            windows_naming: false,
            supports_symlinks: true,
            max_segment_len: 255,
            max_path_len: 1024,
        }
    }

    /// Typical Windows NTFS defaults (case-insensitive, Windows naming rules, symlinks held back).
    pub const fn windows() -> Self {
        Self {
            case_insensitive: true,
            windows_naming: true,
            supports_symlinks: false,
            max_segment_len: 255,
            max_path_len: 240,
        }
    }
}

/// Non-following metadata snapshot of a filesystem entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsMeta {
    /// Kind of object (`File`, `Dir`, or `Symlink`).
    pub kind: EntryKind,
    /// Byte length (`0` for directories and symlinks).
    pub size: u64,
    /// Last modification timestamp.
    pub mtime: Timestamp,
    /// Unix permission bits (`0o777` mask); `None` on Windows.
    pub mode: Option<u32>,
    /// Verbatim target string for symbolic links.
    pub link_target: Option<String>,
    /// Optional local-only change fingerprint (for example derived from `(dev, ino, ctime)` on
    /// Unix) so same-size, same-mtime overwrites and renames are detected without re-hashing
    /// unchanged files.
    pub change_id: Option<u64>,
}

/// One live entry discovered during a [`SyncFs::walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWalkEntry {
    /// Normalized forward-slash relative path inside the folder root.
    pub path: String,
    /// Non-following metadata captured during the walk.
    pub meta: FsMeta,
}

/// Output of a [`SyncFs::walk`] pass: readable entries plus any skipped `(path, reason)` errors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsWalkResult {
    /// Readable live entries sorted lexicographically by [`FsWalkEntry::path`].
    pub entries: Vec<FsWalkEntry>,
    /// Entries that could not be inspected during the walk (`(path, reason)`).
    pub skipped: Vec<(String, String)>,
}

/// A writable handle to a staged file in `.legion-sync/tmp/` that can be flushed and synced to
/// durable storage before atomic rename.
pub trait SyncWrite: Write + Send {
    /// Flushes buffered bytes and commits data to durable storage (`fsync`).
    fn finish_sync(self: Box<Self>) -> Result<()>;
}

/// Filesystem boundary of a single sync folder.
///
/// Operations on the live tree are strictly separated from `.legion-sync/tmp/` and
/// `.legion-sync/bin/` so no live operation can touch sync metadata or escape the folder root.
pub trait SyncFs {
    /// Returns this filesystem's platform naming and case rules.
    fn capabilities(&self) -> FsCapabilities;

    /// Walks the live tree under the folder root, skipping `.legion-sync/` and any entry matched
    /// by `ignore`.
    ///
    /// Never follows symbolic links. Entries are returned sorted lexicographically by `path`.
    /// Unreadable entries are reported in [`FsWalkResult::skipped`] so one unreadable file does
    /// not abort the whole scan.
    fn walk(&self, ignore: &IgnoreMatcher) -> Result<FsWalkResult>;

    /// Inspects a live path without following symlinks, returning `Ok(None)` when absent.
    fn stat_live(&self, rel_path: &str) -> Result<Option<FsMeta>>;

    /// Computes the BLAKE3 digest and byte count of a live regular file using streaming I/O.
    fn hash_live(&self, rel_path: &str) -> Result<(Hash, u64)>;

    /// Opens a live regular file for reading (used when serving files to a peer).
    ///
    /// Must verify no ancestor component is a symlink and refuse `.legion-sync/` or traversal.
    fn open_live_read(&self, rel_path: &str) -> Result<Box<dyn Read + Send>>;

    /// Creates `rel_path` and any missing ancestor directories in the live tree.
    ///
    /// Must refuse if any ancestor is a symlink or regular file.
    fn create_dir_all_live(&self, rel_path: &str) -> Result<()>;

    /// Lists direct child names of a live directory (excluding `.legion-sync` at root).
    fn list_dir_live(&self, rel_path: &str) -> Result<Vec<String>>;

    /// Removes an empty live directory.
    fn remove_empty_dir_live(&self, rel_path: &str) -> Result<()>;

    /// Creates or atomically replaces a live symbolic link pointing to `target`.
    fn create_symlink_live(&self, rel_path: &str, target: &str) -> Result<()>;

    /// Removes a live symbolic link without following it.
    fn remove_symlink_live(&self, rel_path: &str) -> Result<()>;

    /// Applies modification time and (on Unix) permission bits to a live file or directory.
    fn set_live_metadata(&self, rel_path: &str, mtime: Timestamp, mode: Option<u32>) -> Result<()>;

    /// Atomically renames a live entry to another live path inside the same folder (used when
    /// moving a losing local file to its conflict copy path).
    fn rename_live_to_live(&self, from_rel: &str, to_rel: &str) -> Result<()>;

    /// Creates a new staging file inside `.legion-sync/tmp/<tmp_name>`.
    fn create_tmp(&self, tmp_name: &str) -> Result<Box<dyn SyncWrite>>;

    /// Inspects a staged file inside `.legion-sync/tmp/<tmp_name>`.
    fn stat_tmp(&self, tmp_name: &str) -> Result<Option<FsMeta>>;

    /// Computes the BLAKE3 digest and byte length of `.legion-sync/tmp/<tmp_name>`.
    fn hash_tmp(&self, tmp_name: &str) -> Result<(Hash, u64)>;

    /// Removes `.legion-sync/tmp/<tmp_name>` if it exists.
    fn remove_tmp(&self, tmp_name: &str) -> Result<()>;

    /// Lists filenames currently present inside `.legion-sync/tmp/`.
    fn list_tmp(&self) -> Result<Vec<String>>;

    /// Atomically renames `.legion-sync/tmp/<tmp_name>` into place at `live_rel`, creating parent
    /// directories if needed and syncing the parent directory.
    fn commit_tmp_to_live(&self, tmp_name: &str, live_rel: &str) -> Result<()>;

    /// Atomically moves an existing live file at `live_rel` into
    /// `.legion-sync/bin/<bin_data_name>`.
    fn move_live_to_bin(&self, live_rel: &str, bin_data_name: &str) -> Result<()>;

    /// Atomically writes a JSON sidecar metadata file into `.legion-sync/bin/<meta_name>`.
    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> Result<()>;

    /// Reads `.legion-sync/bin/<meta_name>`, returning `Ok(None)` if absent.
    fn read_bin_meta(&self, meta_name: &str) -> Result<Option<Vec<u8>>>;

    /// Inspects `.legion-sync/bin/<bin_name>`.
    fn stat_bin(&self, bin_name: &str) -> Result<Option<FsMeta>>;

    /// Computes the BLAKE3 digest and byte count of `.legion-sync/bin/<bin_name>`.
    fn hash_bin(&self, bin_name: &str) -> Result<(Hash, u64)>;

    /// Copies `.legion-sync/bin/<bin_data_name>` into `.legion-sync/tmp/<tmp_name>` and syncs it
    /// (used for crash-safe restore from bin).
    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> Result<()>;

    /// Removes `.legion-sync/bin/<bin_name>` if it exists.
    fn remove_bin_file(&self, bin_name: &str) -> Result<()>;

    /// Lists filenames currently present inside `.legion-sync/bin/`.
    fn list_bin(&self) -> Result<Vec<String>>;
}

/// Remote member boundary used during pull-only synchronization.
pub trait Peer {
    /// Member identifier of this peer.
    fn member_id(&self) -> &str;

    /// Fetches index records created or updated after `since_seq`, up to `limit` entries, from a
    /// single consistent snapshot of the peer's index.
    fn records_since(&self, since_seq: u64, limit: usize) -> Result<RecordsPage>;

    /// Streams the bytes of `path` whose BLAKE3 digest must equal `expected_hash`.
    fn fetch_file(&self, path: &str, expected_hash: Hash) -> Result<Box<dyn Read + Send>>;
}

/// Atomic batch of mutations committed in a single [`IndexStore::apply_batch`] transaction.
#[derive(Debug, Default, Clone)]
pub struct StoreBatch {
    /// Updated folder metadata, if changed.
    pub folder_meta: Option<FolderMeta>,
    /// Records to insert or replace (keyed by `record.path`, updating seq, tombstone, conflict,
    /// and case-folding secondary indexes automatically).
    pub upsert_records: Vec<IndexRecord>,
    /// Record paths to permanently remove (garbage-collected tombstones).
    pub remove_records: Vec<String>,
    /// Peer cursors to update (`peer -> seq`).
    pub set_peer_cursors: BTreeMap<MemberId, u64>,
    /// Peer cursors to remove (when a member leaves the folder).
    pub remove_peer_cursors: Vec<MemberId>,
    /// Updated causal matrix, if changed.
    pub causal_matrix: Option<CausalMatrix>,
    /// Bin entries to insert or update.
    pub upsert_bin: Vec<BinEntry>,
    /// Bin entry IDs to remove (restored or expired).
    pub remove_bin: Vec<String>,
    /// Held-back peer records to insert or update.
    pub upsert_held_back: Vec<HeldBackRecord>,
    /// Held-back record paths to remove (resolved, applied, or deleted).
    pub remove_held_back: Vec<String>,
    /// Update to the write-ahead crash journal (`Some(Some(op))` sets it; `Some(None)` clears it;
    /// `None` leaves it unchanged).
    pub set_pending_op: Option<Option<PendingOp>>,
}

impl StoreBatch {
    /// Returns `true` when the batch contains no mutations.
    pub fn is_empty(&self) -> bool {
        self.folder_meta.is_none()
            && self.upsert_records.is_empty()
            && self.remove_records.is_empty()
            && self.set_peer_cursors.is_empty()
            && self.remove_peer_cursors.is_empty()
            && self.causal_matrix.is_none()
            && self.upsert_bin.is_empty()
            && self.remove_bin.is_empty()
            && self.upsert_held_back.is_empty()
            && self.remove_held_back.is_empty()
            && self.set_pending_op.is_none()
    }
}

/// Durable, atomic key-value storage backend for a member's sync index.
pub trait IndexStore {
    /// Loads the stored [`FolderMeta`], or `Ok(None)` if this store has not yet been initialized.
    fn load_meta(&self) -> Result<Option<FolderMeta>>;

    /// Loads one [`IndexRecord`] by exact relative path.
    fn get_record(&self, path: &str) -> Result<Option<IndexRecord>>;

    /// Visits every [`IndexRecord`] in lexicographical path order inside a single read snapshot.
    ///
    /// Returning `Ok(false)` from `visitor` stops iteration early.
    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()>;

    /// Returns `true` if any live record in the index is a strict descendant of `dir_path`
    /// (`path` starts with `"{dir_path}/"`).
    fn has_live_descendant(&self, dir_path: &str) -> Result<bool>;

    /// Returns all live exact paths in the index whose case-folded form equals `folded_path`.
    fn live_paths_for_folded(&self, folded_path: &str) -> Result<Vec<String>>;

    /// Reads a consistent page of records with `seq > since_seq` (up to `limit`), together with
    /// the snapshot's `local_seq` and [`CausalMatrix`].
    fn records_since(&self, since_seq: u64, limit: usize) -> Result<RecordsPage>;

    /// Returns all currently active tombstone records (used by tombstone GC without scanning live
    /// entries).
    fn list_tombstones(&self) -> Result<Vec<IndexRecord>>;

    /// Returns all currently active conflict copy records (live records with `conflict_of` set).
    fn list_conflicts(&self) -> Result<Vec<IndexRecord>>;

    /// Loads all peer cursors (`peer -> highest processed peer seq`).
    fn load_peer_cursors(&self) -> Result<BTreeMap<MemberId, u64>>;

    /// Loads the current [`CausalMatrix`].
    fn load_causal_matrix(&self) -> Result<CausalMatrix>;

    /// Lists all entries currently tracked in `.legion-sync/bin`.
    fn list_bin_entries(&self) -> Result<Vec<BinEntry>>;

    /// Loads one bin entry by its unique ID.
    fn get_bin_entry(&self, id: &str) -> Result<Option<BinEntry>>;

    /// Lists all currently held-back peer records.
    fn list_held_back(&self) -> Result<Vec<HeldBackRecord>>;

    /// Loads the active write-ahead [`PendingOp`], if any.
    fn load_pending_op(&self) -> Result<Option<PendingOp>>;

    /// Returns the total number of records (live + tombstones) in the index.
    fn record_count(&self) -> Result<u64>;

    /// Applies every mutation in `batch` in a single atomic transaction.
    fn apply_batch(&mut self, batch: StoreBatch) -> Result<()>;
}
