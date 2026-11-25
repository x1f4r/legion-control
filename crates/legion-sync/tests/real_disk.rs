//! Real-disk integration tests using `RealFs` and `RedbIndexStore` on temporary directories,
//! including a crash-injection wrapper that fails at every apply step and verifies recovery.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use legion_sync::{
    BinReason, ConflictChoice, FetchOptions, FolderConfig, FolderMarker, FsCapabilities,
    FsErrorKind, FsMeta, FsWalkResult, IgnoreMatcher, IndexRecord, IndexStore, MemberState, Peer,
    PeerHandshake, ProblemKind, RealFs, RecordsPage, RedbIndexStore, RestoreStrategy,
    SYNC_PROTOCOL_VERSION, ScanOptions, SyncEngine, SyncError, SyncFs, SyncWrite, VersionKey,
    VersionVector, conflict_copy_path,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{EntryKind, Hash, Timestamp};

struct EnginePeer<'a, F: SyncFs, S: IndexStore> {
    engine: &'a SyncEngine<F, S>,
    caller: String,
    member_id: String,
}

impl<'a, F: SyncFs, S: IndexStore> EnginePeer<'a, F, S> {
    fn new(engine: &'a SyncEngine<F, S>, caller: &str) -> Self {
        let member_id = engine.meta().unwrap().local_member;
        Self {
            engine,
            caller: caller.to_string(),
            member_id,
        }
    }
}

impl<F: SyncFs, S: IndexStore> Peer for EnginePeer<'_, F, S> {
    fn member_id(&self) -> &str {
        &self.member_id
    }

    fn handshake(&self) -> legion_sync::Result<PeerHandshake> {
        self.engine.serve_handshake(&self.caller)
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
        self.engine
            .serve_fetch_file_range(&self.caller, path, expected_hash, options.offset)
    }
}

struct CrashInjectingFs<F: SyncFs> {
    inner: F,
    remaining: Cell<Option<usize>>,
    crash_after_op: bool,
    crashed: Cell<bool>,
}

impl<F: SyncFs> CrashInjectingFs<F> {
    fn new(inner: F, fail_at_op: Option<usize>, crash_after_op: bool) -> Self {
        Self {
            inner,
            remaining: Cell::new(fail_at_op),
            crash_after_op,
            crashed: Cell::new(false),
        }
    }

    fn did_crash(&self) -> bool {
        self.crashed.get()
    }

    fn disarm(&self) {
        self.remaining.set(None);
    }

    fn tick(&self, _label: &str) -> legion_sync::Result<bool> {
        if let Some(left) = self.remaining.get() {
            if left == 0 {
                self.remaining.set(None);
                self.crashed.set(true);
                return Ok(true);
            }
            self.remaining.set(Some(left - 1));
        }
        Ok(false)
    }

    fn crash_err(label: &str) -> SyncError {
        SyncError::Fs {
            path: Some(label.to_string()),
            detail: "injected crash fault".to_string(),
            kind: FsErrorKind::Crash,
        }
    }

    fn wrap_mut<T>(
        &self,
        label: &str,
        op: impl FnOnce(&F) -> legion_sync::Result<T>,
    ) -> legion_sync::Result<T> {
        let should_crash = self.tick(label)?;
        if should_crash && !self.crash_after_op {
            return Err(Self::crash_err(label));
        }
        let res = op(&self.inner)?;
        if should_crash && self.crash_after_op {
            return Err(Self::crash_err(label));
        }
        Ok(res)
    }
}

struct CrashWriter {
    inner: Box<dyn SyncWrite>,
    crash_on_sync: bool,
}

impl Write for CrashWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl SyncWrite for CrashWriter {
    fn finish_sync(self: Box<Self>) -> legion_sync::Result<()> {
        if self.crash_on_sync {
            return Err(CrashInjectingFs::<RealFs>::crash_err("finish_sync"));
        }
        self.inner.finish_sync()
    }
}

impl<F: SyncFs> SyncFs for CrashInjectingFs<F> {
    fn capabilities(&self) -> FsCapabilities {
        self.inner.capabilities()
    }

    fn read_marker(&self) -> legion_sync::Result<Option<FolderMarker>> {
        self.inner.read_marker()
    }

    fn write_marker(&self, marker: &FolderMarker) -> legion_sync::Result<()> {
        self.inner.write_marker(marker)
    }

    fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
        self.inner.ensure_internal_dirs()
    }

    fn check_not_nested(&self) -> legion_sync::Result<()> {
        self.inner.check_not_nested()
    }

    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk(ignore)
    }

    fn walk_subtree(
        &self,
        rel_root: &str,
        ignore: &IgnoreMatcher,
    ) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk_subtree(rel_root, ignore)
    }

    fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
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
        self.wrap_mut("create_dir_all_live", |f| f.create_dir_all_live(rel_path))
    }

    fn list_dir_live(&self, rel_path: &str) -> legion_sync::Result<Vec<String>> {
        self.inner.list_dir_live(rel_path)
    }

    fn remove_empty_dir_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        self.wrap_mut("remove_empty_dir_live", |f| {
            f.remove_empty_dir_live(rel_path)
        })
    }

    fn create_symlink_live(&self, rel_path: &str, target: &str) -> legion_sync::Result<()> {
        self.wrap_mut("create_symlink_live", |f| {
            f.create_symlink_live(rel_path, target)
        })
    }

    fn remove_symlink_live(&self, rel_path: &str) -> legion_sync::Result<()> {
        self.wrap_mut("remove_symlink_live", |f| f.remove_symlink_live(rel_path))
    }

    fn set_live_metadata(
        &self,
        rel_path: &str,
        mtime: Timestamp,
        mode: Option<u32>,
    ) -> legion_sync::Result<()> {
        self.wrap_mut("set_live_metadata", |f| {
            f.set_live_metadata(rel_path, mtime, mode)
        })
    }

    fn rename_live_to_live_no_replace(
        &self,
        from_rel: &str,
        to_rel: &str,
    ) -> legion_sync::Result<()> {
        self.wrap_mut("rename_live_to_live_no_replace", |f| {
            f.rename_live_to_live_no_replace(from_rel, to_rel)
        })
    }

    fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        let crash_now = self.tick("create_tmp")?;
        if crash_now && !self.crash_after_op {
            return Err(Self::crash_err("create_tmp"));
        }
        let writer = self.inner.create_tmp(tmp_name)?;
        Ok(Box::new(CrashWriter {
            inner: writer,
            crash_on_sync: crash_now && self.crash_after_op,
        }))
    }

    fn open_tmp_append(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
        self.inner.open_tmp_append(tmp_name)
    }

    fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
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
        self.wrap_mut("commit_tmp_to_live_no_replace", |f| {
            f.commit_tmp_to_live_no_replace(tmp_name, live_rel)
        })
    }

    fn exchange_tmp_with_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<bool> {
        self.wrap_mut("exchange_tmp_with_live", |f| {
            f.exchange_tmp_with_live(tmp_name, live_rel)
        })
    }

    fn replace_live_with_tmp_backup(
        &self,
        tmp_name: &str,
        live_rel: &str,
        backup_tmp: &str,
    ) -> legion_sync::Result<bool> {
        self.wrap_mut("replace_live_with_tmp_backup", |f| {
            f.replace_live_with_tmp_backup(tmp_name, live_rel, backup_tmp)
        })
    }

    fn move_live_to_tmp_no_replace(
        &self,
        live_rel: &str,
        tmp_name: &str,
    ) -> legion_sync::Result<()> {
        self.wrap_mut("move_live_to_tmp_no_replace", |f| {
            f.move_live_to_tmp_no_replace(live_rel, tmp_name)
        })
    }

    fn move_tmp_to_bin_no_replace(
        &self,
        tmp_name: &str,
        bin_data_name: &str,
    ) -> legion_sync::Result<()> {
        self.wrap_mut("move_tmp_to_bin_no_replace", |f| {
            f.move_tmp_to_bin_no_replace(tmp_name, bin_data_name)
        })
    }

    fn move_live_to_bin_no_replace(
        &self,
        live_rel: &str,
        bin_data_name: &str,
    ) -> legion_sync::Result<()> {
        self.wrap_mut("move_live_to_bin_no_replace", |f| {
            f.move_live_to_bin_no_replace(live_rel, bin_data_name)
        })
    }

    fn write_bin_meta(&self, meta_name: &str, bytes: &[u8]) -> legion_sync::Result<()> {
        self.wrap_mut("write_bin_meta", |f| f.write_bin_meta(meta_name, bytes))
    }

    fn read_bin_meta(&self, meta_name: &str) -> legion_sync::Result<Option<Vec<u8>>> {
        self.inner.read_bin_meta(meta_name)
    }

    fn stat_bin(&self, bin_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_bin(bin_name)
    }

    fn hash_bin(&self, bin_name: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_bin(bin_name)
    }

    fn copy_bin_to_tmp(&self, bin_data_name: &str, tmp_name: &str) -> legion_sync::Result<()> {
        self.wrap_mut("copy_bin_to_tmp", |f| {
            f.copy_bin_to_tmp(bin_data_name, tmp_name)
        })
    }

    fn remove_bin_file(&self, bin_name: &str) -> legion_sync::Result<()> {
        self.inner.remove_bin_file(bin_name)
    }

    fn list_bin(&self) -> legion_sync::Result<Vec<String>> {
        self.inner.list_bin()
    }
}

fn open_real_engine(
    root: &Path,
    db_path: &Path,
    member: &str,
    members: &[&str],
    ignore: Vec<String>,
    caps: Option<FsCapabilities>,
) -> SyncEngine<RealFs, RedbIndexStore> {
    let mut cfg = FolderConfig::new("docs", member, members.iter().map(|s| (*s).to_string()));
    cfg.ignore = ignore;
    let fs = match caps {
        Some(c) => RealFs::open_with_capabilities(root, c).unwrap(),
        None => RealFs::open(root).unwrap(),
    };
    let store = RedbIndexStore::open(db_path).unwrap();
    SyncEngine::open(cfg, fs, store).unwrap()
}

