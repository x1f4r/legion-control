//! Fast, atomic in-memory [`IndexStore`] implementation used by tests and the deterministic
//! simulator.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use legion_xfer::{EntryKind, Hash};

use crate::error::Result;
use crate::model::{
    BinEntry, FolderMeta, HeldBackRecord, INDEX_SCHEMA_VERSION, IndexRecord,
    MAX_DELETED_HISTORY_PATHS, MemberId, PeerCursor, PendingOp, RecordsPage,
};
use crate::path::fold_case;
use crate::traits::{IndexStore, StoreBatch};

/// In-memory [`IndexStore`] mirroring the exact transactional semantics and secondary indexes of
/// [`crate::RedbIndexStore`].
#[derive(Debug, Clone)]
pub struct MemoryIndexStore {
    schema_version: u32,
    meta: Option<FolderMeta>,
    records: BTreeMap<String, IndexRecord>,
    by_seq: BTreeMap<u64, String>,
    tombstones: BTreeSet<String>,
    conflicts: BTreeSet<String>,
    folded_live: BTreeMap<String, BTreeSet<String>>,
    live_files: u64,
    peer_cursors: BTreeMap<MemberId, PeerCursor>,
    bin_entries: BTreeMap<String, BinEntry>,
    held_back: BTreeMap<String, HeldBackRecord>,
    deleted_history: BTreeMap<String, Vec<Hash>>,
    pending_op: Option<PendingOp>,
}

impl Default for MemoryIndexStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryIndexStore {
    /// Creates an empty in-memory index store at [`INDEX_SCHEMA_VERSION`].
    pub fn new() -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            meta: None,
            records: BTreeMap::new(),
            by_seq: BTreeMap::new(),
            tombstones: BTreeSet::new(),
            conflicts: BTreeSet::new(),
            folded_live: BTreeMap::new(),
            live_files: 0,
            peer_cursors: BTreeMap::new(),
            bin_entries: BTreeMap::new(),
            held_back: BTreeMap::new(),
            deleted_history: BTreeMap::new(),
            pending_op: None,
        }
    }

    /// Creates an empty in-memory store stamped with an explicit schema version (for testing
    /// newer-schema refusal).
    pub fn with_schema_version(schema_version: u32) -> Self {
        let mut s = Self::new();
        s.schema_version = schema_version;
        s
    }

    fn unindex_record(&mut self, old: &IndexRecord) {
        if self.by_seq.get(&old.seq).map(String::as_str) == Some(old.path.as_str()) {
            self.by_seq.remove(&old.seq);
        }
        if old.deleted {
            self.tombstones.remove(&old.path);
        } else {
            if old.kind == EntryKind::File {
                self.live_files = self.live_files.saturating_sub(1);
            }
            self.conflicts.remove(&old.path);
            let folded = fold_case(&old.path);
            if let Some(set) = self.folded_live.get_mut(&folded) {
                set.remove(&old.path);
                if set.is_empty() {
                    self.folded_live.remove(&folded);
                }
            }
        }
    }

    fn index_record(&mut self, rec: &IndexRecord) {
        self.by_seq.insert(rec.seq, rec.path.clone());
        if rec.deleted {
            self.tombstones.insert(rec.path.clone());
        } else {
            if rec.kind == EntryKind::File {
                self.live_files = self.live_files.saturating_add(1);
            }
            if rec.conflict_of.is_some() && !rec.conflict_dismissed {
                self.conflicts.insert(rec.path.clone());
            }
            let folded = fold_case(&rec.path);
            self.folded_live
                .entry(folded)
                .or_default()
                .insert(rec.path.clone());
        }
    }
}

impl IndexStore for MemoryIndexStore {
    fn schema_version(&self) -> Result<u32> {
        Ok(self.schema_version)
    }

    fn load_meta(&self) -> Result<Option<FolderMeta>> {
        Ok(self.meta.clone())
    }

    fn get_record(&self, path: &str) -> Result<Option<IndexRecord>> {
        Ok(self.records.get(path).cloned())
    }

    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()> {
        for rec in self.records.values() {
            if !visitor(rec.clone())? {
                break;
            }
        }
        Ok(())
    }

