//! Durable, ACID embedded index storage backed by [`redb`].
//!
//! # Why `redb`
//!
//! - **Pure Rust, no C toolchain**: compiles cleanly for static Linux `musl`, macOS, and Windows
//!   (`x86_64-pc-windows-gnu` and MSVC) with zero C libraries (only `libc` on Unix).
//! - **Crash safety**: copy-on-write B+trees with dual checksummed superblocks and fsynced commits
//!   ensure every [`IndexStore::apply_batch`] call is atomic and survives abrupt power loss.
//! - **Scale**: B+tree range queries handle 500,000+ entries with O(log N) lookups, O(log N + K)
//!   incremental sequence scans (`BY_SEQ`) and subtree scans, O(T) tombstone GC (`TOMBSTONES`),
//!   and batched single-transaction commits.
//! - **Schema versioning**: persists `schema_version` in `META` and refuses cleanly with
//!   [`SyncError::SchemaTooNew`] if opened by an older binary.
//! - **Licence & maintenance**: dual-licensed `MIT OR Apache-2.0` (compatible with this
//!   repository's MIT licence), actively maintained (`redb` 5.0, Rust edition 2024).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::Arc;

use legion_xfer::{EntryKind, Hash};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use crate::codec::{
    decode_hash_list, decode_record, decode_string_list, encode_hash_list, encode_record,
    encode_string_list,
};
use crate::error::{INDEX_DAMAGED_PROBLEM_SENTENCE, Result, SyncError};
use crate::model::{
    BinEntry, DeferredItem, FolderMeta, HeldBackRecord, INDEX_SCHEMA_VERSION, IndexRecord,
    MAX_DELETED_HISTORY_PATHS, MemberId, PeerCursor, PendingOp, RecordsPage, SyncProblem,
};
use crate::path::{fold_case, is_descendant_of};
use crate::traits::{IndexStore, StoreBatch};

const RECORDS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("records");
const BY_SEQ_TABLE: TableDefinition<u64, &str> = TableDefinition::new("by_seq");
const TOMBSTONES_TABLE: TableDefinition<&str, ()> = TableDefinition::new("tombstones");
const CONFLICTS_TABLE: TableDefinition<&str, ()> = TableDefinition::new("conflicts");
const FOLDED_LIVE_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("folded_live");
const PEER_CURSORS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("peer_cursors_v2");
const BIN_ENTRIES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("bin_entries");
const HELD_BACK_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("held_back_v2");
const DELETED_HISTORY_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("deleted_history");
const STAGED_TMPS_TABLE: TableDefinition<&str, ()> = TableDefinition::new("staged_tmps");
const SEQ_FINGERPRINTS_TABLE: TableDefinition<u64, &[u8]> =
    TableDefinition::new("seq_fingerprints");
const PROBLEMS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("problems");
const DEFERRALS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("deferrals");
const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

fn peer_path_key(peer: &str, path: &str) -> String {
    format!("{peer}\0{path}")
}

const META_KEY_SCHEMA: &str = "schema_version";
const META_KEY_FOLDER: &str = "folder_meta";
const META_KEY_PENDING_OP: &str = "pending_op";
const META_KEY_LIVE_FILES: &str = "live_file_count";

/// Default `redb` page cache size (16 MiB) to keep idle runner RSS compact even on large folders.
const DEFAULT_REDB_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// Durable [`IndexStore`] implementation backed by a single `redb` database file.
///
/// Cloning shares the underlying [`redb::Database`] handle, enabling lock-free concurrent MVCC
/// read transactions (e.g. serving peer `records_since` or status queries) while a single writer
/// updates the index.
#[derive(Clone)]
pub struct RedbIndexStore {
    db: Arc<Database>,
    schema_version: u32,
    path_label: String,
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "storage backend panicked".to_string()
    }
}

fn damaged_err(label: &str, detail: impl std::fmt::Display) -> SyncError {
    SyncError::IndexDamaged {
        folder_id: label.to_string(),
        sentence: INDEX_DAMAGED_PROBLEM_SENTENCE.to_string(),
        detail: detail.to_string(),
    }
}

fn catch_redb<T>(label: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(res) => res,
        Err(payload) => Err(damaged_err(
            label,
            format!("storage panic: {}", panic_payload_message(&*payload)),
        )),
    }
}

