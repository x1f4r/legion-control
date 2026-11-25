//! Real-disk integration tests using `RealFs` and `RedbIndexStore` on temporary directories,
//! including a crash-injection wrapper that fails at every apply step and verifies recovery.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use legion_sync::{
    BinReason, ConflictChoice, FolderConfig, FsCapabilities, FsErrorKind, FsMeta, FsWalkResult,
    IgnoreMatcher, IndexStore, MemberState, Peer, ProblemKind, RealFs, RecordsPage, RedbIndexStore,
    RestoreStrategy, ScanOptions, SyncEngine, SyncError, SyncFs, SyncWrite,
};
use legion_xfer::hash::hash_bytes;
use legion_xfer::{Hash, Timestamp};

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

    fn records_since(&self, since_seq: u64, limit: usize) -> legion_sync::Result<RecordsPage> {
        self.engine
            .serve_records_since(&self.caller, since_seq, limit)
    }

    fn fetch_file(
        &self,
        path: &str,
        expected_hash: Hash,
    ) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.engine
            .serve_fetch_file(&self.caller, path, expected_hash)
    }
}

/// Wrapper around any [`SyncFs`] that injects a simulated crash after a configurable number of
/// mutating operations (either before or after the underlying call runs).
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

    fn tick(&self, label: &str) -> legion_sync::Result<bool> {
        if let Some(left) = self.remaining.get() {
            if left == 0 {
                self.remaining.set(None);
                self.crashed.set(true);
                return Ok(true);
            }
            self.remaining.set(Some(left - 1));
        }
        let _ = label;
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

    fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
        self.inner.walk(ignore)
    }

    fn stat_live(&self, rel_path: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_live(rel_path)
    }

    fn hash_live(&self, rel_path: &str) -> legion_sync::Result<(Hash, u64)> {
        self.inner.hash_live(rel_path)
    }

    fn open_live_read(&self, rel_path: &str) -> legion_sync::Result<Box<dyn Read + Send>> {
        self.inner.open_live_read(rel_path)
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

    fn rename_live_to_live(&self, from_rel: &str, to_rel: &str) -> legion_sync::Result<()> {
        self.wrap_mut("rename_live_to_live", |f| {
            f.rename_live_to_live(from_rel, to_rel)
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

    fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
        self.inner.stat_tmp(tmp_name)
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

    fn commit_tmp_to_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<()> {
        self.wrap_mut("commit_tmp_to_live", |f| {
            f.commit_tmp_to_live(tmp_name, live_rel)
        })
    }

    fn move_live_to_bin(&self, live_rel: &str, bin_data_name: &str) -> legion_sync::Result<()> {
        self.wrap_mut("move_live_to_bin", |f| {
            f.move_live_to_bin(live_rel, bin_data_name)
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
    // Ignored files (fixed + custom)
    fs::write(root.join(".DS_Store"), b"junk").unwrap();
    fs::write(root.join("sub/Thumbs.db"), b"junk").unwrap();
    fs::write(root.join("desktop.ini"), b"junk").unwrap();
    fs::write(root.join("partial.bin.legion-part"), b"half").unwrap();
    fs::write(root.join(".legion-sync-9.tmp"), b"tmp").unwrap();
    fs::write(root.join("debug.log"), b"ignored glob").unwrap();
    fs::write(root.join("build/artifact.o"), b"ignored dir").unwrap();

    let r1 = engine.scan(now).unwrap();
    assert_eq!(r1.created, 3); // sub, hello.txt, sub/data.bin
    assert_eq!(r1.hashed, 2); // only the 2 regular files
    assert_eq!(r1.unchanged, 0);

    // Second scan with no edits must hash ZERO files.
    let r2 = engine.scan(now).unwrap();
    assert_eq!(r2.created, 0);
    assert_eq!(r2.modified, 0);
    assert_eq!(r2.deleted, 0);
    assert_eq!(r2.unchanged, 3);
    assert_eq!(r2.hashed, 0);

    // Rename is seen as delete + create.
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

    // First look with require_second_look holds the file back without hashing.
    let opts = ScanOptions {
        require_second_look: true,
        min_stable_age: Duration::ZERO,
    };
    let r1 = engine.scan_with_options(now, opts).unwrap();
    assert_eq!(r1.created, 0);
    assert_eq!(r1.hashed, 0);
    assert_eq!(r1.held_back_unstable, vec!["writing.bin".to_string()]);

    // File grows before second look -> still held back!
    fs::write(root.join("writing.bin"), b"part1-and-part2").unwrap();
    let r2 = engine.scan_with_options(now, opts).unwrap();
    assert_eq!(r2.created, 0);
    assert_eq!(r2.held_back_unstable, vec!["writing.bin".to_string()]);

    // No change on third look -> now committed!
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

    // Create file on tower and sync to laptop.
    fs::write(tmp.path().join("tower/notes.txt"), b"v1").unwrap();
    tower
        .fs()
        .set_live_metadata("notes.txt", t0, Some(0o644))
        .unwrap();
    tower.scan(t0).unwrap();

    let peer_t = EnginePeer::new(&tower, "laptop");
    laptop.pull_from_peer(&peer_t, t0).unwrap();
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"v1"
    );

    // Concurrent divergent edits while apart!
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

    // Sync both directions.
    let peer_l = EnginePeer::new(&laptop, "tower");
    tower.pull_from_peer(&peer_l, t2).unwrap();
    let peer_t = EnginePeer::new(&tower, "laptop");
    laptop.pull_from_peer(&peer_t, t2).unwrap();

    // Laptop had newer mtime -> keeps `notes.txt`, tower becomes conflict copy on BOTH!
    let expected_conflict = "notes (changed on tower 2026-11-25).txt";
    assert_eq!(
        fs::read(tmp.path().join("tower/notes.txt")).unwrap(),
        b"laptop-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"laptop-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("tower").join(expected_conflict)).unwrap(),
        b"tower-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("laptop").join(expected_conflict)).unwrap(),
        b"tower-edit"
    );
    assert_eq!(tower.status().unwrap().conflicts.len(), 1);
    assert_eq!(laptop.status().unwrap().conflicts.len(), 1);

    // Resolve conflict by keeping the conflict copy (`tower-edit`) on laptop, then sync to tower.
    let t3 = Timestamp::new(t0.secs + 180, 0);
    laptop
        .resolve_conflict(expected_conflict, ConflictChoice::KeepConflict, t3)
        .unwrap();
    let peer_l = EnginePeer::new(&laptop, "tower");
    tower.pull_from_peer(&peer_l, t3).unwrap();

    assert_eq!(
        fs::read(tmp.path().join("tower/notes.txt")).unwrap(),
        b"tower-edit"
    );
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"tower-edit"
    );
    assert!(!tmp.path().join("tower").join(expected_conflict).exists());
    assert!(!tmp.path().join("laptop").join(expected_conflict).exists());
    assert!(tower.status().unwrap().conflicts.is_empty());
    assert!(laptop.status().unwrap().conflicts.is_empty());

    // Delete `notes.txt` on tower -> moved to bin on laptop!
    let t4 = Timestamp::new(t0.secs + 240, 0);
    fs::remove_file(tmp.path().join("tower/notes.txt")).unwrap();
    tower.scan(t4).unwrap();
    let peer_t = EnginePeer::new(&tower, "laptop");
    laptop.pull_from_peer(&peer_t, t4).unwrap();

    assert!(!tmp.path().join("laptop/notes.txt").exists());
    let bin = laptop.list_bin().unwrap();
    let deleted_entry = bin
        .iter()
        .find(|e| e.original_path == "notes.txt" && e.reason == BinReason::DeletedByPeer)
        .unwrap()
        .clone();
    assert_eq!(deleted_entry.hash, Some(hash_bytes(b"tower-edit")));

    // Restore from bin on laptop and sync back to tower!
    let t5 = Timestamp::new(t0.secs + 300, 0);
    let restored = laptop
        .restore_from_bin(&deleted_entry.id, None, RestoreStrategy::FailIfExists, t5)
        .unwrap();
    assert_eq!(restored, "notes.txt");
    assert_eq!(
        fs::read(tmp.path().join("laptop/notes.txt")).unwrap(),
        b"tower-edit"
    );

    let peer_l = EnginePeer::new(&laptop, "tower");
    tower.pull_from_peer(&peer_l, t5).unwrap();
    assert_eq!(
        fs::read(tmp.path().join("tower/notes.txt")).unwrap(),
        b"tower-edit"
    );

    // Expire older bin entries after 31 days.
    let after_31d = Timestamp::new(t0.secs + 31 * 86_400, 0);
    let expired = laptop.expire_bin(after_31d).unwrap();
    assert!(!expired.is_empty());
    assert!(laptop.list_bin().unwrap().is_empty());
}

#[test]
fn real_disk_crash_injection_at_every_apply_step_converges_without_loss() {
    let t0 = Timestamp::new(1_795_608_000, 0);
    let t1 = Timestamp::new(t0.secs + 60, 0);

    // Exhaustively test crashing BEFORE and AFTER every single mutating FS op (0..12) during
    // replace, delete, and conflict scenarios against real disk + redb persistence.
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

            // Seed initial common file `replace.txt` and `delete.txt` and `conflict.txt`.
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

            // Now mutate on src: update replace.txt, delete delete.txt, edit conflict.txt (newer).
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

            // Local edit on dst for conflict.txt (older, so dst loses).
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

            // Reopen dst with crash-injecting RealFs + RedbIndexStore!
            let crash_fs =
                CrashInjectingFs::new(RealFs::open(&dst_root).unwrap(), Some(op_idx), crash_after);
            let cfg = FolderConfig::new("docs", "pi", ["tower".into(), "pi".into()]);
            let store = RedbIndexStore::open(&dst_db).unwrap();
            let mut dst_crash = SyncEngine::open(cfg.clone(), crash_fs, store).unwrap();

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

            // Restart from scratch on the same directory and redb file -> recover & finish sync!
            let mut dst_recovered =
                open_real_engine(&dst_root, &dst_db, "pi", &["tower", "pi"], vec![], None);
            dst_recovered.scan(t1).unwrap();
            dst_recovered
                .pull_from_peer(&EnginePeer::new(&src, "pi"), t1)
                .unwrap();
            src.pull_from_peer(&EnginePeer::new(&dst_recovered, "tower"), t1)
                .unwrap();

            // Verify 100% convergence and zero lost bytes (old versions live in bin or conflict copy).
            let conflict_name = "conflict (changed on pi 2026-11-25).txt";
            assert_eq!(
                fs::read(dst_root.join("replace.txt")).unwrap(),
                b"new-replace-bytes"
            );
            assert!(!dst_root.join("delete.txt").exists());
            assert_eq!(
                fs::read(dst_root.join("conflict.txt")).unwrap(),
                b"tower-wins"
            );
            assert_eq!(fs::read(dst_root.join(conflict_name)).unwrap(), b"pi-loses");
            assert_eq!(fs::read(src_root.join(conflict_name)).unwrap(), b"pi-loses");

            // Verify `.legion-sync/tmp` is completely clean and `.legion-sync/bin` kept old data.
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

    // Tower deletes `doc.txt`, while pi edits `doc.txt` on disk WITHOUT scanning yet!
    fs::remove_file(tmp.path().join("tower/doc.txt")).unwrap();
    tower.scan(t1).unwrap();

    // Even with the exact same length and same mtime, the hash re-check catches the unscanned edit!
    fs::write(tmp.path().join("pi/doc.txt"), b"v2").unwrap();
    pi.fs()
        .set_live_metadata("doc.txt", t0, Some(0o644))
        .unwrap();

    pi.pull_from_peer(&EnginePeer::new(&tower, "pi"), t1)
        .unwrap();
    // Delete vs change -> change MUST survive!
    assert_eq!(fs::read(tmp.path().join("pi/doc.txt")).unwrap(), b"v2");

    tower
        .pull_from_peer(&EnginePeer::new(&pi, "tower"), t1)
        .unwrap();
    assert_eq!(fs::read(tmp.path().join("tower/doc.txt")).unwrap(), b"v2");
}

#[test]
fn real_disk_platform_problems_status_and_security_refusals() {
    let tmp = tempfile::tempdir().unwrap();
    let t0 = Timestamp::new(1_795_608_000, 0);

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

    // Status starts UpToDate.
    assert_eq!(winbox.status().unwrap().state, MemberState::UpToDate);

    // Paused status + pull refusal.
    winbox.set_paused(true).unwrap();
    assert_eq!(winbox.status().unwrap().state, MemberState::Paused);
    assert!(matches!(
        winbox.pull_from_peer(&EnginePeer::new(&linux, "winbox"), t0),
        Err(SyncError::Paused { .. })
    ));
    winbox.set_paused(false).unwrap();

    // WaitingForMember status.
    winbox.mark_peer_unreachable("tower");
    assert_eq!(
        winbox.status().unwrap().state,
        MemberState::WaitingForMember {
            members: vec!["tower".to_string()]
        }
    );

    // Non-member refusal.
    assert!(matches!(
        linux.serve_records_since("intruder", 0, 100),
        Err(SyncError::Denied { .. })
    ));
    assert!(matches!(
        linux.serve_fetch_file("intruder", "ok.txt", hash_bytes(b"ok")),
        Err(SyncError::Denied { .. })
    ));

    // Create both representable and Windows-unrepresentable names on Linux.
    fs::write(tmp.path().join("linux/ok.txt"), b"ok").unwrap();
    fs::write(tmp.path().join("linux/CON.txt"), b"reserved").unwrap();
    fs::write(tmp.path().join("linux/README.md"), b"upper").unwrap();
    fs::write(tmp.path().join("linux/readme.md"), b"lower").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("ok.txt", tmp.path().join("linux/sym")).unwrap();
    linux.scan(t0).unwrap();

    // Never serves `.legion-sync/` even to a valid member.
    assert!(matches!(
        linux.serve_fetch_file("winbox", ".legion-sync/bin/x", hash_bytes(b"x")),
        Err(SyncError::InvalidPath { .. })
    ));
    assert!(matches!(
        linux.serve_fetch_file("winbox", "../escape", hash_bytes(b"x")),
        Err(SyncError::InvalidPath { .. })
    ));

    // Pull on Windows-capability member: ok.txt syncs, invalid names + case clash + symlink held back.
    let pull = winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t0)
        .unwrap();
    assert!(pull.held_back >= 2);
    assert_eq!(fs::read(tmp.path().join("winbox/ok.txt")).unwrap(), b"ok");
    assert!(!tmp.path().join("winbox/CON.txt").exists());

    let st = winbox.status().unwrap();
    match st.state {
        MemberState::Problem { sentence, problems } => {
            assert!(!sentence.is_empty());
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::ReservedWindowsName)
            );
            assert!(problems.iter().any(|p| p.kind == ProblemKind::CaseClash));
            #[cfg(unix)]
            assert!(
                problems
                    .iter()
                    .any(|p| p.kind == ProblemKind::UnsupportedSymlink)
            );
        }
        other => panic!("expected Problem state, got {other:?}"),
    }

    // Deleting the clashing `README.md` on Linux unblocks `readme.md` on Windows automatically!
    let t1 = Timestamp::new(t0.secs + 60, 0);
    fs::remove_file(tmp.path().join("linux/README.md")).unwrap();
    fs::remove_file(tmp.path().join("linux/CON.txt")).unwrap();
    #[cfg(unix)]
    fs::remove_file(tmp.path().join("linux/sym")).unwrap();
    linux.scan(t1).unwrap();

    winbox
        .pull_from_peer(&EnginePeer::new(&linux, "winbox"), t1)
        .unwrap();
    assert_eq!(winbox.status().unwrap().state, MemberState::UpToDate);
    assert_eq!(
        fs::read(tmp.path().join("winbox/readme.md")).unwrap(),
        b"lower"
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

    // Symlink itself is recorded as a Symlink (never followed into `outside`).
    assert!(
        a.store()
            .get_record("link_dir/secret.txt")
            .unwrap()
            .is_none()
    );
    let rec = a.store().get_record("link_dir").unwrap().unwrap();
    assert_eq!(rec.kind, legion_xfer::EntryKind::Symlink);

    // Syncs to beta as a symbolic link.
    b.pull_from_peer(&EnginePeer::new(&a, "beta"), t0).unwrap();
    let meta = fs::symlink_metadata(tmp.path().join("b/link_dir")).unwrap();
    assert!(meta.file_type().is_symlink());
    assert_eq!(
        fs::read_link(tmp.path().join("b/link_dir")).unwrap(),
        outside.path()
    );

    // Direct access through symlink ancestor is refused by RealFs!
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

    // Mutate `moving.bin` on `src` without rescanning so `dst` sees a peer-side hash mismatch and
    // defers with `files_left: 1, bytes_left: 12`.
    fs::write(tmp.path().join("src/moving.bin"), b"changed before fetch!!").unwrap();
    let pull = dst
        .pull_from_peer(&EnginePeer::new(&src, "pi"), t0)
        .unwrap();
    assert_eq!(pull.deferred, 1);
    assert_eq!(
        dst.status().unwrap().state,
        MemberState::Syncing {
            files_left: 1,
            bytes_left: payload.len() as u64,
        }
    );

    // Once `src` rescans and `dst` pulls again, status returns cleanly to UpToDate.
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
    /// Wrapper that appends bytes to a named file on disk right while it is being hashed,
    /// simulating an active external writer between Look 1 (walk stat) and Look 2 (post-hash stat).
    struct MutateDuringHashFs {
        inner: RealFs,
        target: String,
        armed: Cell<bool>,
    }

    impl SyncFs for MutateDuringHashFs {
        fn capabilities(&self) -> FsCapabilities {
            self.inner.capabilities()
        }
        fn walk(&self, ignore: &IgnoreMatcher) -> legion_sync::Result<FsWalkResult> {
            self.inner.walk(ignore)
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
        fn open_live_read(&self, rel_path: &str) -> legion_sync::Result<Box<dyn Read + Send>> {
            self.inner.open_live_read(rel_path)
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
        fn rename_live_to_live(&self, from_rel: &str, to_rel: &str) -> legion_sync::Result<()> {
            self.inner.rename_live_to_live(from_rel, to_rel)
        }
        fn create_tmp(&self, tmp_name: &str) -> legion_sync::Result<Box<dyn SyncWrite>> {
            self.inner.create_tmp(tmp_name)
        }
        fn stat_tmp(&self, tmp_name: &str) -> legion_sync::Result<Option<FsMeta>> {
            self.inner.stat_tmp(tmp_name)
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
        fn commit_tmp_to_live(&self, tmp_name: &str, live_rel: &str) -> legion_sync::Result<()> {
            self.inner.commit_tmp_to_live(tmp_name, live_rel)
        }
        fn move_live_to_bin(&self, live_rel: &str, bin_data_name: &str) -> legion_sync::Result<()> {
            self.inner.move_live_to_bin(live_rel, bin_data_name)
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

    // First scan catches the mutation between Look 1 and Look 2 and holds the file back!
    let r1 = engine.scan(now).unwrap();
    assert_eq!(r1.created, 0);
    assert_eq!(r1.held_back_unstable, vec!["growing.bin".to_string()]);
    assert!(engine.store().get_record("growing.bin").unwrap().is_none());

    // Second scan sees the finished file and indexes the complete bytes.
    let r2 = engine.scan(now).unwrap();
    assert_eq!(r2.created, 1);
    assert!(r2.held_back_unstable.is_empty());
    let rec = engine.store().get_record("growing.bin").unwrap().unwrap();
    assert_eq!(rec.hash, Some(hash_bytes(b"initial-more-bytes")));
}
