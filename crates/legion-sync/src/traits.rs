//! Abstraction traits for the filesystem, remote peer, and durable index store.
//!
//! Keeping these three boundaries behind traits lets the core engine run identically against
//! real disks (`RealFs` + `RedbIndexStore`), network peers (supplied by upper crates), and the
//! deterministic fault-injecting simulator.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::atomic::AtomicBool;

use legion_xfer::{EntryKind, Hash, Timestamp};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{
    BinEntry, FetchOptions, FolderMarker, FolderMeta, HeldBackRecord, IndexRecord, MemberId,
    PeerCursor, PeerHandshake, PendingOp, RecordsPage,
};
use crate::path::IgnoreMatcher;

/// Filesystem capabilities and naming rules for a sync member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsCapabilities {
    /// `true` on case-insensitive filesystems (default macOS APFS/HFS+, Windows NTFS).
    pub case_insensitive: bool,
    /// `true` on Unicode-normalizing filesystems (macOS APFS/HFS+ where NFC and NFD names refer
    /// to the same file).
    #[serde(default)]
    pub unicode_normalizing: bool,
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
            unicode_normalizing: false,
            windows_naming: false,
            supports_symlinks: true,
            max_segment_len: 255,
            max_path_len: 4096,
        }
    }

    /// Typical macOS default APFS (case-insensitive, Unicode-normalizing, supports symlinks).
    pub const fn macos_case_insensitive() -> Self {
        Self {
            case_insensitive: true,
            unicode_normalizing: true,
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
            unicode_normalizing: false,
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
    /// Optional local-only change fingerprint (derived from `(dev, ino, ctime)` on Unix or NTFS
    /// `ChangeTime` + `FileId` on Windows) so same-size, same-mtime overwrites and renames are
    /// detected without re-hashing unchanged files.
    pub change_id: Option<u64>,
}

/// One live entry discovered during a [`SyncFs::walk`] or [`SyncFs::walk_subtree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsWalkEntry {
    /// Normalized forward-slash relative path inside the folder root.
    pub path: String,
    /// Non-following metadata captured during the walk.
    pub meta: FsMeta,
}

/// Output of a filesystem walk pass: readable entries plus any skipped `(path, reason)` errors.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FsWalkResult {
    /// Readable live entries sorted lexicographically by [`FsWalkEntry::path`].
    pub entries: Vec<FsWalkEntry>,
    /// Entries or directories that could not be inspected (`(path, reason)`).
    ///
    /// Scanning never records deletions for any path equal to or under a skipped entry.
    pub skipped: Vec<(String, String)>,
}

/// A writable handle to a staged file in `.legion-sync/tmp/` that can be flushed and synced to
/// durable storage before atomic placement.
pub trait SyncWrite: Write + Send {
    /// Flushes buffered bytes and commits data to durable storage (`fsync`).
    fn finish_sync(self: Box<Self>) -> Result<()>;
}

/// Filesystem boundary of a single sync folder.
///
/// Operations on the live tree are strictly separated from `.legion-sync/tmp/`,
/// `.legion-sync/bin/`, and `.legion-sync/marker.json` so no live operation can touch sync
/// metadata or escape the folder root. All placements and bin moves use non-replacing renames or
/// atomic swaps so an entry created or modified in the race window is never silently overwritten.
pub trait SyncFs {
    /// Returns this filesystem's platform naming and case rules.
    fn capabilities(&self) -> FsCapabilities;

    /// Reads the folder identity marker at `.legion-sync/marker.json`, returning `Ok(None)` if
    /// absent (never creating `.legion-sync/` as a side effect).
    fn read_marker(&self) -> Result<Option<FolderMarker>>;

    /// Atomically writes the folder identity marker to `.legion-sync/marker.json`.
    fn write_marker(&self, marker: &FolderMarker) -> Result<()>;

    /// Ensures `.legion-sync/tmp` and `.legion-sync/bin` exist. Called only when initializing a
    /// new folder or after verifying an existing folder marker.
    fn ensure_internal_dirs(&self) -> Result<()>;

    /// Verifies that this folder is neither inside another sync folder nor contains a nested
    /// sync folder.
    fn check_not_nested(&self) -> Result<()> {
        Ok(())
    }

    /// Walks the entire live tree under the folder root, skipping `.legion-sync` at any depth and
    /// any entry matched by `ignore`.
    fn walk(&self, ignore: &IgnoreMatcher) -> Result<FsWalkResult>;

    /// Walks only the live entry or subtree rooted at `rel_root` (used by file watchers for
    /// incremental rescans without walking 500,000 unrelated files).
    fn walk_subtree(&self, rel_root: &str, ignore: &IgnoreMatcher) -> Result<FsWalkResult>;