#[test]
fn real_disk_scan_skips_unchanged_and_honours_ignores() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("folder");
    let db = tmp.path().join("index.redb");
    let now = Timestamp::new(1_795_608_000, 0);

    let mut engine = open_real_engine(
        &root,
        &db,
        "tower",
        &["tower", "laptop"],
        vec!["*.log".to_string(), "build/".to_string()],
        None,
    );

    fs::create_dir_all(root.join("sub")).unwrap();
    fs::create_dir_all(root.join("build")).unwrap();
    fs::write(root.join("hello.txt"), b"hello world").unwrap();
    fs::write(root.join("sub/data.bin"), b"0123456789").unwrap();
    fs::write(root.join(".DS_Store"), b"junk").unwrap();
    fs::write(root.join("sub/Thumbs.db"), b"junk").unwrap();
    fs::write(root.join("desktop.ini"), b"junk").unwrap();
    fs::write(root.join("partial.bin.legion-part"), b"half").unwrap();
    fs::write(root.join(".legion-sync-9.tmp"), b"tmp").unwrap();
    fs::write(root.join("debug.log"), b"ignored glob").unwrap();
    fs::write(root.join("build/artifact.o"), b"ignored dir").unwrap();

    let r1 = engine.scan(now).unwrap();
    assert_eq!(r1.created, 3);
    assert_eq!(r1.hashed, 2);
    assert_eq!(r1.unchanged, 0);

    let r2 = engine.scan(now).unwrap();
    assert_eq!(r2.created, 0);
    assert_eq!(r2.modified, 0);
    assert_eq!(r2.deleted, 0);
    assert_eq!(r2.unchanged, 3);
    assert_eq!(r2.hashed, 0);

    fs::rename(root.join("hello.txt"), root.join("renamed.txt")).unwrap();
    let r3 = engine.scan(now).unwrap();
    assert_eq!(r3.deleted, 1);
    assert_eq!(r3.created, 1);
    assert_eq!(r3.hashed, 1);
    assert_eq!(r3.unchanged, 2);
}

#[test]
fn real_disk_holds_back_still_changing_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("folder");
    let db = tmp.path().join("index.redb");
    let now = Timestamp::new(1_795_608_000, 0);

    let mut engine = open_real_engine(&root, &db, "tower", &["tower", "pi"], vec![], None);
    fs::write(root.join("writing.bin"), b"part1").unwrap();

    let opts = ScanOptions {
        require_second_look: true,
        min_stable_age: Duration::ZERO,
        ..ScanOptions::default()
    };
    let r1 = engine.scan_with_options(now, opts).unwrap();
    assert_eq!(r1.created, 0);
    assert_eq!(r1.hashed, 0);
    assert_eq!(r1.held_back_unstable, vec!["writing.bin".to_string()]);

    fs::write(root.join("writing.bin"), b"part1-and-part2").unwrap();
    let r2 = engine.scan_with_options(now, opts).unwrap();
    assert_eq!(r2.created, 0);
    assert_eq!(r2.held_back_unstable, vec!["writing.bin".to_string()]);

    let r3 = engine.scan_with_options(now, opts).unwrap();
    assert_eq!(r3.created, 1);
    assert_eq!(r3.hashed, 1);
    assert!(r3.held_back_unstable.is_empty());
}

#[test]
fn real_disk_sync_bin_restore_expiry_and_conflict_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);

    let mut tower = open_real_engine(
        &tmp.path().join("tower"),
        &tmp.path().join("tower.redb"),
        "tower",
        &["tower", "laptop"],
        vec![],
        None,
    );
    let mut laptop = open_real_engine(
        &tmp.path().join("laptop"),
        &tmp.path().join("laptop.redb"),
        "laptop",
        &["tower", "laptop"],
        vec![],
        None,
    );

    fs::write(tmp.path().join("tower/notes.txt"), b"v1").unwrap();
    tower
        .fs()
        .set_live_metadata("notes.txt", t0, Some(0o644))
        .unwrap();
    tower.scan(t0).unwrap();

    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), t0)
        .unwrap();
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"v1"
    );

    let t1 = Timestamp::new(t0.secs + 60, 0);
    let t2 = Timestamp::new(t0.secs + 120, 0);
    fs::write(tmp.path().join("tower/notes.txt"), b"tower-edit").unwrap();
    tower
        .fs()
        .set_live_metadata("notes.txt", t1, Some(0o644))
        .unwrap();
    tower.scan(t1).unwrap();

    fs::write(tmp.path().join("laptop/notes.txt"), b"laptop-edit").unwrap();
    laptop
        .fs()
        .set_live_metadata("notes.txt", t2, Some(0o644))
        .unwrap();
    laptop.scan(t2).unwrap();

    tower
        .pull_from_peer(&EnginePeer::new(&laptop, "tower"), t2)
        .unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), t2)
        .unwrap();

    let tower_hash6 = &hash_bytes(b"tower-edit").to_hex()[..6];
    let expected_conflict = conflict_copy_path("notes.txt", "tower", t1, tower_hash6, None);
    assert_eq!(
        fs::read(tmp.path().join("tower/notes.txt")).unwrap(),
        b"laptop-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"laptop-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("tower").join(&expected_conflict)).unwrap(),
        b"tower-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("laptop").join(&expected_conflict)).unwrap(),
        b"tower-edit"
    );
    assert_eq!(tower.status().unwrap().conflicts.len(), 1);
    assert_eq!(laptop.status().unwrap().conflicts.len(), 1);

    let t3 = Timestamp::new(t0.secs + 180, 0);
    laptop
        .resolve_conflict(&expected_conflict, ConflictChoice::KeepConflict, t3)
        .unwrap();
    tower
        .pull_from_peer(&EnginePeer::new(&laptop, "tower"), t3)
        .unwrap();

    assert_eq!(
        fs::read(tmp.path().join("tower/notes.txt")).unwrap(),
        b"tower-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"tower-edit"
    );
    assert!(!tmp.path().join("tower").join(&expected_conflict).exists());
    assert!(!tmp.path().join("laptop").join(&expected_conflict).exists());
    assert!(tower.status().unwrap().conflicts.is_empty());
    assert!(laptop.status().unwrap().conflicts.is_empty());

    let t4 = Timestamp::new(t0.secs + 240, 0);
    fs::remove_file(tmp.path().join("tower/notes.txt")).unwrap();
    tower.scan(t4).unwrap();
    laptop
        .pull_from_peer(&EnginePeer::new(&tower, "laptop"), t4)
        .unwrap();

    assert!(!tmp.path().join("laptop/notes.txt").exists());
    let bin = laptop.list_bin().unwrap();
    let deleted_entry = bin
        .iter()
        .find(|e| e.original_path == "notes.txt" && e.reason == BinReason::DeletedByPeer)
        .unwrap()
        .clone();
    assert_eq!(deleted_entry.hash, Some(hash_bytes(b"tower-edit")));

    let t5 = Timestamp::new(t0.secs + 300, 0);
    let restored = laptop
        .restore_from_bin(&deleted_entry.id, None, RestoreStrategy::FailIfExists, t5)
        .unwrap();
    assert_eq!(restored, "notes.txt");
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"tower-edit"
    );

    tower
        .pull_from_peer(&EnginePeer::new(&laptop, "tower"), t5)
        .unwrap();
    assert_eq!(
        fs::read(tmp.path().join("tower/notes.txt")).unwrap(),
        b"tower-edit"
    );

    let after_31d = Timestamp::new(t0.secs + 31 * 86_400, 0);
    let expired = laptop.expire_bin(after_31d).unwrap();
    assert!(!expired.is_empty());
    assert!(laptop.list_bin().unwrap().is_empty());
}

