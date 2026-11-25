//! Regression tests ported from the PR #64 independent review probes (`a1`..`a8`, `b1`..`b4`),
//! shape tests, and targeted real-disk verification tests for every remediation item A–K.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use legion_sync::{
    ConflictChoice, DeletionBrakeConfig, DeletionBrakeDecision, FetchOptions, FolderConfig,
    FolderMarker, FsErrorKind, INDEX_SCHEMA_VERSION, IndexRecord, IndexStore,
    MAX_DEFERRAL_ATTEMPTS, MAX_HELD_BACK_ENTRIES, MemberState, Peer, PeerHandshake, ProblemKind,
    RealFs, RecordsPage, RedbIndexStore, RestoreStrategy, SYNC_PROTOCOL_VERSION, ScanOptions,
    SyncEngine, SyncError, SyncFs, VersionKey, VersionVector, check_folders_not_nested,
    conflict_copy_path, parse_conflict_copy_path,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};

type RealEngine = SyncEngine<RealFs, RedbIndexStore>;

struct EnginePeer<'a, F: SyncFs, S: IndexStore> {
    engine: &'a SyncEngine<F, S>,
    caller: String,
    member_id: String,
    fetch_calls: Arc<AtomicU64>,
}

impl<'a, F: SyncFs, S: IndexStore> EnginePeer<'a, F, S> {
    fn new(engine: &'a SyncEngine<F, S>, caller: &str) -> Self {
        let member_id = engine.meta().unwrap().local_member;
        Self {
            engine,
            caller: caller.to_string(),
            member_id,
            fetch_calls: Arc::new(AtomicU64::new(0)),
        }
    }

    fn with_fetch_counter(
        engine: &'a SyncEngine<F, S>,
        caller: &str,
        counter: Arc<AtomicU64>,
    ) -> Self {
        let member_id = engine.meta().unwrap().local_member;
        Self {
            engine,
            caller: caller.to_string(),
            member_id,
            fetch_calls: counter,
        }
    }
}

impl<F: SyncFs, S: IndexStore> Peer for EnginePeer<'_, F, S> {
    fn member_id(&self) -> &str {
        &self.member_id
    }

    fn handshake(
        &self,
        caller: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<PeerHandshake> {
        self.engine.serve_handshake(&self.caller, caller)
    }

    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
        _cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        self.engine
            .serve_records_since(&self.caller, since_incarnation, since_seq, limit)
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.fetch_calls.fetch_add(1, Ordering::Relaxed);
        self.engine
            .serve_fetch_file_range(&self.caller, path, expected_hash, options.offset)
    }
}

struct Member {
    id: String,
    root: PathBuf,
    db: PathBuf,
    members: Vec<String>,
    ignore: Vec<String>,
    brake: DeletionBrakeConfig,
}

impl Member {
    fn new(base: &Path, id: &str, members: &[&str]) -> Self {
        Self {
            id: id.to_string(),
            root: base.join(format!("{id}-folder")),
            db: base.join(format!("{id}-index/index.redb")),
            members: members.iter().map(|s| s.to_string()).collect(),
            ignore: Vec::new(),
            brake: DeletionBrakeConfig::default(),
        }
    }

    fn open(&self) -> RealEngine {
        let mut cfg = FolderConfig::new("docs", self.id.clone(), self.members.clone());
        cfg.ignore = self.ignore.clone();
        cfg.deletion_brake = self.brake;
        let fs = RealFs::open(&self.root).unwrap();
        let store = RedbIndexStore::open(&self.db).unwrap();
        SyncEngine::open(cfg, fs, store).unwrap()
    }

    fn write(&self, rel: &str, bytes: &[u8]) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.root.join(rel)).ok()
    }

    fn tree(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        walk(&self.root, "", &mut out);
        out
    }

    fn bin_contents(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(self.root.join(".legion-sync/bin")) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().ends_with(".data") {
                    out.push(fs::read(e.path()).unwrap());
                }
            }
        }
        out
    }
}

fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.eq_ignore_ascii_case(".legion-sync") {
            continue;
        }
        let rel = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let ft = e.file_type().unwrap();
        if ft.is_dir() {
            walk(&e.path(), &rel, out);
        } else if ft.is_file() {
            out.insert(rel, fs::read(e.path()).unwrap_or_default());
        }
    }
}

fn now() -> Timestamp {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    Timestamp::new(d.as_secs() as i64, d.subsec_nanos())
}

fn pull(dst: &mut RealEngine, src: &RealEngine) -> legion_sync::Result<legion_sync::PullReport> {
    let dst_id = dst.meta().unwrap().local_member;
    let peer = EnginePeer::new(src, &dst_id);
    dst.pull_from_peer(&peer, now())
}

fn h(bytes: &[u8]) -> Hash {
    hash_bytes(bytes)
}

fn copy_tree(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap().flatten() {
        let ft = e.file_type().unwrap();
        let to = dst.join(e.file_name());
        if ft.is_dir() {
            copy_tree(&e.path(), &to);
        } else if ft.is_symlink() {
            #[cfg(unix)]
            {
                let t = fs::read_link(e.path()).unwrap();
                std::os::unix::fs::symlink(t, to).unwrap();
            }
        } else {
            fs::copy(e.path(), &to).unwrap();
            let m = fs::metadata(e.path()).unwrap().modified().unwrap();
            let f = fs::OpenOptions::new().write(true).open(&to).unwrap();
            f.set_times(fs::FileTimes::new().set_modified(m)).unwrap();
        }
    }
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

fn setup_pair(base: &Path) -> (Member, Member) {
    let a = Member::new(base, "a", &["a", "b"]);
    let b = Member::new(base, "b", &["a", "b"]);
    (a, b)
}

#[cfg(unix)]
#[test]
fn probe_a1_unreadable_subdirectory_never_deletes_anything() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    for i in 0..5 {
        ma.write(&format!("photos/{i}.jpg"), format!("photo {i}").as_bytes());
    }
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    assert_eq!(mb.tree().len(), 5);

    let dir = ma.root.join("photos");
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
    let report = a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(report.deleted, 0);
    assert!(!report.skipped_errors.is_empty());
    assert_eq!(mb.tree().len(), 5);
    assert!(mb.bin_contents().is_empty());
}

#[test]
fn probe_a2_empty_mount_point_stops_with_problem_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    for i in 0..20 {
        ma.write(&format!("docs/f{i}.txt"), format!("doc {i}").as_bytes());
    }
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    assert_eq!(mb.tree().len(), 20);

    drop(a);
    fs::rename(&ma.root, tmp.path().join("a-folder.unmounted")).unwrap();
    fs::create_dir_all(&ma.root).unwrap();
    let mut a = ma.open();
    // `.legion-sync` must NOT have been silently recreated!
    assert!(!ma.root.join(".legion-sync").exists());
    match a.status().unwrap().state {
        MemberState::Problem { sentence, problems } => {
            assert!(sentence.contains("empty or replaced") || sentence.contains("drive"));
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::FolderMarkerMissing)
            );
        }
        other => panic!("expected FolderMarkerMissing problem, got {other:?}"),
    }
    let scan_res = a.scan(now());
    assert!(matches!(scan_res, Err(SyncError::FolderUnavailable { .. })));
    let _ = pull(&mut b, &a);
    assert_eq!(mb.tree().len(), 20);
}

#[test]
fn probe_a3_dir_with_os_litter_is_removed_and_never_stalls_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    ma.write("old/x.txt", b"x");
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    mb.write("old/.DS_Store", b"finder");
    b.scan(now()).unwrap();

    fs::remove_dir_all(ma.root.join("old")).unwrap();
    for i in 0..10 {
        ma.write(&format!("new/{i}.txt"), format!("n{i}").as_bytes());
    }
    a.scan(now()).unwrap();

    let peer = EnginePeer::new(&a, "b");
    for _ in 0..5 {
        b.pull_from_peer_with_limit(&peer, 4, now()).unwrap();
    }
    let got = mb.tree().keys().filter(|k| k.starts_with("new/")).count();
    assert_eq!(got, 10);
    assert!(!mb.root.join("old").exists());
    assert!(mb.bin_contents().iter().any(|c| c == b"finder"));
}

