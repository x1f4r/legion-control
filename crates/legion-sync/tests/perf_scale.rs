//! Scale and performance tests for 100,000 and 500,000 files on both `MemoryIndexStore` and
//! `RedbIndexStore`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::time::Instant;

use legion_sync::reconcile::LocalIndexView;
use legion_sync::{
    FolderConfig, FsCapabilities, FsMeta, FsWalkEntry, FsWalkResult, IgnoreMatcher, IndexRecord,
    IndexStore, MemoryIndexStore, RedbIndexStore, SyncEngine, SyncFs, SyncWrite, reconcile_record,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};

/// Synthetic read-only filesystem yielding `n` pre-built virtual files without touching disk,
/// so we measure pure engine + index store scan and reconcile overhead at 100k and 500k files.
struct SyntheticFs {
    entries: Vec<FsWalkEntry>,
    sample_hash: Hash,
}

impl SyntheticFs {
    fn new(count: usize, mtime: Timestamp) -> Self {
        let mut entries = Vec::with_capacity(count);
        for i in 0..count {
            let dir = i / 1000;
            entries.push(FsWalkEntry {
                path: format!("d{dir:04}/file_{i:06}.dat"),
                meta: FsMeta {
                    kind: EntryKind::File,
                    size: 128,
                    mtime,
                    mode: Some(0o644),
                    link_target: None,
                    change_id: Some(i as u64 + 1),
                },
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            entries,
            sample_hash: hash_bytes(b"synthetic-payload"),
        }
    }
}

impl SyncFs for SyntheticFs {
    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities::unix()
    }

    fn walk(&self, _ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        Ok(FsWalkResult {
            entries: self.entries.clone(),
            skipped: Vec::new(),
        })
    }

    fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
        match self
            .entries
            .binary_search_by(|e| e.path.as_str().cmp(rel_path))
        {
            Ok(idx) => Ok(Some(self.entries[idx].meta.clone())),
            Err(_) => Ok(None),
        }
    }

    fn hash_live(&self, _rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
        Ok((self.sample_hash, 128))
    }

    fn open_live_read(&self, _rel_path: &str) -> legion_sync::Result<Box<dyn Read + Send>> {
        Ok(Box::new(Cursor::new(vec![0u8; 128])))
    }