#[test]
fn real_disk_crash_injection_at_every_apply_step_converges_without_loss() {
    let t0 = Timestamp::new(1_795_608_000, 0);
    let t1 = Timestamp::new(t0.secs + 60, 0);

    for crash_after in [false, true] {
        for op_idx in 0..10 {
            let tmp = tempfile::tempdir().unwrap();
            let src_root = tmp.path().join("src");
            let dst_root = tmp.path().join("dst");
            let src_db = tmp.path().join("src.redb");
            let dst_db = tmp.path().join("dst.redb");

            let mut src =
                open_real_engine(&src_root, &src_db, "tower", &["tower", "pi"], vec![], None);
            let mut dst_init =
                open_real_engine(&dst_root, &dst_db, "pi", &["tower", "pi"], vec![], None);

            fs::write(src_root.join("replace.txt"), b"old-replace").unwrap();
            fs::write(src_root.join("delete.txt"), b"to-be-deleted").unwrap();
            fs::write(src_root.join("conflict.txt"), b"base").unwrap();
            for p in ["replace.txt", "delete.txt", "conflict.txt"] {
                src.fs().set_live_metadata(p, t0, Some(0o644)).unwrap();
            }
            src.scan(t0).unwrap();
            dst_init
                .pull_from_peer(&EnginePeer::new(&src, "pi"), t0)
                .unwrap();
            drop(dst_init);

            fs::write(src_root.join("replace.txt"), b"new-replace-bytes").unwrap();
            src.fs()
                .set_live_metadata("replace.txt", t1, Some(0o644))
                .unwrap();
            fs::remove_file(src_root.join("delete.txt")).unwrap();
            fs::write(src_root.join("conflict.txt"), b"tower-wins").unwrap();
            src.fs()
                .set_live_metadata("conflict.txt", Timestamp::new(t1.secs + 30, 0), Some(0o644))
                .unwrap();
            src.scan(t1).unwrap();

            {
                let mut dst_edit =
                    open_real_engine(&dst_root, &dst_db, "pi", &["tower", "pi"], vec![], None);
                fs::write(dst_root.join("conflict.txt"), b"pi-loses").unwrap();
                dst_edit
                    .fs()
                    .set_live_metadata("conflict.txt", t1, Some(0o644))
                    .unwrap();
                dst_edit.scan(t1).unwrap();
            }

            let crash_fs =
                CrashInjectingFs::new(RealFs::open(&dst_root).unwrap(), Some(op_idx), crash_after);
            let cfg = FolderConfig::new("docs", "pi", ["tower".into(), "pi".into()]);
            let store = RedbIndexStore::open(&dst_db).unwrap();
            let mut dst_crash = SyncEngine::open(cfg, crash_fs, store).unwrap();

            let res = dst_crash.pull_from_peer(&EnginePeer::new(&src, "pi"), t1);
            let did_crash = dst_crash.fs().did_crash();
            if did_crash {
                assert!(
                    res.as_ref().is_err_and(SyncError::is_crash),
                    "expected crash error at op {op_idx} (after={crash_after}), got {res:?}"
                );
            } else {
                res.unwrap();
            }
            dst_crash.fs().disarm();
            drop(dst_crash);

            let mut dst_recovered =
                open_real_engine(&dst_root, &dst_db, "pi", &["tower", "pi"], vec![], None);
            dst_recovered.scan(t1).unwrap();
            dst_recovered
                .pull_from_peer(&EnginePeer::new(&src, "pi"), t1)
                .unwrap();
            src.pull_from_peer(&EnginePeer::new(&dst_recovered, "tower"), t1)
                .unwrap();

            let pi_hash6 = &hash_bytes(b"pi-loses").to_hex()[..6];
            let conflict_name = conflict_copy_path("conflict.txt", "pi", t1, pi_hash6, None);
            assert_eq!(
                fs::read(dst_root.join("replace.txt")).unwrap(),
                b"new-replace-bytes"
            );
            assert!(!dst_root.join("delete.txt").exists());
            assert_eq!(
                fs::read(dst_root.join("conflict.txt")).unwrap(),
                b"tower-wins"
            );
            assert_eq!(
                fs::read(dst_root.join(&conflict_name)).unwrap(),
                b"pi-loses"
            );
            assert_eq!(
                fs::read(src_root.join(&conflict_name)).unwrap(),
                b"pi-loses"
            );

            assert!(dst_recovered.fs().list_tmp().unwrap().is_empty());
            let bin = dst_recovered.list_bin().unwrap();
            assert!(
                bin.iter()
                    .any(|b| b.hash == Some(hash_bytes(b"old-replace"))),
                "old-replace missing from bin at op_idx={op_idx}, after={crash_after}"
            );
            assert!(
                bin.iter()
                    .any(|b| b.hash == Some(hash_bytes(b"to-be-deleted"))),
                "to-be-deleted missing from bin at op_idx={op_idx}, after={crash_after}"
            );
        }
    }
}

