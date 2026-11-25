//! Durable, ACID embedded index storage backed by [`redb`].
//!
//! # Why `redb`
//!
//! - **Pure Rust, no C toolchain**: compiles cleanly for static Linux `musl`, macOS, and Windows
//!   (`x86_64-pc-windows-gnu` and MSVC) with zero C libraries (only `libc` on Unix).
//! - **Crash safety**: copy-on-write B+trees with dual checksummed superblocks and fsynced commits
//!   ensure every [`IndexStore::apply_batch`] call is atomic and survives abrupt power loss.
//! - **Scale**: B+tree range queries handle 500,000+ entries with O(log N) lookups, O(log N + K)
//!   incremental sequence scans (`BY_SEQ`), O(T) tombstone GC (`TOMBSTONES`), and single-batch
//!   commits without rewriting the full index on single-file edits.
//! - **Licence & maintenance**: dual-licensed `MIT OR Apache-2.0` (compatible with this
//!   repository's MIT licence), actively maintained (`redb` 5.0, Rust edition 2024).

use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::codec::{decode_record, decode_string_list, encode_record, encode_string_list};
use crate::error::{Result, SyncError};
use crate::model::{
    BinEntry, CausalMatrix, FolderMeta, HeldBackRecord, IndexRecord, MemberId, PendingOp,
    RecordsPage,
};
use crate::path::{fold_case, is_descendant_of};
use crate::traits::{IndexStore, StoreBatch};

const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("records");
const BY_SEQ_TABLE: TableDefinition<u64, &str> = TableDefinition::new("by_seq");
const TOMBSTONES_TABLE: TableDefinition<&str, ()> = TableDefinition::new("tombstones");
const CONFLICTS_TABLE: TableDefinition<&str, ()> = TableDefinition::new("conflicts");
const FOLDED_LIVE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("folded_live");
const PEER_CURSORS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("peer_cursors");
const BIN_ENTRIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("bin_entries");
const HELD_BACK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("held_back");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const META_KEY_FOLDER: &str = "folder_meta";
const META_KEY_MATRIX: &str = "causal_matrix";
const META_KEY_PENDING_OP: &str = "pending_op";

/// Durable [`IndexStore`] implementation backed by a single `redb` database file.
pub struct RedbIndexStore {
    db: Database,
}

impl RedbIndexStore {
    /// Opens an existing `redb` index file at `path` or creates a new one with all tables
    /// initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| SyncError::from_io(Some(parent.display().to_string()), err))?;
        }

        let db = Database::create(path).map_err(|err| SyncError::Storage {
            context: format!("open redb index at {}", path.display()),
            detail: err.to_string(),
        })?;

        // Ensure every table exists upfront so read transactions never see missing-table errors.
        let txn = db.begin_write().map_err(|err| SyncError::Storage {
            context: "initialize redb tables".to_string(),
            detail: err.to_string(),
        })?;
        {
            let _ = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
            let _ = txn.open_table(BY_SEQ_TABLE).map_err(table_err)?;
            let _ = txn.open_table(TOMBSTONES_TABLE).map_err(table_err)?;
            let _ = txn.open_table(CONFLICTS_TABLE).map_err(table_err)?;
            let _ = txn.open_table(FOLDED_LIVE_TABLE).map_err(table_err)?;
            let _ = txn.open_table(PEER_CURSORS_TABLE).map_err(table_err)?;
            let _ = txn.open_table(BIN_ENTRIES_TABLE).map_err(table_err)?;
            let _ = txn.open_table(HELD_BACK_TABLE).map_err(table_err)?;
            let _ = txn.open_table(META_TABLE).map_err(table_err)?;
        }
        txn.commit().map_err(|err| SyncError::Storage {
            context: "commit initial redb tables".to_string(),
            detail: err.to_string(),
        })?;

        Ok(Self { db })
    }
}

fn storage_err(context: &str, err: impl std::fmt::Display) -> SyncError {
    SyncError::Storage {
        context: context.to_string(),
        detail: err.to_string(),
    }
}

fn table_err(err: impl std::fmt::Display) -> SyncError {
    storage_err("open redb table", err)
}

fn decode_json<T: serde::de::DeserializeOwned>(context: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|err| SyncError::CorruptData {
        context: context.to_string(),
        detail: err.to_string(),
    })
}

fn encode_json<T: serde::Serialize>(context: &str, value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|err| SyncError::Storage {
        context: context.to_string(),
        detail: err.to_string(),
    })
}