#[test]
fn probe_a4_member_restored_from_backup_preserves_new_edits_and_new_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    mb.write("report.txt", b"v1 from b");
    b.scan(now()).unwrap();
    pull(&mut a, &b).unwrap();

    drop(b);
    copy_tree(&mb.root, &tmp.path().join("b-backup-folder"));
    copy_tree(mb.db.parent().unwrap(), &tmp.path().join("b-backup-index"));
    let mut b = mb.open();

    for i in 2..=6 {
        sleep_ms(20);
        mb.write("report.txt", format!("v{i} from b").as_bytes());
        b.scan(now()).unwrap();
    }
    for i in 0..5 {
        mb.write(&format!("notes/{i}.txt"), b"n");
    }
    b.scan(now()).unwrap();
    pull(&mut a, &b).unwrap();
    assert_eq!(ma.read("report.txt").unwrap(), b"v6 from b");

    drop(b);
    fs::remove_dir_all(&mb.root).unwrap();
    fs::remove_dir_all(mb.db.parent().unwrap()).unwrap();
    copy_tree(&tmp.path().join("b-backup-folder"), &mb.root);
    copy_tree(&tmp.path().join("b-backup-index"), mb.db.parent().unwrap());
    let mut b = mb.open();

    sleep_ms(20);
    let important = b"IMPORTANT new text written after the restore";
    mb.write("report.txt", important);
    mb.write("todo.txt", b"new file after restore");
    b.scan(now()).unwrap();

    for _ in 0..4 {
        let _ = pull(&mut a, &b);
        let _ = pull(&mut b, &a);
    }

    let a_tree = ma.tree();
    let b_tree = mb.tree();
    assert_eq!(a_tree, b_tree);
    assert!(a_tree.values().any(|v| v == important));
    assert!(a_tree.values().any(|v| v == b"v6 from b"));
    assert_eq!(
        ma.read("todo.txt").as_deref(),
        Some(b"new file after restore".as_slice())
    );
}

#[test]
fn probe_a5_reinstalled_member_propagates_deletions_and_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    for i in 0..10 {
        mb.write(&format!("f{i}.txt"), format!("x{i}").as_bytes());
    }
    b.scan(now()).unwrap();
    for round in 0..20 {
        sleep_ms(5);
        mb.write("f9.txt", format!("edit {round}").as_bytes());
        b.scan(now()).unwrap();
    }
    pull(&mut a, &b).unwrap();
    pull(&mut b, &a).unwrap();

    drop(b);
    fs::remove_dir_all(mb.db.parent().unwrap()).unwrap();
    let mut b = mb.open();
    // Item 7: missing index beside existing marker and non-empty folder stops as damaged until
    // explicitly rebuilt from disk.
    assert!(b.is_index_damaged());
    assert!(matches!(b.scan(now()), Err(SyncError::IndexDamaged { .. })));
    b.rebuild_from_disk(now()).unwrap();
    assert!(!b.is_index_damaged());
    pull(&mut b, &a).unwrap();
    pull(&mut a, &b).unwrap();

    fs::remove_file(mb.root.join("f3.txt")).unwrap();
    sleep_ms(20);
    mb.write("f4.txt", b"edited on b after reinstall");
    b.scan(now()).unwrap();

    pull(&mut b, &a).unwrap();
    for _ in 0..3 {
        pull(&mut a, &b).unwrap();
        pull(&mut b, &a).unwrap();
    }
    assert!(ma.read("f3.txt").is_none());
    assert!(mb.read("f3.txt").is_none());
    assert_eq!(
        ma.read("f4.txt").as_deref(),
        Some(b"edited on b after reinstall".as_slice())
    );
    assert_eq!(ma.tree(), mb.tree());
}

#[test]
fn probe_a6_restore_replace_never_overwrites_unscanned_file_without_bin() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    ma.write("plan.txt", b"old plan");
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    fs::remove_file(ma.root.join("plan.txt")).unwrap();
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    let entry = b.list_bin().unwrap().into_iter().next().unwrap();

    mb.write("plan.txt", b"NEW plan, not yet scanned");
    let res = b.restore_from_bin(&entry.id, None, RestoreStrategy::Replace, now());
    let live = mb.read("plan.txt").unwrap();
    assert!(res.is_err());
    assert_eq!(live, b"NEW plan, not yet scanned");
}

#[test]
fn probe_a7_keep_conflict_never_overwrites_unscanned_file_at_original_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    ma.write("n.txt", b"base");
    a.scan(Timestamp::new(now().secs - 100, 0)).unwrap();
    pull(&mut b, &a).unwrap();
    ma.write("n.txt", b"a edit");
    let older = SystemTime::now() - Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(ma.root.join("n.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(older))
        .unwrap();
    a.scan(now()).unwrap();
    mb.write("n.txt", b"b edit");
    b.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    let c = b.list_conflicts().unwrap().into_iter().next().unwrap();

    fs::remove_file(mb.root.join("n.txt")).unwrap();
    b.scan(now()).unwrap();
    mb.write("n.txt", b"brand new n.txt, unscanned");
    let res = b.resolve_conflict(&c.conflict_path, ConflictChoice::KeepConflict, now());
    let live = mb.read("n.txt").unwrap();
    assert!(res.is_err());
    assert_eq!(live, b"brand new n.txt, unscanned");
}

#[test]
fn probe_a8_joining_with_old_copy_does_not_resurrect_deleted_or_conflict_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let ma = Member::new(tmp.path(), "a", &["a", "d"]);
    let mut a = ma.open();
    ma.write("old-draft.txt", b"deleted long ago");
    ma.write("notes.txt", b"notes v1");
    a.scan(now()).unwrap();
    copy_tree(&ma.root, &tmp.path().join("d-folder"));
    let _ = fs::remove_dir_all(tmp.path().join("d-folder/.legion-sync"));

    fs::remove_file(ma.root.join("old-draft.txt")).unwrap();
    sleep_ms(20);
    ma.write("notes.txt", b"notes v2 (current)");
    a.scan(now()).unwrap();

    let md = Member::new(tmp.path(), "d", &["a", "d"]);
    let mut d = md.open();
    d.scan(now()).unwrap();
    for _ in 0..3 {
        pull(&mut d, &a).unwrap();
        pull(&mut a, &d).unwrap();
    }
    assert!(ma.read("old-draft.txt").is_none());
    assert!(md.read("old-draft.txt").is_none());
    assert_eq!(
        ma.read("notes.txt").as_deref(),
        Some(b"notes v2 (current)".as_slice())
    );
    assert_eq!(
        md.read("notes.txt").as_deref(),
        Some(b"notes v2 (current)".as_slice())
    );
    assert!(a.list_conflicts().unwrap().is_empty());
    assert!(d.list_conflicts().unwrap().is_empty());
    assert!(md.bin_contents().iter().any(|b| b == b"deleted long ago"));
    assert!(md.bin_contents().iter().any(|b| b == b"notes v1"));
}

struct FakePeer {
    id: String,
    incarnation: u64,
    pages: RefCell<Vec<RecordsPage>>,
    stream: Option<Vec<u8>>,
    bytes_read: Arc<AtomicU64>,
}

impl Peer for FakePeer {
    fn member_id(&self) -> &str {
        &self.id
    }

    fn handshake(
        &self,
        _caller: &legion_sync::HandshakeHello,
    ) -> legion_sync::Result<PeerHandshake> {
        Ok(PeerHandshake {
            folder_id: "docs".to_string(),
            member_id: self.id.clone(),
            incarnation: self.incarnation,
            snapshot_seq: 1,
            head_fingerprint: None,
            protocol_version: SYNC_PROTOCOL_VERSION,
            caller_pulled: None,
            caller_acked: None,
            caller_pull_matches: None,
            caller_regressed: false,
            caller_rejoining: false,
            refused_from_caller: None,
            pruned_incarnations: Vec::new(),
            known_incarnations: BTreeMap::new(),
        })
    }