#[test]
fn real_disk_local_edit_between_scan_and_apply_is_never_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);
    let t1 = Timestamp::new(t0.secs + 60, 0);

    let mut tower = open_real_engine(
        &tmp.path().join("tower"),
        &tmp.path().join("tower.redb"),
        "tower",
        &["tower", "pi"],
        vec![],
        None,
    );
    let mut pi = open_real_engine(
        &tmp.path().join("pi"),
        &tmp.path().join("pi.redb"),
        "pi",
        &["tower", "pi"],
        vec![],
        None,
    );

    fs::write(tmp.path().join("tower/doc.txt"), b"v1").unwrap();
    tower
        .fs()
        .set_live_metadata("doc.txt", t0, Some(0o644))
        .unwrap();
    tower.scan(t0).unwrap();
    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), t0)
        .unwrap();

    fs::remove_file(tmp.path().join("tower/doc.txt")).unwrap();
    tower.scan(t1).unwrap();

    fs::write(tmp.path().join("pi/doc.txt"), b"v2").unwrap();
    pi.fs()
        .set_live_metadata("doc.txt", t0, Some(0o644))
        .unwrap();

    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), t1)
        .unwrap();
    assert_eq!(fs::read(tmp.path().join("pi/doc.txt")).unwrap(), b"v2");

    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), t1)
        .unwrap();
    assert_eq!(fs::read(tmp.path().join("tower/doc.txt")).unwrap(), b"v2");
}

