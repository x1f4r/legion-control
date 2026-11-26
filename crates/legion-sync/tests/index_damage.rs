//! Real-disk safety regression tests: damaged, 0-byte, missing, and panicking index stores.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod safety_common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use legion_sync::{
    BinEntry, DeferredItem, FolderMeta, HeldBackRecord, INDEX_DAMAGED_PROBLEM_SENTENCE,
    IndexRecord, IndexStore, MemberId, MemberState, MemoryIndexStore, MemoryInstanceTokenStore,
    PeerCursor, PendingOp, ProblemKind, RealFs, RecordsPage, RedbIndexStore, SYNC_INDEX_FILE,
    StoreBatch, SyncEngine, SyncError, SyncProblem,
};
use legion_xfer::Hash;
use safety_common::*;

#[test]
fn i1_truncated_or_torn_index() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let tower_root = t.join("tower");
    let pi_root = t.join("pi");
    fs::create_dir_all(&tower_root).unwrap();
    fs::create_dir_all(&pi_root).unwrap();
    let mut pi = open_folder(&pi_root, &token_store_dir(&pi_root, "pi"), "pi", &all);
    {
        let mut tower = open_folder(
            &tower_root,
            &token_store_dir(&tower_root, "tower"),
            "tower",
            &all,
        );
        for i in 0..200 {
            fs::write(tower_root.join(format!("f{i}.txt")), format!("{i}")).unwrap();
        }
        tower.scan(ts(0)).unwrap();
        // Sync all 200 files to pi, then delete 25 and sync those deletions to pi, then delete 25
        // more while pi is offline.
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();
        tower
            .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(1))
            .unwrap();
        for i in 0..25 {
            fs::remove_file(tower_root.join(format!("f{i}.txt"))).unwrap();
        }
        tower.scan(ts(2)).unwrap();
        assert_eq!(tower.flush_settled_deletions(ts(35)).unwrap(), 25);
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(36))
            .unwrap();
        for i in 25..50 {
            fs::remove_file(tower_root.join(format!("f{i}.txt"))).unwrap();
        }
        tower.scan(ts(40)).unwrap();
        assert_eq!(tower.flush_settled_deletions(ts(75)).unwrap(), 25);
    }
    let tower_db = tower_root.join(SYNC_INDEX_FILE);
    let orig = fs::read(&tower_db).unwrap();
    let orig_marker = fs::read(tower_root.join(".legion-sync/marker.json")).unwrap();
    type DamageCase = (&'static str, Box<dyn Fn(&std::path::Path)>);
    let cases: Vec<DamageCase> = vec![
        (
            "truncate to 0",
            Box::new(|p| {
                fs::File::create(p).unwrap();
            }),
        ),
        (
            "truncate to 10%",
            Box::new(|p| {
                let f = fs::OpenOptions::new().write(true).open(p).unwrap();
                f.set_len((fs::metadata(p).unwrap().len()) / 10).unwrap();
            }),
        ),
        (
            "truncate to 50%",
            Box::new(|p| {
                let f = fs::OpenOptions::new().write(true).open(p).unwrap();
                f.set_len((fs::metadata(p).unwrap().len()) / 2).unwrap();
            }),
        ),
        (
            "truncate to 90%",
            Box::new(|p| {
                let f = fs::OpenOptions::new().write(true).open(p).unwrap();
                f.set_len((fs::metadata(p).unwrap().len()) * 9 / 10)
                    .unwrap();
            }),
        ),
        (
            "zero first 4 KiB",
            Box::new(|p| {
                let mut f = fs::OpenOptions::new().write(true).open(p).unwrap();
                f.write_all(&[0u8; 4096]).unwrap();
            }),
        ),
        (
            "zero 64 KiB at middle",
            Box::new(|p| {
                let n = fs::metadata(p).unwrap().len();
                let mut f = fs::OpenOptions::new().write(true).open(p).unwrap();
                f.seek(SeekFrom::Start(n / 2)).unwrap();
                f.write_all(&vec![0u8; 65536]).unwrap();
            }),
        ),
        (
            "garbage tail",
            Box::new(|p| {
                let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
                f.write_all(&[0xAB; 9999]).unwrap();
            }),
        ),
    ];
    for (name, damage) in cases {
        fs::write(&tower_db, &orig).unwrap();
        damage(&tower_db);
        let err = match RedbIndexStore::open(&tower_db) {
            Ok(_) => panic!("{name}: damaged index must be refused on open"),
            Err(e) => e,
        };
        match &err {
            SyncError::IndexDamaged { sentence, .. } => {
                assert_eq!(
                    sentence, INDEX_DAMAGED_PROBLEM_SENTENCE,
                    "{name}: wrong problem sentence"
                );
            }
            other => panic!("{name}: expected SyncError::IndexDamaged, got {other:?}"),
        }
        assert_eq!(
            fs::read(tower_root.join(".legion-sync/marker.json")).unwrap(),
            orig_marker,
            "{name}: marker must not change when index is damaged"
        );
    }

    // Explicit rebuild-from-disk recovers cleanly as a Decision 3 true join: keeps all 150 live
    // files, honours the 25 deletions known to pi, and receives the 25 files whose unsynced
    // deletions lived only in the destroyed index without losing any data.
    let (mut rebuilt, report) = SyncEngine::rebuild_folder_from_disk(
        cfg("tower", &all),
        &tower_root,
        token_store_for(&tower_root, "tower"),
        ts(80),
    )
    .unwrap();
    assert!(!rebuilt.is_index_damaged());
    assert_eq!(report.created, 150);
    for k in 0..3 {
        rebuilt
            .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(90 + k))
            .unwrap();
        pi.pull_from_peer(&EnginePeer::new(&rebuilt, "pi"), ts(90 + k))
            .unwrap();
    }
    for i in 0..25 {
        assert!(
            !tower_root.join(format!("f{i}.txt")).exists(),
            "f{i}.txt resurrected on tower after rebuild"
        );
        assert!(
            !pi_root.join(format!("f{i}.txt")).exists(),
            "f{i}.txt came back on pi after rebuild"
        );
    }
    for i in 25..200 {
        assert_eq!(
            fs::read_to_string(tower_root.join(format!("f{i}.txt"))).unwrap(),
            format!("{i}")
        );
        assert_eq!(
            fs::read_to_string(pi_root.join(format!("f{i}.txt"))).unwrap(),
            format!("{i}")
        );
    }
}