    fn records_since(
        &self,
        _since_incarnation: u64,
        _since_seq: u64,
        _limit: usize,
        _cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        let mut p = self.pages.borrow_mut();
        if p.is_empty() {
            Ok(RecordsPage {
                incarnation: self.incarnation,
                records: vec![],
                snapshot_seq: 0,
                has_more: false,
                caller_acked: None,
                caller_acked_fingerprint: None,
                head_fingerprint: None,
                chain_fingerprints: BTreeMap::new(),
            })
        } else {
            Ok(p.remove(0))
        }
    }

    fn fetch_file(
        &self,
        _path: &str,
        _h: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        struct Counting(Cursor<Vec<u8>>, Arc<AtomicU64>);
        impl Read for Counting {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.read(buf)?;
                self.1.fetch_add(n as u64, Ordering::Relaxed);
                Ok(n)
            }
        }
        let data = self.stream.clone().unwrap_or_default();
        let start = (options.offset as usize).min(data.len());
        Ok(Box::new(Counting(
            Cursor::new(data[start..].to_vec()),
            Arc::clone(&self.bytes_read),
        )))
    }
}

fn rec(
    path: &str,
    kind: EntryKind,
    bytes: Option<&[u8]>,
    version: &[(&str, u64, u64)],
    by: &str,
    seq: u64,
) -> IndexRecord {
    let mut v = VersionVector::new();
    for (m, inc, c) in version {
        v.set(VersionKey::new(*m, *inc), *c);
    }
    IndexRecord {
        path: path.to_string(),
        kind,
        size: bytes.map_or(0, |b| b.len() as u64),
        mtime: now(),
        disk_mtime: None,
        disk_change_id: None,
        mode: Some(0o644),
        hash: bytes.map(h),
        link_target: None,
        deleted: false,
        version: v,
        modified_by: by.to_string(),
        seq,
        recent_hashes: Vec::new(),
        from_join: false,
        conflict_of: None,
        conflict_dismissed: false,
    }
}

#[test]
fn probe_b1_peer_fetch_caps_read_at_declared_size_plus_one() {
    let tmp = tempfile::tempdir().unwrap();
    let mb = Member::new(tmp.path(), "b", &["b", "evil"]);
    let mut b = mb.open();
    let declared = b"tiny";
    let big = vec![b'z'; 64 * 1024 * 1024];
    let mut r = rec(
        "tiny.txt",
        EntryKind::File,
        Some(declared),
        &[("evil", 1, 1)],
        "evil",
        1,
    );
    r.size = declared.len() as u64;
    let counter = Arc::new(AtomicU64::new(0));
    let peer = FakePeer {
        id: "evil".into(),
        incarnation: 1,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: 1,
            records: vec![r],
            snapshot_seq: 1,
            has_more: false,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        stream: Some(big),
        bytes_read: Arc::clone(&counter),
    };
    let _ = b.pull_from_peer(&peer, now());
    assert!(counter.load(Ordering::Relaxed) <= declared.len() as u64 + 1);
}

#[test]
fn probe_b2_one_bad_record_is_skipped_and_good_records_still_apply() {
    let tmp = tempfile::tempdir().unwrap();
    let mb = Member::new(tmp.path(), "b", &["b", "evil"]);
    let mut b = mb.open();
    let good = rec(
        "good.txt",
        EntryKind::File,
        Some(b"good"),
        &[("evil", 1, 2)],
        "evil",
        2,
    );
    let mut bad = rec(
        "x.txt",
        EntryKind::File,
        Some(b"x"),
        &[("evil", 1, 1)],
        "evil",
        1,
    );
    bad.hash = None;
    let peer = FakePeer {
        id: "evil".into(),
        incarnation: 1,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: 1,
            records: vec![bad, good],
            snapshot_seq: 2,
            has_more: false,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        stream: Some(b"good".to_vec()),
        bytes_read: Arc::new(AtomicU64::new(0)),
    };
    let report = b.pull_from_peer(&peer, now()).unwrap();
    assert_eq!(report.invalid_skipped, 1);
    assert_eq!(mb.read("good.txt").as_deref(), Some(b"good".as_slice()));
}

#[test]
fn probe_b3_hostile_peer_cannot_cause_early_tombstone_gc() {
    let tmp = tempfile::tempdir().unwrap();
    let all = ["a", "b", "c", "evil"];
    let ma = Member::new(tmp.path(), "a", &all);
    let mc = Member::new(tmp.path(), "c", &all);
    let mut a = ma.open();
    let mut c = mc.open();
    ma.write("secret-plan.txt", b"to be deleted");
    a.scan(now()).unwrap();
    pull(&mut c, &a).unwrap();
    fs::remove_file(ma.root.join("secret-plan.txt")).unwrap();
    a.scan(now()).unwrap();

    // Even if `evil` directly acknowledges everything from `a`, `b` and `c` have NOT directly
    // acknowledged `a`'s deletion tombstone, and `evil` can never speak for `b` or `c`.
    let a_inc = a.meta().unwrap().incarnation;
    let a_seq = a.meta().unwrap().local_seq;
    let a_fp = a.meta().unwrap().head_fingerprint;
    let peer = FakePeer {
        id: "evil".into(),
        incarnation: 1,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: 1,
            records: vec![],
            snapshot_seq: 0,
            has_more: false,
            caller_acked: Some((a_inc, a_seq)),
            caller_acked_fingerprint: a_fp,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        stream: None,
        bytes_read: Arc::new(AtomicU64::new(0)),
    };
    let r = a.pull_from_peer(&peer, now()).unwrap();
    assert_eq!(r.tombstones_gc, 0);

    pull(&mut a, &c).unwrap();
    pull(&mut c, &a).unwrap();
    assert!(ma.read("secret-plan.txt").is_none());
    assert!(mc.read("secret-plan.txt").is_none());
}

#[test]
fn probe_b4_peer_modified_by_cannot_forge_conflict_copy_path() {
    let tmp = tempfile::tempdir().unwrap();
    let mb = Member::new(tmp.path(), "b", &["b", "evil"]);
    let mut b = mb.open();
    mb.write("n.txt", b"mine");
    b.scan(now()).unwrap();
    let mut r = rec(
        "n.txt",
        EntryKind::File,
        Some(b"theirs"),
        &[("evil", 1, 1)],
        "x/y/z",
        1,
    );
    r.mtime = Timestamp::new(1, 0);
    let peer = FakePeer {
        id: "evil".into(),
        incarnation: 1,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: 1,
            records: vec![r],
            snapshot_seq: 1,
            has_more: false,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        stream: Some(b"theirs".to_vec()),
        bytes_read: Arc::new(AtomicU64::new(0)),
    };
    let report = b.pull_from_peer(&peer, now()).unwrap();
    assert_eq!(report.invalid_skipped, 1);
    assert!(!mb.tree().keys().any(|k| k.contains("x/y/z")));
}

fn set_mtime_ago(m: &Member, rel: &str, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    fs::OpenOptions::new()
        .write(true)
        .open(m.root.join(rel))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(t))
        .unwrap();
}

fn converge(es: &mut [RealEngine]) {
    for _ in 0..4 {
        for i in 0..es.len() {
            for j in 0..es.len() {
                if i == j {
                    continue;
                }
                let (dst, src) = if i < j {
                    let (l, r) = es.split_at_mut(j);
                    (&mut l[i], &r[0] as &RealEngine)
                } else {
                    let (l, r) = es.split_at_mut(i);
                    (&mut r[0], &l[j] as &RealEngine)
                };
                pull(dst, src).unwrap();
            }
        }
    }
}