impl RedbIndexStore {
    /// Opens an existing `redb` index file at `path` (verifying its integrity and refusing 0-byte,
    /// truncated, corrupt, or panicking files with [`SyncError::IndexDamaged`]) or creates a new
    /// one with all tables initialized when `path` does not yet exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let path_label = path.display().to_string();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| SyncError::from_io(Some(parent.display().to_string()), err))?;
        }

        let existed = match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(damaged_err(
                        &path_label,
                        "index path exists but is not a regular file",
                    ));
                }
                if meta.len() == 0 {
                    return Err(damaged_err(&path_label, "index file is 0 bytes"));
                }
                true
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
            Err(err) => return Err(SyncError::from_io(Some(path_label), err)),
        };

        catch_redb(&path_label, || {
            let mut db = if existed {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    match Database::builder()
                        .set_cache_size(DEFAULT_REDB_CACHE_BYTES)
                        .open(path)
                    {
                        Ok(db) => break db,
                        Err(redb::DatabaseError::DatabaseAlreadyOpen)
                            if std::time::Instant::now() < deadline =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(20));
                        }
                        Err(redb::DatabaseError::DatabaseAlreadyOpen) => {
                            return Err(SyncError::Storage {
                                context: format!("open redb index at {path_label}"),
                                detail: "index database is currently open by another process"
                                    .to_string(),
                            });
                        }
                        Err(redb::DatabaseError::Storage(redb::StorageError::Io(io_err))) => {
                            return Err(SyncError::from_io(Some(path_label.clone()), io_err));
                        }
                        Err(err) => return Err(damaged_err(&path_label, err)),
                    }
                }
            } else {
                Database::builder()
                    .set_cache_size(DEFAULT_REDB_CACHE_BYTES)
                    .create(path)
                    .map_err(|err| SyncError::Storage {
                        context: format!("create redb index at {path_label}"),
                        detail: err.to_string(),
                    })?
            };

            if existed {
                let intact = db
                    .check_integrity()
                    .map_err(|err| damaged_err(&path_label, err))?;
                if !intact {
                    return Err(damaged_err(
                        &path_label,
                        "redb integrity check returned false",
                    ));
                }

                // Read-only verification BEFORE opening any write transaction so a newer schema or
                // corrupt payload never mutates the file.
                let rtxn = db
                    .begin_read()
                    .map_err(|err| damaged_err(&path_label, err))?;
                if let Ok(meta_table) = rtxn.open_table(META_TABLE) {
                    if let Some(g) = meta_table
                        .get(META_KEY_SCHEMA)
                        .map_err(|err| damaged_err(&path_label, err))?
                    {
                        let found = decode_u32_le("schema_version", g.value())
                            .map_err(|err| damaged_err(&path_label, err))?;
                        if found > INDEX_SCHEMA_VERSION {
                            return Err(SyncError::SchemaTooNew {
                                found,
                                supported: INDEX_SCHEMA_VERSION,
                            });
                        }
                    }
                    if let Some(g) = meta_table
                        .get(META_KEY_FOLDER)
                        .map_err(|err| damaged_err(&path_label, err))?
                    {
                        let _ = decode_json::<FolderMeta>("folder_meta", g.value())
                            .map_err(|err| damaged_err(&path_label, err))?;
                    }
                }
                if let Ok(records_table) = rtxn.open_table(RECORDS_TABLE) {
                    let iter = records_table
                        .iter()
                        .map_err(|err| damaged_err(&path_label, err))?;
                    for item in iter {
                        let (_, v) = item.map_err(|err| damaged_err(&path_label, err))?;
                        let _ = decode_record(v.value())
                            .map_err(|err| damaged_err(&path_label, err))?;
                    }
                }
                if let Ok(cursors_table) = rtxn.open_table(PEER_CURSORS_TABLE) {
                    let iter = cursors_table
                        .iter()
                        .map_err(|err| damaged_err(&path_label, err))?;
                    for item in iter {
                        let (_, v) = item.map_err(|err| damaged_err(&path_label, err))?;
                        let _ = decode_json::<PeerCursor>("peer_cursor", v.value())
                            .map_err(|err| damaged_err(&path_label, err))?;
                    }
                }
                drop(rtxn);
            }

            let txn = db.begin_write().map_err(|err| {
                if existed {
                    damaged_err(&path_label, err)
                } else {
                    SyncError::Storage {
                        context: "initialize redb tables".to_string(),
                        detail: err.to_string(),
                    }
                }
            })?;
            let schema_version = {
                let _ = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
                let _ = txn.open_table(BY_SEQ_TABLE).map_err(table_err)?;
                let _ = txn.open_table(TOMBSTONES_TABLE).map_err(table_err)?;
                let _ = txn.open_table(CONFLICTS_TABLE).map_err(table_err)?;
                let _ = txn.open_table(FOLDED_LIVE_TABLE).map_err(table_err)?;
                let _ = txn.open_table(PEER_CURSORS_TABLE).map_err(table_err)?;
                let _ = txn.open_table(BIN_ENTRIES_TABLE).map_err(table_err)?;
                let _ = txn.open_table(HELD_BACK_TABLE).map_err(table_err)?;
                let _ = txn.open_table(DELETED_HISTORY_TABLE).map_err(table_err)?;
                let _ = txn.open_table(STAGED_TMPS_TABLE).map_err(table_err)?;
                let _ = txn.open_table(SEQ_FINGERPRINTS_TABLE).map_err(table_err)?;
                let _ = txn.open_table(PROBLEMS_TABLE).map_err(table_err)?;
                let _ = txn.open_table(DEFERRALS_TABLE).map_err(table_err)?;
                let mut meta_table = txn.open_table(META_TABLE).map_err(table_err)?;

                let existing = meta_table
                    .get(META_KEY_SCHEMA)
                    .map_err(|e| storage_err("read schema_version", e))?
                    .map(|g| decode_u32_le("schema_version", g.value()))
                    .transpose()?;
                match existing {
                    Some(found) if found > INDEX_SCHEMA_VERSION => {
                        return Err(SyncError::SchemaTooNew {
                            found,
                            supported: INDEX_SCHEMA_VERSION,
                        });
                    }
                    Some(found) => found,
                    None => {
                        meta_table
                            .insert(
                                META_KEY_SCHEMA,
                                INDEX_SCHEMA_VERSION.to_le_bytes().as_slice(),
                            )
                            .map_err(|e| storage_err("write schema_version", e))?;
                        INDEX_SCHEMA_VERSION
                    }
                }
            };
            txn.commit().map_err(|err| {
                if existed {
                    damaged_err(&path_label, err)
                } else {
                    SyncError::Storage {
                        context: "commit initial redb tables".to_string(),
                        detail: err.to_string(),
                    }
                }
            })?;

            Ok(Self {
                db: Arc::new(db),
                schema_version,
                path_label: path_label.clone(),
            })
        })
    }

    /// Stamps `schema_version` directly into `META_TABLE` (intended for tests verifying
    /// forward-incompatible schema refusal).
    pub fn write_schema_version_for_test(&mut self, version: u32) -> Result<()> {
        let label = self.path_label.clone();
        catch_redb(&label, || {
            let txn = self
                .db
                .begin_write()
                .map_err(|e| storage_err("begin write", e))?;
            {
                let mut meta_table = txn.open_table(META_TABLE).map_err(table_err)?;
                meta_table
                    .insert(META_KEY_SCHEMA, version.to_le_bytes().as_slice())
                    .map_err(|e| storage_err("write schema_version", e))?;
            }
            txn.commit()
                .map_err(|e| storage_err("commit schema_version", e))?;
            self.schema_version = version;
            Ok(())
        })
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

fn decode_u32_le(context: &str, bytes: &[u8]) -> Result<u32> {
    let arr: [u8; 4] = bytes.try_into().map_err(|_| SyncError::CorruptData {
        context: context.to_string(),
        detail: format!("expected 4 bytes, got {}", bytes.len()),
    })?;
    Ok(u32::from_le_bytes(arr))
}

fn decode_u64_le(context: &str, bytes: &[u8]) -> Result<u64> {
    let arr: [u8; 8] = bytes.try_into().map_err(|_| SyncError::CorruptData {
        context: context.to_string(),
        detail: format!("expected 8 bytes, got {}", bytes.len()),
    })?;
    Ok(u64::from_le_bytes(arr))
}

fn decode_hash(context: &str, bytes: &[u8]) -> Result<Hash> {
    let arr: [u8; 32] = bytes.try_into().map_err(|_| SyncError::CorruptData {
        context: context.to_string(),
        detail: format!("expected 32 bytes for hash, got {}", bytes.len()),
    })?;
    Ok(Hash::from_bytes(arr))
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
    live_files: &mut u64,
) -> Result<()> {
    let owns_seq = by_seq
        .get(old.seq)
        .map_err(|e| storage_err("check by_seq", e))?
        .is_some_and(|g| g.value() == old.path.as_str());
    if owns_seq {
        by_seq
            .remove(old.seq)
            .map_err(|e| storage_err("remove by_seq", e))?;
    }
    if old.deleted {
        tombstones
            .remove(old.path.as_str())
            .map_err(|e| storage_err("remove tombstone index", e))?;
    } else {
        if old.kind == EntryKind::File {
            *live_files = live_files.saturating_sub(1);
        }
        if old.conflict_of.is_some() && !old.conflict_dismissed {
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
    live_files: &mut u64,
) -> Result<()> {
    by_seq
        .insert(rec.seq, rec.path.as_str())
        .map_err(|e| storage_err("insert by_seq", e))?;
    if rec.deleted {
        tombstones
            .insert(rec.path.as_str(), ())
            .map_err(|e| storage_err("insert tombstone index", e))?;
    } else {
        if rec.kind == EntryKind::File {
            *live_files = live_files.saturating_add(1);
        }
        if rec.conflict_of.is_some() && !rec.conflict_dismissed {
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
    fn schema_version(&self) -> Result<u32> {
        Ok(self.schema_version)
    }

    fn load_meta(&self) -> Result<Option<FolderMeta>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn get_record(&self, path: &str) -> Result<Option<IndexRecord>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
            let Some(guard) = table.get(path).map_err(|e| storage_err("get record", e))? else {
                return Ok(None);
            };
            Ok(Some(decode_record(guard.value())?))
        })
    }

    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn scan_subtree_in_order(
        &self,
        subtree_root: &str,
        visitor: &mut dyn FnMut(IndexRecord) -> Result<bool>,
    ) -> Result<()> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
            if let Some(exact) = table
                .get(subtree_root)
                .map_err(|e| storage_err("get subtree root", e))?
                && !visitor(decode_record(exact.value())?)?
            {
                return Ok(());
            }
            let prefix = format!("{subtree_root}/");
            let range = table
                .range((Bound::Included(prefix.as_str()), Bound::Unbounded))
                .map_err(|e| storage_err("range subtree", e))?;
            for item in range {
                let (k, v) = item.map_err(|e| storage_err("step subtree range", e))?;
                if !is_descendant_of(k.value(), subtree_root) {
                    break;
                }
                if !visitor(decode_record(v.value())?)? {
                    break;
                }
            }
            Ok(())
        })
    }

    fn has_live_descendant(&self, dir_path: &str) -> Result<bool> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn live_paths_for_folded(&self, folded_path: &str) -> Result<Vec<String>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn records_since(
        &self,
        caller_member: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage> {
        catch_redb(&self.path_label, || {
            self.records_since_inner(caller_member, since_incarnation, since_seq, limit)
        })
    }

    fn list_tombstones(&self) -> Result<Vec<IndexRecord>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn list_conflicts(&self) -> Result<Vec<IndexRecord>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn load_peer_cursors(&self) -> Result<BTreeMap<MemberId, PeerCursor>> {
        catch_redb(&self.path_label, || {
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
                out.insert(
                    k.value().to_string(),
                    decode_json("peer_cursor", v.value())?,
                );
            }
            Ok(out)
        })
    }

    fn list_bin_entries(&self) -> Result<Vec<BinEntry>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn get_bin_entry(&self, id: &str) -> Result<Option<BinEntry>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(BIN_ENTRIES_TABLE).map_err(table_err)?;
            let Some(guard) = table.get(id).map_err(|e| storage_err("get bin_entry", e))? else {
                return Ok(None);
            };
            Ok(Some(decode_json("bin_entry", guard.value())?))
        })
    }

    fn list_held_back(&self) -> Result<Vec<HeldBackRecord>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn held_back_count(&self) -> Result<usize> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(HELD_BACK_TABLE).map_err(table_err)?;
            let len = table.len().map_err(|e| storage_err("count held_back", e))?;
            Ok(len as usize)
        })
    }

    fn get_deleted_history(&self, path: &str) -> Result<Vec<Hash>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(DELETED_HISTORY_TABLE).map_err(table_err)?;
            let Some(guard) = table
                .get(path)
                .map_err(|e| storage_err("get deleted_history", e))?
            else {
                return Ok(Vec::new());
            };
            decode_hash_list(guard.value())
        })
    }

    fn load_pending_op(&self) -> Result<Option<PendingOp>> {
        catch_redb(&self.path_label, || {
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
        })
    }

    fn list_staged_tmps(&self) -> Result<BTreeSet<String>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(STAGED_TMPS_TABLE).map_err(table_err)?;
            let iter = table
                .iter()
                .map_err(|e| storage_err("iterate staged_tmps", e))?;
            let mut out = BTreeSet::new();
            for item in iter {
                let (k, _) = item.map_err(|e| storage_err("step staged_tmps", e))?;
                out.insert(k.value().to_string());
            }
            Ok(out)
        })
    }

    fn get_seq_fingerprint(&self, seq: u64) -> Result<Option<Hash>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(SEQ_FINGERPRINTS_TABLE).map_err(table_err)?;
            let Some(guard) = table
                .get(seq)
                .map_err(|e| storage_err("get seq_fingerprint", e))?
            else {
                return Ok(None);
            };
            Ok(Some(decode_hash("seq_fingerprint", guard.value())?))
        })
    }

    fn list_problems(&self) -> Result<Vec<(String, SyncProblem)>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(PROBLEMS_TABLE).map_err(table_err)?;
            let iter = table
                .iter()
                .map_err(|e| storage_err("iterate problems", e))?;
            let mut out = Vec::new();
            for item in iter {
                let (k, v) = item.map_err(|e| storage_err("step problems", e))?;
                out.push((
                    k.value().to_string(),
                    decode_json("sync_problem", v.value())?,
                ));
            }
            Ok(out)
        })
    }

    fn list_deferrals(&self) -> Result<Vec<DeferredItem>> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(DEFERRALS_TABLE).map_err(table_err)?;
            let iter = table
                .iter()
                .map_err(|e| storage_err("iterate deferrals", e))?;
            let mut out = Vec::new();
            for item in iter {
                let (_, v) = item.map_err(|e| storage_err("step deferrals", e))?;
                out.push(decode_json("deferred_item", v.value())?);
            }
            Ok(out)
        })
    }

    fn record_count(&self) -> Result<u64> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
            table.len().map_err(|e| storage_err("count records", e))
        })
    }

    fn live_file_count(&self) -> Result<u64> {
        catch_redb(&self.path_label, || {
            let txn = self
                .db
                .begin_read()
                .map_err(|e| storage_err("begin read", e))?;
            let table = txn.open_table(META_TABLE).map_err(table_err)?;
            let Some(guard) = table
                .get(META_KEY_LIVE_FILES)
                .map_err(|e| storage_err("read live_file_count", e))?
            else {
                return Ok(0);
            };
            decode_u64_le("live_file_count", guard.value())
        })
    }

    fn apply_batch(&mut self, batch: StoreBatch) -> Result<()> {
        let label = self.path_label.clone();
        catch_redb(&label, || self.apply_batch_inner(batch))
    }
}

