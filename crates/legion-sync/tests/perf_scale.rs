//! Scale and throughput tests:
//! - Synthetic 5k / 100k / 500k scan & reconcile benchmarks on `MemoryIndexStore` and `RedbIndexStore`.
//! - Real-disk apply throughput benchmark (10,000 small files in files/sec and large files in MB/s)
//!   with group-committed pages on `RealFs` + `RedbIndexStore`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use legion_sync::reconcile::LocalIndexView;
use legion_sync::{
    FetchOptions, FileInstanceTokenStore, FolderConfig, FolderMarker, FsCapabilities, FsMeta,
    FsWalkEntry, FsWalkResult, IgnoreMatcher, IndexRecord, IndexStore, MemoryIndexStore,
    MemoryInstanceTokenStore, Peer, PeerHandshake, RealFs, RecordProvenance, RecordsPage,
    RedbIndexStore, SYNC_PROTOCOL_VERSION, SyncEngine, SyncFs, SyncWrite, VersionKey,
    VersionVector, reconcile_record,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};

struct SyntheticFs {
    marker: Mutex<Option<FolderMarker>>,
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
                    file_identity: Some((1, i as u64 + 1)),
                },
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Self {
            marker: Mutex::new(None),
            entries,
            sample_hash: hash_bytes(b"synthetic-payload"),
        }
    }
}