    fn create_dir_all_live(&self, _rel_path: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn list_dir_live(&self, _rel_path: &str) -> legion_sync::Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn remove_empty_dir_live(&self, _rel_path: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn create_symlink_live(&self, _rel_path: &str, _target: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn remove_symlink_live(&self, _rel_path: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn set_live_metadata(
        &self,
        _rel_path: &str,
        _mtime: Timestamp,
        _mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        Ok(())
    }

    fn rename_live_to_live(&self, _from_rel: &str, _to_rel: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn create_tmp(&self, _tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        unreachable!("not used in scan/reconcile scale benchmark")
    }

    fn stat_tmp(&self, _tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        Ok(None)
    }

    fn hash_tmp(&self, _tmp_name: &str) -> legion_sync::Result<(Hash, u64)> {
        Ok((self.sample_hash, 128))
    }

    fn remove_tmp(&self, _tmp_name: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn commit_tmp_to_live(&self, _tmp_name: &str, _live_rel: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn move_live_to_bin(&self, _live_rel: &str, _bin_data_name: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn write_bin_meta(&self, _meta_name: &str, _bytes: &[u8]) -> legion_sync::Result<()> {
        Ok(())
    }

    fn read_bin_meta(&self, _meta_name: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn stat_bin(&self, _bin_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        Ok(None)
    }

    fn hash_bin(&self, _bin_name: &str) -> legion_sync::Result<(Hash, u64)> {
        Ok((self.sample_hash, 128))
    }

    fn copy_bin_to_tmp(&self, _bin_data_name: &str, _tmp_name: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn remove_bin_file(&self, _bin_name: &str) -> legion_sync::Result<()> {
        Ok(())
    }

    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct MapView<'a>(&'a BTreeMap<String, IndexRecord>);

impl LocalIndexView for MapView<'_> {
    fn get_record(&self, path: &str) -> Option<IndexRecord> {
        self.0.get(path).cloned()
    }

    fn has_live_descendant(&self, _dir_path: &str) -> bool {
        false
    }

    fn live_paths_for_folded(&self, _folded_path: &str) -> Vec<String> {
        Vec::new()
    }
}

fn measure_scale<S: IndexStore>(label: &str, count: usize, store: S) {
    let now = Timestamp::new(1_795_608_000, 0);
    let fs = SyntheticFs::new(count, now);
    let cfg = FolderConfig::new("scale", "tower", ["tower".into(), "pi".into()]);
    let mut engine = SyncEngine::open(cfg, fs, store).unwrap();

    // 1. Initial scan (populates `count` entries in one atomic batch).
    let t_init = Instant::now();
    let r1 = engine.scan(now).unwrap();
    let init_elapsed = t_init.elapsed();
    assert_eq!(r1.created, count);
    assert_eq!(engine.store().record_count().unwrap(), count as u64);

    // 2. Steady-state unchanged scan (must hash 0 files).
    let t_unchanged = Instant::now();
    let r2 = engine.scan(now).unwrap();
    let unchanged_elapsed = t_unchanged.elapsed();
    assert_eq!(r2.unchanged, count);
    assert_eq!(r2.hashed, 0);

    // 3. Reconcile `count` incoming remote records against the `count`-entry index (mix of equal,
    // newer, and concurrent records).
    let mut local_map = BTreeMap::new();
    engine
        .store()
        .scan_records_in_order(&mut |rec| {
            local_map.insert(rec.path.clone(), rec);
            Ok(true)
        })
        .unwrap();

    let alt_hash = hash_bytes(b"remote-updated");
    let mut remote_records = Vec::with_capacity(count);
    for (idx, rec) in local_map.values().enumerate() {
        let mut r = rec.for_wire();
        if idx % 10 == 0 {
            r.version.increment("pi");
            r.hash = Some(alt_hash);
            r.modified_by = "pi".to_string();
            r.mtime = Timestamp::new(now.secs + 60, 0);
        }
        remote_records.push(r);
    }

    let view = MapView(&local_map);
    let caps = FsCapabilities::unix();
    let ignore = IgnoreMatcher::default();
    let t_reconcile = Instant::now();
    let mut fetch_count = 0usize;
    for remote in &remote_records {
        let d = reconcile_record(&view, remote, "pi", &caps, &ignore);
        if matches!(d, legion_sync::ReconcileDecision::Fetch { .. }) {
            fetch_count += 1;
        }
    }
    let reconcile_elapsed = t_reconcile.elapsed();
    assert_eq!(fetch_count, count.div_ceil(10));

    // Also measure reconciling a 1,000-record incoming delta directly against the IndexStore
    // (the steady-state incremental pull path).
    let store_view = legion_sync::apply::StoreIndexView::new(engine.store());
    let delta_slice = &remote_records[..1_000.min(remote_records.len())];
    let t_delta = Instant::now();
    for remote in delta_slice {
        let _ = reconcile_record(&store_view, remote, "pi", &caps, &ignore);
    }
    let delta_elapsed = t_delta.elapsed();

    // 4. Incremental `records_since` page read at tail of `count`-entry index.
    let t_page = Instant::now();
    let page = engine
        .serve_records_since("pi", (count as u64).saturating_sub(100), 100)
        .unwrap();
    let page_elapsed = t_page.elapsed();
    assert_eq!(page.records.len(), 100);

    println!(
        "[{label} n={count}] initial_scan={:.3}s unchanged_scan={:.3}s reconcile_all={:.3}s reconcile_1k_store={:.3}ms tail_page_100={:.3}ms",
        init_elapsed.as_secs_f64(),
        unchanged_elapsed.as_secs_f64(),
        reconcile_elapsed.as_secs_f64(),
        delta_elapsed.as_secs_f64() * 1000.0,
        page_elapsed.as_secs_f64() * 1000.0,
    );
}

#[test]
fn scale_sanity_5k_redb() {
    let dir = tempfile::tempdir().unwrap();
    let store = RedbIndexStore::open(dir.path().join("scale5k.redb")).unwrap();
    measure_scale("redb", 5_000, store);
}

#[test]
#[ignore = "scale benchmark; run in --release with --ignored --nocapture"]
fn scale_100k_and_500k_benchmark() {
    for &n in &[100_000usize, 500_000usize] {
        measure_scale("memory", n, MemoryIndexStore::new());
        let dir = tempfile::tempdir().unwrap();
        let store = RedbIndexStore::open(dir.path().join(format!("scale_{n}.redb"))).unwrap();
        measure_scale("redb", n, store);
    }
}