fn unindex_record_in_txn(
    old: &IndexRecord,
    by_seq: &mut redb::Table<'_, u64, &str>,
    tombstones: &mut redb::Table<'_, &str, ()>,
    conflicts: &mut redb::Table<'_, &str, ()>,
    folded_live: &mut redb::Table<'_, &str, &[u8]>,
) -> Result<()> {
    by_seq
        .remove(old.seq)
        .map_err(|e| storage_err("remove by_seq", e))?;
    if old.deleted {
        tombstones
            .remove(old.path.as_str())
            .map_err(|e| storage_err("remove tombstone index", e))?;
    } else {
        if old.conflict_of.is_some() {
            conflicts
                .remove(old.path.as_str())
                .map_err(|e| storage_err("remove conflict index", e))?;
        }
        let folded = fold_case(&old.path);
        let existing = folded_live
            .get(folded.as_str())
            .map_err(|e| storage_err("read folded_live", e))?
            .map(|guard| decode_string_list(guard.value()))
            .transpose()?;
        if let Some(mut list) = existing {
            list.retain(|p| p != &old.path);
            if list.is_empty() {
                folded_live
                    .remove(folded.as_str())
                    .map_err(|e| storage_err("remove folded_live", e))?;
            } else {
                let encoded = encode_string_list(&list);
                folded_live
                    .insert(folded.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("update folded_live", e))?;
            }
        }
    }
    Ok(())
}

fn index_record_in_txn(
    rec: &IndexRecord,
    by_seq: &mut redb::Table<'_, u64, &str>,
    tombstones: &mut redb::Table<'_, &str, ()>,
    conflicts: &mut redb::Table<'_, &str, ()>,
    folded_live: &mut redb::Table<'_, &str, &[u8]>,
) -> Result<()> {
    by_seq
        .insert(rec.seq, rec.path.as_str())
        .map_err(|e| storage_err("insert by_seq", e))?;
    if rec.deleted {
        tombstones
            .insert(rec.path.as_str(), ())
            .map_err(|e| storage_err("insert tombstone index", e))?;
    } else {
        if rec.conflict_of.is_some() {
            conflicts
                .insert(rec.path.as_str(), ())
                .map_err(|e| storage_err("insert conflict index", e))?;
        }
        let folded = fold_case(&rec.path);
        let mut list = folded_live
            .get(folded.as_str())
            .map_err(|e| storage_err("read folded_live", e))?
            .map(|guard| decode_string_list(guard.value()))
            .transpose()?
            .unwrap_or_default();
        if !list.iter().any(|p| p == &rec.path) {
            list.push(rec.path.clone());
            list.sort();
            let encoded = encode_string_list(&list);
            folded_live
                .insert(folded.as_str(), encoded.as_slice())
                .map_err(|e| storage_err("insert folded_live", e))?;
        }
    }
    Ok(())
}