#[test]
fn shape_three_way_concurrent_edits_keep_all_three() {
    for order in 0..6 {
        let tmp = tempfile::tempdir().unwrap();
        let ids = ["a", "b", "c"];
        let ms: Vec<Member> = ids
            .iter()
            .map(|id| Member::new(tmp.path(), id, &ids))
            .collect();
        let mut es: Vec<RealEngine> = ms.iter().map(|m| m.open()).collect();
        ms[0].write("notes.txt", b"base");
        es[0].scan(now()).unwrap();
        converge(&mut es);
        for (i, m) in ms.iter().enumerate() {
            m.write("notes.txt", format!("edit by {}", ids[i]).as_bytes());
            set_mtime_ago(m, "notes.txt", 100 - 10 * i as u64);
            es[i].scan(now()).unwrap();
        }
        let perms = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        let p = perms[order];
        {
            let (x, y) = (p[0], p[1]);
            let (dst, srce) = if x < y {
                let (l, r) = es.split_at_mut(y);
                (&mut l[x], &r[0] as &RealEngine)
            } else {
                let (l, r) = es.split_at_mut(x);
                (&mut r[0], &l[y] as &RealEngine)
            };
            pull(dst, srce).unwrap();
        }
        {
            let (x, y) = (p[2], p[0]);
            let (dst, srce) = if x < y {
                let (l, r) = es.split_at_mut(y);
                (&mut l[x], &r[0] as &RealEngine)
            } else {
                let (l, r) = es.split_at_mut(x);
                (&mut r[0], &l[y] as &RealEngine)
            };
            pull(dst, srce).unwrap();
        }
        converge(&mut es);
        let trees: Vec<_> = ms.iter().map(|m| m.tree()).collect();
        let contents: BTreeSet<Vec<u8>> = trees[0].values().cloned().collect();
        assert!(
            trees.iter().all(|t| t == &trees[0]),
            "trees differ: {trees:?}"
        );
        for id in ids {
            assert!(
                contents.contains(format!("edit by {id}").as_bytes()),
                "lost edit by {id}"
            );
        }
        assert_eq!(trees[0].len(), 3);
    }
}

#[test]
fn shape_identical_creates_empty_files_and_duplicate_content() {
    let tmp = tempfile::tempdir().unwrap();
    let ids = ["a", "b"];
    let ma = Member::new(tmp.path(), "a", &ids);
    let mb = Member::new(tmp.path(), "b", &ids);
    let (mut a, mut b) = (ma.open(), mb.open());
    ma.write("same.txt", b"identical");
    mb.write("same.txt", b"identical");
    ma.write("empty.txt", b"");
    mb.write("empty2.txt", b"");
    ma.write("dup1.txt", b"dup");
    ma.write("dir/dup2.txt", b"dup");
    a.scan(now()).unwrap();
    b.scan(now()).unwrap();
    for _ in 0..3 {
        pull(&mut a, &b).unwrap();
        pull(&mut b, &a).unwrap();
    }
    fs::remove_file(mb.root.join("dup1.txt")).unwrap();
    b.scan(now()).unwrap();
    for _ in 0..3 {
        pull(&mut a, &b).unwrap();
        pull(&mut b, &a).unwrap();
    }
    assert_eq!(ma.tree(), mb.tree());
    assert!(a.list_conflicts().unwrap().is_empty());
    assert_eq!(ma.read("dir/dup2.txt").unwrap(), b"dup");
    assert!(ma.read("dup1.txt").is_none());
}

// ============================================================================
// Targeted real-disk tests for every remaining remediation requirement (A–K)
// ============================================================================

#[test]
fn item_a_two_separate_same_day_conflicts_keep_both_distinct_hash6_names() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    let t = Timestamp::new(1_795_608_000, 0);

    // Round 1 conflict on `item.txt` lost by `b` (older mtime).
    ma.write("item.txt", b"a-round1");
    a.fs()
        .set_live_metadata("item.txt", Timestamp::new(t.secs + 100, 0), Some(0o644))
        .unwrap();
    a.scan(t).unwrap();
    mb.write("item.txt", b"b-round1");
    b.fs()
        .set_live_metadata("item.txt", Timestamp::new(t.secs + 10, 0), Some(0o644))
        .unwrap();
    b.scan(t).unwrap();
    pull(&mut a, &b).unwrap();
    pull(&mut b, &a).unwrap();

    // Round 2 conflict on `item.txt` lost by `b` on the SAME UTC date!
    ma.write("item.txt", b"a-round2");
    a.fs()
        .set_live_metadata("item.txt", Timestamp::new(t.secs + 200, 0), Some(0o644))
        .unwrap();
    a.scan(t).unwrap();
    mb.write("item.txt", b"b-round2");
    b.fs()
        .set_live_metadata("item.txt", Timestamp::new(t.secs + 150, 0), Some(0o644))
        .unwrap();
    b.scan(t).unwrap();
    pull(&mut a, &b).unwrap();
    pull(&mut b, &a).unwrap();

    assert_eq!(ma.tree(), mb.tree());
    let conflicts = a.list_conflicts().unwrap();
    assert_eq!(
        conflicts.len(),
        2,
        "both same-day conflict copies must survive"
    );
    for c in &conflicts {
        let parsed = parse_conflict_copy_path(&c.conflict_path).unwrap();
        assert_eq!(parsed.original_path, "item.txt");
        assert_eq!(parsed.member, "b");
        assert_eq!(parsed.hash6.len(), 6);
    }
    let vals: BTreeSet<Vec<u8>> = ma.tree().into_values().collect();
    assert!(vals.contains(b"a-round2".as_slice()));
    assert!(vals.contains(b"b-round1".as_slice()));
    assert!(vals.contains(b"b-round2".as_slice()));
}

#[test]
fn item_b3_mass_deletion_brake_pauses_and_handles_rescan_and_confirm() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();

    // Create 60 files (> default threshold of 50 files & > 25%).
    for i in 0..60 {
        ma.write(&format!("f{i:03}.txt"), format!("payload {i}").as_bytes());
    }
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    assert_eq!(mb.tree().len(), 60);

    // Delete 55 of 60 files (55 > 50 and 91% > 25%) -> brake MUST trip!
    for i in 0..55 {
        fs::remove_file(ma.root.join(format!("f{i:03}.txt"))).unwrap();
    }
    let scan = a.scan(now()).unwrap();
    assert!(scan.brake_tripped);
    assert_eq!(scan.deleted, 0);
    assert!(a.is_paused().unwrap());
    let st = a.status().unwrap();
    assert_eq!(st.pending_brake.as_ref().unwrap().deleted_count, 55);
    assert!(matches!(
        a.pull_from_peer(&EnginePeer::new(&b, "a"), now()),
        Err(SyncError::Paused { .. })
    ));

    // Answer 1: "they are not" (Rescan) — if files are still missing, brake trips again safely!
    let rescan1 = a
        .resolve_deletion_brake(DeletionBrakeDecision::Rescan, now())
        .unwrap();
    assert!(rescan1.brake_tripped);
    assert_eq!(rescan1.deleted, 0);

    // Restore files and choose "they are not" (Rescan) -> resumes with 0 deletions!
    for i in 0..55 {
        ma.write(&format!("f{i:03}.txt"), format!("payload {i}").as_bytes());
    }
    let rescan2 = a
        .resolve_deletion_brake(DeletionBrakeDecision::Rescan, now())
        .unwrap();
    assert!(!rescan2.brake_tripped);
    assert!(!a.is_paused().unwrap());

    // Delete 55 files again and choose "they are really deleted" (ConfirmDeleted) -> proceeds!
    for i in 0..55 {
        fs::remove_file(ma.root.join(format!("f{i:03}.txt"))).unwrap();
    }
    assert!(a.scan(now()).unwrap().brake_tripped);
    let confirmed = a
        .resolve_deletion_brake(DeletionBrakeDecision::ConfirmDeleted, now())
        .unwrap();
    assert!(!confirmed.brake_tripped);
    assert_eq!(confirmed.deleted, 55);
    // Item 8: matching inbound peer deletion brake pauses `b`'s pull from `a` before deleting 55/60
    // files, until `b` chooses `ApplyDeletions` (or `KeepLocalFiles`).
    assert!(matches!(pull(&mut b, &a), Err(SyncError::Paused { .. })));
    let b_st = b.status().unwrap();
    assert_eq!(b_st.inbound_brakes.len(), 1);
    assert_eq!(b_st.inbound_brakes[0].deleted_count, 55);
    assert_eq!(mb.tree().len(), 60);
    b.resolve_inbound_deletion_brake(
        "a",
        legion_sync::InboundDeletionBrakeDecision::ApplyDeletions,
        now(),
    )
    .unwrap();
    pull(&mut b, &a).unwrap();
    assert_eq!(mb.tree().len(), 5);
}