    /// Inspects a live path without following symlinks, returning `Ok(None)` when absent.
    fn stat_live(&self, rel_path: &str) -> Result<Option<FsMeta>>;

    /// Computes the BLAKE3 digest and byte count of a live regular file using streaming I/O.
    fn hash_live(&self, rel_path: &str) -> Result<(Hash, u64)>;

    /// Opens a live regular file for reading starting at byte `offset` (used when serving files
    /// and ranged resumes to peers).
    fn open_live_read_at(&self, rel_path: &str, offset: u64) -> Result<Box<dyn Read + Send>>;

    /// Opens a live regular file from offset `0`.
    fn open_live_read(&self, rel_path: &str) -> Result<Box<dyn Read + Send>> {
        self.open_live_read_at(rel_path, 0)
    }

    /// Creates `rel_path` and any missing ancestor directories in the live tree.
    fn create_dir_all_live(&self, rel_path: &str) -> Result<()>;

    /// Lists direct child names of a live directory (excluding `.legion-sync`).
    fn list_dir_live(&self, rel_path: &str) -> Result<Vec<String>>;

    /// Removes an empty live directory.
    fn remove_empty_dir_live(&self, rel_path: &str) -> Result<()>;

    /// Creates a live symbolic link pointing to `target`, refusing to replace an existing entry
    /// (returning `FsErrorKind::AlreadyExists` if `rel_path` already exists).
    fn create_symlink_live(&self, rel_path: &str, target: &str) -> Result<()>;

    /// Removes a live symbolic link without following it.
    fn remove_symlink_live(&self, rel_path: &str) -> Result<()>;

    /// Applies modification time and (on Unix) permission bits to a live file or directory.
    fn set_live_metadata(&self, rel_path: &str, mtime: Timestamp, mode: Option<u32>) -> Result<()>;

    /// Atomically renames a live entry to another live path, refusing to overwrite if `to_rel`
    /// already exists (`FsErrorKind::AlreadyExists`).
    fn rename_live_to_live_no_replace(&self, from_rel: &str, to_rel: &str) -> Result<()>;

    /// Syncs the parent directory of `live_rel` to durable storage (used for batched group
    /// commits).
    fn sync_parent_dir_live(&self, _live_rel: &str) {}

    /// Syncs `.legion-sync/tmp` to durable storage (used once at the end of a batched group
    /// commit).
    fn sync_tmp_dir(&self) {}

    /// Creates a new staging file inside `.legion-sync/tmp/<tmp_name>`.
    fn create_tmp(&self, tmp_name: &str) -> Result<Box<dyn SyncWrite>>;

    /// Opens an existing staging file inside `.legion-sync/tmp/<tmp_name>` for appending during a
    /// resumed ranged transfer.
    fn open_tmp_append(&self, tmp_name: &str) -> Result<Box<dyn SyncWrite>>;

    /// Inspects a staged file inside `.legion-sync/tmp/<tmp_name>`.
    fn stat_tmp(&self, tmp_name: &str) -> Result<Option<FsMeta>>;

    /// Computes the BLAKE3 digest and byte length of `.legion-sync/tmp/<tmp_name>`.
    fn hash_tmp(&self, tmp_name: &str) -> Result<(Hash, u64)>;

    /// Removes `.legion-sync/tmp/<tmp_name>` if it exists.
    fn remove_tmp(&self, tmp_name: &str) -> Result<()>;

    /// Lists filenames currently present inside `.legion-sync/tmp/`.
    fn list_tmp(&self) -> Result<Vec<String>>;

    /// Atomically places `.legion-sync/tmp/<tmp_name>` at `live_rel`, refusing to replace an
    /// existing entry (`FsErrorKind::AlreadyExists`).
    fn commit_tmp_to_live_no_replace(&self, tmp_name: &str, live_rel: &str) -> Result<()>;

    /// Like [`Self::commit_tmp_to_live_no_replace`], but allows deferring parent-directory and
    /// `.legion-sync/tmp` directory `fsync`s to a batch boundary where [`Self::sync_parent_dir_live`]
    /// and [`Self::sync_tmp_dir`] are called before committing the index batch.
    fn commit_tmp_to_live_no_replace_deferred_dir_sync(
        &self,
        tmp_name: &str,
        live_rel: &str,
    ) -> Result<()> {
        self.commit_tmp_to_live_no_replace(tmp_name, live_rel)
    }