#[test]
fn i2_missing_index_beside_existing_marker_stops_as_damaged_until_rebuilt() {
    let tmp = scratch();
    let t = tmp.path();
    let all = ["tower", "pi"];
    let mut pi = open_real(&t.join("pi"), &t.join("pi.redb"), "pi", &all);
    {
        let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
        fs::create_dir_all(t.join("tower")).unwrap();
        fs::write(t.join("tower/keep.txt"), b"keep").unwrap();
        fs::write(t.join("tower/binned.txt"), b"from pi").unwrap();
        tower.scan(ts(0)).unwrap();
        pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), ts(1))
            .unwrap();
        // Delete binned.txt on pi and pull on tower so tower has a real bin entry too.
        fs::remove_file(t.join("pi/binned.txt")).unwrap();
        pi.scan(ts(2)).unwrap();
        tower
            .pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(3))
            .unwrap();
        assert_eq!(tower.list_bin().unwrap().len(), 1);
    }
    let marker_before = fs::read(t.join("tower/.legion-sync/marker.json")).unwrap();
    // Delete tower.redb while keeping the folder + .legion-sync/marker.json.
    fs::remove_file(t.join("tower.redb")).unwrap();

    let mut tower = open_real(&t.join("tower"), &t.join("tower.redb"), "tower", &all);
    assert!(tower.is_index_damaged());
    assert_eq!(
        fs::read(t.join("tower/.legion-sync/marker.json")).unwrap(),
        marker_before,
        "marker must not be overwritten when index is missing beside existing folder"
    );
    let st = tower.status().unwrap();
    match &st.state {
        MemberState::Problem { sentence, problems } => {
            assert_eq!(sentence, INDEX_DAMAGED_PROBLEM_SENTENCE);
            assert!(problems.iter().any(|p| p.kind == ProblemKind::IndexDamaged));
        }
        other => panic!("expected IndexDamaged problem state, got {other:?}"),
    }
    assert!(matches!(
        tower.scan(ts(10)),
        Err(SyncError::IndexDamaged { .. })
    ));
    assert!(matches!(
        tower.pull_from_peer(&EnginePeer::new(&pi, "tower"), ts(10)),
        Err(SyncError::IndexDamaged { .. })
    ));

    // Explicit rebuild recovers the live file AND restores bin metadata sidecars!
    let report = tower.rebuild_from_disk(ts(11)).unwrap();
    assert!(!tower.is_index_damaged());
    assert_eq!(report.created, 1);
    assert_eq!(tower.list_bin().unwrap().len(), 1);
    assert!(matches!(
        tower.status().unwrap().state,
        MemberState::WaitingForMember { .. } | MemberState::NeverReached
    ));
}