#[test]
fn item_c_cloned_machine_duplicate_member_id_detected_and_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let ma = Member::new(tmp.path(), "a", &["a", "b"]);
    let mb1 = Member::new(tmp.path(), "b", &["a", "b"]);
    let mut a = ma.open();
    let mut b1 = mb1.open();

    // Clone machine `b` into a second live instance `b2` with a different incarnation.
    let mb2 = Member {
        id: "b".to_string(),
        root: tmp.path().join("b-clone-folder"),
        db: tmp.path().join("b-clone-index/index.redb"),
        members: vec!["a".into(), "b".into()],
        ignore: vec![],
        brake: DeletionBrakeConfig::default(),
    };
    let mut b2 = mb2.open();

    mb1.write("from-b1.txt", b"b1");
    b1.scan(now()).unwrap();
    mb2.write("from-b2.txt", b"b2");
    b2.scan(now()).unwrap();

    // `a` talks to b1 (inc1), then b2 (inc2: looks like reinstall), then b1 (inc1 returns!).
    pull(&mut a, &b1).unwrap();
    pull(&mut a, &b2).unwrap();
    let err = pull(&mut a, &b1).unwrap_err();
    assert!(matches!(err, SyncError::Conflict { .. }));

    match a.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::DuplicateMemberId)
            );
        }
        other => panic!("expected DuplicateMemberId problem, got {other:?}"),
    }
    // Subsequent pulls from either clone stay refused until resolved.
    assert!(pull(&mut a, &b2).is_err());
}

#[test]
fn item_c_marker_and_index_incarnation_mismatch_rotates_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    ma.write("keep.txt", b"v1");
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();

    let old_inc = b.meta().unwrap().incarnation;
    drop(b);
    // Simulate restoring an older folder marker without restoring the index DB.
    let mismatched = FolderMarker::new("docs".into(), "b".into(), old_inc ^ 0x1234_5678);
    fs::write(
        mb.root.join(".legion-sync/marker.json"),
        serde_json::to_vec_pretty(&mismatched).unwrap(),
    )
    .unwrap();

    let mut b = mb.open();
    let new_inc = b.meta().unwrap().incarnation;
    assert_ne!(new_inc, old_inc);
    assert_ne!(new_inc, mismatched.incarnation);
    b.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    pull(&mut a, &b).unwrap();
    assert_eq!(ma.read("keep.txt").as_deref(), Some(b"v1".as_slice()));
    assert_eq!(mb.read("keep.txt").as_deref(), Some(b"v1".as_slice()));
}

#[test]
fn item_d_removed_and_readded_member_does_not_resurrect_gc_deleted_file() {
    let tmp = tempfile::tempdir().unwrap();
    let ids = ["a", "b", "c"];
    let ma = Member::new(tmp.path(), "a", &ids);
    let mb = Member::new(tmp.path(), "b", &ids);
    let mc = Member::new(tmp.path(), "c", &ids);
    let mut a = ma.open();
    let mut b = mb.open();
    let mut c = mc.open();

    ma.write("gone.txt", b"old secret");
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    pull(&mut c, &a).unwrap();

    // Remove `c` from `a` and `b`, then delete `gone.txt` and GC the tombstone on `a` and `b`!
    a.remove_member("c").unwrap();
    b.remove_member("c").unwrap();
    fs::remove_file(ma.root.join("gone.txt")).unwrap();
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    pull(&mut a, &b).unwrap(); // `a` gets direct ack from `b` and GCs tombstone!
    assert!(a.store().list_tombstones().unwrap().is_empty());

    // Now re-add `c` (which still has `gone.txt` on disk)!
    a.add_member("c").unwrap();
    b.add_member("c").unwrap();

    // Even if `a` pulls from `c` FIRST, `deleted_history` prevents resurrection and sends a
    // superseding tombstone so `c` moves `gone.txt` to `c`'s local bin!
    pull(&mut a, &c).unwrap();
    pull(&mut c, &a).unwrap();
    pull(&mut b, &a).unwrap();

    assert!(ma.read("gone.txt").is_none());
    assert!(mb.read("gone.txt").is_none());
    assert!(mc.read("gone.txt").is_none());
    assert!(mc.bin_contents().iter().any(|x| x == b"old secret"));
}

#[test]
fn item_f_path_safety_restore_destinations_and_held_back_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let mb = Member::new(tmp.path(), "b", &["b", "evil"]);
    let mut b = mb.open();

    // Create a bin entry to test restore destination safety.
    mb.write("ok.txt", b"safe");
    b.scan(now()).unwrap();
    let evil_inc = 99u64;
    let b_rec = b.store().get_record("ok.txt").unwrap().unwrap();
    let mut del = b_rec.clone();
    del.deleted = true;
    del.size = 0;
    del.hash = None;
    del.version.bump_above(VersionKey::new("evil", evil_inc), 1);
    del.modified_by = "evil".into();
    del.seq = 1;
    let peer = FakePeer {
        id: "evil".into(),
        incarnation: evil_inc,
        pages: RefCell::new(vec![RecordsPage {
            incarnation: evil_inc,
            records: vec![del],
            snapshot_seq: 1,
            has_more: false,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        }]),
        stream: None,
        bytes_read: Arc::new(AtomicU64::new(0)),
    };
    b.pull_from_peer(&peer, now()).unwrap();
    let bin_id = b.list_bin().unwrap()[0].id.clone();

    // Unsafe restore destinations must all be rejected!
    for bad in [
        "C:evil.txt",
        "\\\\?\\C:\\evil",
        "sub/.legion-sync/x",
        "sub/.LEGION-SYNC/x",
        "\u{FF0E}\u{FF4C}\u{FF45}\u{FF47}\u{FF49}\u{FF4F}\u{FF4E}\u{FF0D}\u{FF53}\u{FF59}\u{FF4E}\u{FF43}/x",
        "../escape.txt",
        "bad\x07ctrl.txt",
    ] {
        assert!(
            b.restore_from_bin(&bin_id, Some(bad), RestoreStrategy::FailIfExists, now())
                .is_err(),
            "restore destination {bad:?} must be refused"
        );
    }

    // Verify held-back table enforces MAX_HELD_BACK_ENTRIES.
    let win_root = tmp.path().join("win");
    let win_db = tmp.path().join("win.redb");
    let win_fs =
        RealFs::open_with_capabilities(&win_root, legion_sync::FsCapabilities::windows()).unwrap();
    let mut win = SyncEngine::open(
        FolderConfig::new("docs", "win", ["win".into(), "evil".into()]),
        win_fs,
        RedbIndexStore::open(&win_db).unwrap(),
    )
    .unwrap();
    let total = MAX_HELD_BACK_ENTRIES + 50;
    let mut seq = 0u64;
    let mut pages = Vec::new();
    for chunk_start in (0..total).step_by(500) {
        let mut recs = Vec::new();
        for i in chunk_start..(chunk_start + 500).min(total) {
            seq += 1;
            // Symlinks are held back on Windows.
            let mut r = rec(
                &format!("link_{i}"),
                EntryKind::Symlink,
                None,
                &[("evil", 1, seq)],
                "evil",
                seq,
            );
            r.link_target = Some("target".to_string());
            recs.push(r);
        }
        pages.push(RecordsPage {
            incarnation: 1,
            records: recs,
            snapshot_seq: seq,
            has_more: chunk_start + 500 < total,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: None,
            chain_fingerprints: BTreeMap::new(),
        });
    }
    let flood_peer = FakePeer {
        id: "evil".into(),
        incarnation: 1,
        pages: RefCell::new(pages),
        stream: None,
        bytes_read: Arc::new(AtomicU64::new(0)),
    };
    win.pull_from_peer(&flood_peer, now()).unwrap();
    assert_eq!(
        win.store().held_back_count().unwrap(),
        MAX_HELD_BACK_ENTRIES
    );
    // Item 4: stopped pulling at the cap instead of dropping records, with a visible problem.
    let evil_cursor = win
        .store()
        .load_peer_cursors()
        .unwrap()
        .remove("evil")
        .unwrap();
    assert_eq!(evil_cursor.seq, MAX_HELD_BACK_ENTRIES as u64);
    assert_eq!(evil_cursor.applied_ack_seq, 0);
    match win.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::HeldBackLimitReached)
            );
        }
        other => panic!("expected HeldBackLimitReached problem, got {other:?}"),
    }
}