fn list_dir_names(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

struct SyntheticLinuxPeer {
    member_id: String,
    incarnation: u64,
    records: Vec<IndexRecord>,
    files: BTreeMap<String, Vec<u8>>,
    seq: u64,
    clock: u64,
}

impl SyntheticLinuxPeer {
    fn new(member_id: &str) -> Self {
        Self {
            member_id: member_id.to_string(),
            incarnation: 42,
            records: Vec::new(),
            files: BTreeMap::new(),
            seq: 0,
            clock: 0,
        }
    }

    fn vkey(&self) -> VersionKey {
        VersionKey::new(self.member_id.clone(), self.incarnation)
    }

    fn put_file(&mut self, path: &str, bytes: &[u8], mtime: Timestamp) {
        self.seq += 1;
        self.clock += 1;
        let hash = hash_bytes(bytes);
        self.files.insert(path.to_string(), bytes.to_vec());
        self.records.push(IndexRecord {
            path: path.to_string(),
            kind: EntryKind::File,
            size: bytes.len() as u64,
            mtime,
            disk_mtime: Some(mtime),
            disk_change_id: None,
            mode: Some(0o644),
            hash: Some(hash),
            link_target: None,
            deleted: false,
            version: VersionVector::singleton(self.vkey(), self.clock),
            modified_by: self.member_id.clone(),
            seq: self.seq,
            recent_hashes: Vec::new(),
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        });
    }

    fn put_dir(&mut self, path: &str, mtime: Timestamp) {
        self.seq += 1;
        self.clock += 1;
        self.records.push(IndexRecord {
            path: path.to_string(),
            kind: EntryKind::Dir,
            size: 0,
            mtime,
            disk_mtime: Some(mtime),
            disk_change_id: None,
            mode: Some(0o755),
            hash: None,
            link_target: None,
            deleted: false,
            version: VersionVector::singleton(self.vkey(), self.clock),
            modified_by: self.member_id.clone(),
            seq: self.seq,
            recent_hashes: Vec::new(),
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        });
    }

    fn put_symlink(&mut self, path: &str, target: &str, mtime: Timestamp) {
        self.seq += 1;
        self.clock += 1;
        self.records.push(IndexRecord {
            path: path.to_string(),
            kind: EntryKind::Symlink,
            size: 0,
            mtime,
            disk_mtime: Some(mtime),
            disk_change_id: None,
            mode: None,
            hash: None,
            link_target: Some(target.to_string()),
            deleted: false,
            version: VersionVector::singleton(self.vkey(), self.clock),
            modified_by: self.member_id.clone(),
            seq: self.seq,
            recent_hashes: Vec::new(),
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        });
    }

    fn delete_path(&mut self, path: &str, mtime: Timestamp) {
        let prev = self
            .records
            .iter()
            .rev()
            .find(|r| r.path == path)
            .cloned()
            .unwrap();
        self.seq += 1;
        self.clock += 1;
        let mut version = prev.version;
        version.bump_above(self.vkey(), self.clock);
        self.files.remove(path);
        let mut recent = Vec::new();
        if let Some(h) = prev.hash {
            recent.push(h);
        }
        self.records.push(IndexRecord {
            path: path.to_string(),
            kind: prev.kind,
            size: 0,
            mtime,
            disk_mtime: None,
            disk_change_id: None,
            mode: None,
            hash: None,
            link_target: None,
            deleted: true,
            version,
            modified_by: self.member_id.clone(),
            seq: self.seq,
            recent_hashes: recent,
            from_join: false,
            conflict_of: None,
            conflict_dismissed: false,
        });
    }
}

impl Peer for SyntheticLinuxPeer {
    fn member_id(&self) -> &str {
        &self.member_id
    }

    fn handshake(&self) -> legion_sync::Result<PeerHandshake> {
        Ok(PeerHandshake {
            folder_id: "docs".to_string(),
            member_id: self.member_id.clone(),
            incarnation: self.incarnation,
            snapshot_seq: self.seq,
            protocol_version: SYNC_PROTOCOL_VERSION,
        })
    }

    fn records_since(
        &self,
        since_incarnation: u64,
        since_seq: u64,
        limit: usize,
        _cancel: Option<&AtomicBool>,
    ) -> legion_sync::Result<RecordsPage> {
        let eff = if since_incarnation == self.incarnation {
            since_seq
        } else {
            0
        };
        let mut latest_by_path: BTreeMap<String, IndexRecord> = BTreeMap::new();
        for rec in &self.records {
            latest_by_path.insert(rec.path.clone(), rec.clone());
        }
        let mut records: Vec<IndexRecord> = latest_by_path
            .into_values()
            .filter(|r| r.seq > eff)
            .collect();
        records.sort_by_key(|r| r.seq);
        let has_more = records.len() > limit;
        records.truncate(limit);
        let head_fp = self.records.last().map(IndexRecord::head_fingerprint);
        Ok(RecordsPage {
            incarnation: self.incarnation,
            records,
            snapshot_seq: self.seq,
            has_more,
            caller_acked: None,
            caller_acked_fingerprint: None,
            head_fingerprint: head_fp,
        })
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
        options: &FetchOptions,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        let bytes = self
            .files
            .get(path)
            .ok_or_else(|| SyncError::Fs {
                path: Some(path.to_string()),
                detail: "not found".to_string(),
                kind: FsErrorKind::NotFound,
            })?
            .clone();
        let actual = hash_bytes(&bytes);
        if actual != expected_hash {
            return Err(SyncError::HashMismatch {
                path: path.to_string(),
                expected: expected_hash,
                actual,
            });
        }
        let start = (options.offset as usize).min(bytes.len());
        Ok(Box::new(Cursor::new(bytes[start..].to_vec())))
    }
}

#[test]
fn real_disk_platform_problems_status_and_security_refusals() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);

    let host_case_insensitive = RealFs::open(tmp.path().join("linux"))
        .unwrap()
        .capabilities()
        .case_insensitive;

    let mut linux = open_real_engine(
        &tmp.path().join("linux"),
        &tmp.path().join("linux.redb"),
        "tower",
        &["tower", "winbox"],
        vec![],
        Some(FsCapabilities::unix()),
    );
    let mut winbox = open_real_engine(
        &tmp.path().join("winbox"),
        &tmp.path().join("winbox.redb"),
        "winbox",
        &["tower", "winbox"],
        vec![],
        Some(FsCapabilities::windows()),
    );

    assert_eq!(winbox.status().unwrap().state, MemberState::UpToDate);

    winbox.set_paused(true).unwrap();
    assert_eq!(winbox.status().unwrap().state, MemberState::Paused);
    assert!(matches!(
        winbox.pull_from_peer(&EnginePeer::new(&linux, "winbox"), t0),
        Err(SyncError::Paused { .. })
    ));
    winbox.set_paused(false).unwrap();

    winbox.mark_peer_unreachable("tower");
    assert_eq!(
        winbox.status().unwrap().state,
        MemberState::WaitingForMember {
            members: vec!["tower".to_string()]
        }
    );

    assert!(matches!(
        linux.serve_records_since("intruder", 0, 0, 100),
        Err(SyncError::Denied { .. })
    ));
    assert!(matches!(
        linux.serve_fetch_file("intruder", "ok.txt", hash_bytes(b"ok")),
        Err(SyncError::Denied { .. })
    ));

    fs::write(tmp.path().join("linux/ok.txt"), b"ok").unwrap();
    fs::write(tmp.path().join("linux/README.md"), b"upper").unwrap();
    let mut expected_problems: BTreeMap<String, ProblemKind> = BTreeMap::new();
    if !host_case_insensitive {
        fs::write(tmp.path().join("linux/readme.md"), b"lower").unwrap();
        expected_problems.insert("readme.md".to_string(), ProblemKind::CaseClash);
    }
    #[cfg(unix)]
    {
        fs::write(tmp.path().join("linux/CON.txt"), b"reserved").unwrap();
        expected_problems.insert("CON.txt".to_string(), ProblemKind::ReservedWindowsName);
        std::os::unix::fs::symlink("ok.txt", tmp.path().join("linux/sym")).unwrap();
        expected_problems.insert("sym".to_string(), ProblemKind::UnsupportedSymlink);
    }
    linux.scan(t0).unwrap();

    assert!(matches!(
        linux.serve_fetch_file("winbox", ".legion-sync/bin/x", hash_bytes(b"x")),
        Err(SyncError::InvalidPath { .. })
    ));
    assert!(matches!(
        linux.serve_fetch_file("winbox", "../escape", hash_bytes(b"x")),
        Err(SyncError::InvalidPath { .. })
    ));

    let pull = winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t0)
        .unwrap();
    assert_eq!(pull.held_back, expected_problems.len());
    assert_eq!(fs::read(tmp.path().join("winbox/ok.txt")).unwrap(), b"ok");
    let winbox_entries = list_dir_names(&tmp.path().join("winbox"));
    assert!(winbox_entries.contains("ok.txt"));
    assert!(winbox_entries.contains("README.md"));
    assert!(!winbox_entries.contains("CON.txt"));
    assert!(!winbox_entries.contains("readme.md"));
    assert!(!winbox_entries.contains("sym"));

    let st = winbox.status().unwrap();
    if expected_problems.is_empty() {
        assert_eq!(st.state, MemberState::UpToDate);
    } else {
        match st.state {
            MemberState::Problem { sentence, problems } => {
                assert!(!sentence.is_empty());
                let actual: BTreeMap<String, ProblemKind> =
                    problems.into_iter().map(|p| (p.path, p.kind)).collect();
                assert_eq!(actual, expected_problems);
            }
            other => panic!("expected Problem state, got {other:?}"),
        }

        let t1 = Timestamp::new(t0.secs + 60, 0);
        if !host_case_insensitive {
            fs::remove_file(tmp.path().join("linux/README.md")).unwrap();
        }
        #[cfg(unix)]
        {
            fs::remove_file(tmp.path().join("linux/CON.txt")).unwrap();
            fs::remove_file(tmp.path().join("linux/sym")).unwrap();
        }
        linux.scan(t1).unwrap();

        winbox
            .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t1)
            .unwrap();
        assert_eq!(winbox.status().unwrap().state, MemberState::UpToDate);
        if !host_case_insensitive {
            assert_eq!(
                fs::read(tmp.path().join("winbox/readme.md")).unwrap(),
                b"lower"
            );
        }
    }
}