struct PanickingStore {
    inner: MemoryIndexStore,
    panic_now: AtomicBool,
}

impl IndexStore for PanickingStore {
    fn schema_version(&self) -> legion_sync::Result<u32> {
        if self.panic_now.load(Ordering::Relaxed) {
            panic!("simulated redb btree corruption panic");
        }
        self.inner.schema_version()
    }
    fn migrate_schema_in_place(&mut self) -> legion_sync::Result<bool> {
        if self.panic_now.load(Ordering::Relaxed) {
            panic!("simulated redb migrate panic");
        }
        self.inner.migrate_schema_in_place()
    }
    fn load_meta(&self) -> legion_sync::Result<Option<FolderMeta>> {
        if self.panic_now.load(Ordering::Relaxed) {
            panic!("simulated redb load_meta panic");
        }
        self.inner.load_meta()
    }
    fn get_record(&self, path: &str) -> legion_sync::Result<Option<IndexRecord>> {
        self.inner.get_record(path)
    }
    fn scan_records_in_order(
        &self,
        visitor: &mut dyn FnMut(IndexRecord) -> legion_sync::Result<bool>,
    ) -> legion_sync::Result<()> {
        if self.panic_now.load(Ordering::Relaxed) {
            panic!("simulated redb scan panic");
        }
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
        limit: legion_sync::PageLimit,
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
    fn get_seq_fingerprint(&self, seq: u64) -> legion_sync::Result<Option<Hash>> {
        self.inner.get_seq_fingerprint(seq)
    }
    fn list_problems(&self) -> legion_sync::Result<Vec<(String, SyncProblem)>> {
        self.inner.list_problems()
    }
    fn list_deferrals(&self) -> legion_sync::Result<Vec<DeferredItem>> {
        self.inner.list_deferrals()
    }
    fn record_count(&self) -> legion_sync::Result<u64> {
        self.inner.record_count()
    }
    fn live_file_count(&self) -> legion_sync::Result<u64> {
        self.inner.live_file_count()
    }
    fn apply_batch(&mut self, batch: StoreBatch) -> legion_sync::Result<()> {
        if self.panic_now.load(Ordering::Relaxed) {
            panic!("simulated redb commit panic");
        }
        self.inner.apply_batch(batch)
    }
}

#[test]
fn i3_store_panic_is_caught_at_engine_boundary() {
    let tmp = scratch();
    let root = tmp.path().join("tower");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"hello").unwrap();

    let store = PanickingStore {
        inner: MemoryIndexStore::new(),
        panic_now: AtomicBool::new(false),
    };
    let mut engine = SyncEngine::open(
        cfg("tower", &["tower", "pi"]),
        RealFs::open(&root).unwrap(),
        store,
        MemoryInstanceTokenStore::new(),
    )
    .unwrap();
    engine.scan(ts(0)).unwrap();