    /// Atomically exchanges `.legion-sync/tmp/<tmp_name>` and existing `live_rel` (Linux
    /// `renameat2(RENAME_EXCHANGE)`, macOS `renamex_np(RENAME_SWAP)`).
    ///
    /// Returns `Ok(true)` if the swap succeeded (`tmp_name` now holds the displaced old live
    /// bytes and `live_rel` holds the new bytes), or `Ok(false)` if this OS/filesystem does not
    /// support atomic swap so the caller falls back to [`Self::replace_live_with_tmp_backup`] or
    /// claim-then-rename.
    fn exchange_tmp_with_live(&self, tmp_name: &str, live_rel: &str) -> Result<bool>;

    /// Atomically replaces existing `live_rel` with `.legion-sync/tmp/<tmp_name>`, moving the
    /// displaced old bytes to `.legion-sync/tmp/<backup_tmp>` (Windows `ReplaceFileW`).
    ///
    /// Returns `Ok(true)` if succeeded, or `Ok(false)` if unsupported on this platform.
    fn replace_live_with_tmp_backup(
        &self,
        _tmp_name: &str,
        _live_rel: &str,
        _backup_tmp: &str,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Atomically claims existing `live_rel` by moving it to `.legion-sync/tmp/<tmp_name>`
    /// without replacing any existing tmp file (used for safe deletes and claim-then-rename).
    fn move_live_to_tmp_no_replace(&self, live_rel: &str, tmp_name: &str) -> Result<()>;

    /// Atomically moves verified `.legion-sync/tmp/<tmp_name>` into
    /// `.legion-sync/bin/<bin_data_name>`, refusing to overwrite an existing bin file.
    fn move_tmp_to_bin_no_replace(&self, tmp_name: &str, bin_data_name: &str) -> Result<()>;

    /// Atomically moves a live file directly into `.legion-sync/bin/<bin_data_name>` without
    /// replacing any existing bin file (used for OS litter inside deleted directories).
    fn move_live_to_bin_no_replace(&self, live_rel: &str, bin_data_name: &str) -> Result<()>;

    /// Atomically writes a JSON sidecar metadata file into `.legion-sync/bin/<meta_name>`.
    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> Result<()>;

    /// Reads `.legion-sync/bin/<meta_name>`, returning `Ok(None)` if absent.
    fn read_bin_meta(&self, meta_name: &str) -> Result<Option<Vec<u8>>>;

    /// Inspects `.legion-sync/bin/<bin_name>`.
    fn stat_bin(&self, bin_name: &str) -> Result<Option<FsMeta>>;

    /// Computes the BLAKE3 digest and byte count of `.legion-sync/bin/<bin_name>`.
    fn hash_bin(&self, bin_name: &str) -> Result<(Hash, u64)>;

    /// Copies `.legion-sync/bin/<bin_data_name>` into `.legion-sync/tmp/<tmp_name>` and syncs it.
    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> Result<()>;

    /// Removes `.legion-sync/bin/<bin_name>` if it exists.
    fn remove_bin_file(&self, bin_name: &str) -> Result<()>;

    /// Lists filenames currently present inside `.legion-sync/bin/`.
    fn list_bin(&self) -> Result<Vec<String>>;
}

/// Remote member boundary used during pull-only synchronization, shaped for network transports
/// (handshake with incarnation, cancellable paginated record fetches, and ranged resumable timed
/// file fetches).
pub trait Peer {
    /// Member identifier of this peer.
    fn member_id(&self) -> &str;

    /// Exchanges folder/incarnation identity and protocol version before pulling.
    fn handshake(&self) -> Result<PeerHandshake>;

    /// Fetches index records created or updated after `(since_incarnation, since_seq)`, up to
    /// `limit` entries, from a single consistent snapshot of the peer's index.
    ///
    /// When `since_incarnation` differs from the peer's active incarnation, the peer starts from
    /// sequence `0`. Implementations should check `cancel` cooperatively if provided.
    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
        cancel: Option<&AtomicBool>,
    ) -> Result<RecordsPage>;

    /// Streams the bytes of `path` (starting at `options.offset`) whose full-file BLAKE3 digest
    /// must equal `expected_hash`.
    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
        options: &FetchOptions,
    ) -> Result<Box<dyn Read + Send>>;
}