#[test]
fn real_disk_windows_target_holds_back_unrepresentable_peer_records() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);

    let mut winbox = open_real_engine(
        &tmp.path().join("winbox"),
        &tmp.path().join("winbox.redb"),
        "winbox",
        &["tower", "winbox"],
        vec![],
        Some(FsCapabilities::windows()),
    );

    let mut peer = SyntheticLinuxPeer::new("tower");
    let over_long_path = format!("{}/{}/long-name.txt", "d".repeat(115), "e".repeat(115));

    peer.put_file("ok.txt", b"ok", t0);
    peer.put_dir("Dir", t0);
    peer.put_file("Dir/a.txt", b"dir-a", t0);
    peer.put_file("README.md", b"upper", t0);
    peer.put_file("readme.md", b"lower", t0);
    peer.put_file("dir/b.txt", b"dir-b", t0);
    peer.put_file("CON.txt", b"reserved", t0);
    peer.put_file("ab:cd.txt", b"colon", t0);
    peer.put_file("trailing.", b"dot", t0);
    peer.put_file("trailing ", b"space", t0);
    peer.put_symlink("sym", "ok.txt", t0);
    peer.put_file(&over_long_path, b"long-path", t0);

    let pull = winbox.pull_from_peer(&peer, t0).unwrap();
    assert_eq!(pull.held_back, 8);

    let entries = list_dir_names(&tmp.path().join("winbox"));
    assert_eq!(
        entries,
        BTreeSet::from([
            ".legion-sync".to_string(),
            "Dir".to_string(),
            "README.md".to_string(),
            "ok.txt".to_string(),
        ])
    );

    let expected_by_path: BTreeMap<String, ProblemKind> = BTreeMap::from([
        ("CON.txt".to_string(), ProblemKind::ReservedWindowsName),
        ("ab:cd.txt".to_string(), ProblemKind::ForbiddenWindowsChar),
        ("dir/b.txt".to_string(), ProblemKind::CaseClash),
        (over_long_path.clone(), ProblemKind::PathTooLong),
        ("readme.md".to_string(), ProblemKind::CaseClash),
        ("sym".to_string(), ProblemKind::UnsupportedSymlink),
        ("trailing ".to_string(), ProblemKind::TrailingDotOrSpace),
        ("trailing.".to_string(), ProblemKind::TrailingDotOrSpace),
    ]);
    match winbox.status().unwrap().state {
        MemberState::Problem { sentence, problems } => {
            assert!(!sentence.is_empty());
            let actual_by_path: BTreeMap<String, ProblemKind> =
                problems.into_iter().map(|p| (p.path, p.kind)).collect();
            assert_eq!(actual_by_path, expected_by_path);
        }
        other => panic!("expected Problem state, got {other:?}"),
    }

    let t1 = Timestamp::new(t0.secs + 60, 0);
    peer.delete_path("README.md", t1);
    peer.delete_path("Dir/a.txt", t1);
    peer.delete_path("Dir", t1);
    peer.delete_path("CON.txt", t1);
    peer.delete_path("ab:cd.txt", t1);
    peer.delete_path("trailing.", t1);
    peer.delete_path("trailing ", t1);
    peer.delete_path("sym", t1);
    peer.delete_path(&over_long_path, t1);

    let pull2 = winbox.pull_from_peer(&peer, t1).unwrap();
    assert_eq!(pull2.held_back, 0);
    assert_eq!(pull2.held_back_resolved, 2);
    assert_eq!(winbox.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(
        fs::read(tmp.path().join("winbox/readme.md")).unwrap(),
        b"lower"
    );
    assert_eq!(
        fs::read(tmp.path().join("winbox/dir/b.txt")).unwrap(),
        b"dir-b"
    );
}