    // Arm a store panic during scan -> engine must catch it, stop with IndexDamaged, and touch
    // nothing on disk.
    engine.store().panic_now.store(true, Ordering::Relaxed);
    let err = engine.scan(ts(1)).unwrap_err();
    match err {
        SyncError::IndexDamaged { sentence, .. } => {
            assert_eq!(sentence, INDEX_DAMAGED_PROBLEM_SENTENCE);
        }
        other => panic!("expected IndexDamaged from caught panic, got {other:?}"),
    }
    assert!(engine.is_index_damaged());
    match engine.status().unwrap().state {
        MemberState::Problem { sentence, problems } => {
            assert_eq!(sentence, INDEX_DAMAGED_PROBLEM_SENTENCE);
            assert!(problems.iter().any(|p| p.kind == ProblemKind::IndexDamaged));
        }
        other => panic!("expected IndexDamaged status, got {other:?}"),
    }
    assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"hello");

    // Once the store is healthy again, explicit rebuild clears the damaged state.
    engine.store().panic_now.store(false, Ordering::Relaxed);
    engine.rebuild_from_disk(ts(2)).unwrap();
    assert!(!engine.is_index_damaged());
    assert!(matches!(
        engine.status().unwrap().state,
        MemberState::WaitingForMember { .. } | MemberState::NeverReached
    ));
}

#[test]
fn i4_schema_upgrade_migrates_in_place_and_ancient_schema_is_refused() {
    let tmp = scratch();
    let root = tmp.path().join("tower");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("keep.txt"), b"preserved").unwrap();
    fs::write(root.join("gone.txt"), b"to delete").unwrap();

    let inc_before;
    let keep_prov_before;
    {
        let mut engine = open_folder(
            &root,
            &token_store_dir(&root, "tower"),
            "tower",
            &["tower", "pi"],
        );
        engine.scan(ts(0)).unwrap();
        fs::remove_file(root.join("gone.txt")).unwrap();
        engine.scan(ts(1)).unwrap();
        engine.flush_settled_deletions(ts(40)).unwrap();
        inc_before = engine.meta().unwrap().incarnation;
        keep_prov_before = engine
            .store()
            .get_record("keep.txt")
            .unwrap()
            .unwrap()
            .provenance;
        engine
            .store_mut()
            .write_schema_version_for_test(legion_sync::MIN_MIGRATABLE_SCHEMA_VERSION)
            .unwrap();
    }

    // Decision 1: a migratable schema version upgrades IN PLACE without rebuilding, preserving
    // incarnation, versions, live records, and deletion tombstones.
    {
        let engine = open_folder(
            &root,
            &token_store_dir(&root, "tower"),
            "tower",
            &["tower", "pi"],
        );
        assert_eq!(
            engine.store().schema_version().unwrap(),
            legion_sync::INDEX_SCHEMA_VERSION
        );
        assert_eq!(engine.meta().unwrap().incarnation, inc_before);
        assert_eq!(
            engine
                .store()
                .get_record("keep.txt")
                .unwrap()
                .unwrap()
                .provenance,
            keep_prov_before
        );
        assert!(
            engine
                .store()
                .get_record("gone.txt")
                .unwrap()
                .unwrap()
                .deleted
        );
    }

    // An ancient schema version older than MIN_MIGRATABLE_SCHEMA_VERSION is still refused as
    // IndexDamaged instead of misreading it, and can be explicitly rebuilt from disk.
    let db_path = root.join(SYNC_INDEX_FILE);
    {
        let mut store = RedbIndexStore::open(&db_path).unwrap();
        store.write_schema_version_for_test(1).unwrap();
    }
    match RedbIndexStore::open(&db_path) {
        Err(SyncError::IndexDamaged { .. }) => {}
        Err(other) => panic!("expected IndexDamaged for ancient schema version, got {other:?}"),
        Ok(_) => panic!("expected IndexDamaged for ancient schema version, got Ok"),
    }
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"preserved");

    let (engine, report) = SyncEngine::rebuild_folder_from_disk(
        cfg("tower", &["tower", "pi"]),
        &root,
        token_store_for(&root, "tower"),
        ts(50),
    )
    .unwrap();
    assert_eq!(report.created, 1);
    assert_eq!(
        engine.store().schema_version().unwrap(),
        legion_sync::INDEX_SCHEMA_VERSION
    );
    assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"preserved");
}