#[test]
fn item_g_bin_ids_unique_across_restarts_and_expiry_clock_sanity_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    let t0 = Timestamp::new(1_795_608_000, 0);

    // Delete `x.txt` once, restart `b` at the exact same timestamp, delete `x.txt` again!
    ma.write("x.txt", b"same");
    a.scan(t0).unwrap();
    b.pull_from_peer(&EnginePeer::new(&a, "b"), t0).unwrap();
    fs::remove_file(ma.root.join("x.txt")).unwrap();
    a.scan(t0).unwrap();
    b.pull_from_peer(&EnginePeer::new(&a, "b"), t0).unwrap();
    drop(b);

    let mut b = mb.open();
    ma.write("x.txt", b"same");
    a.scan(Timestamp::new(t0.secs + 1, 0)).unwrap();
    b.pull_from_peer(&EnginePeer::new(&a, "b"), t0).unwrap();
    fs::remove_file(ma.root.join("x.txt")).unwrap();
    a.scan(Timestamp::new(t0.secs + 2, 0)).unwrap();
    b.pull_from_peer(&EnginePeer::new(&a, "b"), t0).unwrap();

    let bin = b.list_bin().unwrap();
    assert_eq!(bin.len(), 2, "bin IDs must not collide across restarts");
    assert_ne!(bin[0].id, bin[1].id);

    // Implausible clock (year 1970 or earlier than newest bin entry) must NEVER expire bin!
    assert!(b.expire_bin(Timestamp::new(100, 0)).unwrap().is_empty());
    assert!(
        b.expire_bin(Timestamp::new(t0.secs - 10, 0))
            .unwrap()
            .is_empty()
    );
    assert_eq!(b.list_bin().unwrap().len(), 2);

    // Valid clock > 30 days later expires cleanly.
    let expired = b
        .expire_bin(Timestamp::new(t0.secs + 31 * 86_400, 0))
        .unwrap();
    assert_eq!(expired.len(), 2);
}

#[test]
fn item_h_dir_with_custom_ignored_file_kept_reported_once_and_cursor_advances() {
    let tmp = tempfile::tempdir().unwrap();
    let mut ma = Member::new(tmp.path(), "a", &["a", "b"]);
    let mut mb = Member::new(tmp.path(), "b", &["a", "b"]);
    ma.ignore = vec!["*.log".into()];
    mb.ignore = vec!["*.log".into()];
    let mut a = ma.open();
    let mut b = mb.open();

    ma.write("work/keep.txt", b"sync");
    a.scan(now()).unwrap();
    pull(&mut b, &a).unwrap();
    mb.write("work/local.log", b"ignored log");
    b.scan(now()).unwrap();

    fs::remove_dir_all(ma.root.join("work")).unwrap();
    ma.write("after.txt", b"after");
    a.scan(now()).unwrap();

    pull(&mut b, &a).unwrap();
    // Directory stayed for local.log, after.txt synced, and problem reported ONCE!
    assert!(mb.root.join("work/local.log").exists());
    assert_eq!(mb.read("after.txt").as_deref(), Some(b"after".as_slice()));
    match b.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            let kept_probs: Vec<_> = problems
                .iter()
                .filter(|p| p.kind == ProblemKind::DirectoryKeptLocalFiles)
                .collect();
            assert_eq!(kept_probs.len(), 1);
            assert!(
                kept_probs[0]
                    .sentence
                    .contains("kept here because it still holds files that are not synced")
            );
        }
        other => panic!("expected DirectoryKeptLocalFiles problem, got {other:?}"),
    }

    // Re-scanning `b` must NOT revive `work/` back onto `a`!
    b.scan(now()).unwrap();
    pull(&mut a, &b).unwrap();
    assert!(!ma.root.join("work").exists());
}

#[test]
fn item_h_bounded_deferrals_promote_to_problem_and_advance_cursor() {
    let tmp = tempfile::tempdir().unwrap();
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    ma.write("bad.bin", b"original");
    a.scan(now()).unwrap();
    // Mutate `bad.bin` on `a` without rescanning so every fetch fails hash check (defers!).
    ma.write("bad.bin", b"mutated-unscanned");
    ma.write("good.bin", b"good");
    // Only scan `good.bin` via `scan_subtree` so `bad.bin` remains mismatched!
    a.scan_subtree("good.bin", now(), ScanOptions::default())
        .unwrap();

    let peer = EnginePeer::new(&a, "b");
    for _ in 0..MAX_DEFERRAL_ATTEMPTS {
        b.pull_from_peer(&peer, now()).unwrap();
    }
    // After MAX_DEFERRAL_ATTEMPTS, `bad.bin` is promoted to a visible problem and cursor advances!
    assert_eq!(mb.read("good.bin").as_deref(), Some(b"good".as_slice()));
    match b.status().unwrap().state {
        MemberState::Problem { problems, .. } => {
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::DeferredLimitExceeded)
            );
        }
        other => panic!("expected DeferredLimitExceeded problem, got {other:?}"),
    }
}

#[test]
fn item_i_nested_sync_folders_refused_and_git_is_not_fixed_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let outer = tmp.path().join("outer");
    let inner = outer.join("nested/inner");
    let outer_db = tmp.path().join("outer.redb");
    let inner_db = tmp.path().join("inner.redb");

    let mut outer_eng = SyncEngine::open(
        FolderConfig::new("outer", "a", ["a".into()]),
        RealFs::open(&outer).unwrap(),
        RedbIndexStore::open(&outer_db).unwrap(),
    )
    .unwrap();

    // `.git` is NOT ignored by fixed rules!
    fs::create_dir_all(outer.join(".git")).unwrap();
    fs::write(outer.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
    let scan = outer_eng.scan(now()).unwrap();
    assert!(scan.created >= 2);
    assert!(outer_eng.store().get_record(".git/HEAD").unwrap().is_some());

    // Creating a sync folder INSIDE an existing sync folder is refused!
    fs::create_dir_all(&inner).unwrap();
    let nested_res = SyncEngine::open(
        FolderConfig::new("inner", "a", ["a".into()]),
        RealFs::open(&inner).unwrap(),
        RedbIndexStore::open(&inner_db).unwrap(),
    );
    assert!(matches!(nested_res, Err(SyncError::InvalidConfig { .. })));

    // Also verify `check_folders_not_nested` catches both directions!
    assert!(check_folders_not_nested(&outer, &[inner.as_path()]).is_err());
    assert!(check_folders_not_nested(&inner, &[outer.as_path()]).is_err());
    let sibling = tmp.path().join("sibling");
    assert!(check_folders_not_nested(&outer, &[sibling.as_path()]).is_ok());

    // Creating a parent sync folder CONTAINING an existing sync folder is refused!
    let parent_dir = tmp.path().join("parent");
    let child_dir = parent_dir.join("child");
    let _child_eng = SyncEngine::open(
        FolderConfig::new("child", "a", ["a".into()]),
        RealFs::open(&child_dir).unwrap(),
        RedbIndexStore::open(tmp.path().join("child.redb")).unwrap(),
    )
    .unwrap();
    let parent_res = SyncEngine::open(
        FolderConfig::new("parent", "a", ["a".into()]),
        RealFs::open(&parent_dir).unwrap(),
        RedbIndexStore::open(tmp.path().join("parent.redb")).unwrap(),
    );
    assert!(matches!(parent_res, Err(SyncError::InvalidConfig { .. })));
}