impl SyncFs for SyntheticFs {
    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities::unix()
    }

    fn read_marker(&self) -> legion_sync::Result<Option<FolderMarker>> {
        Ok(self.marker.lock().unwrap().clone())
    }

    fn write_marker(&self, marker: &FolderMarker) -> legion_sync::Result<()> {
        *self.marker.lock().unwrap() = Some(marker.clone());
        Ok(())
    }

    fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
        Ok(())
    }

    fn walk(&self, _ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        Ok(FsWalkResult {
            entries: self.entries.clone(),
            skipped: Vec::new(),
        })
    }

    fn walk_subtree(
        &self,
        _rel_root: &str,
        _ignore: &IgnoreMatcher,
    ) -> legion_sync::Result<FsWalkResult> {
        Ok(FsWalkResult::default())
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

    fn open_live_read_at(
        &self,
        _rel_path: &str,
        _offset: u64,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
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

    fn rename_live_to_live_no_replace(
        &self,
        _from_rel: &str,
        _to_rel: &str,
    ) -> legion_sync::Result<()> {
        Ok(())
    }

    fn create_tmp(&self, _tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        unreachable!("not used in scan/reconcile scale benchmark")
    }

    fn open_tmp_append(&self, _tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        unreachable!("not used in scan/reconcile scale benchmark")
    }

    fn stat_tmp(&self, _tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        Ok(None)
    }

    fn set_tmp_metadata(
        &self,
        _tmp_name: &str,
        _mtime: Timestamp,
        _mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        Ok(())
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

    fn commit_tmp_to_live_no_replace(
        &self,
        _tmp_name: &str,
        _live_rel: &str,
    ) -> legion_sync::Result<()> {
        Ok(())
    }

    fn exchange_tmp_with_live(
        &self,
        _tmp_name: &str,
        _live_rel: &str,
    ) -> legion_sync::Result<bool> {
        Ok(false)
    }

    fn move_live_to_tmp_no_replace(
        &self,
        _live_rel: &str,
        _tmp_name: &str,
    ) -> legion_sync::Result<()> {
        Ok(())
    }

    fn move_tmp_to_bin_no_replace(
        &self,
        _tmp_name: &str,
        _bin_data_name: &str,
    ) -> legion_sync::Result<()> {
        Ok(())
    }

    fn move_live_to_bin_no_replace(
        &self,
        _live_rel: &str,
        _bin_data_name: &str,
    ) -> legion_sync::Result<()> {
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
    let mut engine = SyncEngine::open(cfg, fs, store, MemoryInstanceTokenStore::new()).unwrap();

    let t_init = Instant::now();
    let r1 = engine.scan(now).unwrap();
    let init_elapsed = t_init.elapsed();
    assert_eq!(r1.created, count);
    assert_eq!(engine.store().record_count().unwrap(), count as u64);

    let t_unchanged = Instant::now();
    let r2 = engine.scan(now).unwrap();
    let unchanged_elapsed = t_unchanged.elapsed();
    assert_eq!(r2.unchanged, count);
    assert_eq!(r2.hashed, 0);

    let mut local_map = BTreeMap::new();
    engine
        .store()
        .scan_records_in_order(&mut |rec| {
            local_map.insert(rec.path.clone(), rec);
            Ok(true)
        })
        .unwrap();

    let meta = engine.meta().unwrap();
    let alt_hash = hash_bytes(b"remote-updated");
    let pi_key = VersionKey::new("pi", 1);
    let mut remote_records = Vec::with_capacity(count);
    for (idx, rec) in local_map.values().enumerate() {
        let mut r = rec.for_wire();
        if idx % 10 == 0 {
            let mut vv = r.provenance.into_version_vector();
            vv.increment(pi_key.clone());
            r.provenance = RecordProvenance::Versioned(vv);
            r.base_hash = rec.hash;
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
        let d = reconcile_record(&view, remote, "pi", &meta, &caps, &ignore);
        if matches!(d, legion_sync::ReconcileDecision::Fetch { .. }) {
            fetch_count += 1;
        }
    }
    let reconcile_elapsed = t_reconcile.elapsed();
    assert_eq!(fetch_count, count.div_ceil(10));

    let store_view = legion_sync::apply::StoreIndexView::new(engine.store());
    let delta_slice = &remote_records[..1_000.min(remote_records.len())];
    let t_delta = Instant::now();
    for remote in delta_slice {
        let _ = reconcile_record(&store_view, remote, "pi", &meta, &caps, &ignore);
    }
    let delta_elapsed = t_delta.elapsed();

    let t_page = Instant::now();
    let page = engine
        .serve_records_since(
            "pi",
            meta.incarnation,
            (count as u64).saturating_sub(100),
            100,
        )
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

/// In-memory peer serving synthetic records + file bytes so real-disk apply benchmarks measure
/// local disk + BLAKE3 + atomic placement + redb group-commit throughput directly.
struct BenchPeer {
    folder_id: String,
    member_id: String,
    incarnation: u64,
    records: Vec<IndexRecord>,
    payload: Vec<u8>,
}

impl Peer for BenchPeer {
    fn member_id(&self) -> &str {
        &self.member_id
    }

    fn handshake(
        &self,
        _caller: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<PeerHandshake> {
        Ok(PeerHandshake {
            folder_id: self.folder_id.clone(),
            member_id: self.member_id.clone(),
            incarnation: self.incarnation,
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
        limit: legion_sync::PageLimit,
        _cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        let start = if since_incarnation == self.incarnation {
            since_seq as usize
        } else {
            0
        };
        let end = (start + limit.max_records).min(self.records.len());
        let records = self.records[start..end].to_vec();
        let has_more = end < self.records.len();
        Ok(RecordsPage {
            incarnation: self.incarnation,
            records,
            snapshot_seq: self.records.len() as u64,
            has_more,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: self.records.last().map(IndexRecord::head_fingerprint),
            chain_fingerprints: std::collections::BTreeMap::new(),
        })
    }

    fn fetch_file(
        &self,
        _path: &str,
        _expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        let start = (options.offset as usize).min(self.payload.len());
        Ok(Box::new(Cursor::new(self.payload[start..].to_vec())))
    }
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

fn bench_real_disk_root() -> tempfile::TempDir {
    // Always benchmark on a real disk volume (never accidental `/tmp` tmpfs).
    if let Ok(dir) = std::env::var("LEGION_SYNC_BENCH_DIR") {
        std::fs::create_dir_all(&dir).unwrap();
        return tempfile::tempdir_in(dir).unwrap();
    }
    let workspace_target = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target")
        .join("bench-tmp");
    std::fs::create_dir_all(&workspace_target).unwrap();
    tempfile::tempdir_in(workspace_target).unwrap()
}

#[test]
fn apply_throughput_1k_small_files_real_disk() {
    let now = Timestamp::new(1_795_608_000, 0);
    let tmp = bench_real_disk_root();
    let payload = vec![b's'; 1024];
    let hash = hash_bytes(&payload);
    let count = 1_000usize;
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let dir = i / 100;
        let seq = (i + 1) as u64;
        records.push(IndexRecord {
            path: format!("b{dir:02}/f{i:04}.txt"),
            kind: EntryKind::File,
            size: payload.len() as u64,
            mtime: now,
            disk_mtime: None,
            disk_change_id: None,
            mode: Some(0o644),
            hash: Some(hash),
            link_target: None,
            deleted: false,
            provenance: RecordProvenance::Versioned(VersionVector::singleton(
                VersionKey::new("src", 1),
                seq,
            )),
            base_hash: None,
            modified_by: "src".into(),
            seq,
            recent_hashes: Vec::new(),
            conflict_of: None,
            conflict_dismissed: false,
        });
    }
    let peer = BenchPeer {
        folder_id: "bench-1k".into(),
        member_id: "src".into(),
        incarnation: 1,
        records,
        payload: payload.clone(),
    };
    let folder = tmp.path().join("folder");
    let state = tmp.path().join("state");
    let mut dst = SyncEngine::open_folder(
        FolderConfig::new("bench-1k", "dst", ["src".into(), "dst".into()]),
        &folder,
        FileInstanceTokenStore::new(&state),
    )
    .unwrap();
    let t0 = Instant::now();
    let rep = dst.pull_from_peer(&peer, now).unwrap();
    let apply_secs = t0.elapsed().as_secs_f64();
    assert_eq!(rep.applied, count);

    // Measure pure placement cost for 1,000 already-synced temp files:
    // 1. `commit_tmp_to_live_no_replace_deferred_dir_sync` (uses MOVEFILE_WRITE_THROUGH on Windows).
    // 2. Plain `std::fs::rename` (no MOVEFILE_WRITE_THROUGH on Windows).
    let wt_root = tmp.path().join("wt-place");
    let wt_fs = RealFs::open(&wt_root).unwrap();
    wt_fs.ensure_internal_dirs().unwrap();
    wt_fs.create_dir_all_live("out").unwrap();
    for i in 0..count {
        let mut w = wt_fs.create_tmp(&format!("wt-{i}.tmp")).unwrap();
        std::io::Write::write_all(&mut w, &payload).unwrap();
        w.finish_sync_with_metadata(now, Some(0o644)).unwrap();
    }
    let t_wt = Instant::now();
    for i in 0..count {
        wt_fs
            .commit_tmp_to_live_no_replace_deferred_dir_sync(
                &format!("wt-{i}.tmp"),
                &format!("out/f{i:04}.txt"),
            )
            .unwrap();
    }
    wt_fs.sync_parent_dir_live("out/x").unwrap();
    let wt_secs = t_wt.elapsed().as_secs_f64();

    let plain_root = tmp.path().join("plain-place");
    let plain_tmp = plain_root.join("tmp");
    let plain_out = plain_root.join("out");
    std::fs::create_dir_all(&plain_tmp).unwrap();
    std::fs::create_dir_all(&plain_out).unwrap();
    for i in 0..count {
        let mut f = std::fs::File::create(plain_tmp.join(format!("p-{i}.tmp"))).unwrap();
        std::io::Write::write_all(&mut f, &payload).unwrap();
        f.sync_all().unwrap();
    }
    let t_plain = Instant::now();
    for i in 0..count {
        std::fs::rename(
            plain_tmp.join(format!("p-{i}.tmp")),
            plain_out.join(format!("f{i:04}.txt")),
        )
        .unwrap();
    }
    let plain_secs = t_plain.elapsed().as_secs_f64();

    println!(
        "[apply 1k small files os={}] apply={:.1} files/sec ({:.3}s) | place_write_through={:.1} files/sec ({:.3}s) | place_plain_rename={:.1} files/sec ({:.3}s)",
        std::env::consts::OS,
        count as f64 / apply_secs,
        apply_secs,
        count as f64 / wt_secs,
        wt_secs,
        count as f64 / plain_secs,
        plain_secs,
    );
}

#[test]
#[ignore = "real-disk apply throughput benchmark; run in --release with --ignored --nocapture"]
fn apply_throughput_10k_small_and_large_files_real_disk() {
    let now = Timestamp::new(1_795_608_000, 0);

    // 1. Apply 10,000 small files (1 KiB each) onto real disk with `RealFs` + `RedbIndexStore`.
    let tmp_small = bench_real_disk_root();
    let small_payload = vec![b's'; 1024];
    let small_hash = hash_bytes(&small_payload);
    let small_count = 10_000usize;
    let mut small_records = Vec::with_capacity(small_count);
    for i in 0..small_count {
        let dir = i / 250;
        let seq = (i + 1) as u64;
        small_records.push(IndexRecord {
            path: format!("b{dir:03}/f{i:05}.txt"),
            kind: EntryKind::File,
            size: small_payload.len() as u64,
            mtime: now,
            disk_mtime: None,
            disk_change_id: None,
            mode: Some(0o644),
            hash: Some(small_hash),
            link_target: None,
            deleted: false,
            provenance: RecordProvenance::Versioned(VersionVector::singleton(
                VersionKey::new("src", 1),
                seq,
            )),
            base_hash: None,
            modified_by: "src".into(),
            seq,
            recent_hashes: Vec::new(),
            conflict_of: None,
            conflict_dismissed: false,
        });
    }
    let small_peer = BenchPeer {
        folder_id: "bench-small".into(),
        member_id: "src".into(),
        incarnation: 1,
        records: small_records,
        payload: small_payload,
    };
    let mut dst_small = SyncEngine::open_folder(
        FolderConfig::new("bench-small", "dst", ["src".into(), "dst".into()]),
        tmp_small.path().join("folder"),
        FileInstanceTokenStore::new(tmp_small.path().join("state")),
    )
    .unwrap();
    let t_small = Instant::now();
    let rep_small = dst_small.pull_from_peer(&small_peer, now).unwrap();
    let small_secs = t_small.elapsed().as_secs_f64();
    assert_eq!(rep_small.applied, small_count);
    println!(
        "[apply real-disk 10k small files (1 KiB each)] applied={} elapsed={:.3}s throughput={:.1} files/sec",
        rep_small.applied,
        small_secs,
        small_count as f64 / small_secs
    );

    // 2. Apply large files (4 x 64 MiB = 256 MiB total) onto real disk with `RealFs` + `RedbIndexStore`.
    let tmp_large = bench_real_disk_root();
    let chunk_size = 64 * 1024 * 1024usize;
    let large_count = 4usize;
    let large_payload = vec![0xA5u8; chunk_size];
    let large_hash = hash_bytes(&large_payload);
    let mut large_records = Vec::with_capacity(large_count);
    for i in 0..large_count {
        let seq = (i + 1) as u64;
        large_records.push(IndexRecord {
            path: format!("big_{i}.bin"),
            kind: EntryKind::File,
            size: chunk_size as u64,
            mtime: now,
            disk_mtime: None,
            disk_change_id: None,
            mode: Some(0o644),
            hash: Some(large_hash),
            link_target: None,
            deleted: false,
            provenance: RecordProvenance::Versioned(VersionVector::singleton(
                VersionKey::new("src", 1),
                seq,
            )),
            base_hash: None,
            modified_by: "src".into(),
            seq,
            recent_hashes: Vec::new(),
            conflict_of: None,
            conflict_dismissed: false,
        });
    }
    let large_peer = BenchPeer {
        folder_id: "bench-large".into(),
        member_id: "src".into(),
        incarnation: 1,
        records: large_records,
        payload: large_payload,
    };
    let mut dst_large = SyncEngine::open_folder(
        FolderConfig::new("bench-large", "dst", ["src".into(), "dst".into()]),
        tmp_large.path().join("folder"),
        FileInstanceTokenStore::new(tmp_large.path().join("state")),
    )
    .unwrap();
    let t_large = Instant::now();
    let rep_large = dst_large.pull_from_peer(&large_peer, now).unwrap();
    let large_secs = t_large.elapsed().as_secs_f64();
    assert_eq!(rep_large.applied, large_count);
    let total_mb = (large_count * chunk_size) as f64 / (1024.0 * 1024.0);
    println!(
        "[apply real-disk large files ({large_count}x64 MiB = {total_mb:.0} MiB)] elapsed={:.3}s throughput={:.1} MB/s",
        large_secs,
        total_mb / large_secs
    );
}