impl IndexStore for RedbIndexStore {
    fn load_meta(&self) -> Result<Option<FolderMeta>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(META_TABLE).map_err(table_err)?;
        let Some(guard) = table
            .get(META_KEY_FOLDER)
            .map_err(|e| storage_err("read folder_meta", e))?
        else {
            return Ok(None);
        };
        Ok(Some(decode_json("folder_meta", guard.value())?))
    }

    fn get_record(&self, path: &str) -> Result<Option<IndexRecord>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let Some(guard) = table.get(path).map_err(|e| storage_err("get record", e))? else {
            return Ok(None);
        };
        Ok(Some(decode_record(guard.value())?))
    }

    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let iter = table
            .iter()
            .map_err(|e| storage_err("iterate records", e))?;
        for item in iter {
            let (_, val) = item.map_err(|e| storage_err("step records iterator", e))?;
            let rec = decode_record(val.value())?;
            if !visitor(rec)? {
                break;
            }
        }
        Ok(())
    }

    fn has_live_descendant(&self, dir_path: &str) -> Result<bool> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let prefix = format!("{dir_path}/");
        let range = table
            .range((Bound::Included(prefix.as_str()), Bound::Unbounded))
            .map_err(|e| storage_err("range records", e))?;
        for item in range {
            let (k, v) = item.map_err(|e| storage_err("step range records", e))?;
            let key = k.value();
            if !is_descendant_of(key, dir_path) {
                break;
            }
            let rec = decode_record(v.value())?;
            if !rec.deleted {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn live_paths_for_folded(&self, folded_path: &str) -> Result<Vec<String>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(FOLDED_LIVE_TABLE).map_err(table_err)?;
        let Some(guard) = table
            .get(folded_path)
            .map_err(|e| storage_err("get folded_live", e))?
        else {
            return Ok(Vec::new());
        };
        decode_string_list(guard.value())
    }

    fn records_since(&self, since_seq: u64, limit: usize) -> Result<RecordsPage> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let meta_table = txn.open_table(META_TABLE).map_err(table_err)?;
        let snapshot_seq = match meta_table
            .get(META_KEY_FOLDER)
            .map_err(|e| storage_err("read folder_meta", e))?
        {
            Some(g) => decode_json::<FolderMeta>("folder_meta", g.value())?.local_seq,
            None => 0,
        };
        let matrix = match meta_table
            .get(META_KEY_MATRIX)
            .map_err(|e| storage_err("read causal_matrix", e))?
        {
            Some(g) => decode_json::<CausalMatrix>("causal_matrix", g.value())?,
            None => CausalMatrix::new(),
        };

        let by_seq = txn.open_table(BY_SEQ_TABLE).map_err(table_err)?;
        let records_table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let range = by_seq
            .range((Bound::Excluded(since_seq), Bound::Unbounded))
            .map_err(|e| storage_err("range by_seq", e))?;

        let mut records = Vec::new();
        let mut has_more = false;
        for item in range {
            let (_, path_guard) = item.map_err(|e| storage_err("step by_seq", e))?;
            if records.len() >= limit {
                has_more = true;
                break;
            }
            if let Some(rec_guard) = records_table
                .get(path_guard.value())
                .map_err(|e| storage_err("lookup record by_seq", e))?
            {
                records.push(decode_record(rec_guard.value())?.for_wire());
            }
        }

        Ok(RecordsPage {
            records,
            snapshot_seq,
            has_more,
            matrix,
        })
    }

    fn list_tombstones(&self) -> Result<Vec<IndexRecord>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let tombstones = txn.open_table(TOMBSTONES_TABLE).map_err(table_err)?;
        let records = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let iter = tombstones
            .iter()
            .map_err(|e| storage_err("iterate tombstones", e))?;
        let mut out = Vec::new();
        for item in iter {
            let (k, _) = item.map_err(|e| storage_err("step tombstones", e))?;
            if let Some(g) = records
                .get(k.value())
                .map_err(|e| storage_err("read tombstone record", e))?
            {
                out.push(decode_record(g.value())?);
            }
        }
        Ok(out)
    }

    fn list_conflicts(&self) -> Result<Vec<IndexRecord>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let conflicts = txn.open_table(CONFLICTS_TABLE).map_err(table_err)?;
        let records = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let iter = conflicts
            .iter()
            .map_err(|e| storage_err("iterate conflicts", e))?;
        let mut out = Vec::new();
        for item in iter {
            let (k, _) = item.map_err(|e| storage_err("step conflicts", e))?;
            if let Some(g) = records
                .get(k.value())
                .map_err(|e| storage_err("read conflict record", e))?
            {
                out.push(decode_record(g.value())?);
            }
        }
        Ok(out)
    }

    fn load_peer_cursors(&self) -> Result<BTreeMap<MemberId, u64>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(PEER_CURSORS_TABLE).map_err(table_err)?;
        let iter = table
            .iter()
            .map_err(|e| storage_err("iterate peer_cursors", e))?;
        let mut out = BTreeMap::new();
        for item in iter {
            let (k, v) = item.map_err(|e| storage_err("step peer_cursors", e))?;
            out.insert(k.value().to_string(), v.value());
        }
        Ok(out)
    }

    fn load_causal_matrix(&self) -> Result<CausalMatrix> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(META_TABLE).map_err(table_err)?;
        let Some(guard) = table
            .get(META_KEY_MATRIX)
            .map_err(|e| storage_err("read causal_matrix", e))?
        else {
            return Ok(CausalMatrix::new());
        };
        decode_json("causal_matrix", guard.value())
    }

    fn list_bin_entries(&self) -> Result<Vec<BinEntry>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(BIN_ENTRIES_TABLE).map_err(table_err)?;
        let iter = table
            .iter()
            .map_err(|e| storage_err("iterate bin_entries", e))?;
        let mut out = Vec::new();
        for item in iter {
            let (_, v) = item.map_err(|e| storage_err("step bin_entries", e))?;
            out.push(decode_json("bin_entry", v.value())?);
        }
        Ok(out)
    }

    fn get_bin_entry(&self, id: &str) -> Result<Option<BinEntry>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(BIN_ENTRIES_TABLE).map_err(table_err)?;
        let Some(guard) = table.get(id).map_err(|e| storage_err("get bin_entry", e))? else {
            return Ok(None);
        };
        Ok(Some(decode_json("bin_entry", guard.value())?))
    }

    fn list_held_back(&self) -> Result<Vec<HeldBackRecord>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(HELD_BACK_TABLE).map_err(table_err)?;
        let iter = table
            .iter()
            .map_err(|e| storage_err("iterate held_back", e))?;
        let mut out = Vec::new();
        for item in iter {
            let (_, v) = item.map_err(|e| storage_err("step held_back", e))?;
            out.push(decode_json("held_back", v.value())?);
        }
        Ok(out)
    }

    fn load_pending_op(&self) -> Result<Option<PendingOp>> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(META_TABLE).map_err(table_err)?;
        let Some(guard) = table
            .get(META_KEY_PENDING_OP)
            .map_err(|e| storage_err("read pending_op", e))?
        else {
            return Ok(None);
        };
        Ok(Some(decode_json("pending_op", guard.value())?))
    }

    fn record_count(&self) -> Result<u64> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        table.len().map_err(|e| storage_err("count records", e))
    }

    fn apply_batch(&mut self, batch: StoreBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let txn = self
            .db
            .begin_write()
            .map_err(|e| storage_err("begin write", e))?;
        {
            let mut meta_table = txn.open_table(META_TABLE).map_err(table_err)?;
            let mut records = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
            let mut by_seq = txn.open_table(BY_SEQ_TABLE).map_err(table_err)?;
            let mut tombstones = txn.open_table(TOMBSTONES_TABLE).map_err(table_err)?;
            let mut conflicts = txn.open_table(CONFLICTS_TABLE).map_err(table_err)?;
            let mut folded_live = txn.open_table(FOLDED_LIVE_TABLE).map_err(table_err)?;
            let mut peer_cursors = txn.open_table(PEER_CURSORS_TABLE).map_err(table_err)?;
            let mut bin_entries = txn.open_table(BIN_ENTRIES_TABLE).map_err(table_err)?;
            let mut held_back = txn.open_table(HELD_BACK_TABLE).map_err(table_err)?;

            if let Some(meta) = batch.folder_meta {
                let encoded = encode_json("folder_meta", &meta)?;
                meta_table
                    .insert(META_KEY_FOLDER, encoded.as_slice())
                    .map_err(|e| storage_err("write folder_meta", e))?;
            }

            for rec in batch.upsert_records {
                let prev = records
                    .get(rec.path.as_str())
                    .map_err(|e| storage_err("read previous record", e))?
                    .map(|g| decode_record(g.value()))
                    .transpose()?;
                if let Some(old) = prev {
                    unindex_record_in_txn(
                        &old,
                        &mut by_seq,
                        &mut tombstones,
                        &mut conflicts,
                        &mut folded_live,
                    )?;
                }
                let encoded = encode_record(&rec);
                records
                    .insert(rec.path.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert record", e))?;
                index_record_in_txn(
                    &rec,
                    &mut by_seq,
                    &mut tombstones,
                    &mut conflicts,
                    &mut folded_live,
                )?;
            }

            for path in batch.remove_records {
                let prev = records
                    .remove(path.as_str())
                    .map_err(|e| storage_err("remove record", e))?
                    .map(|g| decode_record(g.value()))
                    .transpose()?;
                if let Some(old) = prev {
                    unindex_record_in_txn(
                        &old,
                        &mut by_seq,
                        &mut tombstones,
                        &mut conflicts,
                        &mut folded_live,
                    )?;
                }
            }

            for (peer, seq) in batch.set_peer_cursors {
                peer_cursors
                    .insert(peer.as_str(), seq)
                    .map_err(|e| storage_err("set peer_cursor", e))?;
            }

            for peer in batch.remove_peer_cursors {
                peer_cursors
                    .remove(peer.as_str())
                    .map_err(|e| storage_err("remove peer_cursor", e))?;
            }

            if let Some(matrix) = batch.causal_matrix {
                let encoded = encode_json("causal_matrix", &matrix)?;
                meta_table
                    .insert(META_KEY_MATRIX, encoded.as_slice())
                    .map_err(|e| storage_err("write causal_matrix", e))?;
            }

            for entry in batch.upsert_bin {
                let encoded = encode_json("bin_entry", &entry)?;
                bin_entries
                    .insert(entry.id.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert bin_entry", e))?;
            }

            for id in batch.remove_bin {
                bin_entries
                    .remove(id.as_str())
                    .map_err(|e| storage_err("remove bin_entry", e))?;
            }

            for hb in batch.upsert_held_back {
                let encoded = encode_json("held_back", &hb)?;
                held_back
                    .insert(hb.record.path.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert held_back", e))?;
            }

            for path in batch.remove_held_back {
                held_back
                    .remove(path.as_str())
                    .map_err(|e| storage_err("remove held_back", e))?;
            }

            if let Some(pending) = batch.set_pending_op {
                match pending {
                    Some(op) => {
                        let encoded = encode_json("pending_op", &op)?;
                        meta_table
                            .insert(META_KEY_PENDING_OP, encoded.as_slice())
                            .map_err(|e| storage_err("write pending_op", e))?;
                    }
                    None => {
                        meta_table
                            .remove(META_KEY_PENDING_OP)
                            .map_err(|e| storage_err("clear pending_op", e))?;
                    }
                }
            }
        }

        txn.commit()
            .map_err(|e| storage_err("commit redb batch", e))?;
        Ok(())
    }
}