#[test]
fn item_k_schema_too_new_full_checksum_scan_subtree_and_local_rename() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. Schema version newer than supported is refused cleanly at store open.
    let db_path = tmp.path().join("future.redb");
    {
        let mut store = RedbIndexStore::open(&db_path).unwrap();
        store
            .write_schema_version_for_test(INDEX_SCHEMA_VERSION + 1)
            .unwrap();
    }
    let res = RedbIndexStore::open(&db_path);
    assert!(matches!(
        res,
        Err(SyncError::SchemaTooNew { found, supported })
            if found == INDEX_SCHEMA_VERSION + 1 && supported == INDEX_SCHEMA_VERSION
    ));

    // 2. Full-checksum scan (`verify_checksums: true`) catches same-size, same-mtime edit even if
    //    `disk_change_id` is absent/cleared.
    let (ma, mb) = setup_pair(tmp.path());
    let mut a = ma.open();
    let mut b = mb.open();
    let t0 = Timestamp::new(1_795_608_000, 0);
    ma.write("sub/file.txt", b"aaaa");
    a.fs()
        .set_live_metadata("sub/file.txt", t0, Some(0o644))
        .unwrap();
    a.scan(t0).unwrap();

    // Mutate bytes with identical size & mtime.
    ma.write("sub/file.txt", b"bbbb");
    a.fs()
        .set_live_metadata("sub/file.txt", t0, Some(0o644))
        .unwrap();
    let r_verify = a
        .scan_with_options(
            t0,
            ScanOptions {
                verify_checksums: true,
                ..ScanOptions::default()
            },
        )
        .unwrap();
    assert_eq!(r_verify.hashed, 1);
    assert_eq!(r_verify.modified, 1);

    // 3. `scan_subtree` rescans ONLY the requested subtree.
    ma.write("other/ignored_for_now.txt", b"other");
    ma.write("sub/new_in_sub.txt", b"inside-sub");
    let r_sub = a.scan_subtree("sub", t0, ScanOptions::default()).unwrap();
    assert_eq!(r_sub.created, 1);
    assert!(
        a.store()
            .get_record("sub/new_in_sub.txt")
            .unwrap()
            .is_some()
    );
    assert!(
        a.store()
            .get_record("other/ignored_for_now.txt")
            .unwrap()
            .is_none()
    );

    // 4. Local rename optimization: delete + create with same hash performs 0 `fetch_file` calls!
    a.scan(t0).unwrap();
    pull(&mut b, &a).unwrap();
    let fetch_counter = Arc::new(AtomicU64::new(0));
    fs::rename(
        ma.root.join("sub/new_in_sub.txt"),
        ma.root.join("sub/renamed.txt"),
    )
    .unwrap();
    a.scan(Timestamp::new(t0.secs + 10, 0)).unwrap();
    let peer = EnginePeer::with_fetch_counter(&a, "b", Arc::clone(&fetch_counter));
    let pull_rep = b
        .pull_from_peer(&peer, Timestamp::new(t0.secs + 10, 0))
        .unwrap();
    assert_eq!(pull_rep.local_renames, 1);
    assert_eq!(
        fetch_counter.load(Ordering::Relaxed),
        0,
        "local rename optimization must transfer 0 files"
    );
    assert!(mb.read("sub/new_in_sub.txt").is_none());
    assert_eq!(
        mb.read("sub/renamed.txt").as_deref(),
        Some(b"inside-sub".as_slice())
    );
}

#[test]
fn item_g_mid_apply_non_crash_error_recovers_immediately() {
    struct FailOnReplaceFs {
        inner: RealFs,
        fail_once: Cell<bool>,
    }

    impl SyncFs for FailOnReplaceFs {
        fn capabilities(&self) -> legion_sync::FsCapabilities {
            self.inner.capabilities()
        }
        fn read_marker(&self) -> legion_sync::Result<Option<FolderMarker>> {
            self.inner.read_marker()
        }
        fn write_marker(&self, marker: &FolderMarker) -> legion_sync::Result<()> {
            self.inner.write_marker(marker)
        }
        fn append_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
            self.inner.append_deletion_log(bytes)
        }
        fn read_deletion_log(&self) -> legion_sync::Result<Vec<u8>> {
            self.inner.read_deletion_log()
        }
        fn compact_deletion_log(&self, bytes: &[u8]) -> legion_sync::Result<()> {
            self.inner.compact_deletion_log(bytes)
        }
        fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
            self.inner.ensure_internal_dirs()
        }
        fn walk(
            &self,
            ignore: &legion_sync::IgnoreMatcher,
        ) -> legion_sync::Result<legion_sync::FsWalkResult> {
            self.inner.walk(ignore)
        }
        fn walk_subtree(
            &self,
            rel_root: &str,
            ignore: &legion_sync::IgnoreMatcher,
        ) -> legion_sync::Result<legion_sync::FsWalkResult> {
            self.inner.walk_subtree(rel_root, ignore)
        }
        fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<legion_sync::FsMeta>> {
            self.inner.stat_live(rel_path)
        }
        fn hash_live(&self, rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
            self.inner.hash_live(rel_path)
        }
        fn open_live_read_at(
            &self,
            rel_path: &str,
            offset: u64,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            self.inner.open_live_read_at(rel_path, offset)
        }
        fn create_dir_all_live(&self, rel_path: &str) -> legion_sync::Result<()> {
            self.inner.create_dir_all_live(rel_path)
        }
        fn list_dir_live(&self, rel_path: &str) -> legion_sync::Result<Vec<String>> {
            self.inner.list_dir_live(rel_path)
        }
        fn remove_empty_dir_live(&self, rel_path: &str) -> legion_sync::Result<()> {
            self.inner.remove_empty_dir_live(rel_path)
        }
        fn create_symlink_live(&self, rel_path: &str, target: &str) -> legion_sync::Result<()> {
            self.inner.create_symlink_live(rel_path, target)
        }
        fn remove_symlink_live(&self, rel_path: &str) -> legion_sync::Result<()> {
            self.inner.remove_symlink_live(rel_path)
        }
        fn set_live_metadata(
            &self,
            rel_path: &str,
            mtime: Timestamp,
            mode: Option<u32>,
        ) -> legion_sync::Result<()> {
            self.inner.set_live_metadata(rel_path, mtime, mode)
        }
        fn rename_live_to_live_no_replace(
            &self,
            from_rel: &str,
            to_rel: &str,
        ) -> legion_sync::Result<()> {
            self.inner.rename_live_to_live_no_replace(from_rel, to_rel)
        }
        fn create_tmp(
            &self,
            tmp_name: &str,
        ) -> legion_sync::Result<Box<dyn legion_sync::SyncWrite>> {
            self.inner.create_tmp(tmp_name)
        }
        fn open_tmp_append(
            &self,
            tmp_name: &str,
        ) -> legion_sync::Result<Box<dyn legion_sync::SyncWrite>> {
            self.inner.open_tmp_append(tmp_name)
        }
        fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<legion_sync::FsMeta>> {
            self.inner.stat_tmp(tmp_name)
        }
        fn set_tmp_metadata(
            &self,
            tmp_name: &str,
            mtime: Timestamp,
            mode: Option<u32>,
        ) -> legion_sync::Result<()> {
            self.inner.set_tmp_metadata(tmp_name, mtime, mode)
        }
        fn hash_tmp(&self, tmp_name: &str) -> legion_sync::Result<(Hash, u64)> {
            self.inner.hash_tmp(tmp_name)
        }
        fn remove_tmp(&self, tmp_name: &str) -> legion_sync::Result<()> {
            self.inner.remove_tmp(tmp_name)
        }
        fn list_tmp(&self) -> legion_sync::Result<Vec<String>> {
            self.inner.list_tmp()
        }
        fn commit_tmp_to_live_no_replace(
            &self,
            tmp_name: &str,
            live_rel: &str,
        ) -> legion_sync::Result<()> {
            self.inner.commit_tmp_to_live_no_replace(tmp_name, live_rel)
        }
        fn exchange_tmp_with_live(
            &self,
            tmp_name: &str,
            live_rel: &str,
        ) -> legion_sync::Result<bool> {
            if self.fail_once.replace(false) {
                return Err(SyncError::Fs {
                    path: Some(live_rel.to_string()),
                    detail: "simulated ENOSPC mid-apply".to_string(),
                    kind: FsErrorKind::NoSpace,
                });
            }
            self.inner.exchange_tmp_with_live(tmp_name, live_rel)
        }
        fn move_live_to_tmp_no_replace(
            &self,
            live_rel: &str,
            tmp_name: &str,
        ) -> legion_sync::Result<()> {
            self.inner.move_live_to_tmp_no_replace(live_rel, tmp_name)
        }
        fn move_tmp_to_bin_no_replace(
            &self,
            tmp_name: &str,
            bin_data_name: &str,
        ) -> legion_sync::Result<()> {
            self.inner
                .move_tmp_to_bin_no_replace(tmp_name, bin_data_name)
        }
        fn move_live_to_bin_no_replace(
            &self,
            live_rel: &str,
            bin_data_name: &str,
        ) -> legion_sync::Result<()> {
            self.inner
                .move_live_to_bin_no_replace(live_rel, bin_data_name)
        }
        fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> legion_sync::Result<()> {
            self.inner.write_bin_meta(meta_name, bytes)
        }
        fn read_bin_meta(&self, meta_name: &str) -> legion_sync::Result<Option<Vec<u8>>> {
            self.inner.read_bin_meta(meta_name)
        }
        fn stat_bin(&self, bin_name: &str) -> legion_sync::Result<Option<legion_sync::FsMeta>> {
            self.inner.stat_bin(bin_name)
        }
        fn hash_bin(&self, bin_name: &str) -> legion_sync::Result<(Hash, u64)> {
            self.inner.hash_bin(bin_name)
        }
        fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> legion_sync::Result<()> {
            self.inner.copy_bin_to_tmp(bin_data_name, tmp_name)
        }
        fn remove_bin_file(&self, bin_name: &str) -> legion_sync::Result<()> {
            self.inner.remove_bin_file(bin_name)
        }
        fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
            self.inner.list_bin()
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let ma = Member::new(tmp.path(), "a", &["a", "b"]);
    let mut a = ma.open();
    let b_root = tmp.path().join("b-folder");
    let b_db = tmp.path().join("b.redb");
    let fail_fs = FailOnReplaceFs {
        inner: RealFs::open(&b_root).unwrap(),
        fail_once: Cell::new(false),
    };
    let mut b = SyncEngine::open(
        FolderConfig::new("docs", "b", ["a".into(), "b".into()]),
        fail_fs,
        RedbIndexStore::open(&b_db).unwrap(),
    )
    .unwrap();

    ma.write("doc.txt", b"v1");
    a.scan(now()).unwrap();
    b.pull_from_peer(&EnginePeer::new(&a, "b"), now()).unwrap();

    sleep_ms(20);
    ma.write("doc.txt", b"v2");
    a.scan(now()).unwrap();
    b.fs().fail_once.set(true);

    let rep = b.pull_from_peer(&EnginePeer::new(&a, "b"), now()).unwrap();
    assert_eq!(
        rep.held_back, 1,
        "local ENOSPC placement error sets single record aside as held back"
    );
    assert!(
        matches!(
            b.status().unwrap().state,
            MemberState::Problem { ref problems, .. }
                if problems.iter().any(|p| p.kind == ProblemKind::NoSpace)
        ),
        "status must report ProblemKind::NoSpace"
    );
    // Immediate recovery must have cleared PendingOp and tmp files WITHOUT waiting for restart!
    assert!(b.store().load_pending_op().unwrap().is_none());
    assert!(b.fs().list_tmp().unwrap().is_empty());
    assert_eq!(fs::read(b_root.join("doc.txt")).unwrap(), b"v1");

    // Next pull retries the held-back record and succeeds cleanly!
    let rep2 = b.pull_from_peer(&EnginePeer::new(&a, "b"), now()).unwrap();
    assert_eq!(rep2.held_back_resolved, 1);
    assert_eq!(fs::read(b_root.join("doc.txt")).unwrap(), b"v2");
    assert_eq!(b.status().unwrap().state, MemberState::UpToDate);
}

