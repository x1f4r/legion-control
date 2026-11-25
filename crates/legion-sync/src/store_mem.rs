//! Fast deterministic in-memory [`IndexStore`] for unit tests and the multi-member simulator.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use crate::error::Result;
use crate::model::{
    BinEntry, CausalMatrix, FolderMeta, HeldBackRecord, IndexRecord, MemberId, PendingOp,
    RecordsPage,
};
use crate::path::{fold_case, is_descendant_of};
use crate::traits::{IndexStore, StoreBatch};

/// In-memory implementation of [`IndexStore`] backed by deterministic `BTreeMap` structures.
///
/// Mirrors the exact transactional semantics and secondary indexes of [`crate::RedbIndexStore`]
/// without disk I/O so the simulator can execute thousands of multi-member seeds per second.
#[derive(Debug, Clone, Default)]
pub struct MemoryIndexStore {
    meta: Option<FolderMeta>,
    records: BTreeMap<String, IndexRecord>,
    by_seq: BTreeMap<u64, String>,
    tombstones: BTreeSet<String>,
    conflicts: BTreeSet<String>,
    folded_live: BTreeMap<String, BTreeSet<String>>,
    peer_cursors: BTreeMap<MemberId, u64>,
    causal_matrix: CausalMatrix,
    bin_entries: BTreeMap<String, BinEntry>,
    held_back: BTreeMap<String, HeldBackRecord>,
    pending_op: Option<PendingOp>,
}

impl MemoryIndexStore {
    /// Creates an empty, uninitialized in-memory index store.
    pub fn new() -> Self {
        Self::default()
    }

    fn unindex_record(&mut self, old: &IndexRecord) {
        self.by_seq.remove(&old.seq);
        if old.deleted {
            self.tombstones.remove(&old.path);
        } else {
            if old.conflict_of.is_some() {
                self.conflicts.remove(&old.path);
            }
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
            if rec.conflict_of.is_some() {
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

    fn has_live_descendant(&self, dir_path: &str) -> Result<bool> {
        let prefix = format!("{dir_path}/");
        for (path, rec) in self
            .records
            .range::<str, _>((Bound::Included(prefix.as_str()), Bound::Unbounded))
        {
            if !is_descendant_of(path, dir_path) {
                break;
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

    fn records_since(&self, since_seq: u64, limit: usize) -> Result<RecordsPage> {
        let snapshot_seq = self.meta.as_ref().map_or(0, |m| m.local_seq);
        let mut records = Vec::new();
        let mut has_more = false;

        for (_, path) in self
            .by_seq
            .range((Bound::Excluded(since_seq), Bound::Unbounded))
        {
            if records.len() >= limit {
                has_more = true;
                break;
            }
            if let Some(rec) = self.records.get(path) {
                records.push(rec.for_wire());
            }
        }

        Ok(RecordsPage {
            records,
            snapshot_seq,
            has_more,
            matrix: self.causal_matrix.clone(),
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

    fn load_peer_cursors(&self) -> Result<BTreeMap<MemberId, u64>> {
        Ok(self.peer_cursors.clone())
    }

    fn load_causal_matrix(&self) -> Result<CausalMatrix> {
        Ok(self.causal_matrix.clone())
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

    fn load_pending_op(&self) -> Result<Option<PendingOp>> {
        Ok(self.pending_op.clone())
    }

    fn record_count(&self) -> Result<u64> {
        Ok(self.records.len() as u64)
    }

    fn apply_batch(&mut self, batch: StoreBatch) -> Result<()> {
        if let Some(meta) = batch.folder_meta {
            self.meta = Some(meta);
        }

        for rec in batch.upsert_records {
            if let Some(old) = self.records.remove(&rec.path) {
                self.unindex_record(&old);
            }
            self.index_record(&rec);
            self.records.insert(rec.path.clone(), rec);
        }

        for path in batch.remove_records {
            if let Some(old) = self.records.remove(&path) {
                self.unindex_record(&old);
            }
        }

        for (peer, seq) in batch.set_peer_cursors {
            self.peer_cursors.insert(peer, seq);
        }

        for peer in batch.remove_peer_cursors {
            self.peer_cursors.remove(&peer);
        }

        if let Some(matrix) = batch.causal_matrix {
            self.causal_matrix = matrix;
        }

        for entry in batch.upsert_bin {
            self.bin_entries.insert(entry.id.clone(), entry);
        }

        for id in batch.remove_bin {
            self.bin_entries.remove(&id);
        }

        for hb in batch.upsert_held_back {
            self.held_back.insert(hb.record.path.clone(), hb);
        }

        for path in batch.remove_held_back {
            self.held_back.remove(&path);
        }

        if let Some(pending) = batch.set_pending_op {
            self.pending_op = pending;
        }

        Ok(())
    }
}