impl RedbIndexStore {
    fn records_since_inner(
        &self,
        caller_member: &str,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
    ) -> Result<RecordsPage> {
        let txn = self
            .db
            .begin_read()
            .map_err(|e| storage_err("begin read", e))?;
        let meta_table = txn.open_table(META_TABLE).map_err(table_err)?;
        let (incarnation, snapshot_seq, head_fingerprint) = match meta_table
            .get(META_KEY_FOLDER)
            .map_err(|e| storage_err("read folder_meta", e))?
        {
            Some(g) => {
                let m = decode_json::<FolderMeta>("folder_meta", g.value())?;
                (m.incarnation, m.local_seq, m.head_fingerprint)
            }
            None => (0, 0, None),
        };

        let effective_since = if since_incarnation == incarnation {
            since_seq
        } else {
            0
        };

        let cursors_table = txn.open_table(PEER_CURSORS_TABLE).map_err(table_err)?;
        let (caller_acked, caller_acked_fingerprint) = match cursors_table
            .get(caller_member)
            .map_err(|e| storage_err("read caller peer_cursor", e))?
        {
            Some(g) => {
                let c = decode_json::<PeerCursor>("peer_cursor", g.value())?;
                if c.incarnation != 0 {
                    (
                        Some((c.incarnation, c.applied_ack_seq)),
                        c.acked_fingerprint,
                    )
                } else {
                    (None, None)
                }
            }
            None => (None, None),
        };

        let by_seq = txn.open_table(BY_SEQ_TABLE).map_err(table_err)?;
        let records_table = txn.open_table(RECORDS_TABLE).map_err(table_err)?;
        let seq_fps_table = txn.open_table(SEQ_FINGERPRINTS_TABLE).map_err(table_err)?;
        let range = by_seq
            .range((Bound::Excluded(effective_since), Bound::Unbounded))
            .map_err(|e| storage_err("range by_seq", e))?;

        let mut records = Vec::new();
        let mut chain_fingerprints = BTreeMap::new();
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
                let rec = decode_record(rec_guard.value())?.for_wire();
                if let Some(fp_guard) = seq_fps_table
                    .get(rec.seq)
                    .map_err(|e| storage_err("read seq_fingerprint", e))?
                {
                    chain_fingerprints
                        .insert(rec.seq, decode_hash("seq_fingerprint", fp_guard.value())?);
                }
                if rec.seq > 1
                    && let Some(prev_guard) = seq_fps_table
                        .get(rec.seq - 1)
                        .map_err(|e| storage_err("read prev seq_fingerprint", e))?
                {
                    chain_fingerprints.insert(
                        rec.seq - 1,
                        decode_hash("seq_fingerprint", prev_guard.value())?,
                    );
                }
                records.push(rec);
            }
        }
        if snapshot_seq > 0
            && let Some(fp_guard) = seq_fps_table
                .get(snapshot_seq)
                .map_err(|e| storage_err("read snapshot seq_fingerprint", e))?
        {
            chain_fingerprints.insert(
                snapshot_seq,
                decode_hash("seq_fingerprint", fp_guard.value())?,
            );
        }

        Ok(RecordsPage {
            incarnation,
            records,
            snapshot_seq,
            has_more,
            caller_acked,
            caller_acked_fingerprint,
            head_fingerprint,
            chain_fingerprints,
        })
    }

    fn apply_batch_inner(&mut self, batch: StoreBatch) -> Result<()> {
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
            let mut deleted_history = txn.open_table(DELETED_HISTORY_TABLE).map_err(table_err)?;
            let mut staged_tmps = txn.open_table(STAGED_TMPS_TABLE).map_err(table_err)?;
            let mut seq_fps = txn.open_table(SEQ_FINGERPRINTS_TABLE).map_err(table_err)?;
            let mut problems = txn.open_table(PROBLEMS_TABLE).map_err(table_err)?;
            let mut deferrals = txn.open_table(DEFERRALS_TABLE).map_err(table_err)?;

            let mut live_files = match meta_table
                .get(META_KEY_LIVE_FILES)
                .map_err(|e| storage_err("read live_file_count", e))?
            {
                Some(g) => decode_u64_le("live_file_count", g.value())?,
                None => 0,
            };
            let initial_live_files = live_files;

            if let Some(ref meta) = batch.folder_meta {
                let encoded = encode_json("folder_meta", meta)?;
                meta_table
                    .insert(META_KEY_FOLDER, encoded.as_slice())
                    .map_err(|e| storage_err("write folder_meta", e))?;
            }

            for path in &batch.remove_records {
                let old = records
                    .remove(path.as_str())
                    .map_err(|e| storage_err("remove record", e))?
                    .map(|g| decode_record(g.value()))
                    .transpose()?;
                if let Some(old_rec) = old {
                    unindex_record_in_txn(
                        &old_rec,
                        &mut by_seq,
                        &mut tombstones,
                        &mut conflicts,
                        &mut folded_live,
                        &mut live_files,
                    )?;
                }
            }

            for rec in &batch.upsert_records {
                let old = records
                    .get(rec.path.as_str())
                    .map_err(|e| storage_err("read existing record", e))?
                    .map(|g| decode_record(g.value()))
                    .transpose()?;
                if let Some(old_rec) = old {
                    unindex_record_in_txn(
                        &old_rec,
                        &mut by_seq,
                        &mut tombstones,
                        &mut conflicts,
                        &mut folded_live,
                        &mut live_files,
                    )?;
                }
                let encoded = encode_record(rec);
                records
                    .insert(rec.path.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert record", e))?;
                index_record_in_txn(
                    rec,
                    &mut by_seq,
                    &mut tombstones,
                    &mut conflicts,
                    &mut folded_live,
                    &mut live_files,
                )?;
            }

            if live_files != initial_live_files {
                meta_table
                    .insert(META_KEY_LIVE_FILES, live_files.to_le_bytes().as_slice())
                    .map_err(|e| storage_err("write live_file_count", e))?;
            }

            for peer in &batch.remove_peer_cursors {
                peer_cursors
                    .remove(peer.as_str())
                    .map_err(|e| storage_err("remove peer_cursor", e))?;
            }
            for (peer, cursor) in &batch.set_peer_cursors {
                let encoded = encode_json("peer_cursor", cursor)?;
                peer_cursors
                    .insert(peer.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("set peer_cursor", e))?;
            }

            for id in &batch.remove_bin {
                bin_entries
                    .remove(id.as_str())
                    .map_err(|e| storage_err("remove bin_entry", e))?;
            }
            for entry in &batch.upsert_bin {
                let encoded = encode_json("bin_entry", entry)?;
                bin_entries
                    .insert(entry.id.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert bin_entry", e))?;
            }

            if batch.clear_held_back {
                let keys: Vec<String> = held_back
                    .iter()
                    .map_err(|e| storage_err("iter held_back", e))?
                    .map(|res| {
                        res.map(|(k, _)| k.value().to_string())
                            .map_err(|e| storage_err("step held_back", e))
                    })
                    .collect::<Result<_>>()?;
                for k in keys {
                    held_back
                        .remove(k.as_str())
                        .map_err(|e| storage_err("clear held_back", e))?;
                }
            }
            for (peer, path) in &batch.remove_held_back {
                let key = peer_path_key(peer, path);
                held_back
                    .remove(key.as_str())
                    .map_err(|e| storage_err("remove held_back", e))?;
            }
            for item in &batch.upsert_held_back {
                let key = peer_path_key(&item.from_peer, &item.record.path);
                let encoded = encode_json("held_back", item)?;
                held_back
                    .insert(key.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert held_back", e))?;
            }

            for path in &batch.remove_deleted_history {
                deleted_history
                    .remove(path.as_str())
                    .map_err(|e| storage_err("remove deleted_history", e))?;
            }
            for (path, hashes) in &batch.upsert_deleted_history {
                if hashes.is_empty() {
                    deleted_history
                        .remove(path.as_str())
                        .map_err(|e| storage_err("remove empty deleted_history", e))?;
                } else {
                    let existing = deleted_history
                        .get(path.as_str())
                        .map_err(|e| storage_err("check deleted_history", e))?
                        .is_some();
                    if !existing {
                        let count = deleted_history
                            .len()
                            .map_err(|e| storage_err("count deleted_history", e))?;
                        if count as usize >= MAX_DELETED_HISTORY_PATHS {
                            let first_key = deleted_history
                                .iter()
                                .map_err(|e| storage_err("iter deleted_history", e))?
                                .next()
                                .transpose()
                                .map_err(|e| storage_err("step deleted_history", e))?
                                .map(|(k, _)| k.value().to_string());
                            if let Some(evict) = first_key {
                                deleted_history
                                    .remove(evict.as_str())
                                    .map_err(|e| storage_err("evict deleted_history", e))?;
                            }
                        }
                    }
                    let encoded = encode_hash_list(hashes);
                    deleted_history
                        .insert(path.as_str(), encoded.as_slice())
                        .map_err(|e| storage_err("upsert deleted_history", e))?;
                }
            }

            if let Some(ref op_update) = batch.set_pending_op {
                match op_update {
                    Some(op) => {
                        let encoded = encode_json("pending_op", op)?;
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

            for tmp in &batch.remove_staged_tmps {
                staged_tmps
                    .remove(tmp.as_str())
                    .map_err(|e| storage_err("remove staged_tmp", e))?;
            }
            for tmp in &batch.add_staged_tmps {
                staged_tmps
                    .insert(tmp.as_str(), ())
                    .map_err(|e| storage_err("add staged_tmp", e))?;
            }

            if batch.clear_seq_fingerprints {
                let keys: Vec<u64> = seq_fps
                    .iter()
                    .map_err(|e| storage_err("iter seq_fingerprints", e))?
                    .map(|res| {
                        res.map(|(k, _)| k.value())
                            .map_err(|e| storage_err("step seq_fingerprints", e))
                    })
                    .collect::<Result<_>>()?;
                for k in keys {
                    seq_fps
                        .remove(k)
                        .map_err(|e| storage_err("clear seq_fingerprint", e))?;
                }
            }
            if let Some(min_keep) = batch.prune_seq_fingerprints_below {
                // Retain every fingerprint at or above `min_keep`, PLUS `rec.seq` and `rec.seq - 1`
                // for every record still stored in `by_seq` so newly joining or rejoining members
                // stopping at any page or deferral boundary always receive and validate valid
                // chained fingerprints.
                let mut to_prune = Vec::new();
                for res in seq_fps
                    .range(..min_keep)
                    .map_err(|e| storage_err("range prune seq_fingerprints", e))?
                {
                    let (k, _) = res.map_err(|e| storage_err("step prune seq_fingerprints", e))?;
                    let seq = k.value();
                    let needed_for_record = by_seq
                        .get(seq)
                        .map_err(|e| storage_err("check by_seq for prune", e))?
                        .is_some()
                        || by_seq
                            .get(seq.saturating_add(1))
                            .map_err(|e| storage_err("check next by_seq for prune", e))?
                            .is_some();
                    if !needed_for_record {
                        to_prune.push(seq);
                    }
                }
                for k in to_prune {
                    seq_fps
                        .remove(k)
                        .map_err(|e| storage_err("prune seq_fingerprint", e))?;
                }
            }
            for &(seq, fp) in &batch.upsert_seq_fingerprints {
                seq_fps
                    .insert(seq, fp.as_bytes().as_slice())
                    .map_err(|e| storage_err("upsert seq_fingerprint", e))?;
            }

            for key in &batch.remove_problems {
                problems
                    .remove(key.as_str())
                    .map_err(|e| storage_err("remove problem", e))?;
            }
            for (key, prob) in &batch.upsert_problems {
                let encoded = encode_json("sync_problem", prob)?;
                problems
                    .insert(key.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert problem", e))?;
            }

            if batch.clear_deferrals {
                let keys: Vec<String> = deferrals
                    .iter()
                    .map_err(|e| storage_err("iter deferrals", e))?
                    .map(|res| {
                        res.map(|(k, _)| k.value().to_string())
                            .map_err(|e| storage_err("step deferrals", e))
                    })
                    .collect::<Result<_>>()?;
                for k in keys {
                    deferrals
                        .remove(k.as_str())
                        .map_err(|e| storage_err("clear deferral", e))?;
                }
            }
            for (peer, path) in &batch.remove_deferrals {
                let key = peer_path_key(peer, path);
                deferrals
                    .remove(key.as_str())
                    .map_err(|e| storage_err("remove deferral", e))?;
            }
            for item in &batch.upsert_deferrals {
                let key = peer_path_key(&item.peer, &item.path);
                let encoded = encode_json("deferred_item", item)?;
                deferrals
                    .insert(key.as_str(), encoded.as_slice())
                    .map_err(|e| storage_err("upsert deferral", e))?;
            }
        }
        txn.commit()
            .map_err(|e| storage_err("commit redb batch", e))?;
        Ok(())
    }
}