/// Atomic batch of mutations committed in a single [`IndexStore::apply_batch`] transaction.
#[derive(Debug, Default, Clone)]
pub struct StoreBatch {
    /// Updated folder metadata, if changed.
    pub folder_meta: Option<FolderMeta>,
    /// Records to insert or replace (updating seq, tombstone, conflict, and case-folding
    /// secondary indexes automatically).
    pub upsert_records: Vec<IndexRecord>,
    /// Record paths to permanently remove (garbage-collected tombstones).
    pub remove_records: Vec<String>,
    /// Peer cursors to update (`peer -> PeerCursor`).
    pub set_peer_cursors: BTreeMap<MemberId, PeerCursor>,
    /// Peer cursors to remove (when a member leaves the folder).
    pub remove_peer_cursors: Vec<MemberId>,
    /// Bin entries to insert or update.
    pub upsert_bin: Vec<BinEntry>,
    /// Bin entry IDs to remove (restored or expired).
    pub remove_bin: Vec<String>,
    /// Held-back peer records to insert or update.
    pub upsert_held_back: Vec<HeldBackRecord>,
    /// Held-back record paths to remove (resolved, applied, or deleted).
    pub remove_held_back: Vec<String>,
    /// Deleted-file content hashes to record per path (persisted across tombstone GC so a
    /// rejoining or re-added member with an old copy never resurrects a deleted file).
    pub upsert_deleted_history: Vec<(String, Vec<Hash>)>,
    /// Deleted history paths to prune.
    pub remove_deleted_history: Vec<String>,
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
            && self.upsert_bin.is_empty()
            && self.remove_bin.is_empty()
            && self.upsert_held_back.is_empty()
            && self.remove_held_back.is_empty()
            && self.upsert_deleted_history.is_empty()
            && self.remove_deleted_history.is_empty()
            && self.set_pending_op.is_none()
    }
}

/// Durable, atomic key-value storage backend for a member's sync index.
pub trait IndexStore {
    /// Returns the schema version recorded in this store.
    fn schema_version(&self) -> Result<u32>;

    /// Loads the stored [`FolderMeta`], or `Ok(None)` if this store has not yet been initialized.
    fn load_meta(&self) -> Result<Option<FolderMeta>>;

    /// Loads one [`IndexRecord`] by exact relative path.
    fn get_record(&self, path: &str) -> Result<Option<IndexRecord>>;

    /// Visits every [`IndexRecord`] in lexicographical path order inside a single read snapshot.
    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()>;

    /// Visits only [`IndexRecord`]s whose path equals `subtree_root` or is a strict descendant
    /// of `subtree_root`, in lexicographical path order.
    fn scan_subtree_in_order(
        &self,
        subtree_root: &str,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()>;

    /// Returns `true` if any live record in the index is a strict descendant of `dir_path`.
    fn has_live_descendant(&self, dir_path: &str) -> Result<bool>;

    /// Returns all live exact paths in the index whose case-folded/normalized form equals
    /// `folded_path`.
    fn live_paths_for_folded(&self, folded_path: &str) -> Result<Vec<String>>;

    /// Reads a consistent page of records with `seq > effective_since_seq` (up to `limit`),
    /// together with the snapshot's incarnation, `local_seq`, direct `caller_acked` cursor, and
    /// head fingerprint.
    fn records_since(
        &self,
        caller_member: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage>;

    /// Returns all currently active tombstone records.
    fn list_tombstones(&self) -> Result<Vec<IndexRecord>>;

    /// Returns all currently active live conflict copy records.
    fn list_conflicts(&self) -> Result<Vec<IndexRecord>>;

    /// Loads all direct peer cursors (`peer -> PeerCursor`).
    fn load_peer_cursors(&self) -> Result<BTreeMap<MemberId, PeerCursor>>;

    /// Lists all entries currently tracked in `.legion-sync/bin`.
    fn list_bin_entries(&self) -> Result<Vec<BinEntry>>;

    /// Loads one bin entry by its unique ID.
    fn get_bin_entry(&self, id: &str) -> Result<Option<BinEntry>>;

    /// Lists all currently held-back peer records.
    fn list_held_back(&self) -> Result<Vec<HeldBackRecord>>;

    /// Returns the number of currently held-back records.
    fn held_back_count(&self) -> Result<usize>;

    /// Returns the recorded historical deleted content hashes for `path`, if any.
    fn get_deleted_history(&self, path: &str) -> Result<Vec<Hash>>;

    /// Loads the active write-ahead [`PendingOp`], if any.
    fn load_pending_op(&self) -> Result<Option<PendingOp>>;

    /// Returns the total number of records (live + tombstones) in the index.
    fn record_count(&self) -> Result<u64>;

    /// Returns the count of currently live regular files in the index (for the deletion brake).
    fn live_file_count(&self) -> Result<u64>;

    /// Applies every mutation in `batch` in a single atomic transaction.
    fn apply_batch(&mut self, batch: StoreBatch) -> Result<()>;
}