#[cfg(unix)]
#[test]
fn real_disk_syncs_symlinks_between_unix_members_and_refuses_symlink_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);

    let mut a = open_real_engine(
        &tmp.path().join("a"),
        &tmp.path().join("a.redb"),
        "alpha",
        &["alpha", "beta"],
        vec![],
        None,
    );
    let mut b = open_real_engine(
        &tmp.path().join("b"),
        &tmp.path().join("b.redb"),
        "beta",
        &["alpha", "beta"],
        vec![],
        None,
    );

    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), tmp.path().join("a/link_dir")).unwrap();
    a.scan(t0).unwrap();

    assert!(
        a.store()
            .get_record("link_dir/secret.txt")
            .unwrap()
            .is_none()
    );
    let rec = a.store().get_record("link_dir").unwrap().unwrap();
    assert_eq!(rec.kind, EntryKind::Symlink);

    b.pull_from_peer(&EnginePeer::new(&a, "beta"), t0).unwrap();
    let meta = fs::symlink_metadata(tmp.path().join("b/link_dir")).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        fs::read_link(tmp.path().join("b/link_dir")).unwrap(),
        outside.path()
    );

    assert!(matches!(
        b.fs().stat_live("link_dir/secret.txt"),
        Err(SyncError::InvalidPath { .. })
    ));
}

#[test]
fn real_disk_status_reports_syncing_files_and_bytes_when_deferred() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);

    let mut src = open_real_engine(
        &tmp.path().join("src"),
        &tmp.path().join("src.redb"),
        "tower",
        &["tower", "pi"],
        vec![],
        None,
    );
    let mut dst = open_real_engine(
        &tmp.path().join("dst"),
        &tmp.path().join("dst.redb"),
        "pi",
        &["tower", "pi"],
        vec![],
        None,
    );

    let payload = b"twelve bytes";
    fs::write(tmp.path().join("src/moving.bin"), payload).unwrap();
    src.scan(t0).unwrap();

    fs::write(tmp.path().join("src/moving.bin"), b"changed before fetch!!").unwrap();
    let pull = dst
        .pull_from_peer(&EnginePeer::new(&src, "pi"), t0)
        .unwrap();
    assert_eq!(pull.deferred, 1);
    let status = dst.status().unwrap();
    assert_eq!(status.deferred.len(), 1);
    assert_eq!(
        status.state,
        MemberState::Syncing {
            files_left: 1,
            bytes_left: 0,
        }
    );

    let t1 = Timestamp::new(t0.secs + 10, 0);
    src.scan(t1).unwrap();
    dst.pull_from_peer(&EnginePeer::new(&src, "pi"), t1)
        .unwrap();
    assert_eq!(dst.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(
        fs::read(tmp.path().join("dst/moving.bin")).unwrap(),
        b"changed before fetch!!"
    );
}

#[test]
fn real_disk_holds_back_file_mutating_during_scan_hash() {
    struct MutateDuringHashFs {
        inner: RealFs,
        target: String,
        armed: Cell<bool>,
    }

    impl SyncFs for MutateDuringHashFs {
        fn capabilities(&self) -> FsCapabilities {
            self.inner.capabilities()
        }
        fn read_marker(&self) -> legion_sync::Result<Option<FolderMarker>> {
            self.inner.read_marker()
        }
        fn write_marker(&self, marker: &FolderMarker) -> legion_sync::Result<()> {
            self.inner.write_marker(marker)
        }
        fn ensure_internal_dirs(&self) -> legion_sync::Result<()> {
            self.inner.ensure_internal_dirs()
        }
        fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
            self.inner.walk(ignore)
        }
        fn walk_subtree(
            &self,
            rel_root: &str,
            ignore: &IgnoreMatcher,
        ) -> legion_sync::Result<FsWalkResult> {
            self.inner.walk_subtree(rel_root, ignore)
        }
        fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
            self.inner.stat_live(rel_path)
        }
        fn hash_live(&self, rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
            let res = self.inner.hash_live(rel_path)?;
            if rel_path == self.target && self.armed.replace(false) {
                let mut f = fs::OpenOptions::new()
                    .append(true)
                    .open(self.inner.root().join(rel_path))
                    .unwrap();
                f.write_all(b"-more-bytes").unwrap();
            }
            Ok(res)
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
        fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
            self.inner.create_tmp(tmp_name)
        }
        fn open_tmp_append(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
            self.inner.open_tmp_append(tmp_name)
        }
        fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
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
        fn stat_bin(&self, bin_name: &str) -> legion_sync::Result<Option<FsMeta>> {
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
    let root = tmp.path().join("folder");
    let db = tmp.path().join("index.redb");
    let now = Timestamp::new(1_795_608_000, 0);

    fs::create_dir_all(&root).unwrap();
    let real = RealFs::open(&root).unwrap();
    fs::write(root.join("growing.bin"), b"initial").unwrap();
    let fs_wrap = MutateDuringHashFs {
        inner: real,
        target: "growing.bin".to_string(),
        armed: Cell::new(true),
    };
    let cfg = FolderConfig::new("docs", "tower", ["tower".into(), "pi".into()]);
    let store = RedbIndexStore::open(&db).unwrap();
    let mut engine = SyncEngine::open(cfg, fs_wrap, store).unwrap();

    let r1 = engine.scan(now).unwrap();
    assert_eq!(r1.created, 0);
    assert_eq!(r1.held_back_unstable, vec!["growing.bin".to_string()]);
    assert!(engine.store().get_record("growing.bin").unwrap().is_none());

    let r2 = engine.scan(now).unwrap();
    assert_eq!(r2.created, 1);
    assert!(r2.held_back_unstable.is_empty());
    let rec = engine.store().get_record("growing.bin").unwrap().unwrap();
    assert_eq!(rec.hash, Some(hash_bytes(b"initial-more-bytes")));
}