    fn scan_subtree_in_order(
        &self,
        subtree_root: &str,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()> {
        if let Some(exact) = self.records.get(subtree_root)
            && !visitor(exact.clone())?
        {
            return Ok(());
        }
        let root_bytes = subtree_root.as_bytes();
        for (path, rec) in self
            .records
            .range::<str, _>((Bound::Excluded(subtree_root), Bound::Unbounded))
        {
            let pb = path.as_bytes();
            if !pb.starts_with(root_bytes) {
                break;
            }
            if pb.get(root_bytes.len()) != Some(&b'/') {
                if pb.get(root_bytes.len()).is_some_and(|&b| b > b'/') {
                    break;
                }
                continue;
            }
            if !visitor(rec.clone())? {
                break;
            }
        }
        Ok(())
    }

    fn has_live_descendant(&self, dir_path: &str) -> Result<bool> {
        let dir_bytes = dir_path.as_bytes();
        for (path, rec) in self
            .records
            .range::<str, _>((Bound::Excluded(dir_path), Bound::Unbounded))
        {
            let pb = path.as_bytes();
            if !pb.starts_with(dir_bytes) {
                break;
            }
            if pb.get(dir_bytes.len()) != Some(&b'/') {
                if pb.get(dir_bytes.len()).is_some_and(|&b| b > b'/') {
                    break;
                }
                continue;
            }
            if !rec.deleted {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn live_paths_for_folded(&self, folded_path: &str) -> Result<Vec<String>> {
        Ok(self
            .folded_live
            .get(folded_path)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default())
    }

    fn records_since(
        &self,
        caller_member: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage> {
        let (incarnation, snapshot_seq, head_fingerprint) = self
            .meta
            .as_ref()
            .map(|m| (m.incarnation, m.local_seq, m.head_fingerprint))
            .unwrap_or((0, 0, None));

        // If the caller's cursor is from an older incarnation, serve from sequence 0.
        let effective_since = if since_incarnation == incarnation {
            since_seq
        } else {
            0
        };

        let mut records = Vec::new();
        let mut has_more = false;
        for (_, path) in self
            .by_seq
            .range((Bound::Excluded(effective_since), Bound::Unbounded))
        {
            if records.len() >= limit {
                has_more = true;
                break;
            }
            if let Some(rec) = self.records.get(path) {
                records.push(rec.for_wire());
            }
        }

        let (caller_acked, caller_acked_fingerprint) = self
            .peer_cursors
            .get(caller_member)
            .filter(|c| c.incarnation != 0)
            .map_or((None, None), |c| {
                (Some((c.incarnation, c.seq)), c.head_fingerprint)
            });

        Ok(RecordsPage {
            incarnation,
            records,
            snapshot_seq,
            has_more,
            caller_acked,
            caller_acked_fingerprint,
            head_fingerprint,
        })
    }

    fn list_tombstones(&self) -> Result<Vec<IndexRecord>> {
        let mut out = Vec::with_capacity(self.tombstones.len());
        for path in &self.tombstones {
            if let Some(rec) = self.records.get(path) {
                out.push(rec.clone());
            }
        }
        Ok(out)
    }

    fn list_conflicts(&self) -> Result<Vec<IndexRecord>> {
        let mut out = Vec::with_capacity(self.conflicts.len());
        for path in &self.conflicts {
            if let Some(rec) = self.records.get(path) {
                out.push(rec.clone());
            }
        }
        Ok(out)
    }

    fn load_peer_cursors(&self) -> Result<BTreeMap<MemberId, PeerCursor>> {
        Ok(self.peer_cursors.clone())
    }

    fn list_bin_entries(&self) -> Result<Vec<BinEntry>> {
        Ok(self.bin_entries.values().cloned().collect())
    }

    fn get_bin_entry(&self, id: &str) -> Result<Option<BinEntry>> {
        Ok(self.bin_entries.get(id).cloned())
    }

    fn list_held_back(&self) -> Result<Vec<HeldBackRecord>> {
        Ok(self.held_back.values().cloned().collect())
    }

    fn held_back_count(&self) -> Result<usize> {
        Ok(self.held_back.len())
    }

    fn get_deleted_history(&self, path: &str) -> Result<Vec<Hash>> {
        Ok(self.deleted_history.get(path).cloned().unwrap_or_default())
    }

    fn load_pending_op(&self) -> Result<Option<PendingOp>> {
        Ok(self.pending_op.clone())
    }

    fn record_count(&self) -> Result<u64> {
        Ok(self.records.len() as u64)
    }

    fn live_file_count(&self) -> Result<u64> {
        Ok(self.live_files)
    }

    fn apply_batch(&mut self, batch: StoreBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        if let Some(meta) = batch.folder_meta {
            self.meta = Some(meta);
        }
        for path in batch.remove_records {
            if let Some(old) = self.records.remove(&path) {
                self.unindex_record(&old);
            }
        }
        for rec in batch.upsert_records {
            if let Some(old) = self.records.remove(&rec.path) {
                self.unindex_record(&old);
            }
            self.index_record(&rec);
            self.records.insert(rec.path.clone(), rec);
        }
        for peer in batch.remove_peer_cursors {
            self.peer_cursors.remove(&peer);
        }
        for (peer, cursor) in batch.set_peer_cursors {
            self.peer_cursors.insert(peer, cursor);
        }
        for id in batch.remove_bin {
            self.bin_entries.remove(&id);
        }
        for entry in batch.upsert_bin {
            self.bin_entries.insert(entry.id.clone(), entry);
        }
        for path in batch.remove_held_back {
            self.held_back.remove(&path);
        }
        for item in batch.upsert_held_back {
            self.held_back.insert(item.record.path.clone(), item);
        }
        for path in batch.remove_deleted_history {
            self.deleted_history.remove(&path);
        }
        for (path, hashes) in batch.upsert_deleted_history {
            if hashes.is_empty() {
                self.deleted_history.remove(&path);
            } else {
                if !self.deleted_history.contains_key(&path)
                    && self.deleted_history.len() >= MAX_DELETED_HISTORY_PATHS
                    && let Some(first_key) = self.deleted_history.keys().next().cloned()
                {
                    self.deleted_history.remove(&first_key);
                }
                self.deleted_history.insert(path, hashes);
            }
        }
        if let Some(op_update) = batch.set_pending_op {
            self.pending_op = op_update;
        }
        Ok(())
    }
}