#[test]
fn item_k_ranged_resumable_fetch_and_cancellation() {
    struct FlakyRangedPeer {
        id: String,
        incarnation: u64,
        record: IndexRecord,
        payload: Vec<u8>,
        fail_first_after: usize,
        calls: Cell<usize>,
    }

    impl Peer for FlakyRangedPeer {
        fn member_id(&self) -> &str {
            &self.id
        }

        fn handshake(
            &self,
            _caller: &legion_sync::HandshakeHello,
        ) -> legion_sync::Result<PeerHandshake> {
            Ok(PeerHandshake {
                folder_id: "docs".into(),
                member_id: self.id.clone(),
                incarnation: self.incarnation,
                snapshot_seq: 1,
                head_fingerprint: None,
                protocol_version: SYNC_PROTOCOL_VERSION,
                caller_pulled: None,
                caller_acked: None,
                caller_pull_matches: None,
                caller_regressed: false,
                caller_rejoining: false,
                refused_from_caller: None,
                pruned_incarnations: Vec::new(),
                known_incarnations: BTreeMap::new(),
            })
        }

        fn records_since(
            &self,
            _inc: u64,
            since_seq: u64,
            _limit: usize,
            cancel: Option<&AtomicBool>,
        ) -> legion_sync::Result<RecordsPage> {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(SyncError::Cancelled {
                    operation: "records_since".into(),
                });
            }
            let recs = if since_seq == 0 {
                vec![self.record.clone()]
            } else {
                vec![]
            };
            Ok(RecordsPage {
                incarnation: self.incarnation,
                records: recs,
                snapshot_seq: 1,
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
            _expected_hash: Hash,
            options: &FetchOptions,
        ) -> legion_sync::Result<Box<dyn Read + Send>> {
            struct DropMidway {
                data: Vec<u8>,
                pos: usize,
                fail_at: Option<usize>,
            }
            impl Read for DropMidway {
                fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                    if let Some(limit) = self.fail_at
                        && self.pos >= limit
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionReset,
                            "simulated mid-stream drop",
                        ));
                    }
                    let end = self.fail_at.unwrap_or(self.data.len()).min(self.data.len());
                    if self.pos >= end {
                        return Ok(0);
                    }
                    let n = (end - self.pos).min(buf.len());
                    buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                    self.pos += n;
                    Ok(n)
                }
            }
            let call_num = self.calls.get();
            self.calls.set(call_num + 1);
            let start = (options.offset as usize).min(self.payload.len());
            let fail_at = if call_num == 0 {
                Some(self.fail_first_after.saturating_sub(start))
            } else {
                None
            };
            Ok(Box::new(DropMidway {
                data: self.payload[start..].to_vec(),
                pos: 0,
                fail_at,
            }))
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let mb = Member::new(tmp.path(), "b", &["b", "a"]);
    let mut b = mb.open();
    let payload: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
    let r = rec(
        "large.bin",
        EntryKind::File,
        Some(&payload),
        &[("a", 7, 1)],
        "a",
        1,
    );
    let peer = FlakyRangedPeer {
        id: "a".into(),
        incarnation: 7,
        record: r,
        payload: payload.clone(),
        fail_first_after: 32_768,
        calls: Cell::new(0),
    };

    // 1. Cancellation flag aborts cleanly.
    let cancel = Arc::new(AtomicBool::new(true));
    let cancel_opts = FetchOptions::at_offset(0, Duration::from_secs(5), Some(cancel));
    assert!(matches!(
        b.pull_from_peer_with_options(&peer, 100, &cancel_opts, now()),
        Err(SyncError::Cancelled { .. })
    ));

    // 2. Resumed ranged fetch recovers mid-stream drop and finishes in 2 fetch calls!
    b.pull_from_peer(&peer, now()).unwrap();
    assert_eq!(peer.calls.get(), 2);
    assert_eq!(mb.read("large.bin").unwrap(), payload);
    assert!(b.fs().list_tmp().unwrap().is_empty());

    // Sanity check conflict_copy_path includes hash6.
    let cpath = conflict_copy_path(
        "a/b.txt",
        "a",
        Timestamp::new(1_795_608_000, 0),
        "abcdef",
        None,
    );
    assert_eq!(cpath, "a/b (changed on a 2026-11-25, abcdef).txt");
    let _ = FsErrorKind::NoSpace;
}
